use crate::{
    App,
    db::{id, now},
    error::{Error, Result},
    identity, money,
};
use serde_json::{Value, json};
use sqlx::{Row, SqliteConnection};
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
    if !["free", "purchase", "admin", "refund", "subscription"].contains(&source) {
        return Err(Error::bad("Invalid credit source"));
    }
    let debt: i64 = sqlx::query_scalar("SELECT debt_micro FROM accounts WHERE id=?")
        .bind(account)
        .fetch_one(&mut *conn)
        .await?;
    let paid = amount.min(debt);
    sqlx::query("UPDATE accounts SET debt_micro=debt_micro-? WHERE id=?")
        .bind(paid)
        .bind(account)
        .execute(&mut *conn)
        .await?;
    sqlx::query("INSERT INTO credits(id,account_id,source,amount_micro,remaining_micro,expires_at,created_at,note) VALUES(?,?,?,?,?,?,?,?)").bind(id()).bind(account).bind(source).bind(amount).bind(amount-paid).bind(expires).bind(now()).bind(note).execute(conn).await?;
    Ok(())
}
pub async fn debit(conn: &mut SqliteConnection, account: &str, amount: i64) -> Result<()> {
    let rows=sqlx::query("SELECT id,remaining_micro FROM credits WHERE account_id=? AND remaining_micro>0 AND (expires_at IS NULL OR expires_at>?) ORDER BY expires_at IS NULL,expires_at,created_at,id").bind(account).bind(now()).fetch_all(&mut *conn).await?;
    let mut left = amount;
    for r in rows {
        if left == 0 {
            break;
        }
        let take = left.min(r.get("remaining_micro"));
        sqlx::query("UPDATE credits SET remaining_micro=remaining_micro-? WHERE id=?")
            .bind(take)
            .bind(r.get::<String, _>("id"))
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
pub async fn grant(conn: &mut SqliteConnection, account: &str, code: &str) -> Result<Value> {
    let p = sqlx::query("SELECT * FROM plans WHERE code=? AND active=1")
        .bind(code)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| Error::bad("Active plan not found"))?;
    grant_snapshot(
        conn,
        account,
        code,
        p.get("credit_micro"),
        p.get("duration_days"),
    )
    .await
}
pub async fn grant_snapshot(
    conn: &mut SqliteConnection,
    account: &str,
    code: &str,
    amount: i64,
    days: i64,
) -> Result<Value> {
    let expires = now() + days * 86400;
    let sid = id();
    credit(conn, account, amount, "subscription", Some(expires), code).await?;
    sqlx::query("INSERT INTO subscriptions(id,account_id,plan_code,started_at,expires_at) VALUES(?,?,?,?,?)").bind(&sid).bind(account).bind(code).bind(now()).bind(expires).execute(conn).await?;
    Ok(json!({"id":sid,"plan_code":code,"expires_at":expires}))
}
pub async fn account(app: &App, aid: &str) -> Result<Value> {
    let mut conn = app.db.acquire().await?;
    let r = sqlx::query("SELECT * FROM accounts WHERE id=?")
        .bind(aid)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| Error::new(404, "account_not_found", "Account not found"))?;
    let bal = crate::db::balance(&mut conn, aid).await?;
    let credits=sqlx::query("SELECT * FROM credits WHERE account_id=? ORDER BY created_at DESC LIMIT 200").bind(aid).fetch_all(&mut *conn).await?.iter().map(|r|json!({"id":r.get::<String,_>("id"),"source":r.get::<String,_>("source"),"amount_usd":money::display(r.get("amount_micro")),"remaining_usd":money::display(r.get("remaining_micro")),"expires_at":r.get::<Option<i64>,_>("expires_at")})).collect::<Vec<_>>();
    let sub=sqlx::query("SELECT plan_code,started_at,expires_at FROM subscriptions WHERE account_id=? AND expires_at>? ORDER BY expires_at DESC LIMIT 1").bind(aid).bind(now()).fetch_optional(&mut *conn).await?.map(|r|json!({"plan_code":r.get::<String,_>("plan_code"),"started_at":r.get::<i64,_>("started_at"),"expires_at":r.get::<i64,_>("expires_at")}));
    Ok(
        json!({"account":{"id":aid,"contact":r.get::<String,_>("contact"),"note":r.get::<String,_>("note"),"status":r.get::<String,_>("status"),"can_create_keys":r.get::<bool,_>("can_create_keys"),"markup_pct":r.get::<Option<String>,_>("markup_pct"),"balance_usd":money::display(bal),"debt_usd":money::display(r.get("debt_micro")),"created_at":r.get::<i64,_>("created_at"),"subscription":sub},"credits":credits}),
    )
}
pub async fn plans(app: &App, active: bool) -> Result<Value> {
    let rows = sqlx::query("SELECT * FROM plans WHERE (?=0 OR active=1) ORDER BY price_micro")
        .bind(active)
        .fetch_all(&app.db)
        .await?;
    Ok(
        json!({"plans":rows.iter().map(|r|json!({"code":r.get::<String,_>("code"),"name":r.get::<String,_>("name"),"price_usd":money::display(r.get("price_micro")),"credit_usd":money::display(r.get("credit_micro")),"duration_days":r.get::<i64,_>("duration_days"),"active":r.get::<bool,_>("active"),"description":r.get::<String,_>("description")})).collect::<Vec<_>>()}),
    )
}
pub async fn redeem(app: &App, account: &str, code: &str) -> Result<Value> {
    let c = app.config.read().await.clone();
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    let row=sqlx::query("SELECT * FROM redeem_codes WHERE code_hash=? AND redeemed_by IS NULL AND (expires_at IS NULL OR expires_at>?)").bind(identity::hash(&c.key_pepper,code.trim())).bind(now()).fetch_optional(&mut *tx).await?.ok_or_else(||Error::bad("Code is invalid, expired, or already redeemed"))?;
    if let Some(plan) = row.get::<Option<String>, _>("plan_code") {
        grant(&mut tx, account, &plan).await?;
    } else {
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
    sqlx::query("UPDATE redeem_codes SET redeemed_by=?,redeemed_at=? WHERE id=?")
        .bind(account)
        .bind(now())
        .bind(row.get::<String, _>("id"))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    self::account(app, account).await
}
