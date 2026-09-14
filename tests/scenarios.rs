//! Multi-step interaction scenarios for the customer workspace and the
//! administrator console.
//!
//! Every test drives the real HTTP surface the embedded panel uses
//! (`/v1/*` for the workspace, `/admin/api/*` for the console) and asserts
//! the resulting state in SQLite and in API responses.

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{HeaderMap, Request},
    routing::any,
};
use reseller::{
    App, billing,
    config::{Config, Policy},
    db, identity,
};
use serde_json::{Value, json};
use sqlx::Row;
use std::net::SocketAddr;
use tower::ServiceExt;

const ADMIN: &str = "admin-test-secret-123456789";

struct Test {
    app: App,
    _dir: tempfile::TempDir,
}
impl Test {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("test.db")).await.unwrap();
        let c = Config {
            admin_token: ADMIN.into(),
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
    async fn admin(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        self.call(method, &format!("/admin/api/{path}"), Some(ADMIN), body)
            .await
    }
    async fn customer(&self, method: &str, path: &str, token: &str, body: Value) -> (u16, Value) {
        self.call(method, &format!("/v1/{path}"), Some(token), body)
            .await
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
/// Point the LLM upstream at a local server that reports a fixed cost/token
/// usage, so billing assertions are deterministic.
async fn mock_llm(t: &Test, cost: &'static str, tokens: i64) -> tokio::task::JoinHandle<()> {
    let (base, task) = upstream(Router::new().fallback(any(
        move |_req: Request<Body>| async move {
            axum::Json(json!({"id":"mock","usage":{"total_tokens":tokens,"cost":cost}}))
        },
    )))
    .await;
    let mut c = t.app.config.write().await;
    c.llm.base_url = format!("{base}/v1");
    c.llm.api_key = "upstream-secret".into();
    c.llm.model = "configured".into();
    task
}
async fn balance(t: &Test, aid: &str) -> String {
    billing::account(&t.app, aid).await.unwrap()["account"]["balance_usd"]
        .as_str()
        .unwrap()
        .to_owned()
}
async fn issue_credit_code(t: &Test, amount: &str) -> String {
    let (s, v) = t
        .admin("POST", "codes", json!({"credit_usd": amount}))
        .await;
    assert_eq!(s, 200, "issue credit code: {v}");
    v["code"].as_str().unwrap().to_owned()
}
async fn issue_plan_code(t: &Test, plan: &str) -> String {
    let (s, v) = t.admin("POST", "codes", json!({"plan_code": plan})).await;
    assert_eq!(s, 200, "issue plan code: {v}");
    v["code"].as_str().unwrap().to_owned()
}
async fn redeem(t: &Test, token: &str, code: &str) -> (u16, Value) {
    t.customer("POST", "account/redeem", token, json!({"code": code}))
        .await
}

// ---------------------------------------------------------------------------
// Credits, redeem codes, and plan stacking
// ---------------------------------------------------------------------------

/// Redeem a perpetual credit code, then a Monthly plan: usage must consume the
/// expiring subscription lot before the non-expiring purchase lot, and an
/// expired subscription remainder must vanish without touching the perpetual
/// credit.
#[tokio::test]
async fn perpetual_code_then_monthly_plan_consumes_expiring_credit_first() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    // Start from a clean ledger (the signup trial credit has its own test).
    sqlx::query("DELETE FROM credits")
        .execute(&t.app.db)
        .await
        .unwrap();

    let code = issue_credit_code(&t, "10").await;
    let (s, v) = redeem(&t, k["dashboard_token"].as_str().unwrap(), &code).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["account"]["balance_usd"], "10");

    let code = issue_plan_code(&t, "monthly").await;
    let (s, v) = redeem(&t, k["dashboard_token"].as_str().unwrap(), &code).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["account"]["balance_usd"], "30");
    assert_eq!(v["account"]["subscription"]["plan_code"], "monthly");

    // Spend $25 directly on the ledger: the subscription lot must be used first.
    let mut tx = t.app.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    billing::debit(&mut tx, &aid, 25_000_000).await.unwrap();
    tx.commit().await.unwrap();
    let sub: i64 =
        sqlx::query_scalar("SELECT remaining_micro FROM credits WHERE source='subscription'")
            .fetch_one(&t.app.db)
            .await
            .unwrap();
    let purchase: i64 =
        sqlx::query_scalar("SELECT remaining_micro FROM credits WHERE source='purchase'")
            .fetch_one(&t.app.db)
            .await
            .unwrap();
    assert_eq!(sub, 0, "subscription credit should be consumed first");
    assert_eq!(purchase, 5_000_000, "perpetual credit must be preserved");
    assert_eq!(balance(&t, &aid).await, "5");

    // Simulate the Monthly lot reaching its expiry: it drops out of the
    // spendable balance, the perpetual lot remains, and the ledger keeps both.
    sqlx::query("UPDATE credits SET expires_at=? WHERE source='subscription'")
        .bind(db::now() - 1)
        .execute(&t.app.db)
        .await
        .unwrap();
    assert_eq!(balance(&t, &aid).await, "5");
    let v = billing::account(&t.app, &aid).await.unwrap();
    assert_eq!(v["credits"].as_array().unwrap().len(), 2);
    let mut tx = t.app.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    billing::debit(&mut tx, &aid, 2_000_000).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(balance(&t, &aid).await, "3");
}

/// A credit code redeemed while the account is in debt must settle the debt
/// first and only expose the remainder as spendable credit.
#[tokio::test]
async fn redeem_credit_code_settles_existing_debt_first() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    sqlx::query("DELETE FROM credits")
        .execute(&t.app.db)
        .await
        .unwrap();
    let mut tx = t.app.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    billing::debit(&mut tx, &aid, 2_000_000).await.unwrap();
    tx.commit().await.unwrap();
    let v = billing::account(&t.app, &aid).await.unwrap();
    assert_eq!(v["account"]["balance_usd"], "-2");
    assert_eq!(v["account"]["debt_usd"], "2");

    let code = issue_credit_code(&t, "10").await;
    let (s, v) = redeem(&t, k["dashboard_token"].as_str().unwrap(), &code).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["account"]["balance_usd"], "8");
    assert_eq!(v["account"]["debt_usd"], "0");
    let remaining: i64 =
        sqlx::query_scalar("SELECT remaining_micro FROM credits WHERE source='purchase'")
            .fetch_one(&t.app.db)
            .await
            .unwrap();
    assert_eq!(remaining, 8_000_000);
}

/// Signup trial credit that has expired cannot be spent, blocks proxying at
/// admission, and an admin top-up restores service without resurrecting the
/// expired lot.
#[tokio::test]
async fn expired_free_credit_blocks_proxy_until_admin_topup() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    sqlx::query("UPDATE credits SET expires_at=?")
        .bind(db::now() - 1)
        .execute(&t.app.db)
        .await
        .unwrap();
    let task = mock_llm(&t, "0.01", 30).await;

    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 402, "{v}");
    assert_eq!(v["error"]["code"], "insufficient_balance");

    let (s, v) = t
        .admin(
            "POST",
            &format!("accounts/{aid}/credits"),
            json!({"amount_usd":"5"}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(balance(&t, &aid).await, "4.987");
    task.abort();
}

/// Codes are single use, rejected once expired, and plan codes stop working
/// while their plan is deactivated (they are bound to the plan, not a
/// snapshot).
#[tokio::test]
async fn redeem_code_is_single_use_and_rejects_expired_or_deactivated_plan() {
    let t = Test::new().await;
    let k = t.key().await;
    let token = k["dashboard_token"].as_str().unwrap();

    let code = issue_credit_code(&t, "10").await;
    assert_eq!(redeem(&t, token, &code).await.0, 200);
    assert_eq!(redeem(&t, token, &code).await.0, 400);

    let plan_code = issue_plan_code(&t, "monthly").await;
    let (s, _) = t
        .admin(
            "POST",
            "plans",
            json!({"code":"monthly","name":"Monthly","price_usd":"20","credit_usd":"20","duration_days":30,"active":false}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, v) = redeem(&t, token, &plan_code).await;
    assert_eq!(s, 400, "{v}");
    let (s, _) = t
        .admin(
            "POST",
            "plans",
            json!({"code":"monthly","name":"Monthly","price_usd":"20","credit_usd":"20","duration_days":30,"active":true}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, v) = redeem(&t, token, &plan_code).await;
    assert_eq!(s, 200, "{v}");

    let expiring = issue_credit_code(&t, "7").await;
    sqlx::query("UPDATE redeem_codes SET expires_at=? WHERE redeemed_at IS NULL")
        .bind(db::now() - 1)
        .execute(&t.app.db)
        .await
        .unwrap();
    let (s, v) = redeem(&t, token, &expiring).await;
    assert_eq!(s, 400, "{v}");
}

/// Two Monthly purchases are separate credit packages: each lot has its own
/// 30-day window (they do not stack into 60 days), and usage drains the
/// earliest window first.
#[tokio::test]
async fn repeat_plan_grants_have_independent_expiry_windows() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    let token = k["dashboard_token"].as_str().unwrap();
    sqlx::query("DELETE FROM credits")
        .execute(&t.app.db)
        .await
        .unwrap();

    let first = issue_plan_code(&t, "monthly").await;
    let (s, v) = redeem(&t, token, &first).await;
    assert_eq!(s, 200, "{v}");
    let second = issue_plan_code(&t, "monthly").await;
    let (s, v) = redeem(&t, token, &second).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["account"]["balance_usd"], "40");

    let subs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM subscriptions")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(subs, 2);
    let oldest: String =
        sqlx::query_scalar("SELECT id FROM credits ORDER BY created_at,id LIMIT 1")
            .fetch_one(&t.app.db)
            .await
            .unwrap();
    sqlx::query("UPDATE credits SET expires_at=? WHERE id=?")
        .bind(db::now() + 5 * 86400)
        .bind(&oldest)
        .execute(&t.app.db)
        .await
        .unwrap();

    let mut tx = t.app.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    billing::debit(&mut tx, &aid, 21_000_000).await.unwrap();
    tx.commit().await.unwrap();
    let older: i64 = sqlx::query_scalar("SELECT remaining_micro FROM credits WHERE id=?")
        .bind(&oldest)
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    let newer: i64 = sqlx::query_scalar(
        "SELECT remaining_micro FROM credits WHERE id<>? ORDER BY created_at,id LIMIT 1",
    )
    .bind(&oldest)
    .fetch_one(&t.app.db)
    .await
    .unwrap();
    assert_eq!(older, 0);
    assert_eq!(newer, 19_000_000);
    assert_eq!(balance(&t, &aid).await, "19");
}

// ---------------------------------------------------------------------------
// Key lifecycle: create, block, rotate, revoke
// ---------------------------------------------------------------------------

/// Rotating a key must preserve limits/markup/expiry on the replacement, block
/// the old API key for proxying, yet keep the old dashboard token able to open
/// the workspace (blocked keys retain workspace access until revoked).
#[tokio::test]
async fn rotate_preserves_terms_and_old_dashboard_token_keeps_workspace_access() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    let kid = k["id"].as_str().unwrap().to_owned();
    let (s, v) = t
        .admin(
            "PATCH",
            &format!("keys/{kid}"),
            json!({"markup_pct":"45","limits":{"rpm":7,"max_children":3,"allowed_models":["configured"]},"expires_at":db::now()+86400}),
        )
        .await;
    assert_eq!(s, 200, "{v}");

    let (s, rotated) = t
        .customer(
            "POST",
            &format!("keys/{kid}/rotate"),
            k["dashboard_token"].as_str().unwrap(),
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{rotated}");
    assert_eq!(rotated["account_id"], aid);
    assert_ne!(rotated["id"], k["id"]);
    let r =
        sqlx::query("SELECT parent_key_id,markup_pct,expires_at,limits FROM api_keys WHERE id=?")
            .bind(rotated["id"].as_str())
            .fetch_one(&t.app.db)
            .await
            .unwrap();
    assert_eq!(r.get::<String, _>("parent_key_id"), kid);
    assert_eq!(r.get::<String, _>("markup_pct"), "45");
    assert!(r.get::<Option<i64>, _>("expires_at").unwrap() > db::now());
    let limits: Value = serde_json::from_str(r.get("limits")).unwrap();
    assert_eq!(limits["rpm"], 7);
    assert_eq!(limits["max_children"], 3);
    let old_status: String = sqlx::query_scalar("SELECT status FROM api_keys WHERE id=?")
        .bind(&kid)
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(old_status, "blocked");

    let task = mock_llm(&t, "0", 0).await;
    // Old API key: workspace yes, proxy no.
    assert_eq!(
        t.customer("GET", "account", k["key"].as_str().unwrap(), json!({}))
            .await
            .0,
        200
    );
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 401, "{v}");
    assert_eq!(v["error"]["code"], "key_blocked");
    // Old dashboard token still opens the workspace and lists keys.
    let (s, keys) = t
        .customer(
            "GET",
            "keys",
            k["dashboard_token"].as_str().unwrap(),
            json!({}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(keys["keys"].as_array().unwrap().len(), 2);
    // Replacement credentials fully work.
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            rotated["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        t.customer(
            "GET",
            "account",
            rotated["dashboard_token"].as_str().unwrap(),
            json!({})
        )
        .await
        .0,
        200
    );
    task.abort();
}

/// Revocation is permanent: both credentials die immediately and neither
/// reactivation nor rotation is possible afterwards.
#[tokio::test]
async fn revocation_is_permanent_and_invalidates_both_credentials() {
    let t = Test::new().await;
    let k = t.key().await;
    let kid = k["id"].as_str().unwrap().to_owned();
    let (s, _) = t
        .admin("POST", &format!("keys/{kid}/revoke"), json!({}))
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer("GET", "account", k["key"].as_str().unwrap(), json!({}))
            .await
            .0,
        401
    );
    assert_eq!(
        t.customer(
            "GET",
            "account",
            k["dashboard_token"].as_str().unwrap(),
            json!({})
        )
        .await
        .0,
        401
    );
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 401, "{v}");
    assert_eq!(
        t.admin("PATCH", &format!("keys/{kid}"), json!({"status":"active"}))
            .await
            .0,
        400
    );
    assert_eq!(
        t.admin("POST", &format!("keys/{kid}/rotate"), json!({}))
            .await
            .0,
        400
    );
}

/// Creating an additional key and revoking the original must leave the new
/// key (and its own dashboard token) fully functional on the shared balance.
#[tokio::test]
async fn revoking_one_key_does_not_affect_siblings() {
    let t = Test::new().await;
    let a = t.key().await;
    let aid = a["account_id"].as_str().unwrap();
    let (s, b) = t
        .customer(
            "POST",
            "keys",
            a["dashboard_token"].as_str().unwrap(),
            json!({"name":"second"}),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["account_id"], aid);
    let (s, _) = t
        .admin(
            "POST",
            &format!("keys/{}/revoke", a["id"].as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!(s, 200);

    assert_eq!(
        t.customer("GET", "account", a["key"].as_str().unwrap(), json!({}))
            .await
            .0,
        401
    );
    assert_eq!(
        t.customer(
            "GET",
            "account",
            a["dashboard_token"].as_str().unwrap(),
            json!({})
        )
        .await
        .0,
        401
    );
    let task = mock_llm(&t, "0", 0).await;
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            b["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        t.customer(
            "GET",
            "account",
            b["dashboard_token"].as_str().unwrap(),
            json!({})
        )
        .await
        .0,
        200
    );
    task.abort();
}

/// Blocking is self-service and reversible by admins only; blocked keys keep
/// workspace access, cannot proxy, and cross-account writes must 404.
#[tokio::test]
async fn block_and_reactivate_cycle_is_enforced_across_accounts() {
    let t = Test::new().await;
    let a = t.key().await;
    let b = t.key().await;
    let kid = a["id"].as_str().unwrap().to_owned();
    let (s, _) = t
        .customer(
            "PATCH",
            &format!("keys/{kid}"),
            a["dashboard_token"].as_str().unwrap(),
            json!({"name":"primary","status":"blocked"}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            a["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 401, "{v}");
    assert_eq!(
        t.customer("GET", "account", a["key"].as_str().unwrap(), json!({}))
            .await
            .0,
        200
    );
    assert_eq!(
        t.customer(
            "PATCH",
            &format!("keys/{kid}"),
            b["dashboard_token"].as_str().unwrap(),
            json!({"name":"stolen"}),
        )
        .await
        .0,
        404
    );
    assert_eq!(
        t.customer(
            "POST",
            &format!("keys/{kid}/rotate"),
            b["dashboard_token"].as_str().unwrap(),
            json!({}),
        )
        .await
        .0,
        404
    );
    let keys = t
        .customer(
            "GET",
            "keys",
            b["dashboard_token"].as_str().unwrap(),
            json!({}),
        )
        .await
        .1;
    assert!(
        !keys["keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k["id"] == a["id"])
    );
    assert_eq!(
        t.customer(
            "PATCH",
            &format!("keys/{kid}"),
            a["dashboard_token"].as_str().unwrap(),
            json!({"status":"active"}),
        )
        .await
        .0,
        400
    );
    assert_eq!(
        t.admin("PATCH", &format!("keys/{kid}"), json!({"status":"active"}))
            .await
            .0,
        200
    );
    let task = mock_llm(&t, "0", 0).await;
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            a["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    task.abort();
}

/// `max_keys_per_account` counts active keys only; blocking frees a slot and
/// rotation never exceeds the cap.
#[tokio::test]
async fn account_key_cap_counts_active_keys_and_rotation_respects_it() {
    let t = Test::new().await;
    t.app.config.write().await.max_keys_per_account = 2;
    let k1 = t.key().await;
    let token = k1["dashboard_token"].as_str().unwrap();
    let (s, k2) = t
        .customer("POST", "keys", token, json!({"name":"second"}))
        .await;
    assert_eq!(s, 200, "{k2}");
    let (s, v) = t
        .customer("POST", "keys", token, json!({"name":"third"}))
        .await;
    assert_eq!(s, 429, "{v}");
    assert_eq!(v["error"]["code"], "account_key_limit");

    let (s, _) = t
        .customer(
            "PATCH",
            &format!("keys/{}", k2["id"].as_str().unwrap()),
            token,
            json!({"status":"blocked"}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, k4) = t
        .customer("POST", "keys", token, json!({"name":"fourth"}))
        .await;
    assert_eq!(s, 200, "{k4}");
    // Blocked keys cannot be rotated at all.
    let (s, v) = t
        .customer(
            "POST",
            &format!("keys/{}/rotate", k2["id"].as_str().unwrap()),
            token,
            json!({}),
        )
        .await;
    assert_eq!(s, 400, "{v}");
    assert_eq!(v["error"]["code"], "key_not_active");
    // Rotating an active key keeps the active count at the cap.
    let (s, k5) = t
        .customer(
            "POST",
            &format!("keys/{}/rotate", k1["id"].as_str().unwrap()),
            token,
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{k5}");
    let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE status='active'")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(active, 2);
}

/// Child-key limits are enforced for API-key callers, but dashboard tokens
/// (and therefore each rotation) start a fresh child budget. This documents
/// the current behavior; the account-level key cap still bounds the account.
#[tokio::test]
async fn child_key_limit_applies_to_api_keys_and_resets_on_rotation() {
    let t = Test::new().await;
    let k = t.key().await;
    let kid = k["id"].as_str().unwrap().to_owned();
    let (s, _) = t
        .admin(
            "PATCH",
            &format!("keys/{kid}"),
            json!({"limits":{"max_children":1,"allowed_models":[]}}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, child) = t
        .customer(
            "POST",
            "keys",
            k["key"].as_str().unwrap(),
            json!({"name":"child"}),
        )
        .await;
    assert_eq!(s, 200, "{child}");
    assert_eq!(child["account_id"], k["account_id"]);
    let (s, v) = t
        .customer(
            "POST",
            "keys",
            k["key"].as_str().unwrap(),
            json!({"name":"child2"}),
        )
        .await;
    assert_eq!(s, 429, "{v}");
    assert_eq!(v["error"]["code"], "child_key_limit");
    // The dashboard token creates account keys without the per-key child cap.
    let (s, _) = t
        .customer(
            "POST",
            "keys",
            k["dashboard_token"].as_str().unwrap(),
            json!({"name":"via dashboard"}),
        )
        .await;
    assert_eq!(s, 200);
    // A rotated replacement inherits the limit but not the old children, so a
    // fresh child is accepted even though the original key was at its limit.
    let (s, rotated) = t
        .customer(
            "POST",
            &format!("keys/{kid}/rotate"),
            k["dashboard_token"].as_str().unwrap(),
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{rotated}");
    let (s, extra) = t
        .customer(
            "POST",
            "keys",
            rotated["key"].as_str().unwrap(),
            json!({"name":"after rotate"}),
        )
        .await;
    assert_eq!(s, 200, "{extra}");
}

// ---------------------------------------------------------------------------
// Account state: suspension and key expiry
// ---------------------------------------------------------------------------

/// Suspension blocks both proxying and workspace access, and lifting it
/// restores the same balance and credentials.
#[tokio::test]
async fn suspension_blocks_proxy_and_workspace_then_restores() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    let code = issue_credit_code(&t, "10").await;
    assert_eq!(
        redeem(&t, k["dashboard_token"].as_str().unwrap(), &code)
            .await
            .0,
        200
    );
    let (s, _) = t
        .admin(
            "PATCH",
            &format!("accounts/{aid}"),
            json!({"status":"suspended"}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 403, "{v}");
    assert_eq!(v["error"]["code"], "account_suspended");
    assert_eq!(
        t.customer(
            "GET",
            "account",
            k["dashboard_token"].as_str().unwrap(),
            json!({})
        )
        .await
        .0,
        403
    );
    let (s, _) = t
        .admin(
            "PATCH",
            &format!("accounts/{aid}"),
            json!({"status":"active"}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer(
            "GET",
            "account",
            k["dashboard_token"].as_str().unwrap(),
            json!({})
        )
        .await
        .0,
        200
    );
    let task = mock_llm(&t, "0", 0).await;
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    assert_eq!(balance(&t, &aid).await, "10.25");
    task.abort();
}

/// Key expiry is a proxy-only gate: the workspace stays reachable, and an
/// admin can clear or extend the expiry.
#[tokio::test]
async fn key_expiry_blocks_proxy_but_keeps_workspace_and_can_be_cleared() {
    let t = Test::new().await;
    let k = t.key().await;
    let kid = k["id"].as_str().unwrap().to_owned();
    let task = mock_llm(&t, "0", 0).await;
    let (s, _) = t
        .admin(
            "PATCH",
            &format!("keys/{kid}"),
            json!({"expires_at":db::now()+60}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    let (s, v) = t
        .admin(
            "PATCH",
            &format!("keys/{kid}"),
            json!({"expires_at":db::now()-1}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 401, "{v}");
    assert_eq!(v["error"]["code"], "key_expired");
    assert_eq!(
        t.customer(
            "GET",
            "account",
            k["dashboard_token"].as_str().unwrap(),
            json!({})
        )
        .await
        .0,
        200
    );
    let (s, _) = t
        .admin("PATCH", &format!("keys/{kid}"), json!({"expires_at":null}))
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    task.abort();
}

// ---------------------------------------------------------------------------
// Limits and metering
// ---------------------------------------------------------------------------

/// Spend limits accumulate across completed requests and stop the next one.
#[tokio::test]
async fn spend_limit_accumulates_across_completed_requests() {
    let t = Test::new().await;
    let k = t.key().await;
    let kid = k["id"].as_str().unwrap();
    let (s, _) = t
        .admin(
            "PATCH",
            &format!("keys/{kid}"),
            json!({"limits":{"spend":{"max_usd":"0.02","window_hours":24}}}),
        )
        .await;
    assert_eq!(s, 200);
    let task = mock_llm(&t, "0.01", 30).await;
    for expected in [200, 200] {
        let (s, v) = t
            .customer(
                "POST",
                "chat/completions",
                k["key"].as_str().unwrap(),
                json!({"messages":[]}),
            )
            .await;
        assert_eq!(s, expected, "{v}");
    }
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 429, "{v}");
    assert_eq!(v["error"]["code"], "spend_limit_exceeded");
    let v = reseller::stats::overview(&t.app).await.unwrap();
    assert_eq!(v["billed_cost_usd"], "0.026");
    task.abort();
}

/// Request and token windows are independent and count completed usage.
#[tokio::test]
async fn request_and_token_windows_count_usage() {
    let t = Test::new().await;
    let requests_key = t.key().await;
    let (s, _) = t
        .admin(
            "PATCH",
            &format!("keys/{}", requests_key["id"].as_str().unwrap()),
            json!({"limits":{"requests":{"max":2,"window_hours":24}}}),
        )
        .await;
    assert_eq!(s, 200);
    let task = mock_llm(&t, "0", 5).await;
    for expected in [200, 200] {
        assert_eq!(
            t.customer(
                "POST",
                "chat/completions",
                requests_key["key"].as_str().unwrap(),
                json!({"messages":[]})
            )
            .await
            .0,
            expected
        );
    }
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            requests_key["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 429, "{v}");
    assert_eq!(v["error"]["code"], "request_limit_exceeded");

    let token_key = t.key().await;
    let (s, _) = t
        .admin(
            "PATCH",
            &format!("keys/{}", token_key["id"].as_str().unwrap()),
            json!({"limits":{"tokens":{"max":10,"window_hours":24}}}),
        )
        .await;
    assert_eq!(s, 200);
    for expected in [200, 200] {
        assert_eq!(
            t.customer(
                "POST",
                "chat/completions",
                token_key["key"].as_str().unwrap(),
                json!({"messages":[]})
            )
            .await
            .0,
            expected
        );
    }
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            token_key["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 429, "{v}");
    assert_eq!(v["error"]["code"], "token_limit_exceeded");
    task.abort();
}

/// An admin can relax a model allowlist without replacing the credential.
#[tokio::test]
async fn model_allowlist_update_takes_effect_on_next_request() {
    let t = Test::new().await;
    let k = t.key().await;
    let kid = k["id"].as_str().unwrap();
    let task = mock_llm(&t, "0", 0).await;
    let (s, _) = t
        .admin(
            "PATCH",
            &format!("keys/{kid}"),
            json!({"limits":{"allowed_models":["other"]}}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 403, "{v}");
    assert_eq!(v["error"]["code"], "model_not_allowed");
    let (s, _) = t
        .admin(
            "PATCH",
            &format!("keys/{kid}"),
            json!({"limits":{"allowed_models":["configured"]}}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    task.abort();
}

/// Markup resolves key → account → global for each request.
#[tokio::test]
async fn markup_resolution_prefers_key_then_account_then_global() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap();
    let kid = k["id"].as_str().unwrap().to_owned();
    let task = mock_llm(&t, "0.01", 0).await;

    let (s, _) = t
        .admin(
            "PATCH",
            &format!("accounts/{aid}"),
            json!({"markup_pct":"50"}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    ); // account 50% -> 0.015
    let (s, _) = t
        .admin("PATCH", &format!("keys/{kid}"), json!({"markup_pct":"45"}))
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    ); // key 45% -> 0.0145
    let (s, _) = t
        .admin("PATCH", &format!("keys/{kid}"), json!({"markup_pct":null}))
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    ); // back to account 50% -> 0.015
    let (s, _) = t
        .admin(
            "PATCH",
            &format!("accounts/{aid}"),
            json!({"markup_pct":null}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    ); // global 30% -> 0.013
    let v = reseller::stats::overview(&t.app).await.unwrap();
    assert_eq!(v["billed_cost_usd"], "0.0575");
    task.abort();
}

// ---------------------------------------------------------------------------
// Settings and credentials
// ---------------------------------------------------------------------------

/// Changing the key pepper invalidates existing keys and unredeemed codes;
/// the admin token is unaffected; freshly issued credentials work.
#[tokio::test]
async fn key_pepper_rotation_invalidates_keys_and_unredeemed_codes() {
    let t = Test::new().await;
    let old = t.key().await;
    let stale_code = issue_credit_code(&t, "10").await;
    let (s, _) = t
        .admin(
            "PATCH",
            "settings",
            json!({"key_pepper":"new-pepper-secret-34567890abcdef"}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer("GET", "account", old["key"].as_str().unwrap(), json!({}))
            .await
            .0,
        401
    );
    assert_eq!(
        t.customer(
            "GET",
            "account",
            old["dashboard_token"].as_str().unwrap(),
            json!({})
        )
        .await
        .0,
        401
    );
    assert_eq!(
        t.admin("GET", "overview", json!({})).await.0,
        200,
        "admin token must survive"
    );
    let fresh = t.key().await;
    let (s, v) = redeem(&t, fresh["dashboard_token"].as_str().unwrap(), &stale_code).await;
    assert_eq!(s, 400, "{v}");
    let new_code = issue_credit_code(&t, "10").await;
    let (s, v) = redeem(&t, fresh["dashboard_token"].as_str().unwrap(), &new_code).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["account"]["balance_usd"], "10.25");
}

/// Rotating the admin token only affects administrator access.
#[tokio::test]
async fn admin_token_rotation_only_affects_admin_access() {
    let t = Test::new().await;
    let k = t.key().await;
    let (s, _) = t
        .admin(
            "PATCH",
            "settings",
            json!({"admin_token":"rotated-admin-token-1234567890"}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(t.admin("GET", "overview", json!({})).await.0, 401);
    assert_eq!(
        t.call(
            "GET",
            "/admin/api/overview",
            Some("rotated-admin-token-1234567890"),
            json!({})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        t.customer(
            "GET",
            "account",
            k["dashboard_token"].as_str().unwrap(),
            json!({})
        )
        .await
        .0,
        200
    );
}

/// Default limits are snapshotted when keys are minted; existing keys keep the
/// limits they were created with.
#[tokio::test]
async fn default_limits_apply_to_new_keys_only() {
    let t = Test::new().await;
    let old = t.key().await;
    let (s, _) = t
        .admin(
            "PATCH",
            "settings",
            json!({"default_limits":{"rpm":3,"allowed_models":["configured"]}}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, fresh) = t
        .customer(
            "POST",
            "keys",
            old["dashboard_token"].as_str().unwrap(),
            json!({"name":"fresh"}),
        )
        .await;
    assert_eq!(s, 200, "{fresh}");
    assert_eq!(t.identity(&fresh).await.limits.rpm, Some(3));
    assert_eq!(
        t.identity(&fresh).await.limits.allowed_models,
        vec!["configured".to_owned()]
    );
    assert_eq!(t.identity(&old).await.limits.rpm, None);
    let task = mock_llm(&t, "0", 0).await;
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            old["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            fresh["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    task.abort();
}

// ---------------------------------------------------------------------------
// Stripe payments
// ---------------------------------------------------------------------------

/// A paid webhook grants the snapshot taken at checkout time and stays
/// idempotent across duplicate events and later plan edits/deactivation.
#[tokio::test]
async fn stripe_webhook_grants_checkout_snapshot_once() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap();
    t.app.config.write().await.stripe_webhook_secret = "whsec_test".into();
    sqlx::query("INSERT INTO payments(id,account_id,plan_code,price_micro,credit_micro,duration_days,session_id,created_at) VALUES('order_snap',?,'monthly',20000000,25000000,40,'cs_snap',?)")
        .bind(aid)
        .bind(db::now())
        .execute(&t.app.db)
        .await
        .unwrap();
    // The plan changes (and is deactivated) after checkout; the paid order must
    // still grant the snapshot the customer paid for.
    let (s, _) = t
        .admin(
            "POST",
            "plans",
            json!({"code":"monthly","name":"Monthly","price_usd":"20","credit_usd":"5","duration_days":10,"active":false}),
        )
        .await;
    assert_eq!(s, 200);
    let event = |id: &str| json!({"id":id,"type":"checkout.session.completed","data":{"object":{"id":"cs_snap","client_reference_id":"order_snap","metadata":{"order_id":"order_snap"},"mode":"payment","currency":"usd","amount_total":2000,"payment_status":"paid"}}});
    for id in ["evt_snap", "evt_snap", "evt_snap_retry"] {
        let body = event(id).to_string();
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
    assert_eq!(balance(&t, aid).await, "25.25");
    let subs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM subscriptions")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(subs, 1);
    let days =
        (billing::account(&t.app, aid).await.unwrap()["account"]["subscription"]["expires_at"]
            .as_i64()
            .unwrap()
            - db::now())
            / 86400;
    assert!(
        (39..=40).contains(&days),
        "snapshot duration must win: {days}"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM payments WHERE id='order_snap'")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(status, "paid");
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

// ---------------------------------------------------------------------------
// Panel journeys
// ---------------------------------------------------------------------------

/// One workspace journey through every customer tab action: sign up, open the
/// dashboard, add/block/rotate keys, redeem, get billed, and inspect history.
#[tokio::test]
async fn workspace_panel_full_journey() {
    let t = Test::new().await;
    let first = t.key().await;
    let aid = first["account_id"].as_str().unwrap().to_owned();
    let dash = first["dashboard_token"].as_str().unwrap().to_owned();
    assert_eq!(t.customer("GET", "account", &dash, json!({})).await.0, 200);

    let (s, second) = t
        .customer("POST", "keys", &dash, json!({"name":"second"}))
        .await;
    assert_eq!(s, 200, "{second}");
    let (s, _) = t
        .customer(
            "PATCH",
            &format!("keys/{}", first["id"].as_str().unwrap()),
            &dash,
            json!({"name":"primary","status":"blocked"}),
        )
        .await;
    assert_eq!(s, 200);
    // Rotation replaces an active key; the blocked one stays blocked.
    let (s, third) = t
        .customer(
            "POST",
            &format!("keys/{}/rotate", second["id"].as_str().unwrap()),
            &dash,
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{third}");
    let (s, keys) = t.customer("GET", "keys", &dash, json!({})).await;
    assert_eq!(s, 200);
    let keys = keys["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 3);

    let code = issue_credit_code(&t, "10").await;
    let (s, v) = redeem(&t, third["dashboard_token"].as_str().unwrap(), &code).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["account"]["balance_usd"], "10.25");

    let task = mock_llm(&t, "0.01", 30).await;
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            third["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(balance(&t, &aid).await, "10.237");
    let usage = t.customer("GET", "account/usage", &dash, json!({})).await.1;
    assert_eq!(usage["series"].as_array().unwrap().len(), 1);
    assert_eq!(usage["series"][0]["requests"], 1);
    let history = t
        .customer("GET", "account/requests?limit=5", &dash, json!({}))
        .await
        .1;
    assert_eq!(history["requests"].as_array().unwrap().len(), 1);
    let admin_requests = t.admin("GET", "requests", json!({})).await.1;
    assert_eq!(admin_requests["requests"].as_array().unwrap().len(), 1);
    assert_eq!(
        reseller::stats::overview(&t.app).await.unwrap()["billed_cost_usd"],
        "0.013"
    );

    // Revoking the active key kills that key's credentials; the account's
    // remaining (blocked) credentials can still open the workspace and mint a
    // replacement.
    let (s, _) = t
        .admin(
            "POST",
            &format!("keys/{}/revoke", third["id"].as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer(
            "GET",
            "account",
            third["dashboard_token"].as_str().unwrap(),
            json!({})
        )
        .await
        .0,
        401
    );
    let (s, recovery) = t
        .customer("POST", "keys", &dash, json!({"name":"recovery"}))
        .await;
    assert_eq!(s, 200, "{recovery}");
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            recovery["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    task.abort();
}

/// The administrator journey: inspect accounts, edit them, grant credit and a
/// plan, mint a limited key, then watch a request land in reports.
#[tokio::test]
async fn admin_console_management_journey() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    let (s, accounts) = t.admin("GET", "accounts", json!({})).await;
    assert_eq!(s, 200);
    assert_eq!(accounts["accounts"].as_array().unwrap().len(), 1);
    let (s, detail) = t.admin("GET", &format!("accounts/{aid}"), json!({})).await;
    assert_eq!(s, 200);
    assert_eq!(detail["keys"].as_array().unwrap().len(), 1);
    assert!(detail["usage"]["series"].is_array());

    let (s, v) = t
        .admin(
            "PATCH",
            &format!("accounts/{aid}"),
            json!({"contact":"ops@example.com","note":"vip","can_create_keys":true,"markup_pct":"55"}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["account"]["contact"], "ops@example.com");
    assert_eq!(v["account"]["can_create_keys"], true);
    let (s, v) = t
        .admin(
            "POST",
            &format!("accounts/{aid}/credits"),
            json!({"amount_usd":"5","expires_at":db::now()+3600,"note":"goodwill"}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["account"]["balance_usd"], "5.25");
    let (s, v) = t
        .admin(
            "POST",
            &format!("accounts/{aid}/subscriptions"),
            json!({"plan_code":"monthly"}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["account"]["balance_usd"], "25.25");
    assert_eq!(v["account"]["subscription"]["plan_code"], "monthly");
    let (s, managed) = t
        .admin(
            "POST",
            &format!("accounts/{aid}/keys"),
            json!({"name":"ops","limits":{"rpm":5}}),
        )
        .await;
    assert_eq!(s, 200, "{managed}");
    assert_eq!(managed["account_id"], aid);

    let task = mock_llm(&t, "0.01", 30).await;
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            managed["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    let (s, groups) = t
        .admin("GET", "requests/groups?field=model", json!({}))
        .await;
    assert_eq!(s, 200);
    assert_eq!(groups["groups"].as_array().unwrap().len(), 1);
    assert_eq!(groups["groups"][0]["requests"], 1);
    task.abort();
}

/// Usage and request history are scoped to the authenticated account, and
/// per-key filters cannot be pointed at another account's key.
#[tokio::test]
async fn usage_and_request_history_are_account_scoped() {
    let t = Test::new().await;
    let a = t.key().await;
    let b = t.key().await;
    let task = mock_llm(&t, "0.005", 10).await;
    for key in [&a, &b] {
        assert_eq!(
            t.customer(
                "POST",
                "chat/completions",
                key["key"].as_str().unwrap(),
                json!({"messages":[]})
            )
            .await
            .0,
            200
        );
    }
    let usage = t
        .customer(
            "GET",
            "account/usage",
            a["dashboard_token"].as_str().unwrap(),
            json!({}),
        )
        .await
        .1;
    assert_eq!(usage["series"].as_array().unwrap().len(), 1);
    assert_eq!(usage["series"][0]["requests"], 1);
    let (s, v) = t
        .customer(
            "GET",
            &format!("account/usage?key={}", b["id"].as_str().unwrap()),
            a["dashboard_token"].as_str().unwrap(),
            json!({}),
        )
        .await;
    assert_eq!(s, 404, "{v}");
    let mine = t
        .customer(
            "GET",
            &format!("account/requests?key={}", b["id"].as_str().unwrap()),
            a["dashboard_token"].as_str().unwrap(),
            json!({}),
        )
        .await
        .1;
    assert_eq!(mine["requests"].as_array().unwrap().len(), 0);
    let all = t.admin("GET", "requests", json!({})).await.1;
    assert_eq!(all["requests"].as_array().unwrap().len(), 2);
    let filtered = t
        .admin(
            "GET",
            &format!("requests?account={}", a["account_id"].as_str().unwrap()),
            json!({}),
        )
        .await
        .1;
    assert_eq!(filtered["requests"].as_array().unwrap().len(), 1);
    task.abort();
}

/// A plan redeem code is single use even when redeemed concurrently.
#[tokio::test]
async fn concurrent_plan_code_redemption_grants_once() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    let code = issue_plan_code(&t, "monthly").await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let app = t.app.clone();
        let aid = aid.clone();
        let code = code.clone();
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
    let subs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM subscriptions")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(subs, 1);
    assert_eq!(balance(&t, &aid).await, "20.25");
}

/// Regression: a real HTTP server must meter a buffered JSON response. The
/// upstream sends Content-Length; if the proxy copies that header, hyper stops
/// polling the relay stream after the declared bytes and drops the meter,
/// settling the request as 499 with zero usage while the client still receives
/// the full body.
#[tokio::test]
async fn live_http_settles_buffered_json_responses() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    let upstream = mock_llm(&t, "0.01", 30).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = reseller::web::router(t.app.clone()).await;
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .bearer_auth(k["key"].as_str().unwrap())
        .json(&json!({"messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(
        response.headers().get("content-length").is_none(),
        "relayed bodies must be chunked so the meter is polled to completion"
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["usage"]["total_tokens"], 30);
    let v = reseller::stats::overview(&t.app).await.unwrap();
    assert_eq!(v["billed_cost_usd"], "0.013");
    assert_eq!(v["tokens"], 30);
    let r = sqlx::query("SELECT status,billed_micro,tokens,finished FROM requests")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(r.get::<i64, _>("status"), 200);
    assert_eq!(r.get::<i64, _>("billed_micro"), 13_000);
    assert_eq!(r.get::<i64, _>("tokens"), 30);
    assert_eq!(r.get::<i64, _>("finished"), 1);
    assert_eq!(balance(&t, &aid).await, "0.237");
    server.abort();
    upstream.abort();
}

/// The fresh-schema seed ships a Yearly plan at $160 for 360 days, and the
/// plan redeems into a credit lot with the matching expiry window.
#[tokio::test]
async fn year_plan_is_seeded_at_160_usd_for_360_days() {
    let t = Test::new().await;
    let plans = t.call("GET", "/v1/plans", None, json!({})).await.1;
    let year = plans["plans"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["code"] == "year")
        .expect("year plan is seeded");
    assert_eq!(year["name"], "Yearly");
    assert_eq!(year["price_usd"], "160");
    assert_eq!(year["credit_usd"], "160");
    assert_eq!(year["duration_days"], 360);
    assert_eq!(year["active"], true);

    let code = issue_plan_code(&t, "year").await;
    let k = t.key().await;
    let (s, v) = redeem(&t, k["dashboard_token"].as_str().unwrap(), &code).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["account"]["balance_usd"], "160.25");
    let days = (v["account"]["subscription"]["expires_at"].as_i64().unwrap() - db::now()) / 86400;
    assert!((359..=360).contains(&days), "year plan expiry: {days} days");
}

// ---------------------------------------------------------------------------
// Adversarial multi-step cases: debt through the proxy, concurrency,
// privilege boundaries, and checkout/webhook validation.
// ---------------------------------------------------------------------------

/// The full proxy path must drain the soonest-expiring lot first, and an
/// expiry event must only remove the expired remainder.
#[tokio::test]
async fn proxy_usage_drains_soonest_expiring_lot_first() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    sqlx::query("DELETE FROM credits")
        .execute(&t.app.db)
        .await
        .unwrap();
    let mut tx = t.app.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    billing::credit(&mut tx, &aid, 20_000, "purchase", None, "")
        .await
        .unwrap();
    billing::credit(
        &mut tx,
        &aid,
        20_000,
        "subscription",
        Some(db::now() + 30 * 86400),
        "monthly",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let task = mock_llm(&t, "0.01", 0).await;
    // billed = 0.013
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    let sub: i64 =
        sqlx::query_scalar("SELECT remaining_micro FROM credits WHERE source='subscription'")
            .fetch_one(&t.app.db)
            .await
            .unwrap();
    let purchase: i64 =
        sqlx::query_scalar("SELECT remaining_micro FROM credits WHERE source='purchase'")
            .fetch_one(&t.app.db)
            .await
            .unwrap();
    assert_eq!(sub, 7_000);
    assert_eq!(purchase, 20_000);
    sqlx::query("UPDATE credits SET expires_at=? WHERE source='subscription'")
        .bind(db::now() - 1)
        .execute(&t.app.db)
        .await
        .unwrap();
    assert_eq!(balance(&t, &aid).await, "0.02");
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    assert_eq!(balance(&t, &aid).await, "0.007");
    task.abort();
}

/// Overspending one admitted request becomes debt; the next request is
/// refused until an admin top-up settles the debt and exposes the surplus.
#[tokio::test]
async fn overspend_via_proxy_creates_debt_settled_by_admin_topup() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    sqlx::query("DELETE FROM credits")
        .execute(&t.app.db)
        .await
        .unwrap();
    let (s, _) = t
        .admin(
            "POST",
            &format!("accounts/{aid}/credits"),
            json!({"amount_usd":"0.01"}),
        )
        .await;
    assert_eq!(s, 200);
    let task = mock_llm(&t, "0.01", 0).await;
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    let v = billing::account(&t.app, &aid).await.unwrap();
    assert_eq!(v["account"]["balance_usd"], "-0.003");
    assert_eq!(v["account"]["debt_usd"], "0.003");
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 402, "{v}");
    let (s, _) = t
        .admin(
            "POST",
            &format!("accounts/{aid}/credits"),
            json!({"amount_usd":"1"}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(balance(&t, &aid).await, "0.997");
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    assert_eq!(balance(&t, &aid).await, "0.984");
    task.abort();
}

/// Concurrent rotations of active keys are serialized: every original is
/// replaced once, blocked originals cannot be rotated again, and the active
/// count never exceeds the account cap.
#[tokio::test]
async fn concurrent_rotations_are_serialized_and_only_active_keys_rotate() {
    let t = Test::new().await;
    t.app.config.write().await.max_keys_per_account = 3;
    let k1 = t.key().await;
    let dash = k1["dashboard_token"].as_str().unwrap().to_owned();
    let (s, k2) = t
        .customer("POST", "keys", &dash, json!({"name":"second"}))
        .await;
    assert_eq!(s, 200, "{k2}");
    let (s, k3) = t
        .customer("POST", "keys", &dash, json!({"name":"third"}))
        .await;
    assert_eq!(s, 200, "{k3}");
    let ids = [
        k1["id"].as_str().unwrap().to_owned(),
        k2["id"].as_str().unwrap().to_owned(),
        k3["id"].as_str().unwrap().to_owned(),
    ];
    let mut tasks = Vec::new();
    for kid in &ids {
        let app = t.app.clone();
        let kid = kid.clone();
        tasks.push(tokio::spawn(async move {
            identity::rotate(&app, &kid, None).await
        }));
    }
    let mut successes = 0;
    for task in tasks {
        if task.await.unwrap().is_ok() {
            successes += 1;
        }
    }
    assert_eq!(successes, 3, "each active original rotates exactly once");
    let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE status='active'")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(active, 3, "the account cap is preserved");
    // Retrying against a replaced (blocked) original is refused and mints nothing.
    assert!(identity::rotate(&t.app, &ids[0], None).await.is_err());
    let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE status='active'")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(active, 3);
}

/// Blocked keys are paused credentials and cannot be rotated into a fresh
/// active key — for customers or admins — until they are reactivated.
#[tokio::test]
async fn only_active_keys_can_be_rotated() {
    let t = Test::new().await;
    let k = t.key().await;
    let kid = k["id"].as_str().unwrap().to_owned();
    let token = k["dashboard_token"].as_str().unwrap();
    let (s, _) = t
        .customer(
            "PATCH",
            &format!("keys/{kid}"),
            token,
            json!({"status":"blocked"}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, v) = t
        .customer("POST", &format!("keys/{kid}/rotate"), token, json!({}))
        .await;
    assert_eq!(s, 400, "{v}");
    assert_eq!(v["error"]["code"], "key_not_active");
    assert_eq!(
        t.admin("POST", &format!("keys/{kid}/rotate"), json!({}))
            .await
            .0,
        400
    );
    // Reactivation restores rotation; the replacement then inherits the
    // blocked state of the key it replaced.
    assert_eq!(
        t.admin("PATCH", &format!("keys/{kid}"), json!({"status":"active"}))
            .await
            .0,
        200
    );
    let (s, rotated) = t
        .customer("POST", &format!("keys/{kid}/rotate"), token, json!({}))
        .await;
    assert_eq!(s, 200, "{rotated}");
    assert_eq!(
        t.customer("POST", &format!("keys/{kid}/rotate"), token, json!({}))
            .await
            .0,
        400
    );
}

/// Customer key patches may rename and block, but must never change limits,
/// markup, or expiry; admins validate the values they set.
#[tokio::test]
async fn customer_patches_cannot_escalate_limits_markup_or_expiry() {
    let t = Test::new().await;
    let k = t.key().await;
    let kid = k["id"].as_str().unwrap().to_owned();
    let expiry = db::now() + 3600;
    let (s, _) = t
        .admin(
            "PATCH",
            &format!("keys/{kid}"),
            json!({"limits":{"rpm":2},"markup_pct":"40","expires_at":expiry}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, v) = t
        .customer(
            "PATCH",
            &format!("keys/{kid}"),
            k["dashboard_token"].as_str().unwrap(),
            json!({"name":"renamed","limits":{"rpm":9999},"markup_pct":"0","expires_at":null}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let fresh = t.identity(&k).await;
    assert_eq!(fresh.limits.rpm, Some(2));
    assert_eq!(fresh.markup, "40");
    let r = sqlx::query("SELECT name,status,expires_at FROM api_keys WHERE id=?")
        .bind(&kid)
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(r.get::<String, _>("name"), "renamed");
    assert_eq!(r.get::<String, _>("status"), "active");
    assert_eq!(r.get::<Option<i64>, _>("expires_at"), Some(expiry));
    assert_eq!(
        t.customer(
            "PATCH",
            &format!("keys/{kid}"),
            k["dashboard_token"].as_str().unwrap(),
            json!({"status":"active"}),
        )
        .await
        .0,
        400
    );
    assert_eq!(
        t.admin(
            "PATCH",
            &format!("keys/{kid}"),
            json!({"limits":{"requests":{"max":0,"window_hours":24}}}),
        )
        .await
        .0,
        400
    );
}

/// Public signup cannot select its own limits; defaults from settings apply.
#[tokio::test]
async fn public_signup_cannot_choose_limits() {
    let t = Test::new().await;
    let (s, v) = t
        .call(
            "POST",
            "/v1/keys",
            None,
            json!({"name":"anonymous","limits":{"rpm":1,"max_children":99}}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let limits: Value = serde_json::from_str(
        sqlx::query_scalar::<_, String>("SELECT limits FROM api_keys WHERE id=?")
            .bind(v["id"].as_str())
            .fetch_one(&t.app.db)
            .await
            .unwrap()
            .as_str(),
    )
    .unwrap();
    assert!(limits["rpm"].is_null());
    assert!(limits["max_children"].is_null());
}

/// Checkout stays unavailable without Stripe configuration and rejects
/// missing, unknown, below-minimum, or sub-cent plans once configured.
#[tokio::test]
async fn checkout_validates_configuration_and_plan_pricing() {
    let t = Test::new().await;
    let k = t.key().await;
    let token = k["dashboard_token"].as_str().unwrap();
    let (s, v) = t
        .customer(
            "POST",
            "account/checkout",
            token,
            json!({"plan_code":"monthly"}),
        )
        .await;
    assert_eq!(s, 503, "{v}");
    assert_eq!(v["error"]["code"], "payments_unavailable");
    let (s, _) = t
        .admin(
            "PATCH",
            "settings",
            json!({"stripe_secret_key":"sk_test_123","stripe_webhook_secret":"whsec_123"}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer("POST", "account/checkout", token, json!({}))
            .await
            .0,
        400
    );
    assert_eq!(
        t.customer(
            "POST",
            "account/checkout",
            token,
            json!({"plan_code":"nope"})
        )
        .await
        .0,
        400
    );
    let (s, _) = t
        .admin(
            "POST",
            "plans",
            json!({"code":"tiny","name":"Tiny","price_usd":"0.40","credit_usd":"0.40","duration_days":10,"active":true}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer(
            "POST",
            "account/checkout",
            token,
            json!({"plan_code":"tiny"})
        )
        .await
        .0,
        400
    );
    let (s, _) = t
        .admin(
            "POST",
            "plans",
            json!({"code":"odd","name":"Odd","price_usd":"2.005","credit_usd":"2","duration_days":10,"active":true}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer(
            "POST",
            "account/checkout",
            token,
            json!({"plan_code":"odd"})
        )
        .await
        .0,
        400
    );
    // The public plan list only exposes active plans; the console shows all.
    let (s, _) = t
        .admin(
            "POST",
            "plans",
            json!({"code":"monthly","name":"Monthly","price_usd":"20","credit_usd":"20","duration_days":30,"active":false}),
        )
        .await;
    assert_eq!(s, 200);
    let public = t.call("GET", "/v1/plans", None, json!({})).await.1;
    assert!(
        !public["plans"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["code"] == "monthly")
    );
    let admin = t.admin("GET", "plans", json!({})).await.1;
    assert!(
        admin["plans"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["code"] == "monthly" && p["active"] == false)
    );
}

/// Unpaid and unknown-order webhooks leave no events or credit behind, while a
/// later async success for a real order is granted exactly once.
#[tokio::test]
async fn webhook_ignores_unpaid_and_unknown_orders_without_side_effects() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap();
    t.app.config.write().await.stripe_webhook_secret = "whsec_test".into();
    let send = |event: Value| {
        let app = t.app.clone();
        async move {
            let body = event.to_string();
            let mut h = HeaderMap::new();
            h.insert(
                "stripe-signature",
                signature("whsec_test", body.as_bytes(), db::now())
                    .parse()
                    .unwrap(),
            );
            reseller::payments::webhook(&app, &h, body.as_bytes()).await
        }
    };
    let unpaid = json!({"id":"evt_unpaid","type":"checkout.session.completed","data":{"object":{"id":"cs_ghost","client_reference_id":"ghost","metadata":{"order_id":"ghost"},"mode":"payment","currency":"usd","amount_total":2000,"payment_status":"unpaid"}}});
    send(unpaid).await.unwrap();
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payment_events")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(events, 0);
    let unknown = json!({"id":"evt_ghost","type":"checkout.session.completed","data":{"object":{"id":"cs_ghost","client_reference_id":"ghost","metadata":{"order_id":"ghost"},"mode":"payment","currency":"usd","amount_total":2000,"payment_status":"paid"}}});
    assert!(send(unknown).await.is_err());
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payment_events")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(events, 0);
    assert_eq!(balance(&t, aid).await, "0.25");

    sqlx::query("INSERT INTO payments(id,account_id,plan_code,price_micro,credit_micro,duration_days,session_id,created_at) VALUES('order_async',?,'monthly',20000000,10000000,15,'cs_async',?)")
        .bind(aid)
        .bind(db::now())
        .execute(&t.app.db)
        .await
        .unwrap();
    let async_paid = json!({"id":"evt_async","type":"checkout.session.async_payment_succeeded","data":{"object":{"id":"cs_async","client_reference_id":"order_async","metadata":{"order_id":"order_async"},"mode":"payment","currency":"usd","amount_total":2000,"payment_status":"paid"}}});
    send(async_paid).await.unwrap();
    assert_eq!(balance(&t, aid).await, "10.25");
    let subs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM subscriptions")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(subs, 1);
}

/// Administrators cannot hand out plans that are deactivated or missing.
#[tokio::test]
async fn admin_cannot_grant_inactive_plan() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap();
    let (s, _) = t
        .admin(
            "POST",
            "plans",
            json!({"code":"monthly","name":"Monthly","price_usd":"20","credit_usd":"20","duration_days":30,"active":false}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, v) = t
        .admin(
            "POST",
            &format!("accounts/{aid}/subscriptions"),
            json!({"plan_code":"monthly"}),
        )
        .await;
    assert_eq!(s, 400, "{v}");
    let (s, _) = t
        .admin(
            "POST",
            "plans",
            json!({"code":"monthly","name":"Monthly","price_usd":"20","credit_usd":"20","duration_days":30,"active":true}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.admin(
            "POST",
            &format!("accounts/{aid}/subscriptions"),
            json!({"plan_code":"monthly"}),
        )
        .await
        .0,
        200
    );
}

/// A blocked (for example, rotated-away) credential can still open the
/// workspace and mint brand-new active keys; only revocation removes that
/// power. This documents the containment boundary of "block" vs "revoke".
#[tokio::test]
async fn blocked_credential_can_still_mint_replacement_keys() {
    let t = Test::new().await;
    let k = t.key().await;
    let kid = k["id"].as_str().unwrap().to_owned();
    let (s, rotated) = t
        .customer(
            "POST",
            &format!("keys/{kid}/rotate"),
            k["dashboard_token"].as_str().unwrap(),
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{rotated}");
    let (s, v) = t
        .customer(
            "POST",
            "chat/completions",
            k["key"].as_str().unwrap(),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 401, "{v}");
    // The blocked credential still creates a fresh, usable key.
    let (s, rescue) = t
        .customer(
            "POST",
            "keys",
            k["key"].as_str().unwrap(),
            json!({"name":"rescue"}),
        )
        .await;
    assert_eq!(s, 200, "{rescue}");
    let task = mock_llm(&t, "0", 0).await;
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            rescue["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    // Revocation is the operation that actually cuts the credential off.
    let (s, _) = t
        .admin("POST", &format!("keys/{kid}/revoke"), json!({}))
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer(
            "POST",
            "keys",
            k["key"].as_str().unwrap(),
            json!({"name":"after revoke"})
        )
        .await
        .0,
        401
    );
    task.abort();
}

/// Revoking a parent key does not revoke keys it created: revocation is per
/// credential, while suspending the account contains the whole tree.
#[tokio::test]
async fn revoking_parent_leaves_children_active_until_account_suspension() {
    let t = Test::new().await;
    let parent = t.key().await;
    let aid = parent["account_id"].as_str().unwrap().to_owned();
    let (s, child) = t
        .customer(
            "POST",
            "keys",
            parent["key"].as_str().unwrap(),
            json!({"name":"child"}),
        )
        .await;
    assert_eq!(s, 200, "{child}");
    let (s, _) = t
        .admin(
            "POST",
            &format!("keys/{}/revoke", parent["id"].as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!(s, 200);
    let task = mock_llm(&t, "0", 0).await;
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            child["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    // Account suspension is the containment mechanism for the whole tree.
    let (s, _) = t
        .admin(
            "PATCH",
            &format!("accounts/{aid}"),
            json!({"status":"suspended"}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            child["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        403
    );
    task.abort();
}

/// Usage windows are per key: one key hitting its cap does not throttle a
/// sibling key on the same shared balance.
#[tokio::test]
async fn usage_limits_are_per_key_within_one_account() {
    let t = Test::new().await;
    let a = t.key().await;
    let (s, b) = t
        .customer(
            "POST",
            "keys",
            a["dashboard_token"].as_str().unwrap(),
            json!({"name":"sibling"}),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    for key in [&a, &b] {
        let (s, _) = t
            .admin(
                "PATCH",
                &format!("keys/{}", key["id"].as_str().unwrap()),
                json!({"limits":{"requests":{"max":1,"window_hours":24}}}),
            )
            .await;
        assert_eq!(s, 200);
    }
    let task = mock_llm(&t, "0", 0).await;
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            a["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            a["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        429
    );
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            b["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        t.customer(
            "POST",
            "chat/completions",
            b["key"].as_str().unwrap(),
            json!({"messages":[]})
        )
        .await
        .0,
        429
    );
    task.abort();
}

/// Plans can be created, edited in place, retired, and deleted — but deletion
/// is only allowed while no subscription, payment, or code references them.
#[tokio::test]
async fn plan_lifecycle_edits_retires_and_deletes_safely() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();

    // Create, then update in place and observe the new terms.
    let (s, _) = t
        .admin(
            "POST",
            "plans",
            json!({"code":"starter","name":"Starter","price_usd":"10","credit_usd":"10","duration_days":30,"active":true,"description":"first"}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, plans) = t
        .admin(
            "POST",
            "plans",
            json!({"code":"starter","name":"Starter v2","price_usd":"12","credit_usd":"11","duration_days":31,"active":true,"description":"second"}),
        )
        .await;
    assert_eq!(s, 200);
    let starter = plans["plans"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["code"] == "starter")
        .unwrap();
    assert_eq!(starter["name"], "Starter v2");
    assert_eq!(starter["price_usd"], "12");
    assert_eq!(starter["credit_usd"], "11");
    assert_eq!(starter["duration_days"], 31);

    // Unreferenced: deletion succeeds.
    let (s, v) = t.admin("DELETE", "plans/starter", json!({})).await;
    assert_eq!(s, 200, "{v}");
    assert!(
        !v["plans"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["code"] == "starter")
    );

    // Referenced by an unredeemed code: refused, then cancel the code and delete.
    let (s, _) = t
        .admin(
            "POST",
            "plans",
            json!({"code":"starter","name":"Starter","price_usd":"10","credit_usd":"10","duration_days":30,"active":true,"description":""}),
        )
        .await;
    assert_eq!(s, 200);
    let _code = issue_plan_code(&t, "starter").await;
    let (s, v) = t.admin("DELETE", "plans/starter", json!({})).await;
    assert_eq!(s, 409, "{v}");
    assert_eq!(v["error"]["code"], "plan_in_use");
    let codes = t.admin("GET", "codes", json!({})).await.1;
    let cid = codes["codes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["plan_code"] == "starter")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        t.admin("DELETE", &format!("codes/{cid}"), json!({}))
            .await
            .0,
        200
    );
    assert_eq!(t.admin("DELETE", "plans/starter", json!({})).await.0, 200);

    // Referenced by a subscription: deactivate to retire it, deletion stays refused.
    let (s, _) = t
        .admin(
            "POST",
            "plans",
            json!({"code":"starter","name":"Starter","price_usd":"10","credit_usd":"10","duration_days":30,"active":true,"description":""}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, v) = t
        .admin(
            "POST",
            &format!("accounts/{aid}/subscriptions"),
            json!({"plan_code":"starter"}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(t.admin("DELETE", "plans/starter", json!({})).await.0, 409);
    let (s, _) = t
        .admin(
            "POST",
            "plans",
            json!({"code":"starter","name":"Starter","price_usd":"10","credit_usd":"10","duration_days":30,"active":false,"description":""}),
        )
        .await;
    assert_eq!(s, 200);
    let public = t.call("GET", "/v1/plans", None, json!({})).await.1;
    assert!(
        !public["plans"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["code"] == "starter")
    );
    let admin_plans = t.admin("GET", "plans", json!({})).await.1;
    assert!(
        admin_plans["plans"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["code"] == "starter" && p["active"] == false)
    );
    assert_eq!(t.admin("DELETE", "plans/starter", json!({})).await.0, 409);
}

/// Unredeemed codes can be edited (expiry/note) or cancelled; redeemed codes
/// are frozen audit records and the granted credit is untouched.
#[tokio::test]
async fn redeem_codes_can_be_edited_or_cancelled_until_redeemed() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    let token = k["dashboard_token"].as_str().unwrap().to_owned();

    let code = issue_credit_code(&t, "10").await;
    let codes = t.admin("GET", "codes", json!({})).await.1;
    let id = codes["codes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["code_prefix"] == code[..12])
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Edit expiry and note; a past expiry is rejected and changes nothing.
    let (s, v) = t
        .admin(
            "PATCH",
            &format!("codes/{id}"),
            json!({"expires_at":db::now()+3600,"note":"vip"}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["code"]["note"], "vip");
    assert_eq!(
        t.admin(
            "PATCH",
            &format!("codes/{id}"),
            json!({"expires_at":db::now()-1})
        )
        .await
        .0,
        400
    );

    // Redeem, then the record is frozen.
    let (s, v) = redeem(&t, &token, &code).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(balance(&t, &aid).await, "10.25");
    assert_eq!(
        t.admin("PATCH", &format!("codes/{id}"), json!({"note":"late"}))
            .await
            .0,
        400
    );
    assert_eq!(
        t.admin("DELETE", &format!("codes/{id}"), json!({})).await.0,
        400
    );
    assert_eq!(balance(&t, &aid).await, "10.25");

    // An unredeemed code can be cancelled and then no longer redeems.
    let code2 = issue_credit_code(&t, "5").await;
    let codes = t.admin("GET", "codes", json!({})).await.1;
    let id2 = codes["codes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["code_prefix"] == code2[..12])
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        t.admin("DELETE", &format!("codes/{id2}"), json!({}))
            .await
            .0,
        200
    );
    let (s, v) = redeem(&t, &token, &code2).await;
    assert_eq!(s, 400, "{v}");
    assert_eq!(balance(&t, &aid).await, "10.25");
}

/// Codes can be issued in atomic batches: mixed definitions produce unique
/// single-use codes, one invalid entry rejects the whole batch, and the batch
/// size is capped at 100.
#[tokio::test]
async fn redeem_codes_can_be_issued_in_bulk_atomically() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    let token = k["dashboard_token"].as_str().unwrap().to_owned();

    let (s, v) = t
        .admin(
            "POST",
            "codes",
            json!([
                {"credit_usd":"5","note":"batch"},
                {"credit_usd":"7.5","expires_at":db::now()+3600},
                {"plan_code":"monthly"}
            ]),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let codes: Vec<String> = v["codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(codes.len(), 3);
    assert_eq!(v["count"], 3);
    let unique: std::collections::HashSet<_> = codes.iter().collect();
    assert_eq!(unique.len(), 3);
    for code in &codes {
        assert!(code.starts_with("RES-"), "{code}");
        assert_eq!(redeem(&t, &token, code).await.0, 200, "{code}");
        assert_eq!(redeem(&t, &token, code).await.0, 400, "reuse of {code}");
    }
    assert_eq!(balance(&t, &aid).await, "32.75");

    // One invalid entry rejects the whole batch without inserting anything.
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM redeem_codes")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    let (s, v) = t
        .admin(
            "POST",
            "codes",
            json!([{"credit_usd":"5"},{"credit_usd":"-1"}]),
        )
        .await;
    assert_eq!(s, 400, "{v}");
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM redeem_codes")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(after, before);

    // The single-object form keeps its response shape.
    let (s, v) = t.admin("POST", "codes", json!({"credit_usd":"1"})).await;
    assert_eq!(s, 200, "{v}");
    assert!(v["code"].as_str().unwrap().starts_with("RES-"));

    // The batch size is capped.
    let many: Vec<Value> = (0..101).map(|_| json!({"credit_usd":"1"})).collect();
    let (s, v) = t.admin("POST", "codes", json!(many)).await;
    assert_eq!(s, 400, "{v}");
    let final_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM redeem_codes")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(final_count, after + 1);
}

/// Model policy semantics: "default" fills only a missing model, "force"
/// always overrides the client, and "passthrough" forwards whatever the client
/// sent — even a placeholder — without substitution.
#[tokio::test]
async fn model_policy_default_force_and_passthrough() {
    let t = Test::new().await;
    let (base, task) = upstream(Router::new().fallback(any(|req: Request<Body>| async move {
        let v: Value =
            serde_json::from_slice(&to_bytes(req.into_body(), 1_000_000).await.unwrap()).unwrap();
        axum::Json(json!({"model": v.get("model").cloned().unwrap_or(Value::Null)}))
    })))
    .await;
    {
        let mut c = t.app.config.write().await;
        c.llm.base_url = format!("{base}/v1");
        c.llm.api_key = "upstream".into();
        c.llm.model = "configured".into();
        c.model_policy = Policy::Default;
    }
    let k = t.key().await;
    let key = k["key"].as_str().unwrap();

    // default + missing model -> the configured model is injected.
    let (s, v) = t
        .call(
            "POST",
            "/v1/chat/completions",
            Some(key),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["model"], "configured");

    // default + explicit unknown model -> the client's value is forwarded.
    let (s, v) = t
        .call(
            "POST",
            "/v1/chat/completions",
            Some(key),
            json!({"model":"your-model","messages":[]}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["model"], "your-model");

    // passthrough + explicit model -> untouched.
    t.app.config.write().await.model_policy = Policy::Passthrough;
    let (s, v) = t
        .call(
            "POST",
            "/v1/chat/completions",
            Some(key),
            json!({"model":"your-model","messages":[]}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["model"], "your-model");

    // passthrough + missing model -> still missing upstream.
    let (s, v) = t
        .call(
            "POST",
            "/v1/chat/completions",
            Some(key),
            json!({"messages":[]}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert!(v["model"].is_null(), "{v}");

    // force + explicit model -> always the configured model.
    t.app.config.write().await.model_policy = Policy::Force;
    let (s, v) = t
        .call(
            "POST",
            "/v1/chat/completions",
            Some(key),
            json!({"model":"your-model","messages":[]}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["model"], "configured");
    task.abort();
}

/// Request history exposes a filtered total, page-size independent pagination,
/// range status filters, and per-account scoping.
#[tokio::test]
async fn request_history_totals_filters_and_scoping() {
    let t = Test::new().await;
    let k = t.key().await;
    let aid = k["account_id"].as_str().unwrap().to_owned();
    let kid = k["id"].as_str().unwrap().to_owned();
    let token = k["dashboard_token"].as_str().unwrap().to_owned();
    let other = t.key().await;
    let other_aid = other["account_id"].as_str().unwrap().to_owned();
    let other_kid = other["id"].as_str().unwrap().to_owned();
    let now = db::now();
    let rows = [
        ("r1", &aid, &kid, 200, "llm", "m-one", 1, 10_000, None),
        (
            "r2",
            &aid,
            &kid,
            500,
            "llm",
            "m-one",
            1,
            20_000,
            Some("upstream"),
        ),
        ("r3", &aid, &kid, 201, "tts", "m-two", 1, 5_000, None),
        ("r4", &aid, &kid, 0, "stt", "m-two", 0, 0, None),
        (
            "r5", &other_aid, &other_kid, 200, "llm", "m-other", 1, 1_000, None,
        ),
    ];
    for (i, (id, account, key, status, kind, model, finished, billed, error)) in
        rows.into_iter().enumerate()
    {
        sqlx::query("INSERT INTO requests(id,account_id,key_id,created_at,method,path,kind,model,ip,status,duration_ms,bytes_in,bytes_out,tokens,upstream_micro,billed_micro,error,finished) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(id)
            .bind(account)
            .bind(key)
            .bind(now - i as i64)
            .bind("POST")
            .bind("/v1/chat/completions")
            .bind(kind)
            .bind(model)
            .bind("127.0.0.1")
            .bind(status)
            .bind(12)
            .bind(64)
            .bind(128)
            .bind(30)
            .bind(10_000)
            .bind(billed)
            .bind(error)
            .bind(finished)
            .execute(&t.app.db)
            .await
            .unwrap();
    }

    // Admin sees everything, newest first, with a total.
    let (s, v) = t.admin("GET", "requests", json!({})).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["total"], 5);
    assert_eq!(v["requests"].as_array().unwrap().len(), 5);
    assert_eq!(v["requests"][0]["id"], "r1");
    assert_eq!(v["requests"][0]["upstream_usd"], "0.01");
    assert_eq!(v["requests"][1]["error"], "upstream");

    // Status filters accept ranges and exact codes.
    for (filter, total) in [
        ("2xx", 3),
        ("4xx", 0),
        ("5xx", 1),
        ("200", 2),
        ("pending", 1),
    ] {
        let (s, v) = t
            .admin("GET", &format!("requests?status={filter}"), json!({}))
            .await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["total"], total, "status={filter}");
    }
    // Kind, model, search, and account filters narrow both rows and total.
    for (query, total) in [
        ("kind=tts", 1),
        ("model=m-one", 2),
        ("q=r3", 1),
        ("ip=127.0.0.1", 5),
        (&format!("account={aid}"), 4),
    ] {
        let (s, v) = t
            .admin("GET", &format!("requests?{query}"), json!({}))
            .await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["total"], total, "{query}");
    }
    // Pagination keeps the unfiltered total while returning one page.
    let (s, v) = t.admin("GET", "requests?limit=2&offset=2", json!({})).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["total"], 5);
    assert_eq!(v["requests"].as_array().unwrap().len(), 2);
    assert_eq!(v["limit"], 2);
    assert_eq!(v["offset"], 2);
    assert_eq!(v["requests"][0]["id"], "r3");

    // Customers only see their own account, even when trying another id.
    let (s, v) = t
        .customer("GET", "account/requests", &token, json!({}))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["total"], 4);
    let (s, v) = t
        .customer(
            "GET",
            &format!("account/requests?account={other_aid}"),
            &token,
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["total"], 0);

    // Grouped views still work.
    let (s, v) = t
        .admin("GET", "requests/groups?field=model", json!({}))
        .await;
    assert_eq!(s, 200, "{v}");
    assert!(!v["groups"].as_array().unwrap().is_empty());
}
