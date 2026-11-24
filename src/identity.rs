//! Credential generation, hashing, and request authentication.
//!
//! Each API key has two credentials:
//!
//! - the proxy key (`sk-res-<hex>`), used to call the proxy.
//! - the dashboard token (`dash_<hex>`), used to manage the account. It cannot
//!   call the proxy.
//!
//! Only HMAC-SHA256 digests, keyed with the configured pepper, are stored.
use crate::{
    App,
    config::Config,
    db::now,
    error::{Error, Result},
    keys::Limits,
};
use axum::http::{Extensions, HeaderMap, header};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use sqlx::Row;
use subtle::ConstantTimeEq;

/// An authenticated API key (or its dashboard token).
#[derive(Clone, Debug)]
pub struct Identity {
    pub key_id: String,
    pub account_id: String,
    /// True when authenticated with the dashboard token rather than the key.
    pub dashboard: bool,
    pub prefix: String,
    /// False for a blocked or expired key reached through a management route
    /// (read-only workspace access). Proxy calls never get such an identity.
    pub active: bool,
    pub limits: Limits,
    /// Effective markup: key override, else account override, else global.
    pub markup: String,
}

impl Identity {
    /// Fails with `key_inactive` when the key is blocked or expired. Required
    /// before any change to keys: such keys may read their workspace only.
    pub fn require_active(&self) -> Result<()> {
        if self.active {
            Ok(())
        } else {
            Err(Error::forbidden(
                "key_inactive",
                "This key is blocked or expired. It can read its workspace but not change keys",
            ))
        }
    }
}

/// Where a credential is being used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// `/v1/*` upstream proxy routes: only active, unexpired proxy keys.
    Proxy,
    /// Customer management routes: proxy keys and dashboard tokens.
    Manage,
}

/// Returns `prefix` followed by 256 bits of OS randomness, hex-encoded.
pub fn secret(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("operating system random source is available");
    format!("{prefix}{}", hex::encode(bytes))
}

/// HMAC-SHA256 of `token` keyed with `pepper`, hex-encoded.
pub fn hash(pepper: &str, token: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(pepper.as_bytes()).expect("HMAC accepts any key length");
    mac.update(token.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Extracts the bearer token from `Authorization`, or the `x-api-key` header
/// when no bearer token is present.
pub fn token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
}

/// An identity the server already authenticated (an OAuth access token on
/// `/mcp`), attached to internal requests as a request extension. Clients cannot
/// set extensions, so it only exists on requests built by [`crate::mcp`].
#[derive(Clone, Debug)]
pub struct Preauth(pub Identity);

/// Whether `presented` is the administrator token (constant time).
pub fn is_admin_token(config: &Config, presented: &str) -> bool {
    // Comparing fixed-length digests hides the token length as well.
    !config.admin_token.is_empty()
        && bool::from(
            hash("admin", presented)
                .as_bytes()
                .ct_eq(hash("admin", &config.admin_token).as_bytes()),
        )
}

/// Message of the error returned when the administrator token is used where a
/// customer account is needed.
const ADMIN_NOT_CUSTOMER: &str = "The administrator token cannot manage a customer account. \
     Use /admin/api, or a customer API key";

/// Resolves the request credential into an [`Identity`].
///
/// Returns `Ok(None)` for anonymous access. The caller decides whether that
/// is acceptable. Anonymous access happens in these cases:
///
/// - A proxy route with `require_key` disabled and no credential sent.
/// - A proxy route with `require_key` disabled, `accept_any_token` set, and an
///   unknown credential.
/// - A proxy route called with the administrator token (an operator: never
///   billed, no limits, even when `require_key` is on).
/// - A management route with no credential sent.
///
/// The administrator token on a management route is refused with
/// `admin_token_not_customer` (not counted as an authentication failure).
pub async fn authenticate(
    app: &App,
    headers: &HeaderMap,
    scope: Scope,
) -> Result<Option<Identity>> {
    let config = app.config().await;
    match token(headers) {
        Some(token) => credential(app, &config, token, scope).await,
        None if scope == Scope::Proxy && config.require_key => Err(missing_credential()),
        None => Ok(None),
    }
}

/// [`authenticate`], but a [`Preauth`] extension on the request wins.
pub async fn authenticate_request(
    app: &App,
    headers: &HeaderMap,
    extensions: &Extensions,
    scope: Scope,
) -> Result<Option<Identity>> {
    match extensions.get::<Preauth>() {
        Some(Preauth(identity)) if scope == Scope::Proxy && identity.dashboard => {
            Err(dashboard_not_allowed())
        }
        Some(Preauth(identity)) => Ok(Some(identity.clone())),
        None => authenticate(app, headers, scope).await,
    }
}

/// Resolves one presented credential string (API key or dashboard token).
pub async fn credential(
    app: &App,
    config: &Config,
    token: &str,
    scope: Scope,
) -> Result<Option<Identity>> {
    let proxy = scope == Scope::Proxy;
    if is_admin_token(config, token) {
        return if proxy {
            Ok(None)
        } else {
            Err(Error::forbidden(
                "admin_token_not_customer",
                ADMIN_NOT_CUSTOMER,
            ))
        };
    }
    let digest = hash(&config.key_pepper, token);
    let row = sqlx::query(
        "SELECT k.*, a.status AS account_status, a.markup_pct AS account_markup \
         FROM api_keys k JOIN accounts a ON a.id=k.account_id \
         WHERE k.key_hash=? OR k.dash_hash=?",
    )
    .bind(&digest)
    .bind(&digest)
    .fetch_optional(&app.db)
    .await?;
    let Some(row) = row else {
        return if proxy && !config.require_key && config.accept_any_token {
            Ok(None)
        } else {
            Err(Error::unauthorized("invalid_api_key", "Invalid API key"))
        };
    };
    let dashboard = row.get::<String, _>("dash_hash") == digest;
    identity_of(config, &row, dashboard, scope).map(Some)
}

/// The identity of an active key by id, with the checks of a proxy call
/// (never a dashboard identity). Used for OAuth grants, which are bound to a key.
pub async fn by_key_id(app: &App, key_id: &str) -> Result<Identity> {
    let config = app.config().await;
    let row = sqlx::query(
        "SELECT k.*, a.status AS account_status, a.markup_pct AS account_markup \
         FROM api_keys k JOIN accounts a ON a.id=k.account_id WHERE k.id=?",
    )
    .bind(key_id)
    .fetch_optional(&app.db)
    .await?
    .ok_or_else(|| Error::unauthorized("invalid_api_key", "Invalid API key"))?;
    identity_of(&config, &row, false, Scope::Proxy)
}

fn dashboard_not_allowed() -> Error {
    Error::unauthorized(
        "dashboard_token_not_allowed",
        "Dashboard tokens cannot call the proxy",
    )
}

fn identity_of(
    config: &Config,
    row: &sqlx::sqlite::SqliteRow,
    dashboard: bool,
    scope: Scope,
) -> Result<Identity> {
    let status: String = row.get("status");
    if status == "revoked" {
        return Err(Error::unauthorized("key_revoked", "Key has been revoked"));
    }
    if row.get::<String, _>("account_status") != "active" {
        return Err(Error::forbidden(
            "account_suspended",
            "Account is suspended",
        ));
    }
    let expired = row
        .get::<Option<i64>, _>("expires_at")
        .is_some_and(|t| t <= now());
    if scope == Scope::Proxy {
        if dashboard {
            return Err(dashboard_not_allowed());
        }
        if status != "active" {
            return Err(Error::unauthorized("key_blocked", "Key is blocked"));
        }
        if expired {
            return Err(Error::unauthorized("key_expired", "Key has expired"));
        }
    }
    let limits = serde_json::from_str(row.get("limits")).map_err(Error::internal)?;
    Ok(Identity {
        key_id: row.get("id"),
        account_id: row.get("account_id"),
        dashboard,
        prefix: row.get("key_prefix"),
        active: status == "active" && !expired,
        limits,
        markup: row
            .get::<Option<String>, _>("markup_pct")
            .or(row.get("account_markup"))
            .unwrap_or_else(|| config.markup_pct.clone()),
    })
}

/// Like [`authenticate`] for management routes, but a credential is required.
pub async fn required(app: &App, headers: &HeaderMap) -> Result<Identity> {
    required_request(app, headers, &Extensions::new()).await
}

/// [`required`] for a request that may carry a [`Preauth`] extension.
pub async fn required_request(
    app: &App,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> Result<Identity> {
    authenticate_request(app, headers, extensions, Scope::Manage)
        .await?
        .ok_or_else(missing_credential)
}

fn missing_credential() -> Error {
    Error::unauthorized("missing_api_key", "Missing bearer authentication in header")
}

/// Verifies the administrator token in constant time.
///
/// The token is read from `x-admin-token`. Without that header, the bearer or
/// `x-api-key` credential is used. An empty configured token rejects every
/// request.
pub async fn admin(app: &App, headers: &HeaderMap) -> Result<()> {
    let config = app.config().await;
    let presented = headers
        .get("x-admin-token")
        .and_then(|v| v.to_str().ok())
        .or_else(|| token(headers))
        .unwrap_or("");
    if is_admin_token(&config, presented) {
        Ok(())
    } else {
        Err(Error::unauthorized(
            "dashboard_unauthorized",
            "Valid administrator token required",
        ))
    }
}
