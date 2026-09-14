use crate::{
    App,
    config::Config,
    db::{id, now},
    error::{Error, Result},
    money,
};
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::{Row, SqliteConnection};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub rpm: Option<u32>,
    pub requests: Option<Window>,
    pub tokens: Option<Window>,
    pub spend: Option<Spend>,
    pub max_children: Option<u32>,
    pub allowed_models: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub max: i64,
    pub window_hours: u32,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spend {
    pub max_usd: String,
    pub window_hours: u32,
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        for w in [&self.requests, &self.tokens].into_iter().flatten() {
            if w.max <= 0 || w.window_hours == 0 || w.window_hours > 8760 {
                return Err(Error::bad("Invalid usage window"));
            }
        }
        if let Some(w) = &self.spend
            && (money::usd(&w.max_usd)? <= 0 || w.window_hours == 0 || w.window_hours > 8760)
        {
            return Err(Error::bad("Invalid spend window"));
        }
        if self.allowed_models.len() > 100 || self.allowed_models.iter().any(|v| v.len() > 256) {
            return Err(Error::bad("Model allowlist too large"));
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct Identity {
    pub key_id: String,
    pub account_id: String,
    pub dashboard: bool,
    pub prefix: String,
    pub limits: Limits,
    pub markup: String,
}
pub fn secret(prefix: &str) -> String {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    format!("{prefix}{}", hex::encode(b))
}
pub fn hash(pepper: &str, token: &str) -> String {
    let mut h =
        Hmac::<Sha256>::new_from_slice(pepper.as_bytes()).expect("HMAC accepts all key lengths");
    h.update(token.as_bytes());
    hex::encode(h.finalize().into_bytes())
}
pub fn token(h: &HeaderMap) -> Option<&str> {
    h.get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(s, _)| s.eq_ignore_ascii_case("bearer"))
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
        .or_else(|| h.get("x-api-key").and_then(|v| v.to_str().ok()))
}
pub async fn authenticate(app: &App, h: &HeaderMap, proxy: bool) -> Result<Option<Identity>> {
    let c = app.config.read().await.clone();
    let Some(t) = token(h) else {
        return if proxy && c.require_key {
            Err(Error::new(401, "missing_api_key", "An API key is required"))
        } else {
            Ok(None)
        };
    };
    let digest = hash(&c.key_pepper, t);
    let r=sqlx::query("SELECT k.*,a.status AS account_status,a.markup_pct AS account_markup FROM api_keys k JOIN accounts a ON a.id=k.account_id WHERE key_hash=? OR dash_hash=?").bind(&digest).bind(&digest).fetch_optional(&app.db).await?;
    let Some(r) = r else {
        return if proxy && !c.require_key && c.accept_any_token {
            Ok(None)
        } else {
            Err(Error::new(401, "invalid_api_key", "Invalid API key"))
        };
    };
    let dashboard = r.get::<String, _>("dash_hash") == digest;
    let status = r.get::<String, _>("status");
    if status == "revoked" {
        return Err(Error::new(401, "key_revoked", "Key has been revoked"));
    }
    if r.get::<String, _>("account_status") != "active" {
        return Err(Error::new(403, "account_suspended", "Account is suspended"));
    }
    if proxy {
        if dashboard {
            return Err(Error::new(
                401,
                "invalid_api_key",
                "Dashboard tokens cannot call the proxy",
            ));
        }
        if status != "active" {
            return Err(Error::new(401, "key_blocked", "Key is blocked"));
        }
        if r.get::<Option<i64>, _>("expires_at")
            .is_some_and(|t| t <= now())
        {
            return Err(Error::new(401, "key_expired", "Key has expired"));
        }
    }
    let limits = serde_json::from_str(r.get("limits")).map_err(Error::internal)?;
    Ok(Some(Identity {
        key_id: r.get("id"),
        account_id: r.get("account_id"),
        dashboard,
        prefix: r.get("key_prefix"),
        limits,
        markup: r
            .get::<Option<String>, _>("markup_pct")
            .or(r.get("account_markup"))
            .unwrap_or(c.markup_pct),
    }))
}
pub async fn required(app: &App, h: &HeaderMap) -> Result<Identity> {
    authenticate(app, h, false).await?.ok_or_else(|| {
        Error::new(
            401,
            "missing_api_key",
            "Sign in with an API key or dashboard token",
        )
    })
}
pub async fn admin(app: &App, h: &HeaderMap) -> Result<()> {
    use subtle::ConstantTimeEq;
    let c = app.config.read().await;
    let t = h
        .get("x-admin-token")
        .and_then(|v| v.to_str().ok())
        .or_else(|| token(h))
        .unwrap_or("");
    if c.admin_token.is_empty()
        || !bool::from(
            hash("admin", t)
                .as_bytes()
                .ct_eq(hash("admin", &c.admin_token).as_bytes()),
        )
    {
        return Err(Error::new(
            401,
            "dashboard_unauthorized",
            "Valid administrator token required",
        ));
    }
    Ok(())
}
pub fn key_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({"id":r.get::<String,_>("id"),"account_id":r.get::<String,_>("account_id"),"key_prefix":r.get::<String,_>("key_prefix"),"name":r.get::<String,_>("name"),"status":r.get::<String,_>("status"),"limits":serde_json::from_str::<Value>(r.get("limits")).unwrap_or(json!({})),"markup_pct":r.get::<Option<String>,_>("markup_pct"),"expires_at":r.get::<Option<i64>,_>("expires_at"),"created_at":r.get::<i64,_>("created_at"),"last_used_at":r.get::<Option<i64>,_>("last_used_at")})
}
#[allow(clippy::too_many_arguments)] // one cohesive, transaction-scoped key-minting context
pub async fn mint(
    conn: &mut SqliteConnection,
    c: &Config,
    account: &str,
    ip: &str,
    name: &str,
    parent: Option<&str>,
    limits: &Limits,
    bypass: bool,
) -> Result<Value> {
    let key = secret("sk-res-");
    let dash = secret("dash_");
    let kid = id();
    let prefix = &key[..15];
    sqlx::query("INSERT INTO api_keys(id,account_id,parent_key_id,key_hash,dash_hash,key_prefix,name,limits,created_at,created_ip,bypass_ip) VALUES(?,?,?,?,?,?,?,?,?,?,?)")
 .bind(&kid).bind(account).bind(parent).bind(hash(&c.key_pepper,&key)).bind(hash(&c.key_pepper,&dash)).bind(prefix).bind(name.chars().take(80).collect::<String>()).bind(serde_json::to_string(limits)?).bind(now()).bind(ip).bind(bypass).execute(conn).await?;
    Ok(
        json!({"id":kid,"account_id":account,"key":key,"key_prefix":prefix,"dashboard_token":dash,"dashboard_url":format!("{}/workspace/#token={dash}",c.public_url.trim_end_matches('/')),"api_base":format!("{}/v1",c.public_url.trim_end_matches('/')),"limits":limits,"notice":"Save these credentials now. They are shown only once."}),
    )
}
pub async fn create(
    app: &App,
    identity: Option<&Identity>,
    account_override: Option<&str>,
    ip: &str,
    b: &Value,
) -> Result<Value> {
    let c = app.config.read().await.clone();
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    let bypass_admin = account_override.is_some();
    let aid = account_override
        .map(str::to_owned)
        .or_else(|| identity.map(|i| i.account_id.clone()));
    let mut bypass = bypass_admin;
    if let Some(a) = &aid {
        let r = sqlx::query("SELECT status,can_create_keys FROM accounts WHERE id=?")
            .bind(a)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| Error::new(404, "account_not_found", "Account not found"))?;
        if r.get::<String, _>("status") != "active" {
            return Err(Error::new(403, "account_suspended", "Account suspended"));
        }
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM api_keys WHERE account_id=? AND status='active'",
        )
        .bind(a)
        .fetch_one(&mut *tx)
        .await?;
        if n >= c.max_keys_per_account as i64 {
            return Err(Error::new(
                429,
                "account_key_limit",
                "Account key limit reached",
            ));
        }
        let sub: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM subscriptions WHERE account_id=? AND expires_at>?",
        )
        .bind(a)
        .bind(now())
        .fetch_one(&mut *tx)
        .await?;
        bypass |= r.get::<bool, _>("can_create_keys") || sub > 0;
    }
    if !bypass_admin {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE created_at>?")
            .bind(now() - 3600)
            .fetch_one(&mut *tx)
            .await?;
        if c.global_hourly_limit > 0 && n >= c.global_hourly_limit as i64 {
            return Err(Error::new(
                429,
                "creation_rate_limited",
                "Key creation hourly limit reached",
            ));
        }
        if let Some(i) = identity.filter(|i| !i.dashboard)
            && let Some(max) = i.limits.max_children
        {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM api_keys WHERE parent_key_id=? AND status='active'",
            )
            .bind(&i.key_id)
            .fetch_one(&mut *tx)
            .await?;
            if n >= max as i64 {
                return Err(Error::new(
                    429,
                    "child_key_limit",
                    "Child key limit reached",
                ));
            }
        }
        if !bypass {
            let limit = c
                .ip_overrides
                .get(ip)
                .copied()
                .unwrap_or(c.ip_limit_per_month as i32);
            let month = chrono::Utc::now().format("%Y-%m-01 00:00:00").to_string();
            let n:i64=sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE created_ip=? AND created_at>=unixepoch(?) AND bypass_ip=0").bind(ip).bind(month).fetch_one(&mut *tx).await?;
            if limit > 0 && n >= limit as i64 {
                return Err(Error::new(
                    429,
                    "ip_key_limit",
                    "Monthly key allowance reached",
                ));
            }
            let last: Option<i64> =
                sqlx::query_scalar("SELECT MAX(created_at) FROM api_keys WHERE created_ip=?")
                    .bind(ip)
                    .fetch_one(&mut *tx)
                    .await?;
            if last.is_some_and(|t| t + c.cooldown_seconds as i64 > now()) {
                return Err(Error::new(
                    429,
                    "creation_cooldown",
                    "Please wait before creating another key",
                ));
            }
        }
    }
    let account = if let Some(a) = aid {
        a
    } else {
        let a = id();
        sqlx::query("INSERT INTO accounts(id,contact,created_at,created_ip) VALUES(?,?,?,?)")
            .bind(&a)
            .bind(
                b["contact"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(254)
                    .collect::<String>(),
            )
            .bind(now())
            .bind(ip)
            .execute(&mut *tx)
            .await?;
        let amount = money::usd(&c.free_credit_usd)?;
        if amount > 0 {
            crate::billing::credit(
                &mut tx,
                &a,
                amount,
                "free",
                (c.free_credit_expiry_days > 0)
                    .then(|| now() + c.free_credit_expiry_days as i64 * 86400),
                "",
            )
            .await?;
        }
        a
    };
    let limits = if bypass_admin && b.get("limits").is_some() {
        let l: Limits = serde_json::from_value(b["limits"].clone())?;
        l.validate()?;
        l
    } else {
        c.default_limits.clone()
    };
    let out = mint(
        &mut tx,
        &c,
        &account,
        ip,
        b["name"].as_str().unwrap_or("My key"),
        identity.map(|i| i.key_id.as_str()),
        &limits,
        bypass,
    )
    .await?;
    tx.commit().await?;
    Ok(out)
}
pub async fn rotate(app: &App, kid: &str, account: Option<&str>) -> Result<Value> {
    let c = app.config.read().await.clone();
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    let r = sqlx::query("SELECT * FROM api_keys WHERE id=?")
        .bind(kid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::new(404, "key_not_found", "Key not found"))?;
    let aid: String = r.get("account_id");
    if account.is_some_and(|a| a != aid) {
        return Err(Error::new(404, "key_not_found", "Key not found"));
    }
    if r.get::<String, _>("status") != "active" {
        return Err(Error::new(
            400,
            "key_not_active",
            "Only active keys can be rotated",
        ));
    }
    // Rotation replaces a key and cannot be used to mine new trial credits.
    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM api_keys WHERE account_id=? AND status='active' AND id<>?",
    )
    .bind(&aid)
    .bind(kid)
    .fetch_one(&mut *tx)
    .await?;
    if active >= c.max_keys_per_account as i64 {
        return Err(Error::new(
            429,
            "account_key_limit",
            "Account key limit reached",
        ));
    }
    sqlx::query("UPDATE api_keys SET status='blocked' WHERE id=?")
        .bind(kid)
        .execute(&mut *tx)
        .await?;
    let limits: Limits = serde_json::from_str(r.get("limits"))?;
    let out = mint(
        &mut tx,
        &c,
        &aid,
        r.get("created_ip"),
        r.get("name"),
        Some(kid),
        &limits,
        true,
    )
    .await?;
    sqlx::query("UPDATE api_keys SET markup_pct=?,expires_at=? WHERE id=?")
        .bind(r.get::<Option<String>, _>("markup_pct"))
        .bind(r.get::<Option<i64>, _>("expires_at"))
        .bind(out["id"].as_str())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(out)
}
