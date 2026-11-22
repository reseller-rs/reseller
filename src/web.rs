//! HTTP router: health probes, embedded dashboards and theme files, and
//! dispatch to the management APIs, payment webhooks, and the upstream proxy.
use crate::{
    App, api,
    error::{Error, Result},
    identity, mcp, payments, proxy, security, stats, theme,
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, Request, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use serde_json::json;
use std::{convert::Infallible, net::IpAddr, net::SocketAddr, time::Duration};
use tower_http::{
    cors::{AllowOrigin, Any, CorsLayer},
    set_header::SetResponseHeaderLayer,
};

const WEBHOOK_BODY_LIMIT: usize = 1024 * 1024;
const EVENTS_INTERVAL: Duration = Duration::from_secs(5);
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
     connect-src 'self'; img-src 'self' data: https:; font-src 'self'; frame-ancestors 'none'; \
     base-uri 'none'; \
     form-action 'self'";

/// Builds the application router. CORS is read once here, so CORS changes
/// require a restart.
pub async fn router(app: App) -> Router {
    let (cors_origin, https) = {
        let config = app.config().await;
        (
            config.cors_origin.clone(),
            config.public_url.starts_with("https://"),
        )
    };
    let mut router = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/", get(page))
        .route("/workspace", get(page))
        .route("/workspace/", get(page))
        .route("/admin", get(page))
        .route("/admin/", get(page))
        .route(
            "/assets/theme.css",
            get(|State(app): State<App>| async move { theme::stylesheet(&app).await }),
        )
        .route(
            "/theme/{*path}",
            get(
                |State(app): State<App>, Path(path): Path<String>| async move {
                    theme::file(&app, &path).await
                },
            ),
        )
        .route(
            "/assets/app.js",
            get(|| {
                asset(
                    "text/javascript; charset=utf-8",
                    include_bytes!("../assets/app.js"),
                )
            }),
        )
        .route(
            "/assets/style.css",
            get(|| {
                asset(
                    "text/css; charset=utf-8",
                    include_bytes!("../assets/style.css"),
                )
            }),
        )
        .route(
            "/assets/favicon.svg",
            get(|| asset("image/svg+xml", include_bytes!("../assets/favicon.svg"))),
        )
        .route(
            "/assets/favicon-32.png",
            get(|| asset("image/png", include_bytes!("../assets/favicon-32.png"))),
        )
        .fallback(dispatch)
        .with_state(app);
    let mut headers = vec![
        (header::CACHE_CONTROL, "no-store"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
    ];
    if https {
        headers.push((header::STRICT_TRANSPORT_SECURITY, "max-age=31536000"));
    }
    for (name, value) in headers {
        router = router.layer(SetResponseHeaderLayer::overriding(
            name,
            HeaderValue::from_static(value),
        ));
    }
    if !cors_origin.is_empty() {
        router = router.layer(cors(&cors_origin));
    }
    router
}

fn cors(origin: &str) -> CorsLayer {
    let origin = if origin == "*" {
        AllowOrigin::from(Any)
    } else {
        AllowOrigin::exact(origin.parse().expect("cors_origin is validated at load"))
    };
    CorsLayer::new()
        .allow_origin(origin)
        .allow_methods(Any)
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            HeaderName::from_static("x-api-key"),
            HeaderName::from_static("x-admin-token"),
            HeaderName::from_static("mcp-protocol-version"),
            HeaderName::from_static("mcp-method"),
            HeaderName::from_static("mcp-name"),
        ])
        .expose_headers([
            HeaderName::from_static("x-request-id"),
            HeaderName::from_static("x-key-prefix"),
            HeaderName::from_static("x-account-balance-usd"),
            HeaderName::from_static("x-model-fallback"),
        ])
}

async fn asset(content_type: &'static str, bytes: &'static [u8]) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, content_type)], bytes)
}

async fn page(State(app): State<App>) -> impl IntoResponse {
    theme::page(&app).await
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok", "service": "reseller"}))
}

fn discovery() -> serde_json::Value {
    json!({
        "service": "reseller",
        "api_base": "/v1",
        "models": "/v1/models",
        "keys": "/v1/keys",
        "plans": "/v1/plans",
        "signup": "/v1/signup",
        "mcp": "/mcp",
        "routes": {
            "llm": ["/v1/chat/completions", "/v1/responses", "/v1/completions", "/v1/embeddings", "/v1/realtime"],
            "tts": ["/v1/audio/speech"],
            "stt": ["/v1/audio/transcriptions", "/v1/audio/translations"],
            "image": ["/v1/images", "/v1/images/generations", "/v1/images/edits", "/v1/images/variations"],
        },
        "workspace": "/workspace/",
        "admin": "/admin/",
    })
}

/// How a `/v1` path outside the customer routes is served.
#[derive(Debug, PartialEq, Eq)]
enum V1 {
    /// `GET /v1`.
    Discovery,
    /// The model list, or an upstream route.
    Proxy,
    /// A known path called with another method. Holds the `Allow` value.
    Method(&'static str),
    Unknown,
}

/// The methods a `/v1` path accepts, or `None` for an unknown path.
fn v1_methods(path: &str) -> Option<&'static str> {
    match path {
        "/v1" | "/v1/models" | "/v1/realtime" => Some("GET"),
        "/v1/chat/completions"
        | "/v1/completions"
        | "/v1/embeddings"
        | "/v1/moderations"
        | "/v1/audio/speech"
        | "/v1/audio/transcriptions"
        | "/v1/audio/translations"
        | "/v1/images"
        | "/v1/images/generations"
        | "/v1/images/edits"
        | "/v1/images/variations"
        | "/v1/responses" => Some("POST"),
        _ => {
            let rest = path.strip_prefix("/v1/")?;
            if let Some(id) = rest.strip_prefix("models/") {
                return (!id.is_empty()).then_some("GET");
            }
            if let Some(sub) = rest.strip_prefix("realtime/") {
                return (!sub.is_empty()).then_some("POST");
            }
            match rest.strip_prefix("responses/")?.split_once('/') {
                None if rest.len() > "responses/".len() => Some("GET, DELETE"),
                Some((id, tail)) if !id.is_empty() && !tail.is_empty() => Some("GET, POST"),
                _ => None,
            }
        }
    }
}

fn v1_route(method: &Method, path: &str) -> V1 {
    match v1_methods(path) {
        None => V1::Unknown,
        Some(allow) if allow.split(", ").any(|m| m == method.as_str()) => {
            if path == "/v1" {
                V1::Discovery
            } else {
                V1::Proxy
            }
        }
        Some(allow) => V1::Method(allow),
    }
}

fn unknown_url(method: &Method, path: &str) -> Error {
    Error::not_found("unknown_url", format!("Invalid URL ({method} {path})"))
}

async fn ready(State(app): State<App>) -> Result<Json<serde_json::Value>> {
    sqlx::query("SELECT 1").execute(&app.db).await?;
    Ok(Json(json!({"status": "ready"})))
}

/// Client address. Behind a trusted proxy, `CF-Connecting-IP` wins, then the
/// rightmost `X-Forwarded-For` entry, the one the proxy itself appends.
/// Entries further left are client-controlled.
fn client_ip(peer: SocketAddr, headers: &HeaderMap, trust_proxy: bool) -> IpAddr {
    if !trust_proxy {
        return peer.ip();
    }
    let parse = |v: &str| v.trim().parse::<IpAddr>().ok();
    headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(parse)
        .or_else(|| {
            headers
                .get_all("x-forwarded-for")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .last()
                .and_then(parse)
        })
        .unwrap_or_else(|| peer.ip())
}

async fn dispatch(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response {
    route(app, peer, req)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

async fn route(app: App, peer: SocketAddr, req: Request<Body>) -> Result<Response> {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();
    let ip = client_ip(peer, req.headers(), app.config().await.trust_proxy).to_string();
    let is_api = |prefix: &str| path == prefix || path.starts_with(&format!("{prefix}/"));

    if method == Method::POST
        && (path == "/webhooks/stripe" || path == payments::payroof::WEBHOOK_PATH)
    {
        let (parts, body) = req.into_parts();
        let body = to_bytes(body, WEBHOOK_BODY_LIMIT)
            .await
            .map_err(|_| Error::too_large("Webhook too large"))?;
        let handled = if path == "/webhooks/stripe" {
            payments::stripe::webhook(&app, &parts.headers, &body).await?
        } else {
            payments::payroof::webhook(&app, &parts.headers, &body).await?
        };
        return Ok(Json(handled).into_response());
    }
    let management = ["/v1/keys", "/v1/account", "/v1/plans", "/v1/signup", "/mcp"]
        .into_iter()
        .any(is_api)
        || path.starts_with("/admin/api/");
    // Signup challenges have their own, tighter per-IP budget.
    if management && path != "/v1/signup/challenge" {
        security::management(&app, &ip).await?;
    }
    // Credentialed requests: refuse before any database lookup once this IP
    // has exhausted its invalid-credential budget.
    let credentialed = (management || path.starts_with("/v1/"))
        && (identity::token(req.headers()).is_some()
            || req.headers().contains_key("x-admin-token"));
    if credentialed {
        security::check_auth_failures(&app, &ip).await?;
    }
    let result = serve(&app, req, &path, &method, &ip).await;
    if credentialed
        && let Err(e) = &result
        && security::is_auth_failure(e)
    {
        security::record_auth_failure(&app, &ip).await;
    }
    result
}

async fn serve(
    app: &App,
    req: Request<Body>,
    path: &str,
    method: &Method,
    ip: &str,
) -> Result<Response> {
    let is_api = |prefix: &str| path == prefix || path.starts_with(&format!("{prefix}/"));
    if path == "/admin/api/events" && method == Method::GET {
        return admin_events(app.clone(), req.headers().clone()).await;
    }
    if path == "/mcp" {
        return mcp::handle(app, req, ip).await;
    }
    if path.starts_with("/admin/api/") {
        return Ok(Json(api::admin(app, req, ip).await?).into_response());
    }
    if ["/v1/keys", "/v1/account", "/v1/plans", "/v1/signup"]
        .into_iter()
        .any(is_api)
    {
        return Ok(Json(api::customer(app, req, ip).await?).into_response());
    }
    if path == "/v1" || path.starts_with("/v1/") {
        return match v1_route(method, path) {
            V1::Discovery => Ok(Json(discovery()).into_response()),
            V1::Proxy => proxy::handle(app.clone(), req, ip.to_owned()).await,
            V1::Method(allow) => Err(Error::method_not_allowed(
                allow,
                format!("Method {method} is not allowed for {path}. Use {allow}."),
            )),
            V1::Unknown => Err(unknown_url(method, path)),
        };
    }
    Err(unknown_url(method, path))
}

/// Server-sent overview snapshots for the console. The token is re-checked
/// on every tick so a rotated administrator token ends open streams.
async fn admin_events(app: App, headers: HeaderMap) -> Result<Response> {
    identity::admin(&app, &headers).await?;
    let stream = async_stream::stream! {
        let mut interval = tokio::time::interval(EVENTS_INTERVAL);
        loop {
            interval.tick().await;
            if identity::admin(&app, &headers).await.is_err() {
                break;
            }
            match stats::overview(&app).await {
                Ok(snapshot) => {
                    yield Ok::<_, Infallible>(Event::default().event("snapshot").data(snapshot.to_string()));
                }
                Err(_) => break,
            }
        }
    };
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn v1_paths_are_routed_by_method() {
        let route = |m: &Method, p: &str| v1_route(m, p);
        let (get, post, delete) = (Method::GET, Method::POST, Method::DELETE);
        assert_eq!(route(&get, "/v1"), V1::Discovery);
        assert_eq!(route(&post, "/v1"), V1::Method("GET"));
        for path in [
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/embeddings",
            "/v1/moderations",
            "/v1/audio/speech",
            "/v1/audio/transcriptions",
            "/v1/audio/translations",
            "/v1/images",
            "/v1/images/generations",
            "/v1/images/edits",
            "/v1/images/variations",
            "/v1/responses",
            "/v1/realtime/calls",
            "/v1/realtime/client_secrets",
        ] {
            assert_eq!(route(&post, path), V1::Proxy, "{path}");
            assert_eq!(route(&get, path), V1::Method("POST"), "{path}");
        }
        for path in [
            "/v1/models",
            "/v1/models/gpt",
            "/v1/models/meta/muse-image",
            "/v1/realtime",
        ] {
            assert_eq!(route(&get, path), V1::Proxy, "{path}");
            assert_eq!(route(&post, path), V1::Method("GET"), "{path}");
        }
        assert_eq!(route(&get, "/v1/responses/resp_1"), V1::Proxy);
        assert_eq!(route(&delete, "/v1/responses/resp_1"), V1::Proxy);
        assert_eq!(
            route(&post, "/v1/responses/resp_1"),
            V1::Method("GET, DELETE")
        );
        assert_eq!(route(&get, "/v1/responses/resp_1/input_items"), V1::Proxy);
        assert_eq!(route(&post, "/v1/responses/resp_1/cancel"), V1::Proxy);
        assert_eq!(
            route(&delete, "/v1/responses/resp_1/cancel"),
            V1::Method("GET, POST")
        );
    }

    #[test]
    fn unknown_v1_paths_are_not_proxied() {
        for path in [
            "/v1/",
            "/v1/files",
            "/v1/chat",
            "/v1/chat/completions/",
            "/v1/models/",
            "/v1/responses/",
            "/v1/responses//cancel",
            "/v1/responses/resp_1/",
            "/v1/realtime/",
            "/v1/../etc",
        ] {
            assert_eq!(route_of(path), V1::Unknown, "{path}");
        }
        let e = unknown_url(&Method::GET, "/v1/files");
        assert_eq!((e.status.as_u16(), e.code), (404, "unknown_url"));
        assert_eq!(e.message, "Invalid URL (GET /v1/files)");
    }

    fn route_of(path: &str) -> V1 {
        v1_route(&Method::POST, path)
    }

    #[test]
    fn client_ip_ignores_spoofable_forwarded_entries() {
        let peer: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let spoofed = headers(&[("x-forwarded-for", "1.1.1.1, 203.0.113.9")]);
        assert_eq!(client_ip(peer, &spoofed, false), peer.ip());
        assert_eq!(
            client_ip(peer, &spoofed, true),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        let cloudflare = headers(&[
            ("cf-connecting-ip", "198.51.100.4"),
            ("x-forwarded-for", "1.1.1.1"),
        ]);
        assert_eq!(
            client_ip(peer, &cloudflare, true),
            "198.51.100.4".parse::<IpAddr>().unwrap()
        );
        let garbage = headers(&[("x-forwarded-for", "not-an-ip")]);
        assert_eq!(client_ip(peer, &garbage, true), peer.ip());
    }
}
