//! API-key lifecycle: per-key limits, self-service creation guards, minting,
//! and rotation.
use crate::{
    App,
    config::Config,
    db::{self, DAY, HOUR, id, now},
    error::{Error, Result},
    identity::{Identity, hash, secret},
    money,
    security::pow,
};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};

/// Maximum window length for usage limits (one year).
const MAX_WINDOW_HOURS: u32 = 8760;
const MAX_NAME_CHARS: usize = 80;
const MAX_CONTACT_CHARS: usize = 254;

/// Usage limits stored as JSON on every key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Requests per rolling minute.
    pub rpm: Option<u32>,
    pub requests: Option<Window>,
    pub tokens: Option<Window>,
    pub spend: Option<Spend>,
    /// Active child keys a proxy key may create.
    pub max_children: Option<u32>,
    /// Case-insensitive model allowlist. An empty list allows every model.
    pub allowed_models: Vec<String>,
}

/// A count limit over a window of whole hours.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub max: i64,
    pub window_hours: u32,
}

/// A spend limit (decimal USD string) over a window of whole hours.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spend {
    pub max_usd: String,
    pub window_hours: u32,
}

impl Limits {
    /// Rejects invalid windows, non-positive maxima, and oversized allowlists.
    ///
    /// The allowlist may hold at most 100 entries of 256 bytes each.
    pub fn validate(&self) -> Result<()> {
        let valid_window = |hours: u32| (1..=MAX_WINDOW_HOURS).contains(&hours);
        for w in [&self.requests, &self.tokens].into_iter().flatten() {
            if w.max <= 0 || !valid_window(w.window_hours) {
                return Err(Error::bad("Invalid usage window"));
            }
        }
        if let Some(w) = &self.spend
            && (money::usd(&w.max_usd)? <= 0 || !valid_window(w.window_hours))
        {
            return Err(Error::bad("Invalid spend window"));
        }
        if self.allowed_models.len() > 100 || self.allowed_models.iter().any(|m| m.len() > 256) {
            return Err(Error::bad("Model allowlist too large"));
        }
        Ok(())
    }

    /// Whether `model` passes the allowlist.
    pub fn allows(&self, model: &str) -> bool {
        self.allowed_models.is_empty()
            || self
                .allowed_models
                .iter()
                .any(|m| m.eq_ignore_ascii_case(model))
    }
}

/// Public JSON view of an `api_keys` row (never includes hashes).
pub fn key_json(r: &SqliteRow) -> Value {
    json!({
        "id": r.get::<String, _>("id"),
        "account_id": r.get::<String, _>("account_id"),
        "key_prefix": r.get::<String, _>("key_prefix"),
        "name": r.get::<String, _>("name"),
        "status": r.get::<String, _>("status"),
        "limits": serde_json::from_str::<Value>(r.get("limits")).unwrap_or_else(|_| json!({})),
        "markup_pct": r.get::<Option<String>, _>("markup_pct"),
        "expires_at": r.get::<Option<i64>, _>("expires_at"),
        "created_at": r.get::<i64, _>("created_at"),
        "last_used_at": r.get::<Option<i64>, _>("last_used_at"),
    })
}

/// Truncates user-supplied display text to `max` characters.
pub fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Everything needed to insert a key row.
struct NewKey<'a> {
    account: &'a str,
    ip: &'a str,
    name: &'a str,
    parent: Option<&'a str>,
    limits: &'a Limits,
    /// Excluded from the per-IP monthly allowance.
    bypass_ip: bool,
}

/// Inserts a key and returns its one-time credentials.
async fn mint(conn: &mut SqliteConnection, config: &Config, key: NewKey<'_>) -> Result<Value> {
    let secret_key = secret("sk-res-");
    let dashboard = secret("dash_");
    let kid = id();
    // The stored prefix is "sk-res-" plus 8 hex characters: enough to recognise
    // a key, not enough to use one.
    let prefix = &secret_key[..15];
    sqlx::query(
        "INSERT INTO api_keys(id,account_id,parent_key_id,key_hash,dash_hash,key_prefix,name,\
         limits,created_at,created_ip,bypass_ip) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&kid)
    .bind(key.account)
    .bind(key.parent)
    .bind(hash(&config.key_pepper, &secret_key))
    .bind(hash(&config.key_pepper, &dashboard))
    .bind(prefix)
    .bind(clip(key.name, MAX_NAME_CHARS))
    .bind(serde_json::to_string(key.limits)?)
    .bind(now())
    .bind(key.ip)
    .bind(key.bypass_ip)
    .execute(conn)
    .await?;
    let base = config.public_base();
    Ok(json!({
        "id": kid,
        "account_id": key.account,
        "key": secret_key,
        "key_prefix": prefix,
        "dashboard_token": dashboard,
        "dashboard_url": format!("{base}/workspace/#token={dashboard}"),
        "api_base": format!("{base}/v1"),
        "limits": key.limits,
        "notice": "Save these credentials now. They are shown only once.",
    }))
}

/// Who is asking for a new key.
#[derive(Clone, Copy, Debug)]
pub enum Requester<'a> {
    /// Anonymous self-service signup: opens an account with trial credit.
    /// `altcha` is the base64 proof-of-work solution (required unless
    /// `signup.pow_max_number` is 0).
    Signup { altcha: Option<&'a str> },
    /// A signed-in customer creating a child key. Creation guards apply.
    Customer(&'a Identity),
    /// Administrator issuing a key on an existing account. This bypasses guards
    /// and may set custom limits.
    Admin { account: &'a str },
}

/// Creates a key for the requester.
///
/// Anonymous signups also open a new account with trial credit.
pub async fn create(app: &App, requester: Requester<'_>, ip: &str, body: &Value) -> Result<Value> {
    let config = app.config().await;
    let proof = match requester {
        Requester::Signup { altcha } => {
            if !config.signup.enabled {
                return Err(Error::forbidden(
                    "signup_disabled",
                    "Self-service signup is disabled",
                ));
            }
            match config.signup.pow_max_number {
                0 => None,
                max => {
                    let payload = altcha.ok_or_else(|| {
                        Error::bad("Solve the proof-of-work challenge to sign up")
                            .with_code("pow_required")
                    })?;
                    Some(pow::verify(&config.key_pepper, payload, max, now())?)
                }
            }
        }
        _ => None,
    };
    let mut tx = db::write(&app.db).await?;
    let (identity, existing) = match requester {
        Requester::Customer(identity) => (Some(identity), Some(identity.account_id.clone())),
        Requester::Admin { account } => (None, Some(account.to_owned())),
        Requester::Signup { .. } => (None, None),
    };
    let admin = matches!(requester, Requester::Admin { .. });
    let mut bypass_ip = admin;
    if let Some(account) = &existing {
        let row = sqlx::query("SELECT status,can_create_keys FROM accounts WHERE id=?")
            .bind(account)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| Error::not_found("account_not_found", "Account not found"))?;
        if row.get::<String, _>("status") != "active" {
            return Err(Error::forbidden("account_suspended", "Account suspended"));
        }
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM api_keys WHERE account_id=? AND status='active'",
        )
        .bind(account)
        .fetch_one(&mut *tx)
        .await?;
        if active >= i64::from(config.keys.max_per_account) {
            return Err(Error::too_many(
                "account_key_limit",
                "Account key limit reached",
            ));
        }
        let subscribed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM subscriptions WHERE account_id=? AND expires_at>?)",
        )
        .bind(account)
        .bind(now())
        .fetch_one(&mut *tx)
        .await?;
        bypass_ip |= row.get::<bool, _>("can_create_keys") || subscribed;
    }
    if !admin {
        check_creation_guards(&mut tx, &config, identity, ip, bypass_ip).await?;
    }
    if let Some(account) = identity.map(|i| i.account_id.as_str()) {
        check_daily_budget(&mut tx, &config, account).await?;
    }
    if let Some(proof) = &proof {
        spend_proof(&mut tx, proof).await?;
    }
    let account = match existing {
        Some(account) => account,
        None => open_trial_account(&mut tx, &config, ip, body).await?,
    };
    let limits = match body.get("limits") {
        Some(v) if admin => {
            let limits: Limits = serde_json::from_value(v.clone())?;
            limits.validate()?;
            limits
        }
        _ => config.keys.default_limits.clone(),
    };
    let out = mint(
        &mut tx,
        &config,
        NewKey {
            account: &account,
            ip,
            name: body["name"].as_str().unwrap_or("My key"),
            parent: identity.map(|i| i.key_id.as_str()),
            limits: &limits,
            bypass_ip,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(out)
}

/// Marks a proof-of-work solution as used, so replays fail.
///
/// Also purges spent records whose challenges have expired. Runs in the same
/// transaction as the account it pays for, so a failed signup does not burn
/// the solution.
async fn spend_proof(conn: &mut SqliteConnection, proof: &pow::Verified) -> Result<()> {
    sqlx::query("DELETE FROM pow_spent WHERE expires_at<=?")
        .bind(now())
        .execute(&mut *conn)
        .await?;
    let inserted = sqlx::query("INSERT OR IGNORE INTO pow_spent(challenge,expires_at) VALUES(?,?)")
        .bind(&proof.challenge)
        .bind(proof.expires)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    if inserted == 0 {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "pow_replayed",
            "This proof of work was already used; request a new challenge",
        ));
    }
    Ok(())
}

/// Returns the seconds until the oldest row leaves the window, or 60 when no
/// row exists.
fn window_retry(oldest: Option<i64>, window: i64) -> u64 {
    oldest.map_or(60, |t| (t + window - now()).max(1).unsigned_abs())
}

/// Global cap on key creations in the last hour, across all accounts.
///
/// A limit of 0 disables the cap.
async fn check_global_hourly(conn: &mut SqliteConnection, config: &Config) -> Result<()> {
    let limit = config.signup.global_hourly_limit;
    if limit == 0 {
        return Ok(());
    }
    let (recent, oldest): (i64, Option<i64>) =
        sqlx::query_as("SELECT COUNT(*),MIN(created_at) FROM api_keys WHERE created_at>?")
            .bind(now() - HOUR)
            .fetch_one(&mut *conn)
            .await?;
    if recent >= i64::from(limit) {
        return Err(
            Error::too_many("creation_rate_limited", "Key creation hourly limit reached")
                .retry_after(window_retry(oldest, HOUR)),
        );
    }
    Ok(())
}

/// Per-account cap on key creations in the last 24 hours.
///
/// A limit of 0 disables the cap.
async fn check_daily_budget(
    conn: &mut SqliteConnection,
    config: &Config,
    account: &str,
) -> Result<()> {
    let limit = config.keys.max_new_per_day;
    if limit == 0 {
        return Ok(());
    }
    let (recent, oldest): (i64, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*),MIN(created_at) FROM api_keys WHERE account_id=? AND created_at>?",
    )
    .bind(account)
    .bind(now() - DAY)
    .fetch_one(&mut *conn)
    .await?;
    if recent >= i64::from(limit) {
        return Err(Error::too_many(
            "key_creation_daily_limit",
            "Daily key creation limit reached for this account",
        )
        .retry_after(window_retry(oldest, DAY)));
    }
    Ok(())
}

/// Abuse guards for self-service creation: global hourly cap, child-key cap,
/// and (unless exempt) the per-IP monthly allowance and cooldown.
async fn check_creation_guards(
    conn: &mut SqliteConnection,
    config: &Config,
    identity: Option<&Identity>,
    ip: &str,
    bypass_ip: bool,
) -> Result<()> {
    check_global_hourly(conn, config).await?;
    if let Some(parent) = identity.filter(|i| !i.dashboard)
        && let Some(max) = parent.limits.max_children
    {
        let children: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM api_keys WHERE parent_key_id=? AND status='active'",
        )
        .bind(&parent.key_id)
        .fetch_one(&mut *conn)
        .await?;
        if children >= i64::from(max) {
            return Err(Error::too_many(
                "child_key_limit",
                "Child key limit reached",
            ));
        }
    }
    if bypass_ip {
        return Ok(());
    }
    let limit = config
        .signup
        .ip_overrides
        .get(ip)
        .copied()
        .unwrap_or(config.signup.ip_limit_per_month);
    if limit > 0 {
        let month = chrono::Utc::now().format("%Y-%m-01 00:00:00").to_string();
        let created: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM api_keys \
             WHERE created_ip=? AND created_at>=unixepoch(?) AND bypass_ip=0",
        )
        .bind(ip)
        .bind(month)
        .fetch_one(&mut *conn)
        .await?;
        if created >= i64::from(limit) {
            return Err(Error::too_many(
                "ip_key_limit",
                "Monthly key allowance reached",
            ));
        }
    }
    let cooldown = i64::from(config.signup.cooldown_seconds);
    if cooldown > 0 {
        let last: Option<i64> =
            sqlx::query_scalar("SELECT MAX(created_at) FROM api_keys WHERE created_ip=?")
                .bind(ip)
                .fetch_one(&mut *conn)
                .await?;
        if let Some(t) = last.filter(|t| t + cooldown > now()) {
            return Err(Error::too_many(
                "creation_cooldown",
                "Please wait before creating another key",
            )
            .retry_after(window_retry(Some(t), cooldown)));
        }
    }
    Ok(())
}

/// Opens a new account and grants the configured trial credit, if any.
async fn open_trial_account(
    conn: &mut SqliteConnection,
    config: &Config,
    ip: &str,
    body: &Value,
) -> Result<String> {
    let account = id();
    sqlx::query("INSERT INTO accounts(id,contact,created_at,created_ip) VALUES(?,?,?,?)")
        .bind(&account)
        .bind(clip(
            body["contact"].as_str().unwrap_or(""),
            MAX_CONTACT_CHARS,
        ))
        .bind(now())
        .bind(ip)
        .execute(&mut *conn)
        .await?;
    let amount = money::usd(&config.signup.free_credit_usd)?;
    if amount > 0 {
        let expires = (config.signup.free_credit_expiry_days > 0)
            .then(|| now() + i64::from(config.signup.free_credit_expiry_days) * DAY);
        crate::billing::credit(conn, &account, amount, "free", expires, "").await?;
    }
    Ok(account)
}

/// Replaces an active key with a fresh one.
///
/// The new key keeps the name, limits, markup, and expiry of the old key, and
/// the old key is blocked. `account` scopes ownership for customer requests.
/// Pass `None` for administrators, who skip the rotation cooldown and the
/// creation budgets.
pub async fn rotate(app: &App, kid: &str, account: Option<&str>) -> Result<Value> {
    let config = app.config().await;
    let mut tx = db::write(&app.db).await?;
    let not_found = || Error::not_found("key_not_found", "Key not found");
    let row = sqlx::query("SELECT * FROM api_keys WHERE id=?")
        .bind(kid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(not_found)?;
    let aid: String = row.get("account_id");
    if account.is_some_and(|a| a != aid) {
        return Err(not_found());
    }
    if row.get::<String, _>("status") != "active" {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "key_not_active",
            "Only active keys can be rotated",
        ));
    }
    if account.is_some() {
        let cooldown = i64::from(config.keys.rotate_cooldown_seconds);
        let age = now() - row.get::<i64, _>("created_at");
        if age < cooldown {
            return Err(Error::too_many(
                "rotate_cooldown",
                "This key was created recently; wait before rotating it",
            )
            .retry_after((cooldown - age).unsigned_abs()));
        }
        check_global_hourly(&mut tx, &config).await?;
        check_daily_budget(&mut tx, &config, &aid).await?;
    }
    // Rotation replaces a key. It must not become a way around the key cap.
    let others: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM api_keys WHERE account_id=? AND status='active' AND id<>?",
    )
    .bind(&aid)
    .bind(kid)
    .fetch_one(&mut *tx)
    .await?;
    if others >= i64::from(config.keys.max_per_account) {
        return Err(Error::too_many(
            "account_key_limit",
            "Account key limit reached",
        ));
    }
    sqlx::query("UPDATE api_keys SET status='blocked' WHERE id=?")
        .bind(kid)
        .execute(&mut *tx)
        .await?;
    let limits: Limits = serde_json::from_str(row.get("limits")).map_err(Error::internal)?;
    let out = mint(
        &mut tx,
        &config,
        NewKey {
            account: &aid,
            ip: row.get("created_ip"),
            name: row.get("name"),
            parent: Some(kid),
            limits: &limits,
            bypass_ip: true,
        },
    )
    .await?;
    sqlx::query("UPDATE api_keys SET markup_pct=?,expires_at=? WHERE id=?")
        .bind(row.get::<Option<String>, _>("markup_pct"))
        .bind(row.get::<Option<i64>, _>("expires_at"))
        .bind(out["id"].as_str())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(out)
}
