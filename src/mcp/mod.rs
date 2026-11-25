//! Model Context Protocol support.
//!
//! - `POST /mcp` ([`handle`]): a stateless Streamable HTTP server speaking
//!   MCP 2026-07-28 (per-request `_meta`, `server/discover`, `Mcp-Method` /
//!   `Mcp-Name` headers) and, for older clients, the 2025-11-25 `initialize`
//!   handshake. Every POST carries one JSON-RPC message and is answered with
//!   `application/json`.
//!   Tools reuse the existing HTTP handlers (`api::customer`, `proxy::handle`)
//!   through internal requests, so admission, limits, metering, fallbacks and
//!   model policy are identical to the HTTP API.
//! - [`bridge`]: the `reseller mcp` stdio bridge that runs next to an agent,
//!   keeps credentials and proof-of-work local, and forwards to a remote
//!   server.
pub mod bridge;
mod generate;
mod tools;

use crate::{
    App,
    config::Config,
    error::{Error, Result},
    identity::{self, Identity, Scope},
    oauth,
};
use axum::{
    Json,
    body::{Body, to_bytes},
    http::{HeaderMap, Method, Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

pub use tools::requires_api_key;

/// The stateless protocol revision (per-request `_meta`, no handshake).
pub const MODERN_VERSION: &str = "2026-07-28";
/// The only handshake-era revision still served (`initialize`).
pub const LEGACY_VERSION: &str = "2025-11-25";
/// Every revision this server speaks, newest first.
pub const SUPPORTED_VERSIONS: [&str; 2] = [MODERN_VERSION, LEGACY_VERSION];
/// Freshness hint for `tools/list` and `server/discover` results.
pub(crate) const CACHE_TTL_MS: u64 = 300_000;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
/// `HeaderMismatchError`.
const HEADER_MISMATCH: i64 = -32020;
/// `UnsupportedProtocolVersionError`.
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

pub(crate) const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub(crate) const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
pub(crate) const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
pub(crate) const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// The protocol semantics a request is served with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Era {
    /// 2026-07-28: stateless, per-request `_meta`.
    Modern,
    /// 2025-11-25: `initialize` handshake.
    Legacy,
}

/// Everything a tool needs to know about the caller.
pub(crate) struct Ctx {
    pub app: App,
    pub ip: String,
    /// The presented credential, forwarded to internal requests only.
    pub bearer: Option<String>,
    /// The customer identity. `None` for the operator (administrator token).
    pub identity: Option<Identity>,
    /// Authenticated with an OAuth access token: internal requests carry the
    /// identity itself, as that token is valid on `/mcp` only.
    pub oauth: bool,
    /// Authenticated with the administrator token.
    pub operator: bool,
}

/// A failure reported to the model as a tool result with `isError: true`.
#[derive(Debug)]
pub(crate) struct ToolError {
    pub code: String,
    pub message: String,
    pub retry_after: Option<u64>,
}

impl ToolError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retry_after: None,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_arguments", message)
    }

    fn result(&self) -> Value {
        let mut text = format!("{} (error code: {})", self.message, self.code);
        if let Some(wait) = self.retry_after {
            text.push_str(&format!(" Retry after {wait} seconds."));
        }
        json!({
            "content": [{"type": "text", "text": text}],
            "structuredContent": {"error": {
                "code": self.code,
                "message": self.message,
                "retry_after_seconds": self.retry_after,
            }},
            "isError": true,
        })
    }
}

impl From<Error> for ToolError {
    fn from(e: Error) -> Self {
        Self {
            code: e.code.into(),
            message: e.message,
            retry_after: e.retry_after,
        }
    }
}

impl From<sqlx::Error> for ToolError {
    fn from(e: sqlx::Error) -> Self {
        Error::from(e).into()
    }
}

/// Successful tool output: content blocks (what the model reads), optional
/// structured data, and optional result `_meta` (for the client, not the model).
pub(crate) struct Output {
    pub content: Vec<Value>,
    pub structured: Option<Value>,
    pub meta: Option<Value>,
}

impl Output {
    /// Structured data, mirrored as text for clients that only show content.
    pub fn json(value: Value) -> Self {
        Self {
            content: vec![json!({"type": "text", "text": value.to_string()})],
            structured: Some(value),
            meta: None,
        }
    }

    fn result(self) -> Value {
        let mut out = json!({"content": self.content, "isError": false});
        if let Some(s) = self.structured {
            out["structuredContent"] = s;
        }
        if let Some(m) = self.meta {
            out["_meta"] = m;
        }
        out
    }
}

/// A JSON-RPC error object plus the HTTP status it is served with.
#[derive(Debug)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
    pub status: StatusCode,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
            status: StatusCode::OK,
        }
    }

    /// Serves the error with `400 Bad Request` (malformed or rejected requests).
    fn bad_request(mut self) -> Self {
        self.status = StatusCode::BAD_REQUEST;
        self
    }

    /// `UnsupportedProtocolVersionError` listing the supported versions.
    pub fn unsupported_version(requested: &str) -> Self {
        Self {
            data: Some(json!({"supported": SUPPORTED_VERSIONS, "requested": requested})),
            ..Self::new(UNSUPPORTED_PROTOCOL_VERSION, "Unsupported protocol version")
        }
        .bad_request()
    }

    /// A modern request that does not carry the required `_meta` fields.
    pub fn missing_meta(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message).bad_request()
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("Method not found: {method}")).not_found()
    }

    fn not_found(mut self) -> Self {
        self.status = StatusCode::NOT_FOUND;
        self
    }

    pub fn to_json(&self, id: Value) -> Value {
        let mut error = json!({"code": self.code, "message": self.message});
        if let Some(data) = &self.data {
            error["data"] = data.clone();
        }
        json!({"jsonrpc": "2.0", "id": id, "error": error})
    }
}

fn rpc_response(id: Value, outcome: std::result::Result<Value, RpcError>) -> Response {
    match outcome {
        Ok(result) => (
            StatusCode::OK,
            Json(json!({"jsonrpc": "2.0", "id": id, "result": result})),
        )
            .into_response(),
        Err(e) => (e.status, Json(e.to_json(id))).into_response(),
    }
}

/// The version a request declares in `_meta`, if any.
pub(crate) fn meta_version(params: &Value) -> std::result::Result<Option<&str>, RpcError> {
    let invalid = |m: &str| RpcError::missing_meta(m.to_owned());
    match params.get("_meta") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(meta)) => match meta.get(META_VERSION) {
            None => Ok(None),
            Some(Value::String(v)) => Ok(Some(v)),
            Some(_) => Err(invalid(&format!("{META_VERSION} must be a string"))),
        },
        Some(_) => Err(invalid("_meta must be an object")),
    }
}

/// Checks the remaining required per-request `_meta` fields of a modern
/// request: `clientCapabilities` (an object) and, when present, `clientInfo`.
pub(crate) fn validate_meta(params: &Value) -> std::result::Result<(), RpcError> {
    let meta = &params["_meta"];
    if !meta[META_CLIENT_CAPABILITIES].is_object() {
        return Err(RpcError::missing_meta(format!(
            "_meta.{META_CLIENT_CAPABILITIES} is required and must be an object"
        )));
    }
    if let Some(info) = meta.get(META_CLIENT_INFO)
        && !(info["name"].is_string() && info["version"].is_string())
    {
        return Err(RpcError::missing_meta(format!(
            "_meta.{META_CLIENT_INFO} needs string name and version"
        )));
    }
    Ok(())
}

/// Turns a tool or list payload into a complete result: `resultType` plus the
/// server's identity in `_meta`.
pub(crate) fn complete(mut result: Value, server_info: &Value) -> Value {
    result["resultType"] = json!("complete");
    if !result["_meta"].is_object() {
        result["_meta"] = json!({});
    }
    result["_meta"][META_SERVER_INFO] = server_info.clone();
    result
}

/// Adds the caching hints of a cacheable result.
pub(crate) fn cacheable(mut result: Value, ttl_ms: u64) -> Value {
    result["ttlMs"] = json!(ttl_ms);
    result["cacheScope"] = json!("public");
    result
}

/// Encodes a header value for `Mcp-Name`: plain when it is safe ASCII,
/// otherwise the `=?base64?...?=` sentinel form.
pub(crate) fn encode_header_value(value: &str) -> String {
    let plain = !value.is_empty()
        && value.trim() == value
        && value.bytes().all(|b| b == b' ' || b.is_ascii_graphic())
        && !(value.starts_with("=?base64?") && value.ends_with("?="));
    if plain {
        value.to_owned()
    } else {
        format!("=?base64?{}?=", STANDARD.encode(value))
    }
}

/// Inverse of [`encode_header_value`]. Returns `None` for malformed sentinel values.
fn decode_header_value(value: &str) -> Option<String> {
    match value
        .strip_prefix("=?base64?")
        .and_then(|v| v.strip_suffix("?="))
    {
        Some(encoded) => String::from_utf8(STANDARD.decode(encoded).ok()?).ok(),
        None => Some(value.to_owned()),
    }
}

fn server_info(config: &Config) -> Value {
    json!({
        "name": env!("CARGO_PKG_NAME"),
        "title": config.branding.name,
        "version": env!("CARGO_PKG_VERSION"),
    })
}

fn header_mismatch(message: impl Into<String>) -> RpcError {
    RpcError::new(HEADER_MISMATCH, message).bad_request()
}

/// Chooses the era for a request and validates the transport headers that
/// go with it.
///
/// - `initialize` without modern `_meta` opens the legacy era.
/// - Every other request must carry `MCP-Protocol-Version`. When `_meta`
///   carries the version too, the two must agree. The version then picks the
///   era: 2026-07-28 needs the full modern `_meta` and the `Mcp-Method` /
///   `Mcp-Name` headers, 2025-11-25 is served with legacy semantics, and any
///   other value is an `UnsupportedProtocolVersionError`.
fn route(headers: &HeaderMap, method: &str, params: &Value) -> std::result::Result<Era, RpcError> {
    let header = |name: &str| -> std::result::Result<Option<&str>, RpcError> {
        match headers.get(name) {
            None => Ok(None),
            Some(v) => v
                .to_str()
                .map(Some)
                .map_err(|_| header_mismatch(format!("Malformed {name} header"))),
        }
    };
    let declared = meta_version(params)?;
    if method == "initialize" && declared.is_none() {
        return Ok(Era::Legacy);
    }
    let Some(version) = header("mcp-protocol-version")? else {
        return Err(header_mismatch(
            "The MCP-Protocol-Version header is required",
        ));
    };
    if let Some(declared) = declared
        && declared != version
    {
        return Err(header_mismatch(format!(
            "Header mismatch: MCP-Protocol-Version header value '{version}' does not match \
             body value '{declared}'"
        )));
    }
    match version {
        LEGACY_VERSION => Ok(Era::Legacy),
        MODERN_VERSION => {
            if declared.is_none() {
                return Err(RpcError::missing_meta(format!(
                    "_meta.{META_VERSION} is required"
                )));
            }
            if header("mcp-method")? != Some(method) {
                return Err(header_mismatch(format!(
                    "Header mismatch: Mcp-Method header must be '{method}'"
                )));
            }
            if method == "tools/call"
                && let Some(name) = params["name"].as_str()
                && header("mcp-name")?.and_then(decode_header_value).as_deref() != Some(name)
            {
                return Err(header_mismatch(format!(
                    "Header mismatch: Mcp-Name header must be '{name}'"
                )));
            }
            validate_meta(params)?;
            Ok(Era::Modern)
        }
        other => Err(RpcError::unsupported_version(other)),
    }
}

/// `Origin` must be this deployment or the configured CORS origin. Other
/// origins are refused so a web page cannot drive a local or private server
/// through the user's browser (DNS rebinding).
fn origin_allowed(config: &Config, origin: &str) -> bool {
    if config.cors_origin == "*" {
        return true;
    }
    let normalize = |s: &str| {
        url::Url::parse(s)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .map(|u| u.origin().ascii_serialization())
    };
    let Some(origin) = normalize(origin) else {
        return false;
    };
    normalize(&config.public_url).as_deref() == Some(origin.as_str())
        || (!config.cors_origin.is_empty()
            && normalize(&config.cors_origin).as_deref() == Some(origin.as_str()))
}

/// Who is calling: identity, forwarded bearer, OAuth flag, operator flag.
type Caller = (Option<Identity>, Option<String>, bool, bool);

async fn authenticate(
    app: &App,
    config: &Config,
    headers: &HeaderMap,
) -> std::result::Result<Caller, Error> {
    let Some(token) = identity::token(headers) else {
        return Err(Error::unauthorized(
            "missing_api_key",
            "Authentication required: send an OAuth access token, API key or dashboard token \
             as `Authorization: Bearer <token>`",
        ));
    };
    if identity::is_admin_token(config, token) {
        // The operator: generation tools only, never billed.
        return Ok((None, Some(token.to_owned()), false, true));
    }
    if config.mcp.oauth && token.starts_with(oauth::ACCESS_PREFIX) {
        let identity = oauth::authenticate_access(app, config, token).await?;
        return Ok((Some(identity), None, true, false));
    }
    let identity = identity::authenticate(app, headers, Scope::Manage).await?;
    Ok((identity, Some(token.to_owned()), false, false))
}

/// Adds the OAuth discovery challenge (RFC 9728) to a 401 from `/mcp`.
fn unauthorized(config: &Config, error: Error) -> Error {
    if error.status != StatusCode::UNAUTHORIZED {
        return error;
    }
    match oauth::challenge(config, error.code != "missing_api_key") {
        Some(challenge) => error.with_challenge(challenge),
        None => error,
    }
}

/// Serves one `/mcp` request.
pub async fn handle(app: &App, req: Request<Body>, ip: &str) -> Result<Response> {
    let config = app.config().await;
    if !config.mcp.enabled {
        return Err(Error::not_found(
            "mcp_disabled",
            "The MCP endpoint is disabled",
        ));
    }
    if req.method() != Method::POST {
        return Err(Error::method_not_allowed(
            "POST",
            "The MCP endpoint accepts POST only. There is no server-initiated stream",
        ));
    }
    let headers = req.headers();
    if let Some(origin) = headers.get(header::ORIGIN)
        && !origin
            .to_str()
            .is_ok_and(|origin| origin_allowed(&config, origin))
    {
        return Err(Error::forbidden(
            "origin_not_allowed",
            "Origin is not allowed to use this MCP endpoint",
        ));
    }
    // Every request needs a credential. Invalid credentials fail the whole
    // request (and count against the per-IP auth-failure budget in `web::route`).
    let who = authenticate(app, &config, headers)
        .await
        .map_err(|e| unauthorized(&config, e))?;
    let (identity, bearer, oauth, operator) = who;
    let headers = headers.clone();

    let limit = config.mcp.max_media_bytes.saturating_mul(4) / 3 + 256 * 1024;
    let bytes = to_bytes(req.into_body(), limit)
        .await
        .map_err(|_| Error::too_large("MCP message exceeds the configured size limit"))?;
    let bad = |code, message: &str| {
        rpc_response(Value::Null, Err(RpcError::new(code, message).bad_request()))
    };
    let message: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Ok(bad(PARSE_ERROR, "Parse error")),
    };
    if message.is_array() {
        return Ok(bad(
            INVALID_REQUEST,
            "JSON-RPC batches are not supported. Send one message per request",
        ));
    }
    if message["jsonrpc"] != "2.0" || !message.is_object() {
        return Ok(bad(INVALID_REQUEST, "Expected a JSON-RPC 2.0 message"));
    }
    let Some(method) = message["method"].as_str() else {
        // A response to a server request (this server sends none).
        return if message.get("result").is_some() || message.get("error").is_some() {
            Ok(StatusCode::ACCEPTED.into_response())
        } else {
            Ok(bad(INVALID_REQUEST, "Missing method"))
        };
    };
    let id = match message.get("id") {
        None => return Ok(StatusCode::ACCEPTED.into_response()),
        Some(id) if id.is_string() || id.is_i64() || id.is_u64() => id.clone(),
        Some(_) => return Ok(bad(INVALID_REQUEST, "id must be a string or an integer")),
    };
    let params = match message.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(p) if p.is_object() => p.clone(),
        Some(_) => {
            return Ok(rpc_response(
                id,
                Err(RpcError::new(INVALID_PARAMS, "params must be an object")),
            ));
        }
    };
    let ctx = Ctx {
        app: app.clone(),
        ip: ip.to_owned(),
        bearer,
        identity,
        oauth,
        operator,
    };
    let outcome = match route(&headers, method, &params) {
        Err(e) => Err(e),
        Ok(Era::Legacy) => legacy_dispatch(&ctx, &config, method, &params).await,
        Ok(Era::Modern) => modern_dispatch(&ctx, &config, method, &params)
            .await
            .map(|result| complete(result, &server_info(&config))),
    };
    Ok(rpc_response(id, outcome))
}

/// 2025-11-25 semantics: no `resultType`, no caching hints.
async fn legacy_dispatch(
    ctx: &Ctx,
    config: &Config,
    method: &str,
    params: &Value,
) -> std::result::Result<Value, RpcError> {
    match method {
        "initialize" => {
            // Any requested revision gets the one handshake revision served here: the client then
            // goes on with it or disconnects, as the 2025-11-25 lifecycle says.
            if !params["protocolVersion"].is_string() {
                return Err(RpcError::new(INVALID_PARAMS, "protocolVersion is required"));
            }
            Ok(json!({
                "protocolVersion": LEGACY_VERSION,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": server_info(config),
                "instructions": tools::instructions(config),
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools::definitions()})),
        "tools/call" => call_tool(ctx, params).await,
        other => Err(RpcError::new(
            METHOD_NOT_FOUND,
            format!("Method not found: {other}"),
        )),
    }
}

/// 2026-07-28 semantics. There is no `ping`, `initialize`, or subscription
/// support. The server only offers tools.
async fn modern_dispatch(
    ctx: &Ctx,
    config: &Config,
    method: &str,
    params: &Value,
) -> std::result::Result<Value, RpcError> {
    match method {
        "server/discover" => Ok(cacheable(
            json!({
                "supportedVersions": SUPPORTED_VERSIONS,
                "capabilities": {"tools": {}},
                "instructions": tools::instructions(config),
            }),
            CACHE_TTL_MS,
        )),
        "tools/list" => {
            // The list is a single page: no cursor can be valid.
            if params.get("cursor").is_some_and(|c| !c.is_null()) {
                return Err(RpcError::new(INVALID_PARAMS, "Invalid cursor"));
            }
            Ok(cacheable(
                json!({"tools": tools::definitions()}),
                CACHE_TTL_MS,
            ))
        }
        "tools/call" => call_tool(ctx, params).await,
        "initialize" => Err(RpcError::new(
            METHOD_NOT_FOUND,
            format!(
                "Method not found: initialize. This request uses protocol {MODERN_VERSION}, which \
                 has no handshake (supported versions: {})",
                SUPPORTED_VERSIONS.join(", ")
            ),
        )
        .not_found()),
        other => Err(RpcError::method_not_found(other)),
    }
}

async fn call_tool(ctx: &Ctx, params: &Value) -> std::result::Result<Value, RpcError> {
    let name = params["name"]
        .as_str()
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, "name is required"))?;
    if !tools::exists(name) {
        return Err(RpcError::new(
            INVALID_PARAMS,
            format!("Unknown tool: {name}"),
        ));
    }
    let arguments = params.get("arguments").unwrap_or(&Value::Null);
    Ok(match tools::call(ctx, name, arguments).await {
        Ok(output) => output.result(),
        Err(e) => e.result(),
    })
}

/// Builds an internal request carrying the caller's credential (or, for an
/// OAuth access token, the already verified identity).
pub(crate) fn internal(
    method: Method,
    uri: &str,
    ctx: &Ctx,
    content_type: Option<&str>,
    body: Body,
) -> std::result::Result<Request<Body>, Error> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = &ctx.bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if ctx.oauth
        && let Some(identity) = &ctx.identity
    {
        builder = builder.extension(identity::Preauth(identity.clone()));
    }
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    builder.body(body).map_err(Error::internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn header_values_round_trip() {
        for value in [
            "list_plans",
            "with space",
            " padded ",
            "line1\nline2",
            "Hello, 世界",
            "=?base64?literal?=",
            "",
        ] {
            let encoded = encode_header_value(value);
            assert!(
                encoded.bytes().all(|b| b == b' ' || b.is_ascii_graphic()),
                "{encoded}"
            );
            assert_eq!(decode_header_value(&encoded).as_deref(), Some(value));
        }
        assert_eq!(encode_header_value("list_plans"), "list_plans");
        // Spec examples.
        assert_eq!(
            encode_header_value("Hello, 世界"),
            "=?base64?SGVsbG8sIOS4lueVjA==?="
        );
        assert_eq!(encode_header_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(
            encode_header_value("=?base64?literal?="),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
        assert_eq!(decode_header_value("=?base64?!!?="), None);
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        map
    }

    fn modern_params(extra: Value) -> Value {
        let mut params = json!({"_meta": {
            META_VERSION: MODERN_VERSION,
            META_CLIENT_CAPABILITIES: {},
        }});
        for (k, v) in extra.as_object().unwrap() {
            params[k] = v.clone();
        }
        params
    }

    #[test]
    fn era_is_chosen_from_meta_headers_and_initialize() {
        let modern = [
            ("mcp-protocol-version", MODERN_VERSION),
            ("mcp-method", "tools/list"),
        ];
        let route = |h: &[(&'static str, &str)], m: &str, p: &Value| {
            route(&headers(h), m, p).map_err(|e| (e.code, e.status))
        };
        let params = modern_params(json!({}));
        assert_eq!(route(&modern, "tools/list", &params), Ok(Era::Modern));
        // `initialize` without modern `_meta` opens the legacy era, headerless.
        assert_eq!(route(&[], "initialize", &json!({})), Ok(Era::Legacy));
        let legacy = [("mcp-protocol-version", LEGACY_VERSION)];
        assert_eq!(route(&legacy, "tools/list", &json!({})), Ok(Era::Legacy));
        let bad = StatusCode::BAD_REQUEST;
        assert_eq!(route(&[], "tools/list", &json!({})), Err((-32020, bad)));
        assert_eq!(route(&modern, "tools/list", &json!({})), Err((-32602, bad)));
        let old = [("mcp-protocol-version", "2025-06-18")];
        assert_eq!(route(&old, "tools/list", &json!({})), Err((-32022, bad)));
        let mut mismatched = params.clone();
        mismatched["_meta"][META_VERSION] = json!(LEGACY_VERSION);
        assert_eq!(
            route(&modern, "tools/list", &mismatched),
            Err((-32020, bad))
        );
        let wrong_method = [
            ("mcp-protocol-version", MODERN_VERSION),
            ("mcp-method", "tools/call"),
        ];
        assert_eq!(
            route(&wrong_method, "tools/list", &params),
            Err((-32020, bad))
        );
    }

    #[test]
    fn complete_adds_result_type_and_identity() {
        let info = json!({"name": "x", "version": "1"});
        let result = complete(json!({"_meta": {"a": 1}, "isError": false}), &info);
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["_meta"]["a"], 1);
        assert_eq!(result["_meta"][META_SERVER_INFO], info);
        let list = cacheable(json!({"tools": []}), 5);
        assert_eq!(
            (list["ttlMs"].clone(), list["cacheScope"].clone()),
            (json!(5), json!("public"))
        );
    }
}
