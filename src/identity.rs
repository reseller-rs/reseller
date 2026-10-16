//! Credential generation, hashing, and request authentication.
//!
//! Every API key has two credentials: the proxy key (`sk-res-…`) and a
//! dashboard token (`dash_…`) that can manage the account but cannot call the
//! proxy. Only HMAC-SHA256 digests (keyed with the configured pepper) are
//! stored.
use crate::{
    App,
    db::now,
    error::{Error, Result},
    keys::Limits,
};
use axum::http::{HeaderMap, header};
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
    pub limits: Limits,
    /// Effective markup: key override, else account override, else global.
    pub markup: String,
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

/// Extracts a bearer token or `x-api-key` header value.
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

/// Resolves the request credential.
///
/// Returns `Ok(None)` for anonymous access, which is only possible on proxy
/// routes when `require_key` is disabled, or on management routes without a
/// credential (the caller decides whether that is acceptable).
pub async fn authenticate(
    app: &App,
    headers: &HeaderMap,
    scope: Scope,
) -> Result<Option<Identity>> {
    let config = app.config().await;
    let proxy = scope == Scope::Proxy;
    let Some(token) = token(headers) else {
        return if proxy && config.require_key {
            Err(Error::unauthorized(
                "missing_api_key",
                "An API key is required",
            ))
        } else {
            Ok(None)
        };
    };
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
    if proxy {
        if dashboard {
            return Err(Error::unauthorized(
                "invalid_api_key",
                "Dashboard tokens cannot call the proxy",
            ));
        }
        if status != "active" {
            return Err(Error::unauthorized("key_blocked", "Key is blocked"));
        }
        if row
            .get::<Option<i64>, _>("expires_at")
            .is_some_and(|t| t <= now())
        {
            return Err(Error::unauthorized("key_expired", "Key has expired"));
        }
    }
    let limits = serde_json::from_str(row.get("limits")).map_err(Error::internal)?;
    Ok(Some(Identity {
        key_id: row.get("id"),
        account_id: row.get("account_id"),
        dashboard,
        prefix: row.get("key_prefix"),
        limits,
        markup: row
            .get::<Option<String>, _>("markup_pct")
            .or(row.get("account_markup"))
            .unwrap_or_else(|| config.markup_pct.clone()),
    }))
}

/// Like [`authenticate`] for management routes, but a credential is required.
pub async fn required(app: &App, headers: &HeaderMap) -> Result<Identity> {
    authenticate(app, headers, Scope::Manage)
        .await?
        .ok_or_else(|| {
            Error::unauthorized(
                "missing_api_key",
                "Sign in with an API key or dashboard token",
            )
        })
}

/// Verifies the administrator token (`x-admin-token` or bearer) in constant
/// time.
pub async fn admin(app: &App, headers: &HeaderMap) -> Result<()> {
    let config = app.config().await;
    let presented = headers
        .get("x-admin-token")
        .and_then(|v| v.to_str().ok())
        .or_else(|| token(headers))
        .unwrap_or("");
    // Comparing fixed-length digests hides the token length as well.
    let matches = !config.admin_token.is_empty()
        && bool::from(
            hash("admin", presented)
                .as_bytes()
                .ct_eq(hash("admin", &config.admin_token).as_bytes()),
        );
    if matches {
        Ok(())
    } else {
        Err(Error::unauthorized(
            "dashboard_unauthorized",
            "Valid administrator token required",
        ))
    }
}
