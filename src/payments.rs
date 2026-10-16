//! Stripe Checkout. Each checkout creates a server-side order that snapshots
//! the plan's price, credit, and duration; the signed webhook grants exactly
//! that snapshot, once.
use crate::{
    App, billing,
    db::{self, id, now},
    error::{Error, Result},
    identity::Identity,
};
use axum::http::{HeaderMap, StatusCode};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::Row;
use std::time::Duration;

const CHECKOUT_URL: &str = "https://api.stripe.com/v1/checkout/sessions";
/// Accepted clock skew for webhook signatures, in seconds.
const SIGNATURE_TOLERANCE: u64 = 300;
const MICROS_PER_CENT: i64 = 10_000;
/// Stripe's minimum USD charge.
const MIN_CHARGE_MICROS: i64 = 500_000;
/// Event types that may complete a paid checkout.
const PAID_EVENTS: [&str; 2] = [
    "checkout.session.completed",
    "checkout.session.async_payment_succeeded",
];

fn provider_error(message: &str) -> Error {
    Error::new(StatusCode::BAD_GATEWAY, "payment_provider_error", message)
}

/// Creates an order and a Stripe Checkout session for an active plan.
pub async fn checkout(app: &App, identity: &Identity, body: &Value) -> Result<Value> {
    let config = app.config().await;
    if config.stripe_secret_key.is_empty() || config.stripe_webhook_secret.is_empty() {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "payments_unavailable",
            "Online payments are not configured. Ask the operator for a redeem code.",
        ));
    }
    let code = body["plan_code"]
        .as_str()
        .ok_or_else(|| Error::bad("plan_code is required"))?;
    let plan = sqlx::query("SELECT * FROM plans WHERE code=? AND active=1")
        .bind(code)
        .fetch_optional(&app.db)
        .await?
        .ok_or_else(|| Error::bad("Plan not found"))?;
    let price: i64 = plan.get("price_micro");
    if price % MICROS_PER_CENT != 0 || price < MIN_CHARGE_MICROS {
        return Err(Error::bad(
            "Stripe plans must be at least $0.50 and priced in whole cents",
        ));
    }
    let order = id();
    sqlx::query(
        "INSERT INTO payments(id,account_id,plan_code,price_micro,credit_micro,duration_days,created_at) \
         VALUES(?,?,?,?,?,?,?)",
    )
    .bind(&order)
    .bind(&identity.account_id)
    .bind(code)
    .bind(price)
    .bind(plan.get::<i64, _>("credit_micro"))
    .bind(plan.get::<i64, _>("duration_days"))
    .bind(now())
    .execute(&app.db)
    .await?;
    let base = config.public_base();
    let form = [
        ("mode", "payment".to_owned()),
        ("client_reference_id", order.clone()),
        ("metadata[order_id]", order.clone()),
        ("success_url", format!("{base}/workspace/?payment=success")),
        ("cancel_url", format!("{base}/workspace/?payment=cancelled")),
        ("line_items[0][price_data][currency]", "usd".into()),
        (
            "line_items[0][price_data][unit_amount]",
            (price / MICROS_PER_CENT).to_string(),
        ),
        (
            "line_items[0][price_data][product_data][name]",
            plan.get::<String, _>("name"),
        ),
        ("line_items[0][quantity]", "1".into()),
    ];
    let response = app
        .client
        .post(CHECKOUT_URL)
        .bearer_auth(&config.stripe_secret_key)
        .header("Idempotency-Key", &order)
        .form(&form)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| provider_error("Could not contact payment provider"))?;
    if !response.status().is_success() {
        tracing::warn!(status = %response.status(), order = %order, "Stripe rejected checkout");
        return Err(provider_error(
            "Payment provider rejected checkout. Check the server's Stripe configuration.",
        ));
    }
    let session: Value = response.json().await.map_err(Error::internal)?;
    let session_id = session["id"]
        .as_str()
        .ok_or_else(|| Error::internal("Stripe response missing id"))?;
    let url = session["url"]
        .as_str()
        .ok_or_else(|| Error::internal("Stripe response missing URL"))?;
    sqlx::query(
        "UPDATE payments SET session_id=? WHERE id=? AND (session_id IS NULL OR session_id=?)",
    )
    .bind(session_id)
    .bind(&order)
    .bind(session_id)
    .execute(&app.db)
    .await?;
    Ok(json!({"url": url, "order_id": order}))
}

/// Verifies a `Stripe-Signature` header (`t=…,v1=…`) against `body` at Unix
/// time `now`.
pub fn verify(secret: &str, header: &str, body: &[u8], now: i64) -> Result<()> {
    let fail = || {
        Error::new(
            StatusCode::BAD_REQUEST,
            "invalid_signature",
            "Invalid or expired Stripe signature",
        )
    };
    if secret.is_empty() {
        return Err(fail());
    }
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for (key, value) in header
        .split(',')
        .filter_map(|entry| entry.trim().split_once('='))
    {
        match key {
            "t" if timestamp.is_some() => return Err(fail()),
            "t" => timestamp = Some(value),
            "v1" => signatures.push(value),
            _ => {}
        }
    }
    let stamp = timestamp.ok_or_else(fail)?;
    let signed_at: i64 = stamp.parse().map_err(|_| fail())?;
    if now.abs_diff(signed_at) > SIGNATURE_TOLERANCE {
        return Err(fail());
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| fail())?;
    mac.update(stamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    let valid = signatures
        .into_iter()
        .filter_map(|sig| hex::decode(sig).ok())
        .any(|sig| mac.clone().verify_slice(&sig).is_ok());
    if valid { Ok(()) } else { Err(fail()) }
}

/// Handles a Stripe webhook: verifies it, deduplicates by event id, checks the
/// session against the stored order, and grants the order's snapshot.
pub async fn webhook(app: &App, headers: &HeaderMap, body: &[u8]) -> Result<Value> {
    let config = app.config().await;
    let signature = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    verify(&config.stripe_webhook_secret, signature, body, now())?;
    let event: Value = serde_json::from_slice(body)?;
    let event_id = event["id"]
        .as_str()
        .ok_or_else(|| Error::bad("Event id required"))?;
    let session = &event["data"]["object"];
    let kind = event["type"].as_str().unwrap_or("");
    if !PAID_EVENTS.contains(&kind) || session["payment_status"] != "paid" {
        return Ok(json!({"received": true}));
    }
    let order_id = session["client_reference_id"]
        .as_str()
        .ok_or_else(|| Error::bad("Order reference missing"))?;
    let session_id = session["id"]
        .as_str()
        .ok_or_else(|| Error::bad("Session id missing"))?;
    let mut tx = db::write(&app.db).await?;
    let inserted = sqlx::query(
        "INSERT INTO payment_events(id,created_at) VALUES(?,?) ON CONFLICT(id) DO NOTHING",
    )
    .bind(event_id)
    .bind(now())
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Ok(json!({"received": true, "duplicate": true}));
    }
    let order = sqlx::query("SELECT * FROM payments WHERE id=?")
        .bind(order_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::bad("Unknown order"))?;
    let expected_cents = order.get::<i64, _>("price_micro") / MICROS_PER_CENT;
    if session["currency"] != "usd"
        || session["mode"] != "payment"
        || session["amount_total"].as_i64() != Some(expected_cents)
        || session["metadata"]["order_id"] != order_id
        || order
            .get::<Option<String>, _>("session_id")
            .is_some_and(|s| s != session_id)
    {
        // Dropping the transaction also forgets the event id, so a corrected
        // retry is not mistaken for a duplicate.
        return Err(Error::bad("Payment does not match order"));
    }
    if order.get::<String, _>("status") != "paid" {
        billing::grant_snapshot(
            &mut tx,
            order.get("account_id"),
            order.get("plan_code"),
            order.get("credit_micro"),
            order.get("duration_days"),
        )
        .await?;
        sqlx::query("UPDATE payments SET status='paid',paid_at=?,session_id=? WHERE id=?")
            .bind(now())
            .bind(session_id)
            .bind(order_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(json!({"received": true}))
}
