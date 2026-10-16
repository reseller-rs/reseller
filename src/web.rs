//! HTTP router: health probes, embedded dashboards, and dispatch to the
//! management APIs, the Stripe webhook, and the upstream proxy.
use crate::{
    App, api,
    error::{Error, Result},
    identity, payments, proxy, stats,
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, Request, header},
    response::{
        Html, IntoResponse, Response,
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
     connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; \
     form-action 'self'";

/// Builds the application router. CORS is read once here, so CORS changes
/// require a restart.
pub async fn router(app: App) -> Router {
    let cors_origin = app.config().await.cors_origin.clone();
    let mut router = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/v1", get(discovery))
        .route("/", get(page))
        .route("/workspace", get(page))
        .route("/workspace/", get(page))
        .route("/admin", get(page))
        .route("/admin/", get(page))
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
    for (name, value) in [
        (header::CACHE_CONTROL, "no-store"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
    ] {
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

async fn page() -> Html<&'static str> {
    Html(include_str!("../assets/index.html"))
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok", "service": "reseller"}))
}

async fn discovery() -> Json<serde_json::Value> {
    Json(json!({
        "service": "reseller",
        "api_base": "/v1",
        "keys": "/v1/keys",
        "workspace": "/workspace/",
        "admin": "/admin/",
    }))
}

async fn ready(State(app): State<App>) -> Result<Json<serde_json::Value>> {
    sqlx::query("SELECT 1").execute(&app.db).await?;
    Ok(Json(json!({"status": "ready"})))
}

/// Client address. Behind a trusted proxy, `CF-Connecting-IP` wins, then the
/// rightmost `X-Forwarded-For` entry — the one appended by the proxy itself;
/// entries further left are client-controlled.
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

    if path == "/webhooks/stripe" && method == Method::POST {
        let (parts, body) = req.into_parts();
        let body = to_bytes(body, WEBHOOK_BODY_LIMIT)
            .await
            .map_err(|_| Error::too_large("Webhook too large"))?;
        return Ok(Json(payments::webhook(&app, &parts.headers, &body).await?).into_response());
    }
    if path == "/admin/api/events" && method == Method::GET {
        return admin_events(app, req.headers().clone()).await;
    }
    if path.starts_with("/admin/api/") {
        return Ok(Json(api::admin(&app, req, &ip).await?).into_response());
    }
    if ["/v1/keys", "/v1/account", "/v1/plans"]
        .into_iter()
        .any(is_api)
    {
        return Ok(Json(api::customer(&app, req, &ip).await?).into_response());
    }
    if path.starts_with("/v1/") {
        return proxy::handle(app, req, ip).await;
    }
    Err(Error::not_found("not_found", "Endpoint not found"))
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
