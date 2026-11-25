//! Native app store purchases: App Store (StoreKit 2) and Google Play.
//!
//! The app buys through the store, then sends the purchase to this server
//! (`POST /v1/account/store/{apple,google}`), which verifies it with the
//! store and credits the account once. The stores' server notifications
//! (`/webhooks/apple`, `/webhooks/google`) keep subscriptions current:
//! renewals, cancellations, refunds, and chargebacks.
//!
//! An account is tied to its purchases by an opaque `store_token` (a UUID
//! the app passes as the App Store `appAccountToken` and the Google Play
//! `obfuscatedAccountId`), so a receipt cannot be claimed by another account.
//!
//! Modules:
//! - [`apple`]: signed transactions (JWS), the App Store Server API, and
//!   Server Notifications V2.
//! - [`google`]: the Play Developer API and Pub/Sub push notifications.
//! - [`jose`] and [`x509`]: token and certificate chain verification.
pub mod apple;
pub mod google;
pub mod jose;
pub mod x509;

use crate::{
    App, billing,
    db::{self, now},
    error::{Error, Result},
    payments::provider_error,
};
use serde_json::{Value, json};
use sqlx::{SqliteConnection, sqlite::SqliteRow};
use std::{future::Future, time::Duration};

/// Waits before each retry of a store call that failed after the purchase
/// was already credited.
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(10),
    Duration::from_secs(60),
];
/// Most units of one consumable a single transaction may carry.
const MAX_QUANTITY: i64 = 100;

/// Caches shared by the store integrations.
#[derive(Default)]
pub struct Cache {
    /// Google's OIDC signing keys.
    pub(crate) jwks: std::sync::Mutex<google::Jwks>,
    /// The service account's current access token.
    pub(crate) token: tokio::sync::Mutex<Option<google::AccessToken>>,
}

/// Returns the account's store token, creating it on first use.
pub async fn store_token(app: &App, account: &str) -> Result<String> {
    sqlx::query("UPDATE accounts SET store_token=? WHERE id=? AND store_token IS NULL")
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(account)
        .execute(&app.db)
        .await?;
    sqlx::query_scalar::<_, Option<String>>("SELECT store_token FROM accounts WHERE id=?")
        .bind(account)
        .fetch_optional(&app.db)
        .await?
        .flatten()
        .ok_or_else(|| Error::not_found("account_not_found", "Account not found"))
}

/// `GET /v1/account/store`: what the app needs to start a purchase.
pub async fn account_info(app: &App, account: &str) -> Result<Value> {
    let token = store_token(app, account).await?;
    let config = app.config().await;
    let mut out = json!({
        "app_account_token": token,
        "obfuscated_account_id": token,
        "apple": {
            "enabled": config.apple.enabled(),
            "bundle_id": config.apple.bundle_id,
            "environment": config.apple.environment.as_apple(),
        },
        "google": {
            "enabled": config.google.enabled(),
            "package_name": config.google.package_name,
        },
    });
    let plans = sqlx::query(
        "SELECT * FROM plans WHERE active=1 AND (apple_product_id IS NOT NULL \
         OR google_product_id IS NOT NULL) ORDER BY kind='topup',price_micro,code",
    )
    .fetch_all(&app.db)
    .await?;
    out["plans"] = plans.iter().map(billing::plan_json).collect();
    Ok(out)
}

/// `PATCH /v1/account`: records or withdraws consent to share consumption
/// data with the stores when they ask about a refund request.
pub async fn set_consent(app: &App, account: &str, consent: bool) -> Result<Value> {
    sqlx::query("UPDATE accounts SET consumption_consent=?,consent_at=? WHERE id=?")
        .bind(consent)
        .bind(consent.then(now))
        .bind(account)
        .execute(&app.db)
        .await?;
    billing::account(app, account).await
}

/// Looks up the plan sold as a store product.
pub(crate) async fn plan_by_product(
    conn: &mut SqliteConnection,
    store: &str,
    product_id: &str,
    active_only: bool,
) -> Result<Option<SqliteRow>> {
    let sql = match store {
        "apple" => "SELECT * FROM plans WHERE apple_product_id=? AND (?=0 OR active=1)",
        _ => "SELECT * FROM plans WHERE google_product_id=? AND (?=0 OR active=1)",
    };
    Ok(sqlx::query(sql)
        .bind(product_id)
        .bind(active_only)
        .fetch_optional(conn)
        .await?)
}

/// The terms of `quantity` units of a plan. Only top-ups scale.
pub(crate) fn scaled_terms(plan: &SqliteRow, quantity: i64) -> Result<billing::Terms> {
    let mut terms = billing::Terms::from_row(plan, "code");
    if !terms.is_subscription() {
        if !(1..=MAX_QUANTITY).contains(&quantity) {
            return Err(Error::bad("Unsupported purchase quantity"));
        }
        let overflow = || Error::bad("Purchase quantity is too large");
        terms.price = terms.price.checked_mul(quantity).ok_or_else(overflow)?;
        terms.credit = terms.credit.checked_mul(quantity).ok_or_else(overflow)?;
    }
    Ok(terms)
}

/// Ends the subscription started by a store transaction or purchase token:
/// marks it refunded, stops its renewal, and zeroes its quota lots. With
/// `mark_payments` its paid orders become refunded too. The account is not
/// blocked.
pub(crate) async fn end_subscription(
    conn: &mut SqliteConnection,
    id: &str,
    mark_payments: bool,
) -> Result<()> {
    let t = now();
    let ended: Vec<String> = sqlx::query_scalar(
        "UPDATE subscriptions SET status='refunded',auto_renew=0,\
         expires_at=MAX(MIN(expires_at,?),period_start+1) \
         WHERE original_transaction_id=? OR purchase_token=? RETURNING id",
    )
    .bind(t)
    .bind(id)
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    for subscription in ended {
        sqlx::query("UPDATE credits SET remaining_micro=0 WHERE subscription_id=?")
            .bind(&subscription)
            .execute(&mut *conn)
            .await?;
        if mark_payments {
            sqlx::query(
                "UPDATE payments SET status='refunded',refunded_at=? \
                 WHERE subscription_id=? AND status='paid'",
            )
            .bind(t)
            .bind(&subscription)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// The account that owns a store subscription, by its original transaction
/// id (App Store) or purchase token (Google Play).
pub(crate) async fn subscription_owner(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT account_id FROM subscriptions WHERE original_transaction_id=? OR purchase_token=?",
    )
    .bind(id)
    .bind(id)
    .fetch_optional(conn)
    .await?)
}

/// Finds the account holding `store_token` (case-insensitive UUID).
pub(crate) async fn account_by_token(
    conn: &mut SqliteConnection,
    token: &str,
) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM accounts WHERE store_token=?")
            .bind(token.to_ascii_lowercase())
            .fetch_optional(conn)
            .await?,
    )
}

/// The response of a verified purchase: the account overview plus the order.
pub(crate) async fn purchase_response(
    app: &App,
    account: &str,
    order: &str,
    credited: bool,
) -> Result<Value> {
    let mut out = billing::account(app, account).await?;
    out["purchase"] = json!({"order_id": order, "credited": credited});
    Ok(out)
}

/// Runs a store call that must eventually succeed (acknowledging a Google
/// purchase) in the background, retrying a few times with growing delays.
/// A purchase the call never reaches is retried when a later notification
/// shows it is still pending.
pub(crate) fn retry_in_background<F, Fut>(app: &App, what: &'static str, call: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send,
{
    app.tasks.spawn(async move {
        for delay in RETRY_DELAYS {
            tokio::time::sleep(delay).await;
            match call().await {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!(call = what, error = %e.message, "store call failed, will retry")
                }
            }
        }
        tracing::error!(call = what, "store call kept failing");
    });
}

/// Runs a store call once now. When it fails, the purchase is already
/// credited, so the call is retried in the background instead of failing the
/// request.
pub(crate) async fn settle<F, Fut>(app: &App, what: &'static str, call: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send,
{
    if let Err(e) = call().await {
        tracing::warn!(call = what, error = %e.message, "store call failed, retrying in the background");
        retry_in_background(app, what, call);
    }
}

/// Maps a store HTTP status to the error a client or the store sees.
pub(crate) fn status_error(status: reqwest::StatusCode, what: &str) -> Error {
    tracing::warn!(%status, call = what, "store API call failed");
    if status == reqwest::StatusCode::NOT_FOUND {
        return Error::not_found(
            "store_purchase_not_found",
            "The store does not know this purchase",
        );
    }
    provider_error("The store rejected the request")
}

/// The `received` reply of a webhook.
pub(crate) fn received(duplicate: bool) -> Value {
    if duplicate {
        json!({"received": true, "duplicate": true})
    } else {
        json!({"received": true})
    }
}

/// Opens a write transaction that has already claimed `event`, or reports
/// the event as a duplicate.
pub(crate) async fn claim(
    app: &App,
    event: &str,
) -> Result<Option<sqlx::Transaction<'static, sqlx::Sqlite>>> {
    let mut tx = db::write(&app.db).await?;
    if super::first_delivery(&mut tx, event).await? {
        Ok(Some(tx))
    } else {
        Ok(None)
    }
}
