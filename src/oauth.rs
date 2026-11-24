//! OAuth 2.1 authorization server for the remote MCP endpoint.
//!
//! Implements what MCP clients need to connect by URL alone: protected
//! resource and authorization server metadata (RFC 9728, RFC 8414), dynamic
//! public-client registration (RFC 7591), the authorization code flow with
//! PKCE S256 (RFC 7636) and resource indicators (RFC 8707), refresh token
//! rotation with reuse detection, and revocation (RFC 7009).
//!
//! The consent screen is part of the single-page UI. `GET /oauth/authorize`
//! serves the page, which calls `/oauth/api/authorize` to validate the request
//! and to record the user's decision. The user proves who they are with an API
//! key or dashboard token, and the resulting grant is bound to that key.
//!
//! Codes and tokens are stored as HMAC digests only. Access tokens work on
//! `/mcp` only. A grant stops working as soon as its key does.
use crate::{
    App,
    config::Config,
    db::{self, DAY, now},
    error::{Error, Result},
    identity::{self, Identity, Scope},
    security,
};
use axum::{
    Json,
    body::{Body, to_bytes},
    http::{HeaderValue, Method, Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqliteConnection};
use std::collections::HashMap;
use subtle::ConstantTimeEq;
use url::{Host, Url};

/// The only scope: access to the MCP endpoint.
pub const SCOPE: &str = "mcp";
/// Prefix of OAuth access tokens.
pub const ACCESS_PREFIX: &str = "mcp_at_";
const REFRESH_PREFIX: &str = "mcp_rt_";
const CODE_PREFIX: &str = "mcp_ac_";
const CLIENT_PREFIX: &str = "mcpc_";
/// Authorization code lifetime in seconds.
const CODE_TTL: i64 = 600;
/// Access token lifetime in seconds.
const ACCESS_TTL: i64 = 3600;
/// A refresh token expires after this many seconds without use.
const REFRESH_TTL: i64 = 90 * DAY;
/// Most clients kept. Unused registrations are purged first when it is reached.
const MAX_CLIENTS: i64 = 10_000;
const MAX_REDIRECT_URIS: usize = 10;
const MAX_FORM_BYTES: usize = 64 * 1024;
const LAST_USED_GRANULARITY: i64 = 60;

/// Whether the authorization server is served.
pub fn enabled(config: &Config) -> bool {
    config.mcp.enabled && config.mcp.oauth
}

fn issuer(config: &Config) -> &str {
    config.public_base()
}

fn resource_of(config: &Config) -> String {
    format!("{}/mcp", issuer(config))
}

/// The `WWW-Authenticate` challenge of a `/mcp` 401, or `None` when OAuth is
/// off. `invalid` adds `error="invalid_token"` (a credential was presented).
pub fn challenge(config: &Config, invalid: bool) -> Option<String> {
    enabled(config).then(|| {
        format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\", scope=\"{SCOPE}\"{}",
            issuer(config),
            if invalid {
                ", error=\"invalid_token\""
            } else {
                ""
            }
        )
    })
}

fn ensure_enabled(config: &Config) -> Result<()> {
    if enabled(config) {
        Ok(())
    } else {
        Err(Error::not_found(
            "oauth_disabled",
            "OAuth is not enabled on this server",
        ))
    }
}

// ---------------------------------------------------------------------------
// Errors in RFC 6749 form
// ---------------------------------------------------------------------------

/// An RFC 6749 error response: `{"error","error_description"}`.
#[derive(Debug)]
pub struct OErr {
    status: StatusCode,
    error: String,
    description: String,
    retry_after: Option<u64>,
}

impl OErr {
    fn new(status: StatusCode, error: &str, description: impl Into<String>) -> Self {
        Self {
            status,
            error: error.to_owned(),
            description: description.into(),
            retry_after: None,
        }
    }

    fn bad(error: &str, description: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, error, description)
    }
}

impl From<Error> for OErr {
    fn from(e: Error) -> Self {
        Self {
            status: e.status,
            error: e.code.to_owned(),
            description: e.message,
            retry_after: e.retry_after,
        }
    }
}

impl From<sqlx::Error> for OErr {
    fn from(e: sqlx::Error) -> Self {
        Error::from(e).into()
    }
}

impl IntoResponse for OErr {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(json!({"error": self.error, "error_description": self.description})),
        )
            .into_response();
        let headers = response.headers_mut();
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
        if let Some(wait) = self.retry_after {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(wait));
        }
        response
    }
}

type OResult<T> = std::result::Result<T, OErr>;

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

fn metadata(config: &Config, path: &str) -> Option<Value> {
    let base = issuer(config);
    let rest = path.strip_prefix("/.well-known/")?;
    let rest = rest.strip_suffix("/mcp").unwrap_or(rest);
    match rest {
        "oauth-protected-resource" => Some(json!({
            "resource": resource_of(config),
            "authorization_servers": [base],
            "scopes_supported": [SCOPE],
            "bearer_methods_supported": ["header"],
            "resource_name": config.branding.name,
        })),
        "oauth-authorization-server" | "openid-configuration" => Some(json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/oauth/authorize"),
            "token_endpoint": format!("{base}/oauth/token"),
            "registration_endpoint": format!("{base}/oauth/register"),
            "revocation_endpoint": format!("{base}/oauth/revoke"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "scopes_supported": [SCOPE],
            "authorization_response_iss_parameter_supported": true,
            "service_documentation": format!("{base}/docs/"),
        })),
        _ => None,
    }
}

/// Serves the endpoints that allow any origin: `/.well-known/*`,
/// `/oauth/token`, `/oauth/register` and `/oauth/revoke`.
pub async fn serve_public(app: &App, req: Request<Body>, ip: &str) -> Response {
    let result = public(app, req, ip).await;
    result.unwrap_or_else(IntoResponse::into_response)
}

async fn public(app: &App, req: Request<Body>, ip: &str) -> OResult<Response> {
    let config = app.config().await;
    ensure_enabled(&config)?;
    let path = req.uri().path().to_owned();
    let allowed = if path.starts_with("/.well-known/") {
        Method::GET
    } else {
        Method::POST
    };
    if *req.method() != allowed {
        return Err(Error::method_not_allowed(
            if allowed == Method::GET {
                "GET"
            } else {
                "POST"
            },
            format!("Use {allowed} for {path}"),
        )
        .into());
    }
    if let Some(document) = metadata(&config, &path) {
        return Ok(Json(document).into_response());
    }
    security::management(app, ip).await?;
    match path.as_str() {
        "/oauth/register" => register(app, req, ip).await,
        "/oauth/token" => token(app, &config, req).await,
        "/oauth/revoke" => revoke(app, req).await,
        _ => Err(Error::not_found("not_found", "Unknown endpoint").into()),
    }
}

async fn read_body(req: Request<Body>) -> OResult<(bool, bytes::Bytes)> {
    let json = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.trim_start()
                .to_ascii_lowercase()
                .starts_with("application/json")
        });
    let bytes = to_bytes(req.into_body(), MAX_FORM_BYTES)
        .await
        .map_err(|_| OErr::bad("invalid_request", "The request body is too large"))?;
    Ok((json, bytes))
}

/// Request parameters from a form or JSON body (string values only).
async fn parameters(req: Request<Body>) -> OResult<HashMap<String, String>> {
    let (json, bytes) = read_body(req).await?;
    if !json {
        return Ok(url::form_urlencoded::parse(&bytes).into_owned().collect());
    }
    let Ok(Value::Object(map)) = serde_json::from_slice(&bytes) else {
        return Err(OErr::bad(
            "invalid_request",
            "The body must be a JSON object",
        ));
    };
    Ok(map
        .into_iter()
        .filter_map(|(k, v)| match v {
            Value::String(s) => Some((k, s)),
            Value::Number(n) => Some((k, n.to_string())),
            _ => None,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Dynamic client registration (RFC 7591)
// ---------------------------------------------------------------------------

/// A registered client.
struct Client {
    id: String,
    name: String,
    client_uri: Option<String>,
    logo_uri: Option<String>,
    redirect_uris: Vec<String>,
}

impl Client {
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Self {
        Self {
            id: row.get("id"),
            name: row.get("name"),
            client_uri: row.get("client_uri"),
            logo_uri: row.get("logo_uri"),
            redirect_uris: serde_json::from_str(row.get("redirect_uris")).unwrap_or_default(),
        }
    }
}

async fn find_client(app: &App, id: &str) -> Result<Option<Client>> {
    Ok(sqlx::query("SELECT * FROM oauth_clients WHERE id=?")
        .bind(id)
        .fetch_optional(&app.db)
        .await?
        .map(|row| Client::from_row(&row)))
}

/// Validates a redirect URI: https, http for loopback only, or a private-use
/// custom scheme. No fragments or credentials, and never a scheme that runs
/// code or reads files.
fn check_redirect_uri(raw: &str) -> std::result::Result<(), String> {
    if raw.len() > 2048 {
        return Err("a redirect URI is longer than 2048 characters".into());
    }
    let url = Url::parse(raw).map_err(|_| format!("{raw:?} is not an absolute URI"))?;
    if url.fragment().is_some() || raw.contains('#') {
        return Err("redirect URIs must not contain a fragment".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("redirect URIs must not contain credentials".into());
    }
    match url.scheme() {
        "https" if url.host().is_some() => Ok(()),
        "https" => Err("an https redirect URI needs a host".into()),
        "http" => {
            let loopback = match url.host() {
                Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
                Some(Host::Ipv4(ip)) => ip.is_loopback(),
                Some(Host::Ipv6(ip)) => ip.is_loopback(),
                None => false,
            };
            if loopback {
                Ok(())
            } else {
                Err("http redirect URIs are allowed for localhost, 127.0.0.1 and [::1] only".into())
            }
        }
        "javascript" | "data" | "file" | "about" | "vbscript" | "blob" | "ftp" | "ws" | "wss"
        | "mailto" | "tel" => Err(format!("the {} scheme is not allowed", url.scheme())),
        _ => Ok(()),
    }
}

fn metadata_error(description: impl Into<String>) -> OErr {
    OErr::bad("invalid_client_metadata", description)
}

fn optional_url(body: &Map<String, Value>, key: &str, https_only: bool) -> OResult<Option<String>> {
    let Some(value) = body.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let text = value
        .as_str()
        .ok_or_else(|| metadata_error(format!("{key} must be a string")))?;
    let url = Url::parse(text)
        .ok()
        .filter(|u| text.len() <= 2048 && u.host().is_some())
        .filter(|u| u.scheme() == "https" || (!https_only && u.scheme() == "http"))
        .ok_or_else(|| {
            metadata_error(format!(
                "{key} must be an {} URL",
                if https_only { "https" } else { "http(s)" }
            ))
        })?;
    Ok(Some(url.to_string()))
}

fn subset(body: &Map<String, Value>, key: &str, allowed: &[&str]) -> OResult<()> {
    let Some(value) = body.get(key).filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let ok = value.as_array().is_some_and(|a| {
        a.iter()
            .all(|v| v.as_str().is_some_and(|s| allowed.contains(&s)))
    });
    if ok {
        Ok(())
    } else {
        Err(metadata_error(format!(
            "{key} must only contain: {}",
            allowed.join(", ")
        )))
    }
}

async fn register(app: &App, req: Request<Body>, ip: &str) -> OResult<Response> {
    security::oauth_register(app, ip).await?;
    let (_, bytes) = read_body(req).await?;
    let Ok(Value::Object(body)) = serde_json::from_slice(&bytes) else {
        return Err(metadata_error("The body must be a JSON object"));
    };
    if let Some(method) = body.get("token_endpoint_auth_method")
        && method != "none"
    {
        return Err(metadata_error(
            "Only public clients are supported: token_endpoint_auth_method must be \"none\"",
        ));
    }
    subset(
        &body,
        "grant_types",
        &["authorization_code", "refresh_token"],
    )?;
    subset(&body, "response_types", &["code"])?;
    let uris = body
        .get("redirect_uris")
        .and_then(Value::as_array)
        .filter(|a| (1..=MAX_REDIRECT_URIS).contains(&a.len()))
        .ok_or_else(|| {
            OErr::bad(
                "invalid_redirect_uri",
                format!("redirect_uris must be an array of 1 to {MAX_REDIRECT_URIS} URIs"),
            )
        })?;
    let mut redirect_uris: Vec<String> = Vec::new();
    for uri in uris {
        let uri = uri
            .as_str()
            .ok_or_else(|| OErr::bad("invalid_redirect_uri", "redirect_uris must be strings"))?;
        check_redirect_uri(uri).map_err(|m| OErr::bad("invalid_redirect_uri", m))?;
        if !redirect_uris.iter().any(|u| u == uri) {
            redirect_uris.push(uri.to_owned());
        }
    }
    let name = match body.get("client_name").filter(|v| !v.is_null()) {
        None => String::new(),
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(_) => return Err(metadata_error("client_name must be a string")),
    };
    if name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(metadata_error(
            "client_name must be at most 100 characters without control characters",
        ));
    }
    let client_uri = optional_url(&body, "client_uri", false)?;
    let logo_uri = optional_url(&body, "logo_uri", true)?;

    let mut conn = app.db.acquire().await?;
    let mut count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_clients")
        .fetch_one(&mut *conn)
        .await?;
    if count >= MAX_CLIENTS {
        // Registrations nobody ever authorized are the first to go.
        sqlx::query(
            "DELETE FROM oauth_clients WHERE created_at<? AND NOT EXISTS \
             (SELECT 1 FROM oauth_grants WHERE client_id=oauth_clients.id)",
        )
        .bind(now() - DAY)
        .execute(&mut *conn)
        .await?;
        count = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_clients")
            .fetch_one(&mut *conn)
            .await?;
        if count >= MAX_CLIENTS {
            return Err(OErr::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "temporarily_unavailable",
                "Too many registered clients. Try again later",
            ));
        }
    }
    let id = format!("{CLIENT_PREFIX}{}", hex::encode(random::<16>()));
    let issued = now();
    sqlx::query(
        "INSERT INTO oauth_clients(id,name,client_uri,logo_uri,redirect_uris,created_at,created_ip) \
         VALUES(?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(&name)
    .bind(&client_uri)
    .bind(&logo_uri)
    .bind(serde_json::to_string(&redirect_uris).map_err(Error::internal)?)
    .bind(issued)
    .bind(ip)
    .execute(&mut *conn)
    .await?;
    let mut out = json!({
        "client_id": id,
        "client_id_issued_at": issued,
        "client_name": name,
        "redirect_uris": redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "scope": SCOPE,
    });
    if let Some(uri) = client_uri {
        out["client_uri"] = uri.into();
    }
    if let Some(uri) = logo_uri {
        out["logo_uri"] = uri.into();
    }
    Ok((StatusCode::CREATED, Json(out)).into_response())
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("operating system random source is available");
    bytes
}

// ---------------------------------------------------------------------------
// Authorization request (consent)
// ---------------------------------------------------------------------------

/// A validated authorization request.
struct Authz {
    client: Client,
    redirect_uri: String,
    scope: &'static str,
    resource: String,
    challenge: String,
    state: Option<String>,
}

/// Why an authorization request was refused.
enum Reject {
    /// The client or redirect URI cannot be trusted: never redirect.
    Direct(&'static str, String),
    /// Another parameter is wrong: the error goes back to the client.
    Redirect {
        error: &'static str,
        description: String,
        redirect_uri: String,
        state: Option<String>,
    },
    Server(Error),
}

impl From<Error> for Reject {
    fn from(e: Error) -> Self {
        Self::Server(e)
    }
}

impl From<sqlx::Error> for Reject {
    fn from(e: sqlx::Error) -> Self {
        Self::Server(e.into())
    }
}

/// Appends `pairs`, `state` and `iss` (RFC 9207) to a client redirect URI.
fn redirect_to(config: &Config, uri: &str, pairs: &[(&str, &str)], state: Option<&str>) -> String {
    let Ok(mut url) = Url::parse(uri) else {
        return uri.to_owned();
    };
    {
        let mut query = url.query_pairs_mut();
        for (k, v) in pairs {
            query.append_pair(k, v);
        }
        if let Some(state) = state {
            query.append_pair("state", state);
        }
        query.append_pair("iss", issuer(config));
    }
    url.to_string()
}

impl Reject {
    fn into_error(self) -> Error {
        match self {
            Self::Direct(code, message) => Error::bad(message).with_code(code),
            Self::Server(e) => e,
            Self::Redirect { description, .. } => Error::bad(description),
        }
    }

    fn respond(self, config: &Config) -> Response {
        match self {
            Self::Redirect {
                error,
                description,
                redirect_uri,
                state,
            } => {
                let redirect = redirect_to(
                    config,
                    &redirect_uri,
                    &[("error", error), ("error_description", &description)],
                    state.as_deref(),
                );
                let mut body = Error::bad(description).with_code(error).body();
                body["redirect"] = redirect.into();
                (StatusCode::BAD_REQUEST, Json(body)).into_response()
            }
            other => other.into_error().into_response(),
        }
    }
}

fn valid_pkce_string(s: &str) -> bool {
    (43..=128).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

async fn parse_authorize(
    app: &App,
    config: &Config,
    query: &str,
) -> std::result::Result<Authz, Reject> {
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let mut params: HashMap<&str, &str> = HashMap::new();
    let mut duplicated = false;
    for (k, v) in &pairs {
        duplicated |= params.insert(k, v).is_some();
    }
    let client_id = params.get("client_id").copied().unwrap_or_default();
    let client = if client_id.is_empty() {
        None
    } else {
        find_client(app, client_id).await?
    };
    let Some(client) = client else {
        return Err(Reject::Direct(
            "invalid_client",
            "The client_id is missing or unknown. Register the client first".into(),
        ));
    };
    let redirect_uri = match params.get("redirect_uri").copied() {
        Some(uri) if client.redirect_uris.iter().any(|u| u == uri) => uri.to_owned(),
        None if client.redirect_uris.len() == 1 => client.redirect_uris[0].clone(),
        _ => {
            return Err(Reject::Direct(
                "invalid_redirect_uri",
                "The redirect_uri is missing or does not exactly match a registered URI".into(),
            ));
        }
    };
    let state = params.get("state").map(|s| (*s).to_owned());
    let fail = |error: &'static str, description: &str| Reject::Redirect {
        error,
        description: description.to_owned(),
        redirect_uri: redirect_uri.clone(),
        state: state.clone(),
    };
    if duplicated {
        return Err(fail(
            "invalid_request",
            "A parameter was sent more than once",
        ));
    }
    if state.as_ref().is_some_and(|s| s.len() > 2048) {
        return Err(fail("invalid_request", "state is too long"));
    }
    if params.get("response_type") != Some(&"code") {
        return Err(fail(
            "unsupported_response_type",
            "response_type must be code",
        ));
    }
    let challenge = params.get("code_challenge").copied().unwrap_or_default();
    if !valid_pkce_string(challenge) {
        return Err(fail(
            "invalid_request",
            "code_challenge is required (43 to 128 URL-safe characters)",
        ));
    }
    if params.get("code_challenge_method") != Some(&"S256") {
        return Err(fail(
            "invalid_request",
            "code_challenge_method must be S256",
        ));
    }
    if let Some(scope) = params.get("scope")
        && scope.split_whitespace().any(|s| s != SCOPE)
    {
        return Err(fail("invalid_scope", "The only supported scope is mcp"));
    }
    let resource = resource_of(config);
    if let Some(requested) = params.get("resource")
        && *requested != resource
        && *requested != format!("{resource}/")
    {
        return Err(fail(
            "invalid_target",
            &format!("resource must be {resource}"),
        ));
    }
    Ok(Authz {
        challenge: challenge.to_owned(),
        scope: SCOPE,
        resource,
        client,
        state: state.clone(),
        redirect_uri: redirect_uri.clone(),
    })
}

fn redirect_host(uri: &str) -> String {
    let Ok(url) = Url::parse(uri) else {
        return String::new();
    };
    let host = url.host_str().unwrap_or_default();
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    match url.scheme() {
        "http" | "https" => format!("{host}{port}"),
        scheme if host.is_empty() => format!("{scheme}:"),
        scheme => format!("{scheme}://{host}{port}"),
    }
}

/// `GET /oauth/api/authorize?<authorization request>`: validates the request
/// for the consent page.
pub async fn authorize_info(app: &App, req: Request<Body>) -> Result<Response> {
    let config = app.config().await;
    ensure_enabled(&config)?;
    let query = req.uri().query().unwrap_or_default().to_owned();
    let authz = match parse_authorize(app, &config, &query).await {
        Ok(a) => a,
        Err(reject) => return Ok(reject.respond(&config)),
    };
    Ok(Json(json!({
        "client": {
            "client_id": authz.client.id,
            "client_name": authz.client.name,
            "client_uri": authz.client.client_uri,
            "logo_uri": authz.client.logo_uri,
        },
        "redirect_uri": authz.redirect_uri,
        "redirect_host": redirect_host(&authz.redirect_uri),
        "scope": authz.scope,
        "resource": authz.resource,
        "brand": config.branding.name,
    }))
    .into_response())
}

/// The origin of `public_url`, as browsers send it in `Origin`.
fn public_origin(config: &Config) -> String {
    Url::parse(&config.public_url)
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_default()
}

/// `POST /oauth/api/authorize`: records the user's decision and returns the
/// URL the browser must go to.
pub async fn authorize_decide(app: &App, req: Request<Body>, ip: &str) -> Result<Response> {
    let config = app.config().await;
    ensure_enabled(&config)?;
    if let Some(origin) = req.headers().get(header::ORIGIN)
        && origin.to_str().ok().map(|o| o.trim_end_matches('/'))
            != Some(public_origin(&config).as_str())
    {
        return Err(Error::forbidden(
            "origin_not_allowed",
            "The consent page must be used from this server's own origin",
        ));
    }
    let bytes = to_bytes(req.into_body(), MAX_FORM_BYTES)
        .await
        .map_err(|_| Error::too_large("The request body is too large"))?;
    let body: Value = serde_json::from_slice(&bytes)?;
    let query = body["query"].as_str().unwrap_or_default();
    let decision = body["decision"].as_str().unwrap_or_default();
    if !matches!(decision, "allow" | "deny") {
        return Err(Error::bad("decision must be allow or deny").with_param("decision"));
    }
    let authz = match parse_authorize(app, &config, query).await {
        Ok(a) => a,
        Err(reject) => return Ok(reject.respond(&config)),
    };
    if decision == "deny" {
        let redirect = redirect_to(
            &config,
            &authz.redirect_uri,
            &[
                ("error", "access_denied"),
                ("error_description", "The user denied the request"),
            ],
            authz.state.as_deref(),
        );
        return Ok(Json(json!({"redirect": redirect})).into_response());
    }

    let credential = body["credential"]
        .as_str()
        .map(str::trim)
        .unwrap_or_default();
    if credential.is_empty() {
        return Err(Error::unauthorized(
            "missing_api_key",
            "Enter an API key or dashboard token to authorize",
        ));
    }
    security::check_auth_failures(app, ip).await?;
    let verified = match identity::credential(app, &config, credential, Scope::Manage).await {
        Ok(Some(me)) => identity::by_key_id(app, &me.key_id).await,
        Ok(None) => Err(Error::unauthorized("invalid_api_key", "Invalid API key")),
        Err(e) => Err(e),
    };
    let me = match verified {
        Ok(me) => me,
        Err(e) => {
            if security::is_auth_failure(&e) {
                security::record_auth_failure(app, ip).await;
            }
            return Err(e);
        }
    };
    let code = identity::secret(CODE_PREFIX);
    let pepper = &config.key_pepper;
    sqlx::query(
        "INSERT INTO oauth_codes(code_hash,client_id,key_id,redirect_uri,code_challenge,resource,scope,expires_at) \
         VALUES(?,?,?,?,?,?,?,?)",
    )
    .bind(identity::hash(pepper, &code))
    .bind(&authz.client.id)
    .bind(&me.key_id)
    .bind(&authz.redirect_uri)
    .bind(&authz.challenge)
    .bind(&authz.resource)
    .bind(authz.scope)
    .bind(now() + CODE_TTL)
    .execute(&app.db)
    .await?;
    let redirect = redirect_to(
        &config,
        &authz.redirect_uri,
        &[("code", &code)],
        authz.state.as_deref(),
    );
    Ok(Json(json!({"redirect": redirect})).into_response())
}

// ---------------------------------------------------------------------------
// Token endpoint
// ---------------------------------------------------------------------------

fn invalid_grant(description: &str) -> OErr {
    OErr::bad("invalid_grant", description)
}

fn token_response(access: &str, refresh: &str) -> Response {
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL,
            "refresh_token": refresh,
            "scope": SCOPE,
        })),
    )
        .into_response()
}

/// Removes expired codes and tokens, and grants left without any token.
async fn sweep(conn: &mut SqliteConnection) -> std::result::Result<(), sqlx::Error> {
    let t = now();
    sqlx::query("DELETE FROM oauth_codes WHERE expires_at<?")
        .bind(t)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM oauth_tokens WHERE expires_at<?")
        .bind(t)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "DELETE FROM oauth_grants WHERE created_at<? AND NOT EXISTS \
         (SELECT 1 FROM oauth_tokens WHERE grant_id=oauth_grants.id)",
    )
    .bind(t - 60)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Inserts a fresh access and refresh token for `grant`.
async fn issue(
    conn: &mut SqliteConnection,
    config: &Config,
    grant: &str,
) -> std::result::Result<Response, sqlx::Error> {
    let access = identity::secret(ACCESS_PREFIX);
    let refresh = identity::secret(REFRESH_PREFIX);
    let t = now();
    for (token, kind, ttl) in [
        (&access, "access", ACCESS_TTL),
        (&refresh, "refresh", REFRESH_TTL),
    ] {
        sqlx::query("INSERT INTO oauth_tokens(hash,grant_id,kind,expires_at) VALUES(?,?,?,?)")
            .bind(identity::hash(&config.key_pepper, token))
            .bind(grant)
            .bind(kind)
            .bind(t + ttl)
            .execute(&mut *conn)
            .await?;
    }
    Ok(token_response(&access, &refresh))
}

async fn token(app: &App, config: &Config, req: Request<Body>) -> OResult<Response> {
    let p = parameters(req).await?;
    let get = |k: &str| p.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let client_id =
        get("client_id").ok_or_else(|| OErr::bad("invalid_request", "client_id is required"))?;
    if find_client(app, client_id).await?.is_none() {
        return Err(OErr::new(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "Unknown client_id",
        ));
    }
    if let Some(resource) = get("resource") {
        let expected = resource_of(config);
        if resource != expected && resource != format!("{expected}/") {
            return Err(OErr::bad(
                "invalid_target",
                format!("resource must be {expected}"),
            ));
        }
    }
    match get("grant_type") {
        Some("authorization_code") => exchange_code(app, config, &p, client_id).await,
        Some("refresh_token") => refresh(app, config, &p, client_id).await,
        _ => Err(OErr::bad(
            "unsupported_grant_type",
            "grant_type must be authorization_code or refresh_token",
        )),
    }
}

async fn exchange_code(
    app: &App,
    config: &Config,
    p: &HashMap<String, String>,
    client_id: &str,
) -> OResult<Response> {
    let get = |k: &str| p.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let (Some(code), Some(redirect_uri), Some(verifier)) =
        (get("code"), get("redirect_uri"), get("code_verifier"))
    else {
        return Err(OErr::bad(
            "invalid_request",
            "code, redirect_uri and code_verifier are required",
        ));
    };
    // Single use: the code is gone whatever happens next.
    let row = sqlx::query(
        "DELETE FROM oauth_codes WHERE code_hash=? \
         RETURNING client_id,key_id,redirect_uri,code_challenge,resource,scope,expires_at",
    )
    .bind(identity::hash(&config.key_pepper, code))
    .fetch_optional(&app.db)
    .await?
    .ok_or_else(|| invalid_grant("The authorization code is invalid, expired or already used"))?;
    if row.get::<i64, _>("expires_at") <= now()
        || row.get::<String, _>("client_id") != client_id
        || row.get::<String, _>("redirect_uri") != redirect_uri
    {
        return Err(invalid_grant(
            "The authorization code does not match this client or redirect_uri, or has expired",
        ));
    }
    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let challenge: String = row.get("code_challenge");
    if !valid_pkce_string(verifier) || !bool::from(computed.as_bytes().ct_eq(challenge.as_bytes()))
    {
        return Err(invalid_grant(
            "The code_verifier does not match the code_challenge",
        ));
    }
    let key_id: String = row.get("key_id");
    if identity::by_key_id(app, &key_id).await.is_err() {
        return Err(invalid_grant(
            "The key that authorized this client is no longer active",
        ));
    }
    let grant = db::id();
    let mut tx = db::write(&app.db).await?;
    sweep(&mut tx).await?;
    sqlx::query(
        "INSERT INTO oauth_grants(id,client_id,key_id,scope,resource,created_at) VALUES(?,?,?,?,?,?)",
    )
    .bind(&grant)
    .bind(client_id)
    .bind(&key_id)
    .bind(row.get::<String, _>("scope"))
    .bind(row.get::<String, _>("resource"))
    .bind(now())
    .execute(&mut *tx)
    .await?;
    let response = issue(&mut tx, config, &grant).await?;
    tx.commit().await?;
    Ok(response)
}

async fn refresh(
    app: &App,
    config: &Config,
    p: &HashMap<String, String>,
    client_id: &str,
) -> OResult<Response> {
    let token = p
        .get("refresh_token")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| OErr::bad("invalid_request", "refresh_token is required"))?;
    let mut tx = db::write(&app.db).await?;
    let row = sqlx::query(
        "SELECT t.grant_id,t.used,t.expires_at,g.client_id,g.key_id FROM oauth_tokens t \
         JOIN oauth_grants g ON g.id=t.grant_id WHERE t.hash=? AND t.kind='refresh'",
    )
    .bind(identity::hash(&config.key_pepper, token))
    .fetch_optional(&mut *tx)
    .await?
    .filter(|row| row.get::<String, _>("client_id") == client_id)
    .ok_or_else(|| invalid_grant("The refresh token is invalid or expired"))?;
    let grant: String = row.get("grant_id");
    let key_id: String = row.get("key_id");
    let reused = row.get::<i64, _>("used") != 0;
    let expired = row.get::<i64, _>("expires_at") <= now();
    let key_ok = !reused && !expired && identity::by_key_id(app, &key_id).await.is_ok();
    if !key_ok {
        // A rotated token shown again means it leaked: revoke the whole grant.
        sqlx::query("DELETE FROM oauth_grants WHERE id=?")
            .bind(&grant)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Err(invalid_grant(if reused {
            "The refresh token was already used. The authorization has been revoked"
        } else {
            "The refresh token is invalid or expired"
        }));
    }
    sqlx::query("UPDATE oauth_tokens SET used=1 WHERE hash=?")
        .bind(identity::hash(&config.key_pepper, token))
        .execute(&mut *tx)
        .await?;
    sweep(&mut tx).await?;
    let response = issue(&mut tx, config, &grant).await?;
    tx.commit().await?;
    Ok(response)
}

/// `POST /oauth/revoke` (RFC 7009): revokes the grant a token belongs to.
async fn revoke(app: &App, req: Request<Body>) -> OResult<Response> {
    let config = app.config().await;
    let p = parameters(req).await?;
    let token = p
        .get("token")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| OErr::bad("invalid_request", "token is required"))?;
    sqlx::query(
        "DELETE FROM oauth_grants WHERE id IN (SELECT t.grant_id FROM oauth_tokens t \
         JOIN oauth_grants g ON g.id=t.grant_id WHERE t.hash=?1 AND (?2 IS NULL OR g.client_id=?2))",
    )
    .bind(identity::hash(&config.key_pepper, token))
    .bind(p.get("client_id").filter(|v| !v.is_empty()))
    .execute(&app.db)
    .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(json!({}))).into_response())
}

// ---------------------------------------------------------------------------
// Using an access token
// ---------------------------------------------------------------------------

/// Resolves an OAuth access token presented on `/mcp` into the identity of
/// the key its grant is bound to.
pub async fn authenticate_access(app: &App, config: &Config, token: &str) -> Result<Identity> {
    let invalid = || {
        Error::unauthorized(
            "invalid_token",
            "The access token is invalid, expired or revoked. Authorize again",
        )
    };
    let row = sqlx::query(
        "SELECT t.expires_at,g.id AS grant_id,g.key_id,g.last_used_at FROM oauth_tokens t \
         JOIN oauth_grants g ON g.id=t.grant_id WHERE t.hash=? AND t.kind='access'",
    )
    .bind(identity::hash(&config.key_pepper, token))
    .fetch_optional(&app.db)
    .await?
    .filter(|row| row.get::<i64, _>("expires_at") > now())
    .ok_or_else(invalid)?;
    let identity = identity::by_key_id(app, row.get("key_id"))
        .await
        .map_err(|e| {
            if e.status.is_server_error() {
                e
            } else {
                invalid()
            }
        })?;
    let t = now();
    if row
        .get::<Option<i64>, _>("last_used_at")
        .is_none_or(|last| last <= t - LAST_USED_GRANULARITY)
    {
        sqlx::query("UPDATE oauth_grants SET last_used_at=? WHERE id=?")
            .bind(t)
            .bind(row.get::<String, _>("grant_id"))
            .execute(&app.db)
            .await?;
    }
    Ok(identity)
}

// ---------------------------------------------------------------------------
// Connected apps (customer API)
// ---------------------------------------------------------------------------

/// `GET /v1/account/apps`: the OAuth grants on the account's keys.
pub async fn list_apps(app: &App, account: &str) -> Result<Value> {
    let rows = sqlx::query(
        "SELECT g.id,g.client_id,c.name,c.client_uri,c.logo_uri,g.key_id,k.key_prefix,k.name AS key_name,\
         g.scope,g.created_at,g.last_used_at FROM oauth_grants g \
         JOIN oauth_clients c ON c.id=g.client_id JOIN api_keys k ON k.id=g.key_id \
         WHERE k.account_id=? ORDER BY g.created_at DESC,g.id",
    )
    .bind(account)
    .fetch_all(&app.db)
    .await?;
    let apps: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String, _>("id"),
                "client_id": r.get::<String, _>("client_id"),
                "client_name": r.get::<String, _>("name"),
                "client_uri": r.get::<Option<String>, _>("client_uri"),
                "logo_uri": r.get::<Option<String>, _>("logo_uri"),
                "key_id": r.get::<String, _>("key_id"),
                "key_prefix": r.get::<String, _>("key_prefix"),
                "key_name": r.get::<String, _>("key_name"),
                "scope": r.get::<String, _>("scope"),
                "created_at": r.get::<i64, _>("created_at"),
                "last_used_at": r.get::<Option<i64>, _>("last_used_at"),
            })
        })
        .collect();
    Ok(json!({"apps": apps}))
}

/// `DELETE /v1/account/apps/{id}`: revokes a grant and all its tokens.
pub async fn revoke_app(app: &App, account: &str, id: &str) -> Result<Value> {
    let done = sqlx::query(
        "DELETE FROM oauth_grants WHERE id=? AND key_id IN (SELECT id FROM api_keys WHERE account_id=?)",
    )
    .bind(id)
    .bind(account)
    .execute(&app.db)
    .await?;
    if done.rows_affected() == 0 {
        return Err(Error::not_found("app_not_found", "No such connected app"));
    }
    Ok(json!({"revoked": true, "id": id}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_uris_are_checked() {
        for ok in [
            "https://claude.ai/api/mcp/auth_callback",
            "http://localhost:3000/cb",
            "http://127.0.0.1:8080/cb",
            "http://[::1]:9/cb",
            "cursor://anysphere.cursor-retrieval/oauth/user-mcp/callback",
            "vscode://vscode.github-authentication/did-authenticate",
            "com.example.app:/callback",
        ] {
            assert_eq!(check_redirect_uri(ok), Ok(()), "{ok}");
        }
        for bad in [
            "javascript:alert(1)",
            "data:text/html,x",
            "file:///etc/passwd",
            "about:blank",
            "http://example.com/cb",
            "https://example.com/cb#frag",
            "https://user:pw@example.com/cb",
            "/relative",
            "",
        ] {
            assert!(check_redirect_uri(bad).is_err(), "{bad}");
        }
    }
}
