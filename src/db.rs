//! SQLite pool, migrations, persisted settings, and shared ledger queries.
use crate::{config::Config, error::Result};
use sqlx::{
    Sqlite, SqliteConnection, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{path::Path, time::Duration};

pub const HOUR: i64 = 3600;
pub const DAY: i64 = 86_400;

/// Current Unix time in seconds.
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Random UUIDv4 primary key.
pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Opens (creating if needed) the database in WAL mode and runs migrations.
pub async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await?;
    }
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}

/// Starts a write transaction that takes the database lock up front, so
/// read-check-write sequences cannot interleave with other writers.
pub async fn write(pool: &SqlitePool) -> Result<Transaction<'static, Sqlite>> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}

/// The stored settings, if the database has been seeded.
pub async fn stored_settings(pool: &SqlitePool) -> anyhow::Result<Option<Config>> {
    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE id=1")
        .fetch_optional(pool)
        .await?
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(|e| anyhow::anyhow!("stored settings are not readable by this version: {e}"))
}

/// Returns the stored settings, or seeds them from `seed` on first start.
///
/// Missing secrets are generated during seeding; a generated administrator
/// token is printed once because it cannot be recovered later.
pub async fn settings(pool: &SqlitePool, mut seed: Config) -> anyhow::Result<Config> {
    if let Some(stored) = stored_settings(pool).await? {
        return Ok(stored);
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

/// Overwrites the stored settings.
pub async fn save_settings(pool: &SqlitePool, config: &Config) -> Result<()> {
    sqlx::query("UPDATE settings SET value=? WHERE id=1")
        .bind(serde_json::to_string(config)?)
        .execute(pool)
        .await?;
    Ok(())
}

/// Spendable balance in micro-USD: unexpired credit minus outstanding debt.
pub async fn balance(conn: &mut SqliteConnection, account: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE((SELECT SUM(remaining_micro) FROM credits \
           WHERE account_id=a.id AND (expires_at IS NULL OR expires_at>?)),0) - debt_micro \
         FROM accounts a WHERE id=?",
    )
    .bind(now())
    .bind(account)
    .fetch_one(conn)
    .await?)
}
