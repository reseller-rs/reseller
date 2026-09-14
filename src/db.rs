use crate::{config::Config, error::Result};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{path::Path, time::Duration};
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
pub async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    if let Some(p) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(p).await?;
    }
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}
pub async fn settings(pool: &SqlitePool, mut seed: Config) -> anyhow::Result<Config> {
    if let Some(row) = sqlx::query("SELECT value FROM settings WHERE id=1")
        .fetch_optional(pool)
        .await?
    {
        return Ok(serde_json::from_str(row.get("value"))?);
    }
    if seed.admin_token.is_empty() {
        seed.admin_token = crate::identity::secret("admin_");
        println!("Administrator token (save now): {}", seed.admin_token);
    }
    if seed.key_pepper.is_empty() {
        seed.key_pepper = crate::identity::secret("");
    }
    seed.validate()?;
    sqlx::query("INSERT INTO settings(id,value) VALUES(1,?)")
        .bind(serde_json::to_string(&seed)?)
        .execute(pool)
        .await?;
    Ok(seed)
}
pub async fn balance(conn: &mut sqlx::SqliteConnection, account: &str) -> Result<i64> {
    Ok(sqlx::query_scalar::<_,i64>("SELECT COALESCE((SELECT SUM(remaining_micro) FROM credits WHERE account_id=a.id AND (expires_at IS NULL OR expires_at>?)),0)-debt_micro FROM accounts a WHERE id=?").bind(now()).bind(account).fetch_one(conn).await?)
}
