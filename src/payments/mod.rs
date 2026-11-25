//! Plan and top-up purchases.
//!
//! Every checkout creates a server-side order that snapshots the plan's kind,
//! price, credit (monthly quota or top-up credit), and months. A verified
//! provider event, or an administrator, completes the order. Completion
//! grants exactly that snapshot, once: a subscription with a monthly quota,
//! or non-expiring top-up credit.
//!
//! Providers:
//! - [`stripe`]: hosted card Checkout, completed by a signed webhook.
//! - [`payroof`]: watch-only crypto payments (stablecoins and native coins on
//!   EVM chains), completed by signed `confirmed` transfer webhooks.
//! - [`deposits`]: standing per-account Payroof addresses. Confirmed
//!   stablecoin transfers to them credit the account directly.
//! - [`store`]: native App Store and Google Play purchases, verified with
//!   the stores and kept current by their server notifications.
pub mod chains;
pub mod deposits;
pub mod payroof;
pub mod store;
pub mod stripe;

use crate::{
    App, billing,
    config::Config,
    db::{self, HOUR, now},
    error::{Error, Result},
    identity::Identity,
    money,
};
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};

/// Method name for card payments through Stripe.
pub const STRIPE: &str = "stripe";
/// Method name for crypto payments through Payroof.
pub const CRYPTO: &str = "crypto";

/// Payment methods currently available to customers, card first.
pub fn methods(config: &Config) -> Vec<&'static str> {
    let mut out = Vec::new();
    if config.stripe.enabled() {
        out.push(STRIPE);
    }
    if config.payroof.enabled() {
        out.push(CRYPTO);
    }
    out
}

/// Builds a 503 error for a payment method that is not available right now.
pub(crate) fn unavailable(message: &str) -> Error {
    Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "payments_unavailable",
        message,
    )
}

/// Builds a 502 error for a failed call to a payment provider.
pub(crate) fn provider_error(message: &str) -> Error {
    Error::new(StatusCode::BAD_GATEWAY, "payment_provider_error", message)
}

/// Starts a checkout for `{"plan_code", "method"}`. Without `method` the
/// first available method is used (card before crypto).
pub async fn checkout(app: &App, identity: &Identity, body: &Value) -> Result<Value> {
    let config = app.config().await;
    let available = methods(&config);
    if available.is_empty() {
        return Err(unavailable(
            "Online payments are not configured. Ask the operator for a redeem code.",
        ));
    }
    let method = match body.get("method") {
        None | Some(Value::Null) => available[0],
        Some(Value::String(m)) => available
            .iter()
            .copied()
            .find(|a| a == m)
            .ok_or_else(|| unavailable("This payment method is not available"))?,
        Some(_) => return Err(Error::bad("method must be a string")),
    };
    let code = body["plan_code"]
        .as_str()
        .ok_or_else(|| Error::bad("plan_code is required"))?;
    check_checkout_budget(app, &config, &identity.account_id).await?;
    let plan = sqlx::query("SELECT * FROM plans WHERE code=? AND active=1")
        .bind(code)
        .fetch_optional(&app.db)
        .await?
        .ok_or_else(|| Error::bad("Plan not found"))?;
    match method {
        STRIPE => stripe::checkout(app, &config, identity, &plan).await,
        _ => payroof::checkout(app, &config, identity, &plan).await,
    }
}

/// Caps checkout orders per account per hour, so a signed-in client cannot
/// spam the payment providers.
///
/// The count comes from stored orders, so it survives restarts.
async fn check_checkout_budget(app: &App, config: &Config, account: &str) -> Result<()> {
    let limit = config.rate_limits.checkouts_per_hour;
    if limit == 0 {
        return Ok(());
    }
    let (recent, oldest): (i64, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*),MIN(created_at) FROM payments WHERE account_id=? AND created_at>?",
    )
    .bind(account)
    .bind(now() - HOUR)
    .fetch_one(&app.db)
    .await?;
    if recent >= i64::from(limit) {
        return Err(Error::too_many(
            "checkout_rate_limited",
            "Too many checkout attempts. Try again later",
            oldest.map_or(60, |t| (t + HOUR - now()).max(1).unsigned_abs()),
        ));
    }
    Ok(())
}

/// Inserts a pending order snapshotting `plan`.
pub(crate) async fn create_order(
    conn: &mut SqliteConnection,
    order: &str,
    account: &str,
    plan: &SqliteRow,
    provider: &str,
    expires_at: Option<i64>,
) -> Result<()> {
    let terms = billing::Terms::from_row(plan, "code");
    sqlx::query(
        "INSERT INTO payments(id,account_id,plan_code,provider,kind,price_micro,credit_micro,months,\
         created_at,expires_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(order)
    .bind(account)
    .bind(&terms.code)
    .bind(provider)
    .bind(&terms.kind)
    .bind(terms.price)
    .bind(terms.credit)
    .bind(terms.months)
    .bind(now())
    .bind(expires_at)
    .execute(conn)
    .await?;
    Ok(())
}

/// Grants a paid order's snapshot: a subscription starting now (or, for a
/// store, over `period`), or a non-expiring top-up lot linked to the order.
async fn fulfil(
    conn: &mut SqliteConnection,
    order_id: &str,
    store: Option<&StoreCharge<'_>>,
) -> Result<()> {
    let order = sqlx::query("SELECT * FROM payments WHERE id=?")
        .bind(order_id)
        .fetch_one(&mut *conn)
        .await?;
    let account: String = order.get("account_id");
    let provider: String = order.get("provider");
    let terms = billing::Terms::from_row(&order, "plan_code");
    let Some(months) = terms.months else {
        let lot = billing::Lot {
            note: &terms.code,
            payment: Some(order_id),
            ..billing::Lot::default()
        };
        billing::credit_lot(conn, &account, terms.credit, "purchase", &lot).await?;
        return Ok(());
    };
    // A store renewal continues the subscription that the original
    // transaction (App Store) or purchase token (Google Play) started.
    let existing: Option<String> =
        match store {
            Some(charge) => sqlx::query_scalar(
                "SELECT id FROM subscriptions WHERE original_transaction_id=? OR purchase_token=?",
            )
            .bind(charge.original_transaction_id)
            .bind(charge.purchase_token)
            .fetch_optional(&mut *conn)
            .await?,
            None => None,
        };
    let subscription = match (existing, store) {
        (Some(subscription), Some(charge)) => {
            let period = charge
                .period
                .ok_or_else(|| Error::internal("store subscription without a period"))?;
            billing::renew(conn, &subscription, period, charge.auto_renew).await?;
            subscription
        }
        _ => {
            let start = billing::Subscription {
                plan: &terms.code,
                quota: terms.credit,
                months,
                provider: &provider,
                period: store.and_then(|c| c.period),
                auto_renew: store.is_some_and(|c| c.auto_renew),
                original_transaction_id: store.and_then(|c| c.original_transaction_id),
                purchase_token: store.and_then(|c| c.purchase_token),
                idempotency_key: None,
            };
            billing::subscribe(conn, &account, &start).await?
        }
    };
    sqlx::query("UPDATE payments SET subscription_id=? WHERE id=?")
        .bind(&subscription)
        .bind(order_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Marks a pending order paid and grants its snapshot. Returns `false` when
/// the order was already paid (nothing is granted twice).
pub(crate) async fn complete(
    conn: &mut SqliteConnection,
    order_id: &str,
    paid_by: &str,
) -> Result<bool> {
    let updated = sqlx::query(
        "UPDATE payments SET status='paid',paid_at=?,paid_by=? WHERE id=? AND status='pending'",
    )
    .bind(now())
    .bind(paid_by)
    .bind(order_id)
    .execute(&mut *conn)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(false);
    }
    fulfil(conn, order_id, None).await?;
    Ok(true)
}

/// A verified App Store or Google Play charge.
#[derive(Clone, Debug)]
pub struct StoreCharge<'a> {
    /// `apple` or `google`.
    pub provider: &'a str,
    pub account: &'a str,
    pub plan: &'a billing::Terms,
    /// App Store transactionId or Google Play orderId of this charge. Each
    /// subscription renewal is a new charge with its own id.
    pub external_id: &'a str,
    pub original_transaction_id: Option<&'a str>,
    pub purchase_token: Option<&'a str>,
    pub product_id: &'a str,
    pub environment: &'a str,
    /// Paid period of a subscription charge, from the store.
    pub period: Option<(i64, i64)>,
    pub auto_renew: bool,
}

/// Records a verified store charge as a paid order and grants it.
///
/// The order is keyed by `(provider, external_id)`, and a Google Play
/// one-time purchase token is unique as well, so a receipt or notification
/// delivered again (by the app or the store) grants nothing. Returns the
/// order id and whether this call recorded it. A purchase by an App Store
/// customer whose earlier subscription was refunded suspends the account.
pub async fn record_store_charge(
    conn: &mut SqliteConnection,
    charge: &StoreCharge<'_>,
    refund_ip_block_days: u32,
) -> Result<(String, bool)> {
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT id FROM payments WHERE (provider=? AND external_id=?) \
         OR (provider='google' AND kind='topup' AND purchase_token=? AND ?='topup')",
    )
    .bind(charge.provider)
    .bind(charge.external_id)
    .bind(charge.purchase_token)
    .bind(&charge.plan.kind)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(order) = previous {
        return Ok((order, false));
    }
    let order = db::id();
    let t = now();
    sqlx::query(
        "INSERT INTO payments(id,account_id,plan_code,provider,platform,kind,price_micro,credit_micro,\
         months,external_id,original_transaction_id,purchase_token,product_id,environment,status,\
         received_micro,created_at,paid_at,paid_by) \
         VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,'paid',?,?,?,?)",
    )
    .bind(&order)
    .bind(charge.account)
    .bind(&charge.plan.code)
    .bind(charge.provider)
    .bind(if charge.provider == "apple" { "ios" } else { "android" })
    .bind(&charge.plan.kind)
    .bind(charge.plan.price)
    .bind(charge.plan.credit)
    .bind(charge.plan.months)
    .bind(charge.external_id)
    .bind(charge.original_transaction_id)
    .bind(charge.purchase_token)
    .bind(charge.product_id)
    .bind(charge.environment)
    .bind(charge.plan.price)
    .bind(t)
    .bind(t)
    .bind(charge.provider)
    .execute(&mut *conn)
    .await?;
    fulfil(conn, &order, Some(charge)).await?;
    if let Some(original) = charge.original_transaction_id
        && billing::refund_blocked(conn, "apple_original_transaction", original).await?
    {
        billing::block_after_refund(conn, charge.account, refund_ip_block_days, Some(original))
            .await?;
    }
    Ok((order, true))
}

/// Marks the paid orders matching a refunded store charge as refunded and
/// blocks their account (see [`billing::block_after_refund`]). Returns the
/// affected account ids.
pub async fn refund_store_charge(
    conn: &mut SqliteConnection,
    provider: &str,
    external_id: Option<&str>,
    purchase_token: Option<&str>,
    refund_ip_block_days: u32,
) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "UPDATE payments SET status='refunded',refunded_at=? WHERE provider=? AND status='paid' \
         AND ((? IS NOT NULL AND external_id=?) OR (? IS NOT NULL AND purchase_token=?)) \
         RETURNING account_id,original_transaction_id",
    )
    .bind(now())
    .bind(provider)
    .bind(external_id)
    .bind(external_id)
    .bind(purchase_token)
    .bind(purchase_token)
    .fetch_all(&mut *conn)
    .await?;
    let mut accounts: Vec<String> = Vec::new();
    for row in rows {
        let account: String = row.get("account_id");
        let original: Option<String> = row.get("original_transaction_id");
        billing::block_after_refund(conn, &account, refund_ip_block_days, original.as_deref())
            .await?;
        if !accounts.contains(&account) {
            accounts.push(account);
        }
    }
    Ok(accounts)
}

/// Records a provider event id. Returns `false` when the event was already
/// processed.
pub(crate) async fn first_delivery(conn: &mut SqliteConnection, event: &str) -> Result<bool> {
    Ok(sqlx::query(
        "INSERT INTO payment_events(id,created_at) VALUES(?,?) ON CONFLICT(id) DO NOTHING",
    )
    .bind(event)
    .bind(now())
    .execute(conn)
    .await?
    .rows_affected()
        == 1)
}

/// Customer-facing payment method of a provider.
fn method(provider: &str) -> &'static str {
    match provider {
        "stripe" => STRIPE,
        "payroof" => CRYPTO,
        "apple" => "app_store",
        _ => "google_play",
    }
}

/// Customer-safe view of an order, with crypto quotes and transfers.
pub async fn order(app: &App, order_id: &str, account: Option<&str>) -> Result<Value> {
    let row = sqlx::query("SELECT * FROM payments WHERE id=? AND (? IS NULL OR account_id=?)")
        .bind(order_id)
        .bind(account)
        .bind(account)
        .fetch_optional(&app.db)
        .await?
        .ok_or_else(|| Error::not_found("payment_not_found", "Payment not found"))?;
    let mut quotes: Vec<Value> =
        sqlx::query("SELECT * FROM payment_quotes WHERE payment_id=? ORDER BY chain_name,symbol")
            .bind(order_id)
            .fetch_all(&app.db)
            .await?
            .iter()
            .map(payroof::quote_json)
            .collect();
    let transfers: Vec<Value> =
        sqlx::query("SELECT * FROM payment_transfers WHERE payment_id=? ORDER BY seen_at")
            .bind(order_id)
            .fetch_all(&app.db)
            .await?
            .iter()
            .map(payroof::transfer_json)
            .collect();
    let mut out = summary(&row);
    if row.get::<String, _>("provider") == "payroof" {
        // Confirmations come from the cached catalog, so a Payroof outage
        // only drops this hint.
        let config = app.config().await;
        let catalog = match config.payroof.enabled() {
            true => payroof::catalog(app, &config.payroof).await.ok(),
            false => None,
        };
        for quote in &mut quotes {
            let confirmations = catalog.as_ref().and_then(|chains| {
                chains
                    .iter()
                    .find(|c| quote["chain"] == c.id.as_str())
                    .map(|c| c.confirmations)
            });
            quote["confirmations"] = confirmations.into();
        }
        out["underpayment_tolerance_pct"] =
            config.payroof.underpayment_tolerance_pct.clone().into();
    }
    out["quotes"] = quotes.into();
    out["transfers"] = transfers.into();
    Ok(out)
}

/// List view of a `payments` row.
///
/// A pending order past its expiry is reported as `expired`.
pub fn summary(r: &SqliteRow) -> Value {
    let expires_at: Option<i64> = r.get("expires_at");
    let status: String = r.get("status");
    let expired = status == "pending" && expires_at.is_some_and(|t| t <= now());
    json!({
        "id": r.get::<String, _>("id"),
        "account_id": r.get::<String, _>("account_id"),
        "plan_code": r.get::<String, _>("plan_code"),
        "kind": r.get::<String, _>("kind"),
        "provider": r.get::<String, _>("provider"),
        "platform": r.get::<Option<String>, _>("platform"),
        "method": method(&r.get::<String, _>("provider")),
        "price_usd": money::display(r.get("price_micro")),
        "credit_usd": money::display(r.get("credit_micro")),
        "months": r.get::<Option<i64>, _>("months"),
        "product_id": r.get::<Option<String>, _>("product_id"),
        "refunded_at": r.get::<Option<i64>, _>("refunded_at"),
        "received_usd": money::display(r.get("received_micro")),
        "status": if expired { "expired" } else { status.as_str() },
        "created_at": r.get::<i64, _>("created_at"),
        "expires_at": expires_at,
        "paid_at": r.get::<Option<i64>, _>("paid_at"),
        "paid_by": r.get::<Option<String>, _>("paid_by"),
    })
}

/// Lists up to 50 of the latest orders for one account.
pub async fn account_orders(app: &App, account: &str) -> Result<Value> {
    let rows = sqlx::query(
        "SELECT * FROM payments WHERE account_id=? ORDER BY created_at DESC,id DESC LIMIT 50",
    )
    .bind(account)
    .fetch_all(&app.db)
    .await?;
    Ok(json!({"payments": rows.iter().map(summary).collect::<Vec<_>>()}))
}

/// Administrator override: completes a pending order (for example a late or
/// slightly underpaid crypto payment that was verified by hand).
pub async fn admin_complete(app: &App, order_id: &str) -> Result<Value> {
    let mut tx = db::write(&app.db).await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payments WHERE id=?)")
        .bind(order_id)
        .fetch_one(&mut *tx)
        .await?;
    if !exists {
        return Err(Error::not_found("payment_not_found", "Payment not found"));
    }
    if !complete(&mut tx, order_id, "admin").await? {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "payment_already_paid",
            "Payment is already paid",
        ));
    }
    tx.commit().await?;
    order(app, order_id, None).await
}
