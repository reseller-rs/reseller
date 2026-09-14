//! Stripe Checkout: price snapshots and grants are bound to server-side orders.
use crate::{
    App,
    db::{id, now},
    error::{Error, Result},
    identity::Identity,
};
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::Row;

pub async fn checkout(app: &App, i: &Identity, b: &Value) -> Result<Value> {
    let c = app.config.read().await.clone();
    if c.stripe_secret_key.is_empty() || c.stripe_webhook_secret.is_empty() {
        return Err(Error::new(
            503,
            "payments_unavailable",
            "Online payments are not configured. Ask the operator for a redeem code.",
        ));
    }
    let code = b["plan_code"]
        .as_str()
        .ok_or_else(|| Error::bad("plan_code is required"))?;
    let p = sqlx::query("SELECT * FROM plans WHERE code=? AND active=1")
        .bind(code)
        .fetch_optional(&app.db)
        .await?
        .ok_or_else(|| Error::bad("Plan not found"))?;
    let price: i64 = p.get("price_micro");
    if price % 10000 != 0 || price < 500000 {
        return Err(Error::bad(
            "Stripe plans must be at least $0.50 and priced in whole cents",
        ));
    }
    let pid = id();
    sqlx::query("INSERT INTO payments(id,account_id,plan_code,price_micro,credit_micro,duration_days,created_at) VALUES(?,?,?,?,?,?,?)")
 .bind(&pid).bind(&i.account_id).bind(code).bind(price).bind(p.get::<i64,_>("credit_micro")).bind(p.get::<i64,_>("duration_days")).bind(now()).execute(&app.db).await?;
    let form = [
        ("mode", "payment".to_owned()),
        ("client_reference_id", pid.clone()),
        ("metadata[order_id]", pid.clone()),
        (
            "success_url",
            format!(
                "{}/workspace/?payment=success",
                c.public_url.trim_end_matches('/')
            ),
        ),
        (
            "cancel_url",
            format!(
                "{}/workspace/?payment=cancelled",
                c.public_url.trim_end_matches('/')
            ),
        ),
        ("line_items[0][price_data][currency]", "usd".into()),
        (
            "line_items[0][price_data][unit_amount]",
            (price / 10000).to_string(),
        ),
        (
            "line_items[0][price_data][product_data][name]",
            p.get::<String, _>("name"),
        ),
        ("line_items[0][quantity]", "1".into()),
    ];
    let response = app
        .client
        .post("https://api.stripe.com/v1/checkout/sessions")
        .bearer_auth(&c.stripe_secret_key)
        .header("Idempotency-Key", &pid)
        .form(&form)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| {
            Error::new(
                502,
                "payment_provider_error",
                "Could not contact payment provider",
            )
        })?;
    if !response.status().is_success() {
        return Err(Error::new(
            502,
            "payment_provider_error",
            "Payment provider rejected checkout. Check the server's Stripe configuration.",
        ));
    }
    let v: Value = response.json().await.map_err(Error::internal)?;
    let session = v["id"]
        .as_str()
        .ok_or_else(|| Error::internal("Stripe response missing id"))?;
    let url = v["url"]
        .as_str()
        .ok_or_else(|| Error::internal("Stripe response missing URL"))?;
    sqlx::query(
        "UPDATE payments SET session_id=? WHERE id=? AND (session_id IS NULL OR session_id=?)",
    )
    .bind(session)
    .bind(&pid)
    .bind(session)
    .execute(&app.db)
    .await?;
    Ok(json!({"url":url,"order_id":pid}))
}
pub fn verify(secret: &str, header: &str, body: &[u8], time: i64) -> Result<()> {
    let fail = || {
        Error::new(
            400,
            "invalid_signature",
            "Invalid or expired Stripe signature",
        )
    };
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for entry in header.split(',') {
        if let Some((key, value)) = entry.trim().split_once('=') {
            match key {
                "t" => {
                    if timestamp.is_some() {
                        return Err(fail());
                    }
                    timestamp = Some(value);
                }
                "v1" => signatures.push(value),
                _ => {}
            }
        }
    }
    let stamp = timestamp.ok_or_else(fail)?;
    let t: i64 = stamp.parse().map_err(|_| fail())?;
    if time.abs_diff(t) > 300 || secret.is_empty() {
        return Err(fail());
    }
    for sig in signatures {
        let Ok(bytes) = hex::decode(sig) else {
            continue;
        };
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| fail())?;
        mac.update(stamp.as_bytes());
        mac.update(b".");
        mac.update(body);
        if mac.verify_slice(&bytes).is_ok() {
            return Ok(());
        }
    }
    Err(fail())
}
pub async fn webhook(app: &App, headers: &HeaderMap, body: &[u8]) -> Result<Value> {
    let c = app.config.read().await.clone();
    let signature = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    verify(&c.stripe_webhook_secret, signature, body, now())?;
    let event: Value = serde_json::from_slice(body)?;
    let event_id = event["id"]
        .as_str()
        .ok_or_else(|| Error::bad("Event id required"))?;
    let kind = event["type"].as_str().unwrap_or("");
    if ![
        "checkout.session.completed",
        "checkout.session.async_payment_succeeded",
    ]
    .contains(&kind)
    {
        return Ok(json!({"received":true}));
    }
    let session = &event["data"]["object"];
    if session["payment_status"] != "paid" {
        return Ok(json!({"received":true}));
    }
    let pid = session["client_reference_id"]
        .as_str()
        .ok_or_else(|| Error::bad("Order reference missing"))?;
    let sid = session["id"]
        .as_str()
        .ok_or_else(|| Error::bad("Session id missing"))?;
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    let inserted = sqlx::query(
        "INSERT INTO payment_events(id,created_at) VALUES(?,?) ON CONFLICT(id) DO NOTHING",
    )
    .bind(event_id)
    .bind(now())
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Ok(json!({"received":true,"duplicate":true}));
    }
    let order = sqlx::query("SELECT * FROM payments WHERE id=?")
        .bind(pid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::bad("Unknown order"))?;
    if session["currency"] != "usd"
        || session["mode"] != "payment"
        || session["amount_total"].as_i64() != Some(order.get::<i64, _>("price_micro") / 10000)
        || session["metadata"]["order_id"] != pid
        || order
            .get::<Option<String>, _>("session_id")
            .is_some_and(|s| s != sid)
    {
        return Err(Error::bad("Payment does not match order"));
    }
    if order.get::<String, _>("status") != "paid" {
        crate::billing::grant_snapshot(
            &mut tx,
            order.get("account_id"),
            order.get("plan_code"),
            order.get("credit_micro"),
            order.get("duration_days"),
        )
        .await?;
        sqlx::query("UPDATE payments SET status='paid',paid_at=?,session_id=? WHERE id=?")
            .bind(now())
            .bind(sid)
            .bind(pid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(json!({"received":true}))
}
