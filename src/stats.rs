//! Dashboard statistics, usage series, and log retention.
use crate::{
    App,
    db::{DAY, now},
    error::Result,
    money,
};
use serde_json::{Value, json};
use sqlx::Row;

/// Usage buckets are kept a little over a year for yearly charts.
const BUCKET_RETENTION_DAYS: i64 = 367;

/// Platform-wide totals for the administrator overview.
///
/// Credit and debt are split by account status: suspended accounts cannot
/// spend, so their unexpired credit is reported apart from the credit active
/// customers can still draw on. `active_keys` counts keys that can call the
/// proxy right now (active, unexpired, on an active account).
pub async fn overview(app: &App) -> Result<Value> {
    let now = now();
    let row = sqlx::query(
        "SELECT t.requests,t.tokens,t.upstream_micro,t.billed_micro,\
           (SELECT COUNT(*) FROM accounts) AS accounts,\
           (SELECT COUNT(*) FROM accounts WHERE status='suspended') AS suspended_accounts,\
           (SELECT COUNT(*) FROM api_keys k JOIN accounts a ON a.id=k.account_id \
              WHERE k.status='active' AND a.status='active' \
              AND (k.expires_at IS NULL OR k.expires_at>?1)) AS active_keys,\
           (SELECT COALESCE(SUM(c.remaining_micro),0) FROM credits c JOIN accounts a ON a.id=c.account_id \
              WHERE a.status='active' AND (c.expires_at IS NULL OR c.expires_at>?1)) AS outstanding_micro,\
           (SELECT COALESCE(SUM(c.remaining_micro),0) FROM credits c JOIN accounts a ON a.id=c.account_id \
              WHERE a.status='suspended' AND (c.expires_at IS NULL OR c.expires_at>?1)) AS suspended_micro,\
           (SELECT COALESCE(SUM(debt_micro),0) FROM accounts) AS debt_micro \
         FROM totals t WHERE t.id=1",
    )
    .bind(now)
    .fetch_one(&app.db)
    .await?;
    let upstream: i64 = row.get("upstream_micro");
    let billed: i64 = row.get("billed_micro");
    Ok(json!({
        "requests": row.get::<i64, _>("requests"),
        "tokens": row.get::<i64, _>("tokens"),
        "upstream_cost_usd": money::display(upstream),
        "billed_cost_usd": money::display(billed),
        "margin_usd": money::display(billed - upstream),
        "accounts": row.get::<i64, _>("accounts"),
        "suspended_accounts": row.get::<i64, _>("suspended_accounts"),
        "active_keys": row.get::<i64, _>("active_keys"),
        "outstanding_usd": money::display(row.get("outstanding_micro")),
        "suspended_credit_usd": money::display(row.get("suspended_micro")),
        "debt_usd": money::display(row.get("debt_micro")),
    }))
}

/// Who reads a usage series. Customers see what they were billed, never the
/// provider cost behind it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Operator,
    Customer,
}

/// Returns daily usage per model over the last `days`, clamped to 1 to 365.
///
/// The result can be scoped to one account and/or one key. Only the
/// [`Audience::Operator`] view carries `upstream_cost_usd`.
pub async fn usage(
    app: &App,
    audience: Audience,
    account: Option<&str>,
    key: Option<&str>,
    days: i64,
) -> Result<Value> {
    let rows = sqlx::query(
        "SELECT hour/86400*86400 AS day,model,SUM(requests) AS requests,SUM(tokens) AS tokens,\
           SUM(upstream_micro) AS upstream_micro,SUM(billed_micro) AS billed_micro \
         FROM usage_buckets \
         WHERE hour>=? AND (? IS NULL OR account_id=?) AND (? IS NULL OR key_id=?) \
         GROUP BY day,model ORDER BY day",
    )
    .bind(now() - days.clamp(1, 365) * DAY)
    .bind(account)
    .bind(account)
    .bind(key)
    .bind(key)
    .fetch_all(&app.db)
    .await?;
    let series: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut day = json!({
                "day": r.get::<i64, _>("day"),
                "model": r.get::<String, _>("model"),
                "requests": r.get::<i64, _>("requests"),
                "tokens": r.get::<i64, _>("tokens"),
                "billed_cost_usd": money::display(r.get("billed_micro")),
            });
            if audience == Audience::Operator {
                day["upstream_cost_usd"] = money::display(r.get("upstream_micro")).into();
            }
            day
        })
        .collect();
    Ok(json!({"series": series}))
}

/// Prunes old statistics.
///
/// Deletes finished request logs beyond the retention window or the entry
/// cap. Logs from the last minute are always kept. Also deletes usage buckets
/// and route usage older than [`BUCKET_RETENTION_DAYS`].
pub async fn prune(app: &App) -> Result<()> {
    let config = app.config().await;
    // Keep at least the last minute: the RPM limiter counts those rows.
    sqlx::query(
        "DELETE FROM requests WHERE finished=1 AND created_at<? AND \
         (created_at<? OR id NOT IN (SELECT id FROM requests ORDER BY created_at DESC LIMIT ?))",
    )
    .bind(now() - 60)
    .bind(now() - i64::from(config.log_retention_days) * DAY)
    .bind(i64::from(config.max_log_entries))
    .execute(&app.db)
    .await?;
    for table in ["usage_buckets", "route_usage"] {
        // Table names come from this fixed list.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE hour<?"
        )))
        .bind(now() - BUCKET_RETENTION_DAYS * DAY)
        .execute(&app.db)
        .await?;
    }
    Ok(())
}
