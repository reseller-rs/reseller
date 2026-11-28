//! Image generation proxying, Payroof crypto payments, App Store and Google
//! Play purchases, white-label branding, and nested settings, driven through
//! the real router against local mocks.

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, Path},
    http::{HeaderMap, Request},
    routing::{any, get, put},
};
use hmac::{Hmac, KeyInit, Mac};
use reseller::{
    App,
    config::{Capability, Config, Tier},
    db, keys,
    payments::payroof,
};
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

mod common;

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

    /// A new account with 0.10 USD of credit.
    async fn key(&self) -> Value {
        let key = self.unfunded_key().await;
        common::fund(&self.app, key["account_id"].as_str().unwrap(), common::DIME).await;
        key
    }

    /// A new account, as signup leaves it: without credit.
    async fn unfunded_key(&self) -> Value {
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
        common::serve(&mut c, "image", Capability::Image, "meta/muse-image");
        common::point(&mut c, "image", &format!("{base}/api/v1"), "img-secret");
        c.providers[0]
            .headers
            .insert("X-Title".into(), "Reseller".into());
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

    // With the image provider disabled, its model has no enabled provider: it
    // is hidden and refused like a disabled model, even though other
    // providers may be configured.
    t.app.config_mut().await.providers[0].enabled = false;
    let (s, v) = t
        .call(
            "POST",
            "/v1/images/generations",
            k["key"].as_str(),
            json!({"model":"meta/muse-image","prompt":"x"}),
        )
        .await;
    assert_eq!(s, 404, "{v}");
    assert_eq!(v["error"]["code"], "model_not_found");
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
        common::serve(&mut c, "image", Capability::Image, "meta/muse-image");
        common::model(
            &mut c,
            "artist",
            Capability::Image,
            Tier::Paid,
            "image",
            "artist",
        );
        common::point(&mut c, "image", &format!("{base}/v1"), "img-secret");
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
                "chains": ["eip155:1", "eip155:56"],
                // Standing deposit addresses are off by default.
                "deposits": true
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
    let provider = |key: &str, headers: Value| json!({"id": "up", "base_url": "https://up.example/v1", "api_key": key, "headers": headers});
    let (s, v) = t
        .admin(
            "PATCH",
            "settings",
            json!({"stripe": {"secret_key": "sk_live_x", "webhook_secret": "whsec_x"},
                   "providers": [provider("up-key", json!({"X-Title": "A"}))]}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["settings"]["stripe"]["secret_key"], "");
    assert_eq!(v["settings"]["providers"][0]["api_key"], "");
    assert_eq!(v["settings"]["providers"][0]["id"], "up");
    assert_eq!(t.app.config().await.providers[0].api_key, "up-key");
    let (s, v) = t
        .admin(
            "PATCH",
            "settings",
            json!({"stripe": {"secret_key": ""},
                   "providers": [provider("", json!({"X-Other": "B"}))]}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let c = t.app.config().await;
    assert_eq!(c.stripe.secret_key, "sk_live_x");
    assert_eq!(
        c.providers[0].api_key, "up-key",
        "blank keeps the stored key"
    );
    assert_eq!(
        c.providers[0].headers.len(),
        1,
        "header maps are replaced wholesale"
    );
    drop(c);
    // A new key replaces it, and a provider with a new id starts without one.
    let (s, _) = t
        .admin(
            "PATCH",
            "settings",
            json!({"providers": [provider("rotated", json!({})),
                   {"id": "other", "base_url": "https://o.example/v1", "api_key": ""}]}),
        )
        .await;
    assert_eq!(s, 200);
    let c = t.app.config().await;
    assert_eq!(c.providers[0].api_key, "rotated");
    assert_eq!(c.providers[1].api_key, "");
    drop(c);
    let exported = t.app.config().await.to_toml(false).unwrap();
    assert!(!exported.contains("rotated") && !exported.contains("sk_live_x"));
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
// Model catalog, OpenAI errors, quota, and deposits
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
    common::llm(&mut c, "configured");
    common::point(&mut c, "llm", &format!("{base}/v1"), "upstream-secret");
    seen
}

fn catalog() -> Vec<reseller::config::Model> {
    let route = |model: &str| reseller::config::Route {
        provider: "llm".into(),
        model: model.into(),
        ..Default::default()
    };
    let entry = |id: &str| reseller::config::Model {
        id: id.into(),
        capability: Some(Capability::Chat),
        ..Default::default()
    };
    vec![
        reseller::config::Model {
            aliases: vec!["gpt-4o".into()],
            name: "Assistant".into(),
            context_length: Some(128_000),
            routes: vec![route("vendor/secret-upstream-name")],
            ..entry("assistant")
        },
        reseller::config::Model {
            routes: vec![route("vendor/other")],
            ..entry("other")
        },
        reseller::config::Model {
            enabled: false,
            routes: vec![route("vendor/retired")],
            ..entry("retired")
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
    {
        let mut c = t.app.config_mut().await;
        c.models = catalog();
        if c.provider("llm").is_none() {
            common::provider(&mut c, "llm", common::UNREACHABLE, "upstream-secret");
        }
    }
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
async fn models_list_derives_capability_and_modalities_from_the_capability() {
    let t = Test::new().await;
    {
        let mut c = t.app.config_mut().await;
        for (id, capability) in [
            ("chat-up", Capability::Chat),
            ("voice-up", Capability::Speech),
            ("ears-up", Capability::Transcription),
            ("paint-up", Capability::Image),
        ] {
            common::model(&mut c, id, capability, Tier::Paid, "llm", id);
        }
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
    assert_eq!(arch("voice-up")["capability"], "speech");
    assert_eq!(arch("ears-up")["modality"], "audio->text");
    assert_eq!(arch("ears-up")["capability"], "transcription");
    assert_eq!(arch("paint-up")["modality"], "text->image");
    assert_eq!(arch("paint-up")["capability"], "image");
    // Explicit modalities win over derivation.
    {
        let mut c = t.app.config_mut().await;
        common::model(
            &mut c,
            "vision",
            Capability::Chat,
            Tier::Paid,
            "llm",
            "chat-up",
        )
        .input_modalities = vec!["text".into(), "image".into()];
    }
    let (_, v) = t
        .call("GET", "/v1/models/vision", Some(key), json!({}))
        .await;
    assert_eq!(v["architecture"]["modality"], "text+image->text");
    assert_eq!(v["architecture"]["capability"], "chat");
}

#[tokio::test]
async fn models_list_is_open_to_everyone_and_needs_no_balance() {
    let t = Test::new().await;
    {
        let mut c = t.app.config_mut().await;
        common::model(
            &mut c,
            "gratis",
            Capability::Chat,
            Tier::Free,
            "llm",
            "gratis",
        );
        common::model(
            &mut c,
            "premium",
            Capability::Chat,
            Tier::Paid,
            "llm",
            "premium",
        );
    }
    // No credential, and a placeholder token, see the free models only.
    for token in [None, Some("free"), Some("not-a-key")] {
        let (s, v) = t.call("GET", "/v1/models", token, json!({})).await;
        assert_eq!(s, 200, "{v}");
        let ids: Vec<&str> = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["gratis"], "{token:?}");
        assert_eq!(v["data"][0]["tier"], "free");
    }
    // A key that looks like ours but is not valid is refused.
    let (s, v) = t
        .call("GET", "/v1/models", Some("sk-res-wrong"), json!({}))
        .await;
    assert_eq!(s, 401);
    assert_eq!(error_keys(&v), ERROR_KEYS);
    assert_eq!(v["error"]["code"], "invalid_api_key");
    // A key lists free and paid models, even without a balance.
    let k = t.unfunded_key().await;
    let (s, v) = t
        .call("GET", "/v1/models", k["key"].as_str(), json!({}))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["data"].as_array().unwrap().len(), 2);
    assert_eq!(v["data"][1]["id"], "premium");
    assert_eq!(v["data"][1]["tier"], "paid");
    // With the free tier off, nothing is served without a key.
    t.app.config_mut().await.free.enabled = false;
    let (s, v) = t.call("GET", "/v1/models", None, json!({})).await;
    assert_eq!(s, 401);
    assert_eq!(error_keys(&v), ERROR_KEYS);
    assert_eq!(v["error"]["code"], "missing_api_key");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(
        v["error"]["message"],
        "Missing bearer authentication in header"
    );
    let (s, _) = t.call("GET", "/v1/models", Some("free"), json!({})).await;
    assert_eq!(s, 401);
    let (s, _) = t
        .call("GET", "/v1/models", k["key"].as_str(), json!({}))
        .await;
    assert_eq!(s, 200, "keys still work");
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
    assert_eq!(s, 402, "{v}");
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
    {
        let mut c = t.app.config_mut().await;
        c.models = catalog();
        if c.provider("llm").is_none() {
            common::provider(&mut c, "llm", common::UNREACHABLE, "upstream-secret");
        }
    }
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

/// Signup carries no credit: the account starts empty and the response has no
/// trial amount.
#[tokio::test]
async fn new_accounts_start_without_credit() {
    let t = Test::new().await;
    let k = t.unfunded_key().await;
    assert_eq!(t.balance(k["account_id"].as_str().unwrap()).await, "0");
    assert!(k.get("trial_credit_usd").is_none(), "{k}");
    let (_, plans) = t.call("GET", "/v1/plans", None, json!({})).await;
    assert!(plans.get("trial_credit_usd").is_none(), "{plans}");
    let credits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM credits")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(credits, 0);
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
        "INSERT INTO requests(id,account_id,key_id,created_at,method,path,kind,model,public_model,tier,\
         ip,status,upstream_micro,billed_micro,finished) VALUES('r1',?,?,?,'POST','/v1/chat/completions','llm',\
         'vendor/m','m','paid','127.0.0.1',200,1000,1300,1)",
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
            "INSERT INTO payments(id,account_id,plan_code,provider,kind,price_micro,credit_micro,months,created_at) \
             VALUES(?,?,'monthly','stripe','subscription',20000000,20000000,1,?)",
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

// ---------------------------------------------------------------------------
// App Store and Google Play purchases
// ---------------------------------------------------------------------------

use aws_lc_rs::{
    encoding::AsDer,
    rsa::{KeyPair as RsaPair, KeySize},
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair, RsaPublicKeyComponents},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use reseller::payments::store::{apple, jose};

const BUNDLE: &str = "com.example.app";
const PACKAGE: &str = "com.example.app";
const PUSH_EMAIL: &str = "push@project.iam.gserviceaccount.com";
const PUSH_AUDIENCE: &str = "https://reseller.example/pubsub";

/// A request the store mock received.
#[derive(Clone, Debug)]
struct Call {
    method: String,
    path: String,
    auth: Option<String>,
    body: Value,
}

/// Canned replies by `"METHOD /path"` and the calls received.
#[derive(Default)]
struct StoreMock {
    replies: std::collections::HashMap<String, Value>,
    calls: Vec<Call>,
}

type Mock = Arc<Mutex<StoreMock>>;

fn reply(mock: &Mock, key: &str, body: Value) {
    mock.lock().unwrap().replies.insert(key.to_owned(), body);
}

fn calls(mock: &Mock, method: &str, path_suffix: &str) -> Vec<Call> {
    mock.lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c.method == method && c.path.ends_with(path_suffix))
        .cloned()
        .collect()
}

/// One server for the App Store Server API, the Play Developer API, the
/// Google token endpoint, and the JWKS.
async fn mock_store() -> (String, Mock) {
    let mock = Mock::default();
    let state = mock.clone();
    let router = Router::new().fallback(
        move |method: axum::http::Method,
              uri: axum::http::Uri,
              headers: HeaderMap,
              body: String| {
            let state = state.clone();
            async move {
                let mut s = state.lock().unwrap();
                let path = uri.path().to_owned();
                s.calls.push(Call {
                    method: method.to_string(),
                    path: path.clone(),
                    auth: headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                    body: serde_json::from_str(&body).unwrap_or(Value::String(body)),
                });
                match s.replies.get(&format!("{method} {path}")) {
                    Some(v) => (axum::http::StatusCode::OK, Json(v.clone())),
                    None => (axum::http::StatusCode::NOT_FOUND, Json(json!({}))),
                }
            }
        },
    );
    (serve(router).await, mock)
}

fn pem(label: &str, der: &[u8]) -> String {
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        STANDARD.encode(der)
    )
}

fn rfc3339(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0).unwrap().to_rfc3339()
}

fn bal(balance: &str) -> f64 {
    balance.parse().unwrap()
}

/// An Apple-shaped certificate chain and the leaf's PKCS#8 key.
struct AppleChain {
    der: Vec<Vec<u8>>,
    key: Vec<u8>,
}

fn apple_chain(markers: bool) -> AppleChain {
    use rcgen::{
        BasicConstraints, CertificateParams, CustomExtension, DnType, IsCa, Issuer, KeyPair,
    };
    let marker = |oid: &[u64]| CustomExtension::from_oid_content(oid, vec![0x05, 0x00]);
    let ca = |name: &str| {
        let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.distinguished_name.push(DnType::CommonName, name);
        p
    };
    let root_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let root_params = ca("Test Root");
    let root = root_params.self_signed(&root_key).unwrap();
    let root_issuer = Issuer::new(root_params, root_key);
    let mid_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let mut mid_params = ca("Test Intermediate");
    if markers {
        mid_params
            .custom_extensions
            .push(marker(&[1, 2, 840, 113635, 100, 6, 2, 1]));
    }
    let mid = mid_params.signed_by(&mid_key, &root_issuer).unwrap();
    let mid_issuer = Issuer::new(mid_params, mid_key);
    let leaf_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut leaf_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, "Test Leaf");
    if markers {
        leaf_params
            .custom_extensions
            .push(marker(&[1, 2, 840, 113635, 100, 6, 11, 1]));
    }
    let leaf = leaf_params.signed_by(&leaf_key, &mid_issuer).unwrap();
    AppleChain {
        der: vec![leaf.der().to_vec(), mid.der().to_vec(), root.der().to_vec()],
        key: leaf_key.serialize_der(),
    }
}

/// Signs `payload` like StoreKit 2 does, with the chain in `x5c`.
fn apple_jws(chain: &AppleChain, payload: &Value) -> String {
    let x5c: Vec<String> = chain.der.iter().map(|c| STANDARD.encode(c)).collect();
    jose::sign_es256(&chain.key, &json!({"alg": "ES256", "x5c": x5c}), payload).unwrap()
}

fn apple_tx(
    token: &str,
    id: &str,
    original: &str,
    product: &str,
    kind: &str,
    purchased: i64,
    expires: Option<i64>,
) -> Value {
    json!({
        "transactionId": id, "originalTransactionId": original, "bundleId": BUNDLE,
        "productId": product, "type": kind, "purchaseDate": purchased * 1000,
        "expiresDate": expires.map(|e| e * 1000), "signedDate": db::now() * 1000,
        "quantity": 1, "appAccountToken": token, "environment": "Sandbox",
    })
}

struct AppleEnv {
    t: Test,
    mock: Mock,
    chain: AppleChain,
    /// Public key of the App Store Server API key.
    api_public: Vec<u8>,
}

/// Sets up plans with product ids and an App Store configuration pointing at
/// the mock, trusting only a freshly generated root.
async fn apple_env() -> AppleEnv {
    let mut t = Test::new().await;
    let chain = apple_chain(true);
    t.app = t.app.clone().with_apple_roots(vec![chain.der[2].clone()]);
    store_plans(&t).await;
    let (base, mock) = mock_store().await;
    let api_key = EcdsaKeyPair::generate_pkcs8(
        &ECDSA_P256_SHA256_FIXED_SIGNING,
        &aws_lc_rs::rand::SystemRandom::new(),
    )
    .unwrap();
    let api_public = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, api_key.as_ref())
        .unwrap()
        .public_key()
        .as_ref()
        .to_vec();
    {
        let mut c = t.app.config_mut().await;
        c.apple.bundle_id = BUNDLE.into();
        c.apple.app_apple_id = Some(123);
        c.apple.environment = reseller::config::AppleEnvironment::Sandbox;
        c.apple.issuer_id = "issuer-1".into();
        c.apple.key_id = "KEY123".into();
        c.apple.private_key = pem("PRIVATE KEY", api_key.as_ref());
        c.apple.api_url = base;
        c.apple.consumption_reporting = true;
    }
    AppleEnv {
        t,
        mock,
        chain,
        api_public,
    }
}

/// Gives the `monthly` plan and a new `topup10` plan store product ids.
async fn store_plans(t: &Test) {
    sqlx::query("UPDATE plans SET apple_product_id='pro.monthly',google_product_id='pro.monthly' WHERE code='monthly'")
        .execute(&t.app.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO plans(code,name,kind,price_micro,credit_micro,apple_product_id,google_product_id) \
         VALUES('topup10','Top up $10','topup',10000000,10000000,'credits.10','credits.10')",
    )
    .execute(&t.app.db)
    .await
    .unwrap();
}

/// A new account's key, id, and store token.
async fn store_account(t: &Test) -> (String, String, String) {
    let k = t.key().await;
    let key = k["key"].as_str().unwrap().to_owned();
    let (s, v) = t
        .call("GET", "/v1/account/store", Some(&key), json!({}))
        .await;
    assert_eq!(s, 200, "{v}");
    (
        key,
        k["account_id"].as_str().unwrap().to_owned(),
        v["app_account_token"].as_str().unwrap().to_owned(),
    )
}

async fn apple_post(e: &AppleEnv, key: &str, tx: &Value) -> (u16, Value) {
    e.t.call(
        "POST",
        "/v1/account/store/apple",
        Some(key),
        json!({"signed_transaction": apple_jws(&e.chain, tx)}),
    )
    .await
}

/// Delivers a signed App Store Server Notification V2.
async fn apple_notify(
    e: &AppleEnv,
    kind: &str,
    subtype: &str,
    uuid: &str,
    tx: &Value,
) -> (u16, Value) {
    apple_notify_for(e, kind, subtype, uuid, tx, BUNDLE, "Sandbox").await
}

async fn apple_notify_for(
    e: &AppleEnv,
    kind: &str,
    subtype: &str,
    uuid: &str,
    tx: &Value,
    bundle: &str,
    environment: &str,
) -> (u16, Value) {
    let payload = json!({
        "notificationType": kind, "subtype": subtype, "notificationUUID": uuid,
        "signedDate": db::now() * 1000,
        "data": {"appAppleId": 123, "bundleId": bundle, "environment": environment,
                 "signedTransactionInfo": apple_jws(&e.chain, tx)},
    });
    let body = json!({"signedPayload": apple_jws(&e.chain, &payload)});
    e.t.call("POST", "/webhooks/apple", None, body).await
}

/// JWS verification trusts only the configured root and rejects tampering.
#[tokio::test]
async fn apple_jws_verification_accepts_the_test_chain_and_rejects_forgeries() {
    let e = apple_env().await;
    let payload = json!({"signedDate": db::now() * 1000, "transactionId": "1"});
    let jws = apple_jws(&e.chain, &payload);
    assert_eq!(
        apple::verify_signed(&e.t.app, &jws).unwrap()["transactionId"],
        "1"
    );

    // Tampered payload.
    let parts: Vec<&str> = jws.split('.').collect();
    let forged_payload = json!({"signedDate": db::now() * 1000, "transactionId": "2"});
    let forged = format!(
        "{}.{}.{}",
        parts[0],
        jose::b64url(forged_payload.to_string().as_bytes()),
        parts[2]
    );
    assert!(apple::verify_signed(&e.t.app, &forged).is_err());

    // A chain that does not end in a trusted root: production trusts only
    // Apple's pinned root.
    let production = App::new(e.t.app.db.clone(), (*e.t.app.config().await).clone()).unwrap();
    assert!(apple::verify_signed(&production, &jws).is_err());
    let other = apple_chain(true);
    let foreign = apple_jws(&other, &payload);
    assert!(apple::verify_signed(&e.t.app, &foreign).is_err());

    // A chain without Apple's marker extensions.
    let unmarked = apple_chain(false);
    let app =
        e.t.app
            .clone()
            .with_apple_roots(vec![unmarked.der[2].clone()]);
    assert!(apple::verify_signed(&app, &apple_jws(&unmarked, &payload)).is_err());

    // Wrong algorithm and a missing chain.
    let hs = jose::b64url(br#"{"alg":"HS256"}"#);
    let unsigned = format!("{hs}.{}.{}", parts[1], parts[2]);
    assert!(apple::verify_signed(&e.t.app, &unsigned).is_err());
    assert!(apple::verify_signed(&e.t.app, "garbage").is_err());
}

/// A verified transaction credits once, is bound to its account, and is
/// refused when revoked, of the wrong type, or for another app.
#[tokio::test]
async fn apple_verify_credits_once_and_binds_the_account() {
    let e = apple_env().await;
    let (key, aid, token) = store_account(&e.t).await;
    let (other_key, _, other_token) = store_account(&e.t).await;
    assert_ne!(token, other_token);
    let now = db::now();
    let before = bal(&e.t.balance(&aid).await);

    let tx = apple_tx(
        &token,
        "3001",
        "3001",
        "credits.10",
        "Consumable",
        now,
        None,
    );
    let (s, v) = apple_post(&e, &key, &tx).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["purchase"]["credited"], true);
    let order = v["purchase"]["order_id"].clone();
    assert_eq!(
        bal(v["account"]["balance_usd"].as_str().unwrap()) - before,
        10.0
    );

    let (s, v) = apple_post(&e, &key, &tx).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (
            v["purchase"]["credited"].clone(),
            v["purchase"]["order_id"].clone()
        ),
        (json!(false), order)
    );
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 10.0);

    // Another account cannot claim the same transaction, and a missing or
    // foreign appAccountToken is refused.
    let (s, v) = apple_post(&e, &other_key, &tx).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (403, Some("store_account_mismatch"))
    );
    let mut foreign = apple_tx(
        &other_token,
        "3002",
        "3002",
        "credits.10",
        "Consumable",
        now,
        None,
    );
    let (s, _) = apple_post(&e, &key, &foreign).await;
    assert_eq!(s, 403);
    foreign["appAccountToken"] = Value::Null;
    assert_eq!(apple_post(&e, &key, &foreign).await.0, 403);

    // Quantity scales a top-up.
    let mut two = apple_tx(
        &token,
        "3003",
        "3003",
        "credits.10",
        "Consumable",
        now,
        None,
    );
    two["quantity"] = 2.into();
    let (s, v) = apple_post(&e, &key, &two).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 30.0);

    // Revoked, wrong type, unknown product, another app, expired.
    let mut revoked = apple_tx(
        &token,
        "3004",
        "3004",
        "credits.10",
        "Consumable",
        now,
        None,
    );
    revoked["revocationDate"] = (now * 1000).into();
    let (s, v) = apple_post(&e, &key, &revoked).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("purchase_revoked"))
    );
    let wrong = apple_tx(
        &token,
        "3005",
        "3005",
        "credits.10",
        "Auto-Renewable Subscription",
        now,
        Some(now + 3600),
    );
    assert_eq!(apple_post(&e, &key, &wrong).await.0, 400);
    let unknown = apple_tx(&token, "3006", "3006", "nope", "Consumable", now, None);
    assert_eq!(apple_post(&e, &key, &unknown).await.0, 400);
    let mut elsewhere = apple_tx(
        &token,
        "3007",
        "3007",
        "credits.10",
        "Consumable",
        now,
        None,
    );
    elsewhere["bundleId"] = "com.other".into();
    assert_eq!(apple_post(&e, &key, &elsewhere).await.0, 400);
    let expired = apple_tx(
        &token,
        "3008",
        "3008",
        "pro.monthly",
        "Auto-Renewable Subscription",
        now - 7200,
        Some(now - 3600),
    );
    let (s, v) = apple_post(&e, &key, &expired).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("subscription_expired"))
    );
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 30.0);

    // Garbage and a missing payload.
    let (s, _) =
        e.t.call(
            "POST",
            "/v1/account/store/apple",
            Some(&key),
            json!({"signed_transaction": "x.y.z"}),
        )
        .await;
    assert_eq!(s, 400);
    let (s, _) =
        e.t.call("POST", "/v1/account/store/apple", Some(&key), json!({}))
            .await;
    assert_eq!(s, 400);
}

/// A transaction id is looked up with the Server API (signed with the .p8
/// key). Without the key the lookup is unavailable.
#[tokio::test]
async fn apple_verify_by_transaction_id_calls_the_server_api() {
    let e = apple_env().await;
    let (key, aid, token) = store_account(&e.t).await;
    let before = bal(&e.t.balance(&aid).await);
    let tx = apple_tx(
        &token,
        "4001",
        "4001",
        "credits.10",
        "Consumable",
        db::now(),
        None,
    );
    reply(
        &e.mock,
        "GET /inApps/v1/transactions/4001",
        json!({"signedTransactionInfo": apple_jws(&e.chain, &tx)}),
    );
    let (s, v) =
        e.t.call(
            "POST",
            "/v1/account/store/apple",
            Some(&key),
            json!({"transaction_id": "4001"}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 10.0);

    let call = calls(&e.mock, "GET", "/inApps/v1/transactions/4001").remove(0);
    let bearer = call.auth.unwrap();
    let jws = jose::split(bearer.strip_prefix("Bearer ").unwrap()).unwrap();
    assert_eq!(jws.header["alg"], "ES256");
    assert_eq!(jws.header["kid"], "KEY123");
    assert_eq!(jws.payload["iss"], "issuer-1");
    assert_eq!(jws.payload["aud"], "appstoreconnect-v1");
    assert_eq!(jws.payload["bid"], BUNDLE);
    let (iat, exp) = (
        jws.payload["iat"].as_i64().unwrap(),
        jws.payload["exp"].as_i64().unwrap(),
    );
    assert!(exp > iat && exp - iat <= 3600);
    jose::verify_es256(&e.api_public, &jws).unwrap();

    let (s, _) =
        e.t.call(
            "POST",
            "/v1/account/store/apple",
            Some(&key),
            json!({"transaction_id": "../x"}),
        )
        .await;
    assert_eq!(s, 400);
    e.t.app.config_mut().await.apple.private_key.clear();
    let (s, v) =
        e.t.call(
            "POST",
            "/v1/account/store/apple",
            Some(&key),
            json!({"transaction_id": "4002"}),
        )
        .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (503, Some("payments_unavailable"))
    );
}

/// The store endpoint hands out a stable token and the sellable plans, and
/// consent can be set and withdrawn.
#[tokio::test]
async fn account_store_info_and_consumption_consent() {
    let e = apple_env().await;
    let (key, aid, token) = store_account(&e.t).await;
    let (s, v) =
        e.t.call("GET", "/v1/account/store", Some(&key), json!({}))
            .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["app_account_token"], token.as_str());
    assert_eq!(v["obfuscated_account_id"], token.as_str());
    assert_eq!(uuid::Uuid::parse_str(&token).unwrap().get_version_num(), 4);
    assert_eq!(
        v["apple"],
        json!({"enabled": true, "bundle_id": BUNDLE, "environment": "Sandbox"})
    );
    assert_eq!(v["google"]["enabled"], false);
    let codes: Vec<&str> = v["plans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["monthly", "topup10"]);
    assert_eq!(v["plans"][1]["apple_product_id"], "credits.10");

    let (s, v) =
        e.t.call(
            "PATCH",
            "/v1/account",
            Some(&key),
            json!({"consumption_consent": true}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let (consent, at): (bool, Option<i64>) =
        sqlx::query_as("SELECT consumption_consent,consent_at FROM accounts WHERE id=?")
            .bind(&aid)
            .fetch_one(&e.t.app.db)
            .await
            .unwrap();
    assert!(consent && at.is_some());
    let (s, _) =
        e.t.call(
            "PATCH",
            "/v1/account",
            Some(&key),
            json!({"consumption_consent": false}),
        )
        .await;
    assert_eq!(s, 200);
    let (consent, at): (bool, Option<i64>) =
        sqlx::query_as("SELECT consumption_consent,consent_at FROM accounts WHERE id=?")
            .bind(&aid)
            .fetch_one(&e.t.app.db)
            .await
            .unwrap();
    assert!(!consent && at.is_none());
    let (s, _) =
        e.t.call(
            "PATCH",
            "/v1/account",
            Some(&key),
            json!({"consumption_consent": "yes"}),
        )
        .await;
    assert_eq!(s, 400);
}

/// A renewal notification grants the new month's quota exactly once, even
/// when delivered twice or under a second notification id.
#[tokio::test]
async fn apple_renewal_extends_and_grants_the_quota_once() {
    let e = apple_env().await;
    let (key, aid, token) = store_account(&e.t).await;
    let now = db::now();
    let before = bal(&e.t.balance(&aid).await);
    let first = apple_tx(
        &token,
        "5001",
        "5001",
        "pro.monthly",
        "Auto-Renewable Subscription",
        now - 3600,
        Some(now + 1800),
    );
    let (s, v) = apple_post(&e, &key, &first).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 20.0);

    let renewal = apple_tx(
        &token,
        "5002",
        "5001",
        "pro.monthly",
        "Auto-Renewable Subscription",
        now,
        Some(now + 30 * 86400),
    );
    let (s, v) = apple_notify(&e, "DID_RENEW", "", "uuid-renew-1", &renewal).await;
    assert_eq!((s, v.clone()), (200, json!({"received": true})));
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 40.0);
    let (s, v) = apple_notify(&e, "DID_RENEW", "", "uuid-renew-1", &renewal).await;
    assert_eq!((s, v), (200, json!({"received": true, "duplicate": true})));
    let (s, _) = apple_notify(&e, "DID_RENEW", "", "uuid-renew-2", &renewal).await;
    assert_eq!(s, 200);
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 40.0);

    let (subs, expires): (i64, i64) =
        sqlx::query_as("SELECT COUNT(*),MAX(expires_at) FROM subscriptions WHERE account_id=?")
            .bind(&aid)
            .fetch_one(&e.t.app.db)
            .await
            .unwrap();
    assert_eq!((subs, expires), (1, now + 30 * 86400));
    let payments: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM payments WHERE account_id=? AND provider='apple' AND status='paid'",
    )
    .bind(&aid)
    .fetch_one(&e.t.app.db)
    .await
    .unwrap();
    assert_eq!(payments, 2);

    // Auto-renew follows the status notifications.
    let auto = || async {
        sqlx::query_scalar::<_, bool>("SELECT auto_renew FROM subscriptions WHERE account_id=?")
            .bind(&aid)
            .fetch_one(&e.t.app.db)
            .await
            .unwrap()
    };
    assert!(auto().await);
    apple_notify(
        &e,
        "DID_CHANGE_RENEWAL_STATUS",
        "AUTO_RENEW_DISABLED",
        "uuid-off",
        &renewal,
    )
    .await;
    assert!(!auto().await);
    apple_notify(
        &e,
        "DID_CHANGE_RENEWAL_STATUS",
        "AUTO_RENEW_ENABLED",
        "uuid-on",
        &renewal,
    )
    .await;
    assert!(auto().await);

    // Another app or environment is acknowledged and ignored.
    let next = apple_tx(
        &token,
        "5003",
        "5001",
        "pro.monthly",
        "Auto-Renewable Subscription",
        now,
        Some(now + 60 * 86400),
    );
    for (bundle, environment) in [("com.other", "Sandbox"), (BUNDLE, "Production")] {
        let (s, v) =
            apple_notify_for(&e, "DID_RENEW", "", "uuid-x", &next, bundle, environment).await;
        assert_eq!((s, v), (200, json!({"received": true})));
    }
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 40.0);

    // An unsigned or forged notification is refused.
    let (s, _) =
        e.t.call(
            "POST",
            "/webhooks/apple",
            None,
            json!({"signedPayload": "a.b.c"}),
        )
        .await;
    assert_eq!(s, 400);
}

/// A refund zeroes the balance, suspends the account, blocks its keys, and
/// records the identifiers that may not return.
#[tokio::test]
async fn apple_refund_zeroes_suspends_and_blocks() {
    let e = apple_env().await;
    let (key, aid, token) = store_account(&e.t).await;
    let now = db::now();
    let tx = apple_tx(
        &token,
        "6001",
        "6000",
        "pro.monthly",
        "Auto-Renewable Subscription",
        now,
        Some(now + 30 * 86400),
    );
    assert_eq!(apple_post(&e, &key, &tx).await.0, 200);
    let topup = apple_tx(
        &token,
        "6002",
        "6002",
        "credits.10",
        "Consumable",
        now,
        None,
    );
    assert_eq!(apple_post(&e, &key, &topup).await.0, 200);
    assert!(bal(&e.t.balance(&aid).await) >= 30.0);

    let (s, v) = apple_notify(&e, "REFUND", "", "uuid-refund", &tx).await;
    assert_eq!((s, v), (200, json!({"received": true})));
    assert_eq!(bal(&e.t.balance(&aid).await), 0.0);
    let (status, reason): (String, String) =
        sqlx::query_as("SELECT status,suspended_reason FROM accounts WHERE id=?")
            .bind(&aid)
            .fetch_one(&e.t.app.db)
            .await
            .unwrap();
    assert_eq!((status.as_str(), reason.as_str()), ("suspended", "refund"));
    let active: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE account_id=? AND status='active'")
            .bind(&aid)
            .fetch_one(&e.t.app.db)
            .await
            .unwrap();
    assert_eq!(active, 0);
    let blocks: Vec<(String, String)> =
        sqlx::query_as("SELECT kind,value FROM refund_blocks WHERE account_id=? ORDER BY kind")
            .bind(&aid)
            .fetch_all(&e.t.app.db)
            .await
            .unwrap();
    assert_eq!(
        blocks,
        [
            ("apple_original_transaction".to_owned(), "6000".to_owned()),
            ("ip".to_owned(), "127.0.0.1".to_owned())
        ]
    );
    let refunded: Vec<String> = sqlx::query_scalar(
        "SELECT external_id FROM payments WHERE account_id=? AND status='refunded'",
    )
    .bind(&aid)
    .fetch_all(&e.t.app.db)
    .await
    .unwrap();
    assert_eq!(refunded, ["6001"]);
    // The key no longer works.
    let (s, _) = e.t.call("GET", "/v1/models", Some(&key), json!({})).await;
    assert_ne!(s, 200);

    // Same refund again: nothing more happens.
    let (s, v) = apple_notify(&e, "REFUND", "", "uuid-refund", &tx).await;
    assert_eq!((s, v["duplicate"].clone()), (200, json!(true)));

    // Reversal is only logged: the account stays suspended.
    let (s, _) = apple_notify(&e, "REFUND_REVERSED", "", "uuid-reversed", &tx).await;
    assert_eq!(s, 200);
    let status: String = sqlx::query_scalar("SELECT status FROM accounts WHERE id=?")
        .bind(&aid)
        .fetch_one(&e.t.app.db)
        .await
        .unwrap();
    assert_eq!(status, "suspended");
}

/// REVOKE (Family Sharing) ends the subscription and its quota but does not
/// block the account.
#[tokio::test]
async fn apple_revoke_ends_the_subscription_without_blocking() {
    let e = apple_env().await;
    let (key, aid, token) = store_account(&e.t).await;
    let now = db::now();
    let before = bal(&e.t.balance(&aid).await);
    let tx = apple_tx(
        &token,
        "7001",
        "7001",
        "pro.monthly",
        "Auto-Renewable Subscription",
        now,
        Some(now + 30 * 86400),
    );
    assert_eq!(apple_post(&e, &key, &tx).await.0, 200);
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 20.0);
    let (s, _) = apple_notify(&e, "REVOKE", "", "uuid-revoke", &tx).await;
    assert_eq!(s, 200);
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 0.0);
    let sub: String = sqlx::query_scalar("SELECT status FROM subscriptions WHERE account_id=?")
        .bind(&aid)
        .fetch_one(&e.t.app.db)
        .await
        .unwrap();
    assert_eq!(sub, "refunded");
    let status: String = sqlx::query_scalar("SELECT status FROM accounts WHERE id=?")
        .bind(&aid)
        .fetch_one(&e.t.app.db)
        .await
        .unwrap();
    assert_eq!(status, "active");
    let blocks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM refund_blocks")
        .fetch_one(&e.t.app.db)
        .await
        .unwrap();
    assert_eq!(blocks, 0);
}

/// Consumption data goes to Apple only for customers who consented.
#[tokio::test]
async fn apple_consumption_request_needs_consent_and_sends_the_v2_body() {
    let e = apple_env().await;
    // The service offers a free tier, which Apple is told about as sample content.
    common::model(
        &mut *e.t.app.config_mut().await,
        "gratis",
        Capability::Chat,
        Tier::Free,
        "llm",
        "gratis",
    );
    let (key, aid, token) = store_account(&e.t).await;
    let now = db::now();
    let tx = apple_tx(
        &token,
        "8001",
        "8001",
        "credits.10",
        "Consumable",
        now,
        None,
    );
    assert_eq!(apple_post(&e, &key, &tx).await.0, 200);
    sqlx::query("UPDATE credits SET remaining_micro=amount_micro*7/10 WHERE payment_id IN (SELECT id FROM payments WHERE external_id='8001')")
        .execute(&e.t.app.db)
        .await
        .unwrap();
    let path = "/inApps/v2/transactions/consumption/8001";
    reply(&e.mock, &format!("PUT {path}"), json!({}));

    // No consent, nothing is sent.
    let (s, v) = apple_notify(&e, "CONSUMPTION_REQUEST", "", "uuid-c1", &tx).await;
    assert_eq!((s, v), (200, json!({"received": true})));
    assert!(calls(&e.mock, "PUT", path).is_empty());

    // Consent: the exact v2 body, signed with the API key.
    let (s, _) =
        e.t.call(
            "PATCH",
            "/v1/account",
            Some(&key),
            json!({"consumption_consent": true}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, v) = apple_notify(&e, "CONSUMPTION_REQUEST", "", "uuid-c2", &tx).await;
    assert_eq!((s, v), (200, json!({"received": true})));
    let sent = calls(&e.mock, "PUT", path);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].body,
        json!({"customerConsented": true, "consumptionPercentage": 30000, "deliveryStatus": "DELIVERED",
               "refundPreference": "GRANT_PRORATED", "sampleContentProvided": true})
    );
    let jws = jose::split(
        sent[0]
            .auth
            .as_deref()
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap(),
    )
    .unwrap();
    jose::verify_es256(&e.api_public, &jws).unwrap();
    // The same notification again is a duplicate and sends nothing more.
    let (_, v) = apple_notify(&e, "CONSUMPTION_REQUEST", "", "uuid-c2", &tx).await;
    assert_eq!(v["duplicate"], true);
    assert_eq!(calls(&e.mock, "PUT", path).len(), 1);

    // Mostly consumed: decline. Unused: grant in full.
    sqlx::query("UPDATE credits SET remaining_micro=amount_micro/4 WHERE account_id=? AND payment_id IS NOT NULL").bind(&aid).execute(&e.t.app.db).await.unwrap();
    apple_notify(&e, "CONSUMPTION_REQUEST", "", "uuid-c3", &tx).await;
    sqlx::query("UPDATE credits SET remaining_micro=amount_micro WHERE account_id=? AND payment_id IS NOT NULL").bind(&aid).execute(&e.t.app.db).await.unwrap();
    apple_notify(&e, "CONSUMPTION_REQUEST", "", "uuid-c4", &tx).await;
    let sent = calls(&e.mock, "PUT", path);
    assert_eq!(sent[1].body["refundPreference"], "DECLINE");
    assert_eq!(sent[1].body["consumptionPercentage"], 75000);
    assert_eq!(sent[2].body["refundPreference"], "GRANT_FULL");
    assert_eq!(sent[2].body["consumptionPercentage"], 0);

    // Withdrawn consent and disabled reporting send nothing.
    e.t.call(
        "PATCH",
        "/v1/account",
        Some(&key),
        json!({"consumption_consent": false}),
    )
    .await;
    apple_notify(&e, "CONSUMPTION_REQUEST", "", "uuid-c5", &tx).await;
    e.t.call(
        "PATCH",
        "/v1/account",
        Some(&key),
        json!({"consumption_consent": true}),
    )
    .await;
    e.t.app.config_mut().await.apple.consumption_reporting = false;
    apple_notify(&e, "CONSUMPTION_REQUEST", "", "uuid-c6", &tx).await;
    assert_eq!(calls(&e.mock, "PUT", path).len(), 3);
}

struct GoogleEnv {
    t: Test,
    mock: Mock,
    /// PKCS#8 key of the Pub/Sub token signer.
    push_key: Vec<u8>,
    /// Public key of the service account.
    account_public: RsaPublicKeyComponents<Vec<u8>>,
}

/// Sets up plans and a Google Play configuration pointing at the mock.
async fn google_env() -> GoogleEnv {
    let t = Test::new().await;
    store_plans(&t).await;
    let (base, mock) = mock_store().await;
    let push = RsaPair::generate(KeySize::Rsa2048).unwrap();
    let account = RsaPair::generate(KeySize::Rsa2048).unwrap();
    reply(&mock, "GET /certs", {
        let c = RsaPublicKeyComponents::<Vec<u8>>::from(push.public_key());
        json!({"keys": [{"kty": "RSA", "kid": "k1", "alg": "RS256", "use": "sig",
                         "n": URL_SAFE_NO_PAD.encode(&c.n), "e": URL_SAFE_NO_PAD.encode(&c.e)}]})
    });
    reply(
        &mock,
        "POST /token",
        json!({"access_token": "ya29.test", "expires_in": 3600, "token_type": "Bearer"}),
    );
    let account_key = json!({
        "type": "service_account", "client_email": "play@project.iam.gserviceaccount.com",
        "private_key": pem("PRIVATE KEY", account.as_der().unwrap().as_ref()),
        "token_uri": format!("{base}/token"),
    });
    {
        let mut c = t.app.config_mut().await;
        c.google.package_name = PACKAGE.into();
        c.google.service_account_json = account_key.to_string();
        c.google.pubsub_audience = PUSH_AUDIENCE.into();
        c.google.pubsub_service_account = PUSH_EMAIL.into();
        c.google.refund_review = true;
        c.google.api_url = base.clone();
        c.google.certs_url = format!("{base}/certs");
    }
    GoogleEnv {
        t,
        mock,
        push_key: push.as_der().unwrap().as_ref().to_vec(),
        account_public: RsaPublicKeyComponents::<Vec<u8>>::from(account.public_key()),
    }
}

fn push_token(e: &GoogleEnv, edit: impl FnOnce(&mut Value)) -> String {
    let now = db::now();
    let mut claims = json!({
        "iss": "https://accounts.google.com", "aud": PUSH_AUDIENCE, "email": PUSH_EMAIL,
        "email_verified": true, "iat": now, "exp": now + 3600, "sub": "1234",
    });
    edit(&mut claims);
    jose::sign_rs256(
        &e.push_key,
        &json!({"alg": "RS256", "kid": "k1", "typ": "JWT"}),
        &claims,
    )
    .unwrap()
}

/// Delivers a Pub/Sub push of a developer notification with a valid token.
async fn google_push(e: &GoogleEnv, message_id: &str, note: Value) -> (u16, Value) {
    let token = push_token(e, |_| {});
    google_push_with(e, Some(&token), message_id, note).await
}

async fn google_push_with(
    e: &GoogleEnv,
    token: Option<&str>,
    message_id: &str,
    note: Value,
) -> (u16, Value) {
    let body = json!({
        "message": {"data": STANDARD.encode(note.to_string()), "messageId": message_id},
        "subscription": "projects/p/subscriptions/s",
    });
    e.t.call("POST", "/webhooks/google", token, body).await
}

fn play_sub(state: &str, order: &str, account: &str, expiry: i64, ack: &str) -> Value {
    json!({
        "subscriptionState": state, "acknowledgementState": ack, "latestSuccessfulOrderId": order,
        "startTime": rfc3339(db::now()),
        "lineItems": [{"productId": "pro.monthly", "expiryTime": rfc3339(expiry),
                       "autoRenewingPlan": {"autoRenewEnabled": true}}],
        "externalAccountIdentifiers": {"obfuscatedExternalAccountId": account},
    })
}

const SUB_PATH: &str =
    "/androidpublisher/v3/applications/com.example.app/purchases/subscriptionsv2/tokens/";
const PRODUCT_PATH: &str =
    "/androidpublisher/v3/applications/com.example.app/purchases/products/credits.10/tokens/";
const ACTIVE: &str = "SUBSCRIPTION_STATE_ACTIVE";
const PENDING_ACK: &str = "ACKNOWLEDGEMENT_STATE_PENDING";
const ACKED: &str = "ACKNOWLEDGEMENT_STATE_ACKNOWLEDGED";

fn sub_note(kind: i64, token: &str) -> Value {
    json!({"version": "1.0", "packageName": PACKAGE, "eventTimeMillis": "1",
           "subscriptionNotification": {"version": "1.0", "notificationType": kind,
                                         "purchaseToken": token, "subscriptionId": "pro.monthly"}})
}

/// Pub/Sub pushes need Google's OIDC token for the configured audience and
/// service account.
#[tokio::test]
async fn google_push_requires_a_valid_oidc_token() {
    let e = google_env().await;
    let note =
        json!({"version": "1.0", "packageName": PACKAGE, "testNotification": {"version": "1.0"}});
    let (s, v) = google_push_with(&e, None, "m0", note.clone()).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (401, Some("invalid_push_token"))
    );
    for bad in ["garbage", "a.b.c"] {
        assert_eq!(
            google_push_with(&e, Some(bad), "m0", note.clone()).await.0,
            401
        );
    }
    let now = db::now();
    type Edit = Box<dyn Fn(&mut Value)>;
    let variants: Vec<Edit> = vec![
        Box::new(|c| c["aud"] = "https://evil.example".into()),
        Box::new(|c| c["email"] = "other@project.iam.gserviceaccount.com".into()),
        Box::new(|c| c["email_verified"] = false.into()),
        Box::new(|c| c["iss"] = "https://evil.example".into()),
        Box::new(move |c| c["exp"] = (now - 600).into()),
        Box::new(move |c| c["iat"] = (now + 600).into()),
    ];
    for edit in variants {
        let token = push_token(&e, edit);
        assert_eq!(
            google_push_with(&e, Some(&token), "m0", note.clone())
                .await
                .0,
            401
        );
    }
    // A token from a key Google does not publish.
    let rogue = RsaPair::generate(KeySize::Rsa2048).unwrap();
    let claims = json!({"iss": "accounts.google.com", "aud": PUSH_AUDIENCE, "email": PUSH_EMAIL,
                        "email_verified": true, "iat": now, "exp": now + 3600});
    let forged = jose::sign_rs256(
        rogue.as_der().unwrap().as_ref(),
        &json!({"alg": "RS256", "kid": "k1"}),
        &claims,
    )
    .unwrap();
    assert_eq!(
        google_push_with(&e, Some(&forged), "m0", note.clone())
            .await
            .0,
        401
    );
    let unknown = jose::sign_rs256(
        rogue.as_der().unwrap().as_ref(),
        &json!({"alg": "RS256", "kid": "zz"}),
        &claims,
    )
    .unwrap();
    assert_eq!(
        google_push_with(&e, Some(&unknown), "m0", note.clone())
            .await
            .0,
        401
    );

    // The right token works, with either spelling of the issuer.
    assert_eq!(
        google_push(&e, "m1", note.clone()).await,
        (200, json!({"received": true}))
    );
    let token = push_token(&e, |c| c["iss"] = "accounts.google.com".into());
    assert_eq!(google_push_with(&e, Some(&token), "m2", note).await.0, 200);
    assert_eq!(calls(&e.mock, "GET", "/certs").len(), 1, "keys are cached");
}

/// A Play subscription credits once, is bound to its account, and is
/// acknowledged after it is credited.
#[tokio::test]
async fn google_subscription_verify_credits_once_and_acknowledges() {
    let e = google_env().await;
    let (key, aid, token) = store_account(&e.t).await;
    let (_, _, other) = store_account(&e.t).await;
    let before = bal(&e.t.balance(&aid).await);
    let expiry = db::now() + 30 * 86400;
    let body = json!({"product_id": "pro.monthly", "purchase_token": "tok-sub-1"});
    let path = format!("{SUB_PATH}tok-sub-1");

    reply(
        &e.mock,
        &format!("GET {path}"),
        play_sub(ACTIVE, "GPA.1001", &other, expiry, PENDING_ACK),
    );
    let (s, v) =
        e.t.call("POST", "/v1/account/store/google", Some(&key), body.clone())
            .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (403, Some("store_account_mismatch"))
    );
    assert!(calls(&e.mock, "POST", ":acknowledge").is_empty());

    reply(
        &e.mock,
        &format!("GET {path}"),
        play_sub(ACTIVE, "GPA.1001", &token, expiry, PENDING_ACK),
    );
    let (s, v) =
        e.t.call("POST", "/v1/account/store/google", Some(&key), body.clone())
            .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["purchase"]["credited"], true);
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 20.0);
    let ack = calls(
        &e.mock,
        "POST",
        "/purchases/subscriptions/pro.monthly/tokens/tok-sub-1:acknowledge",
    );
    assert_eq!(ack.len(), 1);
    assert_eq!(ack[0].auth.as_deref(), Some("Bearer ya29.test"));
    let (order, ext): (String, String) =
        sqlx::query_as("SELECT id,external_id FROM payments WHERE account_id=?")
            .bind(&aid)
            .fetch_one(&e.t.app.db)
            .await
            .unwrap();
    assert_eq!(
        (v["purchase"]["order_id"].as_str(), ext.as_str()),
        (Some(order.as_str()), "GPA.1001")
    );

    // Repeat: credited false, balance unchanged.
    reply(
        &e.mock,
        &format!("GET {path}"),
        play_sub(ACTIVE, "GPA.1001", &token, expiry, ACKED),
    );
    let (s, v) =
        e.t.call("POST", "/v1/account/store/google", Some(&key), body.clone())
            .await;
    assert_eq!((s, v["purchase"]["credited"].clone()), (200, json!(false)));
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 20.0);
    assert_eq!(calls(&e.mock, "POST", ":acknowledge").len(), 1);

    // The service account assertion was a valid RS256 JWT for the scope.
    let exchange = calls(&e.mock, "POST", "/token");
    assert_eq!(exchange.len(), 1, "the access token is cached");
    let form = exchange[0].body.as_str().unwrap();
    assert!(
        form.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"),
        "{form}"
    );
    let assertion = form.split("assertion=").nth(1).unwrap();
    let jws = jose::split(assertion).unwrap();
    jose::verify_rs256(&e.account_public.n, &e.account_public.e, &jws).unwrap();
    assert_eq!(
        jws.payload["scope"],
        "https://www.googleapis.com/auth/androidpublisher"
    );
    assert_eq!(jws.payload["iss"], "play@project.iam.gserviceaccount.com");

    // Not active, wrong product, unknown product, missing fields.
    reply(
        &e.mock,
        &format!("GET {path}"),
        play_sub(
            "SUBSCRIPTION_STATE_EXPIRED",
            "GPA.1001",
            &token,
            expiry,
            ACKED,
        ),
    );
    let (s, v) =
        e.t.call("POST", "/v1/account/store/google", Some(&key), body)
            .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("subscription_not_active"))
    );
    let (s, _) =
        e.t.call(
            "POST",
            "/v1/account/store/google",
            Some(&key),
            json!({"product_id": "nope", "purchase_token": "t"}),
        )
        .await;
    assert_eq!(s, 400);
    let (s, _) =
        e.t.call(
            "POST",
            "/v1/account/store/google",
            Some(&key),
            json!({"product_id": "pro.monthly"}),
        )
        .await;
    assert_eq!(s, 400);
    e.t.app
        .config_mut()
        .await
        .google
        .service_account_json
        .clear();
    let (s, v) =
        e.t.call(
            "POST",
            "/v1/account/store/google",
            Some(&key),
            json!({"product_id": "pro.monthly", "purchase_token": "x"}),
        )
        .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (503, Some("payments_unavailable"))
    );
}

/// A one-time purchase credits its quantity once and is consumed.
#[tokio::test]
async fn google_one_time_verify_credits_quantity_and_consumes() {
    let e = google_env().await;
    let (key, aid, token) = store_account(&e.t).await;
    let before = bal(&e.t.balance(&aid).await);
    let body = json!({"product_id": "credits.10", "purchase_token": "tok-one-1"});
    let path = format!("{PRODUCT_PATH}tok-one-1");
    let purchase = |state: i64, consumption: i64| {
        json!({
            "purchaseState": state, "consumptionState": consumption, "acknowledgementState": 0,
            "orderId": "GPA.2001", "purchaseTimeMillis": "1", "quantity": 2, "obfuscatedExternalAccountId": token,
        })
    };

    reply(&e.mock, &format!("GET {path}"), purchase(2, 0));
    let (s, v) =
        e.t.call("POST", "/v1/account/store/google", Some(&key), body.clone())
            .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("purchase_pending"))
    );
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 0.0);

    reply(&e.mock, &format!("GET {path}"), purchase(0, 0));
    let (s, v) =
        e.t.call("POST", "/v1/account/store/google", Some(&key), body.clone())
            .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["purchase"]["credited"], true);
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 20.0);
    assert_eq!(
        calls(
            &e.mock,
            "POST",
            "/purchases/products/credits.10/tokens/tok-one-1:consume"
        )
        .len(),
        1
    );

    reply(&e.mock, &format!("GET {path}"), purchase(0, 1));
    let (s, v) =
        e.t.call("POST", "/v1/account/store/google", Some(&key), body)
            .await;
    assert_eq!((s, v["purchase"]["credited"].clone()), (200, json!(false)));
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 20.0);
    assert_eq!(calls(&e.mock, "POST", ":consume").len(), 1);

    // Without an order id the token identifies the charge.
    let mut no_order = purchase(0, 0);
    no_order.as_object_mut().unwrap().remove("orderId");
    reply(&e.mock, &format!("GET {PRODUCT_PATH}tok-one-2"), no_order);
    let (s, _) =
        e.t.call(
            "POST",
            "/v1/account/store/google",
            Some(&key),
            json!({"product_id": "credits.10", "purchase_token": "tok-one-2"}),
        )
        .await;
    assert_eq!(s, 200);
    let ext: String =
        sqlx::query_scalar("SELECT external_id FROM payments WHERE purchase_token='tok-one-2'")
            .fetch_one(&e.t.app.db)
            .await
            .unwrap();
    assert_eq!(ext, "token:tok-one-2");
}

/// Renewal notifications extend the subscription once per purchase and the
/// same Pub/Sub message is never applied twice. Cancellation stops renewal,
/// and notifications for another package are ignored.
#[tokio::test]
async fn google_renewal_notifications_grant_once_and_dedupe_messages() {
    let e = google_env().await;
    let (key, aid, token) = store_account(&e.t).await;
    let before = bal(&e.t.balance(&aid).await);
    let path = format!("{SUB_PATH}tok-sub-2");
    let first = db::now() + 3600;
    reply(
        &e.mock,
        &format!("GET {path}"),
        play_sub(ACTIVE, "GPA.3001", &token, first, ACKED),
    );
    let (s, v) =
        e.t.call(
            "POST",
            "/v1/account/store/google",
            Some(&key),
            json!({"product_id": "pro.monthly", "purchase_token": "tok-sub-2"}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 20.0);

    let renewed = db::now() + 30 * 86400;
    reply(
        &e.mock,
        &format!("GET {path}"),
        play_sub(ACTIVE, "GPA.3001..0", &token, renewed, PENDING_ACK),
    );
    assert_eq!(
        google_push(&e, "msg-1", sub_note(2, "tok-sub-2")).await,
        (200, json!({"received": true}))
    );
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 40.0);
    assert_eq!(
        calls(&e.mock, "POST", ":acknowledge").len(),
        1,
        "a pending renewal is acknowledged"
    );
    assert_eq!(
        google_push(&e, "msg-1", sub_note(2, "tok-sub-2")).await,
        (200, json!({"received": true, "duplicate": true}))
    );
    // Another message for the same order credits nothing more.
    assert_eq!(
        google_push(&e, "msg-2", sub_note(2, "tok-sub-2")).await.0,
        200
    );
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 40.0);
    let (subs, end): (i64, i64) =
        sqlx::query_as("SELECT COUNT(*),MAX(expires_at) FROM subscriptions WHERE account_id=?")
            .bind(&aid)
            .fetch_one(&e.t.app.db)
            .await
            .unwrap();
    assert_eq!((subs, end), (1, renewed));

    // Canceled: access continues, renewal stops.
    assert_eq!(
        google_push(&e, "msg-3", sub_note(3, "tok-sub-2")).await.0,
        200
    );
    let auto: bool = sqlx::query_scalar("SELECT auto_renew FROM subscriptions WHERE account_id=?")
        .bind(&aid)
        .fetch_one(&e.t.app.db)
        .await
        .unwrap();
    assert!(!auto);

    // Another package, a test ping, and ignorable types change nothing.
    let mut elsewhere = sub_note(4, "tok-sub-2");
    elsewhere["packageName"] = "com.other".into();
    assert_eq!(
        google_push(&e, "msg-4", elsewhere).await,
        (200, json!({"received": true}))
    );
    assert_eq!(
        google_push(&e, "msg-5", sub_note(13, "tok-sub-2")).await.0,
        200
    );
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 40.0);
}

/// A purchase upgrade ends the subscription it replaces without blocking.
#[tokio::test]
async fn google_linked_purchase_token_ends_the_old_subscription() {
    let e = google_env().await;
    let (key, aid, token) = store_account(&e.t).await;
    let expiry = db::now() + 30 * 86400;
    for (name, order) in [("old", "GPA.4001"), ("new", "GPA.4002")] {
        let mut sub = play_sub(ACTIVE, order, &token, expiry, ACKED);
        if name == "new" {
            sub["linkedPurchaseToken"] = "old".into();
        }
        reply(&e.mock, &format!("GET {SUB_PATH}{name}"), sub);
        let (s, v) =
            e.t.call(
                "POST",
                "/v1/account/store/google",
                Some(&key),
                json!({"product_id": "pro.monthly", "purchase_token": name}),
            )
            .await;
        assert_eq!(s, 200, "{v}");
    }
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT purchase_token,status FROM subscriptions WHERE account_id=? ORDER BY purchase_token").bind(&aid).fetch_all(&e.t.app.db).await.unwrap();
    assert_eq!(
        rows,
        [
            ("new".to_owned(), "active".to_owned()),
            ("old".to_owned(), "refunded".to_owned())
        ]
    );
    let status: String = sqlx::query_scalar("SELECT status FROM accounts WHERE id=?")
        .bind(&aid)
        .fetch_one(&e.t.app.db)
        .await
        .unwrap();
    assert_eq!(status, "active");
}

/// A one-time purchase notification credits and consumes the purchase.
#[tokio::test]
async fn google_one_time_notification_credits_once() {
    let e = google_env().await;
    let (_, aid, token) = store_account(&e.t).await;
    let before = bal(&e.t.balance(&aid).await);
    reply(
        &e.mock,
        &format!("GET {PRODUCT_PATH}tok-one-3"),
        json!({
            "purchaseState": 0, "consumptionState": 0, "orderId": "GPA.5001", "quantity": 1,
            "obfuscatedExternalAccountId": token,
        }),
    );
    let note = json!({"version": "1.0", "packageName": PACKAGE, "eventTimeMillis": "1",
        "oneTimeProductNotification": {"version": "1.0", "notificationType": 1, "purchaseToken": "tok-one-3", "sku": "credits.10"}});
    assert_eq!(google_push(&e, "msg-a", note.clone()).await.0, 200);
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 10.0);
    assert_eq!(calls(&e.mock, "POST", ":consume").len(), 1);
    assert_eq!(google_push(&e, "msg-b", note).await.0, 200);
    assert_eq!(bal(&e.t.balance(&aid).await) - before, 10.0);
}

/// Voided purchases and revocations zero the account and block it.
#[tokio::test]
async fn google_voided_purchase_blocks_the_account() {
    let e = google_env().await;
    let (key, aid, token) = store_account(&e.t).await;
    // A second account is opened first: a refund blocks new signups from the address.
    let (key2, aid2, token2) = store_account(&e.t).await;
    reply(
        &e.mock,
        &format!("GET {SUB_PATH}tok-sub-9"),
        play_sub(ACTIVE, "GPA.6002", &token2, db::now() + 86400, ACKED),
    );
    let (s, _) =
        e.t.call(
            "POST",
            "/v1/account/store/google",
            Some(&key2),
            json!({"product_id": "pro.monthly", "purchase_token": "tok-sub-9"}),
        )
        .await;
    assert_eq!(s, 200);
    reply(
        &e.mock,
        &format!("GET {PRODUCT_PATH}tok-one-4"),
        json!({
            "purchaseState": 0, "consumptionState": 0, "orderId": "GPA.6001", "quantity": 1,
            "obfuscatedExternalAccountId": token,
        }),
    );
    let (s, v) =
        e.t.call(
            "POST",
            "/v1/account/store/google",
            Some(&key),
            json!({"product_id": "credits.10", "purchase_token": "tok-one-4"}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert!(bal(&e.t.balance(&aid).await) >= 10.0);

    let note = json!({"version": "1.0", "packageName": PACKAGE, "eventTimeMillis": "1",
        "voidedPurchaseNotification": {"purchaseToken": "tok-one-4", "orderId": "GPA.6001", "productType": 2, "refundType": 1}});
    assert_eq!(
        google_push(&e, "msg-v1", note.clone()).await,
        (200, json!({"received": true}))
    );
    assert_eq!(bal(&e.t.balance(&aid).await), 0.0);
    let (status, reason): (String, String) =
        sqlx::query_as("SELECT status,suspended_reason FROM accounts WHERE id=?")
            .bind(&aid)
            .fetch_one(&e.t.app.db)
            .await
            .unwrap();
    assert_eq!((status.as_str(), reason.as_str()), ("suspended", "refund"));
    let blocks: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM refund_blocks WHERE account_id=?")
            .bind(&aid)
            .fetch_all(&e.t.app.db)
            .await
            .unwrap();
    assert_eq!(blocks, ["ip"]);
    let payment: String =
        sqlx::query_scalar("SELECT status FROM payments WHERE external_id='GPA.6001'")
            .fetch_one(&e.t.app.db)
            .await
            .unwrap();
    assert_eq!(payment, "refunded");
    assert_eq!(google_push(&e, "msg-v1", note).await.1["duplicate"], true);

    // A revoked subscription blocks as well.
    assert_eq!(
        google_push(&e, "msg-v2", sub_note(12, "tok-sub-9")).await.0,
        200
    );
    assert_eq!(bal(&e.t.balance(&aid2).await), 0.0);
    let status: String = sqlx::query_scalar("SELECT status FROM accounts WHERE id=?")
        .bind(&aid2)
        .fetch_one(&e.t.app.db)
        .await
        .unwrap();
    assert_eq!(status, "suspended");
}

/// A pending chargeback review is answered with the consumption share.
#[tokio::test]
async fn google_pending_refund_review_posts_the_review() {
    let e = google_env().await;
    common::model(
        &mut *e.t.app.config_mut().await,
        "gratis",
        Capability::Chat,
        Tier::Free,
        "llm",
        "gratis",
    );
    let (key, _, token) = store_account(&e.t).await;
    reply(
        &e.mock,
        &format!("GET {PRODUCT_PATH}tok-one-5"),
        json!({
            "purchaseState": 0, "consumptionState": 0, "orderId": "GPA.7001", "quantity": 1,
            "obfuscatedExternalAccountId": token,
        }),
    );
    let (s, _) =
        e.t.call(
            "POST",
            "/v1/account/store/google",
            Some(&key),
            json!({"product_id": "credits.10", "purchase_token": "tok-one-5"}),
        )
        .await;
    assert_eq!(s, 200);
    sqlx::query("UPDATE credits SET remaining_micro=amount_micro*7/10 WHERE payment_id IN (SELECT id FROM payments WHERE external_id='GPA.7001')").execute(&e.t.app.db).await.unwrap();
    let review = |reason: i64| {
        json!({"version": "1.0", "packageName": PACKAGE, "eventTimeMillis": "1",
        "pendingRefundReviewNotification": {"pendingRefundToken": "prt-1", "orderId": "GPA.7001",
                                             "refundReason": reason, "obfuscatedAccountId": token}})
    };
    let path = "/orders/GPA.7001:reviewrefund";
    reply(
        &e.mock,
        &format!("POST /androidpublisher/v3/applications/com.example.app{path}"),
        json!({}),
    );

    // Only chargebacks are answered.
    assert_eq!(google_push(&e, "msg-r0", review(1)).await.0, 200);
    assert!(calls(&e.mock, "POST", path).is_empty());
    assert_eq!(
        google_push(&e, "msg-r1", review(7)).await,
        (200, json!({"received": true}))
    );
    let sent = calls(&e.mock, "POST", path);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].body,
        json!({"pendingRefundToken": "prt-1", "sampleContentProvided": true,
               "refundPreference": "NEUTRAL", "consumptionPercentageMilliunits": 30000})
    );
    assert_eq!(sent[0].auth.as_deref(), Some("Bearer ya29.test"));

    // Mostly consumed: decline. Unused: approve.
    for (message, remaining, preference) in [("msg-r2", 3, "DECLINE"), ("msg-r3", 10, "APPROVE")] {
        sqlx::query(
            "UPDATE credits SET remaining_micro=amount_micro*?/10 WHERE payment_id IS NOT NULL",
        )
        .bind(remaining)
        .execute(&e.t.app.db)
        .await
        .unwrap();
        assert_eq!(google_push(&e, message, review(7)).await.0, 200);
        assert_eq!(
            calls(&e.mock, "POST", path).last().unwrap().body["refundPreference"],
            preference
        );
    }
    // The same message again is a duplicate.
    assert_eq!(
        google_push(&e, "msg-r3", review(7)).await.1["duplicate"],
        true
    );
    assert_eq!(calls(&e.mock, "POST", path).len(), 3);
}
