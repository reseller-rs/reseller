use crate::{
    App,
    config::Config,
    db::{id, now},
    error::{Error, Result},
    identity::Identity,
    money,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::Row;

#[derive(Default, Debug, Clone)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub total: i64,
    pub audio_ms: i64,
    pub cost: Option<i64>,
}
impl Usage {
    pub fn parse(v: &Value) -> Self {
        let n = |a: &str, b: &str| {
            v[a].as_i64()
                .or_else(|| v[b].as_i64())
                .unwrap_or(0)
                .clamp(0, 1_000_000_000)
        };
        let input = n("prompt_tokens", "input_tokens");
        let output = n("completion_tokens", "output_tokens");
        let audio = v["audio_seconds"]
            .as_f64()
            .or_else(|| v["seconds"].as_f64())
            .unwrap_or(0.)
            .clamp(0., 864000.);
        Self {
            input,
            output,
            total: v["total_tokens"]
                .as_i64()
                .unwrap_or(input + output)
                .clamp(0, 2_000_000_000),
            audio_ms: (audio * 1000.).round() as i64,
            cost: v
                .get("cost")
                .filter(|v| !v.is_null())
                .and_then(|v| money::value(v).ok()),
        }
    }
    pub fn cost(&self, c: &Config, model: &str) -> Result<i64> {
        if let Some(cost) = self.cost {
            return Ok(cost);
        }
        let Some(p) = c
            .prices
            .get(&model.to_lowercase())
            .or_else(|| c.prices.get("default"))
        else {
            return Ok(0);
        };
        money::micro(
            (Decimal::from(self.input) * money::decimal(&p.input)?
                + Decimal::from(self.output) * money::decimal(&p.output)?)
                / Decimal::from(1_000_000)
                + Decimal::from(self.audio_ms) * money::decimal(&p.audio_second)?
                    / Decimal::from(1000),
        )
    }
}
pub struct RequestMeta<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub kind: &'a str,
    pub model: &'a str,
    pub ip: &'a str,
}
pub struct Meter {
    app: App,
    pub id: String,
    identity: Option<Identity>,
    config: Config,
    pub model: String,
    pub status: u16,
    pub bytes_in: i64,
    pub bytes_out: i64,
    pub usage: Usage,
    pub error: Option<String>,
    start: std::time::Instant,
    finished: bool,
}
impl Meter {
    pub async fn begin(app: &App, i: Option<Identity>, meta: RequestMeta<'_>) -> Result<Self> {
        let c = app.config.read().await.clone();
        let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(i) = &i {
            // Recheck mutable status inside the same transaction as admission.
            let r=sqlx::query("SELECT k.status,k.expires_at,a.status AS account_status FROM api_keys k JOIN accounts a ON a.id=k.account_id WHERE k.id=?").bind(&i.key_id).fetch_one(&mut *tx).await?;
            if r.get::<String, _>("status") != "active"
                || r.get::<Option<i64>, _>("expires_at")
                    .is_some_and(|t| t <= now())
            {
                return Err(Error::new(
                    401,
                    "key_unavailable",
                    "Key is blocked, revoked, or expired",
                ));
            }
            if r.get::<String, _>("account_status") != "active" {
                return Err(Error::new(403, "account_suspended", "Account suspended"));
            }
            check_model(i, meta.model)?;
            if crate::db::balance(&mut tx, &i.account_id).await? <= 0 {
                return Err(Error::new(
                    402,
                    "insufficient_balance",
                    "Add credit to continue",
                ));
            }
            if let Some(rpm) = i.limits.rpm.filter(|n| *n > 0) {
                let n: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM requests WHERE key_id=? AND created_at>?",
                )
                .bind(&i.key_id)
                .bind(now() - 60)
                .fetch_one(&mut *tx)
                .await?;
                if n >= rpm as i64 {
                    return Err(Error::new(
                        429,
                        "rate_limit_exceeded",
                        "Requests per minute limit reached",
                    ));
                }
            }
            for (field, window, max, code) in [
                (
                    "requests",
                    i.limits.requests.as_ref().map(|v| v.window_hours),
                    i.limits.requests.as_ref().map(|v| v.max),
                    "request_limit_exceeded",
                ),
                (
                    "tokens",
                    i.limits.tokens.as_ref().map(|v| v.window_hours),
                    i.limits.tokens.as_ref().map(|v| v.max),
                    "token_limit_exceeded",
                ),
                (
                    "billed_micro",
                    i.limits.spend.as_ref().map(|v| v.window_hours),
                    i.limits
                        .spend
                        .as_ref()
                        .map(|v| money::usd(&v.max_usd))
                        .transpose()?,
                    "spend_limit_exceeded",
                ),
            ] {
                if let (Some(hours), Some(max)) = (window, max) {
                    let since = (now() - hours as i64 * 3600) / 3600 * 3600;
                    let mut used:i64=sqlx::query_scalar(&format!("SELECT COALESCE(SUM({field}),0) FROM usage_buckets WHERE key_id=? AND hour>=?")).bind(&i.key_id).bind(since).fetch_one(&mut *tx).await?;
                    if field == "requests" {
                        used += sqlx::query_scalar::<_, i64>(
                            "SELECT COUNT(*) FROM requests WHERE key_id=? AND finished=0",
                        )
                        .bind(&i.key_id)
                        .fetch_one(&mut *tx)
                        .await?;
                    }
                    if used >= max {
                        return Err(Error::new(429, code, "Usage limit reached"));
                    }
                }
            }
            sqlx::query("UPDATE api_keys SET last_used_at=? WHERE id=?")
                .bind(now())
                .bind(&i.key_id)
                .execute(&mut *tx)
                .await?;
        }
        let rid = id();
        sqlx::query("INSERT INTO requests(id,account_id,key_id,created_at,method,path,kind,model,ip) VALUES(?,?,?,?,?,?,?,?,?)")
  .bind(&rid).bind(i.as_ref().map(|i|&i.account_id)).bind(i.as_ref().map(|i|&i.key_id)).bind(now()).bind(meta.method).bind(meta.path).bind(meta.kind).bind(meta.model).bind(meta.ip).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Self {
            app: app.clone(),
            id: rid,
            identity: i,
            config: c,
            model: meta.model.into(),
            status: 502,
            bytes_in: 0,
            bytes_out: 0,
            usage: Usage::default(),
            error: None,
            start: std::time::Instant::now(),
            finished: false,
        })
    }
    pub async fn finish(mut self) -> Result<()> {
        let out = self.persist().await;
        if out.is_ok() {
            self.finished = true;
        }
        out
    }
    async fn persist(&self) -> Result<()> {
        let upstream = self.usage.cost(&self.config, &self.model)?;
        let billed = if let Some(i) = &self.identity {
            money::markup(upstream, &i.markup)?
        } else {
            0
        };
        let mut tx = self.app.db.begin_with("BEGIN IMMEDIATE").await?;
        let r=sqlx::query("UPDATE requests SET status=?,duration_ms=?,bytes_in=?,bytes_out=?,tokens=?,upstream_micro=?,billed_micro=?,error=?,model=?,finished=1 WHERE id=? AND finished=0")
  .bind(self.status as i64).bind(self.start.elapsed().as_millis().min(i64::MAX as u128) as i64).bind(self.bytes_in).bind(self.bytes_out).bind(self.usage.total).bind(upstream).bind(billed).bind(&self.error).bind(&self.model).bind(&self.id).execute(&mut *tx).await?;
        if r.rows_affected() == 0 {
            return Ok(());
        }
        if let Some(i) = &self.identity {
            crate::billing::debit(&mut tx, &i.account_id, billed).await?;
            sqlx::query("INSERT INTO usage_buckets(hour,key_id,account_id,model,requests,tokens,upstream_micro,billed_micro) VALUES(?,?,?,?,1,?,?,?) ON CONFLICT(hour,key_id,model) DO UPDATE SET requests=requests+1,tokens=tokens+excluded.tokens,upstream_micro=upstream_micro+excluded.upstream_micro,billed_micro=billed_micro+excluded.billed_micro")
   .bind(now()/3600*3600).bind(&i.key_id).bind(&i.account_id).bind(&self.model).bind(self.usage.total).bind(upstream).bind(billed).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE totals SET requests=requests+1,tokens=tokens+?,upstream_micro=upstream_micro+?,billed_micro=billed_micro+? WHERE id=1").bind(self.usage.total).bind(upstream).bind(billed).execute(&mut *tx).await?;
        tx.commit().await?;
        tracing::info!(request_id=%self.id,status=self.status,model=%self.model,tokens=self.usage.total,billed_micro=billed,"request completed");
        Ok(())
    }
}
impl Drop for Meter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let copy = Self {
            app: self.app.clone(),
            id: self.id.clone(),
            identity: self.identity.clone(),
            config: self.config.clone(),
            model: self.model.clone(),
            status: 499,
            bytes_in: self.bytes_in,
            bytes_out: self.bytes_out,
            usage: std::mem::take(&mut self.usage),
            error: Some("stream interrupted".into()),
            start: self.start,
            finished: true,
        };
        self.app.tasks.spawn(async move {
            for attempt in 0..3 {
                match copy.persist().await {
                    Ok(()) => return,
                    Err(e) => {
                        tracing::error!(error=%e,request_id=%copy.id,"meter settlement failed")
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1))).await;
            }
        });
    }
}
pub fn check_model(i: &Identity, model: &str) -> Result<()> {
    if !i.limits.allowed_models.is_empty()
        && !i
            .limits
            .allowed_models
            .iter()
            .any(|m| m.eq_ignore_ascii_case(model))
    {
        return Err(Error::new(
            403,
            "model_not_allowed",
            "Model is not allowed on this key",
        ));
    }
    Ok(())
}
pub async fn overview(app: &App) -> Result<Value> {
    let r = sqlx::query("SELECT * FROM totals WHERE id=1")
        .fetch_one(&app.db)
        .await?;
    let accounts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
        .fetch_one(&app.db)
        .await?;
    let keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE status='active'")
        .fetch_one(&app.db)
        .await?;
    let outstanding:i64=sqlx::query_scalar("SELECT COALESCE(SUM(remaining_micro),0) FROM credits WHERE expires_at IS NULL OR expires_at>?").bind(now()).fetch_one(&app.db).await?;
    Ok(
        json!({"requests":r.get::<i64,_>("requests"),"tokens":r.get::<i64,_>("tokens"),"upstream_cost_usd":money::display(r.get("upstream_micro")),"billed_cost_usd":money::display(r.get("billed_micro")),"margin_usd":money::display(r.get::<i64,_>("billed_micro")-r.get::<i64,_>("upstream_micro")),"accounts":accounts,"active_keys":keys,"outstanding_usd":money::display(outstanding)}),
    )
}
pub async fn usage(
    app: &App,
    account: Option<&str>,
    key: Option<&str>,
    days: i64,
) -> Result<Value> {
    let rows=sqlx::query("SELECT hour/86400*86400 AS day,model,SUM(requests) AS requests,SUM(tokens) AS tokens,SUM(upstream_micro) AS upstream_micro,SUM(billed_micro) AS billed_micro FROM usage_buckets WHERE hour>=? AND (? IS NULL OR account_id=?) AND (? IS NULL OR key_id=?) GROUP BY day,model ORDER BY day")
 .bind(now()-days.clamp(1,365)*86400).bind(account).bind(account).bind(key).bind(key).fetch_all(&app.db).await?;
    Ok(
        json!({"series":rows.iter().map(|r|json!({"day":r.get::<i64,_>("day"),"model":r.get::<String,_>("model"),"requests":r.get::<i64,_>("requests"),"tokens":r.get::<i64,_>("tokens"),"upstream_cost_usd":money::display(r.get("upstream_micro")),"billed_cost_usd":money::display(r.get("billed_micro"))})).collect::<Vec<_>>()}),
    )
}
pub async fn prune(app: &App) -> Result<()> {
    let c = app.config.read().await.clone();
    // Retain at least a minute for the persistent RPM limiter.
    sqlx::query("DELETE FROM requests WHERE finished=1 AND created_at<? AND (created_at<? OR id NOT IN (SELECT id FROM requests ORDER BY created_at DESC LIMIT ?))")
 .bind(now()-60).bind(now()-c.log_retention_days as i64*86400).bind(c.max_log_entries as i64).execute(&app.db).await?;
    sqlx::query("DELETE FROM usage_buckets WHERE hour<?")
        .bind(now() - 367 * 86400)
        .execute(&app.db)
        .await?;
    Ok(())
}
