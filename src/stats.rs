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
pub async fn overview(app: &App) -> Result<Value> {
    let row = sqlx::query(
        "SELECT t.requests,t.tokens,t.upstream_micro,t.billed_micro,\
           (SELECT COUNT(*) FROM accounts) AS accounts,\
           (SELECT COUNT(*) FROM api_keys WHERE status='active') AS active_keys,\
           (SELECT COALESCE(SUM(remaining_micro),0) FROM credits \
              WHERE expires_at IS NULL OR expires_at>?) AS outstanding_micro \
         FROM totals t WHERE t.id=1",
    )
    .bind(now())
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
        "active_keys": row.get::<i64, _>("active_keys"),
        "outstanding_usd": money::display(row.get("outstanding_micro")),
    }))
}

/// Daily usage per model over the last `days` (1–365), optionally scoped to
/// an account and/or key.
pub async fn usage(
    app: &App,
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
            json!({
                "day": r.get::<i64, _>("day"),
                "model": r.get::<String, _>("model"),
                "requests": r.get::<i64, _>("requests"),
                "tokens": r.get::<i64, _>("tokens"),
                "upstream_cost_usd": money::display(r.get("upstream_micro")),
                "billed_cost_usd": money::display(r.get("billed_micro")),
            })
        })
        .collect();
    Ok(json!({"series": series}))
}

/// Deletes request logs beyond the retention window or entry cap, and usage
/// buckets older than [`BUCKET_RETENTION_DAYS`].
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
    sqlx::query("DELETE FROM usage_buckets WHERE hour<?")
        .bind(now() - BUCKET_RETENTION_DAYS * DAY)
        .execute(&app.db)
        .await?;
    Ok(())
}
