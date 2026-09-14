use crate::{
    App,
    error::{Error, Result},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, State},
    http::{HeaderValue, Request},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use std::{convert::Infallible, net::SocketAddr, time::Duration};
use tower_http::{cors::CorsLayer, set_header::SetResponseHeaderLayer};

pub async fn router(app: App) -> Router {
    let origin = app.config.read().await.cors_origin.clone();
    let mut router=Router::new()
 .route("/healthz",get(||async {axum::Json(serde_json::json!({"status":"ok","service":"reseller"}))}))
 .route("/readyz",get(ready))
 .route("/",get(page)).route("/workspace",get(page)).route("/workspace/",get(page)).route("/admin",get(page)).route("/admin/",get(page))
 .route("/assets/app.js",get(||async {([("content-type","text/javascript; charset=utf-8")],include_str!("../assets/app.js"))}))
 .route("/assets/style.css",get(||async {([("content-type","text/css; charset=utf-8")],include_str!("../assets/style.css"))}))
 .route("/assets/favicon.svg",get(||async {([("content-type","image/svg+xml")],include_str!("../assets/favicon.svg"))}))
 .route("/assets/favicon-32.png",get(||async {([("content-type","image/png")],include_bytes!("../assets/favicon-32.png").as_slice())}))
 .route("/v1",get(||async {axum::Json(serde_json::json!({"service":"reseller","api_base":"/v1","keys":"/v1/keys","workspace":"/workspace/","admin":"/admin/"}))}))
 .fallback(dispatch).with_state(app)
 .layer(SetResponseHeaderLayer::overriding(axum::http::header::CACHE_CONTROL,HeaderValue::from_static("no-store")))
 .layer(SetResponseHeaderLayer::overriding(axum::http::header::X_CONTENT_TYPE_OPTIONS,HeaderValue::from_static("nosniff")))
 .layer(SetResponseHeaderLayer::overriding(axum::http::header::REFERRER_POLICY,HeaderValue::from_static("no-referrer")))
 .layer(SetResponseHeaderLayer::overriding(axum::http::header::CONTENT_SECURITY_POLICY,HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'")));
    if !origin.is_empty() {
        let cors = CorsLayer::new()
            .allow_methods(tower_http::cors::Any)
            .allow_headers([
                axum::http::header::AUTHORIZATION,
                axum::http::header::CONTENT_TYPE,
                "x-api-key".parse().unwrap(),
                "x-admin-token".parse().unwrap(),
            ])
            .expose_headers([
                "x-request-id".parse().unwrap(),
                "x-key-prefix".parse().unwrap(),
                "x-account-balance-usd".parse().unwrap(),
                "x-model-fallback".parse().unwrap(),
            ]);
        router = router.layer(if origin == "*" {
            cors.allow_origin(tower_http::cors::Any)
        } else {
            cors.allow_origin(origin.parse::<HeaderValue>().expect("validated CORS"))
        });
    }
    router
}
async fn page() -> Html<&'static str> {
    Html(include_str!("../assets/index.html"))
}
async fn ready(State(app): State<App>) -> Result<axum::Json<serde_json::Value>> {
    sqlx::query("SELECT 1").execute(&app.db).await?;
    Ok(axum::Json(serde_json::json!({"status":"ready"})))
}
async fn dispatch(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response {
    match route(app, peer, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}
async fn route(app: App, peer: SocketAddr, req: Request<Body>) -> Result<Response> {
    let path = req.uri().path().to_owned();
    let mut ip = peer.ip().to_string();
    if app.config.read().await.trust_proxy
        && let Some(v) = req
            .headers()
            .get("cf-connecting-ip")
            .or_else(|| req.headers().get("x-forwarded-for"))
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .and_then(|v| v.trim().parse::<std::net::IpAddr>().ok())
    {
        ip = v.to_string();
    }
    if path == "/webhooks/stripe" && req.method() == "POST" {
        let (parts, body) = req.into_parts();
        let b = to_bytes(body, 1024 * 1024)
            .await
            .map_err(|_| Error::new(413, "request_too_large", "Webhook too large"))?;
        return Ok(
            axum::Json(crate::payments::webhook(&app, &parts.headers, &b).await?).into_response(),
        );
    }
    if path == "/admin/api/events" && req.method() == "GET" {
        crate::identity::admin(&app, req.headers()).await?;
        let headers = req.headers().clone();
        let stream = async_stream::stream! {
         let mut interval=tokio::time::interval(Duration::from_secs(5));
         loop {interval.tick().await;if crate::identity::admin(&app,&headers).await.is_err(){break;}
          match crate::stats::overview(&app).await {Ok(v)=>yield Ok::<_,Infallible>(Event::default().event("snapshot").data(v.to_string())),Err(_)=>break}
         }
        };
        return Ok(Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response());
    }
    if path.starts_with("/admin/api/") {
        return Ok(axum::Json(crate::api::admin(&app, req, &ip).await?).into_response());
    }
    if ["/v1/keys", "/v1/account", "/v1/plans"]
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
    {
        return Ok(axum::Json(crate::api::customer(&app, req, &ip).await?).into_response());
    }
    if path.starts_with("/v1/") {
        return crate::proxy::handle(app, req, ip).await;
    }
    Err(Error::new(404, "not_found", "Endpoint not found"))
}
