//! Spending and token accounting end to end: a real proxy server in front of
//! mock upstreams, with exact micro-USD assertions on the requests table,
//! the hourly buckets, the platform totals, and account balances.

use axum::{
    Json, Router,
    body::Body,
    extract::{Request, ws::WebSocketUpgrade},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use futures_util::{SinkExt, StreamExt};
use reseller::{
    App, billing,
    config::{Config, Price},
    db, keys,
};
use serde_json::{Value, json};
use sqlx::Row;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::Notify;

struct Test {
    app: App,
    key: Value,
    addr: SocketAddr,
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

/// Starts `router` on an ephemeral port and returns its base URL.
async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (url, task)
}

impl Test {
    /// A proxy with zero markup and the given prices (model `m` is the
    /// configured LLM model), plus a funded key.
    async fn new(prices: &[(&str, Price)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("test.db")).await.unwrap();
        let mut config = Config {
            admin_token: "admin-test-secret-123456789".into(),
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
        config.prices.clear();
        for (model, price) in prices {
            config.prices.insert((*model).into(), price.clone());
        }
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
            _dir: dir,
        }
    }

    /// Points the upstream chosen by `select` at `router`.
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

    fn token(&self) -> &str {
        self.key["key"].as_str().unwrap()
    }

    fn account(&self) -> &str {
        self.key["account_id"].as_str().unwrap()
    }

    async fn post(&self, path: &str, body: &Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("http://{}{path}", self.addr))
            .bearer_auth(self.token())
            .json(body)
            .send()
            .await
            .unwrap()
    }

    /// The settled request rows, oldest first.
    async fn rows(&self) -> Vec<sqlx::sqlite::SqliteRow> {
        sqlx::query("SELECT * FROM requests ORDER BY created_at,rowid")
            .fetch_all(&self.app.db)
            .await
            .unwrap()
    }

    /// Waits until `n` requests are settled (the proxy settles on a
    /// background task when the client disconnects).
    async fn settled(&self, n: usize) -> Vec<sqlx::sqlite::SqliteRow> {
        for _ in 0..100 {
            let rows = self.rows().await;
            if rows.len() == n && rows.iter().all(|r| r.get::<i64, _>("finished") == 1) {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("requests did not settle: {:?}", self.rows().await.len());
    }

    async fn balance(&self) -> String {
        billing::account(&self.app, self.account()).await.unwrap()["account"]["balance_usd"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

fn price(input: &str, output: &str) -> Price {
    Price {
        input: input.into(),
        output: output.into(),
        ..Price::default()
    }
}

fn llm_upstream(c: &mut Config) -> &mut reseller::config::Upstream {
    &mut c.llm
}
fn tts_upstream(c: &mut Config) -> &mut reseller::config::Upstream {
    &mut c.tts
}
fn stt_upstream(c: &mut Config) -> &mut reseller::config::Upstream {
    &mut c.stt
}

/// An upstream that answers every request with `handler`.
fn fixed<F, Fut>(handler: F) -> Router
where
    F: Fn(Value) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = Response> + Send + 'static,
{
    Router::new().fallback(any(move |request: Request| {
        let handler = handler.clone();
        async move {
            let bytes = axum::body::to_bytes(request.into_body(), 10_000_000)
                .await
                .unwrap();
            handler(serde_json::from_slice(&bytes).unwrap_or(Value::Null)).await
        }
    }))
}

fn json_response(body: Value) -> Response {
    Json(body).into_response()
}

// ---------------------------------------------------------------------------

/// The client drops the connection after the first SSE chunk. The upstream
/// then finishes and reports usage. The usage is still observed and billed.
#[tokio::test]
async fn client_disconnect_mid_stream_still_bills_final_usage() {
    let mut t = Test::new(&[("m", price("2", "4"))]).await;
    let release = Arc::new(Notify::new());
    let gate = release.clone();
    t.mock(
        llm_upstream,
        Router::new().fallback(any(move || {
            let gate = gate.clone();
            async move {
                let stream = async_stream::stream! {
                    yield Ok::<_, std::io::Error>(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_owned(),
                    );
                    gate.notified().await;
                    yield Ok("data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":500,\"total_tokens\":1500}}\n\n".to_owned());
                    yield Ok("data: [DONE]\n\n".to_owned());
                };
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        })),
    )
    .await;

    let mut response = t
        .post(
            "/v1/chat/completions",
            &json!({"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}),
        )
        .await;
    assert_eq!(response.status(), 200);
    let first = response.chunk().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("\"hi\""));
    drop(response);
    // Let the server notice the closed connection, then finish the stream.
    tokio::time::sleep(Duration::from_millis(300)).await;
    release.notify_one();

    let rows = t.settled(1).await;
    let row = &rows[0];
    assert_eq!(row.get::<i64, _>("status"), 499);
    assert_eq!(
        row.get::<Option<String>, _>("error").as_deref(),
        Some("client disconnected")
    );
    assert_eq!(row.get::<i64, _>("tokens"), 1500);
    // (1000*2 + 500*4) / 1e6 USD.
    assert_eq!(row.get::<i64, _>("upstream_micro"), 4000);
    assert_eq!(row.get::<i64, _>("billed_micro"), 4000);
    assert_eq!(row.get::<i64, _>("estimated"), 0);
    assert_eq!(t.balance().await, "0.096");
}

/// Without any upstream usage, tokens are estimated at four characters per
/// token and the request is flagged. Reported usage (even zero) never is.
#[tokio::test]
async fn missing_usage_is_estimated_and_flagged() {
    let mut t = Test::new(&[("m", price("2", "4"))]).await;
    t.mock(
        llm_upstream,
        fixed(|body| async move {
            if body["model"] == "reported" {
                return json_response(
                    json!({"choices":[{"message":{"content":"abcd"}}],"usage":{"cost":0}}),
                );
            }
            json_response(json!({"choices":[{"message":{"content":"abcdefghijklmnopqrst"}}]}))
        }),
    )
    .await;
    let content = "x".repeat(40);
    let r = t
        .post(
            "/v1/chat/completions",
            &json!({"model":"m","messages":[{"role":"user","content":content}]}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let _ = r.bytes().await.unwrap();
    // A reported zero cost is a report: no estimate.
    let r = t
        .post(
            "/v1/chat/completions",
            &json!({"model":"reported","messages":[{"role":"user","content":content}]}),
        )
        .await;
    let _ = r.bytes().await.unwrap();

    let rows = t.settled(2).await;
    let estimated = rows
        .iter()
        .find(|r| r.get::<String, _>("model") == "m")
        .unwrap();
    assert_eq!(estimated.get::<i64, _>("estimated"), 1);
    assert_eq!(estimated.get::<i64, _>("tokens"), 15);
    // input 10 * 2 + output 5 * 4 = 40 micro-USD.
    assert_eq!(estimated.get::<i64, _>("upstream_micro"), 40);
    let reported = rows
        .iter()
        .find(|r| r.get::<String, _>("model") == "reported")
        .unwrap();
    assert_eq!(reported.get::<i64, _>("estimated"), 0);
    assert_eq!(reported.get::<i64, _>("upstream_micro"), 0);

    // The flag is visible in the request history API.
    let history = reqwest::Client::new()
        .get(format!("http://{}/admin/api/requests", t.addr))
        .bearer_auth("admin-test-secret-123456789")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let flags: Vec<i64> = history["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["estimated"].as_i64().unwrap())
        .collect();
    assert_eq!(flags.iter().sum::<i64>(), 1);
}

/// A streamed response without usage is estimated from the deltas.
#[tokio::test]
async fn streamed_response_without_usage_is_estimated() {
    let mut t = Test::new(&[("m", price("2", "4"))]).await;
    t.mock(
        llm_upstream,
        Router::new().fallback(any(|| async {
            Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"12345678\"}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"content\":\"12345678\"}}]}\n\n\
                     data: [DONE]\n\n",
                ))
                .unwrap()
        })),
    )
    .await;
    let r = t
        .post(
            "/v1/chat/completions",
            &json!({"model":"m","stream":true,"messages":[{"role":"user","content":"12345678"}]}),
        )
        .await;
    let _ = r.bytes().await.unwrap();
    let rows = t.settled(1).await;
    // input 2 tokens, output 4 tokens.
    assert_eq!(rows[0].get::<i64, _>("tokens"), 6);
    assert_eq!(rows[0].get::<i64, _>("upstream_micro"), 2 * 2 + 4 * 4);
    assert_eq!(rows[0].get::<i64, _>("estimated"), 1);
}

/// Default mp3 speech has no measurable duration: it is billed per input
/// character, Unicode scalars rather than bytes, and only on success.
#[tokio::test]
async fn tts_mp3_is_billed_by_characters_and_only_on_success() {
    let speech = Price {
        characters: "15".into(),
        ..price("0", "0")
    };
    let mut t = Test::new(&[("m", price("0", "0")), ("tts-1", speech)]).await;
    t.app.config_mut().await.tts.model = "tts-1".into();
    t.mock(
        tts_upstream,
        fixed(|body| async move {
            if body["input"] == "fail" {
                return (StatusCode::BAD_REQUEST, "bad").into_response();
            }
            let mut response = Response::new(Body::from(vec![0xFF, 0xFB, 0x90, 0x00]));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
            response
        }),
    )
    .await;
    // 9 Unicode characters, 13 bytes.
    let r = t
        .post(
            "/v1/audio/speech",
            &json!({"model":"tts-1","voice":"alloy","input":"héllo 世界!"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let _ = r.bytes().await.unwrap();
    let r = t
        .post(
            "/v1/audio/speech",
            &json!({"model":"tts-1","voice":"alloy","input":"fail"}),
        )
        .await;
    assert_eq!(r.status(), 400);
    let _ = r.bytes().await.unwrap();
    let rows = t.settled(2).await;
    // 9 chars * 15 / 1e6 USD = 135 micro-USD.
    assert_eq!(rows[0].get::<i64, _>("upstream_micro"), 135);
    assert_eq!(rows[0].get::<i64, _>("characters"), 9);
    assert_eq!(rows[1].get::<i64, _>("upstream_micro"), 0);
    assert_eq!(rows[1].get::<i64, _>("characters"), 0);
}

/// Streamed speech reports token usage in its final SSE event.
#[tokio::test]
async fn tts_sse_usage_is_billed_in_tokens() {
    let mut t = Test::new(&[("m", price("0", "0")), ("tts-1", price("1", "10"))]).await;
    t.app.config_mut().await.tts.model = "tts-1".into();
    t.mock(
        tts_upstream,
        Router::new().fallback(any(|| async {
            Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from(
                    "data: {\"type\":\"speech.audio.delta\",\"audio\":\"AAAA\"}\n\n\
                     data: {\"type\":\"speech.audio.done\",\"usage\":{\"input_tokens\":100,\"output_tokens\":200,\"total_tokens\":300}}\n\n",
                ))
                .unwrap()
        })),
    )
    .await;
    let r = t
        .post(
            "/v1/audio/speech",
            &json!({"model":"tts-1","voice":"alloy","input":"hi","stream_format":"sse"}),
        )
        .await;
    let _ = r.bytes().await.unwrap();
    let rows = t.settled(1).await;
    assert_eq!(rows[0].get::<i64, _>("tokens"), 300);
    assert_eq!(rows[0].get::<i64, _>("upstream_micro"), 100 + 2000);
}

/// Both transcription usage shapes are billed: whisper duration seconds and
/// gpt-4o-transcribe tokens. Text formats without usage pay the flat fee.
#[tokio::test]
async fn transcription_usage_shapes_and_flat_request_fee() {
    let stt = Price {
        input: "1".into(),
        output: "10".into(),
        audio_second: "0.0001".into(),
        request: "0.001".into(),
        ..Price::default()
    };
    let mut t = Test::new(&[("m", price("0", "0")), ("whisper-1", stt)]).await;
    t.app.config_mut().await.stt.model = "whisper-1".into();
    t.mock(
        stt_upstream,
        fixed(|body| async move {
            match body["response_format"].as_str() {
                Some("duration") => {
                    json_response(json!({"text":"x","usage":{"type":"duration","seconds":30}}))
                }
                Some("tokens") => json_response(json!({"text":"x","usage":{
                    "type":"tokens","input_tokens":14,"output_tokens":45,"total_tokens":59,
                    "input_token_details":{"audio_tokens":10,"text_tokens":4}}})),
                Some("fail") => (StatusCode::BAD_REQUEST, "bad").into_response(),
                _ => "plain transcript".into_response(),
            }
        }),
    )
    .await;
    for format in ["duration", "tokens", "srt", "fail"] {
        let r = t
            .post(
                "/v1/audio/transcriptions",
                &json!({"model":"whisper-1","response_format":format}),
            )
            .await;
        let _ = r.bytes().await.unwrap();
    }
    let rows = t.settled(4).await;
    let micro = |i: usize| rows[i].get::<i64, _>("upstream_micro");
    // 30 s * 0.0001 + 1000 fee.
    assert_eq!(micro(0), 3000 + 1000);
    // 14 * 1 + 45 * 10 + 1000 fee.
    assert_eq!(micro(1), 14 + 450 + 1000);
    assert_eq!(rows[1].get::<i64, _>("tokens"), 59);
    // No usage at all: flat fee only.
    assert_eq!(micro(2), 1000);
    // Upstream failure: nothing.
    assert_eq!(micro(3), 0);
}

/// A provider-reported cost of zero replaces the token components but the
/// flat request fee is still added on success.
#[tokio::test]
async fn zero_provider_cost_plus_request_fee() {
    let priced = Price {
        request: "0.002".into(),
        ..price("5", "5")
    };
    let mut t = Test::new(&[("m", priced)]).await;
    t.mock(
        llm_upstream,
        fixed(|_| async {
            json_response(
                json!({"usage":{"prompt_tokens":1000,"total_tokens":1000,"cost":"1.2e-5"}}),
            )
        }),
    )
    .await;
    let r = t
        .post("/v1/chat/completions", &json!({"model":"m","messages":[]}))
        .await;
    let _ = r.bytes().await.unwrap();
    let rows = t.settled(1).await;
    assert_eq!(rows[0].get::<i64, _>("upstream_micro"), 12 + 2000);
}

/// Cached input tokens bill at the cached price. Every aggregate (request
/// row, hourly bucket, totals, stats, balance) carries identical numbers,
/// including the markup rounded once.
#[tokio::test]
async fn cached_tokens_and_aggregates_agree_to_the_micro_dollar() {
    let cached = Price {
        cached_input: Some("0.5".into()),
        ..price("2", "8")
    };
    let mut t = Test::new(&[("m", cached)]).await;
    t.app.config_mut().await.markup_pct = "30".into();
    t.mock(
        llm_upstream,
        fixed(|_| async {
            json_response(json!({"usage":{
                "prompt_tokens":1000,"completion_tokens":100,"total_tokens":1100,
                "prompt_tokens_details":{"cached_tokens":400},
                "completion_tokens_details":{"reasoning_tokens":60}}}))
        }),
    )
    .await;
    for _ in 0..3 {
        let r = t
            .post("/v1/chat/completions", &json!({"model":"m","messages":[]}))
            .await;
        let _ = r.bytes().await.unwrap();
    }
    let rows = t.settled(3).await;
    for row in &rows {
        // 600*2 + 400*0.5 + 100*8 (reasoning is inside the 100) = 2200;
        // +30% = 2860.
        assert_eq!(row.get::<i64, _>("upstream_micro"), 2200);
        assert_eq!(row.get::<i64, _>("billed_micro"), 2860);
        let breakdown = ["tokens", "input_tokens", "cached_tokens", "output_tokens"]
            .map(|c| row.get::<i64, _>(c));
        assert_eq!(breakdown, [1100, 1000, 400, 100]);
        assert_eq!(row.get::<i64, _>("tokens"), 1100);
    }
    let bucket = sqlx::query(
        "SELECT SUM(requests) r,SUM(tokens) t,SUM(upstream_micro) u,SUM(billed_micro) b FROM usage_buckets",
    )
    .fetch_one(&t.app.db)
    .await
    .unwrap();
    assert_eq!(
        (
            bucket.get::<i64, _>("r"),
            bucket.get::<i64, _>("t"),
            bucket.get::<i64, _>("u"),
            bucket.get::<i64, _>("b")
        ),
        (3, 3300, 6600, 8580)
    );
    let overview = reseller::stats::overview(&t.app).await.unwrap();
    assert_eq!(overview["requests"], 3);
    assert_eq!(overview["tokens"], 3300);
    assert_eq!(overview["upstream_cost_usd"], "0.0066");
    assert_eq!(overview["billed_cost_usd"], "0.00858");
    let usage = reseller::stats::usage(&t.app, Some(t.account()), None, 7)
        .await
        .unwrap();
    let series = usage["series"].as_array().unwrap();
    assert_eq!(series.len(), 1);
    assert_eq!(series[0]["billed_cost_usd"], "0.00858");
    assert_eq!(series[0]["tokens"], 3300);
    // Free credit of 0.25 minus the billed total.
    assert_eq!(t.balance().await, "0.09142");
}

/// Images returned through chat completions (OpenRouter style) are counted
/// and priced per image when the provider reports no cost.
#[tokio::test]
async fn chat_completion_images_are_priced() {
    let image = Price {
        image: "0.04".into(),
        ..price("0", "0")
    };
    let mut t = Test::new(&[("m", image)]).await;
    t.mock(
        llm_upstream,
        fixed(|body| async move {
            if body["stream"] == true {
                let events = [
                    json!({"choices":[{"delta":{"images":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA"}}]}}]}),
                    json!({"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":0,"total_tokens":3}}),
                ];
                let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
                return Response::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(body))
                    .unwrap();
            }
            json_response(json!({"choices":[{"message":{"content":"","images":[
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AA"}},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,BB"}}]}}],
                "usage":{"prompt_tokens":3,"completion_tokens":0,"total_tokens":3}}))
        }),
    )
    .await;
    let r = t
        .post("/v1/chat/completions", &json!({"model":"m","messages":[]}))
        .await;
    let _ = r.bytes().await.unwrap();
    let r = t
        .post(
            "/v1/chat/completions",
            &json!({"model":"m","messages":[],"stream":true}),
        )
        .await;
    let _ = r.bytes().await.unwrap();
    let rows = t.settled(2).await;
    assert_eq!(rows[0].get::<i64, _>("upstream_micro"), 80_000);
    assert_eq!(rows[1].get::<i64, _>("upstream_micro"), 40_000);
    assert_eq!(
        rows.iter()
            .map(|r| r.get::<i64, _>("images"))
            .collect::<Vec<_>>(),
        [2, 1]
    );
}

/// A fallback model is priced by the model that actually served the request.
#[tokio::test]
async fn fallback_attempt_is_priced_by_the_serving_model() {
    let mut t = Test::new(&[("m", price("1", "1")), ("fb", price("10", "10"))]).await;
    {
        let mut config = t.app.config_mut().await;
        config.llm.fallbacks = vec!["fb".into()];
    }
    t.mock(
        llm_upstream,
        fixed(|body| async move {
            if body["model"] == "m" {
                return (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response();
            }
            json_response(
                json!({"usage":{"prompt_tokens":1000,"completion_tokens":0,"total_tokens":1000}}),
            )
        }),
    )
    .await;
    let r = t
        .post("/v1/chat/completions", &json!({"model":"m","messages":[]}))
        .await;
    assert_eq!(r.headers()["x-model-fallback"], "fb");
    let _ = r.bytes().await.unwrap();
    let rows = t.settled(1).await;
    assert_eq!(rows[0].get::<String, _>("model"), "fb");
    assert_eq!(rows[0].get::<i64, _>("upstream_micro"), 10_000);
    let bucket: String = sqlx::query_scalar("SELECT model FROM usage_buckets")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(bucket, "fb");
}

/// An unfinished request row that outlived the upstream timeout (a crash
/// leftover) must not count against a key's request limit forever.
#[tokio::test]
async fn stale_unfinished_rows_do_not_consume_request_limits() {
    let mut t = Test::new(&[("m", price("1", "1"))]).await;
    t.mock(
        llm_upstream,
        fixed(|_| async { json_response(json!({"usage":{"total_tokens":1}})) }),
    )
    .await;
    sqlx::query("UPDATE api_keys SET limits=?")
        .bind(json!({"requests":{"max":1,"window_hours":24}}).to_string())
        .execute(&t.app.db)
        .await
        .unwrap();
    let key_id: String = sqlx::query_scalar("SELECT id FROM api_keys")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO requests(id,account_id,key_id,created_at,method,path,kind,model,ip,finished) \
         VALUES('stale',?,?,?,'POST','/v1/chat/completions','llm','m','1.1.1.1',0)",
    )
    .bind(t.account())
    .bind(key_id)
    .bind(db::now() - 7 * 86_400)
    .execute(&t.app.db)
    .await
    .unwrap();
    let r = t
        .post("/v1/chat/completions", &json!({"model":"m","messages":[]}))
        .await;
    assert_eq!(r.status(), 200);
}

/// A realtime client that leaves while a response is still being generated
/// is still billed for that response once `response.done` arrives.
#[tokio::test]
async fn realtime_client_close_waits_for_in_flight_response_done() {
    let mut t = Test::new(&[("m", price("2", "4"))]).await;
    t.mock(
        llm_upstream,
        Router::new().fallback(any(|ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|mut socket| async move {
                use axum::extract::ws::Message;
                // Wait for the client's request, then answer slowly.
                let _ = socket.recv().await;
                let _ = socket
                    .send(Message::Text(
                        json!({"type":"response.created","response":{}})
                            .to_string()
                            .into(),
                    ))
                    .await;
                tokio::time::sleep(Duration::from_millis(600)).await;
                let _ = socket
                    .send(Message::Text(
                        json!({"type":"response.done","response":{"usage":{
                                "input_tokens":1000,"output_tokens":500,"total_tokens":1500}}})
                        .to_string()
                        .into(),
                    ))
                    .await;
                // Keep the socket open until the proxy closes it.
                while socket.recv().await.is_some() {}
            })
        })),
    )
    .await;
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let mut request = format!("ws://{}/v1/realtime?model=m", t.addr)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", t.token())).unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket
        .send(Message::Text(
            json!({"type":"response.create"}).to_string().into(),
        ))
        .await
        .unwrap();
    // Wait for the response to start, then leave before it finishes.
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) if text.contains("response.created") => break,
            Some(Ok(_)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
    drop(socket);
    let rows = t.settled(1).await;
    assert_eq!(rows[0].get::<i64, _>("tokens"), 1500);
    assert_eq!(rows[0].get::<i64, _>("upstream_micro"), 4000);
}

/// A multipart body naming `model` (when given) plus a binary file part.
fn multipart_body(model: Option<&str>) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(model) = model {
        body.extend_from_slice(
            format!("--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(
        b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
          Content-Type: audio/wav\r\n\r\n\x00\x01AUDIO\xff\r\n--b--\r\n",
    );
    body
}

impl Test {
    /// Posts `body` as a multipart STT upload.
    async fn upload(&self, body: Vec<u8>) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("http://{}/v1/audio/transcriptions", self.addr))
            .bearer_auth(self.token())
            .header("content-type", "multipart/form-data; boundary=b")
            .body(body)
            .send()
            .await
            .unwrap()
    }

    async fn restrict_models(&self, models: &[&str]) {
        sqlx::query("UPDATE api_keys SET limits=?")
            .bind(json!({ "allowed_models": models }).to_string())
            .execute(&self.app.db)
            .await
            .unwrap();
    }
}

/// An STT test fixture whose upstream records every completed request body.
async fn stt_test(policy: reseller::config::Policy) -> (Test, Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
    let stt = Price {
        audio_second: "0.0001".into(),
        ..Price::default()
    };
    let cheap = Price {
        audio_second: "0.00001".into(),
        ..Price::default()
    };
    let mut t = Test::new(&[("m", price("0", "0")), ("whisper-1", stt), ("cheap", cheap)]).await;
    {
        let mut c = t.app.config_mut().await;
        c.model_policy = policy;
        c.stt.model = "whisper-1".into();
    }
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    t.mock(
        stt_upstream,
        Router::new().fallback(any(move |request: Request| {
            let log = log.clone();
            async move {
                let bytes = axum::body::to_bytes(request.into_body(), 10_000_000)
                    .await
                    .unwrap();
                log.lock().unwrap().push(bytes.to_vec());
                json_response(json!({"text":"x","usage":{"type":"duration","seconds":30}}))
            }
        })),
    )
    .await;
    (t, seen)
}

/// The client's multipart model is priced and metered under default and
/// passthrough policies. The body reaches the upstream unchanged.
#[tokio::test]
async fn multipart_model_is_priced_under_default_and_passthrough() {
    use reseller::config::Policy;
    for policy in [Policy::Default, Policy::Passthrough] {
        let (t, seen) = stt_test(policy).await;
        let body = multipart_body(Some("cheap"));
        let r = t.upload(body.clone()).await;
        assert_eq!(r.status(), 200);
        let _ = r.bytes().await.unwrap();
        let rows = t.settled(1).await;
        assert_eq!(rows[0].get::<String, _>("model"), "cheap");
        // 30 s * 0.00001 USD.
        assert_eq!(rows[0].get::<i64, _>("upstream_micro"), 300, "{policy:?}");
        assert_eq!(seen.lock().unwrap()[0], body);
    }
    // Missing model: injected under default (priced by it), empty and
    // priced by the default entry under passthrough.
    let (t, seen) = stt_test(Policy::Default).await;
    let _ = t.upload(multipart_body(None)).await.bytes().await.unwrap();
    let rows = t.settled(1).await;
    assert_eq!(rows[0].get::<String, _>("model"), "whisper-1");
    assert_eq!(rows[0].get::<i64, _>("upstream_micro"), 3000);
    assert!(String::from_utf8_lossy(&seen.lock().unwrap()[0]).contains("whisper-1"));
    let (t, _) = stt_test(Policy::Passthrough).await;
    let _ = t.upload(multipart_body(None)).await.bytes().await.unwrap();
    assert_eq!(t.settled(1).await[0].get::<String, _>("model"), "");
}

/// The key's allowlist applies to the multipart model: allowed passes,
/// disallowed or absent is a 404 and the upstream never gets a request.
#[tokio::test]
async fn multipart_model_allowlist_is_enforced() {
    use reseller::config::Policy;
    for policy in [Policy::Default, Policy::Passthrough] {
        let (t, seen) = stt_test(policy).await;
        t.restrict_models(&["cheap"]).await;

        let r = t.upload(multipart_body(Some("CHEAP"))).await;
        assert_eq!(r.status(), 200, "{policy:?}");
        let _ = r.bytes().await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1);

        for body in [multipart_body(Some("whisper-1")), multipart_body(None)] {
            let r = t.upload(body).await;
            assert_eq!(r.status(), 404, "{policy:?}");
            let v: Value = r.json().await.unwrap();
            assert_eq!(v["error"]["code"], "model_not_found");
        }
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "upstream saw a rejected upload"
        );
        let rows = t.settled(3).await;
        assert_eq!(rows[1].get::<i64, _>("status"), 404);
        assert_eq!(rows[1].get::<String, _>("model"), "whisper-1");
        assert!(
            rows[1]
                .get::<String, _>("error")
                .contains("model_not_found")
        );
        assert_eq!(rows[2].get::<i64, _>("status"), 404);
        // Only the allowed request was billed.
        assert_eq!(rows[1].get::<i64, _>("billed_micro"), 0);
        assert_eq!(rows[2].get::<i64, _>("billed_micro"), 0);
    }
}

/// A model value longer than the limit is a 400.
#[tokio::test]
async fn multipart_model_over_limit_is_rejected() {
    let (t, seen) = stt_test(reseller::config::Policy::Default).await;
    let r = t.upload(multipart_body(Some(&"x".repeat(257)))).await;
    assert_eq!(r.status(), 400);
    assert!(seen.lock().unwrap().is_empty());
    let r = t.upload(multipart_body(Some(&"x".repeat(256)))).await;
    assert_eq!(r.status(), 200);
}

/// Requests left unfinished by a previous process are closed on startup
/// without billing. Settled rows are untouched.
#[tokio::test]
async fn startup_sweep_closes_interrupted_requests() {
    let t = Test::new(&[("m", price("1", "1"))]).await;
    for (id, finished) in [("open", 0), ("done", 1)] {
        sqlx::query(
            "INSERT INTO requests(id,account_id,key_id,created_at,method,path,kind,model,ip,finished,status) \
             VALUES(?,?,NULL,?,'POST','/v1/chat/completions','llm','m','1.1.1.1',?,200)",
        )
        .bind(id)
        .bind(t.account())
        .bind(db::now())
        .bind(finished)
        .execute(&t.app.db)
        .await
        .unwrap();
    }
    let before = t.balance().await;
    assert_eq!(db::close_interrupted_requests(&t.app.db).await.unwrap(), 1);
    assert_eq!(db::close_interrupted_requests(&t.app.db).await.unwrap(), 0);
    let row = sqlx::query("SELECT * FROM requests WHERE id='open'")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>("finished"), 1);
    assert_eq!(row.get::<i64, _>("status"), 500);
    assert_eq!(
        row.get::<String, _>("error"),
        "interrupted by server restart"
    );
    assert_eq!(row.get::<i64, _>("billed_micro"), 0);
    let done = sqlx::query("SELECT status FROM requests WHERE id='done'")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(done.get::<i64, _>("status"), 200);
    assert_eq!(t.balance().await, before);
}

/// Unauthorized responses carry an RFC 6750 challenge.
#[tokio::test]
async fn unauthorized_responses_carry_www_authenticate() {
    let t = Test::new(&[("m", price("1", "1"))]).await;
    let client = reqwest::Client::new();
    let url = format!("http://{}/v1/chat/completions", t.addr);
    let r = client.post(&url).json(&json!({})).send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(r.headers()["www-authenticate"], "Bearer realm=\"reseller\"");
    let r = client
        .post(&url)
        .bearer_auth("sk-wrong")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(
        r.headers()["www-authenticate"],
        "Bearer realm=\"reseller\", error=\"invalid_token\""
    );
}

#[tokio::test]
async fn discovery_lists_mcp_and_signup() {
    let t = Test::new(&[("m", price("1", "1"))]).await;
    let v: Value = reqwest::get(format!("http://{}/v1", t.addr))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["mcp"], "/mcp");
    assert_eq!(v["signup"], "/v1/signup");
}
