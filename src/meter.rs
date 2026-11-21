//! Request admission, usage accounting, and settlement.
//!
//! [`Meter::begin`] admits a request (key status, balance, rate and usage
//! limits) and logs it in the same write transaction, so concurrent requests
//! cannot overshoot limits. [`Meter::finish`] prices observed usage and
//! settles it exactly once. A meter dropped mid-stream settles in the
//! background with whatever usage was observed.
use crate::{
    App,
    config::{Config, Price},
    db::{self, HOUR, id, now},
    error::{Error, Result},
    identity::Identity,
    money,
};
use axum::http::StatusCode;
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde_json::Value;
use sqlx::{Row, SqliteConnection};
use std::{sync::Arc, time::Instant};

/// Billable quantities observed for one request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    /// Input tokens, including cached ones.
    pub input: i64,
    /// Of `input`, tokens served from the provider's prompt cache.
    pub cached_input: i64,
    /// Output tokens, including reasoning tokens (providers already count
    /// those within the completion total).
    pub output: i64,
    /// Total tokens, as reported or as input plus output.
    pub total: i64,
    /// Audio duration in milliseconds.
    pub audio_ms: i64,
    /// Generated images (images API responses, chat-completion images).
    pub images: i64,
    /// Unicode characters of text-to-speech input.
    pub characters: i64,
    /// Provider-reported cost in micro-USD. It replaces the token, audio,
    /// image, and character components of the price table, including an
    /// explicit zero. The flat per-request fee is always added on top.
    pub cost: Option<i64>,
    /// Token counts are our estimate: the upstream reported no usage.
    pub estimated: bool,
}

/// Rounds a USD amount to whole micro-USD, half away from zero. A positive
/// amount never rounds down to nothing, so sub-micro requests cannot be
/// repeated for free.
fn round_micro(usd: Decimal) -> Result<i64> {
    let micro = money::round_half_up(usd * Decimal::from(1_000_000))
        .to_i64()
        .ok_or_else(|| Error::bad("Billing overflow"))?;
    Ok(if micro == 0 && usd > Decimal::ZERO {
        1
    } else {
        micro
    })
}

impl Usage {
    /// Reads an OpenAI-style `usage` object.
    ///
    /// Accepts both `prompt_tokens` and `input_tokens` spellings, and audio
    /// durations in `audio_seconds` or `seconds`. Counts are clamped to sane
    /// ranges.
    pub fn parse(v: &Value) -> Self {
        let tokens = |a: &str, b: &str| {
            v[a].as_i64()
                .or_else(|| v[b].as_i64())
                .unwrap_or(0)
                .clamp(0, 1_000_000_000)
        };
        let input = tokens("prompt_tokens", "input_tokens");
        let output = tokens("completion_tokens", "output_tokens");
        let cached = [
            "prompt_tokens_details",
            "input_tokens_details",
            "input_token_details",
        ]
        .into_iter()
        .find_map(|k| v[k]["cached_tokens"].as_i64())
        .unwrap_or(0)
        .clamp(0, input);
        let audio_seconds = v["audio_seconds"]
            .as_f64()
            .or_else(|| v["seconds"].as_f64())
            .unwrap_or(0.0)
            .clamp(0.0, 864_000.0);
        Self {
            input,
            cached_input: cached,
            output,
            total: v["total_tokens"]
                .as_i64()
                .unwrap_or(input + output)
                .clamp(0, 2_000_000_000),
            audio_ms: (audio_seconds * 1000.0).round() as i64,
            cost: v
                .get("cost")
                .filter(|c| !c.is_null())
                .and_then(|c| money::value(c).ok()),
            ..Self::default()
        }
    }

    /// Whether any billable quantity was observed.
    pub fn is_empty(&self) -> bool {
        self.cost.is_none()
            && self.total == 0
            && self.audio_ms == 0
            && self.images == 0
            && self.characters == 0
    }

    /// Accumulates another usage report (for example one per realtime
    /// response within a WebSocket session).
    pub fn add(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.cached_input = self.cached_input.saturating_add(other.cached_input);
        self.output = self.output.saturating_add(other.output);
        self.total = self.total.saturating_add(other.total);
        self.audio_ms = self.audio_ms.saturating_add(other.audio_ms);
        self.images = self.images.saturating_add(other.images);
        self.characters = self.characters.saturating_add(other.characters);
        self.estimated |= other.estimated;
        self.cost = match (self.cost, other.cost) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
        };
    }

    /// Returns the upstream cost of the request in micro-USD.
    ///
    /// The exact decimal sum is rounded once:
    ///
    /// ```text
    /// usage  = provider cost                      if the provider reported one
    ///        = ((input - cached) * input_price
    ///           + cached * cached_input_price     (defaults to input_price)
    ///           + output * output_price
    ///           + characters * characters_price) / 1e6
    ///          + audio_seconds * audio_second_price
    ///          + images * image_price             otherwise
    /// cost   = usage + request_price              only when `success`
    /// ```
    ///
    /// `success` means the upstream answered 2xx (or a realtime session was
    /// established). Failed requests are never charged the flat fee.
    pub fn cost(&self, config: &Config, model: &str, success: bool) -> Result<i64> {
        let price = config.price(model);
        let mut total = match (self.cost, price) {
            (Some(cost), _) => Decimal::from(cost) / Decimal::from(1_000_000),
            (None, Some(price)) => self.priced(price)?,
            (None, None) => Decimal::ZERO,
        };
        if success && let Some(price) = price {
            total += money::decimal(&price.request)?;
        }
        round_micro(total)
    }

    fn priced(&self, price: &Price) -> Result<Decimal> {
        let per_million = Decimal::from(1_000_000);
        let input = money::decimal(&price.input)?;
        let cached_price = match &price.cached_input {
            Some(p) => money::decimal(p)?,
            None => input,
        };
        let cached = self.cached_input.clamp(0, self.input);
        Ok((Decimal::from(self.input - cached) * input
            + Decimal::from(cached) * cached_price
            + Decimal::from(self.output) * money::decimal(&price.output)?
            + Decimal::from(self.characters) * money::decimal(&price.characters)?)
            / per_million
            + Decimal::from(self.audio_ms) * money::decimal(&price.audio_second)?
                / Decimal::from(1000)
            + Decimal::from(self.images) * money::decimal(&price.image)?)
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
    /// The upstream answered successfully. It gates the flat request fee.
    /// Independent of `status`, which also records client-side outcomes
    /// such as 499.
    pub success: bool,
    pub bytes_in: i64,
    pub bytes_out: i64,
    pub usage: Usage,
    pub error: Option<String>,
    start: Instant,
    settled: bool,
}

/// Values persisted at settlement. They are detached from [`Meter`] so a
/// dropped meter can settle on a background task.
struct Settlement {
    app: App,
    id: String,
    identity: Option<Identity>,
    config: Arc<Config>,
    model: String,
    status: u16,
    success: bool,
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
        Self::begin_with(app, identity, meta, true).await
    }

    /// Like [`Meter::begin`], but with `check_model = false` the key's model
    /// allowlist is not applied at admission. The model is only learned while
    /// the body streams. The caller must then enforce it.
    pub async fn begin_with(
        app: &App,
        identity: Option<Identity>,
        meta: RequestMeta<'_>,
        check_model: bool,
    ) -> Result<Self> {
        let config = app.config().await;
        let mut tx = db::write(&app.db).await?;
        if let Some(identity) = &identity {
            admit(
                &mut tx,
                &config,
                identity,
                check_model.then_some(meta.model),
            )
            .await?;
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
            success: false,
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

    /// Settles the request: bills usage, updates aggregates, and closes the log.
    ///
    /// If settlement fails, the same values are retried in the background and
    /// the error is still returned.
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
            success: self.success,
            duration_ms: i64::try_from(self.start.elapsed().as_millis()).unwrap_or(i64::MAX),
            bytes_in: self.bytes_in,
            bytes_out: self.bytes_out,
            usage: self.usage.clone(),
            error: self.error.clone(),
        }
    }
}

impl Drop for Meter {
    /// Settles an unfinished meter in the background as a 499 interrupted
    /// request, so streamed tokens are still billed.
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
    /// Persists on a tracked background task that shutdown waits for.
    ///
    /// Retries up to three times to ride out transient database contention.
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
        let upstream = self.usage.cost(&self.config, &self.model, self.success)?;
        let billed = match &self.identity {
            Some(identity) => money::markup(upstream, &identity.markup)?,
            None => 0,
        };
        let mut tx = db::write(&self.app.db).await?;
        let closed = sqlx::query(
            "UPDATE requests SET status=?,duration_ms=?,bytes_in=?,bytes_out=?,tokens=?,\
             upstream_micro=?,billed_micro=?,error=?,model=?,estimated=?,input_tokens=?,output_tokens=?,\
             cached_tokens=?,characters=?,audio_ms=?,images=?,finished=1 WHERE id=? AND finished=0",
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
        .bind(self.usage.estimated)
        .bind(self.usage.input.max(0))
        .bind(self.usage.output.max(0))
        .bind(self.usage.cached_input.max(0))
        .bind(self.usage.characters.max(0))
        .bind(self.usage.audio_ms.max(0))
        .bind(self.usage.images.max(0))
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
            estimated = self.usage.estimated,
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
async fn admit(
    conn: &mut SqliteConnection,
    config: &Config,
    identity: &Identity,
    model: Option<&str>,
) -> Result<()> {
    // Status may have changed since authentication. Recheck under the lock.
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
    if let Some(model) = model {
        check_model(identity, model)?;
    }
    if db::balance(conn, &identity.account_id).await? <= 0 {
        return Err(Error::new(
            StatusCode::PAYMENT_REQUIRED,
            "insufficient_balance",
            "Add credit to continue",
        ));
    }
    check_concurrency(conn, config, &identity.account_id).await?;
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
                "SELECT COUNT(*) FROM requests WHERE key_id=? AND finished=0 AND created_at>?",
            )
            .bind(key)
            .bind(in_flight_since(config))
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

/// Oldest creation time at which an unfinished request can still be in
/// flight: unfinished rows older than the longest upstream timeout are
/// leftovers of a crash (or exhausted settlement retries) and never count.
fn in_flight_since(config: &Config) -> i64 {
    let timeout = config
        .upstreams()
        .into_iter()
        .map(|(_, u)| match u.timeout_seconds {
            0 => config.upstream_timeout_seconds,
            t => t,
        })
        .max()
        .unwrap_or(config.upstream_timeout_seconds);
    now() - i64::try_from(timeout).unwrap_or(i64::MAX / 2)
}

/// Rejects a request when its account has too many requests in flight.
///
/// A configured limit of 0 disables the check.
async fn check_concurrency(
    conn: &mut SqliteConnection,
    config: &Config,
    account: &str,
) -> Result<()> {
    let limit = config.rate_limits.max_concurrent_requests;
    if limit == 0 {
        return Ok(());
    }
    let in_flight: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM requests WHERE account_id=? AND finished=0 AND created_at>?",
    )
    .bind(account)
    .bind(in_flight_since(config))
    .fetch_one(&mut *conn)
    .await?;
    if in_flight >= i64::from(limit) {
        return Err(Error::too_many(
            "concurrency_limit_exceeded",
            "Too many requests in flight for this account",
        )
        .retry_after(1));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(price: Price) -> Config {
        let mut config = Config::default();
        config.prices.insert("m".into(), price);
        config
    }

    fn price(input: &str, output: &str) -> Price {
        Price {
            input: input.into(),
            output: output.into(),
            ..Price::default()
        }
    }

    #[test]
    fn cached_input_is_billed_at_its_own_rate_and_total_is_unchanged() {
        let usage = Usage {
            input: 1000,
            cached_input: 400,
            output: 100,
            total: 1100,
            ..Usage::default()
        };
        let cached = Price {
            cached_input: Some("0.5".into()),
            ..price("2", "8")
        };
        // 600*2 + 400*0.5 + 100*8 = 2200 micro-USD.
        assert_eq!(usage.cost(&config(cached), "m", true).unwrap(), 2200);
        // Without a cached price the input price applies to every input token.
        assert_eq!(
            usage.cost(&config(price("2", "8")), "m", true).unwrap(),
            2800
        );
    }

    #[test]
    fn provider_cost_replaces_components_but_not_the_request_fee() {
        let priced = Price {
            request: "0.001".into(),
            ..price("2", "8")
        };
        let config = config(priced);
        let free = Usage {
            input: 1000,
            cost: Some(0),
            ..Usage::default()
        };
        assert_eq!(free.cost(&config, "m", true).unwrap(), 1000);
        assert_eq!(free.cost(&config, "m", false).unwrap(), 0);
        let paid = Usage {
            cost: Some(250),
            ..Usage::default()
        };
        assert_eq!(paid.cost(&config, "m", true).unwrap(), 1250);
    }

    #[test]
    fn characters_audio_and_images_are_priced() {
        let priced = Price {
            characters: "15".into(),
            audio_second: "0.01".into(),
            image: "0.04".into(),
            ..price("0", "0")
        };
        let usage = Usage {
            characters: 1000,
            audio_ms: 1500,
            images: 2,
            ..Usage::default()
        };
        // 1000 chars * 15 / 1e6 + 1.5 s * 0.01 + 2 * 0.04.
        assert_eq!(
            usage.cost(&config(priced), "m", true).unwrap(),
            15_000 + 15_000 + 80_000
        );
    }

    #[test]
    fn rounding_is_done_once_half_away_and_never_to_free() {
        let config = config(price("0.5", "1.5"));
        // 10*0.5 + 5*1.5 = 12.5 micro-USD rounds up (half-even would give 12).
        let usage = Usage {
            input: 10,
            output: 5,
            ..Usage::default()
        };
        assert_eq!(usage.cost(&config, "m", true).unwrap(), 13);
        // 1 token at 0.5/M is 0.5 micro-USD: charged, not rounded to nothing.
        let one = Usage {
            input: 1,
            ..Usage::default()
        };
        assert_eq!(one.cost(&config, "m", true).unwrap(), 1);
        assert_eq!(Usage::default().cost(&config, "m", true).unwrap(), 0);
    }

    #[test]
    fn add_accumulates_every_component() {
        let mut a = Usage {
            input: 1,
            cached_input: 1,
            characters: 2,
            ..Usage::default()
        };
        a.add(&Usage {
            input: 2,
            estimated: true,
            cost: Some(5),
            ..Usage::default()
        });
        assert_eq!(
            (a.input, a.cached_input, a.characters, a.cost),
            (3, 1, 2, Some(5))
        );
        assert!(a.estimated);
    }
}
