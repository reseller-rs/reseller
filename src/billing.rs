//! Credit ledger, plans, subscriptions, and redeem codes.
//!
//! Credit is held in lots (`credits` rows) that may expire. Debits consume the
//! soonest-expiring lots first; any shortfall becomes account debt, which is
//! repaid from the next credit before a new lot is created.
use crate::{
    App,
    db::{self, DAY, id, now},
    error::{Error, Result},
    identity, money,
};
use serde_json::{Value, json};
use sqlx::{Row, SqliteConnection};

/// Allowed values of `credits.source`.
pub const CREDIT_SOURCES: [&str; 5] = ["free", "purchase", "admin", "refund", "subscription"];

/// Adds a credit lot of `amount` micro-USD, repaying outstanding debt first.
pub async fn credit(
    conn: &mut SqliteConnection,
    account: &str,
    amount: i64,
    source: &str,
    expires: Option<i64>,
    note: &str,
) -> Result<()> {
    if amount <= 0 || expires.is_some_and(|e| e <= now()) {
        return Err(Error::bad("Credit must be positive and not expired"));
    }
    if !CREDIT_SOURCES.contains(&source) {
        return Err(Error::bad("Invalid credit source"));
    }
    let debt: i64 = sqlx::query_scalar("SELECT debt_micro FROM accounts WHERE id=?")
        .bind(account)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| Error::not_found("account_not_found", "Account not found"))?;
    let repaid = amount.min(debt);
    sqlx::query("UPDATE accounts SET debt_micro=debt_micro-? WHERE id=?")
        .bind(repaid)
        .bind(account)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO credits(id,account_id,source,amount_micro,remaining_micro,expires_at,created_at,note) \
         VALUES(?,?,?,?,?,?,?,?)",
    )
    .bind(id())
    .bind(account)
    .bind(source)
    .bind(amount)
    .bind(amount - repaid)
    .bind(expires)
    .bind(now())
    .bind(note)
    .execute(conn)
    .await?;
    Ok(())
}

/// Consumes `amount` micro-USD from unexpired lots (earliest expiry first);
/// the remainder is recorded as debt.
pub async fn debit(conn: &mut SqliteConnection, account: &str, amount: i64) -> Result<()> {
    let lots = sqlx::query(
        "SELECT id,remaining_micro FROM credits \
         WHERE account_id=? AND remaining_micro>0 AND (expires_at IS NULL OR expires_at>?) \
         ORDER BY expires_at IS NULL,expires_at,created_at,id",
    )
    .bind(account)
    .bind(now())
    .fetch_all(&mut *conn)
    .await?;
    let mut left = amount;
    for lot in lots {
        if left <= 0 {
            break;
        }
        let take = left.min(lot.get("remaining_micro"));
        sqlx::query("UPDATE credits SET remaining_micro=remaining_micro-? WHERE id=?")
            .bind(take)
            .bind(lot.get::<String, _>("id"))
            .execute(&mut *conn)
            .await?;
        left -= take;
    }
    if left > 0 {
        sqlx::query("UPDATE accounts SET debt_micro=debt_micro+? WHERE id=?")
            .bind(left)
            .bind(account)
            .execute(conn)
            .await?;
    }
    Ok(())
}

/// Grants an active plan at its current terms.
pub async fn grant(conn: &mut SqliteConnection, account: &str, plan: &str) -> Result<Value> {
    let row = sqlx::query("SELECT credit_micro,duration_days FROM plans WHERE code=? AND active=1")
        .bind(plan)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| Error::bad("Active plan not found"))?;
    grant_snapshot(
        conn,
        account,
        plan,
        row.get("credit_micro"),
        row.get("duration_days"),
    )
    .await
}

/// Grants plan credit using terms captured earlier (for example at checkout),
/// so later plan edits do not change what a customer paid for.
pub async fn grant_snapshot(
    conn: &mut SqliteConnection,
    account: &str,
    plan: &str,
    amount: i64,
    days: i64,
) -> Result<Value> {
    let started = now();
    let expires = started + days * DAY;
    let subscription = id();
    credit(conn, account, amount, "subscription", Some(expires), plan).await?;
    sqlx::query(
        "INSERT INTO subscriptions(id,account_id,plan_code,started_at,expires_at) VALUES(?,?,?,?,?)",
    )
    .bind(&subscription)
    .bind(account)
    .bind(plan)
    .bind(started)
    .bind(expires)
    .execute(conn)
    .await?;
    Ok(json!({"id": subscription, "plan_code": plan, "expires_at": expires}))
}

/// Account summary with balance, current subscription, and recent credit lots.
pub async fn account(app: &App, aid: &str) -> Result<Value> {
    let mut conn = app.db.acquire().await?;
    let row = sqlx::query("SELECT * FROM accounts WHERE id=?")
        .bind(aid)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| Error::not_found("account_not_found", "Account not found"))?;
    let balance = db::balance(&mut conn, aid).await?;
    let credits: Vec<Value> =
        sqlx::query("SELECT * FROM credits WHERE account_id=? ORDER BY created_at DESC LIMIT 200")
            .bind(aid)
            .fetch_all(&mut *conn)
            .await?
            .iter()
            .map(|r| {
                json!({
                    "id": r.get::<String, _>("id"),
                    "source": r.get::<String, _>("source"),
                    "amount_usd": money::display(r.get("amount_micro")),
                    "remaining_usd": money::display(r.get("remaining_micro")),
                    "expires_at": r.get::<Option<i64>, _>("expires_at"),
                })
            })
            .collect();
    let subscription = sqlx::query(
        "SELECT plan_code,started_at,expires_at FROM subscriptions \
         WHERE account_id=? AND expires_at>? ORDER BY expires_at DESC LIMIT 1",
    )
    .bind(aid)
    .bind(now())
    .fetch_optional(&mut *conn)
    .await?
    .map(|r| {
        json!({
            "plan_code": r.get::<String, _>("plan_code"),
            "started_at": r.get::<i64, _>("started_at"),
            "expires_at": r.get::<i64, _>("expires_at"),
        })
    });
    Ok(json!({
        "account": {
            "id": aid,
            "contact": row.get::<String, _>("contact"),
            "note": row.get::<String, _>("note"),
            "status": row.get::<String, _>("status"),
            "can_create_keys": row.get::<bool, _>("can_create_keys"),
            "markup_pct": row.get::<Option<String>, _>("markup_pct"),
            "balance_usd": money::display(balance),
            "debt_usd": money::display(row.get("debt_micro")),
            "created_at": row.get::<i64, _>("created_at"),
            "subscription": subscription,
        },
        "credits": credits,
    }))
}

/// Plan catalogue ordered by price; `active_only` hides retired plans.
pub async fn plans(app: &App, active_only: bool) -> Result<Value> {
    let rows = sqlx::query("SELECT * FROM plans WHERE (?=0 OR active=1) ORDER BY price_micro")
        .bind(active_only)
        .fetch_all(&app.db)
        .await?;
    let plans: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "code": r.get::<String, _>("code"),
                "name": r.get::<String, _>("name"),
                "price_usd": money::display(r.get("price_micro")),
                "credit_usd": money::display(r.get("credit_micro")),
                "duration_days": r.get::<i64, _>("duration_days"),
                "active": r.get::<bool, _>("active"),
                "description": r.get::<String, _>("description"),
            })
        })
        .collect();
    Ok(json!({"plans": plans}))
}

/// Redeems a single-use code (plan grant or plain credit) atomically.
pub async fn redeem(app: &App, account: &str, code: &str) -> Result<Value> {
    let config = app.config().await;
    let mut tx = db::write(&app.db).await?;
    let row = sqlx::query(
        "SELECT id,plan_code,credit_micro FROM redeem_codes \
         WHERE code_hash=? AND redeemed_by IS NULL AND (expires_at IS NULL OR expires_at>?)",
    )
    .bind(identity::hash(&config.key_pepper, code.trim()))
    .bind(now())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| Error::bad("Code is invalid, expired, or already redeemed"))?;
    match row.get::<Option<String>, _>("plan_code") {
        Some(plan) => {
            grant(&mut tx, account, &plan).await?;
        }
        None => {
            credit(
                &mut tx,
                account,
                row.get("credit_micro"),
                "purchase",
                None,
                "Redeem code",
            )
            .await?;
        }
    }
    sqlx::query("UPDATE redeem_codes SET redeemed_by=?,redeemed_at=? WHERE id=?")
        .bind(account)
        .bind(now())
        .bind(row.get::<String, _>("id"))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    self::account(app, account).await
}
