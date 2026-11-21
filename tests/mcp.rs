//! Model Context Protocol: the `/mcp` endpoint and the stdio bridge's logic,
//! against a real server with mock upstreams and exact accounting checks.

use axum::{
    Json, Router,
    body::Body,
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use reseller::{
    App, billing,
    config::{Config, Price},
    db, keys,
    mcp::bridge::{Bridge, Credentials, Options, Store},
    security::pow,
};
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

const ADMIN: &str = "admin-test-secret-123456789";
const PNG: [u8; 12] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];

struct Test {
    app: App,
    key: Value,
    addr: SocketAddr,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    dir: tempfile::TempDir,
}

impl Drop for Test {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (url, task)
}

fn price(input: &str, output: &str) -> Price {
    Price {
        input: input.into(),
        output: output.into(),
        ..Price::default()
    }
}

impl Test {
    /// A zero-markup server with priced models and one funded key.
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    async fn with(tweak: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("test.db")).await.unwrap();
        let mut config = Config {
            admin_token: ADMIN.into(),
            key_pepper: "pepper-test-secret-12345678".into(),
            markup_pct: "0".into(),
            signup: reseller::config::Signup {
                cooldown_seconds: 0,
                ip_limit_per_month: 0,
                global_hourly_limit: 0,
                pow_max_number: 0,
                ..Default::default()
            },
            ..Config::default()
        };
        config.llm.model = "m".into();
        config.tts.model = "tts-1".into();
        config.stt.model = "whisper-1".into();
        config.image.model = "img-1".into();
        config.prices.clear();
        config.prices.insert("m".into(), price("2", "4"));
        config.prices.insert(
            "tts-1".into(),
            Price {
                characters: "15".into(),
                ..price("0", "0")
            },
        );
        config.prices.insert(
            "whisper-1".into(),
            Price {
                audio_second: "0.002".into(),
                ..price("0", "0")
            },
        );
        config.prices.insert(
            "img-1".into(),
            Price {
                image: "0.04".into(),
                ..price("0", "0")
            },
        );
        tweak(&mut config);
        // Create the initial key without a proof of work, then restore it.
        let pow_max_number = std::mem::take(&mut config.signup.pow_max_number);
        let config = db::settings(&pool, config).await.unwrap();
        let app = App::new(pool, config).unwrap();
        let key = keys::create(
            &app,
            keys::Requester::Signup { altcha: None },
            "127.0.0.1",
            &json!({"name":"test"}),
        )
        .await
        .unwrap();
        app.config_mut().await.signup.pow_max_number = pow_max_number;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = reseller::web::router(app.clone()).await;
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self {
            app,
            key,
            addr,
            tasks: vec![server],
            dir,
        }
    }

    async fn mock(
        &mut self,
        select: fn(&mut Config) -> &mut reseller::config::Upstream,
        router: Router,
    ) {
        let (base, task) = serve(router).await;
        let mut config = self.app.config_mut().await;
        let upstream = select(&mut config);
        upstream.base_url = format!("{base}/v1");
        upstream.api_key = "upstream-secret".into();
        self.tasks.push(task);
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn api_key(&self) -> &str {
        self.key["key"].as_str().unwrap()
    }

    fn dashboard(&self) -> &str {
        self.key["dashboard_token"].as_str().unwrap()
    }

    fn account(&self) -> &str {
        self.key["account_id"].as_str().unwrap()
    }

    async fn raw(&self, token: Option<&str>, headers: &[(&str, &str)], body: Value) -> Rpc {
        let mut request = reqwest::Client::new()
            .post(format!("{}/mcp", self.url()))
            .json(&body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        for (k, v) in headers {
            request = request.header(*k, *v);
        }
        Rpc::from(request.send().await.unwrap()).await
    }

    async fn rpc(&self, token: Option<&str>, method: &str, params: Value) -> Rpc {
        self.raw(
            token,
            &[],
            json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}),
        )
        .await
    }

    /// The `result` of a `tools/call`.
    async fn tool(&self, token: Option<&str>, name: &str, arguments: Value) -> Value {
        let r = self
            .rpc(
                token,
                "tools/call",
                json!({"name":name,"arguments":arguments}),
            )
            .await;
        assert_eq!(r.status, 200, "{:?}", r.body);
        assert_eq!(r.body["id"], 7);
        r.body["result"].clone()
    }

    async fn requests(&self) -> Vec<sqlx::sqlite::SqliteRow> {
        sqlx::query("SELECT * FROM requests ORDER BY created_at,rowid")
            .fetch_all(&self.app.db)
            .await
            .unwrap()
    }

    async fn balance(&self) -> String {
        billing::account(&self.app, self.account()).await.unwrap()["account"]["balance_usd"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

struct Rpc {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: Value,
}

impl Rpc {
    async fn from(response: reqwest::Response) -> Self {
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Self {
            status,
            headers,
            body,
        }
    }
}

fn llm(c: &mut Config) -> &mut reseller::config::Upstream {
    &mut c.llm
}
fn tts(c: &mut Config) -> &mut reseller::config::Upstream {
    &mut c.tts
}
fn stt(c: &mut Config) -> &mut reseller::config::Upstream {
    &mut c.stt
}
fn image(c: &mut Config) -> &mut reseller::config::Upstream {
    &mut c.image
}

/// Upstream requests seen by a mock: (Authorization header, body).
type Seen = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// Records the `Authorization` header and body of each upstream request and
/// answers with `respond`.
fn recording(seen: Seen, respond: impl Fn() -> Response + Clone + Send + Sync + 'static) -> Router {
    Router::new().fallback(any(move |request: Request| {
        let seen = seen.clone();
        let respond = respond.clone();
        async move {
            let auth = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let body = axum::body::to_bytes(request.into_body(), 10_000_000)
                .await
                .unwrap();
            seen.lock().unwrap().push((auth, body.to_vec()));
            respond()
        }
    }))
}

fn chat_reply() -> Response {
    Json(json!({
        "model": "m",
        "choices": [{"message": {"role": "assistant", "content": "Hello there"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1000, "completion_tokens": 500, "total_tokens": 1500},
    }))
    .into_response()
}

fn text_of(result: &Value) -> String {
    result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn error_code(result: &Value) -> &str {
    assert_eq!(result["isError"], true, "{result}");
    result["structuredContent"]["error"]["code"]
        .as_str()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Protocol

#[tokio::test]
async fn initialize_negotiates_versions() {
    let t = Test::new().await;
    for version in ["2025-11-25", "2025-06-18", "2025-03-26"] {
        let r = t
            .rpc(
                None,
                "initialize",
                json!({"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"t","version":"1"}}),
            )
            .await;
        assert_eq!(r.status, 200);
        assert_eq!(r.body["jsonrpc"], "2.0");
        assert_eq!(r.body["result"]["protocolVersion"], version);
    }
    let r = t
        .rpc(None, "initialize", json!({"protocolVersion":"1999-01-01"}))
        .await;
    let result = &r.body["result"];
    assert_eq!(result["protocolVersion"], "2025-11-25");
    assert_eq!(result["serverInfo"]["name"], "reseller");
    assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(result["capabilities"]["tools"].is_object());
    let instructions = result["instructions"].as_str().unwrap();
    for needle in ["Bearer", "create_account", "generate_text", "$0.25"] {
        assert!(instructions.contains(needle), "{needle}: {instructions}");
    }
    let r = t.rpc(None, "initialize", json!({})).await;
    assert_eq!(r.body["error"]["code"], -32602);
    assert_eq!(
        t.rpc(None, "ping", json!({})).await.body["result"],
        json!({})
    );
}

#[tokio::test]
async fn protocol_version_header_is_validated() {
    let t = Test::new().await;
    let ping = json!({"jsonrpc":"2.0","id":1,"method":"ping"});
    for version in ["2025-11-25", "2025-06-18", "2025-03-26"] {
        let r = t
            .raw(None, &[("mcp-protocol-version", version)], ping.clone())
            .await;
        assert_eq!(r.status, 200, "{version}");
    }
    let r = t
        .raw(None, &[("mcp-protocol-version", "2024-11-05")], ping)
        .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn origin_is_checked_against_public_url_and_cors_origin() {
    let t = Test::with(|c| {
        c.public_url = "https://reseller.example.com".into();
        c.cors_origin = "https://app.example.org".into();
    })
    .await;
    let ping = json!({"jsonrpc":"2.0","id":1,"method":"ping"});
    for (origin, status) in [
        ("https://reseller.example.com", 200),
        ("https://app.example.org", 200),
        ("https://evil.example", 403),
        ("http://reseller.example.com", 403),
        ("https://reseller.example.com.evil.example", 403),
        ("null", 403),
        ("not an origin", 403),
    ] {
        let r = t.raw(None, &[("origin", origin)], ping.clone()).await;
        assert_eq!(r.status, status, "{origin}");
        if status == 403 {
            assert_eq!(r.body["error"]["code"], "origin_not_allowed");
        }
    }
    // Non-browser clients send no Origin.
    assert_eq!(t.raw(None, &[], ping).await.status, 200);
}

#[tokio::test]
async fn http_semantics() {
    let t = Test::new().await;
    let client = reqwest::Client::new();
    for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
        let r = client
            .request(method, format!("{}/mcp", t.url()))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 405);
        assert_eq!(r.headers()["allow"], "POST");
    }
    // Notifications and client responses are accepted without a body.
    for message in [
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":3,"result":{}}),
    ] {
        let r = t.raw(None, &[], message).await;
        assert_eq!(r.status, 202);
        assert_eq!(r.body, Value::Null);
    }
    let r = t
        .raw(None, &[], json!([{"jsonrpc":"2.0","id":1,"method":"ping"}]))
        .await;
    assert_eq!(
        (r.status, r.body["error"]["code"].as_i64()),
        (400, Some(-32600))
    );
    let r = client
        .post(format!("{}/mcp", t.url()))
        .body("{nope")
        .send()
        .await
        .unwrap();
    let r = Rpc::from(r).await;
    assert_eq!(
        (r.status, r.body["error"]["code"].as_i64()),
        (400, Some(-32700))
    );
    let r = t
        .raw(None, &[], json!({"jsonrpc":"1.0","id":1,"method":"ping"}))
        .await;
    assert_eq!(r.status, 400);
    let r = t
        .raw(
            None,
            &[],
            json!({"jsonrpc":"2.0","id":null,"method":"ping"}),
        )
        .await;
    assert_eq!(r.status, 400);
    let r = t.rpc(None, "resources/list", json!({})).await;
    assert_eq!(r.body["error"]["code"], -32601);
    let r = t.rpc(None, "tools/call", json!({"name":"nope"})).await;
    assert_eq!(r.body["error"]["code"], -32602);
    // String ids are echoed.
    let r = t
        .raw(
            None,
            &[],
            json!({"jsonrpc":"2.0","id":"abc","method":"ping"}),
        )
        .await;
    assert_eq!(r.body["id"], "abc");
    assert!(
        r.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
}

#[tokio::test]
async fn disabled_endpoint_is_not_found_and_management_limit_applies() {
    let t = Test::with(|c| c.mcp.enabled = false).await;
    let r = t.rpc(None, "ping", json!({})).await;
    assert_eq!(r.status, 404);
    let t = Test::with(|c| c.rate_limits.management_per_minute = 3).await;
    for _ in 0..3 {
        assert_eq!(t.rpc(None, "ping", json!({})).await.status, 200);
    }
    let r = t.rpc(None, "ping", json!({})).await;
    assert_eq!(r.status, 429);
    assert!(r.headers.contains_key("retry-after"));
}

#[tokio::test]
async fn invalid_credentials_are_rejected_and_counted() {
    let t = Test::with(|c| c.rate_limits.auth_failures_per_hour = 2).await;
    let bad = format!("sk-res-{}", "0".repeat(64));
    for _ in 0..2 {
        let r = t.rpc(Some(&bad), "tools/list", json!({})).await;
        assert_eq!(
            (r.status, r.body["error"]["code"].as_str()),
            (401, Some("invalid_api_key"))
        );
    }
    let r = t.rpc(Some(&bad), "tools/list", json!({})).await;
    assert_eq!(r.status, 429);
}

#[tokio::test]
async fn tools_list_is_complete_and_well_formed() {
    let t = Test::new().await;
    let mut seen = None;
    for token in [None, Some(t.dashboard()), Some(t.api_key())] {
        let r = t.rpc(token, "tools/list", json!({})).await;
        let tools = r.body["result"]["tools"].as_array().unwrap().clone();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in [
            "generate_text",
            "generate_speech",
            "transcribe_audio",
            "generate_image",
            "list_models",
            "account_overview",
            "usage_summary",
            "list_requests",
            "list_keys",
            "create_key",
            "rotate_key",
            "block_key",
            "list_plans",
            "redeem_code",
            "start_checkout",
            "signup_challenge",
            "create_account",
        ] {
            assert!(names.contains(&expected), "{expected} missing");
        }
        for tool in &tools {
            let name = tool["name"].as_str().unwrap();
            assert!(!tool["title"].as_str().unwrap().is_empty(), "{name}");
            assert!(tool["description"].as_str().unwrap().len() > 20, "{name}");
            assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
            assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{name}");
            assert!(tool["annotations"]["readOnlyHint"].is_boolean(), "{name}");
            assert!(tool["annotations"]["openWorldHint"].is_boolean(), "{name}");
        }
        let auth = |name: &str| {
            tools.iter().find(|t| t["name"] == name).unwrap()["_meta"]["reseller/auth"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(auth("generate_text"), "api_key");
        assert_eq!(auth("account_overview"), "api_key_or_dashboard_token");
        assert_eq!(auth("list_plans"), "none");
        assert_eq!(auth("signup_challenge"), "none");
        if let Some(previous) = &seen {
            assert_eq!(previous, &tools);
        }
        seen = Some(tools);
    }
    let tools = seen.unwrap();
    let schema =
        |name: &str| tools.iter().find(|t| t["name"] == name).unwrap()["inputSchema"].clone();
    assert_eq!(schema("generate_image")["properties"]["n"]["maximum"], 4);
    assert_eq!(schema("generate_image")["properties"]["n"]["minimum"], 1);
    assert_eq!(
        schema("generate_speech")["properties"]["format"]["enum"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    let destructive = |name: &str| {
        tools.iter().find(|t| t["name"] == name).unwrap()["annotations"]["destructiveHint"].clone()
    };
    assert_eq!(destructive("block_key"), true);
    assert_eq!(destructive("generate_text"), false);
}

// ---------------------------------------------------------------------------
// Authentication

#[tokio::test]
async fn tools_need_the_right_credential() {
    let mut t = Test::new().await;
    let seen = Arc::default();
    t.mock(llm, recording(Arc::clone(&seen), chat_reply)).await;

    let anonymous = t.tool(None, "generate_text", json!({"prompt":"hi"})).await;
    assert_eq!(error_code(&anonymous), "authentication_required");
    let text = text_of(&anonymous);
    assert!(
        text.contains("create_account") && text.contains("Bearer"),
        "{text}"
    );
    let anonymous = t.tool(None, "account_overview", json!({})).await;
    assert_eq!(error_code(&anonymous), "authentication_required");

    let dashboard = t
        .tool(Some(t.dashboard()), "generate_text", json!({"prompt":"hi"}))
        .await;
    assert_eq!(error_code(&dashboard), "dashboard_token_not_allowed");
    let dashboard = t.tool(Some(t.dashboard()), "list_models", json!({})).await;
    assert_eq!(error_code(&dashboard), "dashboard_token_not_allowed");
    assert!(t.requests().await.is_empty());
    assert!(seen.lock().unwrap().is_empty());

    // Account tools take either credential; public tools need none.
    for token in [t.dashboard(), t.api_key()] {
        let r = t.tool(Some(token), "account_overview", json!({})).await;
        assert_eq!(r["isError"], false);
        assert_eq!(r["structuredContent"]["account"]["balance_usd"], "0.25");
    }
    let plans = t.tool(None, "list_plans", json!({})).await;
    assert_eq!(plans["isError"], false);
    assert!(plans["structuredContent"]["plans"].is_array());
}

// ---------------------------------------------------------------------------
// Generation

/// A generation through MCP is metered exactly like the same HTTP request.
#[tokio::test]
async fn generate_text_is_billed_like_the_http_api() {
    let mut t = Test::new().await;
    let seen = Arc::default();
    t.mock(llm, recording(Arc::clone(&seen), chat_reply)).await;

    let result = t
        .tool(
            Some(t.api_key()),
            "generate_text",
            json!({"prompt":"hi","system":"be brief","max_tokens":50,"temperature":0.5}),
        )
        .await;
    assert_eq!(result["isError"], false, "{result}");
    let data = &result["structuredContent"];
    assert_eq!(data["text"], "Hello there");
    assert_eq!(data["finish_reason"], "stop");
    assert_eq!(result["content"][0]["text"], "Hello there");
    // (1000 * 2 + 500 * 4) / 1e6 USD.
    assert_eq!(data["billing"]["billed_usd"], "0.004");
    assert_eq!(data["billing"]["tokens"], 1500);
    assert_eq!(data["billing"]["balance_usd"], "0.246");
    assert!(text_of(&result).contains("Remaining balance: $0.246"));

    // The upstream saw our credential and the translated request, never the client's token.
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "Bearer upstream-secret");
        let body: Value = serde_json::from_slice(&seen[0].1).unwrap();
        assert_eq!(body["model"], "m");
        assert_eq!(body["max_tokens"], 50);
        assert_eq!(body["stream"], false);
        assert_eq!(
            body["messages"][0],
            json!({"role":"system","content":"be brief"})
        );
        assert_eq!(body["messages"][1], json!({"role":"user","content":"hi"}));
    }
    assert!(!seen.lock().unwrap().iter().any(|(auth, body)| {
        auth.contains(t.api_key()) || String::from_utf8_lossy(body).contains(t.api_key())
    }));

    // The same request over HTTP.
    let http = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", t.url()))
        .bearer_auth(t.api_key())
        .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    let _ = http.bytes().await.unwrap();
    let rows = t.requests().await;
    assert_eq!(rows.len(), 2);
    for column in [
        "tokens",
        "upstream_micro",
        "billed_micro",
        "status",
        "estimated",
        "finished",
    ] {
        assert_eq!(
            rows[0].get::<i64, _>(column),
            rows[1].get::<i64, _>(column),
            "{column}"
        );
    }
    for column in ["kind", "model", "path", "key_id", "account_id"] {
        assert_eq!(
            rows[0].get::<String, _>(column),
            rows[1].get::<String, _>(column),
            "{column}"
        );
    }
    assert_eq!(
        data["billing"]["request_id"],
        rows[0].get::<String, _>("id")
    );
    assert_eq!(t.balance().await, "0.242");
}

#[tokio::test]
async fn generate_text_validates_arguments_and_reports_failures() {
    let mut t = Test::new().await;
    let seen = Arc::default();
    t.mock(
        llm,
        recording(Arc::clone(&seen), || {
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error":{"message":"slow down"}})),
            )
                .into_response()
        }),
    )
    .await;
    let key = Some(t.api_key());
    for arguments in [
        json!({}),
        json!({"prompt":"a","messages":[{"role":"user","content":"b"}]}),
        json!({"prompt":""}),
        json!({"prompt":"x","max_tokens":0}),
        json!({"prompt":"x","temperature":3}),
        json!({"prompt":"x","extra":1}),
        json!({"messages":[]}),
        json!({"messages":[{"role":"robot","content":"x"}]}),
        json!({"messages":[{"role":"user"}]}),
    ] {
        let r = t.tool(key, "generate_text", arguments.clone()).await;
        assert_eq!(error_code(&r), "invalid_arguments", "{arguments}");
    }
    assert!(seen.lock().unwrap().is_empty());
    // Provider failures surface as tool errors, not protocol errors.
    let r = t.tool(key, "generate_text", json!({"prompt":"x"})).await;
    assert_eq!(error_code(&r), "upstream_error");
    assert!(text_of(&r).contains("429") && text_of(&r).contains("slow down"));
    // Conversation form.
    let r = t
        .tool(
            key,
            "generate_text",
            json!({"messages":[{"role":"user","content":"a"},{"role":"assistant","content":"b"}]}),
        )
        .await;
    assert_eq!(error_code(&r), "upstream_error");
}

#[tokio::test]
async fn insufficient_balance_is_a_tool_error_with_the_code() {
    let mut t = Test::new().await;
    let seen = Arc::default();
    t.mock(llm, recording(Arc::clone(&seen), chat_reply)).await;
    sqlx::query("UPDATE credits SET remaining_micro=0 WHERE account_id=?")
        .bind(t.account())
        .execute(&t.app.db)
        .await
        .unwrap();
    let r = t
        .tool(Some(t.api_key()), "generate_text", json!({"prompt":"x"}))
        .await;
    assert_eq!(error_code(&r), "insufficient_balance");
    assert!(text_of(&r).contains("insufficient_balance"));
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn generate_speech_returns_audio_and_is_billed_by_characters() {
    let mut t = Test::new().await;
    let seen = Arc::default();
    let audio = vec![0xFF, 0xFB, 0x90, 0x00, 7, 7];
    let reply = audio.clone();
    t.mock(
        tts,
        recording(Arc::clone(&seen), move || {
            let mut response = Response::new(Body::from(reply.clone()));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
            response
        }),
    )
    .await;
    let r = t
        .tool(
            Some(t.api_key()),
            "generate_speech",
            json!({"text":"héllo 世界!","voice":"alloy"}),
        )
        .await;
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["content"][0]["type"], "audio");
    assert_eq!(r["content"][0]["mimeType"], "audio/mpeg");
    assert_eq!(
        STANDARD
            .decode(r["content"][0]["data"].as_str().unwrap())
            .unwrap(),
        audio
    );
    // 9 characters * 15 / 1e6 USD.
    assert_eq!(r["structuredContent"]["billing"]["billed_usd"], "0.000135");
    assert_eq!(r["structuredContent"]["bytes"], audio.len());
    let body: Value = serde_json::from_slice(&seen.lock().unwrap()[0].1).unwrap();
    assert_eq!(body["input"], "héllo 世界!");
    assert_eq!(body["voice"], "alloy");
    assert_eq!(body["response_format"], "mp3");
    let rows = t.requests().await;
    assert_eq!(rows[0].get::<i64, _>("billed_micro"), 135);
    assert_eq!(rows[0].get::<String, _>("kind"), "tts");

    let r = t
        .tool(
            Some(t.api_key()),
            "generate_speech",
            json!({"text":"x","format":"wav"}),
        )
        .await;
    assert_eq!(r["content"][0]["mimeType"], "audio/wav");
    for arguments in [
        json!({"text":"x","format":"ogg"}),
        json!({"text":""}),
        json!({"text":"x".repeat(4097)}),
    ] {
        let r = t
            .tool(Some(t.api_key()), "generate_speech", arguments)
            .await;
        assert_eq!(error_code(&r), "invalid_arguments");
    }
}

#[tokio::test]
async fn media_limits_apply_to_outputs_and_inputs() {
    let mut t = Test::with(|c| c.mcp.max_media_bytes = 100).await;
    t.mock(
        tts,
        Router::new().fallback(any(|| async {
            let mut response = Response::new(Body::from(vec![1u8; 200]));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
            response
        })),
    )
    .await;
    t.mock(
        image,
        Router::new().fallback(any(|| async {
            Json(json!({"data":[{"b64_json": STANDARD.encode(vec![9u8; 80])},{"b64_json": STANDARD.encode(vec![9u8; 80])}]}))
        })),
    )
    .await;
    let key = Some(t.api_key());
    let r = t.tool(key, "generate_speech", json!({"text":"x"})).await;
    assert_eq!(error_code(&r), "response_too_large");
    // Two 80-byte images exceed the 100-byte total.
    let r = t
        .tool(key, "generate_image", json!({"prompt":"x","n":2}))
        .await;
    assert_eq!(error_code(&r), "response_too_large");
    let r = t
        .tool(
            key,
            "transcribe_audio",
            json!({"audio_base64": STANDARD.encode(vec![0u8; 101]), "filename":"a.mp3"}),
        )
        .await;
    assert_eq!(error_code(&r), "media_too_large");
    let r = t
        .tool(
            key,
            "transcribe_audio",
            json!({"audio_base64": "x".repeat(10_000), "filename":"a.mp3"}),
        )
        .await;
    assert_eq!(error_code(&r), "media_too_large");
}

#[tokio::test]
async fn transcribe_audio_uploads_multipart_and_bills_duration() {
    // The default policy: the model comes from the multipart body itself.
    let mut t = Test::new().await;
    let seen = Arc::default();
    t.mock(
        stt,
        recording(Arc::clone(&seen), || {
            Json(json!({"text":"hello world","language":"en","usage":{"type":"duration","seconds":30}}))
                .into_response()
        }),
    )
    .await;
    let audio: Vec<u8> = (0..=255u8).collect();
    let r = t
        .tool(
            Some(t.api_key()),
            "transcribe_audio",
            json!({
                "audio_base64": STANDARD.encode(&audio),
                "filename": "../../evil\"name.mp3",
                "language": "en",
            }),
        )
        .await;
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["content"][0]["text"], "hello world");
    // 30 s * 0.002 USD.
    assert_eq!(r["structuredContent"]["billing"]["billed_usd"], "0.06");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "Bearer upstream-secret");
        let body = &seen[0].1;
        let text = String::from_utf8_lossy(body);
        assert!(text.contains("name=\"model\"\r\n\r\nwhisper-1"), "{text}");
        assert!(text.contains("name=\"language\"\r\n\r\nen"));
        // Path components and quotes are stripped from the file name.
        assert!(text.contains("filename=\"evilname.mp3\""), "{text}");
        assert!(body.windows(audio.len()).any(|w| w == audio.as_slice()));
    }
    assert_eq!(t.requests().await[0].get::<String, _>("kind"), "stt");
    assert_eq!(t.requests().await[0].get::<String, _>("model"), "whisper-1");
    for arguments in [
        json!({"audio_base64":"!!!","filename":"a.mp3"}),
        json!({"audio_base64":"","filename":"a.mp3"}),
        json!({"audio_base64":"AAAA","filename":""}),
        json!({"audio_base64":"AAAA","filename":"a.mp3","language":"e n"}),
    ] {
        let r = t
            .tool(Some(t.api_key()), "transcribe_audio", arguments)
            .await;
        assert!(r["isError"] == true, "{r}");
    }
}

#[tokio::test]
async fn generate_image_returns_inline_images_and_never_fetches_urls() {
    let hits = Arc::new(Mutex::new(0u32));
    let counter = hits.clone();
    let (victim, victim_task) = serve(Router::new().fallback(any(move || {
        let counter = counter.clone();
        async move {
            *counter.lock().unwrap() += 1;
            "internal"
        }
    })))
    .await;
    let mut t = Test::new().await;
    t.tasks.push(victim_task);
    let seen = Arc::default();
    let link = format!("{victim}/secret.png");
    let b64 = STANDARD.encode(PNG);
    t.mock(
        image,
        recording(Arc::clone(&seen), move || {
            Json(json!({"data":[
                {"b64_json": b64, "revised_prompt":"a red cat"},
                {"url": link},
                {"url": "file:///etc/passwd"},
            ]}))
            .into_response()
        }),
    )
    .await;
    let r = t
        .tool(
            Some(t.api_key()),
            "generate_image",
            json!({"prompt":"cat","size":"512x512","n":3}),
        )
        .await;
    assert_eq!(r["isError"], false, "{r}");
    let content = r["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "image");
    assert_eq!(content[0]["mimeType"], "image/png");
    assert_eq!(
        STANDARD
            .decode(content[0]["data"].as_str().unwrap())
            .unwrap(),
        PNG
    );
    assert!(content[1]["text"].as_str().unwrap().contains("/secret.png"));
    // file: URLs are dropped; the summary line is last.
    assert_eq!(content.len(), 3);
    assert_eq!(
        r["structuredContent"]["images"][0]["revised_prompt"],
        "a red cat"
    );
    // 3 images at 0.04 USD.
    assert!(r["structuredContent"]["billing"]["billed_usd"].is_string());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        *hits.lock().unwrap(),
        0,
        "the server must not fetch provider URLs"
    );
    let body: Value = serde_json::from_slice(&seen.lock().unwrap()[0].1).unwrap();
    assert_eq!(
        (body["n"].clone(), body["size"].clone()),
        (json!(3), json!("512x512"))
    );
    assert_eq!(t.requests().await[0].get::<String, _>("kind"), "image");

    for arguments in [
        json!({"prompt":"x","n":0}),
        json!({"prompt":"x","n":5}),
        json!({"prompt":"x","size":"huge"}),
        json!({"prompt":""}),
    ] {
        let r = t.tool(Some(t.api_key()), "generate_image", arguments).await;
        assert_eq!(error_code(&r), "invalid_arguments");
    }
}

#[tokio::test]
async fn list_models_goes_through_the_proxy() {
    let mut t = Test::new().await;
    t.mock(
        llm,
        Router::new().fallback(any(|| async {
            Json(json!({"object":"list","data":[{"id":"m"},{"id":"other"}]}))
        })),
    )
    .await;
    let r = t.tool(Some(t.api_key()), "list_models", json!({})).await;
    assert_eq!(r["structuredContent"]["models"], json!(["m", "other"]));
}

// ---------------------------------------------------------------------------
// Account tools

#[tokio::test]
async fn account_tools_reuse_the_http_handlers() {
    let mut t = Test::new().await;
    t.mock(llm, recording(Arc::default(), chat_reply)).await;
    let key = Some(t.api_key());
    let dash = Some(t.dashboard());
    t.tool(key, "generate_text", json!({"prompt":"x"})).await;

    let usage = t.tool(dash, "usage_summary", json!({"days":7})).await;
    assert_eq!(usage["isError"], false, "{usage}");
    let http: Value = reqwest::Client::new()
        .get(format!("{}/v1/account/usage?days=7", t.url()))
        .bearer_auth(t.dashboard())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(usage["structuredContent"], http);
    let bad = t.tool(dash, "usage_summary", json!({"days":0})).await;
    assert_eq!(error_code(&bad), "invalid_arguments");
    let bad = t
        .tool(dash, "usage_summary", json!({"key_id":"../x"}))
        .await;
    assert_eq!(error_code(&bad), "invalid_arguments");
    let missing = t
        .tool(dash, "usage_summary", json!({"key_id":"nope"}))
        .await;
    assert_eq!(error_code(&missing), "key_not_found");

    let list = t
        .tool(
            dash,
            "list_requests",
            json!({"limit":5,"kind":"llm","status":"2xx","model":"m"}),
        )
        .await;
    assert_eq!(list["structuredContent"]["total"], 1);
    assert_eq!(
        list["structuredContent"]["requests"][0]["billed_usd"],
        "0.004"
    );
    let none = t.tool(dash, "list_requests", json!({"kind":"tts"})).await;
    assert_eq!(none["structuredContent"]["total"], 0);
    assert_eq!(
        error_code(&t.tool(dash, "list_requests", json!({"kind":"x"})).await),
        "invalid_arguments"
    );
    assert_eq!(
        error_code(&t.tool(dash, "list_requests", json!({"limit":101})).await),
        "invalid_arguments"
    );

    let keys = t.tool(dash, "list_keys", json!({})).await;
    let listed = keys["structuredContent"]["keys"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    let text = text_of(&keys);
    assert!(!text.contains(t.api_key()) && !text.contains(t.dashboard()));

    let plans = t.tool(None, "list_plans", json!({})).await;
    assert!(plans["structuredContent"]["payment_methods"].is_array());
    let invalid = t
        .tool(dash, "redeem_code", json!({"code":"nonsense"}))
        .await;
    assert_eq!(error_code(&invalid), "invalid_code");
    let checkout = t
        .tool(dash, "start_checkout", json!({"plan":"basic"}))
        .await;
    assert_eq!(error_code(&checkout), "payments_unavailable");
    let checkout = t
        .tool(
            dash,
            "start_checkout",
            json!({"plan":"basic","method":"paypal"}),
        )
        .await;
    assert_eq!(error_code(&checkout), "invalid_arguments");
}

#[tokio::test]
async fn redeem_failures_are_rate_limited_like_http() {
    let t = Test::with(|c| c.rate_limits.redeem_failures_per_hour = 2).await;
    for _ in 0..2 {
        let r = t
            .tool(Some(t.dashboard()), "redeem_code", json!({"code":"bad"}))
            .await;
        assert_eq!(error_code(&r), "invalid_code");
    }
    let r = t
        .tool(Some(t.dashboard()), "redeem_code", json!({"code":"bad"}))
        .await;
    assert_eq!(error_code(&r), "redeem_rate_limited");
    assert!(
        r["structuredContent"]["error"]["retry_after_seconds"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[tokio::test]
async fn key_tools_follow_the_http_guards() {
    let t = Test::new().await;
    let dash = Some(t.dashboard());
    let child = t.tool(dash, "create_key", json!({"name":"worker"})).await;
    assert_eq!(child["isError"], false, "{child}");
    let child = &child["structuredContent"];
    assert!(child["key"].as_str().unwrap().starts_with("sk-res-"));
    assert!(
        child["dashboard_token"]
            .as_str()
            .unwrap()
            .starts_with("dash_")
    );
    assert!(child["notice"].as_str().unwrap().contains("only once"));
    assert_eq!(child["account_id"], t.account());
    let child_id = child["id"].as_str().unwrap();

    let definitions = t.rpc(None, "tools/list", json!({})).await;
    for name in ["create_key", "rotate_key", "create_account"] {
        let tool = definitions.body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap();
        assert!(
            tool["description"].as_str().unwrap().contains("ONCE"),
            "{name}"
        );
    }

    // The fresh key cannot be rotated until the cooldown has passed.
    let rotated = t.tool(dash, "rotate_key", json!({"key_id":child_id})).await;
    assert_eq!(error_code(&rotated), "rotate_cooldown");
    assert!(
        rotated["structuredContent"]["error"]["retry_after_seconds"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(text_of(&rotated).contains("rotate_cooldown"));

    t.app.config_mut().await.keys.rotate_cooldown_seconds = 0;
    let rotated = t.tool(dash, "rotate_key", json!({"key_id":child_id})).await;
    assert_eq!(rotated["isError"], false, "{rotated}");
    assert_ne!(rotated["structuredContent"]["key"], child["key"]);
    let other = t
        .tool(dash, "rotate_key", json!({"key_id":"missing"}))
        .await;
    assert_eq!(error_code(&other), "key_not_found");

    let new_id = rotated["structuredContent"]["id"].as_str().unwrap();
    let blocked = t.tool(dash, "block_key", json!({"key_id":new_id})).await;
    assert_eq!(blocked["structuredContent"]["key"]["status"], "blocked");
    let status: String = sqlx::query_scalar("SELECT status FROM api_keys WHERE id=?")
        .bind(new_id)
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(status, "blocked");

    // Per-account key cap.
    t.app.config_mut().await.keys.max_per_account = 1;
    let capped = t.tool(dash, "create_key", json!({"name":"one more"})).await;
    assert_eq!(error_code(&capped), "account_key_limit");
    assert_eq!(
        error_code(&t.tool(dash, "create_key", json!({})).await),
        "invalid_arguments"
    );
}

// ---------------------------------------------------------------------------
// Signup

async fn solve_challenge(challenge: &Value) -> String {
    let challenge: pow::Challenge = serde_json::from_value(challenge.clone()).unwrap();
    pow::encode(&challenge, pow::solve(&challenge).unwrap())
}

#[tokio::test]
async fn account_creation_through_tools() {
    let mut t = Test::with(|c| c.signup.pow_max_number = 300).await;
    t.mock(llm, recording(Arc::default(), chat_reply)).await;
    let challenge = t.tool(None, "signup_challenge", json!({})).await;
    assert_eq!(challenge["isError"], false, "{challenge}");
    let challenge = challenge["structuredContent"].clone();
    assert_eq!(challenge["algorithm"], "SHA-256");
    assert_eq!(challenge["maxnumber"], 300);

    let missing = t.tool(None, "create_account", json!({})).await;
    assert_eq!(error_code(&missing), "pow_required");
    let altcha = solve_challenge(&challenge).await;
    let created = t
        .tool(
            None,
            "create_account",
            json!({"altcha":altcha,"name":"agent","contact":"a@b.c"}),
        )
        .await;
    assert_eq!(created["isError"], false, "{created}");
    let account = &created["structuredContent"];
    let key = account["key"].as_str().unwrap().to_owned();
    assert!(
        account["dashboard_token"]
            .as_str()
            .unwrap()
            .starts_with("dash_")
    );
    assert!(account["notice"].as_str().unwrap().contains("only once"));

    // Solutions are single use.
    let replay = t
        .tool(None, "create_account", json!({"altcha":altcha}))
        .await;
    assert_eq!(error_code(&replay), "pow_replayed");
    let invalid = t
        .tool(None, "create_account", json!({"altcha":"e30="}))
        .await;
    assert_eq!(error_code(&invalid), "pow_invalid");

    // The new credentials work over MCP and HTTP.
    let overview = t.tool(Some(&key), "account_overview", json!({})).await;
    assert_eq!(
        overview["structuredContent"]["account"]["balance_usd"],
        "0.25"
    );
    let text = t
        .tool(Some(&key), "generate_text", json!({"prompt":"hi"}))
        .await;
    assert_eq!(text["isError"], false, "{text}");
    let http = reqwest::Client::new()
        .get(format!("{}/v1/account", t.url()))
        .bearer_auth(account["dashboard_token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(http.status(), 200);

    // Signup guards apply: disabled signup is a tool error.
    t.app.config_mut().await.signup.enabled = false;
    let disabled = t.tool(None, "signup_challenge", json!({})).await;
    assert_eq!(error_code(&disabled), "signup_disabled");
}

// ---------------------------------------------------------------------------
// Bridge

struct Local {
    bridge: Bridge,
    credentials: PathBuf,
    output: PathBuf,
}

fn local(t: &Test, token: Option<&str>) -> Local {
    let credentials = t.dir.path().join("config").join("credentials.json");
    let output = t.dir.path().join("out");
    let bridge = Bridge::new(Options {
        url: t.url(),
        credentials: Some(credentials.clone()),
        output_dir: output.clone(),
        token: token.map(str::to_owned),
    })
    .unwrap();
    Local {
        bridge,
        credentials,
        output,
    }
}

impl Local {
    async fn send(&self, method: &str, params: Value) -> Value {
        self.bridge
            .handle(json!({"jsonrpc":"2.0","id":5,"method":method,"params":params}))
            .await
            .unwrap()
    }

    async fn tool(&self, name: &str, arguments: Value) -> Value {
        let reply = self
            .send("tools/call", json!({"name":name,"arguments":arguments}))
            .await;
        assert_eq!(reply["id"], 5);
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }
}

#[tokio::test]
async fn bridge_lists_local_tools_and_hides_remote_signup() {
    let t = Test::new().await;
    let b = local(&t, None);
    assert!(
        b.bridge
            .handle(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await
            .is_none()
    );
    let init = b
        .send("initialize", json!({"protocolVersion":"2025-06-18"}))
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("signup") && instructions.contains("Server instructions"));
    assert_eq!(b.send("ping", json!({})).await["result"], json!({}));

    let list = b.send("tools/list", json!({})).await;
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for local in ["signup", "login", "logout", "whoami", "reveal_credentials"] {
        assert!(names.contains(&local), "{local}");
    }
    assert!(names.contains(&"generate_text") && names.contains(&"list_plans"));
    assert!(!names.contains(&"signup_challenge") && !names.contains(&"create_account"));
    let unknown = b
        .send(
            "tools/call",
            json!({"name":"create_account","arguments":{}}),
        )
        .await;
    assert_eq!(unknown["error"]["code"], -32602);
    assert_eq!(b.send("nope", json!({})).await["error"]["code"], -32601);
    let batch = b.bridge.handle(json!([])).await.unwrap();
    assert_eq!(batch["error"]["code"], -32600);

    // Public tools work before signing in; everything else asks to sign in.
    assert_eq!(b.tool("list_plans", json!({})).await["isError"], false);
    let r = b.tool("generate_text", json!({"prompt":"x"})).await;
    assert_eq!(error_code(&r), "not_signed_in");
    let who = b.tool("whoami", json!({})).await;
    assert_eq!(who["structuredContent"]["signed_in"], false);
}

#[tokio::test]
async fn bridge_signup_generate_and_credential_lifecycle() {
    let mut t = Test::with(|c| {
        c.signup.pow_max_number = 300;
        c.keys.rotate_cooldown_seconds = 0;
    })
    .await;
    t.mock(llm, recording(Arc::default(), chat_reply)).await;
    t.mock(
        tts,
        Router::new().fallback(any(|| async {
            let mut response = Response::new(Body::from(vec![1u8, 2, 3]));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
            response
        })),
    )
    .await;
    let b = local(&t, None);

    let signup = b.tool("signup", json!({"name":"agent"})).await;
    assert_eq!(signup["isError"], false, "{signup}");
    let stored: Value = serde_json::from_slice(&std::fs::read(&b.credentials).unwrap()).unwrap();
    let entry = &stored[t.url()];
    let api_key = entry["api_key"].as_str().unwrap().to_owned();
    let dashboard = entry["dashboard_token"].as_str().unwrap().to_owned();
    assert!(api_key.starts_with("sk-res-") && dashboard.starts_with("dash_"));
    // Secrets are never returned by signup; the prefix and path are.
    let shown = signup.to_string();
    assert!(!shown.contains(&api_key) && !shown.contains(&dashboard));
    assert!(shown.contains(&api_key[..15]));
    assert!(shown.contains(b.credentials.to_str().unwrap()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&b.credentials), 0o600);
        assert_eq!(mode(b.credentials.parent().unwrap()), 0o700);
    }
    let again = b.tool("signup", json!({})).await;
    assert_eq!(error_code(&again), "already_signed_in");

    let who = b.tool("whoami", json!({})).await;
    let who = &who["structuredContent"];
    assert_eq!(who["signed_in"], true);
    assert_eq!(who["balance_usd"], "0.25");
    assert_eq!(who["key_prefix"], api_key[..15]);
    assert!(!who.to_string().contains(&api_key));

    // Generation uses the API key and is billed.
    let text = b.tool("generate_text", json!({"prompt":"hi"})).await;
    assert_eq!(text["structuredContent"]["text"], "Hello there");
    let rows = t.requests().await;
    assert_eq!(rows.len(), 1);
    let key_id: String = rows[0].get("key_id");
    assert_eq!(entry["key_id"], key_id);

    // Media is saved locally and announced.
    let speech = b.tool("generate_speech", json!({"text":"hello"})).await;
    let blocks = speech["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "audio");
    let saved = blocks
        .iter()
        .filter_map(|b| b["text"].as_str())
        .find_map(|t| t.strip_prefix("Saved audio to "))
        .expect("saved path is announced");
    let path = std::path::Path::new(saved);
    assert!(path.starts_with(&b.output) && path.extension().unwrap() == "mp3");
    assert_eq!(std::fs::read(path).unwrap(), vec![1u8, 2, 3]);
    let second = b.tool("generate_speech", json!({"text":"hello"})).await;
    let second = second["content"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|b| {
            b["text"]
                .as_str()
                .and_then(|t| t.strip_prefix("Saved audio to "))
        })
        .unwrap()
        .to_owned();
    assert_ne!(second, saved, "saved files get unique names");

    // Account tools work and use the least-privileged credential.
    let keys = b.tool("list_keys", json!({})).await;
    assert_eq!(keys["structuredContent"]["keys"][0]["id"], key_id);

    // Rotating the stored key keeps the credential file current and hides the secrets.
    let rotated = b.tool("rotate_key", json!({"key_id":key_id})).await;
    assert_eq!(rotated["isError"], false, "{rotated}");
    let shown = rotated.to_string();
    let stored: Value = serde_json::from_slice(&std::fs::read(&b.credentials).unwrap()).unwrap();
    let new_key = stored[t.url()]["api_key"].as_str().unwrap();
    assert_ne!(new_key, api_key);
    assert!(!shown.contains(new_key));
    assert_eq!(
        b.tool("generate_text", json!({"prompt":"again"})).await["isError"],
        false
    );

    // reveal_credentials is the only way to read the secrets.
    let revealed = b.tool("reveal_credentials", json!({})).await;
    assert_eq!(revealed["structuredContent"]["api_key"], new_key);

    // Logout deletes only this server's entry.
    let store = Store::new(b.credentials.clone());
    store
        .save(
            "https://other.example",
            Credentials {
                api_key: Some("x".into()),
                ..Credentials::default()
            },
        )
        .unwrap();
    let out = b.tool("logout", json!({})).await;
    assert_eq!(out["structuredContent"]["removed"], true);
    assert!(store.load(&t.url()).unwrap().is_none());
    assert!(store.load("https://other.example").unwrap().is_some());
    assert_eq!(
        error_code(&b.tool("generate_text", json!({"prompt":"x"})).await),
        "not_signed_in"
    );
    assert_eq!(
        error_code(&b.tool("reveal_credentials", json!({})).await),
        "not_signed_in"
    );
}

#[tokio::test]
async fn bridge_login_verifies_credentials() {
    let mut t = Test::new().await;
    t.mock(llm, recording(Arc::default(), chat_reply)).await;
    let b = local(&t, None);
    let bad = format!("sk-res-{}", "a".repeat(64));
    let r = b.tool("login", json!({"api_key":bad})).await;
    assert_eq!(error_code(&r), "invalid_api_key");
    assert!(!b.credentials.exists());
    for arguments in [
        json!({}),
        json!({"api_key":"nope"}),
        json!({"dashboard_token":t.api_key()}),
    ] {
        assert_eq!(
            error_code(&b.tool("login", arguments).await),
            "invalid_arguments"
        );
    }
    // A key and a token from different accounts are refused.
    let other = keys::create(
        &t.app,
        keys::Requester::Signup { altcha: None },
        "127.0.0.1",
        &json!({}),
    )
    .await
    .unwrap();
    let r = b
        .tool(
            "login",
            json!({"api_key":t.api_key(),"dashboard_token":other["dashboard_token"]}),
        )
        .await;
    assert_eq!(error_code(&r), "invalid_arguments");

    let ok = b
        .tool(
            "login",
            json!({"api_key":t.api_key(),"dashboard_token":t.dashboard()}),
        )
        .await;
    assert_eq!(ok["isError"], false, "{ok}");
    assert!(!ok.to_string().contains(t.api_key()));
    assert_eq!(ok["structuredContent"]["account_id"], t.account());
    let stored = Store::new(b.credentials.clone())
        .load(&t.url())
        .unwrap()
        .unwrap();
    assert_eq!(stored.api_key.as_deref(), Some(t.api_key()));
    assert_eq!(stored.key_id.as_deref(), t.key["id"].as_str());
    assert_eq!(
        b.tool("generate_text", json!({"prompt":"x"})).await["isError"],
        false
    );

    // Dashboard-only login can manage but not generate.
    b.tool("logout", json!({})).await;
    let ok = b
        .tool("login", json!({"dashboard_token":t.dashboard()}))
        .await;
    assert_eq!(ok["isError"], false, "{ok}");
    assert_eq!(
        b.tool("account_overview", json!({})).await["isError"],
        false
    );
    let r = b.tool("generate_text", json!({"prompt":"x"})).await;
    assert_eq!(error_code(&r), "dashboard_token_not_allowed");
}

#[tokio::test]
async fn bridge_token_option_is_used_without_storing_it() {
    let mut t = Test::new().await;
    t.mock(llm, recording(Arc::default(), chat_reply)).await;
    let b = local(&t, Some(t.api_key()));
    let who = b.tool("whoami", json!({})).await;
    assert_eq!(who["structuredContent"]["signed_in"], true);
    assert_eq!(who["structuredContent"]["source"], "token option");
    assert_eq!(
        b.tool("generate_text", json!({"prompt":"x"})).await["isError"],
        false
    );
    assert!(!b.credentials.exists(), "--token is never written to disk");
    for tool in ["signup", "login", "logout"] {
        let args = if tool == "login" {
            json!({"api_key":t.api_key()})
        } else {
            json!({})
        };
        assert_eq!(
            error_code(&b.tool(tool, args).await),
            "token_override_active"
        );
    }
    // A malformed token is rejected up front.
    let bad = Bridge::new(Options {
        url: t.url(),
        credentials: Some(b.credentials.clone()),
        output_dir: b.output.clone(),
        token: Some("hunter2".into()),
    });
    assert!(bad.is_err());
}

#[tokio::test]
async fn bridge_reports_unreachable_and_rejecting_servers() {
    let dir = tempfile::tempdir().unwrap();
    let options = |url: &str| Options {
        url: url.into(),
        credentials: Some(dir.path().join("c.json")),
        output_dir: dir.path().join("out"),
        token: None,
    };
    assert!(Bridge::new(options("http://example.com")).is_err());
    assert!(Bridge::new(options("https://example.com/path")).is_err());
    // Nothing listens here: tools/list still offers the local tools.
    let free = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", free.local_addr().unwrap());
    drop(free);
    let bridge = Bridge::new(options(&url)).unwrap();
    let list = bridge
        .handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .await
        .unwrap();
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 5);
    let reply = bridge
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"signup","arguments":{}}}))
        .await
        .unwrap();
    assert_eq!(error_code(&reply["result"]), "network_error");
}
