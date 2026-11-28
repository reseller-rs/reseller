//! Multi-provider routing and the keyless free tier on `/v1`: route
//! selection and failover, circuit breakers, provider cooldowns, free-tier
//! admission and limits, per-client grouping, the models list by caller,
//! `route_usage`, and the administrator routing view. Every upstream is a mock
//! axum server.

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, Request},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use reseller::{
    App,
    config::{Budget, Capability, Config, Strategy, Tier},
    db, keys,
};
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU16, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;
use tower::ServiceExt;

mod common;

const ADMIN: &str = "admin-test-secret-123456789";

struct Reply {
    status: u16,
    headers: HeaderMap,
    body: Value,
    /// The body as received.
    text: String,
}

impl Reply {
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }

    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_else(|| panic!("no {name} header in {:?}", self.headers))
    }
}

/// Sends one request through the real router as if from `ip`.
async fn send(
    app: &App,
    ip: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> Reply {
    let router = reseller::web::router(app.clone()).await;
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
    let peer = if ip.contains(':') {
        format!("[{ip}]:9000")
    } else {
        format!("{ip}:9000")
    };
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    let r = router.oneshot(req).await.unwrap();
    let status = r.status().as_u16();
    let headers = r.headers().clone();
    let bytes = to_bytes(r.into_body(), 64 * 1024 * 1024).await.unwrap();
    Reply {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

struct Test {
    app: App,
    _dir: tempfile::TempDir,
    mocks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Test {
    fn drop(&mut self) {
        for task in &self.mocks {
            task.abort();
        }
    }
}

impl Test {
    async fn new(tweak: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("test.db")).await.unwrap();
        let mut c = Config {
            admin_token: ADMIN.into(),
            key_pepper: "pepper-test-secret-12345678".into(),
            markup_pct: "0".into(),
            signup: reseller::config::Signup {
                cooldown_seconds: 0,
                global_hourly_limit: 0,
                pow_max_number: 0,
                ..Default::default()
            },
            rate_limits: reseller::config::RateLimits {
                new_account_hours: 0,
                ..Default::default()
            },
            ..Config::default()
        };
        tweak(&mut c);
        let c = db::settings(&pool, c).await.unwrap();
        Self {
            app: App::new(pool, c).unwrap(),
            _dir: dir,
            mocks: Vec::new(),
        }
    }

    /// Starts an upstream for the provider `id` and returns its handle.
    async fn upstream(&mut self, id: &str, up: &Up) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", l.local_addr().unwrap());
        let router = up.router();
        self.mocks.push(tokio::spawn(async move {
            axum::serve(l, router).await.unwrap();
        }));
        let mut c = self.app.config_mut().await;
        common::provider(&mut c, id, &url, "upstream-secret");
    }

    /// Adds a chat model `id` with the given routes (provider, upstream model).
    async fn model(&self, id: &str, tier: Tier, strategy: Strategy, routes: &[(&str, &str)]) {
        let mut c = self.app.config_mut().await;
        let (provider, upstream) = routes[0];
        let model = common::model(&mut c, id, Capability::Chat, tier, provider, upstream);
        model.strategy = strategy;
        for (provider, upstream) in &routes[1..] {
            model.routes.push(reseller::config::Route {
                provider: (*provider).into(),
                model: (*upstream).into(),
                ..Default::default()
            });
        }
    }

    async fn account(&self, funded: bool) -> Value {
        let key = keys::create(
            &self.app,
            keys::Requester::Signup { altcha: None },
            "10.0.0.1",
            &json!({"name":"k"}),
        )
        .await
        .unwrap();
        if funded {
            common::fund(&self.app, key["account_id"].as_str().unwrap(), common::DIME).await;
        }
        key
    }

    async fn chat(&self, ip: &str, token: Option<&str>, model: Option<&str>) -> Reply {
        let mut body = json!({"messages":[{"role":"user","content":"hi"}]});
        if let Some(model) = model {
            body["model"] = model.into();
        }
        send(&self.app, ip, "POST", "/v1/chat/completions", token, body).await
    }

    async fn requests(&self) -> Vec<sqlx::sqlite::SqliteRow> {
        // The exchange task settles before it closes the body, but a request
        // that failed early settles on its own task: give it a moment.
        tokio::time::sleep(Duration::from_millis(50)).await;
        sqlx::query("SELECT * FROM requests ORDER BY created_at,rowid")
            .fetch_all(&self.app.db)
            .await
            .unwrap()
    }
}

/// A mock upstream that records request bodies and answers with a settable
/// status, headers and (optionally) a gate that holds the response.
#[derive(Clone)]
struct Up {
    hits: Arc<Mutex<Vec<Value>>>,
    status: Arc<AtomicU16>,
    headers: Arc<Mutex<Vec<(String, String)>>>,
    gate: Option<Arc<Semaphore>>,
    cost: Arc<Mutex<Option<f64>>>,
    tokens: Arc<AtomicUsize>,
    /// The error body, when not the default one.
    error: Arc<Mutex<Option<Value>>>,
    /// Answers like an upstream that names itself: its own model and a
    /// `provider` key, as JSON or (for `stream: true`) as an event stream.
    leaky: bool,
}

impl Up {
    fn new(status: u16) -> Self {
        Self {
            hits: Arc::default(),
            status: Arc::new(AtomicU16::new(status)),
            headers: Arc::default(),
            gate: None,
            cost: Arc::default(),
            tokens: Arc::new(AtomicUsize::new(30)),
            error: Arc::default(),
            leaky: false,
        }
    }

    fn leaky(mut self) -> Self {
        self.leaky = true;
        self
    }

    fn with_headers(self, headers: &[(&str, &str)]) -> Self {
        *self.headers.lock().unwrap() = headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        self
    }

    fn with_error(self, body: Value) -> Self {
        *self.error.lock().unwrap() = Some(body);
        self
    }

    fn gated(mut self) -> (Self, Arc<Semaphore>) {
        let gate = Arc::new(Semaphore::new(0));
        self.gate = Some(gate.clone());
        (self, gate)
    }

    fn hits(&self) -> usize {
        self.hits.lock().unwrap().len()
    }

    fn last(&self) -> Value {
        self.hits.lock().unwrap().last().cloned().unwrap()
    }

    fn router(&self) -> Router {
        let up = self.clone();
        Router::new().fallback(any(move |req: Request| {
            let up = up.clone();
            async move {
                let bytes = to_bytes(req.into_body(), 10_000_000).await.unwrap();
                let sent: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let streaming = sent["stream"] == true;
                up.hits.lock().unwrap().push(sent);
                if up.leaky {
                    return leaky_reply(streaming);
                }
                if let Some(gate) = &up.gate {
                    drop(gate.acquire().await.unwrap());
                }
                let status = StatusCode::from_u16(up.status.load(Ordering::SeqCst)).unwrap();
                let body = if status.is_success() {
                    let mut usage = json!({
                        "prompt_tokens": 10,
                        "completion_tokens": 20,
                        "total_tokens": up.tokens.load(Ordering::SeqCst),
                    });
                    if let Some(cost) = *up.cost.lock().unwrap() {
                        usage["cost"] = cost.into();
                    }
                    json!({"id": "mock", "choices": [], "usage": usage})
                } else {
                    up.error.lock().unwrap().clone().unwrap_or_else(|| {
                        json!({"error": {"message": "upstream says no", "type": "x", "code": "upstream_code"}})
                    })
                };
                let mut response: Response = (status, Json(body)).into_response();
                for (name, value) in up.headers.lock().unwrap().iter() {
                    response.headers_mut().insert(
                        axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                        value.parse().unwrap(),
                    );
                }
                response
            }
        }))
    }
}

/// What an upstream that names itself answers.
fn leaky_reply(streaming: bool) -> Response {
    let usage = r#""usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30}"#;
    if streaming {
        let body = format!(
            concat!(
                ": keep-alive\n\n",
                "data: {{\"id\":\"c\",\"model\":\"{m}\",\"provider\":\"Nvidia\",\"choices\":[{{\"delta\":{{\"content\":\"hi\"}}}}]}}\n\n",
                "data: {{\"id\":\"c\",\"model\":\"{m}\",\"provider\":\"Nvidia\",\"choices\":[],{u}}}\n\n",
                "data: [DONE]\n\n"
            ),
            m = LEAKED_MODEL,
            u = usage
        );
        ([("content-type", "text/event-stream")], body).into_response()
    } else {
        let body = format!(
            r#"{{"id":"c","model":"{LEAKED_MODEL}","provider":"Nvidia","choices":[],{usage}}}"#
        );
        ([("content-type", "application/json")], body).into_response()
    }
}

const LEAKED_MODEL: &str = "nvidia/nemotron-3-nano:free";
const IP: &str = "198.51.100.10";

// ---------------------------------------------------------------------------
// Route selection and failover
// ---------------------------------------------------------------------------

#[tokio::test]
async fn balanced_spreads_requests_and_ordered_takes_the_first_route() {
    let mut t = Test::new(|_| {}).await;
    let (a, b) = (Up::new(200), Up::new(200));
    t.upstream("pa", &a).await;
    t.upstream("pb", &b).await;
    t.model(
        "spread",
        Tier::Paid,
        Strategy::Balanced,
        &[("pa", "m"), ("pb", "m")],
    )
    .await;
    t.model(
        "first",
        Tier::Paid,
        Strategy::Ordered,
        &[("pa", "o"), ("pb", "o")],
    )
    .await;
    let k = t.account(true).await;
    let key = k["key"].as_str();
    for _ in 0..6 {
        let r = t.chat(IP, key, Some("spread")).await;
        assert_eq!(r.status, 200, "{}", r.body);
    }
    assert_eq!(
        (a.hits(), b.hits()),
        (3, 3),
        "equal routes share the traffic"
    );
    for _ in 0..4 {
        assert_eq!(t.chat(IP, key, Some("first")).await.status, 200);
    }
    assert_eq!(
        (a.hits(), b.hits()),
        (7, 3),
        "ordered sticks to the first route"
    );
    // The upstream sees its own model name and key, never the public id.
    assert_eq!(a.last()["model"], "o");
}

#[tokio::test]
async fn responses_name_the_public_model_and_hide_the_upstream_provider() {
    let mut t = Test::new(|_| {}).await;
    let up = Up::new(200).leaky();
    t.upstream("pa", &up).await;
    t.model(
        "public-model",
        Tier::Paid,
        Strategy::Ordered,
        &[("pa", LEAKED_MODEL)],
    )
    .await;
    let k = t.account(true).await;
    let key: &'static str = Box::leak(k["key"].as_str().unwrap().to_owned().into_boxed_str());
    let ask = |token: &'static str, stream: bool| {
        let t = &t;
        async move {
            send(
                &t.app,
                IP,
                "POST",
                "/v1/chat/completions",
                Some(token),
                json!({"model":"public-model","stream":stream,"messages":[{"role":"user","content":"hi"}]}),
            )
            .await
        }
    };
    let mut sent = Vec::new();
    for token in [key, ADMIN] {
        let plain = ask(token, false).await;
        assert_eq!(plain.status, 200, "{}", plain.text);
        assert_eq!(plain.body["model"], "public-model");
        assert!(plain.body.get("provider").is_none());
        assert_eq!(plain.body["usage"]["total_tokens"], 30);
        assert!(!plain.text.contains("nvidia") && !plain.text.contains("Nvidia"));
        sent.push(plain.text.len() as i64);

        let streamed = ask(token, true).await;
        assert_eq!(streamed.status, 200, "{}", streamed.text);
        assert!(!streamed.text.contains("nvidia") && !streamed.text.contains("Nvidia"));
        sent.push(streamed.text.len() as i64);
        assert!(streamed.text.starts_with(": keep-alive\n\ndata: {"));
        assert!(streamed.text.ends_with("data: [DONE]\n\n"));
        let events: Vec<Value> = streamed
            .text
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str(d).ok())
            .collect();
        assert_eq!(events.len(), 2);
        for event in &events {
            assert_eq!(event["model"], "public-model");
            assert!(event.get("provider").is_none());
        }
        assert_eq!(events[0]["choices"][0]["delta"]["content"], "hi");
    }
    // Metering saw the upstream's usage, and bytes_out counts what was sent.
    let rows = t.requests().await;
    assert_eq!(rows.len(), 4);
    for (row, sent) in rows.iter().zip(sent) {
        assert_eq!(row.get::<i64, _>("tokens"), 30);
        assert_eq!(row.get::<i64, _>("bytes_out"), sent);
    }
}

#[tokio::test]
async fn a_rate_limited_or_failing_route_falls_through_to_the_next() {
    let mut t = Test::new(|_| {}).await;
    let limited = Up::new(429).with_headers(&[("retry-after", "30")]);
    let broken = Up::new(503);
    let good = Up::new(200);
    t.upstream("limited", &limited).await;
    t.upstream("broken", &broken).await;
    t.upstream("good", &good).await;
    t.model(
        "m429",
        Tier::Paid,
        Strategy::Ordered,
        &[("limited", "x"), ("good", "x")],
    )
    .await;
    t.model(
        "m503",
        Tier::Paid,
        Strategy::Ordered,
        &[("broken", "x"), ("good", "y")],
    )
    .await;
    let k = t.account(true).await;
    let key = k["key"].as_str();

    let r = t.chat(IP, key, Some("m429")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        r.headers.get("x-model-fallback").is_none(),
        "no upstream names leak"
    );
    assert_eq!((limited.hits(), good.hits()), (1, 1));
    // The upstream's Retry-After parked the route: it is not asked again.
    for _ in 0..3 {
        assert_eq!(t.chat(IP, key, Some("m429")).await.status, 200);
    }
    assert_eq!((limited.hits(), good.hits()), (1, 4));

    let r = t.chat(IP, key, Some("m503")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        (broken.hits(), good.last()["model"].as_str()),
        (1, Some("y"))
    );

    let rows = t.requests().await;
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0].get::<i64, _>("attempts"), 2);
    assert_eq!(rows[0].get::<String, _>("provider"), "good");
    assert_eq!(
        rows[1].get::<i64, _>("attempts"),
        1,
        "the parked route was skipped"
    );
    let usage: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "SELECT provider,SUM(requests),SUM(failures),SUM(rate_limited) FROM route_usage \
         GROUP BY provider ORDER BY provider",
    )
    .fetch_all(&t.app.db)
    .await
    .unwrap();
    assert_eq!(
        usage,
        [
            ("broken".to_owned(), 1, 1, 0),
            ("good".to_owned(), 5, 0, 0),
            ("limited".to_owned(), 1, 0, 1),
        ]
    );
}

#[tokio::test]
async fn when_every_route_fails_the_last_upstream_error_is_returned() {
    let mut t = Test::new(|c| c.routing.max_attempts = 2).await;
    let (a, b, c) = (Up::new(500), Up::new(502), Up::new(500));
    t.upstream("pa", &a).await;
    t.upstream("pb", &b).await;
    t.upstream("pc", &c).await;
    t.model(
        "m",
        Tier::Paid,
        Strategy::Ordered,
        &[("pa", "x"), ("pb", "x"), ("pc", "x")],
    )
    .await;
    let k = t.account(true).await;
    let r = t.chat(IP, k["key"].as_str(), Some("m")).await;
    // max_attempts = 2: two routes tried, the second one's error is returned.
    assert_eq!(r.status, 502, "{}", r.body);
    assert_eq!(r.body["error"]["message"], "upstream says no");
    assert_eq!((a.hits(), b.hits(), c.hits()), (1, 1, 0));
    let rows = t.requests().await;
    assert_eq!(rows[0].get::<i64, _>("attempts"), 2);
    assert_eq!(rows[0].get::<i64, _>("status"), 502);
    assert_eq!(rows[0].get::<i64, _>("billed_micro"), 0);
}

/// Groq's answer to a request above its per-minute token window.
fn groq_too_large() -> Value {
    json!({"error": {
        "message": "Request too large for model `qwen/qwen3.8-27b` in organization `org_01secret` service tier `on_demand` on input tokens per minute (ITPM): Limit 7000, Requested 7503, please reduce your message size and try again. Need more tokens? Upgrade to Dev Tier today at https://console.groq.com/settings/billing",
        "type": "tokens", "code": "rate_limit_exceeded"}})
}

#[tokio::test]
async fn a_request_too_large_for_a_route_moves_on_without_parking_it() {
    let mut t = Test::new(|_| {}).await;
    let small = Up::new(413)
        .with_headers(&[
            ("retry-after", "9"),
            ("x-ratelimit-remaining-tokens", "8000"),
        ])
        .with_error(groq_too_large());
    let big = Up::new(200);
    t.upstream("small", &small).await;
    t.upstream("big", &big).await;
    t.model(
        "m",
        Tier::Free,
        Strategy::Ordered,
        &[("small", "x"), ("big", "y")],
    )
    .await;
    let r = t.chat(IP, None, Some("m")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!((small.hits(), big.hits()), (1, 1));
    // Smaller requests still fit the route, so it is asked first again.
    small.status.store(200, Ordering::SeqCst);
    assert_eq!(t.chat(IP, None, Some("m")).await.status, 200);
    assert_eq!((small.hits(), big.hits()), (2, 1));

    // When no route can take it, the client is told to shorten the request,
    // and the upstream's account details stay private.
    let mut t = Test::new(|_| {}).await;
    let only = Up::new(413).with_error(groq_too_large());
    t.upstream("only", &only).await;
    t.model("m", Tier::Free, Strategy::Ordered, &[("only", "x")])
        .await;
    let r = t.chat(IP, None, Some("m")).await;
    assert_eq!(
        (r.status, r.code()),
        (413, "request_too_large"),
        "{}",
        r.body
    );
    assert!(!r.text.contains("org_"), "{}", r.text);
    assert!(!r.text.contains("groq"), "{}", r.text);
    assert!(!r.headers.contains_key("retry-after"));
}

#[tokio::test]
async fn a_provider_blocked_in_this_region_is_parked_as_a_whole() {
    let mut t = Test::new(|_| {}).await;
    let blocked = Up::new(400).with_error(json!([{"error": {
        "code": 400,
        "message": "User location is not supported for the API use.",
        "status": "FAILED_PRECONDITION"}}]));
    let good = Up::new(200);
    t.upstream("blocked", &blocked).await;
    t.upstream("good", &good).await;
    t.model(
        "m",
        Tier::Free,
        Strategy::Ordered,
        &[("blocked", "x"), ("good", "y")],
    )
    .await;
    t.model("n", Tier::Free, Strategy::Ordered, &[("blocked", "z")])
        .await;
    assert_eq!(t.chat(IP, None, Some("m")).await.status, 200);
    assert_eq!(t.chat(IP, None, Some("m")).await.status, 200);
    assert_eq!((blocked.hits(), good.hits()), (1, 2));
    // Every route of the provider is parked, and the free caller gets a
    // limit with a retry hint, not the provider's error.
    let r = t.chat(IP, None, Some("n")).await;
    assert_eq!(
        (r.status, r.code()),
        (503, "capacity_exhausted"),
        "{}",
        r.body
    );
    assert!(r.headers.contains_key("retry-after"));
    assert_eq!(blocked.hits(), 1);
}

#[tokio::test]
async fn free_callers_get_a_limit_not_the_providers_rate_limit_text() {
    let mut t = Test::new(|_| {}).await;
    let (a, b) = (
        Up::new(429).with_headers(&[("retry-after", "20")]).with_error(json!({"error": {
            "message": "Rate limit reached in organization org_01secret on requests per minute (RPM): Limit 30, Used 30, Requested 1.",
            "type": "requests", "code": "rate_limit_exceeded"}})),
        Up::new(503).with_error(json!({"error": {"message": "Too many requests, slow down"}})),
    );
    t.upstream("a", &a).await;
    t.upstream("b", &b).await;
    t.model(
        "m",
        Tier::Free,
        Strategy::Ordered,
        &[("a", "x"), ("b", "y")],
    )
    .await;
    let r = t.chat(IP, None, Some("m")).await;
    assert_eq!(
        (r.status, r.code()),
        (503, "capacity_exhausted"),
        "{}",
        r.body
    );
    assert!(
        !r.text.contains("org_") && !r.text.contains("slow down"),
        "{}",
        r.text
    );
    let wait: u64 = r.header("retry-after").parse().unwrap();
    assert!((1..=20).contains(&wait), "{wait}");
    assert_eq!((a.hits(), b.hits()), (1, 1));
    // Both routes are parked: the next request is refused without asking.
    let r = t.chat(IP, None, Some("m")).await;
    assert_eq!(r.status, 503);
    assert_eq!((a.hits(), b.hits()), (1, 1));
}

#[tokio::test]
async fn a_moderation_refusal_does_not_park_the_provider() {
    let mut t = Test::new(|_| {}).await;
    let strict = Up::new(403).with_error(json!({"error": {
        "code": 403,
        "message": "gpt-x requires moderation on OpenAI. Your input was flagged for \"harassment\"."}}));
    let other = Up::new(200);
    t.upstream("strict", &strict).await;
    t.upstream("other", &other).await;
    t.model(
        "m",
        Tier::Free,
        Strategy::Ordered,
        &[("strict", "x"), ("other", "y")],
    )
    .await;
    assert_eq!(t.chat(IP, None, Some("m")).await.status, 200);
    strict.status.store(200, Ordering::SeqCst);
    assert_eq!(t.chat(IP, None, Some("m")).await.status, 200);
    assert_eq!(
        (strict.hits(), other.hits()),
        (2, 1),
        "strict is still first"
    );
    // Alone, the refusal is the client's: 400, not the operator's 502.
    let mut t = Test::new(|_| {}).await;
    let strict = Up::new(403).with_error(json!({"error": {"message": "Your input was flagged"}}));
    t.upstream("strict", &strict).await;
    t.model("m", Tier::Free, Strategy::Ordered, &[("strict", "x")])
        .await;
    let r = t.chat(IP, None, Some("m")).await;
    assert_eq!(r.status, 400, "{}", r.body);
}

#[tokio::test]
async fn the_circuit_opens_after_repeated_failures_and_half_opens_for_a_probe() {
    let mut t = Test::new(|c| {
        c.routing.failure_threshold = 2;
        c.routing.cooldown_seconds = 1;
        c.routing.max_cooldown_seconds = 5;
    })
    .await;
    let up = Up::new(500);
    t.upstream("p", &up).await;
    t.model("m", Tier::Paid, Strategy::Ordered, &[("p", "x")])
        .await;
    let k = t.account(true).await;
    let key = k["key"].as_str();
    for _ in 0..2 {
        assert_eq!(t.chat(IP, key, Some("m")).await.status, 500);
    }
    assert_eq!(up.hits(), 2);
    // Open: no request reaches the upstream, and the client is told when to
    // come back.
    let r = t.chat(IP, key, Some("m")).await;
    assert_eq!(
        (r.status, r.code()),
        (503, "capacity_exhausted"),
        "{}",
        r.body
    );
    let wait: u64 = r.header("retry-after").parse().unwrap();
    assert!((1..=2).contains(&wait), "{wait}");
    assert!(r.headers.contains_key("retry-after-ms"));
    assert_eq!(up.hits(), 2);
    assert_eq!(
        send(
            &t.app,
            IP,
            "GET",
            "/admin/api/routing",
            Some(ADMIN),
            json!({})
        )
        .await
        .body["routes"][0]["circuit"],
        "open"
    );
    // After the cooldown one probe goes through. A success closes the circuit.
    up.status.store(200, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(t.chat(IP, key, Some("m")).await.status, 200);
    assert_eq!(up.hits(), 3);
    for _ in 0..3 {
        assert_eq!(t.chat(IP, key, Some("m")).await.status, 200);
    }
    assert_eq!(up.hits(), 6);
    // The failed requests were logged, unbilled, with their attempts.
    let rows = t.requests().await;
    assert_eq!(rows.len(), 7);
    assert_eq!(rows[2].get::<i64, _>("status"), 503);
    assert_eq!(rows[2].get::<i64, _>("attempts"), 0);
}

#[tokio::test]
async fn a_failed_probe_reopens_the_circuit() {
    let mut t = Test::new(|c| {
        c.routing.failure_threshold = 1;
        c.routing.cooldown_seconds = 1;
    })
    .await;
    let up = Up::new(500);
    t.upstream("p", &up).await;
    t.model("m", Tier::Paid, Strategy::Ordered, &[("p", "x")])
        .await;
    let k = t.account(true).await;
    let key = k["key"].as_str();
    assert_eq!(t.chat(IP, key, Some("m")).await.status, 500);
    assert_eq!(t.chat(IP, key, Some("m")).await.status, 503);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    // The probe fails: back to open, with a longer wait.
    assert_eq!(t.chat(IP, key, Some("m")).await.status, 500);
    let r = t.chat(IP, key, Some("m")).await;
    assert_eq!(r.status, 503);
    assert_eq!(up.hits(), 2);
    let wait: u64 = r.header("retry-after").parse().unwrap();
    assert!((2..=3).contains(&wait), "the cooldown doubled: {wait}");
}

#[tokio::test]
async fn an_auth_failure_cools_the_whole_provider_down() {
    let mut t = Test::new(|_| {}).await;
    let (revoked, backup) = (Up::new(401), Up::new(200));
    t.upstream("revoked", &revoked).await;
    t.upstream("backup", &backup).await;
    // Two models on the same provider, one with a backup elsewhere.
    t.model(
        "with-backup",
        Tier::Paid,
        Strategy::Ordered,
        &[("revoked", "a"), ("backup", "a")],
    )
    .await;
    t.model(
        "only-revoked",
        Tier::Paid,
        Strategy::Ordered,
        &[("revoked", "b")],
    )
    .await;
    let k = t.account(true).await;
    let key = k["key"].as_str();
    // The 401 never reaches the customer: the backup answers.
    assert_eq!(t.chat(IP, key, Some("with-backup")).await.status, 200);
    assert_eq!((revoked.hits(), backup.hits()), (1, 1));
    // Another model on the revoked provider is not even tried.
    let r = t.chat(IP, key, Some("only-revoked")).await;
    assert_eq!(
        (r.status, r.code()),
        (503, "capacity_exhausted"),
        "{}",
        r.body
    );
    assert!(r.header("retry-after").parse::<u64>().unwrap() >= 1);
    assert_eq!(revoked.hits(), 1);
    // And the backup keeps serving the first model without asking again.
    assert_eq!(t.chat(IP, key, Some("with-backup")).await.status, 200);
    assert_eq!(revoked.hits(), 1);
    // Without a backup, the provider's own 401 is hidden from the customer.
    let mut t = Test::new(|_| {}).await;
    let revoked = Up::new(401);
    t.upstream("revoked", &revoked).await;
    t.model("m", Tier::Paid, Strategy::Ordered, &[("revoked", "a")])
        .await;
    let k = t.account(true).await;
    let r = t.chat(IP, k["key"].as_str(), Some("m")).await;
    // It is the operator's capacity: a retry hint, not the upstream's text.
    assert_eq!((r.status, r.code()), (503, "capacity_exhausted"));
    assert!(r.headers.contains_key("retry-after"));
    assert!(!r.body.to_string().contains("upstream says no"));
}

#[tokio::test]
async fn an_exhausted_upstream_account_cools_the_provider_down() {
    let mut t = Test::new(|_| {}).await;
    t.model("m", Tier::Paid, Strategy::Ordered, &[("p", "a")])
        .await;
    t.model("n", Tier::Paid, Strategy::Ordered, &[("p", "b")])
        .await;
    // The mock answers with an insufficient_quota body.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", l.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    t.mocks.push(tokio::spawn(async move {
        let router = Router::new().fallback(any(move || {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}})),
                )
            }
        }));
        axum::serve(l, router).await.unwrap();
    }));
    common::provider(&mut *t.app.config_mut().await, "p", &url, "k");
    let k = t.account(true).await;
    let key = k["key"].as_str();
    let r = t.chat(IP, key, Some("m")).await;
    assert_eq!(
        (r.status, r.code()),
        (503, "capacity_exhausted"),
        "{}",
        r.body
    );
    assert!(!r.body.to_string().contains("quota"));
    // Not a rate limit of the route: the whole provider is parked.
    let r = t.chat(IP, key, Some("n")).await;
    assert_eq!((r.status, r.code()), (503, "capacity_exhausted"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_empty_upstream_window_in_a_success_parks_the_route() {
    let mut t = Test::new(|_| {}).await;
    let tight = Up::new(200).with_headers(&[
        ("x-ratelimit-remaining-requests", "0"),
        ("x-ratelimit-reset-requests", "45s"),
    ]);
    let other = Up::new(200);
    t.upstream("tight", &tight).await;
    t.upstream("other", &other).await;
    t.model(
        "m",
        Tier::Paid,
        Strategy::Ordered,
        &[("tight", "x"), ("other", "x")],
    )
    .await;
    let k = t.account(true).await;
    let key = k["key"].as_str();
    assert_eq!(t.chat(IP, key, Some("m")).await.status, 200);
    assert_eq!(t.chat(IP, key, Some("m")).await.status, 200);
    assert_eq!((tight.hits(), other.hits()), (1, 1));
}

#[tokio::test]
async fn provider_and_route_budgets_make_routes_unavailable() {
    let mut t = Test::new(|_| {}).await;
    let (a, b) = (Up::new(200), Up::new(200));
    t.upstream("pa", &a).await;
    t.upstream("pb", &b).await;
    t.model(
        "m",
        Tier::Paid,
        Strategy::Ordered,
        &[("pa", "x"), ("pb", "x")],
    )
    .await;
    {
        let mut c = t.app.config_mut().await;
        c.providers[0].limits = Budget {
            rpm: 2,
            ..Budget::default()
        };
        c.providers[1].limits = Budget {
            rpd: 1,
            ..Budget::default()
        };
    }
    let k = t.account(true).await;
    let key = k["key"].as_str();
    for _ in 0..3 {
        assert_eq!(t.chat(IP, key, Some("m")).await.status, 200);
    }
    // pa served two, pb one, then both budgets are spent.
    assert_eq!((a.hits(), b.hits()), (2, 1));
    let r = t.chat(IP, key, Some("m")).await;
    assert_eq!(
        (r.status, r.code()),
        (503, "capacity_exhausted"),
        "{}",
        r.body
    );
    assert!(r.header("retry-after").parse::<u64>().unwrap() >= 1);
    assert_eq!((a.hits(), b.hits()), (2, 1));
}

#[tokio::test]
async fn token_budgets_are_charged_when_a_request_settles() {
    let mut t = Test::new(|_| {}).await;
    let up = Up::new(200);
    up.tokens.store(1000, Ordering::SeqCst);
    t.upstream("p", &up).await;
    t.model("m", Tier::Paid, Strategy::Ordered, &[("p", "x")])
        .await;
    t.app.config_mut().await.providers[0].limits.tpd = 1000;
    let k = t.account(true).await;
    let key = k["key"].as_str();
    assert_eq!(t.chat(IP, key, Some("m")).await.status, 200);
    // The 1000 tokens of the first request used the budget up.
    let r = t.chat(IP, key, Some("m")).await;
    assert_eq!(
        (r.status, r.code()),
        (503, "capacity_exhausted"),
        "{}",
        r.body
    );
    assert!(r.header("retry-after").parse::<u64>().unwrap() >= 1);
    assert_eq!(up.hits(), 1);
}

#[tokio::test]
async fn a_restart_restores_daily_budgets_from_route_usage() {
    let mut t = Test::new(|_| {}).await;
    let up = Up::new(200);
    t.upstream("p", &up).await;
    t.model("m", Tier::Paid, Strategy::Ordered, &[("p", "x")])
        .await;
    t.app.config_mut().await.models[0].routes[0].limits.rpd = 5;
    sqlx::query(
        "INSERT INTO route_usage(hour,provider,model,tier,requests,tokens) VALUES(?,?,?,?,?,?)",
    )
    .bind(db::now() / 3600 * 3600)
    .bind("p")
    .bind("x")
    .bind("paid")
    .bind(5)
    .bind(0)
    .execute(&t.app.db)
    .await
    .unwrap();
    let k = t.account(true).await;
    let key = k["key"].as_str();
    // Before the warm start the fresh router knows nothing about it.
    let config = t.app.config().await;
    t.app.router.warm_start(&t.app.db, &config).await.unwrap();
    let r = t.chat(IP, key, Some("m")).await;
    assert_eq!(
        (r.status, r.code()),
        (503, "capacity_exhausted"),
        "{}",
        r.body
    );
    assert_eq!(up.hits(), 0);
}

// ---------------------------------------------------------------------------
// Callers: anonymous, keyed, operator
// ---------------------------------------------------------------------------

async fn free_setup(tweak: impl FnOnce(&mut Config)) -> (Test, Up) {
    let mut t = Test::new(tweak).await;
    let up = Up::new(200);
    t.upstream("free-p", &up).await;
    t.upstream("paid-p", &Up::new(200)).await;
    t.model(
        "gratis",
        Tier::Free,
        Strategy::Balanced,
        &[("free-p", "free-up")],
    )
    .await;
    t.model(
        "premium",
        Tier::Paid,
        Strategy::Balanced,
        &[("paid-p", "paid-up")],
    )
    .await;
    {
        let mut c = t.app.config_mut().await;
        c.models
            .iter_mut()
            .find(|m| m.id == "gratis")
            .unwrap()
            .default = true;
        c.models
            .iter_mut()
            .find(|m| m.id == "premium")
            .unwrap()
            .default = true;
    }
    (t, up)
}

#[tokio::test]
async fn anonymous_callers_use_free_models_without_a_key_and_are_not_billed() {
    let (t, up) = free_setup(|_| {}).await;
    *up.cost.lock().unwrap() = Some(0.002);
    // No credential, and placeholder tokens of any shape, are anonymous.
    for token in [None, Some("free"), Some("sk-whatever"), Some("not a key")] {
        let r = t.chat(IP, token, None).await;
        assert_eq!(r.status, 200, "{token:?}: {}", r.body);
        assert!(r.headers.contains_key("x-request-id"));
        assert!(!r.headers.contains_key("x-key-prefix"));
        assert!(!r.headers.contains_key("x-account-balance-usd"));
    }
    let r = t.chat(IP, None, Some("GRATIS")).await;
    assert_eq!(r.status, 200);
    assert_eq!(up.hits(), 5);
    assert_eq!(up.last()["model"], "free-up", "the route's model is sent");
    let rows = t.requests().await;
    assert_eq!(rows.len(), 5);
    for row in &rows {
        assert_eq!(row.get::<Option<String>, _>("account_id"), None);
        assert_eq!(row.get::<String, _>("tier"), "free");
        assert_eq!(row.get::<String, _>("public_model"), "gratis");
        assert_eq!(row.get::<String, _>("model"), "free-up");
        assert_eq!(row.get::<String, _>("provider"), "free-p");
        assert_eq!(row.get::<i64, _>("billed_micro"), 0);
        // The provider's reported cost is recorded, never charged.
        assert_eq!(row.get::<i64, _>("upstream_micro"), 2000);
    }
    // route_usage is written for anonymous requests too.
    let usage: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT requests,failures,tokens,upstream_micro FROM route_usage WHERE tier='free'",
    )
    .fetch_one(&t.app.db)
    .await
    .unwrap();
    assert_eq!(usage, (5, 0, 150, 10_000));
    let totals: (i64, i64) = sqlx::query_as("SELECT requests,billed_micro FROM totals WHERE id=1")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(totals, (5, 0));
}

#[tokio::test]
async fn anonymous_callers_need_a_key_for_paid_models() {
    let (t, up) = free_setup(|_| {}).await;
    for token in [None, Some("free")] {
        let r = t.chat(IP, token, Some("premium")).await;
        assert_eq!(
            (r.status, r.code()),
            (401, "api_key_required"),
            "{}",
            r.body
        );
        assert_eq!(
            r.body["error"]["message"],
            "Model premium requires an API key"
        );
        assert!(r.headers.contains_key("www-authenticate"));
        assert!(
            !r.headers["www-authenticate"]
                .to_str()
                .unwrap()
                .contains("invalid_token")
        );
    }
    // No model and no free default for the capability: asked to pick one.
    let r = send(
        &t.app,
        IP,
        "POST",
        "/v1/audio/speech",
        None,
        json!({"input": "hi"}),
    )
    .await;
    assert_eq!((r.status, r.code()), (400, "model_required"));
    assert_eq!(up.hits(), 0);
    assert!(t.requests().await.is_empty(), "refused before admission");
    // A key that looks like ours but is wrong is refused, not anonymous.
    let r = t.chat(IP, Some("sk-res-nope"), Some("gratis")).await;
    assert_eq!((r.status, r.code()), (401, "invalid_api_key"));
}

#[tokio::test]
async fn with_the_free_tier_off_anonymous_requests_need_a_key() {
    let (t, up) = free_setup(|c| c.free.enabled = false).await;
    for token in [None, Some("free")] {
        let r = t.chat(IP, token, Some("gratis")).await;
        assert_eq!((r.status, r.code()), (401, "missing_api_key"), "{}", r.body);
    }
    // Keys still use free models.
    let k = t.account(false).await;
    let r = t.chat(IP, k["key"].as_str(), Some("gratis")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(up.hits(), 1);
}

#[tokio::test]
async fn keys_use_free_models_without_credit_and_paid_models_need_it() {
    let (t, up) = free_setup(|_| {}).await;
    let poor = t.account(false).await;
    let rich = t.account(true).await;
    let r = t.chat(IP, poor["key"].as_str(), Some("gratis")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.header("x-key-prefix").len(), 15);
    // A paid model needs a balance.
    let r = t.chat(IP, poor["key"].as_str(), Some("premium")).await;
    assert_eq!((r.status, r.code()), (402, "insufficient_quota"));
    // Free requests are never billed, even with a provider-reported cost, and
    // leave the balance alone.
    *up.cost.lock().unwrap() = Some(0.05);
    let aid = rich["account_id"].as_str().unwrap();
    let r = t.chat(IP, rich["key"].as_str(), Some("gratis")).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-account-balance-usd"), "0.1");
    let balance = reseller::billing::account(&t.app, aid).await.unwrap();
    assert_eq!(balance["account"]["balance_usd"], "0.1");
    let rows = t.requests().await;
    assert_eq!(rows.len(), 2, "the refused paid request was never logged");
    let row = rows.last().unwrap();
    assert_eq!(row.get::<String, _>("tier"), "free");
    assert_eq!(row.get::<i64, _>("billed_micro"), 0);
    assert_eq!(row.get::<i64, _>("upstream_micro"), 50_000);
    let billed: i64 =
        sqlx::query_scalar("SELECT billed_micro FROM usage_buckets WHERE account_id=?")
            .bind(aid)
            .fetch_one(&t.app.db)
            .await
            .unwrap();
    assert_eq!(billed, 0);
    // The key's own limits still apply to free models.
    sqlx::query("UPDATE api_keys SET limits=? WHERE id=?")
        .bind(json!({"allowed_models": ["premium"]}).to_string())
        .bind(rich["id"].as_str())
        .execute(&t.app.db)
        .await
        .unwrap();
    let r = t.chat(IP, rich["key"].as_str(), Some("gratis")).await;
    assert_eq!((r.status, r.code()), (404, "model_not_found"));
    sqlx::query("UPDATE api_keys SET limits=? WHERE id=?")
        .bind(json!({"rpm": 2}).to_string())
        .bind(rich["id"].as_str())
        .execute(&t.app.db)
        .await
        .unwrap();
    // One request is already logged for the key.
    assert_eq!(
        t.chat(IP, rich["key"].as_str(), Some("gratis"))
            .await
            .status,
        200
    );
    let r = t.chat(IP, rich["key"].as_str(), Some("gratis")).await;
    assert_eq!((r.status, r.code()), (429, "rate_limit_exceeded"));
}

#[tokio::test]
async fn the_operator_is_unbilled_and_unlimited() {
    let (t, up) = free_setup(|c| {
        c.free.rpm = 1;
        c.free.rpd = 1;
    })
    .await;
    for model in ["gratis", "premium", "gratis", "premium"] {
        let r = t.chat(IP, Some(ADMIN), Some(model)).await;
        assert_eq!(r.status, 200, "{model}: {}", r.body);
        assert!(!r.headers.contains_key("x-ratelimit-limit-requests"));
    }
    assert_eq!(up.hits(), 2);
    for row in t.requests().await {
        assert_eq!(row.get::<Option<String>, _>("account_id"), None);
        assert_eq!(row.get::<i64, _>("billed_micro"), 0);
    }
}

#[tokio::test]
async fn the_models_list_depends_on_the_caller_and_carries_the_tier() {
    let (t, _) = free_setup(|_| {}).await;
    let list = |token: Option<&'static str>| {
        let app = t.app.clone();
        async move { send(&app, IP, "GET", "/v1/models", token, json!({})).await }
    };
    let anonymous = list(None).await;
    assert_eq!(anonymous.status, 200);
    assert_eq!(anonymous.body["data"].as_array().unwrap().len(), 1);
    assert_eq!(anonymous.body["data"][0]["id"], "gratis");
    assert_eq!(anonymous.body["data"][0]["tier"], "free");
    assert_eq!(list(Some("free")).await.body, anonymous.body);
    let k = t.account(false).await;
    let key: &'static str = Box::leak(k["key"].as_str().unwrap().to_owned().into_boxed_str());
    let keyed = list(Some(key)).await;
    let tiers: Vec<(&str, &str)> = keyed.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["id"].as_str().unwrap(), m["tier"].as_str().unwrap()))
        .collect();
    assert_eq!(tiers, [("gratis", "free"), ("premium", "paid")]);
    assert_eq!(list(Some(ADMIN)).await.body, keyed.body);
    // Retrieval follows the same visibility.
    let one = |path: &'static str, token| {
        let app = t.app.clone();
        async move { send(&app, IP, "GET", path, token, json!({})).await }
    };
    assert_eq!(one("/v1/models/premium", None).await.status, 404);
    assert_eq!(
        one("/v1/models/premium", Some(key)).await.body["tier"],
        "paid"
    );
    assert_eq!(one("/v1/models/GRATIS", None).await.body["id"], "gratis");
}

// ---------------------------------------------------------------------------
// Free-tier limits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn free_requests_per_minute_are_refused_with_retry_hints_and_rate_headers() {
    let (t, up) = free_setup(|c| {
        c.free.rpm = 2;
        c.free.rpd = 100;
    })
    .await;
    let first = t.chat(IP, None, None).await;
    assert_eq!(first.status, 200);
    assert_eq!(first.header("x-ratelimit-limit-requests"), "2");
    assert_eq!(first.header("x-ratelimit-remaining-requests"), "1");
    assert!(first.header("x-ratelimit-reset-requests").ends_with('s'));
    assert_eq!(
        first.header("ratelimit-policy"),
        "\"free\";q=2;w=60, \"free-day\";q=100;w=86400"
    );
    assert!(first.header("ratelimit").starts_with("\"free\";a=1;w="));
    assert_eq!(t.chat(IP, None, None).await.status, 200);
    let limited = t.chat(IP, None, None).await;
    assert_eq!(
        (limited.status, limited.code()),
        (429, "rate_limit_exceeded")
    );
    let wait: u64 = limited.header("retry-after").parse().unwrap();
    assert!((1..=30).contains(&wait), "{wait}");
    let ms: u64 = limited.header("retry-after-ms").parse().unwrap();
    assert!(ms > 0 && ms <= 30_000, "{ms}");
    assert!(
        !limited.headers.contains_key("x-should-retry"),
        "worth retrying"
    );
    assert_eq!(limited.header("x-ratelimit-remaining-requests"), "0");
    assert!(limited.header("ratelimit").starts_with("\"free\";a=0;w="));
    assert_eq!(up.hits(), 2, "the limit holds before the upstream");
    // Nothing was written for the refused request.
    assert_eq!(t.requests().await.len(), 2);
    // Another address is unaffected.
    assert_eq!(t.chat("198.51.100.11", None, None).await.status, 200);
}

#[tokio::test]
async fn free_requests_per_day_are_a_quota_that_sdks_must_not_retry() {
    let (t, up) = free_setup(|c| {
        c.free.rpm = 0;
        c.free.rpd = 2;
    })
    .await;
    for _ in 0..2 {
        assert_eq!(t.chat(IP, None, None).await.status, 200);
    }
    let r = t.chat(IP, None, None).await;
    assert_eq!(
        (r.status, r.code()),
        (429, "free_quota_exceeded"),
        "{}",
        r.body
    );
    assert_eq!(r.header("x-should-retry"), "false");
    assert!(r.header("retry-after").parse::<u64>().unwrap() >= 1);
    assert!(r.headers.contains_key("retry-after-ms"));
    let message = r.body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("API key") && message.contains("/workspace/"),
        "{message}"
    );
    assert_eq!(up.hits(), 2);
    // With only the daily limit set, it is the one advertised.
    let ok = t.chat("198.51.100.77", None, None).await;
    assert_eq!(ok.header("x-ratelimit-limit-requests"), "2");
    assert_eq!(ok.header("ratelimit-policy"), "\"free-day\";q=2;w=86400");
}

#[tokio::test]
async fn free_tokens_per_day_are_charged_after_each_request() {
    let (t, up) = free_setup(|c| {
        c.free.rpm = 0;
        c.free.rpd = 0;
        c.free.tpd = 100;
    })
    .await;
    up.tokens.store(100, Ordering::SeqCst);
    assert_eq!(t.chat(IP, None, None).await.status, 200);
    let r = t.chat(IP, None, None).await;
    assert_eq!(
        (r.status, r.code()),
        (429, "free_quota_exceeded"),
        "{}",
        r.body
    );
    assert_eq!(r.header("x-should-retry"), "false");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("tokens")
    );
    assert_eq!(up.hits(), 1);
    assert_eq!(t.chat("198.51.100.99", None, None).await.status, 200);
}

#[tokio::test]
async fn free_concurrency_is_limited_per_client_and_in_total() {
    let mut t = Test::new(|c| {
        c.free.max_concurrent = 1;
        c.free.max_concurrent_total = 2;
        c.free.rpm = 0;
        c.free.rpd = 0;
    })
    .await;
    let (up, gate) = Up::new(200).gated();
    t.upstream("p", &up).await;
    t.model("gratis", Tier::Free, Strategy::Ordered, &[("p", "x")])
        .await;
    t.app.config_mut().await.models[0].default = true;
    let held = |ip: &'static str| {
        let app = t.app.clone();
        tokio::spawn(async move {
            send(
                &app,
                ip,
                "POST",
                "/v1/chat/completions",
                None,
                json!({"messages":[]}),
            )
            .await
        })
    };
    let wait_for = |n: usize| {
        let up = up.clone();
        async move {
            for _ in 0..100 {
                if up.hits() >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("the upstream never saw {n} requests");
        }
    };
    let first = held("203.0.113.1");
    wait_for(1).await;
    // The same client may not start a second one at once.
    let r = t.chat("203.0.113.1", None, None).await;
    assert_eq!(
        (r.status, r.code()),
        (429, "concurrency_limit_exceeded"),
        "{}",
        r.body
    );
    assert!(r.header("retry-after").parse::<u64>().unwrap() >= 1);
    assert_eq!(r.header("retry-after-ms"), "1000");
    // Another client can, until the whole tier is full.
    let second = held("203.0.113.2");
    wait_for(2).await;
    let r = t.chat("203.0.113.3", None, None).await;
    assert_eq!(
        (r.status, r.code()),
        (503, "capacity_exhausted"),
        "{}",
        r.body
    );
    assert!(r.header("retry-after").parse::<u64>().unwrap() >= 1);
    assert!(r.headers.contains_key("retry-after-ms"));
    // Slots are freed as the requests finish.
    gate.add_permits(2);
    assert_eq!(first.await.unwrap().status, 200);
    assert_eq!(second.await.unwrap().status, 200);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(t.chat("203.0.113.1", None, None).await.status, 200);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (in_flight, _) = t.app.router.free.stats();
    assert_eq!(in_flight, 0);
}

#[tokio::test]
async fn ipv6_clients_share_a_budget_per_prefix() {
    let (t, _) = free_setup(|c| {
        c.free.rpm = 1;
        c.free.rpd = 0;
        c.free.tpd = 0;
    })
    .await;
    assert_eq!(t.chat("2001:db8:1:2::1", None, None).await.status, 200);
    // Same /64, another address: the same client.
    let r = t.chat("2001:db8:1:2:aaaa:bbbb:cccc:dddd", None, None).await;
    assert_eq!((r.status, r.code()), (429, "rate_limit_exceeded"));
    // Another /64 and other families are separate clients.
    assert_eq!(t.chat("2001:db8:1:3::1", None, None).await.status, 200);
    assert_eq!(t.chat("203.0.113.5", None, None).await.status, 200);
    assert_eq!(t.chat("203.0.113.6", None, None).await.status, 200);
    let r = t.chat("203.0.113.5", None, None).await;
    assert_eq!(r.status, 429);
    // A shorter prefix groups more clients.
    t.app.config_mut().await.free.ipv6_prefix = 48;
    assert_eq!(t.chat("2001:db8:5:1::1", None, None).await.status, 200);
    let r = t.chat("2001:db8:5:9::1", None, None).await;
    assert_eq!(r.status, 429, "the whole /48 is one client now");
}

#[tokio::test]
async fn unpaid_accounts_share_their_address_budget_and_paying_ones_get_their_own() {
    let (t, _) = free_setup(|c| {
        c.free.rpm = 2;
        c.free.rpd = 0;
        c.free.tpd = 0;
    })
    .await;
    let (a, b, c) = (
        t.account(false).await,
        t.account(false).await,
        t.account(false).await,
    );
    // Accounts without credit are charged to their address: more accounts
    // buy nothing.
    assert_eq!(
        t.chat(IP, a["key"].as_str(), Some("gratis")).await.status,
        200
    );
    assert_eq!(
        t.chat(IP, b["key"].as_str(), Some("gratis")).await.status,
        200
    );
    let r = t.chat(IP, c["key"].as_str(), Some("gratis")).await;
    assert_eq!(
        (r.status, r.code()),
        (429, "rate_limit_exceeded"),
        "{}",
        r.body
    );
    assert_eq!(
        t.chat(IP, None, None).await.status,
        429,
        "anonymous shares it too"
    );
    // Once an account has credit it has a budget of its own.
    common::fund(&t.app, a["account_id"].as_str().unwrap(), common::DIME).await;
    for _ in 0..2 {
        assert_eq!(
            t.chat(IP, a["key"].as_str(), Some("gratis")).await.status,
            200
        );
    }
    assert_eq!(
        t.chat(IP, a["key"].as_str(), Some("gratis")).await.status,
        429
    );
    // ... and so does a subscriber, or an account the operator trusts.
    sqlx::query("UPDATE accounts SET can_create_keys=1 WHERE id=?")
        .bind(c["account_id"].as_str())
        .execute(&t.app.db)
        .await
        .unwrap();
    assert_eq!(
        t.chat(IP, c["key"].as_str(), Some("gratis")).await.status,
        200
    );
    // The unpaid account is still stuck behind the address budget.
    assert_eq!(
        t.chat(IP, b["key"].as_str(), Some("gratis")).await.status,
        429
    );
}

#[tokio::test]
async fn free_requests_clamp_output_tokens_and_cap_the_body() {
    let (t, up) = free_setup(|c| {
        c.free.max_output_tokens = 256;
        c.free.max_body_bytes = 2000;
        c.free.rpm = 0;
    })
    .await;
    let ask = |extra: Value| {
        let app = t.app.clone();
        async move {
            let mut body = json!({"messages":[{"role":"user","content":"hi"}]});
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            send(&app, IP, "POST", "/v1/chat/completions", None, body).await
        }
    };
    assert_eq!(ask(json!({})).await.status, 200);
    assert_eq!(up.last()["max_tokens"], 256, "injected when absent");
    assert_eq!(ask(json!({"max_tokens": 100_000})).await.status, 200);
    assert_eq!(up.last()["max_tokens"], 256);
    assert_eq!(ask(json!({"max_tokens": 64})).await.status, 200);
    assert_eq!(up.last()["max_tokens"], 64, "lower values are kept");
    assert_eq!(
        ask(json!({"max_completion_tokens": 9999})).await.status,
        200
    );
    assert_eq!(up.last()["max_completion_tokens"], 256);
    // Bodies over the cap are refused before anything else.
    let r = ask(json!({"padding": "x".repeat(3000)})).await;
    assert_eq!((r.status, r.code()), (413, "request_too_large"));
    assert_eq!(up.hits(), 4);
    // The cap does not reach paid models.
    let k = t.account(true).await;
    let r = send(
        &t.app,
        IP,
        "POST",
        "/v1/chat/completions",
        k["key"].as_str(),
        json!({"model":"premium","messages":[],"max_tokens": 100_000}),
    )
    .await;
    assert_eq!(
        r.status, 402,
        "a hold for 100000 tokens needs more than the credit"
    );
}

// ---------------------------------------------------------------------------
// Settings and the administrator view
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_routing_view_shows_live_state_and_usage() {
    let (t, _) = free_setup(|_| {}).await;
    t.app.config_mut().await.models[0].routes[0].limits.rpd = 10;
    for _ in 0..3 {
        assert_eq!(t.chat(IP, None, None).await.status, 200);
    }
    t.requests().await;
    let denied = send(&t.app, IP, "GET", "/admin/api/routing", None, json!({})).await;
    assert_eq!(denied.status, 401);
    let r = send(
        &t.app,
        IP,
        "GET",
        "/admin/api/routing",
        Some(ADMIN),
        json!({}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let providers = r.body["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 2);
    let free_p = providers.iter().find(|p| p["id"] == "free-p").unwrap();
    assert_eq!(free_p["enabled"], true);
    assert_eq!(free_p["in_flight"], 0);
    assert_eq!(free_p["cooldown_until"], Value::Null);
    assert_eq!(free_p["usage_24h"]["requests"], 3);
    assert_eq!(free_p["usage_24h"]["free_requests"], 3);
    assert!(!r.body.to_string().contains("upstream-secret"));
    let routes = r.body["routes"].as_array().unwrap();
    let route = routes.iter().find(|x| x["model"] == "free-up").unwrap();
    assert_eq!(route["provider"], "free-p");
    assert_eq!(route["models"], json!(["gratis"]));
    assert_eq!(route["circuit"], "closed");
    assert_eq!(route["failures"], 0);
    assert_eq!(route["success_rate"], 1.0);
    assert!(route["latency_ms"].is_number());
    assert_eq!(route["limits"]["rpd"]["limit"], 10);
    assert_eq!(route["limits"]["rpd"]["remaining"], 7);
    assert_eq!(route["usage_24h"]["requests"], 3);
    assert_eq!(route["usage_24h"]["tokens"], 90);
    assert!(route["usage_24h"]["avg_latency_ms"].is_number());
    let idle = routes.iter().find(|x| x["model"] == "paid-up").unwrap();
    assert_eq!(idle["usage_24h"]["requests"], 0);
    assert_eq!(idle["latency_ms"], Value::Null);
    assert_eq!(r.body["free"]["in_flight"], 0);
    assert!(r.body["free"]["subjects"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn settings_validate_providers_models_and_defaults() {
    let (t, _) = free_setup(|_| {}).await;
    let patch = |body: Value| {
        let app = t.app.clone();
        async move { send(&app, IP, "PATCH", "/admin/api/settings", Some(ADMIN), body).await }
    };
    let provider = json!({"id": "p2", "base_url": "https://p2.example/v1"});
    let model = |extra: Value| {
        let mut m = json!({"id": "x", "capability": "chat",
            "routes": [{"provider": "p2", "model": "up"}]});
        m.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        m
    };
    let current: Value = t.app.config().await.redacted();
    let providers = || {
        let mut all = current["providers"].as_array().unwrap().clone();
        all.push(provider.clone());
        Value::Array(all)
    };
    let models = |extra: Vec<Value>| {
        let mut all = current["models"].as_array().unwrap().clone();
        all.extend(extra);
        Value::Array(all)
    };
    // A model must route to a configured provider.
    let r = patch(json!({"models": models(vec![model(json!({}))])})).await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown provider")
    );
    // Capability is required.
    let mut no_capability = model(json!({}));
    no_capability.as_object_mut().unwrap().remove("capability");
    let r = patch(json!({"providers": providers(), "models": models(vec![no_capability])})).await;
    assert_eq!(r.status, 400, "{}", r.body);
    // Unique ids, defaults, free pricing and weights.
    for bad in [
        model(json!({"id": "gratis"})),
        model(json!({"default": true, "tier": "free"})),
        model(json!({"tier": "free", "show_pricing": true})),
        model(json!({"routes": []})),
        model(json!({"routes": [{"provider": "p2", "model": "up", "weight": 0}]})),
        model(json!({"strategy": "random"})),
        model(json!({"tier": "gold"})),
    ] {
        let r = patch(json!({"providers": providers(), "models": models(vec![bad.clone()])})).await;
        assert_eq!(r.status, 400, "{bad}: {}", r.body);
    }
    let r = patch(json!({"providers": [provider.clone(), provider.clone()]})).await;
    assert_eq!(r.status, 400, "duplicate provider ids: {}", r.body);
    let r = patch(json!({"routing": {"max_attempts": 0}})).await;
    assert_eq!(r.status, 400);
    let r = patch(json!({"free": {"ipv6_prefix": 16}})).await;
    assert_eq!(r.status, 400);
    for removed in ["require_key", "accept_any_token", "model_policy", "llm"] {
        let r = patch(json!({removed: true})).await;
        assert_eq!(r.status, 400, "{removed} no longer exists");
    }
    // A valid change applies at once, and leaves the stored keys alone.
    let r = patch(json!({
        "providers": providers(),
        "models": models(vec![model(json!({"default": true, "capability": "speech"}))]),
        "free": {"rpm": 7},
    }))
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let c = t.app.config().await;
    assert_eq!(c.free.rpm, 7);
    assert_eq!(c.providers.len(), 3);
    assert_eq!(
        c.providers[0].api_key, "upstream-secret",
        "blank key kept by id"
    );
    assert_eq!(c.providers[2].api_key, "");
    drop(c);
    let shown = send(
        &t.app,
        IP,
        "GET",
        "/admin/api/settings",
        Some(ADMIN),
        json!({}),
    )
    .await;
    assert_eq!(shown.body["settings"]["providers"][0]["api_key"], "");
    assert!(!shown.body.to_string().contains("upstream-secret"));
}

#[tokio::test]
async fn a_fallback_route_serves_only_after_every_other_route_failed() {
    // max_attempts = 2 is spent on the free routes. The fallback still gets
    // its own attempt.
    let mut t = Test::new(|c| c.routing.max_attempts = 2).await;
    let (a, b, c, paid) = (Up::new(500), Up::new(502), Up::new(500), Up::new(200));
    t.upstream("pa", &a).await;
    t.upstream("pb", &b).await;
    t.upstream("pc", &c).await;
    t.upstream("cheap", &paid).await;
    t.model(
        "free",
        Tier::Free,
        Strategy::Balanced,
        &[
            ("pa", "x"),
            ("pb", "x"),
            ("pc", "x"),
            ("cheap", "tencent/hy4-flash"),
        ],
    )
    .await;
    t.app.config_mut().await.models[0].routes[3].fallback = true;
    let r = t.chat(IP, None, Some("free")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(a.hits() + b.hits() + c.hits(), 2);
    assert_eq!(paid.hits(), 1);
    let rows = t.requests().await;
    assert_eq!(rows[0].get::<i64, _>("attempts"), 3);
    assert_eq!(rows[0].get::<String, _>("provider"), "cheap");
    assert_eq!(rows[0].get::<i64, _>("billed_micro"), 0, "free stays free");
}

#[tokio::test]
async fn a_fallback_route_stays_idle_while_other_routes_serve() {
    let mut t = Test::new(|_| {}).await;
    let (a, b, paid) = (Up::new(200), Up::new(200), Up::new(200));
    t.upstream("pa", &a).await;
    t.upstream("pb", &b).await;
    t.upstream("cheap", &paid).await;
    t.model(
        "free",
        Tier::Free,
        Strategy::Balanced,
        &[("pa", "x"), ("cheap", "tencent/hy4-flash"), ("pb", "x")],
    )
    .await;
    t.app.config_mut().await.models[0].routes[1].fallback = true;
    for _ in 0..6 {
        assert_eq!(t.chat(IP, None, Some("free")).await.status, 200);
    }
    assert_eq!(paid.hits(), 0);
    assert_eq!(a.hits() + b.hits(), 6);
    assert!(
        a.hits() > 0 && b.hits() > 0,
        "the other routes are balanced"
    );
    // A model must keep at least one route that is not a fallback.
    let mut c = (*t.app.config().await).clone();
    for route in &mut c.models[0].routes {
        route.fallback = true;
    }
    assert!(c.validate().is_err());
}
