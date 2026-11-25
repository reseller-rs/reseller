//! OAuth 2.1 for the remote MCP endpoint, and the administrator token as an
//! operator credential on `/v1` and `/mcp`.

use axum::{Json, Router, extract::Request, response::IntoResponse, routing::any};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reseller::{App, config::Config, db, keys};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::net::SocketAddr;
use url::Url;

const ADMIN: &str = "admin-test-secret-123456789";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

struct Test {
    app: App,
    key: Value,
    base: String,
    http: reqwest::Client,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Drop for Test {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

impl Test {
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    async fn with(tweak: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("t.db")).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut config = Config {
            admin_token: ADMIN.into(),
            key_pepper: "pepper-test-secret-12345678".into(),
            markup_pct: "0".into(),
            public_url: format!("http://{addr}"),
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
        tweak(&mut config);
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
            base: format!("http://{addr}"),
            http: reqwest::Client::new(),
            tasks: vec![server],
            _dir: dir,
        }
    }

    async fn mock_llm(&mut self) {
        let router = Router::new().fallback(any(|_: Request| async {
            Json(json!({
                "model": "m",
                "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
            }))
            .into_response()
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        self.tasks.push(tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }));
        let mut config = self.app.config_mut().await;
        config.llm.base_url = format!("{url}/v1");
        config.llm.api_key = "upstream".into();
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn api_key(&self) -> &str {
        self.key["key"].as_str().unwrap()
    }

    fn dashboard(&self) -> &str {
        self.key["dashboard_token"].as_str().unwrap()
    }

    /// POST /oauth/register for a loopback client. Returns its client_id.
    async fn register(&self) -> String {
        let r = self
            .http
            .post(self.url("/oauth/register"))
            .json(&json!({
                "client_name": "  Test App  ",
                "redirect_uris": ["http://127.0.0.1:7777/callback", "cursor://anysphere/cb"],
                "token_endpoint_auth_method": "none",
                "client_uri": "https://app.example/",
                "logo_uri": "https://app.example/logo.png",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["client_name"], "Test App");
        body["client_id"].as_str().unwrap().to_owned()
    }

    fn authorize_query(&self, client_id: &str, extra: &str) -> String {
        format!(
            "response_type=code&client_id={client_id}&redirect_uri=http%3A%2F%2F127.0.0.1%3A7777%2Fcallback\
             &code_challenge={}&code_challenge_method=S256&state=xyz&resource={}%2Fmcp{extra}",
            challenge_of(VERIFIER),
            self.base.replace(':', "%3A").replace('/', "%2F"),
        )
    }

    async fn decide(&self, query: &str, decision: &str, credential: &str) -> (u16, Value) {
        let r = self
            .http
            .post(self.url("/oauth/api/authorize"))
            .json(&json!({"query": query, "decision": decision, "credential": credential}))
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    /// Runs consent with the API key and returns the authorization code.
    async fn code(&self, client_id: &str) -> String {
        let query = self.authorize_query(client_id, "");
        let (status, body) = self.decide(&query, "allow", self.api_key()).await;
        assert_eq!(status, 200, "{body}");
        let redirect = Url::parse(body["redirect"].as_str().unwrap()).unwrap();
        let pairs: std::collections::HashMap<_, _> = redirect.query_pairs().into_owned().collect();
        assert_eq!(pairs["state"], "xyz");
        assert_eq!(pairs["iss"], self.base);
        pairs["code"].clone()
    }

    async fn token(&self, form: &[(&str, &str)]) -> (u16, Value) {
        let r = self
            .http
            .post(self.url("/oauth/token"))
            .form(form)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap())
    }

    async fn exchange(&self, client_id: &str, code: &str, verifier: &str) -> (u16, Value) {
        self.token(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client_id),
            ("redirect_uri", "http://127.0.0.1:7777/callback"),
            ("code_verifier", verifier),
        ])
        .await
    }

    /// A fully authorized client: (client_id, token response).
    async fn connect(&self) -> (String, Value) {
        let client = self.register().await;
        let code = self.code(&client).await;
        let (status, tokens) = self.exchange(&client, &code, VERIFIER).await;
        assert_eq!(status, 200, "{tokens}");
        (client, tokens)
    }

    async fn mcp(&self, token: Option<&str>, method: &str, params: Value) -> reqwest::Response {
        let mut params = params;
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let mut request = self
            .http
            .post(self.url("/mcp"))
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", method)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
        if let Some(name) = params["name"].as_str() {
            request = request.header("mcp-name", name);
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send().await.unwrap()
    }

    async fn call(&self, token: &str, name: &str, arguments: Value) -> Value {
        let r = self
            .mcp(
                Some(token),
                "tools/call",
                json!({"name": name, "arguments": arguments}),
            )
            .await;
        assert_eq!(r.status(), 200);
        let body: Value = r.json().await.unwrap();
        body["result"].clone()
    }
}

fn header<'a>(r: &'a reqwest::Response, name: &str) -> &'a str {
    r.headers()
        .get(name)
        .map(|v| v.to_str().unwrap())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Discovery

#[tokio::test]
async fn mcp_without_credentials_is_401_with_resource_metadata() {
    let t = Test::new().await;
    let expected = format!(
        "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\", scope=\"mcp\"",
        t.base
    );
    let r = t.mcp(None, "tools/list", json!({})).await;
    assert_eq!(r.status(), 401);
    assert_eq!(header(&r, "www-authenticate"), expected);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "missing_api_key");

    // Invalid credentials add error="invalid_token".
    for token in ["sk-res-nope", "mcp_at_nope"] {
        let r = t.mcp(Some(token), "tools/list", json!({})).await;
        assert_eq!(r.status(), 401, "{token}");
        assert_eq!(
            header(&r, "www-authenticate"),
            format!("{expected}, error=\"invalid_token\""),
            "{token}"
        );
    }
    // No anonymous mode, not even for discovery.
    assert_eq!(
        t.mcp(None, "server/discover", json!({})).await.status(),
        401
    );
    // Static credentials keep working.
    for token in [t.api_key(), t.dashboard()] {
        assert_eq!(
            t.mcp(Some(token), "tools/list", json!({})).await.status(),
            200
        );
    }
}

#[tokio::test]
async fn disabled_oauth_serves_no_endpoints_and_no_resource_metadata() {
    let t = Test::with(|c| c.mcp.oauth = false).await;
    let r = t.mcp(None, "tools/list", json!({})).await;
    assert_eq!(r.status(), 401);
    assert!(!header(&r, "www-authenticate").contains("resource_metadata"));
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-authorization-server",
    ] {
        assert_eq!(t.http.get(t.url(path)).send().await.unwrap().status(), 404);
    }
    let r = t
        .http
        .post(t.url("/oauth/register"))
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(r.unwrap().status(), 404);
    // OAuth access tokens are not recognised either.
    let r = t.mcp(Some("mcp_at_x"), "tools/list", json!({})).await;
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn metadata_documents_describe_the_server() {
    let t = Test::new().await;
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let r = t.http.get(t.url(path)).send().await.unwrap();
        assert_eq!(r.status(), 200, "{path}");
        assert_eq!(header(&r, "access-control-allow-origin"), "*");
        let doc: Value = r.json().await.unwrap();
        assert_eq!(doc["resource"], format!("{}/mcp", t.base));
        assert_eq!(doc["authorization_servers"], json!([t.base]));
        assert_eq!(doc["scopes_supported"], json!(["mcp"]));
        assert_eq!(doc["bearer_methods_supported"], json!(["header"]));
        assert_eq!(doc["resource_name"], "Reseller");
    }
    for path in [
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-authorization-server/mcp",
        "/.well-known/openid-configuration",
    ] {
        let doc: Value = t
            .http
            .get(t.url(path))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(doc["issuer"], t.base, "{path}");
        assert_eq!(
            doc["authorization_endpoint"],
            format!("{}/oauth/authorize", t.base)
        );
        assert_eq!(doc["token_endpoint"], format!("{}/oauth/token", t.base));
        assert_eq!(
            doc["registration_endpoint"],
            format!("{}/oauth/register", t.base)
        );
        assert_eq!(
            doc["revocation_endpoint"],
            format!("{}/oauth/revoke", t.base)
        );
        assert_eq!(doc["response_types_supported"], json!(["code"]));
        assert_eq!(
            doc["grant_types_supported"],
            json!(["authorization_code", "refresh_token"])
        );
        assert_eq!(doc["code_challenge_methods_supported"], json!(["S256"]));
        assert_eq!(
            doc["token_endpoint_auth_methods_supported"],
            json!(["none"])
        );
        assert_eq!(doc["scopes_supported"], json!(["mcp"]));
        assert_eq!(doc["service_documentation"], format!("{}/docs/", t.base));
    }
    // CORS preflight on the public endpoints.
    for path in [
        "/oauth/token",
        "/oauth/register",
        "/oauth/revoke",
        "/.well-known/oauth-protected-resource",
    ] {
        let r = t
            .http
            .request(reqwest::Method::OPTIONS, t.url(path))
            .header("origin", "https://claude.ai")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "content-type")
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{path} {}", r.status());
        assert_eq!(header(&r, "access-control-allow-origin"), "*", "{path}");
        assert_eq!(header(&r, "access-control-allow-credentials"), "", "{path}");
    }
    // The consent page itself is the SPA.
    let r = t
        .http
        .get(t.url("/oauth/authorize?x=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(header(&r, "content-type").starts_with("text/html"));
}

// ---------------------------------------------------------------------------
// Registration

#[tokio::test]
async fn registration_validates_clients() {
    let t = Test::new().await;
    let post = |body: Value| {
        let t = &t;
        async move {
            let r = t
                .http
                .post(t.url("/oauth/register"))
                .json(&body)
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap())
        }
    };
    let (status, ok) = post(json!({
        "client_name": "App",
        "redirect_uris": ["https://claude.ai/cb", "http://localhost:1/cb", "http://[::1]:2/cb", "vscode://x.y/cb"],
    }))
    .await;
    assert_eq!(status, 201, "{ok}");
    assert!(ok["client_id"].as_str().unwrap().starts_with("mcpc_"));
    assert!(ok["client_id_issued_at"].as_i64().unwrap() > 0);
    assert_eq!(ok["token_endpoint_auth_method"], "none");

    for uris in [
        json!([]),
        json!(["javascript:alert(1)"]),
        json!(["data:text/html,hi"]),
        json!(["file:///etc/passwd"]),
        json!(["about:blank"]),
        json!(["http://evil.example/cb"]),
        json!(["https://ok.example/cb#frag"]),
        json!(["not a uri"]),
        json!(
            (0..11)
                .map(|i| format!("https://a.example/{i}"))
                .collect::<Vec<_>>()
        ),
        Value::Null,
    ] {
        let (status, body) = post(json!({"redirect_uris": uris})).await;
        assert_eq!(status, 400, "{uris}");
        assert_eq!(body["error"], "invalid_redirect_uri", "{uris}");
    }
    let base = json!({"redirect_uris": ["https://a.example/cb"]});
    for (field, value) in [
        ("token_endpoint_auth_method", json!("client_secret_basic")),
        ("client_name", json!("x".repeat(101))),
        ("logo_uri", json!("http://a.example/logo.png")),
        ("grant_types", json!(["password"])),
    ] {
        let mut body = base.clone();
        body[field] = value;
        let (status, err) = post(body).await;
        assert_eq!(
            (status, err["error"].as_str()),
            (400, Some("invalid_client_metadata")),
            "{field}"
        );
    }
}

#[tokio::test]
async fn registration_is_rate_limited_per_ip() {
    let t = Test::new().await;
    let mut last = 0;
    for _ in 0..40 {
        last = t
            .http
            .post(t.url("/oauth/register"))
            .json(&json!({"redirect_uris": ["https://a.example/cb"]}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        if last == 429 {
            break;
        }
    }
    assert_eq!(last, 429);
}

// ---------------------------------------------------------------------------
// Authorization and tokens

#[tokio::test]
async fn authorize_info_validates_the_request() {
    let t = Test::new().await;
    let client = t.register().await;
    let get = |query: String| {
        let t = &t;
        async move {
            let r = t
                .http
                .get(format!("{}/oauth/api/authorize?{query}", t.base))
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap())
        }
    };
    let (status, info) = get(t.authorize_query(&client, "")).await;
    assert_eq!(status, 200, "{info}");
    assert_eq!(info["client"]["client_name"], "Test App");
    assert_eq!(info["client"]["client_id"], client);
    assert_eq!(info["client"]["logo_uri"], "https://app.example/logo.png");
    assert_eq!(info["redirect_uri"], "http://127.0.0.1:7777/callback");
    assert_eq!(info["redirect_host"], "127.0.0.1:7777");
    assert_eq!(info["scope"], "mcp");
    assert_eq!(info["resource"], format!("{}/mcp", t.base));
    assert_eq!(info["brand"], "Reseller");
    // Explicit scope and the trailing-slash resource are fine.
    let (status, _) = get(t.authorize_query(&client, "&scope=mcp")).await;
    assert_eq!(status, 200);

    // Untrusted client or redirect: a plain error, never a redirect.
    let (status, e) = get(t.authorize_query("mcpc_unknown", "")).await;
    assert_eq!(
        (status, e["error"]["code"].as_str()),
        (400, Some("invalid_client"))
    );
    assert!(e.get("redirect").is_none());
    let evil = t
        .authorize_query(&client, "")
        .replace("127.0.0.1%3A7777", "127.0.0.1%3A7778");
    let (status, e) = get(evil).await;
    assert_eq!(
        (status, e["error"]["code"].as_str()),
        (400, Some("invalid_redirect_uri"))
    );
    assert!(e.get("redirect").is_none());

    // Other errors go back to the client.
    let cases = [
        ("&scope=admin", "invalid_scope"),
        ("&code_challenge_method=plain", "invalid_request"),
    ];
    for (extra, code) in cases {
        let (status, e) = get(t.authorize_query(&client, extra)).await;
        assert_eq!(status, 400, "{extra}");
        let redirect = Url::parse(e["redirect"].as_str().unwrap()).unwrap();
        let pairs: std::collections::HashMap<_, _> = redirect.query_pairs().into_owned().collect();
        assert_eq!(pairs["error"], code, "{extra}");
        assert_eq!(pairs["state"], "xyz");
        assert_eq!(pairs["iss"], t.base);
        assert_eq!(e["error"]["code"], code);
    }
    let no_pkce = format!(
        "response_type=code&client_id={client}&redirect_uri=http%3A%2F%2F127.0.0.1%3A7777%2Fcallback"
    );
    let (status, e) = get(no_pkce).await;
    assert_eq!(status, 400);
    assert!(
        e["redirect"]
            .as_str()
            .unwrap()
            .contains("error=invalid_request")
    );
    let wrong_resource = t.authorize_query(&client, "x");
    let (status, e) = get(wrong_resource).await;
    assert_eq!(status, 400);
    assert!(
        e["redirect"]
            .as_str()
            .unwrap()
            .contains("error=invalid_target")
    );
    let token_type = t
        .authorize_query(&client, "")
        .replace("response_type=code", "response_type=token");
    let (status, e) = get(token_type).await;
    assert_eq!(status, 400);
    assert!(
        e["redirect"]
            .as_str()
            .unwrap()
            .contains("unsupported_response_type")
    );
}

#[tokio::test]
async fn deny_redirects_with_access_denied() {
    let t = Test::new().await;
    let client = t.register().await;
    let query = t.authorize_query(&client, "");
    let (status, body) = t.decide(&query, "deny", "").await;
    assert_eq!(status, 200);
    let redirect = Url::parse(body["redirect"].as_str().unwrap()).unwrap();
    let pairs: std::collections::HashMap<_, _> = redirect.query_pairs().into_owned().collect();
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    assert_eq!(pairs["error"], "access_denied");
    assert_eq!(pairs["state"], "xyz");
    assert_eq!(pairs["iss"], t.base);
    assert!(!pairs.contains_key("code"));
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_codes")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn consent_checks_credentials_origin_and_counts_failures() {
    let t = Test::new().await;
    let client = t.register().await;
    let query = t.authorize_query(&client, "");
    let (status, body) = t.decide(&query, "allow", "").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (401, Some("missing_api_key"))
    );
    let (status, body) = t.decide(&query, "allow", "sk-res-wrong").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (401, Some("invalid_api_key"))
    );
    let (status, body) = t.decide(&query, "maybe", t.api_key()).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (400, Some("invalid_request"))
    );
    // The administrator token is no customer credential.
    let (status, body) = t.decide(&query, "allow", ADMIN).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (403, Some("admin_token_not_customer"))
    );
    // A dashboard token of an active key can authorize.
    let (status, _) = t.decide(&query, "allow", t.dashboard()).await;
    assert_eq!(status, 200);

    // Same-origin only.
    let r = t
        .http
        .post(t.url("/oauth/api/authorize"))
        .header("origin", "https://evil.example")
        .json(&json!({"query": query, "decision": "deny"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = t
        .http
        .post(t.url("/oauth/api/authorize"))
        .header("origin", &t.base)
        .json(&json!({"query": query, "decision": "deny"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Bad guesses spend the per-IP failure budget (20 by default).
    let mut last = 0;
    for _ in 0..25 {
        last = t.decide(&query, "allow", "sk-res-wrong").await.0;
    }
    assert_eq!(last, 429);
}

#[tokio::test]
async fn code_flow_with_pkce_issues_tokens_for_mcp_only() {
    let mut t = Test::new().await;
    t.mock_llm().await;
    let client = t.register().await;

    // Wrong verifier fails, and burns the code.
    let code = t.code(&client).await;
    let (status, e) = t.exchange(&client, &code, &"x".repeat(43)).await;
    assert_eq!((status, e["error"].as_str()), (400, Some("invalid_grant")));
    let (status, e) = t.exchange(&client, &code, VERIFIER).await;
    assert_eq!((status, e["error"].as_str()), (400, Some("invalid_grant")));

    // Wrong redirect_uri or client fails.
    let code = t.code(&client).await;
    let (status, _) = t
        .token(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("client_id", &client),
            ("redirect_uri", "cursor://anysphere/cb"),
            ("code_verifier", VERIFIER),
        ])
        .await;
    assert_eq!(status, 400);
    let (status, e) = t.exchange("mcpc_unknown", &code, VERIFIER).await;
    assert_eq!((status, e["error"].as_str()), (401, Some("invalid_client")));

    // The happy path. Codes are single use.
    let code = t.code(&client).await;
    let r = t
        .http
        .post(t.url("/oauth/token"))
        .header("origin", "https://claude.ai")
        .json(&json!({
            "grant_type": "authorization_code", "code": code, "client_id": client,
            "redirect_uri": "http://127.0.0.1:7777/callback", "code_verifier": VERIFIER,
            "resource": format!("{}/mcp", t.base),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(header(&r, "cache-control"), "no-store");
    assert_eq!(header(&r, "access-control-allow-origin"), "*");
    let tokens: Value = r.json().await.unwrap();
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in"], 3600);
    assert_eq!(tokens["scope"], "mcp");
    let access = tokens["access_token"].as_str().unwrap();
    assert!(access.starts_with("mcp_at_"));
    assert!(
        tokens["refresh_token"]
            .as_str()
            .unwrap()
            .starts_with("mcp_rt_")
    );
    let (status, _) = t.exchange(&client, &code, VERIFIER).await;
    assert_eq!(status, 400);

    // Only digests are stored.
    let stored: Vec<String> = sqlx::query_scalar("SELECT hash FROM oauth_tokens")
        .fetch_all(&t.app.db)
        .await
        .unwrap();
    assert!(
        stored
            .iter()
            .all(|h| h.len() == 64 && !access.contains(h.as_str()))
    );
    assert!(!stored.iter().any(|h| h == access));

    // The token works on /mcp, as an API key identity: every tool.
    let overview = t.call(access, "account_overview", json!({})).await;
    assert_eq!(overview["isError"], false, "{overview}");
    assert_eq!(
        overview["structuredContent"]["account"]["balance_usd"],
        "0.1"
    );
    let text = t
        .call(access, "generate_text", json!({"prompt": "hi"}))
        .await;
    assert_eq!(text["isError"], false, "{text}");
    let used: Option<i64> = sqlx::query_scalar("SELECT last_used_at FROM oauth_grants")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert!(used.is_some());
    let billed: i64 = sqlx::query_scalar("SELECT billed_micro FROM api_keys WHERE key_prefix=?")
        .bind(&t.api_key()[..15])
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert!(billed > 0, "the OAuth identity is billed like its key");

    // ... and nowhere else.
    for (method, path) in [
        ("GET", "/v1/models"),
        ("POST", "/v1/chat/completions"),
        ("GET", "/v1/account"),
        ("GET", "/v1/keys"),
    ] {
        let r = t
            .http
            .request(method.parse().unwrap(), t.url(path))
            .bearer_auth(access)
            .json(&json!({"model":"m","messages":[]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "{path}");
    }
}

#[tokio::test]
async fn refresh_tokens_rotate_and_reuse_revokes_the_grant() {
    let t = Test::new().await;
    let (client, first) = t.connect().await;
    let refresh1 = first["refresh_token"].as_str().unwrap().to_owned();
    let refresh = |token: String, client: String| {
        let t = &t;
        async move {
            t.token(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &token),
                ("client_id", &client),
            ])
            .await
        }
    };

    // Another client cannot use it.
    let other = t.register().await;
    let (status, e) = refresh(refresh1.clone(), other).await;
    assert_eq!((status, e["error"].as_str()), (400, Some("invalid_grant")));

    let (status, second) = refresh(refresh1.clone(), client.clone()).await;
    assert_eq!(status, 200, "{second}");
    let refresh2 = second["refresh_token"].as_str().unwrap().to_owned();
    assert_ne!(refresh1, refresh2);
    assert_ne!(first["access_token"], second["access_token"]);
    let new_access = second["access_token"].as_str().unwrap();
    assert_eq!(
        t.mcp(Some(new_access), "tools/list", json!({}))
            .await
            .status(),
        200
    );

    // Replaying the rotated token revokes the whole family.
    let (status, e) = refresh(refresh1, client.clone()).await;
    assert_eq!((status, e["error"].as_str()), (400, Some("invalid_grant")));
    let (status, _) = refresh(refresh2, client).await;
    assert_eq!(status, 400);
    assert_eq!(
        t.mcp(Some(new_access), "tools/list", json!({}))
            .await
            .status(),
        401
    );
    let grants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_grants")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(grants, 0);
}

#[tokio::test]
async fn expired_tokens_are_refused() {
    let t = Test::new().await;
    let (client, tokens) = t.connect().await;
    let access = tokens["access_token"].as_str().unwrap();
    sqlx::query("UPDATE oauth_tokens SET expires_at=1 WHERE kind='access'")
        .execute(&t.app.db)
        .await
        .unwrap();
    let r = t.mcp(Some(access), "tools/list", json!({})).await;
    assert_eq!(r.status(), 401);
    assert!(header(&r, "www-authenticate").contains("invalid_token"));
    // The refresh token still renews it, until it is idle for 90 days.
    let refresh = tokens["refresh_token"].as_str().unwrap();
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", client.as_str()),
    ];
    sqlx::query("UPDATE oauth_tokens SET expires_at=1 WHERE kind='refresh'")
        .execute(&t.app.db)
        .await
        .unwrap();
    assert_eq!(t.token(&form).await.0, 400);
}

#[tokio::test]
async fn a_revoked_or_blocked_key_kills_its_tokens() {
    let t = Test::new().await;
    let (client, tokens) = t.connect().await;
    let access = tokens["access_token"].as_str().unwrap();
    assert_eq!(
        t.mcp(Some(access), "tools/list", json!({})).await.status(),
        200
    );
    sqlx::query("UPDATE api_keys SET status='revoked'")
        .execute(&t.app.db)
        .await
        .unwrap();
    let r = t.mcp(Some(access), "tools/list", json!({})).await;
    assert_eq!(r.status(), 401);
    assert!(header(&r, "www-authenticate").contains("invalid_token"));
    let (status, _) = t
        .token(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
            ("client_id", &client),
        ])
        .await;
    assert_eq!(status, 400);

    // A key blocked before the code is exchanged cannot start a session.
    let t = Test::new().await;
    let client = t.register().await;
    let code = t.code(&client).await;
    sqlx::query("UPDATE api_keys SET status='blocked'")
        .execute(&t.app.db)
        .await
        .unwrap();
    assert_eq!(t.exchange(&client, &code, VERIFIER).await.0, 400);

    // A suspended account stops its tokens as well.
    let t = Test::new().await;
    let (_, tokens) = t.connect().await;
    sqlx::query("UPDATE accounts SET status='suspended'")
        .execute(&t.app.db)
        .await
        .unwrap();
    let r = t
        .mcp(tokens["access_token"].as_str(), "tools/list", json!({}))
        .await;
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn revocation_endpoint_revokes_the_grant() {
    let t = Test::new().await;
    let (client, tokens) = t.connect().await;
    let access = tokens["access_token"].as_str().unwrap();
    // Someone else's client_id does not revoke it.
    let other = t.register().await;
    let r = t
        .http
        .post(t.url("/oauth/revoke"))
        .form(&[
            ("token", tokens["refresh_token"].as_str().unwrap()),
            ("client_id", other.as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        t.mcp(Some(access), "tools/list", json!({})).await.status(),
        200
    );
    let r = t
        .http
        .post(t.url("/oauth/revoke"))
        .form(&[
            ("token", tokens["refresh_token"].as_str().unwrap()),
            ("client_id", client.as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        t.mcp(Some(access), "tools/list", json!({})).await.status(),
        401
    );
    // Unknown tokens are accepted silently (RFC 7009).
    let r = t
        .http
        .post(t.url("/oauth/revoke"))
        .form(&[("token", "mcp_at_nothing")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn connected_apps_are_listed_and_revocable_by_the_customer() {
    let t = Test::new().await;
    let (_, tokens) = t.connect().await;
    let access = tokens["access_token"].as_str().unwrap();
    t.mcp(Some(access), "tools/list", json!({})).await;

    let get = |token: &str| {
        let r = t
            .http
            .get(t.url("/v1/account/apps"))
            .bearer_auth(token.to_owned());
        async move { r.send().await.unwrap() }
    };
    let r = get(t.dashboard()).await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let app = &body["apps"][0];
    assert_eq!(app["client_name"], "Test App");
    assert_eq!(app["scope"], "mcp");
    assert_eq!(app["key_prefix"], t.api_key()[..15]);
    assert!(app["created_at"].as_i64().unwrap() > 0);
    assert!(app["last_used_at"].as_i64().is_some());
    assert_eq!(body["apps"].as_array().unwrap().len(), 1);

    // Another account sees nothing and cannot revoke it.
    let other = keys::create(
        &t.app,
        keys::Requester::Signup { altcha: None },
        "127.0.0.2",
        &json!({"name":"other"}),
    )
    .await
    .unwrap();
    let other_token = other["dashboard_token"].as_str().unwrap();
    let theirs: Value = get(other_token).await.json().await.unwrap();
    assert_eq!(theirs["apps"], json!([]));
    let id = app["id"].as_str().unwrap();
    let r = t
        .http
        .delete(t.url(&format!("/v1/account/apps/{id}")))
        .bearer_auth(other_token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    let r = t
        .http
        .delete(t.url(&format!("/v1/account/apps/{id}")))
        .bearer_auth(t.dashboard())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["revoked"], true);
    assert_eq!(
        t.mcp(Some(access), "tools/list", json!({})).await.status(),
        401
    );
    let body: Value = get(t.dashboard()).await.json().await.unwrap();
    assert_eq!(body["apps"], json!([]));
}

// ---------------------------------------------------------------------------
// The administrator token

#[tokio::test]
async fn admin_token_is_an_operator_credential_on_v1() {
    let mut t = Test::with(|c| c.require_key = true).await;
    t.mock_llm().await;
    let get = |token: Option<&str>, path: &str| {
        let mut r = t.http.get(t.url(path));
        if let Some(token) = token {
            r = r.bearer_auth(token);
        }
        async move { r.send().await.unwrap() }
    };
    // Without a credential require_key still refuses.
    assert_eq!(get(None, "/v1/models").await.status(), 401);
    assert_eq!(get(Some("not-the-token"), "/v1/models").await.status(), 401);
    let r = get(Some(ADMIN), "/v1/models").await;
    assert_eq!(r.status(), 200);
    let models: Value = r.json().await.unwrap();
    assert_eq!(models["data"][0]["id"], "m");

    // Proxying works, is logged as anonymous, and nothing is billed.
    let r = t
        .http
        .post(t.url("/v1/chat/completions"))
        .bearer_auth(ADMIN)
        .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let _ = r.bytes().await.unwrap();
    let row = sqlx::query("SELECT account_id,key_id,billed_micro,status FROM requests")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(row.get::<Option<String>, _>("account_id"), None);
    assert_eq!(row.get::<Option<String>, _>("key_id"), None);
    assert_eq!(row.get::<i64, _>("billed_micro"), 0);
    let spent: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(billed_micro),0) FROM api_keys")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(spent, 0);
}

#[tokio::test]
async fn admin_token_on_customer_routes_is_a_clear_uncounted_error() {
    let t = Test::new().await;
    // More attempts than the failure budget: none of them counts.
    for _ in 0..30 {
        for path in ["/v1/keys", "/v1/account", "/v1/account/apps"] {
            let r = t
                .http
                .get(t.url(path))
                .bearer_auth(ADMIN)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 403, "{path}");
            let body: Value = r.json().await.unwrap();
            assert_eq!(body["error"]["code"], "admin_token_not_customer");
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("/admin/api")
            );
        }
    }
    // Real customers are still served from the same address.
    let r = t
        .http
        .get(t.url("/v1/account"))
        .bearer_auth(t.dashboard())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // The admin API itself is unchanged.
    let r = t
        .http
        .get(t.url("/admin/api/overview"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn admin_token_may_generate_over_mcp_but_has_no_account_tools() {
    let mut t = Test::with(|c| c.require_key = true).await;
    t.mock_llm().await;
    assert_eq!(
        t.mcp(Some(ADMIN), "tools/list", json!({})).await.status(),
        200
    );
    let models = t.call(ADMIN, "list_models", json!({})).await;
    assert_eq!(models["isError"], false, "{models}");
    let text = t
        .call(ADMIN, "generate_text", json!({"prompt": "hi"}))
        .await;
    assert_eq!(text["isError"], false, "{text}");
    assert_eq!(text["_meta"]["reseller/billing"]["operator"], true);
    assert_eq!(
        t.call(ADMIN, "list_plans", json!({})).await["isError"],
        false
    );
    for tool in [
        "account_overview",
        "list_keys",
        "usage_summary",
        "redeem_code",
    ] {
        let args = if tool == "redeem_code" {
            json!({"code": "x"})
        } else {
            json!({})
        };
        let r = t.call(ADMIN, tool, args).await;
        assert_eq!(r["isError"], true, "{tool}");
        assert_eq!(
            r["structuredContent"]["error"]["code"], "admin_token_not_customer",
            "{tool}"
        );
        assert!(
            r["structuredContent"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("/admin")
        );
    }
    let row = sqlx::query("SELECT account_id,billed_micro FROM requests")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(row.get::<Option<String>, _>("account_id"), None);
    assert_eq!(row.get::<i64, _>("billed_micro"), 0);
}
