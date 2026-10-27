//! Request admission, usage accounting, and settlement.
//!
//! [`Meter::begin`] admits a request (key status, balance, rate and usage
//! limits) and logs it in the same write transaction, so concurrent requests
//! cannot overshoot limits. [`Meter::finish`] prices observed usage and
//! settles it exactly once; a meter dropped mid-stream settles in the
//! background with whatever usage was observed.
use crate::{
    App,
    config::Config,
    db::{self, HOUR, id, now},
    error::{Error, Result},
    identity::Identity,
    money,
};
use axum::http::StatusCode;
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::{Row, SqliteConnection};
use std::{sync::Arc, time::Instant};

/// Usage reported by an upstream response.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub total: i64,
    pub audio_ms: i64,
    /// Generated images (images API responses).
    pub images: i64,
    /// Provider-reported cost in micro-USD; overrides the price table,
    /// including an explicit zero.
    pub cost: Option<i64>,
}

impl Usage {
    /// Reads an OpenAI-style `usage` object (chat, responses, or audio).
    pub fn parse(v: &Value) -> Self {
        let tokens = |a: &str, b: &str| {
            v[a].as_i64()
                .or_else(|| v[b].as_i64())
                .unwrap_or(0)
                .clamp(0, 1_000_000_000)
        };
        let input = tokens("prompt_tokens", "input_tokens");
        let output = tokens("completion_tokens", "output_tokens");
        let audio_seconds = v["audio_seconds"]
            .as_f64()
            .or_else(|| v["seconds"].as_f64())
            .unwrap_or(0.0)
            .clamp(0.0, 864_000.0);
        Self {
            input,
            output,
            total: v["total_tokens"]
                .as_i64()
                .unwrap_or(input + output)
                .clamp(0, 2_000_000_000),
            audio_ms: (audio_seconds * 1000.0).round() as i64,
            images: 0,
            cost: v
                .get("cost")
                .filter(|c| !c.is_null())
                .and_then(|c| money::value(c).ok()),
        }
    }

    /// Whether any billable quantity was observed.
    pub fn is_empty(&self) -> bool {
        self.cost.is_none() && self.total == 0 && self.audio_ms == 0 && self.images == 0
    }

    /// Accumulates another usage report (for example one per realtime
    /// response within a WebSocket session).
    pub fn add(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.total = self.total.saturating_add(other.total);
        self.audio_ms = self.audio_ms.saturating_add(other.audio_ms);
        self.images = self.images.saturating_add(other.images);
        self.cost = match (self.cost, other.cost) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
        };
    }

    /// Upstream cost in micro-USD from the provider cost or the price table.
    pub fn cost(&self, config: &Config, model: &str) -> Result<i64> {
        if let Some(cost) = self.cost {
            return Ok(cost);
        }
        let Some(price) = config.price(model) else {
            return Ok(0);
        };
        let per_million = Decimal::from(1_000_000);
        money::micro(
            (Decimal::from(self.input) * money::decimal(&price.input)?
                + Decimal::from(self.output) * money::decimal(&price.output)?)
                / per_million
                + Decimal::from(self.audio_ms) * money::decimal(&price.audio_second)?
                    / Decimal::from(1000)
                + Decimal::from(self.images) * money::decimal(&price.image)?,
        )
    }
}

/// Descriptive fields recorded for every proxied request.
#[derive(Clone, Copy, Debug)]
pub struct RequestMeta<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub kind: &'a str,
    pub model: &'a str,
    pub ip: &'a str,
}

/// An admitted request awaiting settlement.
pub struct Meter {
    app: App,
    pub id: String,
    identity: Option<Identity>,
    config: Arc<Config>,
    pub model: String,
    pub status: u16,
    pub bytes_in: i64,
    pub bytes_out: i64,
    pub usage: Usage,
    pub error: Option<String>,
    start: Instant,
    settled: bool,
}

/// Values persisted at settlement; detached from [`Meter`] so a dropped
/// meter can settle on a background task.
struct Settlement {
    app: App,
    id: String,
    identity: Option<Identity>,
    config: Arc<Config>,
    model: String,
    status: u16,
    duration_ms: i64,
    bytes_in: i64,
    bytes_out: i64,
    usage: Usage,
    error: Option<String>,
}

impl Meter {
    /// Admits and logs a request. Anonymous requests (`identity = None`) are
    /// logged but never billed.
    pub async fn begin(
        app: &App,
        identity: Option<Identity>,
        meta: RequestMeta<'_>,
    ) -> Result<Self> {
        let config = app.config().await;
        let mut tx = db::write(&app.db).await?;
        if let Some(identity) = &identity {
            admit(&mut tx, identity, meta.model).await?;
        }
        let request_id = id();
        sqlx::query(
            "INSERT INTO requests(id,account_id,key_id,created_at,method,path,kind,model,ip) \
             VALUES(?,?,?,?,?,?,?,?,?)",
        )
        .bind(&request_id)
        .bind(identity.as_ref().map(|i| &i.account_id))
        .bind(identity.as_ref().map(|i| &i.key_id))
        .bind(now())
        .bind(meta.method)
        .bind(meta.path)
        .bind(meta.kind)
        .bind(meta.model)
        .bind(meta.ip)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Self {
            app: app.clone(),
            id: request_id,
            identity,
            config,
            model: meta.model.into(),
            status: StatusCode::BAD_GATEWAY.as_u16(),
            bytes_in: 0,
            bytes_out: 0,
            usage: Usage::default(),
            error: None,
            start: Instant::now(),
            settled: false,
        })
    }

    /// Records a failure that happened before any response was relayed.
    pub fn fail(&mut self, status: StatusCode, error: &str) {
        self.status = status.as_u16();
        self.error = Some(error.into());
    }

    /// Settles the request: bills usage, updates aggregates, closes the log.
    /// A failed attempt is retried in the background with the same values.
    pub async fn finish(mut self) -> Result<()> {
        self.settled = true;
        let settlement = self.settlement();
        let result = settlement.persist().await;
        if result.is_err() {
            settlement.retry_in_background();
        }
        result
    }

    fn settlement(&self) -> Settlement {
        Settlement {
            app: self.app.clone(),
            id: self.id.clone(),
            identity: self.identity.clone(),
            config: self.config.clone(),
            model: self.model.clone(),
            status: self.status,
            duration_ms: i64::try_from(self.start.elapsed().as_millis()).unwrap_or(i64::MAX),
            bytes_in: self.bytes_in,
            bytes_out: self.bytes_out,
            usage: self.usage.clone(),
            error: self.error.clone(),
        }
    }
}

impl Drop for Meter {
    /// Client disconnects drop the response stream before `finish` runs;
    /// settle what was observed so streamed tokens are still billed.
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut settlement = self.settlement();
        settlement.status = 499;
        settlement.error = Some("stream interrupted".into());
        settlement.retry_in_background();
    }
}

impl Settlement {
    /// Persists on a tracked background task (awaited at shutdown), with a
    /// few retries for transient database contention.
    fn retry_in_background(self) {
        let tasks = self.app.tasks.clone();
        tasks.spawn(async move {
            for attempt in 1..=3u64 {
                match self.persist().await {
                    Ok(()) => return,
                    Err(e) => tracing::error!(
                        error = %e,
                        request_id = %self.id,
                        attempt,
                        "meter settlement failed"
                    ),
                }
                tokio::time::sleep(std::time::Duration::from_millis(100 * attempt)).await;
            }
        });
    }

    async fn persist(&self) -> Result<()> {
        let upstream = self.usage.cost(&self.config, &self.model)?;
        let billed = match &self.identity {
            Some(identity) => money::markup(upstream, &identity.markup)?,
            None => 0,
        };
        let mut tx = db::write(&self.app.db).await?;
        let closed = sqlx::query(
            "UPDATE requests SET status=?,duration_ms=?,bytes_in=?,bytes_out=?,tokens=?,\
             upstream_micro=?,billed_micro=?,error=?,model=?,finished=1 WHERE id=? AND finished=0",
        )
        .bind(i64::from(self.status))
        .bind(self.duration_ms)
        .bind(self.bytes_in)
        .bind(self.bytes_out)
        .bind(self.usage.total)
        .bind(upstream)
        .bind(billed)
        .bind(&self.error)
        .bind(&self.model)
        .bind(&self.id)
        .execute(&mut *tx)
        .await?;
        if closed.rows_affected() == 0 {
            return Ok(()); // already settled
        }
        if let Some(identity) = &self.identity {
            crate::billing::debit(&mut tx, &identity.account_id, billed).await?;
            sqlx::query(
                "INSERT INTO usage_buckets(hour,key_id,account_id,model,requests,tokens,upstream_micro,billed_micro) \
                 VALUES(?,?,?,?,1,?,?,?) ON CONFLICT(hour,key_id,model) DO UPDATE SET \
                 requests=requests+1,tokens=tokens+excluded.tokens,\
                 upstream_micro=upstream_micro+excluded.upstream_micro,\
                 billed_micro=billed_micro+excluded.billed_micro",
            )
            .bind(now() / HOUR * HOUR)
            .bind(&identity.key_id)
            .bind(&identity.account_id)
            .bind(&self.model)
            .bind(self.usage.total)
            .bind(upstream)
            .bind(billed)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE totals SET requests=requests+1,tokens=tokens+?,\
             upstream_micro=upstream_micro+?,billed_micro=billed_micro+? WHERE id=1",
        )
        .bind(self.usage.total)
        .bind(upstream)
        .bind(billed)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        tracing::info!(
            request_id = %self.id,
            status = self.status,
            model = %self.model,
            tokens = self.usage.total,
            billed_micro = billed,
            "request completed"
        );
        Ok(())
    }
}

/// Rejects a model that is not on the key's allowlist.
pub fn check_model(identity: &Identity, model: &str) -> Result<()> {
    if identity.limits.allows(model) {
        Ok(())
    } else {
        Err(Error::forbidden(
            "model_not_allowed",
            "Model is not allowed on this key",
        ))
    }
}

/// Admission checks, run inside the logging transaction.
async fn admit(conn: &mut SqliteConnection, identity: &Identity, model: &str) -> Result<()> {
    // Status may have changed since authentication; recheck under the lock.
    let row = sqlx::query(
        "SELECT k.status,k.expires_at,a.status AS account_status \
         FROM api_keys k JOIN accounts a ON a.id=k.account_id WHERE k.id=?",
    )
    .bind(&identity.key_id)
    .fetch_one(&mut *conn)
    .await?;
    if row.get::<String, _>("status") != "active"
        || row
            .get::<Option<i64>, _>("expires_at")
            .is_some_and(|t| t <= now())
    {
        return Err(Error::unauthorized(
            "key_unavailable",
            "Key is blocked, revoked, or expired",
        ));
    }
    if row.get::<String, _>("account_status") != "active" {
        return Err(Error::forbidden("account_suspended", "Account suspended"));
    }
    check_model(identity, model)?;
    if db::balance(conn, &identity.account_id).await? <= 0 {
        return Err(Error::new(
            StatusCode::PAYMENT_REQUIRED,
            "insufficient_balance",
            "Add credit to continue",
        ));
    }
    let key = identity.key_id.as_str();
    let limits = &identity.limits;
    if let Some(rpm) = limits.rpm.filter(|n| *n > 0) {
        let recent: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM requests WHERE key_id=? AND created_at>?")
                .bind(key)
                .bind(now() - 60)
                .fetch_one(&mut *conn)
                .await?;
        if recent >= i64::from(rpm) {
            return Err(Error::too_many(
                "rate_limit_exceeded",
                "Requests per minute limit reached",
            ));
        }
    }
    if let Some(w) = &limits.requests {
        // In-flight requests count too, so parallel bursts cannot overshoot.
        let used = window_sum(conn, Bucket::Requests, key, w.window_hours).await?
            + sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM requests WHERE key_id=? AND finished=0",
            )
            .bind(key)
            .fetch_one(&mut *conn)
            .await?;
        if used >= w.max {
            return Err(Error::too_many(
                "request_limit_exceeded",
                "Usage limit reached",
            ));
        }
    }
    if let Some(w) = &limits.tokens
        && window_sum(conn, Bucket::Tokens, key, w.window_hours).await? >= w.max
    {
        return Err(Error::too_many(
            "token_limit_exceeded",
            "Usage limit reached",
        ));
    }
    if let Some(w) = &limits.spend
        && window_sum(conn, Bucket::Billed, key, w.window_hours).await? >= money::usd(&w.max_usd)?
    {
        return Err(Error::too_many(
            "spend_limit_exceeded",
            "Usage limit reached",
        ));
    }
    sqlx::query("UPDATE api_keys SET last_used_at=? WHERE id=?")
        .bind(now())
        .bind(key)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

#[derive(Clone, Copy)]
enum Bucket {
    Requests,
    Tokens,
    Billed,
}

/// Sums a usage-bucket column over the last `hours` whole hours.
async fn window_sum(
    conn: &mut SqliteConnection,
    bucket: Bucket,
    key: &str,
    hours: u32,
) -> Result<i64> {
    let sql = match bucket {
        Bucket::Requests => {
            "SELECT COALESCE(SUM(requests),0) FROM usage_buckets WHERE key_id=? AND hour>=?"
        }
        Bucket::Tokens => {
            "SELECT COALESCE(SUM(tokens),0) FROM usage_buckets WHERE key_id=? AND hour>=?"
        }
        Bucket::Billed => {
            "SELECT COALESCE(SUM(billed_micro),0) FROM usage_buckets WHERE key_id=? AND hour>=?"
        }
    };
    let since = (now() - i64::from(hours) * HOUR) / HOUR * HOUR;
    Ok(sqlx::query_scalar(sql)
        .bind(key)
        .bind(since)
        .fetch_one(conn)
        .await?)
}
