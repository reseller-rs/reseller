//! Abuse protection: proof-of-work signup, key creation budgets and cooldowns,
//! authentication failure lockouts, rate limits, and the HSTS header, which is
//! sent only when public_url uses https.
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{HeaderMap, Request},
};
use reseller::{
    App,
    config::{Config, KeyPolicy, RateLimits, Signup},
    db, identity,
    meter::{Meter, RequestMeta},
    security::pow,
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use tower::ServiceExt;

const ADMIN: &str = "admin-test-secret-123456789";
const PEPPER: &str = "pepper-test-secret-12345678";

struct Reply {
    status: u16,
    body: Value,
    headers: HeaderMap,
}

struct Test {
    app: App,
    _dir: tempfile::TempDir,
}

impl Test {
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    /// Open signup (no proof of work) with the abuse limits that are not
    /// under test disabled.
    async fn with(tweak: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(&dir.path().join("test.db")).await.unwrap();
        let mut c = Config {
            admin_token: ADMIN.into(),
            key_pepper: PEPPER.into(),
            signup: Signup {
                cooldown_seconds: 0,
                ip_limit_per_month: 0,
                global_hourly_limit: 0,
                pow_max_number: 0,
                ..Signup::default()
            },
            keys: KeyPolicy {
                rotate_cooldown_seconds: 0,
                max_new_per_day: 0,
                ..KeyPolicy::default()
            },
            rate_limits: RateLimits {
                management_per_minute: 0,
                auth_failures_per_hour: 0,
                max_concurrent_requests: 0,
                checkouts_per_hour: 0,
                redeem_failures_per_hour: 0,
                challenges_per_minute: 0,
            },
            ..Config::default()
        };
        tweak(&mut c);
        let c = db::settings(&pool, c).await.unwrap();
        Self {
            app: App::new(pool, c).unwrap(),
            _dir: dir,
        }
    }

    async fn call_from(
        &self,
        ip: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> Reply {
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
        req.extensions_mut().insert(ConnectInfo(
            format!("{ip}:9000").parse::<SocketAddr>().unwrap(),
        ));
        let r = router.oneshot(req).await.unwrap();
        let status = r.status().as_u16();
        let headers = r.headers().clone();
        let b = to_bytes(r.into_body(), 10 * 1024 * 1024).await.unwrap();
        Reply {
            status,
            body: serde_json::from_slice(&b).unwrap_or(Value::Null),
            headers,
        }
    }

    async fn call(&self, method: &str, path: &str, token: Option<&str>, body: Value) -> Reply {
        self.call_from("127.0.0.1", method, path, token, body).await
    }

    /// Opens an account through `POST /v1/signup` (proof of work disabled).
    async fn key(&self) -> Value {
        let r = self
            .call("POST", "/v1/signup", None, json!({"name":"t"}))
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
        r.body
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

fn code(r: &Reply) -> &str {
    r.body["error"]["code"].as_str().unwrap_or("")
}

fn retry_after(r: &Reply) -> u64 {
    r.headers["retry-after"].to_str().unwrap().parse().unwrap()
}

// --- proof-of-work signup ----------------------------------------------------

#[tokio::test]
async fn signup_with_solved_challenge_over_http_and_replay_is_refused() {
    let t = Test::with(|c| c.signup.pow_max_number = 2000).await;
    let r = t.call("GET", "/v1/signup/challenge", None, json!({})).await;
    assert_eq!(r.status, 200);
    let challenge: pow::Challenge = serde_json::from_value(r.body.clone()).unwrap();
    assert_eq!(challenge.algorithm, "SHA-256");
    assert_eq!(challenge.maxnumber, 2000);
    assert!(challenge.salt.contains("?expires="));
    let altcha = pow::encode(&challenge, pow::solve(&challenge).unwrap());

    let ok = t
        .call(
            "POST",
            "/v1/signup",
            None,
            json!({"name":"pow","altcha":altcha}),
        )
        .await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    assert!(ok.body["key"].as_str().unwrap().starts_with("sk-res-"));
    assert!(ok.body["dashboard_token"].is_string());

    let replay = t
        .call("POST", "/v1/signup", None, json!({"altcha":altcha}))
        .await;
    assert_eq!((replay.status, code(&replay)), (409, "pow_replayed"));
    let spent: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pow_spent")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(spent, 1);
    let accounts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(accounts, 1);
}

#[tokio::test]
async fn signup_rejects_missing_garbage_expired_and_foreign_proofs() {
    let t = Test::with(|c| c.signup.pow_max_number = 300).await;
    let missing = t.call("POST", "/v1/signup", None, json!({})).await;
    assert_eq!((missing.status, code(&missing)), (400, "pow_required"));
    for altcha in ["not-base64!", "e30="] {
        let r = t
            .call("POST", "/v1/signup", None, json!({"altcha":altcha}))
            .await;
        assert_eq!((r.status, code(&r)), (400, "pow_invalid"), "{altcha}");
    }
    let wrong_type = t
        .call("POST", "/v1/signup", None, json!({"altcha":5}))
        .await;
    assert_eq!(wrong_type.status, 400);

    // Expired (issued in the past) and signed with another pepper.
    let long_ago = pow::create(PEPPER, 300, 60, 1_000);
    let expired = pow::encode(&long_ago, pow::solve(&long_ago).unwrap());
    let foreign = pow::create("some-other-pepper-1234567890", 300, 60, db::now());
    let foreign = pow::encode(&foreign, pow::solve(&foreign).unwrap());
    for altcha in [expired, foreign] {
        let r = t
            .call("POST", "/v1/signup", None, json!({"altcha":altcha}))
            .await;
        assert_eq!((r.status, code(&r)), (400, "pow_invalid"));
    }
    let accounts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(accounts, 0);
}

#[tokio::test]
async fn refused_signup_does_not_burn_the_solution_and_old_records_are_purged() {
    let t = Test::with(|c| c.signup.pow_max_number = 100).await;
    sqlx::query("INSERT INTO pow_spent(challenge,expires_at) VALUES('stale',1)")
        .execute(&t.app.db)
        .await
        .unwrap();
    let solve = |ch: &pow::Challenge| pow::encode(ch, pow::solve(ch).unwrap());
    let next = || async {
        let r = t.call("GET", "/v1/signup/challenge", None, json!({})).await;
        serde_json::from_value::<pow::Challenge>(r.body).unwrap()
    };
    let first = next().await;
    assert_eq!(
        t.call("POST", "/v1/signup", None, json!({"altcha":solve(&first)}))
            .await
            .status,
        200
    );
    let stale: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pow_spent WHERE challenge='stale'")
        .fetch_one(&t.app.db)
        .await
        .unwrap();
    assert_eq!(stale, 0, "expired spent records are purged on signup");

    t.app.config_mut().await.signup.global_hourly_limit = 1;
    let second = next().await;
    let blocked = t
        .call("POST", "/v1/signup", None, json!({"altcha":solve(&second)}))
        .await;
    assert_eq!(
        (blocked.status, code(&blocked)),
        (429, "creation_rate_limited")
    );
    assert!(retry_after(&blocked) > 0);
    t.app.config_mut().await.signup.global_hourly_limit = 0;
    let retried = t
        .call("POST", "/v1/signup", None, json!({"altcha":solve(&second)}))
        .await;
    assert_eq!(retried.status, 200, "{}", retried.body);
}

#[tokio::test]
async fn challenge_reports_disabled_pow_and_is_rate_limited_per_ip() {
    let t = Test::new().await;
    let r = t.call("GET", "/v1/signup/challenge", None, json!({})).await;
    assert_eq!((r.status, r.body), (200, json!({"required": false})));

    let t = Test::with(|c| {
        c.signup.pow_max_number = 50;
        c.rate_limits.challenges_per_minute = 2;
    })
    .await;
    for _ in 0..2 {
        assert_eq!(
            t.call("GET", "/v1/signup/challenge", None, json!({}))
                .await
                .status,
            200
        );
    }
    let limited = t.call("GET", "/v1/signup/challenge", None, json!({})).await;
    assert_eq!(
        (limited.status, code(&limited)),
        (429, "challenge_rate_limited")
    );
    assert!(retry_after(&limited) >= 1);
    let other = t
        .call_from("10.1.1.1", "GET", "/v1/signup/challenge", None, json!({}))
        .await;
    assert_eq!(other.status, 200);
}

#[tokio::test]
async fn disabled_signup_refuses_challenges_and_accounts() {
    let t = Test::with(|c| c.signup.enabled = false).await;
    let r = t.call("GET", "/v1/signup/challenge", None, json!({})).await;
    assert_eq!((r.status, code(&r)), (403, "signup_disabled"));
    let r = t.call("POST", "/v1/signup", None, json!({})).await;
    assert_eq!((r.status, code(&r)), (403, "signup_disabled"));
}

#[tokio::test]
async fn anonymous_key_creation_is_gone_but_customers_can_add_child_keys() {
    let t = Test::new().await;
    let r = t.call("POST", "/v1/keys", None, json!({"name":"x"})).await;
    assert_eq!(r.status, 401);
    let k = t.key().await;
    let token = k["dashboard_token"].as_str();
    let child = t
        .call("POST", "/v1/keys", token, json!({"name":"child"}))
        .await;
    assert_eq!(child.status, 200, "{}", child.body);
    assert_eq!(child.body["account_id"], k["account_id"]);
}

// --- key budgets ---------------------------------------------------------------

#[tokio::test]
async fn customers_cannot_rotate_young_keys_but_administrators_can() {
    let t = Test::with(|c| c.keys.rotate_cooldown_seconds = 300).await;
    let k = t.key().await;
    let token = k["dashboard_token"].as_str();
    let kid = k["id"].as_str().unwrap();
    let r = t
        .call("POST", &format!("/v1/keys/{kid}/rotate"), token, json!({}))
        .await;
    assert_eq!((r.status, code(&r)), (429, "rotate_cooldown"));
    assert!((290..=300).contains(&retry_after(&r)));
    let admin = t
        .call(
            "POST",
            &format!("/admin/api/keys/{kid}/rotate"),
            Some(ADMIN),
            json!({}),
        )
        .await;
    assert_eq!(admin.status, 200, "{}", admin.body);

    // Once old enough the customer may rotate. The new key starts a new cooldown.
    let new_id = admin.body["id"].as_str().unwrap();
    sqlx::query("UPDATE api_keys SET created_at=created_at-400")
        .execute(&t.app.db)
        .await
        .unwrap();
    let ok = t
        .call(
            "POST",
            &format!("/v1/keys/{new_id}/rotate"),
            token,
            json!({}),
        )
        .await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    let again = t
        .call(
            "POST",
            &format!("/v1/keys/{}/rotate", ok.body["id"].as_str().unwrap()),
            token,
            json!({}),
        )
        .await;
    assert_eq!(code(&again), "rotate_cooldown");
}

#[tokio::test]
async fn daily_creation_cap_counts_child_keys_and_rotations_but_not_admins() {
    let t = Test::with(|c| c.keys.max_new_per_day = 3).await;
    let k = t.key().await;
    let token = k["dashboard_token"].as_str();
    let account = k["account_id"].as_str().unwrap();
    let mut last = k["id"].as_str().unwrap().to_owned();
    for _ in 0..2 {
        let r = t.call("POST", "/v1/keys", token, json!({"name":"c"})).await;
        assert_eq!(r.status, 200, "{}", r.body);
        last = r.body["id"].as_str().unwrap().to_owned();
    }
    let capped = t.call("POST", "/v1/keys", token, json!({"name":"c"})).await;
    assert_eq!(
        (capped.status, code(&capped)),
        (429, "key_creation_daily_limit")
    );
    assert!(retry_after(&capped) > 3600);
    let rotate = t
        .call("POST", &format!("/v1/keys/{last}/rotate"), token, json!({}))
        .await;
    assert_eq!(
        (rotate.status, code(&rotate)),
        (429, "key_creation_daily_limit")
    );
    // Administrators are exempt for creation and rotation.
    let by_admin = t
        .call(
            "POST",
            &format!("/admin/api/accounts/{account}/keys"),
            Some(ADMIN),
            json!({"name":"admin"}),
        )
        .await;
    assert_eq!(by_admin.status, 200, "{}", by_admin.body);
    let rotated = t
        .call(
            "POST",
            &format!("/admin/api/keys/{last}/rotate"),
            Some(ADMIN),
            json!({}),
        )
        .await;
    assert_eq!(rotated.status, 200, "{}", rotated.body);
    // The window rolls: old creations stop counting after 24 hours.
    sqlx::query("UPDATE api_keys SET created_at=created_at-90000")
        .execute(&t.app.db)
        .await
        .unwrap();
    assert_eq!(
        t.call("POST", "/v1/keys", token, json!({"name":"c"}))
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn rotations_count_toward_the_global_hourly_limit() {
    let t = Test::new().await;
    let k = t.key().await;
    t.app.config_mut().await.signup.global_hourly_limit = 1;
    let r = t
        .call(
            "POST",
            &format!("/v1/keys/{}/rotate", k["id"].as_str().unwrap()),
            k["dashboard_token"].as_str(),
            json!({}),
        )
        .await;
    assert_eq!((r.status, code(&r)), (429, "creation_rate_limited"));
}

// --- request limits ------------------------------------------------------------

#[tokio::test]
async fn invalid_credentials_lock_the_ip_out_before_any_lookup() {
    let t = Test::with(|c| c.rate_limits.auth_failures_per_hour = 3).await;
    let k = t.key().await;
    for _ in 0..3 {
        let r = t
            .call("GET", "/v1/account", Some("sk-res-wrong"), json!({}))
            .await;
        assert_eq!((r.status, code(&r)), (401, "invalid_api_key"));
    }
    let locked = t
        .call("GET", "/v1/account", Some("sk-res-wrong"), json!({}))
        .await;
    assert_eq!(
        (locked.status, code(&locked)),
        (429, "too_many_auth_failures")
    );
    assert!(retry_after(&locked) > 60);
    // Even a valid credential, on any credentialed path, is refused from this IP.
    for (path, token) in [
        ("/v1/account", k["dashboard_token"].as_str()),
        ("/v1/chat/completions", k["key"].as_str()),
        ("/admin/api/overview", Some(ADMIN)),
    ] {
        let r = t.call("GET", path, token, json!({})).await;
        assert_eq!(code(&r), "too_many_auth_failures", "{path}");
    }
    // Other addresses and anonymous public routes are unaffected.
    let other = t
        .call_from(
            "10.9.9.9",
            "GET",
            "/v1/account",
            k["dashboard_token"].as_str(),
            json!({}),
        )
        .await;
    assert_eq!(other.status, 200);
    assert_eq!(
        t.call("GET", "/v1/plans", None, json!({})).await.status,
        200
    );
}

#[tokio::test]
async fn wrong_admin_tokens_and_proxy_keys_count_as_failures() {
    let t = Test::with(|c| c.rate_limits.auth_failures_per_hour = 2).await;
    let a = t
        .call(
            "GET",
            "/admin/api/overview",
            Some("wrong-admin-token"),
            json!({}),
        )
        .await;
    assert_eq!(code(&a), "dashboard_unauthorized");
    let p = t
        .call(
            "POST",
            "/v1/chat/completions",
            Some("sk-res-nope"),
            json!({}),
        )
        .await;
    assert_eq!(code(&p), "invalid_api_key");
    let locked = t
        .call("GET", "/admin/api/overview", Some(ADMIN), json!({}))
        .await;
    assert_eq!(
        (locked.status, code(&locked)),
        (429, "too_many_auth_failures")
    );
}

#[tokio::test]
async fn missing_credentials_and_valid_logins_do_not_consume_the_failure_budget() {
    let t = Test::with(|c| c.rate_limits.auth_failures_per_hour = 2).await;
    let k = t.key().await;
    for _ in 0..5 {
        assert_eq!(
            t.call("GET", "/v1/account", None, json!({})).await.status,
            401
        );
        assert_eq!(
            t.call(
                "GET",
                "/v1/account",
                k["dashboard_token"].as_str(),
                json!({})
            )
            .await
            .status,
            200
        );
    }
}

#[tokio::test]
async fn management_requests_are_rate_limited_per_ip() {
    let t = Test::with(|c| c.rate_limits.management_per_minute = 3).await;
    for _ in 0..3 {
        assert_eq!(
            t.call("GET", "/v1/plans", None, json!({})).await.status,
            200
        );
    }
    let r = t.call("GET", "/v1/plans", None, json!({})).await;
    assert_eq!((r.status, code(&r)), (429, "management_rate_limited"));
    assert!((1..=20).contains(&retry_after(&r)));
    let admin = t
        .call("GET", "/admin/api/overview", Some(ADMIN), json!({}))
        .await;
    assert_eq!(admin.status, 429, "admin API shares the per-IP budget");
    assert_eq!(t.call("GET", "/healthz", None, json!({})).await.status, 200);
    assert_eq!(
        t.call_from("10.2.2.2", "GET", "/v1/plans", None, json!({}))
            .await
            .status,
        200
    );
}

fn meta() -> RequestMeta<'static> {
    RequestMeta {
        method: "POST",
        path: "/v1/chat/completions",
        kind: "llm",
        model: "m",
        ip: "127.0.0.1",
    }
}

#[tokio::test]
async fn concurrent_requests_are_capped_per_account_and_stale_rows_do_not_count() {
    let t = Test::with(|c| c.rate_limits.max_concurrent_requests = 2).await;
    let k = t.key().await;
    let me = t.identity(&k).await;
    let a = Meter::begin(&t.app, Some(me.clone()), meta())
        .await
        .unwrap();
    let b = Meter::begin(&t.app, Some(me.clone()), meta())
        .await
        .unwrap();
    let third = Meter::begin(&t.app, Some(me.clone()), meta()).await;
    assert!(matches!(&third, Err(e) if e.code == "concurrency_limit_exceeded"));
    // Another account is independent.
    let other = t.key().await;
    let other_me = t.identity(&other).await;
    Meter::begin(&t.app, Some(other_me), meta())
        .await
        .unwrap()
        .finish()
        .await
        .unwrap();
    // Finishing frees a slot.
    a.finish().await.unwrap();
    let c = Meter::begin(&t.app, Some(me.clone()), meta())
        .await
        .unwrap();
    assert!(
        Meter::begin(&t.app, Some(me.clone()), meta())
            .await
            .is_err()
    );
    // A crashed request left behind (older than the upstream timeout) is ignored.
    sqlx::query("UPDATE requests SET created_at=created_at-100000 WHERE finished=0")
        .execute(&t.app.db)
        .await
        .unwrap();
    let d = Meter::begin(&t.app, Some(me), meta()).await.unwrap();
    for m in [b, c, d] {
        m.finish().await.unwrap();
    }
}

#[tokio::test]
async fn checkouts_are_capped_per_account_per_hour() {
    let t = Test::with(|c| {
        c.stripe.secret_key = "sk_test_x".into();
        c.stripe.webhook_secret = "whsec_x".into();
        c.rate_limits.checkouts_per_hour = 2;
    })
    .await;
    let k = t.key().await;
    for i in 0..2 {
        sqlx::query(
            "INSERT INTO payments(id,account_id,plan_code,provider,price_micro,credit_micro,\
             duration_days,created_at) VALUES(?,?,'monthly','stripe',1,1,30,?)",
        )
        .bind(format!("p{i}"))
        .bind(k["account_id"].as_str())
        .bind(db::now() - 10)
        .execute(&t.app.db)
        .await
        .unwrap();
    }
    let r = t
        .call(
            "POST",
            "/v1/account/checkout",
            k["dashboard_token"].as_str(),
            json!({"plan_code":"monthly"}),
        )
        .await;
    assert_eq!((r.status, code(&r)), (429, "checkout_rate_limited"));
    assert!(retry_after(&r) > 3000);
}

#[tokio::test]
async fn invalid_redeem_codes_lock_the_account_out_of_redeeming() {
    let t = Test::with(|c| c.rate_limits.redeem_failures_per_hour = 2).await;
    let k = t.key().await;
    let token = k["dashboard_token"].as_str();
    for _ in 0..2 {
        let r = t
            .call(
                "POST",
                "/v1/account/redeem",
                token,
                json!({"code":"RES-nope"}),
            )
            .await;
        assert_eq!((r.status, code(&r)), (400, "invalid_code"));
    }
    let good = t
        .call(
            "POST",
            "/admin/api/codes",
            Some(ADMIN),
            json!({"credit_usd":"3"}),
        )
        .await;
    let blocked = t
        .call(
            "POST",
            "/v1/account/redeem",
            token,
            json!({"code": good.body["code"]}),
        )
        .await;
    assert_eq!(
        (blocked.status, code(&blocked)),
        (429, "redeem_rate_limited")
    );
    assert!(retry_after(&blocked) > 0);
    // A different account can redeem the same code.
    let other = t.key().await;
    let ok = t
        .call(
            "POST",
            "/v1/account/redeem",
            other["dashboard_token"].as_str(),
            json!({"code": good.body["code"]}),
        )
        .await;
    assert_eq!(ok.status, 200, "{}", ok.body);
}

#[tokio::test]
async fn strict_transport_security_only_with_https_public_url() {
    let plain = Test::new().await;
    let r = plain.call("GET", "/healthz", None, json!({})).await;
    assert!(r.headers.get("strict-transport-security").is_none());
    assert_eq!(r.headers["x-content-type-options"], "nosniff");
    let secure = Test::with(|c| c.public_url = "https://api.example.com".into()).await;
    let r = secure.call("GET", "/healthz", None, json!({})).await;
    assert_eq!(r.headers["strict-transport-security"], "max-age=31536000");
    assert_eq!(r.headers["x-content-type-options"], "nosniff");
}
