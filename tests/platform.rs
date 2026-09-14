use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{HeaderMap, Request},
    response::IntoResponse,
    routing::any,
};
use reseller::{
    App, billing,
    config::{Config, Policy},
    db, identity, money,
    stats::{Meter, RequestMeta, Usage},
};
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;

struct Test {
    app: App,
    _dir: tempfile::TempDir,
}
impl Test {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("test.db")).await.unwrap();
        let c = Config {
            admin_token: "admin-test-secret-123456789".into(),
            key_pepper: "pepper-test-secret-12345678".into(),
            cooldown_seconds: 0,
            ip_limit_per_month: 0,
            global_hourly_limit: 0,
            ..Config::default()
        };
        let c = db::settings(&pool, c).await.unwrap();
        Self {
            app: App::new(pool, c).unwrap(),
            _dir: dir,
        }
    }
    async fn key(&self) -> Value {
        identity::create(&self.app, None, None, "127.0.0.1", &json!({"name":"test"}))
            .await
            .unwrap()
    }
    async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> (u16, Value) {
        let router = reseller::web::router(self.app.clone()).await;
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let mut req = req
            .body(if method == "GET" {
                Body::empty()
            } else {
                Body::from(body.to_string())
            })
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("127.0.0.1:9000".parse::<SocketAddr>().unwrap()));
        let r = router.oneshot(req).await.unwrap();
        let status = r.status().as_u16();
        let b = to_bytes(r.into_body(), 10 * 1024 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&b)
                .unwrap_or_else(|_| json!({"text":String::from_utf8_lossy(&b)})),
        )
    }
    async fn identity(&self, key: &Value) -> identity::Identity {
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            format!("Bearer {}", key["key"].as_str().unwrap())
                .parse()
                .unwrap(),
        );
        identity::required(&self.app, &h).await.unwrap()
    }
}
async fn upstream(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(l, router).await.unwrap();
    });
    (url, task)
}
fn meta() -> RequestMeta<'static> {
    RequestMeta {
        method: "POST",
        path: "/v1/chat/completions",
        kind: "llm",
        model: "test-model",
        ip: "127.0.0.1",
    }
}
const ADMIN: &str = "admin-test-secret-123456789";

#[test]
fn decimal_money_is_exact_and_rejects_invalid_values() {
    assert_eq!(money::usd("0.000001").unwrap(), 1);
    assert_eq!(money::usd("20.25").unwrap(), 20_250_000);
    assert_eq!(money::markup(101, "30").unwrap(), 131);
    assert_eq!(money::display(1), "0.000001");
    for s in ["NaN", "inf", "-1", "1000000001"] {
        assert!(money::usd(s).is_err());
    }
}
#[test]
fn explicit_zero_provider_cost_overrides_price_table() {
    let c = Config::default();
    let u = Usage::parse(&json!({"prompt_tokens":1000,"cost":0}));
    assert_eq!(u.cost(&c, "unknown").unwrap(), 0);
    let u = Usage::parse(&json!({"input_tokens":1000,"output_tokens":100}));
    assert_eq!(u.total, 1100);
    assert_eq!(u.cost(&c, "unknown").unwrap(), 650);
}
#[test]
fn target_preserves_base_and_query_but_rejects_traversal() {
    assert_eq!(
        reseller::proxy::target(
            "https://example.com/api/v1/",
            "/v1/chat/completions",
            Some("a=1&a=2")
        )
        .unwrap()
        .as_str(),
        "https://example.com/api/v1/chat/completions?a=1&a=2"
    );
    for p in ["/v1/../secret", "/v1/%2e%2e/secret", "/v1/%2e%2e%2fsecret"] {
        assert!(reseller::proxy::target("https://example.com/api/v1", p, None).is_err());
    }
}
#[test]
fn sse_inspector_handles_split_utf8_usage_and_done() {
    let mut i = reseller::proxy::Inspector::default();
    let s = "data: {\"choices\":[{\"delta\":{\"content\":\"世界\"}}]}\r\ndata: {\"response\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":9,\"cost\":0.001}}}\n\ndata: [DONE]\n\n";
    for byte in s.as_bytes().chunks(1) {
        i.feed(byte, "text/event-stream");
    }
    i.finish("text/event-stream");
    assert_eq!(i.usage.total, 16);
    assert_eq!(i.usage.cost, Some(1000));
}
#[test]
fn sse_inspector_recovers_after_oversized_line() {
    let mut i = reseller::proxy::Inspector::default();
    i.feed(&vec![b'x'; 5 * 1024 * 1024], "text/event-stream");
    i.feed(
        b"\ndata: {\"usage\":{\"total_tokens\":12}}\n",
        "text/event-stream",
    );
    i.finish("text/event-stream");
    assert_eq!(i.usage.total, 12);
}
#[test]
fn audio_inspection_handles_chunked_wav_header() {
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&48036u32.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&24000u32.to_le_bytes());
    wav.extend_from_slice(&48000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&48000u32.to_le_bytes());
    wav.resize(48044, 0);
    let mut i = reseller::proxy::Inspector::default();
    for c in wav.chunks(3) {
        i.feed(c, "audio/wav");
    }
    i.finish("audio/wav");
    assert_eq!(i.usage.audio_ms, 1000);
}
#[tokio::test]
async fn signup_hashes_secrets_and_dashboard_cannot_proxy() {
    let t = Test::new().await;
    let k = t.key().await;
    let r = sqlx::query("SELECT key_hash,dash_hash FROM api_keys")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_ne!(r.get::<String, _>("key_hash"), k["key"]);
    assert_ne!(r.get::<String, _>("dash_hash"), k["dashboard_token"]);
    assert_eq!(
        t.call(
            "GET",
            "/v1/account",
            k["dashboard_token"].as_str(),
            json!({})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        t.call(
            "POST",
            "/v1/chat/completions",
            k["dashboard_token"].as_str(),
            json!({})
        )
        .await
        .0,
        401
    );
    assert_eq!(
        t.call("GET", "/admin/api/overview", None, json!({}))
            .await
            .0,
        401
    );
}
#[tokio::test]
async fn concurrent_signup_cannot_exceed_monthly_allowance() {
    let t = Test::new().await;
    t.app.config.write().await.ip_limit_per_month = 2;
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let app = t.app.clone();
        tasks.push(tokio::spawn(async move {
            identity::create(&app, None, None, "203.0.113.7", &json!({})).await
        }));
    }
    let mut successes = 0;
    for task in tasks {
        if task.await.unwrap().is_ok() {
            successes += 1;
        }
    }
    assert_eq!(successes, 2);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(count, 2);
}
#[tokio::test]
async fn child_keys_share_credit_without_trial_mining() {
    let t = Test::new().await;
    let k = t.key().await;
    let i = t.identity(&k).await;
    let child = identity::create(&t.app, Some(&i), None, "127.0.0.1", &json!({}))
        .await
        .unwrap();
    assert_eq!(child["account_id"], k["account_id"]);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM credits")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(n, 1);
}
#[tokio::test]
async fn ownership_block_rotation_and_permanent_revocation() {
    let t = Test::new().await;
    let a = t.key().await;
    let b = t.key().await;
    let id = a["id"].as_str().unwrap();
    assert_eq!(
        t.call(
            "PATCH",
            &format!("/v1/keys/{id}"),
            b["key"].as_str(),
            json!({"name":"stolen"})
        )
        .await
        .0,
        404
    );
    assert_eq!(
        t.call(
            "PATCH",
            &format!("/v1/keys/{id}"),
            a["key"].as_str(),
            json!({"status":"active"})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        t.call(
            "PATCH",
            &format!("/admin/api/keys/{id}"),
            Some(ADMIN),
            json!({"markup_pct":"45","limits":{"rpm":3},"expires_at":db::now()+86400})
        )
        .await
        .0,
        200
    );
    let (s, rotated) = t
        .call(
            "POST",
            &format!("/v1/keys/{id}/rotate"),
            a["dashboard_token"].as_str(),
            json!({}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(rotated["account_id"], a["account_id"]);
    assert_eq!(
        t.call(
            "GET",
            "/v1/account",
            a["dashboard_token"].as_str(),
            json!({})
        )
        .await
        .0,
        200
    );
    let r = sqlx::query("SELECT limits,markup_pct,expires_at FROM api_keys WHERE id=?")
        .bind(rotated["id"].as_str())
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(r.get::<String, _>("markup_pct"), "45");
    assert!(r.get::<Option<i64>, _>("expires_at").is_some());
    t.call(
        "POST",
        &format!("/admin/api/keys/{id}/revoke"),
        Some(ADMIN),
        json!({}),
    )
    .await;
    assert_eq!(
        t.call(
            "GET",
            "/v1/account",
            a["dashboard_token"].as_str(),
            json!({})
        )
        .await
        .0,
        401
    );
    assert_eq!(
        t.call(
            "PATCH",
            &format!("/admin/api/keys/{id}"),
            Some(ADMIN),
            json!({"status":"active"})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        t.call(
            "POST",
            &format!("/admin/api/keys/{id}/rotate"),
            Some(ADMIN),
            json!({})
        )
        .await
        .0,
        400
    );
}
#[tokio::test]
async fn credit_uses_earliest_expiry_and_preserves_debt() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap();
    let mut tx = t.app.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("DELETE FROM credits")
        .execute(&mut *tx)
        .await
        .unwrap();
    billing::credit(&mut tx, aid, 100, "purchase", None, "")
        .await
        .unwrap();
    billing::credit(
        &mut tx,
        aid,
        200,
        "subscription",
        Some(db::now() + 1000),
        "",
    )
    .await
    .unwrap();
    billing::debit(&mut tx, aid, 250).await.unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT remaining_micro FROM credits WHERE source='purchase'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(remaining, 50);
    billing::debit(&mut tx, aid, 100).await.unwrap();
    assert_eq!(db::balance(&mut tx, aid).await.unwrap(), -50);
    billing::credit(&mut tx, aid, 200, "admin", None, "")
        .await
        .unwrap();
    assert_eq!(db::balance(&mut tx, aid).await.unwrap(), 150);
    tx.commit().await.unwrap();
}
#[tokio::test]
async fn expired_credit_is_not_spendable() {
    let t = Test::new().await;
    let k = t.key().await;
    sqlx::query("UPDATE credits SET expires_at=?")
        .bind(db::now() - 1)
        .execute(&t.app.db)
        .await
        .unwrap();
    let result = Meter::begin(&t.app, Some(t.identity(&k).await), meta()).await;
    assert!(matches!(result,Err(e) if e.status.as_u16()==402));
}
#[tokio::test]
async fn redemption_is_atomic_under_retries() {
    let t = Test::new().await;
    let k = t.key().await;
    let (s, v) = t
        .call(
            "POST",
            "/admin/api/codes",
            Some(ADMIN),
            json!({"credit_usd":"10"}),
        )
        .await;
    assert_eq!(s, 200);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let app = t.app.clone();
        let aid = k["account_id"].as_str().unwrap().to_owned();
        let code = v["code"].as_str().unwrap().to_owned();
        tasks.push(tokio::spawn(async move {
            billing::redeem(&app, &aid, &code).await
        }));
    }
    let mut successes = 0;
    for task in tasks {
        if task.await.unwrap().is_ok() {
            successes += 1;
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(
        billing::account(&t.app, k["account_id"].as_str().unwrap())
            .await
            .unwrap()["account"]["balance_usd"],
        "10.25"
    );
}
#[tokio::test]
async fn plan_grants_expiring_credit_and_bypasses_ip_allowance() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap();
    t.app.config.write().await.ip_limit_per_month = 1;
    let i = t.identity(&k).await;
    assert!(
        identity::create(&t.app, Some(&i), None, "127.0.0.1", &json!({}))
            .await
            .is_err()
    );
    let (s, _) = t
        .call(
            "POST",
            &format!("/admin/api/accounts/{aid}/subscriptions"),
            Some(ADMIN),
            json!({"plan_code":"monthly"}),
        )
        .await;
    assert_eq!(s, 200);
    assert!(
        identity::create(&t.app, Some(&i), None, "127.0.0.1", &json!({}))
            .await
            .is_ok()
    );
    let v = billing::account(&t.app, aid).await.unwrap();
    assert_eq!(v["account"]["balance_usd"], "20.25");
    assert!(v["account"]["subscription"]["expires_at"].as_i64().unwrap() > db::now());
}
#[tokio::test]
async fn request_admission_counts_concurrent_inflight_requests() {
    let t = Test::new().await;
    let k = t.key().await;
    let mut i = t.identity(&k).await;
    i.limits.requests = Some(identity::Window {
        max: 1,
        window_hours: 24,
    });
    let m = Meter::begin(&t.app, Some(i.clone()), meta()).await.unwrap();
    let second = Meter::begin(&t.app, Some(i), meta()).await;
    assert!(matches!(second,Err(e) if e.code=="request_limit_exceeded"));
    m.finish().await.unwrap();
}
#[tokio::test]
async fn rpm_is_persistent_and_model_allowlist_applies() {
    let t = Test::new().await;
    let k = t.key().await;
    let mut i = t.identity(&k).await;
    i.limits.rpm = Some(1);
    i.limits.allowed_models = vec!["other".into()];
    assert!(
        matches!(Meter::begin(&t.app,Some(i.clone()),meta()).await,Err(e) if e.code=="model_not_allowed")
    );
    i.limits.allowed_models = vec!["test-model".into()];
    Meter::begin(&t.app, Some(i.clone()), meta())
        .await
        .unwrap()
        .finish()
        .await
        .unwrap();
    assert!(
        matches!(Meter::begin(&t.app,Some(i),meta()).await,Err(e) if e.code=="rate_limit_exceeded")
    );
}
#[tokio::test]
async fn metering_commits_debit_log_and_aggregates_once() {
    let t = Test::new().await;
    let k = t.key().await;
    let mut m = Meter::begin(&t.app, Some(t.identity(&k).await), meta())
        .await
        .unwrap();
    m.status = 200;
    m.usage = Usage::parse(&json!({"total_tokens":50,"cost":0.01}));
    m.finish().await.unwrap();
    let v = reseller::stats::overview(&t.app).await.unwrap();
    assert_eq!(v["requests"], 1);
    assert_eq!(v["billed_cost_usd"], "0.013");
    assert_eq!(v["tokens"], 50);
    let a = billing::account(&t.app, k["account_id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(a["account"]["balance_usd"], "0.237");
    let r = sqlx::query("SELECT finished,billed_micro FROM requests")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(r.get::<i64, _>("finished"), 1);
    assert_eq!(r.get::<i64, _>("billed_micro"), 13000);
}
#[tokio::test]
async fn dropped_stream_settles_observed_usage() {
    let t = Test::new().await;
    let k = t.key().await;
    let mut m = Meter::begin(&t.app, Some(t.identity(&k).await), meta())
        .await
        .unwrap();
    m.usage = Usage::parse(&json!({"cost":0.01}));
    drop(m);
    t.app.tasks.close();
    t.app.tasks.wait().await;
    let r = sqlx::query("SELECT status,billed_micro FROM requests")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(r.get::<i64, _>("status"), 499);
    assert_eq!(r.get::<i64, _>("billed_micro"), 13000);
}
#[tokio::test]
async fn settings_are_redacted_validated_and_persisted() {
    let t = Test::new().await;
    let (s, v) = t
        .call("GET", "/admin/api/settings", Some(ADMIN), json!({}))
        .await;
    assert_eq!(s, 200);
    assert_eq!(v["settings"]["admin_token"], "");
    assert_eq!(v["settings"]["key_pepper"], "");
    assert_eq!(
        t.call(
            "PATCH",
            "/admin/api/settings",
            Some(ADMIN),
            json!({"llm":{"base_url":"file:///secret"}})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        t.call(
            "PATCH",
            "/admin/api/settings",
            Some(ADMIN),
            json!({"unknown":true})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        t.call(
            "PATCH",
            "/admin/api/settings",
            Some(ADMIN),
            json!({"markup_pct":"12.5","admin_token":""})
        )
        .await
        .0,
        200
    );
    let reloaded = db::settings(&t.app.db, Config::default()).await.unwrap();
    assert_eq!(reloaded.markup_pct, "12.5");
    assert_eq!(reloaded.admin_token, ADMIN);
}
#[tokio::test]
async fn json_proxy_rewrites_model_strips_secrets_and_bills() {
    let t = Test::new().await;
    let seen = Arc::new(AtomicUsize::new(0));
    let seen2 = seen.clone();
    let (base,task)=upstream(Router::new().fallback(any(move|req:Request<Body>|{let seen=seen2.clone();async move {
  assert_eq!(req.uri().to_string(),"/api/v1/chat/completions?test=1");assert_eq!(req.headers()["authorization"],"Bearer upstream-secret");assert!(!req.headers().contains_key("x-admin-token"));assert!(!req.headers().contains_key("cookie"));
  let v:Value=serde_json::from_slice(&to_bytes(req.into_body(),1024*1024).await.unwrap()).unwrap();assert_eq!(v["model"],"configured");seen.fetch_add(1,Ordering::SeqCst);
  axum::Json(json!({"id":"fake","usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30,"cost":0.01}}))
 }}))).await;
    {
        let mut c = t.app.config.write().await;
        c.llm.base_url = format!("{base}/api/v1");
        c.llm.api_key = "upstream-secret".into();
        c.llm.model = "configured".into();
    }
    let k = t.key().await;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions?test=1")
        .header(
            "authorization",
            format!("Bearer {}", k["key"].as_str().unwrap()),
        )
        .header("content-type", "application/json")
        .header("x-admin-token", "must-not-leak")
        .header("cookie", "secret=1")
        .body(Body::from("{\"messages\":[]}"))
        .unwrap();
    let r = reseller::proxy::handle(t.app.clone(), req, "127.0.0.1".into())
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().contains_key("x-request-id"));
    to_bytes(r.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert_eq!(
        reseller::stats::overview(&t.app).await.unwrap()["billed_cost_usd"],
        "0.013"
    );
    task.abort();
}
#[tokio::test]
async fn sse_proxy_streams_before_upstream_finishes_and_tracks_final_usage() {
    let t = Test::new().await;
    let (base,task)=upstream(Router::new().fallback(any(|req:Request<Body>|async move {
  let v:Value=serde_json::from_slice(&to_bytes(req.into_body(),10000).await.unwrap()).unwrap();assert_eq!(v["stream_options"]["include_usage"],true);
  let stream=async_stream::stream! {yield Ok::<_,std::io::Error>(bytes::Bytes::from_static(b"data: {\"choices\":[]}\n\n"));tokio::time::sleep(std::time::Duration::from_millis(150)).await;yield Ok(bytes::Bytes::from_static(b"data: {\"usage\":{\"total_tokens\":9,\"cost\":0.002}}\n\ndata: [DONE]\n\n"));};
  ([("content-type","text/event-stream")],Body::from_stream(stream)).into_response()
 })) ).await;
    {
        let mut c = t.app.config.write().await;
        c.llm.base_url = format!("{base}/v1");
        c.llm.api_key = "upstream".into();
    }
    let k = t.key().await;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(
            "authorization",
            format!("Bearer {}", k["key"].as_str().unwrap()),
        )
        .header("content-type", "application/json")
        .body(Body::from("{\"stream\":true,\"messages\":[]}"))
        .unwrap();
    let r = reseller::proxy::handle(t.app.clone(), req, "127.0.0.1".into())
        .await
        .unwrap();
    use futures_util::StreamExt;
    let mut s = r.into_body().into_data_stream();
    let first = tokio::time::timeout(std::time::Duration::from_millis(100), s.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(first.starts_with(b"data:"));
    while s.next().await.is_some() {}
    assert_eq!(
        reseller::stats::overview(&t.app).await.unwrap()["tokens"],
        9
    );
    task.abort();
}
#[tokio::test]
async fn fallback_retries_configured_models_only() {
    let t = Test::new().await;
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let (base, task) = upstream(Router::new().fallback(any(move |req: Request<Body>| {
        let seen = seen.clone();
        async move {
            seen.fetch_add(1, Ordering::SeqCst);
            let v: Value =
                serde_json::from_slice(&to_bytes(req.into_body(), 10000).await.unwrap()).unwrap();
            if v["model"] == "backup" {
                (
                    axum::http::StatusCode::OK,
                    axum::Json(json!({"usage":{"cost":0}})),
                )
                    .into_response()
            } else {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(json!({"error":"capacity"})),
                )
                    .into_response()
            }
        }
    })))
    .await;
    {
        let mut c = t.app.config.write().await;
        c.llm.base_url = format!("{base}/v1");
        c.llm.api_key = "upstream".into();
        c.llm.model = "primary".into();
        c.llm.fallbacks = vec!["backup".into()];
        c.model_policy = Policy::Default;
    }
    let k = t.key().await;
    assert_eq!(
        t.call(
            "POST",
            "/v1/chat/completions",
            k["key"].as_str(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(
        t.call(
            "POST",
            "/v1/chat/completions",
            k["key"].as_str(),
            json!({"model":"custom"})
        )
        .await
        .0,
        503
    );
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    task.abort();
}
#[tokio::test]
async fn multipart_is_streamed_unchanged_to_stt() {
    let t = Test::new().await;
    let payload =
        "--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n--b--\r\n";
    let (base, task) = upstream(Router::new().fallback(any(
        move |req: Request<Body>| async move {
            assert_eq!(req.uri().path(), "/api/audio/transcriptions");
            let b = to_bytes(req.into_body(), 10000).await.unwrap();
            assert_eq!(b.as_ref(), payload.as_bytes());
            axum::Json(json!({"text":"hello","usage":{"seconds":1}}))
        },
    )))
    .await;
    {
        let mut c = t.app.config.write().await;
        c.stt.base_url = format!("{base}/api");
        c.stt.api_key = "stt-secret".into();
    }
    let k = t.key().await;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/audio/transcriptions")
        .header(
            "authorization",
            format!("Bearer {}", k["key"].as_str().unwrap()),
        )
        .header("content-type", "multipart/form-data; boundary=b")
        .body(Body::from(payload))
        .unwrap();
    let r = reseller::proxy::handle(t.app.clone(), req, "127.0.0.1".into())
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    to_bytes(r.into_body(), 10000).await.unwrap();
    task.abort();
}
#[tokio::test]
async fn request_body_limit_is_enforced_before_upstream() {
    let t = Test::new().await;
    let k = t.key().await;
    {
        let mut c = t.app.config.write().await;
        c.max_json_body_bytes = 10;
        c.llm.api_key = "upstream".into();
    }
    assert_eq!(
        t.call(
            "POST",
            "/v1/chat/completions",
            k["key"].as_str(),
            json!({"messages":["too large"]})
        )
        .await
        .0,
        413
    );
}
#[tokio::test]
async fn anonymous_mode_does_not_bill_accounts() {
    let t = Test::new().await;
    {
        let mut c = t.app.config.write().await;
        c.require_key = false;
        c.accept_any_token = false;
    }
    let mut h = HeaderMap::new();
    assert!(
        identity::authenticate(&t.app, &h, true)
            .await
            .unwrap()
            .is_none()
    );
    h.insert("authorization", "Bearer placeholder".parse().unwrap());
    assert!(identity::authenticate(&t.app, &h, true).await.is_err());
    t.app.config.write().await.accept_any_token = true;
    assert!(
        identity::authenticate(&t.app, &h, true)
            .await
            .unwrap()
            .is_none()
    );
    let mut m = Meter::begin(&t.app, None, meta()).await.unwrap();
    m.usage = Usage::parse(&json!({"cost":0.02}));
    m.finish().await.unwrap();
    let v = reseller::stats::overview(&t.app).await.unwrap();
    assert_eq!(v["billed_cost_usd"], "0");
    assert_eq!(v["upstream_cost_usd"], "0.02");
}
fn signature(secret: &str, body: &[u8], timestamp: i64) -> String {
    use hmac::{Hmac, Mac};
    let mut m = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    m.update(format!("{timestamp}.").as_bytes());
    m.update(body);
    format!(
        "t={timestamp},v1={}",
        hex::encode(m.finalize().into_bytes())
    )
}
#[test]
fn stripe_signature_rejects_tampering_and_replay() {
    let body = b"{\"id\":\"evt_test\"}";
    let h = signature("whsec_test", body, 1000);
    assert!(reseller::payments::verify("whsec_test", &h, body, 1000).is_ok());
    assert!(reseller::payments::verify("whsec_test", &h, b"{}", 1000).is_err());
    assert!(reseller::payments::verify("whsec_test", &h, body, 1301).is_err());
    assert!(reseller::payments::verify("", &h, body, 1000).is_err());
}
#[tokio::test]
async fn payment_webhook_uses_snapshot_and_grants_only_once() {
    let t = Test::new().await;
    let k = t.key().await;
    t.app.config.write().await.stripe_webhook_secret = "whsec_test".into();
    let aid = k["account_id"].as_str().unwrap();
    sqlx::query("INSERT INTO payments(id,account_id,plan_code,price_micro,credit_micro,duration_days,session_id,created_at) VALUES('order1',?,'monthly',20000000,25000000,40,'cs_test',?)").bind(aid).bind(db::now()).execute(&t.app.db).await.unwrap();
    let mut event = json!({"id":"evt_one","type":"checkout.session.completed","data":{"object":{"id":"cs_test","client_reference_id":"order1","metadata":{"order_id":"order1"},"mode":"payment","currency":"usd","amount_total":2000,"payment_status":"paid"}}});
    for eid in ["evt_one", "evt_one", "evt_two"] {
        event["id"] = eid.into();
        let body = event.to_string();
        let mut h = HeaderMap::new();
        h.insert(
            "stripe-signature",
            signature("whsec_test", body.as_bytes(), db::now())
                .parse()
                .unwrap(),
        );
        reseller::payments::webhook(&t.app, &h, body.as_bytes())
            .await
            .unwrap();
    }
    let v = billing::account(&t.app, aid).await.unwrap();
    assert_eq!(v["account"]["balance_usd"], "25.25");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM subscriptions")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let days = (v["account"]["subscription"]["expires_at"].as_i64().unwrap() - db::now()) / 86400;
    assert!((39..=40).contains(&days));
}
#[tokio::test]
async fn payment_amount_mismatch_rolls_back_event_and_credit() {
    let t = Test::new().await;
    let k = t.key().await;
    t.app.config.write().await.stripe_webhook_secret = "whsec_test".into();
    sqlx::query("INSERT INTO payments(id,account_id,plan_code,price_micro,credit_micro,duration_days,session_id,created_at) VALUES('order1',?,'monthly',20000000,20000000,30,'cs_test',?)").bind(k["account_id"].as_str()).bind(db::now()).execute(&t.app.db).await.unwrap();
    let event=json!({"id":"evt_bad","type":"checkout.session.completed","data":{"object":{"id":"cs_test","client_reference_id":"order1","metadata":{"order_id":"order1"},"mode":"payment","currency":"usd","amount_total":1,"payment_status":"paid"}}}).to_string();
    let mut h = HeaderMap::new();
    h.insert(
        "stripe-signature",
        signature("whsec_test", event.as_bytes(), db::now())
            .parse()
            .unwrap(),
    );
    assert!(
        reseller::payments::webhook(&t.app, &h, event.as_bytes())
            .await
            .is_err()
    );
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payment_events")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(n, 0);
}
#[test]
fn starter_config_documents_every_option_and_validates() {
    // `reseller init` copies this file verbatim, so it must parse, pass
    // validation, and mention every configurable field at least once.
    let text = include_str!("../assets/reseller.example.toml");
    let c: Config = toml::from_str(text).unwrap();
    c.validate().unwrap();
    for key in [
        // Config
        "site_name",
        "public_url",
        "admin_token",
        "key_pepper",
        "llm",
        "tts",
        "stt",
        "model_policy",
        "voice_policy",
        "markup_pct",
        "free_credit_usd",
        "free_credit_expiry_days",
        "prices",
        "require_key",
        "accept_any_token",
        "ip_limit_per_month",
        "cooldown_seconds",
        "global_hourly_limit",
        "max_keys_per_account",
        "ip_overrides",
        "default_limits",
        "trust_proxy",
        "cors_origin",
        "max_json_body_bytes",
        "max_stream_body_bytes",
        "upstream_timeout_seconds",
        "log_retention_days",
        "max_log_entries",
        "stripe_secret_key",
        "stripe_webhook_secret",
        // Upstream
        "base_url",
        "api_key",
        "model",
        "voice",
        "fallbacks",
        // Price
        "input",
        "output",
        "audio_second",
        // Limits / windows
        "rpm",
        "requests",
        "tokens",
        "spend",
        "max_children",
        "allowed_models",
        "window_hours",
        "max_usd",
    ] {
        assert!(
            text.contains(key),
            "starter config does not document `{key}`"
        );
    }
    // The starter file must be directly usable: upstreams present, no secrets
    // baked in, and the placeholder key clearly marked.
    assert!(c.llm.api_key.contains("replace"));
    assert!(c.admin_token.is_empty() && c.key_pepper.is_empty());
}
