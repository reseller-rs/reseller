//! Plan purchases. Every checkout creates a server-side order that snapshots
//! the plan's price, credit, and duration; a verified provider event (or an
//! administrator) completes the order and grants exactly that snapshot, once.
//!
//! Providers:
//! - [`stripe`]: hosted card Checkout, completed by a signed webhook.
//! - [`payroof`]: watch-only crypto payments (stablecoins and native coins on
//!   EVM chains), completed by signed `confirmed` transfer webhooks.
pub mod payroof;
pub mod stripe;

use crate::{
    App, billing,
    config::Config,
    db::{self, now},
    error::{Error, Result},
    identity::Identity,
    money,
};
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};

/// Customer-facing payment methods.
pub const STRIPE: &str = "stripe";
pub const CRYPTO: &str = "crypto";

/// Payment methods currently available to customers.
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

pub(crate) fn unavailable(message: &str) -> Error {
    Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "payments_unavailable",
        message,
    )
}

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

/// Inserts a pending order snapshotting `plan`.
pub(crate) async fn create_order(
    conn: &mut SqliteConnection,
    order: &str,
    account: &str,
    plan: &SqliteRow,
    provider: &str,
    expires_at: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO payments(id,account_id,plan_code,provider,price_micro,credit_micro,duration_days,\
         created_at,expires_at) VALUES(?,?,?,?,?,?,?,?,?)",
    )
    .bind(order)
    .bind(account)
    .bind(plan.get::<String, _>("code"))
    .bind(provider)
    .bind(plan.get::<i64, _>("price_micro"))
    .bind(plan.get::<i64, _>("credit_micro"))
    .bind(plan.get::<i64, _>("duration_days"))
    .bind(now())
    .bind(expires_at)
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
    let order = sqlx::query(
        "SELECT account_id,plan_code,credit_micro,duration_days FROM payments WHERE id=?",
    )
    .bind(order_id)
    .fetch_one(&mut *conn)
    .await?;
    billing::grant_snapshot(
        conn,
        order.get("account_id"),
        order.get("plan_code"),
        order.get("credit_micro"),
        order.get("duration_days"),
    )
    .await?;
    Ok(true)
}

/// Records a provider event id; `false` means it was already processed.
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

/// Customer-safe view of an order, with crypto quotes and transfers.
pub async fn order(app: &App, order_id: &str, account: Option<&str>) -> Result<Value> {
    let row = sqlx::query("SELECT * FROM payments WHERE id=? AND (? IS NULL OR account_id=?)")
        .bind(order_id)
        .bind(account)
        .bind(account)
        .fetch_optional(&app.db)
        .await?
        .ok_or_else(|| Error::not_found("payment_not_found", "Payment not found"))?;
    let quotes: Vec<Value> =
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
    out["quotes"] = quotes.into();
    out["transfers"] = transfers.into();
    Ok(out)
}

/// List view of a `payments` row.
pub fn summary(r: &SqliteRow) -> Value {
    let expires_at: Option<i64> = r.get("expires_at");
    let status: String = r.get("status");
    let expired = status == "pending" && expires_at.is_some_and(|t| t <= now());
    json!({
        "id": r.get::<String, _>("id"),
        "account_id": r.get::<String, _>("account_id"),
        "plan_code": r.get::<String, _>("plan_code"),
        "provider": r.get::<String, _>("provider"),
        "method": if r.get::<String, _>("provider") == "stripe" { STRIPE } else { CRYPTO },
        "price_usd": money::display(r.get("price_micro")),
        "credit_usd": money::display(r.get("credit_micro")),
        "duration_days": r.get::<i64, _>("duration_days"),
        "received_usd": money::display(r.get("received_micro")),
        "status": if expired { "expired" } else { status.as_str() },
        "created_at": r.get::<i64, _>("created_at"),
        "expires_at": expires_at,
        "paid_at": r.get::<Option<i64>, _>("paid_at"),
        "paid_by": r.get::<Option<String>, _>("paid_by"),
    })
}

/// The latest orders of one account.
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
