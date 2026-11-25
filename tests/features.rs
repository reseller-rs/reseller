//! Image generation proxying, Payroof crypto payments, white-label branding,
//! and nested settings, driven through the real router against local mocks.

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, Path},
    http::{HeaderMap, Request},
    routing::{any, get, put},
};
use hmac::{Hmac, KeyInit, Mac};
use reseller::{App, config::Config, db, keys, payments::payroof};
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

const ADMIN: &str = "admin-test-secret-123456789";
const WHSEC: &str = "whsec_aXM6kkfVj6PFUBgwA3JVX9I18Rgp-ziMIuRMtjfwEhA";

struct Test {
    app: App,
    dir: tempfile::TempDir,
}

impl Test {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("test.db")).await.unwrap();
        let c = Config {
            admin_token: ADMIN.into(),
            key_pepper: "pepper-test-secret-12345678".into(),
            signup: reseller::config::Signup {
                cooldown_seconds: 0,
                ip_limit_per_month: 0,
                global_hourly_limit: 0,
                pow_max_number: 0,
                ..Default::default()
            },
            keys: reseller::config::KeyPolicy {
                rotate_cooldown_seconds: 0,
                ..Default::default()
            },
            ..Config::default()
        };
        let c = db::settings(&pool, c).await.unwrap();
        Self {
            app: App::new(pool, c).unwrap(),
            dir,
        }
    }

    async fn key(&self) -> Value {
        keys::create(
            &self.app,
            keys::Requester::Signup { altcha: None },
            "127.0.0.1",
            &json!({"name":"test"}),
        )
        .await
        .unwrap()
    }

    async fn raw(&self, req: Request<Body>) -> (u16, HeaderMap, Vec<u8>) {
        let mut req = req;
        req.extensions_mut()
            .insert(ConnectInfo("127.0.0.1:9000".parse::<SocketAddr>().unwrap()));
        let router = reseller::web::router(self.app.clone()).await;
        let r = router.oneshot(req).await.unwrap();
        let status = r.status().as_u16();
        let headers = r.headers().clone();
        let body = to_bytes(r.into_body(), 64 * 1024 * 1024).await.unwrap();
        (status, headers, body.to_vec())
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> (u16, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let body = if method == "GET" {
            Body::empty()
        } else {
            Body::from(body.to_string())
        };
        let (status, _, bytes) = self.raw(req.body(body).unwrap()).await;
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!({"text": String::from_utf8_lossy(&bytes)})),
        )
    }

    async fn admin(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        self.call(method, &format!("/admin/api/{path}"), Some(ADMIN), body)
            .await
    }

    async fn page(&self, path: &str) -> (u16, HeaderMap, String) {
        let (s, h, b) = self
            .raw(Request::get(path).body(Body::empty()).unwrap())
            .await;
        (s, h, String::from_utf8_lossy(&b).into_owned())
    }

    async fn balance(&self, aid: &str) -> String {
        reseller::billing::account(&self.app, aid).await.unwrap()["account"]["balance_usd"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

async fn serve(router: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    url
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// `/v1/images` routes to the image upstream with the configured model, keeps
/// inline reference images intact, and bills a response whose usage trails a
/// multi-megabyte base64 payload.
#[tokio::test]
async fn image_generation_routes_to_image_upstream_and_bills_trailing_usage() {
    let t = Test::new().await;
    let seen: Arc<Mutex<Vec<(String, Value, String)>>> = Arc::default();
    let log = seen.clone();
    let base = serve(Router::new().fallback(any(move |req: Request<Body>| {
        let log = log.clone();
        async move {
            let path = req.uri().path().to_owned();
            let title = req
                .headers()
                .get("x-title")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let body: Value =
                serde_json::from_slice(&to_bytes(req.into_body(), 1 << 24).await.unwrap())
                    .unwrap();
            log.lock().unwrap().push((path, body, title));
            // Larger than the 4 MiB inspection buffer: usage arrives last.
            let image = "A".repeat(5 * 1024 * 1024);
            Json(json!({
                "created": 1,
                "data": [{"b64_json": image, "media_type": "image/webp"}],
                "usage": {"prompt_tokens": 9, "completion_tokens": 642, "total_tokens": 651, "cost": 0.01}
            }))
        }
    })))
    .await;
    {
        let mut c = t.app.config_mut().await;
        c.image.base_url = format!("{base}/api/v1");
        c.image.api_key = "img-secret".into();
        c.image.model = "meta/muse-image".into();
        c.image.headers.insert("X-Title".into(), "Reseller".into());
        c.markup_pct = "0".into();
    }
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap();
    let before = t.balance(aid).await;
    assert_eq!(before, "0.1");
    let reference = "data:image/png;base64,iVBORw0KGgo=";
    let (s, v) = t
        .call(
            "POST",
            "/v1/images",
            k["key"].as_str(),
            json!({"prompt":"make it blue","input_references":[{"type":"image_url","image_url":{"url":reference}}]}),
        )
        .await;
    assert_eq!(
        s,
        200,
        "{}",
        v.to_string().chars().take(200).collect::<String>()
    );
    assert_eq!(v["usage"]["total_tokens"], 651);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    {
        let seen = seen.lock().unwrap();
        let (path, body, title) = &seen[0];
        assert_eq!(path, "/api/v1/images");
        assert_eq!(body["model"], "meta/muse-image", "default model applied");
        assert_eq!(body["input_references"][0]["image_url"]["url"], reference);
        assert_eq!(title, "Reseller", "configured upstream header sent");
    }
    // Provider cost $0.01 at 0% markup.
    assert_eq!(t.balance(aid).await, "0.09");
    let row = sqlx::query("SELECT kind,model,billed_micro,tokens FROM requests")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("kind"), "image");
    assert_eq!(row.get::<String, _>("model"), "meta/muse-image");
    assert_eq!(row.get::<i64, _>("billed_micro"), 10_000);
    assert_eq!(row.get::<i64, _>("tokens"), 651);

    // Without the image upstream key, image routes are unavailable even though
    // the LLM upstream may be configured.
    t.app.config_mut().await.image.api_key.clear();
    let (s, v) = t
        .call(
            "POST",
            "/v1/images/generations",
            k["key"].as_str(),
            json!({"prompt":"x"}),
        )
        .await;
    assert_eq!(s, 503, "{v}");
}

/// Without provider cost, images are priced per generated image.
#[tokio::test]
async fn image_price_applies_per_generated_image() {
    let t = Test::new().await;
    let base = serve(Router::new().fallback(any(|| async {
        Json(json!({"data": [{"url": "https://cdn.example/1.png"}, {"url": "https://cdn.example/2.png"}]}))
    })))
    .await;
    {
        let mut c = t.app.config_mut().await;
        c.image.base_url = format!("{base}/v1");
        c.image.api_key = "img-secret".into();
        c.markup_pct = "50".into();
        c.prices.insert(
            "artist".into(),
            reseller::config::Price {
                input: "0".into(),
                output: "0".into(),
                image: "0.02".into(),
                ..Default::default()
            },
        );
    }
    let k = t.key().await;
    let (s, _) = t
        .call(
            "POST",
            "/v1/images/generations",
            k["key"].as_str(),
            json!({"model":"artist","prompt":"two cats","n":2}),
        )
        .await;
    assert_eq!(s, 200);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (upstream, billed): (i64, i64) =
        sqlx::query_as("SELECT upstream_micro,billed_micro FROM requests")
            .fetch_one(&t.app.db)
            .await
            .unwrap();
    assert_eq!((upstream, billed), (40_000, 60_000));
}

// ---------------------------------------------------------------------------
// Payroof crypto payments
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PayroofMock {
    references: Vec<String>,
    webhooks: Vec<Value>,
    auth: Vec<String>,
}

async fn mock_payroof(state: Arc<Mutex<PayroofMock>>) -> String {
    let catalog = json!({
        "project": "shop",
        "watch_roots": [],
        "chains": [
            {"id": "eip155:1", "name": "Ethereum", "confirmations": 12, "assets": [
                {"id": "eip155:1/slip44:60", "symbol": "ETH", "decimals": 18},
                {"id": "eip155:1/erc20:0xa0b8", "symbol": "USDC", "decimals": 6},
                {"id": "eip155:1/erc20:0xdac1", "symbol": "USDT", "decimals": 6},
                {"id": "eip155:1/erc20:0xc02a", "symbol": "WETH", "decimals": 18}
            ]},
            {"id": "eip155:56", "name": "BNB Smart Chain", "confirmations": 15, "assets": [
                {"id": "eip155:56/slip44:714", "symbol": "BNB", "decimals": 18},
                {"id": "eip155:56/erc20:0x8ac7", "symbol": "USDC", "decimals": 18}
            ]},
            {"id": "eip155:137", "name": "Polygon", "confirmations": 64, "assets": [
                {"id": "eip155:137/slip44:966", "symbol": "POL", "decimals": 18}
            ]}
        ],
        "created_at": "2026-10-26T09:14:07Z"
    });
    let s1 = state.clone();
    let s2 = state.clone();
    let s3 = state.clone();
    let s4 = state.clone();
    let router = Router::new()
        .route(
            "/v1/projects/shop",
            get(move |h: HeaderMap| {
                let catalog = catalog.clone();
                let state = s1.clone();
                async move {
                    state.lock().unwrap().auth.push(
                        h.get("authorization").unwrap().to_str().unwrap().to_owned(),
                    );
                    Json(catalog)
                }
            }),
        )
        .route(
            "/v1/projects/shop/references/{reference}",
            put(move |Path(reference): Path<String>, body: String| {
                let state = s2.clone();
                async move {
                    assert!(body.is_empty(), "references take an empty body");
                    state.lock().unwrap().references.push(reference.clone());
                    (
                        axum::http::StatusCode::CREATED,
                        Json(json!({"project": "shop", "reference": reference, "destinations": [
                            {"chain": "eip155:1", "address": "0x14da8c659ce2b10cccdaebf990d19838b0d7ec0b"},
                            {"chain": "eip155:56", "address": "0x14da8c659ce2b10cccdaebf990d19838b0d7ec0b"}
                        ]})),
                    )
                }
            }),
        )
        .route(
            "/v1/projects/shop/webhooks",
            get(move || {
                let state = s3.clone();
                async move { Json(json!({"webhooks": state.lock().unwrap().webhooks.clone()})) }
            })
            .post(move |Json(body): Json<Value>| {
                let state = s4.clone();
                async move {
                    let hook = json!({"id": "wh_1", "url": body["url"], "events": body["events"], "created_at": "2026-10-26T09:14:07Z"});
                    state.lock().unwrap().webhooks.push(hook.clone());
                    let mut created = hook;
                    created["secret"] = WHSEC.into();
                    (axum::http::StatusCode::CREATED, Json(created))
                }
            }),
        )
        .route(
            "/simple/price",
            get(|q: axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                assert_eq!(q.0.get("vs_currencies").map(String::as_str), Some("usd"));
                Json(json!({"ethereum": {"usd": 2500.5}, "binancecoin": {"usd": 600}}))
            }),
        );
    serve(router).await
}

fn sign(body: &str, timestamp: i64) -> HeaderMap {
    let key = payroof::webhook_key(WHSEC).unwrap();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&key).unwrap();
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(body.as_bytes());
    let mut h = HeaderMap::new();
    let event: Value = serde_json::from_str(body).unwrap();
    h.insert("payroof-id", event["id"].as_str().unwrap().parse().unwrap());
    h.insert("payroof-timestamp", timestamp.to_string().parse().unwrap());
    h.insert(
        "payroof-signature",
        format!("v1={}", hex::encode(mac.finalize().into_bytes()))
            .parse()
            .unwrap(),
    );
    h
}

fn event(
    id: &str,
    kind: &str,
    transfer: &str,
    reference: &str,
    asset: &str,
    amount: &str,
) -> String {
    let chain = asset.split('/').next().unwrap();
    json!({
        "id": id, "event": kind, "transfer": transfer, "project": "shop", "reference": reference,
        "chain": chain, "asset": asset, "address": "0x14DA8C659CE2B10CCCDAEBF990D19838B0D7EC0B",
        "transaction": format!("0x{transfer}"), "amount": amount,
        "block": {"height": "21874302", "hash": "0x58"}
    })
    .to_string()
}

async fn deliver(t: &Test, body: &str) -> (u16, Value) {
    let headers = sign(body, db::now());
    let mut req = Request::post("/webhooks/payroof").header("content-type", "application/json");
    for (k, v) in &headers {
        req = req.header(k, v);
    }
    let (s, _, b) = t.raw(req.body(Body::from(body.to_owned())).unwrap()).await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

async fn configure_payroof(t: &Test, base: &str) {
    let (s, v) = t
        .admin(
            "PATCH",
            "settings",
            json!({"payroof": {
                "api_url": base, "project": "shop", "token": "prf_test_token",
                "webhook_secret": WHSEC, "rate_url": format!("{base}/simple/price"),
                "chains": ["eip155:1", "eip155:56"]
            }}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["settings"]["payroof"]["token"], "", "token is redacted");
}

#[tokio::test]
async fn payroof_checkout_quotes_assets_and_completes_on_confirmed_transfers() {
    let t = Test::new().await;
    let state: Arc<Mutex<PayroofMock>> = Arc::default();
    let base = mock_payroof(state.clone()).await;
    let k = t.key().await;
    let token = k["dashboard_token"].as_str().unwrap();
    let aid = k["account_id"].as_str().unwrap();

    // Not configured: no methods, checkout unavailable.
    let (_, plans) = t.call("GET", "/v1/plans", None, json!({})).await;
    assert_eq!(plans["payment_methods"], json!([]));
    configure_payroof(&t, &base).await;
    let (_, plans) = t.call("GET", "/v1/plans", None, json!({})).await;
    assert_eq!(plans["payment_methods"], json!(["crypto"]));
    let (s, v) = t
        .call(
            "POST",
            "/v1/account/checkout",
            Some(token),
            json!({"plan_code":"monthly","method":"stripe"}),
        )
        .await;
    assert_eq!(s, 503, "{v}");

    let (s, order) = t
        .call(
            "POST",
            "/v1/account/checkout",
            Some(token),
            json!({"plan_code":"monthly","method":"crypto"}),
        )
        .await;
    assert_eq!(s, 200, "{order}");
    let order_id = order["order_id"].as_str().unwrap().to_owned();
    assert_eq!(order["status"], "pending");
    assert_eq!(order["price_usd"], "20");
    assert_eq!(
        state.lock().unwrap().references,
        std::slice::from_ref(&order_id)
    );
    assert_eq!(state.lock().unwrap().auth[0], "Bearer prf_test_token");
    let quotes = order["quotes"].as_array().unwrap();
    let quote = |asset: &str| quotes.iter().find(|q| q["asset"] == asset).cloned();
    // Stablecoins at 1:1 with their own decimals (BSC USDC has 18).
    assert_eq!(quote("eip155:1/erc20:0xa0b8").unwrap()["amount"], "20");
    assert_eq!(
        quote("eip155:1/erc20:0xa0b8").unwrap()["amount_units"],
        "20000000"
    );
    assert_eq!(quote("eip155:1/erc20:0xdac1").unwrap()["symbol"], "USDT");
    assert_eq!(
        quote("eip155:56/erc20:0x8ac7").unwrap()["amount_units"],
        "20000000000000000000"
    );
    // Native coins at the feed rate, rounded up to 8 decimals.
    assert_eq!(quote("eip155:1/slip44:60").unwrap()["amount"], "0.00799841");
    assert_eq!(
        quote("eip155:56/slip44:714").unwrap()["amount"],
        "0.03333334"
    );
    // Non-stable tokens and chains outside the allowlist are not offered.
    assert!(quote("eip155:1/erc20:0xc02a").is_none());
    assert!(quote("eip155:137/slip44:966").is_none());
    assert_eq!(quotes.len(), 5);

    // `seen` records the transfer without crediting anything.
    let usdc = "eip155:1/erc20:0xa0b8";
    let (s, _) = deliver(
        &t,
        &event("evt_1", "seen", "trf_a", &order_id, usdc, "12000000"),
    )
    .await;
    assert_eq!(s, 200);
    let (_, detail) = t
        .call(
            "GET",
            &format!("/v1/account/payments/{order_id}"),
            Some(token),
            json!({}),
        )
        .await;
    assert_eq!(detail["status"], "pending");
    assert_eq!(detail["transfers"][0]["status"], "seen");
    assert_eq!(t.balance(aid).await, "0.1");

    // A confirmed partial payment counts but does not complete the order.
    let (s, _) = deliver(
        &t,
        &event("evt_2", "confirmed", "trf_a", &order_id, usdc, "12000000"),
    )
    .await;
    assert_eq!(s, 200);
    let (_, detail) = t
        .call(
            "GET",
            &format!("/v1/account/payments/{order_id}"),
            Some(token),
            json!({}),
        )
        .await;
    assert_eq!(detail["received_usd"], "12");
    assert_eq!(detail["status"], "pending");

    // Redelivery of the same event is ignored.
    let (s, v) = deliver(
        &t,
        &event("evt_2", "confirmed", "trf_a", &order_id, usdc, "12000000"),
    )
    .await;
    assert_eq!((s, v["duplicate"].clone()), (200, json!(true)));

    // The remainder in native ETH (confirmed without a prior `seen`)
    // completes the order: 8 USD of ETH at 2500.5 = 0.00319936 ETH (rounded).
    let (s, _) = deliver(
        &t,
        &event(
            "evt_3",
            "confirmed",
            "trf_b",
            &order_id,
            "eip155:1/slip44:60",
            "3199400000000000",
        ),
    )
    .await;
    assert_eq!(s, 200);
    let (_, detail) = t
        .call(
            "GET",
            &format!("/v1/account/payments/{order_id}"),
            Some(token),
            json!({}),
        )
        .await;
    assert_eq!(detail["status"], "paid", "{detail}");
    assert_eq!(detail["paid_by"], "payroof");
    assert_eq!(t.balance(aid).await, "20.1");

    // More transfers after payment never grant twice.
    deliver(
        &t,
        &event("evt_4", "confirmed", "trf_c", &order_id, usdc, "20000000"),
    )
    .await;
    assert_eq!(t.balance(aid).await, "20.1");
    let subs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM subscriptions")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(subs, 1);

    // The customer and the console list the order.
    let (_, list) = t
        .call("GET", "/v1/account/payments", Some(token), json!({}))
        .await;
    assert_eq!(list["payments"][0]["method"], "crypto");
    let (_, admin_list) = t.admin("GET", "payments?q=payroof", json!({})).await;
    assert_eq!(admin_list["total"], 1);
}

#[tokio::test]
async fn payroof_webhook_rejects_forgeries_and_ignores_foreign_references() {
    let t = Test::new().await;
    let state: Arc<Mutex<PayroofMock>> = Arc::default();
    let base = mock_payroof(state).await;
    configure_payroof(&t, &base).await;
    let body = event(
        "evt_x",
        "confirmed",
        "trf_x",
        "not-ours",
        "eip155:1/erc20:0xa0b8",
        "1",
    );

    // Unknown references (other apps on the same project) are acknowledged.
    let (s, v) = deliver(&t, &body).await;
    assert_eq!((s, v["ignored"].clone()), (200, json!(true)));

    // Tampered body, stale timestamp, and mismatched delivery id are rejected.
    let mut headers = sign(&body, db::now());
    let tampered = body.replace("\"1\"", "\"2\"");
    for (name, value, payload) in [
        ("payroof-signature", "v1=00", body.clone()),
        ("payroof-id", "evt_other", body.clone()),
        ("x-noop", "1", tampered),
    ] {
        let mut h = headers.clone();
        h.insert(name, value.parse().unwrap());
        let err = payroof::webhook(&t.app, &h, payload.as_bytes())
            .await
            .unwrap_err();
        assert_eq!(err.status.as_u16(), 400);
    }
    headers = sign(&body, db::now() - 301);
    assert!(
        payroof::webhook(&t.app, &headers, body.as_bytes())
            .await
            .is_err()
    );

    // Events for another project are ignored.
    let other = body
        .replace("\"shop\"", "\"other\"")
        .replace("evt_x", "evt_y");
    let (s, v) = deliver(&t, &other).await;
    assert_eq!((s, v["ignored"].clone()), (200, json!(true)));
}

#[tokio::test]
async fn late_native_transfers_need_review_and_admin_can_complete() {
    let t = Test::new().await;
    let state: Arc<Mutex<PayroofMock>> = Arc::default();
    let base = mock_payroof(state).await;
    configure_payroof(&t, &base).await;
    // Static rates only: BNB has none, so only ETH is quoted natively.
    let (s, _) = t
        .admin(
            "PATCH",
            "settings",
            json!({"payroof": {"rate_source": "static", "native_rates_usd": {"eth": "2000"}, "token": ""}}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.app.config().await.payroof.token,
        "prf_test_token",
        "blank secret kept"
    );
    let k = t.key().await;
    let token = k["dashboard_token"].as_str().unwrap();
    let aid = k["account_id"].as_str().unwrap();
    let (s, order) = t
        .call(
            "POST",
            "/v1/account/checkout",
            Some(token),
            json!({"plan_code":"monthly"}),
        )
        .await;
    assert_eq!(s, 200, "{order}");
    let quotes = order["quotes"].as_array().unwrap();
    assert!(
        quotes
            .iter()
            .any(|q| q["symbol"] == "ETH" && q["amount"] == "0.01")
    );
    assert!(!quotes.iter().any(|q| q["symbol"] == "BNB"));
    let order_id = order["order_id"].as_str().unwrap();

    // The quote expires before the transfer is seen.
    sqlx::query("UPDATE payments SET expires_at=? WHERE id=?")
        .bind(db::now() - 10)
        .bind(order_id)
        .execute(&t.app.db)
        .await
        .unwrap();
    let eth = "eip155:1/slip44:60";
    deliver(
        &t,
        &event(
            "evt_l1",
            "confirmed",
            "trf_l",
            order_id,
            eth,
            "10000000000000000",
        ),
    )
    .await;
    let (_, detail) = t
        .admin("GET", &format!("payments/{order_id}"), json!({}))
        .await;
    assert_eq!(detail["status"], "expired");
    assert_eq!(detail["transfers"][0]["note"], "late");
    assert_eq!(detail["received_usd"], "0");
    assert_eq!(t.balance(aid).await, "0.1");

    // A transfer to the wrong address or in an unquoted asset never counts.
    let wrong = event(
        "evt_l2",
        "confirmed",
        "trf_w",
        order_id,
        "eip155:1/erc20:0xc02a",
        "1",
    );
    deliver(&t, &wrong).await;
    let (_, detail) = t
        .admin("GET", &format!("payments/{order_id}"), json!({}))
        .await;
    assert!(
        detail["transfers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["note"] == "unquoted_asset")
    );

    // The operator verifies by hand and completes the order once.
    let (s, done) = t
        .admin("POST", &format!("payments/{order_id}/complete"), json!({}))
        .await;
    assert_eq!(s, 200, "{done}");
    assert_eq!(
        (done["status"].clone(), done["paid_by"].clone()),
        (json!("paid"), json!("admin"))
    );
    assert_eq!(t.balance(aid).await, "20.1");
    let (s, _) = t
        .admin("POST", &format!("payments/{order_id}/complete"), json!({}))
        .await;
    assert_eq!(s, 409);
}

#[tokio::test]
async fn console_registers_the_payroof_webhook_and_stores_its_secret() {
    let t = Test::new().await;
    let state: Arc<Mutex<PayroofMock>> = Arc::default();
    let base = mock_payroof(state.clone()).await;
    let (s, _) = t
        .admin(
            "PATCH",
            "settings",
            json!({"payroof": {"api_url": base, "project": "shop", "token": "prf_test_token"}}),
        )
        .await;
    assert_eq!(s, 200);
    // Payroof only delivers to HTTPS.
    let (s, _) = t.admin("POST", "payroof/webhook", json!({})).await;
    assert_eq!(s, 400);
    t.admin(
        "PATCH",
        "settings",
        json!({"public_url": "https://api.example.com"}),
    )
    .await;
    let (s, v) = t.admin("POST", "payroof/webhook", json!({})).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["url"], "https://api.example.com/webhooks/payroof");
    assert_eq!(t.app.config().await.payroof.webhook_secret, WHSEC);
    // Persisted, not only in memory.
    let stored = db::settings(&t.app.db, Config::default()).await.unwrap();
    assert_eq!(stored.payroof.webhook_secret, WHSEC);
    let (s, status) = t.admin("GET", "payroof", json!({})).await;
    assert_eq!(s, 200);
    assert_eq!(status["enabled"], true);
    assert_eq!(status["webhook_registered"], true);
    assert_eq!(status["chains"].as_array().unwrap().len(), 3);
}

// ---------------------------------------------------------------------------
// Branding and theme files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn branding_settings_render_into_pages_and_theme_css() {
    let t = Test::new().await;
    let (s, v) = t
        .admin(
            "PATCH",
            "settings",
            json!({"branding": {
                "name": "Acme AI", "tagline": "Model gateway", "logo_url": "/theme/logo.svg",
                "favicon_url": "https://cdn.example.com/icon.png", "hero_title": "Build <fast>",
                "css_vars": {"accent": "#ff7a1a"}, "custom_css": ".hero h1 { letter-spacing: 0; }",
                "footer_links": [{"label": "Status", "url": "https://status.example.com"}]
            }}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    for path in ["/", "/workspace/", "/admin/"] {
        let (s, h, html) = t.page(path).await;
        assert_eq!(s, 200);
        assert!(
            h["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("img-src 'self' data: https:")
        );
        assert!(
            html.contains("<title>Acme AI: Model gateway</title>"),
            "{path}"
        );
        assert!(html.contains("src=\"/theme/logo.svg\""));
        assert!(html.contains("href=\"https://cdn.example.com/icon.png\""));
        assert!(html.contains("href=\"https://status.example.com\""));
        assert!(
            html.contains("Build \\u003cfast\\u003e"),
            "branding JSON is script-safe"
        );
    }
    let (s, h, css) = t.page("/assets/theme.css").await;
    assert_eq!(s, 200);
    assert!(h["content-type"].to_str().unwrap().starts_with("text/css"));
    assert!(css.contains("--accent: #ff7a1a;"));
    assert!(css.contains(".hero h1 { letter-spacing: 0; }"));

    // Unsafe values are rejected and leave the settings untouched.
    let (s, _) = t
        .admin(
            "PATCH",
            "settings",
            json!({"branding": {"css_vars": {"accent": "red;}body{x"}}}),
        )
        .await;
    assert_eq!(s, 400);
    let (s, _) = t
        .admin(
            "PATCH",
            "settings",
            json!({"branding": {"logo_url": "javascript:alert(1)"}}),
        )
        .await;
    assert_eq!(s, 400);
    assert_eq!(t.app.config().await.branding.name, "Acme AI");
}

#[tokio::test]
async fn theme_directory_serves_files_and_overrides_template_safely() {
    let mut t = Test::new().await;
    let theme = t.dir.path().join("theme");
    std::fs::create_dir_all(theme.join("img")).unwrap();
    std::fs::write(theme.join("img/logo.svg"), "<svg/>").unwrap();
    std::fs::write(theme.join("theme.css"), "body { background: #fff; }").unwrap();
    std::fs::write(theme.join("secret.toml"), "token = 1").unwrap();
    std::fs::write(t.dir.path().join("outside.css"), "nope").unwrap();
    t.app = t.app.clone().with_theme_dir(Some(theme.clone()));

    let (s, h, body) = t.page("/theme/img/logo.svg").await;
    assert_eq!((s, body.as_str()), (200, "<svg/>"));
    assert_eq!(h["content-type"], "image/svg+xml");
    for blocked in [
        "/theme/secret.toml",
        "/theme/../outside.css",
        "/theme/%2e%2e/outside.css",
        "/theme/.hidden.css",
        "/theme/missing.png",
    ] {
        assert_eq!(t.page(blocked).await.0, 404, "{blocked}");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(t.dir.path().join("outside.css"), theme.join("link.css"))
            .unwrap();
        assert_eq!(t.page("/theme/link.css").await.0, 404, "symlink escape");
    }
    let (_, _, css) = t.page("/assets/theme.css").await;
    assert!(css.contains("body { background: #fff; }"));

    std::fs::write(
        theme.join("index.html"),
        "<html><title>{{title}}</title><h1>{{name}}</h1></html>",
    )
    .unwrap();
    let (s, _, html) = t.page("/").await;
    assert_eq!(s, 200);
    assert_eq!(
        html,
        "<html><title>Reseller: API workspace</title><h1>Reseller</h1></html>"
    );
}

#[tokio::test]
async fn theme_favicon_gets_png_and_touch_icon_companions() {
    let mut t = Test::new().await;
    let theme = t.dir.path().join("theme");
    std::fs::create_dir_all(&theme).unwrap();
    std::fs::write(theme.join("favicon.svg"), "<svg/>").unwrap();
    t.app = t.app.clone().with_theme_dir(Some(theme.clone()));
    t.app.config_mut().await.branding.favicon_url = "/theme/favicon.svg".into();

    let (_, _, html) = t.page("/").await;
    assert!(html.contains(r#"href="/theme/favicon.svg""#));
    assert!(
        !html.contains("apple-touch-icon"),
        "only existing files are linked"
    );
    assert!(!html.contains("/theme/favicon-32.png"));

    std::fs::write(theme.join("favicon-32.png"), "png").unwrap();
    std::fs::write(theme.join("apple-touch-icon.png"), "png").unwrap();
    let (_, _, html) = t.page("/").await;
    assert!(html.contains(
        r#"<link rel="icon" type="image/png" sizes="32x32" href="/theme/favicon-32.png">"#
    ));
    assert!(html.contains(
        r#"<link rel="apple-touch-icon" sizes="180x180" href="/theme/apple-touch-icon.png">"#
    ));
    assert!(html.find("apple-touch-icon").unwrap() < html.find("</head>").unwrap());
}

// ---------------------------------------------------------------------------
// Settings and accounting fixes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn nested_secrets_are_redacted_and_preserved_by_blank_patches() {
    let t = Test::new().await;
    let (s, v) = t
        .admin(
            "PATCH",
            "settings",
            json!({"stripe": {"secret_key": "sk_live_x", "webhook_secret": "whsec_x"},
                   "image": {"api_key": "img-key", "headers": {"X-Title": "A"}}}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["settings"]["stripe"]["secret_key"], "");
    assert_eq!(v["settings"]["image"]["api_key"], "");
    let (s, _) = t
        .admin(
            "PATCH",
            "settings",
            json!({"stripe": {"secret_key": ""}, "image": {"api_key": "", "headers": {"X-Other": "B"}}}),
        )
        .await;
    assert_eq!(s, 200);
    let c = t.app.config().await;
    assert_eq!(c.stripe.secret_key, "sk_live_x");
    assert_eq!(c.image.api_key, "img-key");
    assert_eq!(
        c.image.headers.len(),
        1,
        "header maps are replaced wholesale"
    );
    let (_, plans) = t.call("GET", "/v1/plans", None, json!({})).await;
    assert_eq!(plans["payment_methods"], json!(["stripe"]));
}

#[tokio::test]
async fn account_list_balance_subtracts_debt_and_search_is_literal() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap();
    let mut tx = db::write(&t.app.db).await.unwrap();
    reseller::billing::debit(&mut tx, aid, 1_000_000)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (_, v) = t.admin("GET", "accounts", json!({})).await;
    assert_eq!(v["accounts"][0]["balance_usd"], "-0.9");
    assert_eq!(t.balance(aid).await, "-0.9");
    // `%` and `_` are matched literally rather than as wildcards.
    let (_, v) = t.admin("GET", "accounts?q=%25", json!({})).await;
    assert_eq!(v["total"], 0);
    let (_, v) = t.admin("GET", "keys?q=_", json!({})).await;
    assert_eq!(v["total"], 0);
}

// ---------------------------------------------------------------------------
// Model catalog, OpenAI errors, quota, deposits, and trial credit
// ---------------------------------------------------------------------------

const ERROR_KEYS: [&str; 4] = ["code", "message", "param", "type"];

fn error_keys(v: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = v["error"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// Serves an LLM upstream that records every request body it receives.
async fn recording_llm(t: &Test) -> Arc<Mutex<Vec<Value>>> {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let log = seen.clone();
    let base = serve(Router::new().fallback(any(move |req: Request<Body>| {
        let log = log.clone();
        async move {
            let body = to_bytes(req.into_body(), 1 << 20).await.unwrap();
            log.lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
            Json(json!({"id": "mock", "usage": {"total_tokens": 3, "cost": 0.0001}}))
        }
    })))
    .await;
    let mut c = t.app.config_mut().await;
    c.llm.base_url = format!("{base}/v1");
    c.llm.api_key = "upstream-secret".into();
    c.llm.model = "configured".into();
    seen
}

fn catalog() -> Vec<reseller::config::Model> {
    vec![
        reseller::config::Model {
            id: "assistant".into(),
            upstream: "vendor/secret-upstream-name".into(),
            aliases: vec!["gpt-4o".into()],
            name: "Assistant".into(),
            context_length: Some(128_000),
            ..Default::default()
        },
        reseller::config::Model {
            id: "other".into(),
            upstream: "vendor/other".into(),
            ..Default::default()
        },
        reseller::config::Model {
            id: "retired".into(),
            enabled: false,
            ..Default::default()
        },
    ]
}

async fn empty_balance(t: &Test) {
    sqlx::query("UPDATE credits SET remaining_micro=0")
        .execute(&t.app.db)
        .await
        .unwrap();
}

#[tokio::test]
async fn models_list_follows_the_catalog_and_hides_upstream_names() {
    let t = Test::new().await;
    t.app.config_mut().await.models = catalog();
    let k = t.key().await;
    let key = k["key"].as_str().unwrap();
    let (s, v) = t.call("GET", "/v1/models", Some(key), json!({})).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["object"], "list");
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["assistant", "other"],
        "disabled and aliases are hidden"
    );
    let first = &v["data"][0];
    assert_eq!(first["object"], "model");
    assert_eq!(first["name"], "Assistant");
    assert_eq!(first["context_length"], 128_000);
    assert!(first.get("pricing").is_none());
    assert!(!v.to_string().contains("secret-upstream-name"));
    let (s, v) = t
        .call("GET", "/v1/models/gpt-4o", Some(key), json!({}))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["id"], "assistant");
    let (s, v) = t
        .call("GET", "/v1/models/retired", Some(key), json!({}))
        .await;
    assert_eq!(s, 404);
    assert_eq!(v["error"]["code"], "model_not_found");
    assert_eq!(v["error"]["param"], "model");
    assert_eq!(
        v["error"]["message"],
        "The model 'retired' does not exist or you do not have access to it."
    );
}

#[tokio::test]
async fn models_list_derives_capability_and_modalities_from_the_routes() {
    let t = Test::new().await;
    {
        let mut c = t.app.config_mut().await;
        c.llm.model = "chat-up".into();
        c.tts.model = "voice-up".into();
        c.tts.fallbacks = vec!["voice-fallback".into()];
        c.stt.model = "ears-up".into();
        c.image.model = "paint-up".into();
    }
    let k = t.key().await;
    let key = k["key"].as_str().unwrap();
    let (s, v) = t.call("GET", "/v1/models", Some(key), json!({})).await;
    assert_eq!(s, 200, "{v}");
    let arch = |id: &str| {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("{id} in {v}"))["architecture"]
            .clone()
    };
    assert_eq!(arch("chat-up")["modality"], "text->text");
    assert_eq!(arch("chat-up")["capability"], "chat");
    assert_eq!(arch("voice-up")["modality"], "text->audio");
    assert_eq!(arch("voice-fallback")["capability"], "speech");
    assert_eq!(arch("ears-up")["modality"], "audio->text");
    assert_eq!(arch("ears-up")["capability"], "transcription");
    assert_eq!(arch("paint-up")["modality"], "text->image");
    assert_eq!(arch("paint-up")["capability"], "image");
    // An explicit catalog entry wins over derivation.
    t.app.config_mut().await.models = vec![reseller::config::Model {
        id: "vision".into(),
        upstream: "chat-up".into(),
        input_modalities: vec!["text".into(), "image".into()],
        ..Default::default()
    }];
    let (_, v) = t
        .call("GET", "/v1/models/vision", Some(key), json!({}))
        .await;
    assert_eq!(v["architecture"]["modality"], "text+image->text");
    assert_eq!(v["architecture"]["capability"], "chat");
}

#[tokio::test]
async fn models_list_requires_a_valid_key_and_a_balance() {
    let t = Test::new().await;
    let (s, v) = t.call("GET", "/v1/models", None, json!({})).await;
    assert_eq!(s, 401);
    assert_eq!(error_keys(&v), ERROR_KEYS);
    assert_eq!(v["error"]["code"], "missing_api_key");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(
        v["error"]["message"],
        "Missing bearer authentication in header"
    );
    let (s, v) = t
        .call("GET", "/v1/models", Some("sk-wrong"), json!({}))
        .await;
    assert_eq!(s, 401);
    assert_eq!(error_keys(&v), ERROR_KEYS);
    assert_eq!(v["error"]["code"], "invalid_api_key");
    let k = t.key().await;
    empty_balance(&t).await;
    let (s, v) = t
        .call("GET", "/v1/models", k["key"].as_str(), json!({}))
        .await;
    assert_eq!(s, 429, "{v}");
    assert_eq!(error_keys(&v), ERROR_KEYS);
    assert_eq!(v["error"]["code"], "insufficient_quota");
    assert_eq!(v["error"]["type"], "insufficient_quota");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with("You exceeded your current quota."),
        "{message}"
    );
    assert!(message.contains("/workspace/"), "{message}");
}

#[tokio::test]
async fn empty_balance_blocks_chat_with_the_deposit_instructions() {
    let t = Test::new().await;
    let seen = recording_llm(&t).await;
    let base = mock_payroof(Arc::default()).await;
    configure_payroof(&t, &base).await;
    let k = t.key().await;
    empty_balance(&t).await;
    let (s, v) = t
        .call(
            "POST",
            "/v1/chat/completions",
            k["key"].as_str(),
            json!({"messages": []}),
        )
        .await;
    assert_eq!(s, 429, "{v}");
    assert_eq!(v["error"]["type"], "insufficient_quota");
    assert_eq!(v["error"]["code"], "insufficient_quota");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("0x14da8c659ce2b10cccdaebf990d19838b0d7ec0b"),
        "{message}"
    );
    assert!(message.contains("GET /v1/account"), "{message}");
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_paths_and_wrong_methods_use_openai_errors() {
    let t = Test::new().await;
    let k = t.key().await;
    let key = k["key"].as_str();
    let (s, v) = t.call("GET", "/v1/nope", key, json!({})).await;
    assert_eq!(s, 404);
    assert_eq!(v["error"]["code"], "unknown_url");
    assert_eq!(v["error"]["message"], "Invalid URL (GET /v1/nope)");
    assert_eq!(error_keys(&v), ERROR_KEYS);
    let req = Request::get("/v1/chat/completions")
        .header("authorization", format!("Bearer {}", key.unwrap()))
        .body(Body::empty())
        .unwrap();
    let (s, headers, body) = t.raw(req).await;
    assert_eq!(s, 405);
    assert!(
        headers
            .get("allow")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("POST")
    );
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "method_not_allowed");
    assert_eq!(v["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn aliases_reach_the_upstream_by_its_name_and_hidden_models_are_not_found() {
    let t = Test::new().await;
    let seen = recording_llm(&t).await;
    t.app.config_mut().await.models = catalog();
    let k = t.key().await;
    let key = k["key"].as_str();
    let chat = |model: &'static str| json!({"model": model, "messages": []});
    let (s, v) = t
        .call("POST", "/v1/chat/completions", key, chat("gpt-4o"))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        seen.lock().unwrap()[0]["model"],
        "vendor/secret-upstream-name"
    );
    for unknown in ["missing", "retired", "vendor/secret-upstream-name"] {
        let (s, v) = t
            .call("POST", "/v1/chat/completions", key, chat(unknown))
            .await;
        assert_eq!(s, 404, "{unknown}: {v}");
        assert_eq!(v["error"]["code"], "model_not_found");
        assert_eq!(v["error"]["param"], "model");
    }
    sqlx::query("UPDATE api_keys SET limits=?")
        .bind(json!({"allowed_models": ["other"]}).to_string())
        .execute(&t.app.db)
        .await
        .unwrap();
    let (s, v) = t
        .call("POST", "/v1/chat/completions", key, chat("assistant"))
        .await;
    assert_eq!(s, 404, "{v}");
    assert_eq!(v["error"]["code"], "model_not_found");
    let (s, v) = t.call("GET", "/v1/models", key, json!({})).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["id"], "other");
    let (s, _) = t.call("GET", "/v1/models/assistant", key, json!({})).await;
    assert_eq!(s, 404);
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "rejected requests never reach the upstream"
    );
}

#[tokio::test]
async fn deposit_webhook_credits_stablecoins_once() {
    let t = Test::new().await;
    let base = mock_payroof(Arc::default()).await;
    configure_payroof(&t, &base).await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap();
    let reference = format!("acct-{aid}");
    let usdc = "eip155:1/erc20:0xa0b8";
    let before = t.balance(aid).await;
    let (s, v) = t
        .call("GET", "/v1/account", k["key"].as_str(), json!({}))
        .await;
    assert_eq!(s, 200, "{v}");

    let seen = event("evt_d1", "seen", "tr_d1", &reference, usdc, "5000000");
    assert_eq!(deliver(&t, &seen).await.0, 200);
    assert_eq!(
        t.balance(aid).await,
        before,
        "a seen transfer is not credited"
    );

    let confirmed = event("evt_d2", "confirmed", "tr_d1", &reference, usdc, "5000000");
    assert_eq!(deliver(&t, &confirmed).await.0, 200);
    let credited = t.balance(aid).await;
    assert_ne!(credited, before);
    assert_eq!(deliver(&t, &confirmed).await.0, 200, "replay");
    let again = event("evt_d3", "confirmed", "tr_d1", &reference, usdc, "5000000");
    assert_eq!(deliver(&t, &again).await.0, 200, "same transfer, new event");
    assert_eq!(t.balance(aid).await, credited, "credited once");

    let weth = event(
        "evt_d4",
        "confirmed",
        "tr_d2",
        &reference,
        "eip155:1/erc20:0xc02a",
        "1000000000000000000",
    );
    assert_eq!(deliver(&t, &weth).await.0, 200);
    let eth = event(
        "evt_d5",
        "confirmed",
        "tr_d3",
        &reference,
        "eip155:1/slip44:60",
        "1000000000000000000",
    );
    assert_eq!(deliver(&t, &eth).await.0, 200);
    assert_eq!(
        t.balance(aid).await,
        credited,
        "unsupported assets are not credited"
    );
    let notes: Vec<String> =
        sqlx::query_scalar("SELECT note FROM deposit_transfers WHERE id IN ('tr_d2','tr_d3')")
            .fetch_all(&t.app.db)
            .await
            .unwrap();
    assert_eq!(notes, ["unsupported_asset", "unsupported_asset"]);
}

#[tokio::test]
async fn account_top_up_lists_deposit_addresses_without_secrets() {
    let t = Test::new().await;
    let base = mock_payroof(Arc::default()).await;
    configure_payroof(&t, &base).await;
    let k = t.key().await;
    let (s, v) = t
        .call("GET", "/v1/account", k["key"].as_str(), json!({}))
        .await;
    assert_eq!(s, 200, "{v}");
    let top_up = &v["top_up"];
    assert_eq!(top_up["automatic"], true);
    assert_eq!(top_up["eta_minutes"], 5);
    assert_eq!(top_up["addresses"][0]["chain"], "eip155:1");
    assert_eq!(top_up["addresses"][0]["chain_name"], "Ethereum");
    assert_eq!(
        top_up["addresses"][0]["address"],
        "0x14da8c659ce2b10cccdaebf990d19838b0d7ec0b"
    );
    let symbols: Vec<&str> = top_up["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["symbol"].as_str().unwrap())
        .collect();
    assert!(
        symbols.contains(&"USDC") && !symbols.contains(&"ETH"),
        "{symbols:?}"
    );
    let net = &top_up["networks"][0];
    assert_eq!(net["chain_id"], 1);
    assert_eq!(net["address"], top_up["addresses"][0]["address"]);
    assert_eq!(
        net["explorer"]["address"],
        "https://etherscan.io/address/{address}"
    );
    let usdc = net["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["symbol"] == "USDC")
        .expect("USDC listed");
    assert_eq!(usdc["native"], false);
    assert_eq!(usdc["contract"], "0xa0b8");
    assert_eq!(usdc["credit"], "1 USDC = 1 USD of credit");
    assert_eq!(top_up["recent"], json!([]));
    let aid = k["account_id"].as_str().unwrap();
    let reference = format!("acct-{aid}");
    let usdc_id = "eip155:1/erc20:0xa0b8";
    for (id, kind) in [("e1", "seen"), ("e2", "confirmed")] {
        let ev = event(id, kind, "tr_r1", &reference, usdc_id, "2500000");
        assert_eq!(deliver(&t, &ev).await.0, 200);
    }
    let ev = event(
        "e3",
        "confirmed",
        "tr_r2",
        &reference,
        "eip155:1/slip44:60",
        "1000",
    );
    assert_eq!(deliver(&t, &ev).await.0, 200);
    let (_, v) = t
        .call("GET", "/v1/account", k["key"].as_str(), json!({}))
        .await;
    let recent = v["top_up"]["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 2);
    let paid = recent.iter().find(|r| r["id"] == "tr_r1").unwrap();
    assert_eq!(
        (
            paid["state"].as_str(),
            paid["amount"].as_str(),
            paid["symbol"].as_str()
        ),
        (Some("credited"), Some("2.5"), Some("USDC"))
    );
    assert_eq!(paid["credited_usd"], "2.5");
    let native = recent.iter().find(|r| r["id"] == "tr_r2").unwrap();
    assert_eq!(native["state"], "not_credited");
    assert_eq!(native["note"], "unsupported_asset");
    let text = top_up.to_string();
    for secret in ["prf_test_token", "shop", "acct-", &base] {
        assert!(!text.contains(secret), "{secret} leaked in {text}");
    }
}

#[tokio::test]
async fn account_top_up_degrades_when_payroof_is_down_or_deposits_are_off() {
    let t = Test::new().await;
    configure_payroof(&t, "http://127.0.0.1:1").await;
    let k = t.key().await;
    let (s, v) = t
        .call("GET", "/v1/account", k["key"].as_str(), json!({}))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["top_up"]["available"], false);
    assert_eq!(v["top_up"]["addresses"], json!([]));
    t.app.config_mut().await.payroof.deposits = false;
    let (_, v) = t
        .call("GET", "/v1/account", k["key"].as_str(), json!({}))
        .await;
    assert_eq!(v["top_up"]["automatic"], false);
    assert_eq!(v["top_up"]["addresses"], json!([]));
    assert!(v["top_up"].get("available").is_none());
    assert!(
        v["top_up"]["workspace_url"]
            .as_str()
            .unwrap()
            .ends_with("/workspace/")
    );
}

#[tokio::test]
async fn new_accounts_get_the_default_or_configured_trial_credit() {
    let t = Test::new().await;
    let k = t.key().await;
    assert_eq!(t.balance(k["account_id"].as_str().unwrap()).await, "0.1");
    t.app.config_mut().await.signup.free_credit_usd = "0.75".into();
    let k = t.key().await;
    assert_eq!(t.balance(k["account_id"].as_str().unwrap()).await, "0.75");
}

#[tokio::test]
async fn halving_gives_each_further_signup_from_one_ip_half_the_trial() {
    let t = Test::new().await;
    {
        let mut c = t.app.config_mut().await;
        c.signup.free_credit_usd = "0.1".into();
        c.signup.free_credit_halving = true;
        c.signup.ip_limit_per_month = 4;
    }
    let signup = |ip: &'static str| {
        let app = t.app.clone();
        async move {
            reseller::keys::create(
                &app,
                reseller::keys::Requester::Signup { altcha: None },
                ip,
                &json!({}),
            )
            .await
        }
    };
    for expected in ["0.1", "0.05", "0.025", "0.0125"] {
        let v = signup("203.0.113.9").await.unwrap();
        assert_eq!(v["trial_credit_usd"], expected);
        assert_eq!(t.balance(v["account_id"].as_str().unwrap()).await, expected);
    }
    // The fifth is over the monthly allowance; another IP starts at full credit.
    assert!(signup("203.0.113.9").await.is_err());
    let other = signup("198.51.100.4").await.unwrap();
    assert_eq!(other["trial_credit_usd"], "0.1");

    // Without halving every signup gets the full amount.
    t.app.config_mut().await.signup.free_credit_halving = false;
    t.app.config_mut().await.signup.ip_limit_per_month = 0;
    let v = signup("203.0.113.9").await.unwrap();
    assert_eq!(v["trial_credit_usd"], "0.1");
}

/// The overview counts only credit active accounts can spend as outstanding.
/// Credit of suspended accounts and customer debt are reported apart, and
/// usable keys exclude keys of suspended accounts.
#[tokio::test]
async fn overview_splits_credit_and_keys_by_account_status() {
    let t = Test::new().await;
    let active = t.key().await;
    let suspended = t.key().await;
    let (s, v) = t
        .admin(
            "PATCH",
            &format!("accounts/{}", suspended["account_id"].as_str().unwrap()),
            json!({"status": "suspended"}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    sqlx::query("UPDATE accounts SET debt_micro=250000 WHERE id=?")
        .bind(active["account_id"].as_str())
        .execute(&t.app.db)
        .await
        .unwrap();
    let (s, v) = t.admin("GET", "overview", json!({})).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["accounts"], 2);
    assert_eq!(v["suspended_accounts"], 1);
    assert_eq!(v["active_keys"], 1);
    assert_eq!(v["outstanding_usd"], "0.1");
    assert_eq!(v["suspended_credit_usd"], "0.1");
    assert_eq!(v["debt_usd"], "0.25");
}

/// Key and account lists carry lifetime and 30-day spend and rank by them,
/// request rows are labelled with the key and account, and customer views
/// never show the provider cost.
#[tokio::test]
async fn dashboards_rank_spend_and_link_keys_accounts_and_requests() {
    let t = Test::new().await;
    let first = t.key().await;
    let other = t.key().await;
    let token = first["key"].as_str().unwrap();
    let aid = first["account_id"].as_str().unwrap();
    let (s, second) = t
        .call(
            "POST",
            "/v1/keys",
            Some(token),
            json!({"name": "Batch jobs"}),
        )
        .await;
    assert_eq!(s, 200, "{second}");
    let (k1, k2) = (
        first["id"].as_str().unwrap(),
        second["id"].as_str().unwrap(),
    );
    let now = db::now();
    for (kid, hour, billed, lifetime) in [
        (k1, now - 40 * 86400, 900_000, 1_000_000),
        (k2, now - 3600, 300_000, 300_000),
    ] {
        sqlx::query("INSERT INTO usage_buckets VALUES(?,?,?,'m',2,100,?,?)")
            .bind(hour / 3600 * 3600)
            .bind(kid)
            .bind(aid)
            .bind(billed / 2)
            .bind(billed)
            .execute(&t.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE api_keys SET billed_micro=? WHERE id=?")
            .bind(lifetime)
            .bind(kid)
            .execute(&t.app.db)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO requests(id,account_id,key_id,created_at,method,path,kind,model,ip,status,\
         upstream_micro,billed_micro,finished) VALUES('r1',?,?,?,'POST','/v1/chat/completions','llm','m','127.0.0.1',200,1000,1300,1)",
    )
    .bind(aid)
    .bind(k2)
    .bind(now)
    .execute(&t.app.db)
    .await
    .unwrap();
    sqlx::query("UPDATE accounts SET contact='ops@example.com' WHERE id=?")
        .bind(aid)
        .execute(&t.app.db)
        .await
        .unwrap();

    // Lifetime spend ranks the old key first, 30-day spend the new one.
    let (_, v) = t.admin("GET", "keys?sort=spend&dir=desc", json!({})).await;
    assert_eq!(v["keys"][0]["id"], k1);
    assert_eq!(v["keys"][0]["billed_usd"], "1");
    assert_eq!(v["keys"][0]["usage_30d_usd"], "0");
    assert_eq!(v["keys"][0]["account_contact"], "ops@example.com");
    let (_, v) = t.admin("GET", "keys?sort=usage&dir=desc", json!({})).await;
    assert_eq!(v["keys"][0]["id"], k2);
    assert_eq!(v["keys"][0]["usage_30d_usd"], "0.3");
    let (_, v) = t
        .admin("GET", &format!("keys?account={aid}"), json!({}))
        .await;
    assert_eq!(v["total"], 2);

    let (_, v) = t
        .admin("GET", "accounts?sort=spend&dir=desc", json!({}))
        .await;
    assert_eq!(v["accounts"][0]["id"], aid);
    assert_eq!(v["accounts"][0]["billed_usd"], "1.3");
    assert_eq!(v["accounts"][0]["usage_30d_usd"], "0.3");
    assert_eq!(v["accounts"][0]["active_key_count"], 2);
    assert_eq!(v["accounts"][1]["id"], other["account_id"]);
    let (_, v) = t.admin("GET", &format!("accounts/{aid}"), json!({})).await;
    assert_eq!(v["account"]["billed_usd"], "1.3");

    // Request rows name their key and account for the operator.
    let (_, v) = t
        .admin("GET", &format!("requests?key={k2}"), json!({}))
        .await;
    assert_eq!(v["total"], 1);
    assert_eq!(v["requests"][0]["key_name"], "Batch jobs");
    assert_eq!(v["requests"][0]["account_contact"], "ops@example.com");
    assert_eq!(v["requests"][0]["upstream_usd"], "0.001");
    let (_, v) = t.admin("GET", "usage", json!({})).await;
    assert_eq!(v["series"][0]["upstream_cost_usd"], "0.15");

    // Customers see billed amounts only.
    let (_, v) = t
        .call("GET", "/v1/account/requests", Some(token), json!({}))
        .await;
    assert_eq!(v["requests"][0]["key_name"], "Batch jobs");
    assert_eq!(v["requests"][0]["billed_usd"], "0.0013");
    assert!(v["requests"][0].get("upstream_usd").is_none(), "{v}");
    let (_, v) = t
        .call("GET", "/v1/account/usage", Some(token), json!({}))
        .await;
    assert_eq!(v["series"][0]["billed_cost_usd"], "0.3");
    assert!(v["series"][0].get("upstream_cost_usd").is_none(), "{v}");

    // A customer opens one of its keys, never another account's.
    let (s, v) = t
        .call("GET", &format!("/v1/keys/{k2}"), Some(token), json!({}))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["key"]["billed_usd"], "0.3");
    assert_eq!(v["usage"]["series"].as_array().unwrap().len(), 1);
    let (s, _) = t
        .call(
            "GET",
            &format!("/v1/keys/{}", other["id"].as_str().unwrap()),
            Some(token),
            json!({}),
        )
        .await;
    assert_eq!(s, 404);

    // Dashboard tokens, which cannot call /v1/models, still list the models.
    let dashboard = first["dashboard_token"].as_str().unwrap();
    let (s, v) = t
        .call("GET", "/v1/account/models", Some(dashboard), json!({}))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["object"], "list");
    let (s, _) = t
        .call("GET", "/v1/models", Some(dashboard), json!({}))
        .await;
    assert_eq!(s, 401);

    // Payments filter by account.
    for (id, account) in [("o1", aid), ("o2", other["account_id"].as_str().unwrap())] {
        sqlx::query(
            "INSERT INTO payments(id,account_id,plan_code,provider,price_micro,credit_micro,duration_days,created_at) \
             VALUES(?,?,'monthly','stripe',20000000,20000000,30,?)",
        )
        .bind(id)
        .bind(account)
        .bind(now)
        .execute(&t.app.db)
        .await
        .unwrap();
    }
    let (_, v) = t
        .admin("GET", &format!("payments?account={aid}"), json!({}))
        .await;
    assert_eq!(v["total"], 1);
    assert_eq!(v["payments"][0]["id"], "o1");
}

/// The docs page is served by the dashboard shell.
#[tokio::test]
async fn docs_page_is_served() {
    let t = Test::new().await;
    for path in ["/docs", "/docs/"] {
        let (s, _, body) = t.page(path).await;
        assert_eq!(s, 200);
        assert!(body.contains("/assets/app.js"));
    }
}
