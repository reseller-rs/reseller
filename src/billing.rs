//! Credit ledger, plans, subscriptions, top-ups, and redeem codes.
//!
//! Credit is held in lots (`credits` rows) that may expire. Debits consume the
//! soonest-expiring lots first. Any shortfall becomes account debt, which is
//! repaid from the next credit before a new lot is created.
//!
//! Plans come in two kinds:
//!
//! - A **subscription** runs for 1 to 12 months and includes a monthly quota.
//!   Each month of the paid period grants one quota lot that expires at the
//!   end of that month, so unused quota does not carry over. Store
//!   subscriptions renew by starting a new period.
//! - A **top-up** adds credit once. Purchased top-up credit never expires and
//!   is spent after the monthly quota.
use crate::{
    App,
    db::{self, add_months, id, months_between, now},
    error::{Error, Result},
    identity, money,
};
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};

/// Allowed values of `credits.source`.
pub const CREDIT_SOURCES: [&str; 4] = ["purchase", "admin", "refund", "subscription"];

/// Plan kind with a monthly quota over a paid period.
pub const SUBSCRIPTION: &str = "subscription";
/// Plan kind that adds non-expiring credit once.
pub const TOPUP: &str = "topup";

/// Optional attributes of a new credit lot.
#[derive(Clone, Copy, Debug, Default)]
pub struct Lot<'a> {
    pub expires: Option<i64>,
    pub note: &'a str,
    /// The subscription whose monthly quota this lot is.
    pub subscription: Option<&'a str>,
    /// The paid order this lot was bought with.
    pub payment: Option<&'a str>,
    /// Client-supplied key that makes an administrator top-up idempotent.
    pub idempotency_key: Option<&'a str>,
}

/// Adds a credit lot of `amount` micro-USD, repaying outstanding debt first.
///
/// Only the part left after repayment becomes spendable. Rejects amounts that
/// are not positive, expiry times in the past, and unknown sources.
pub async fn credit(
    conn: &mut SqliteConnection,
    account: &str,
    amount: i64,
    source: &str,
    expires: Option<i64>,
    note: &str,
) -> Result<String> {
    let lot = Lot {
        expires,
        note,
        ..Lot::default()
    };
    credit_lot(conn, account, amount, source, &lot).await
}

/// Like [`credit`], with every lot attribute. Returns the lot id.
pub async fn credit_lot(
    conn: &mut SqliteConnection,
    account: &str,
    amount: i64,
    source: &str,
    lot: &Lot<'_>,
) -> Result<String> {
    if amount <= 0 || lot.expires.is_some_and(|e| e <= now()) {
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
    let lot_id = id();
    sqlx::query(
        "INSERT INTO credits(id,account_id,source,amount_micro,remaining_micro,expires_at,created_at,\
         note,subscription_id,payment_id,idempotency_key) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&lot_id)
    .bind(account)
    .bind(source)
    .bind(amount)
    .bind(amount - repaid)
    .bind(lot.expires)
    .bind(now())
    .bind(lot.note)
    .bind(lot.subscription)
    .bind(lot.payment)
    .bind(lot.idempotency_key)
    .execute(conn)
    .await?;
    Ok(lot_id)
}

/// Consumes `amount` micro-USD from unexpired lots (earliest expiry first).
/// The remainder is recorded as debt.
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

/// Validates a client-supplied `Idempotency-Key`: 1 to 255 visible ASCII
/// characters.
pub fn idempotency_key(value: Option<&str>) -> Result<&str> {
    let key = value.map(str::trim).unwrap_or("");
    if key.is_empty() {
        return Err(Error::bad(
            "The Idempotency-Key header is required. Send a new unique value (for example a UUID) \
             for each top-up, and the same value when retrying it",
        )
        .with_code("idempotency_key_required"));
    }
    if key.len() > 255 || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::bad(
            "Idempotency-Key must be 1-255 visible ASCII characters",
        ));
    }
    Ok(key)
}

/// 422 for an idempotency key that was already used with a different body.
fn key_reused() -> Error {
    Error::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "idempotency_key_reused",
        "This Idempotency-Key was already used for a different request",
    )
}

/// An administrator top-up, deduplicated by `key`.
///
/// The first request with a key adds the lot. A retry with the same key and
/// the same amount, source, expiry, and note adds nothing and answers with
/// the original lot (`replayed: true`). The same key with different values
/// is a 422 `idempotency_key_reused`.
pub async fn admin_credit(
    app: &App,
    account: &str,
    key: &str,
    amount: i64,
    source: &str,
    expires: Option<i64>,
    note: &str,
) -> Result<Value> {
    let mut tx = db::write(&app.db).await?;
    let existing = sqlx::query(
        "SELECT id,amount_micro,source,expires_at,note FROM credits \
         WHERE account_id=? AND idempotency_key=?",
    )
    .bind(account)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    let (lot, replayed) = match existing {
        Some(row) => {
            let same = row.get::<i64, _>("amount_micro") == amount
                && row.get::<String, _>("source") == source
                && row.get::<Option<i64>, _>("expires_at") == expires
                && row.get::<String, _>("note") == note;
            if !same {
                return Err(key_reused());
            }
            (row.get::<String, _>("id"), true)
        }
        None => {
            let lot = Lot {
                expires,
                note,
                idempotency_key: Some(key),
                ..Lot::default()
            };
            (
                credit_lot(&mut tx, account, amount, source, &lot).await?,
                false,
            )
        }
    };
    tx.commit().await?;
    let mut out = self::account(app, account).await?;
    out["credit"] = json!({"id": lot, "replayed": replayed});
    Ok(out)
}

/// An administrator plan grant, deduplicated by `key` like [`admin_credit`].
pub async fn admin_grant(app: &App, account: &str, key: &str, plan: &str) -> Result<Value> {
    let mut tx = db::write(&app.db).await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id=?)")
        .bind(account)
        .fetch_one(&mut *tx)
        .await?;
    if !exists {
        return Err(Error::not_found("account_not_found", "Account not found"));
    }
    let previous = sqlx::query(
        "SELECT id,plan_code FROM subscriptions WHERE account_id=?1 AND idempotency_key=?2 \
         UNION ALL SELECT id,note FROM credits WHERE account_id=?1 AND idempotency_key=?2",
    )
    .bind(account)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    let (grant, replayed) = match previous {
        Some(row) if row.get::<String, _>(1) == plan => (row.get::<String, _>(0), true),
        Some(_) => return Err(key_reused()),
        None => (
            grant(&mut tx, account, plan, Grantor::Admin, Some(key)).await?,
            false,
        ),
    };
    tx.commit().await?;
    let mut out = self::account(app, account).await?;
    out["grant"] = json!({"id": grant, "replayed": replayed});
    Ok(out)
}

/// The terms of a plan at the time it was sold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terms {
    pub code: String,
    pub kind: String,
    pub price: i64,
    /// Monthly quota of a subscription, or the credit of a top-up.
    pub credit: i64,
    /// Months of a subscription, or `None` for a top-up.
    pub months: Option<i64>,
}

impl Terms {
    /// Reads the terms from a `plans` or `payments` row.
    pub fn from_row(row: &SqliteRow, code_column: &str) -> Self {
        Self {
            code: row.get(code_column),
            kind: row.get("kind"),
            price: row.get("price_micro"),
            credit: row.get("credit_micro"),
            months: row.get("months"),
        }
    }

    pub fn is_subscription(&self) -> bool {
        self.kind == SUBSCRIPTION
    }
}

/// The current terms of an active plan.
pub async fn active_plan(conn: &mut SqliteConnection, code: &str) -> Result<Terms> {
    let row = sqlx::query("SELECT * FROM plans WHERE code=? AND active=1")
        .bind(code)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| Error::bad("Active plan not found"))?;
    Ok(Terms::from_row(&row, "code"))
}

/// Who grants a plan outside a paid order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grantor {
    Admin,
    Code,
}

/// Grants an active plan at its current terms: a subscription starting now,
/// or a top-up lot. Returns the subscription or lot id.
pub async fn grant(
    conn: &mut SqliteConnection,
    account: &str,
    plan: &str,
    grantor: Grantor,
    idempotency_key: Option<&str>,
) -> Result<String> {
    let terms = active_plan(conn, plan).await?;
    match terms.months {
        Some(months) => {
            let start = Subscription {
                plan: &terms.code,
                quota: terms.credit,
                months,
                provider: match grantor {
                    Grantor::Admin => "admin",
                    Grantor::Code => "code",
                },
                period: None,
                auto_renew: false,
                original_transaction_id: None,
                purchase_token: None,
                idempotency_key,
            };
            subscribe(conn, account, &start).await
        }
        None => {
            let lot = Lot {
                note: &terms.code,
                idempotency_key,
                ..Lot::default()
            };
            let source = match grantor {
                Grantor::Admin => "admin",
                Grantor::Code => "purchase",
            };
            credit_lot(conn, account, terms.credit, source, &lot).await
        }
    }
}

/// A subscription to start.
#[derive(Clone, Debug)]
pub struct Subscription<'a> {
    pub plan: &'a str,
    /// Monthly quota in micro-USD.
    pub quota: i64,
    /// Plan length in months. The period is that long unless `period` is set.
    pub months: i64,
    /// `admin`, `code`, `stripe`, `payroof`, `apple`, or `google`.
    pub provider: &'a str,
    /// Paid period `(start, end)` reported by a store. `None` starts now.
    pub period: Option<(i64, i64)>,
    pub auto_renew: bool,
    pub original_transaction_id: Option<&'a str>,
    pub purchase_token: Option<&'a str>,
    pub idempotency_key: Option<&'a str>,
}

/// Starts a subscription and grants its first monthly quota. Returns its id.
pub async fn subscribe(
    conn: &mut SqliteConnection,
    account: &str,
    s: &Subscription<'_>,
) -> Result<String> {
    if s.quota <= 0 || !(1..=12).contains(&s.months) {
        return Err(Error::bad("Invalid subscription terms"));
    }
    let months = u32::try_from(s.months).unwrap_or(1);
    let (start, end) = s.period.unwrap_or_else(|| {
        let start = now();
        (start, add_months(start, months))
    });
    if end <= start.max(now()) {
        return Err(Error::bad("Subscription period has already ended"));
    }
    let subscription = id();
    sqlx::query(
        "INSERT INTO subscriptions(id,account_id,plan_code,quota_micro,months,started_at,period_start,\
         expires_at,auto_renew,provider,original_transaction_id,purchase_token,idempotency_key) \
         VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&subscription)
    .bind(account)
    .bind(s.plan)
    .bind(s.quota)
    .bind(s.months)
    .bind(start)
    .bind(start)
    .bind(end)
    .bind(s.auto_renew)
    .bind(s.provider)
    .bind(s.original_transaction_id)
    .bind(s.purchase_token)
    .bind(s.idempotency_key)
    .execute(&mut *conn)
    .await?;
    grant_due(conn, Some(account)).await?;
    Ok(subscription)
}

/// Starts a new paid period of an existing store subscription (a renewal)
/// and grants its first monthly quota. The previous period's quota lot
/// expires as scheduled.
pub async fn renew(
    conn: &mut SqliteConnection,
    subscription: &str,
    period: (i64, i64),
    auto_renew: bool,
) -> Result<()> {
    let account: String = sqlx::query_scalar(
        "UPDATE subscriptions SET period_start=?,expires_at=?,granted_months=0,status='active',\
         auto_renew=? WHERE id=? RETURNING account_id",
    )
    .bind(period.0)
    .bind(period.1)
    .bind(auto_renew)
    .bind(subscription)
    .fetch_one(&mut *conn)
    .await?;
    grant_due(conn, Some(&account)).await
}

/// Grants the monthly quota lots that are due: for every active subscription
/// whose current quota month has not been granted yet, one lot of its quota
/// that expires at the end of that month (or of the paid period, if
/// sooner). Months that passed without activity are skipped, since their
/// lots would already have expired. Limited to `account` when given.
pub async fn grant_due(conn: &mut SqliteConnection, account: Option<&str>) -> Result<()> {
    let t = now();
    let due = sqlx::query(
        "SELECT id,account_id,plan_code,quota_micro,months,period_start,expires_at,granted_months \
         FROM subscriptions WHERE status='active' AND expires_at>?1 AND period_start<=?1 \
         AND (?2 IS NULL OR account_id=?2)",
    )
    .bind(t)
    .bind(account)
    .fetch_all(&mut *conn)
    .await?;
    for row in due {
        let start: i64 = row.get("period_start");
        let month = i64::from(months_between(start, t));
        if month < row.get::<i64, _>("granted_months") || month >= row.get::<i64, _>("months") {
            continue;
        }
        let subscription: String = row.get("id");
        let claimed = sqlx::query(
            "UPDATE subscriptions SET granted_months=? WHERE id=? AND granted_months=?",
        )
        .bind(month + 1)
        .bind(&subscription)
        .bind(row.get::<i64, _>("granted_months"))
        .execute(&mut *conn)
        .await?;
        if claimed.rows_affected() == 0 {
            continue;
        }
        let month_end = add_months(start, u32::try_from(month + 1).unwrap_or(u32::MAX));
        let lot = Lot {
            expires: Some(month_end.min(row.get("expires_at"))),
            note: row.get("plan_code"),
            subscription: Some(&subscription),
            ..Lot::default()
        };
        credit_lot(
            conn,
            row.get("account_id"),
            row.get("quota_micro"),
            "subscription",
            &lot,
        )
        .await?;
    }
    Ok(())
}

/// Grants due monthly quotas of every account, for the maintenance loop.
pub async fn grant_all_due(app: &App) -> Result<()> {
    let mut tx = db::write(&app.db).await?;
    grant_due(&mut tx, None).await?;
    tx.commit().await?;
    Ok(())
}

/// How much of a purchase the customer has used, for store refund answers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Consumption {
    /// Credit the purchase granted so far, in micro-USD.
    pub granted: i64,
    /// Of that, credit already spent.
    pub spent: i64,
    /// Share of the whole purchase spent, in thousandths of a percent
    /// (0 to 100000, the unit Apple and Google use).
    pub milli_percent: i64,
}

/// Consumption of a paid order: its top-up lot, or the quota lots of its
/// subscription period. A subscription's share is measured against the
/// quota of the whole period, so months not yet granted count as unused.
pub async fn consumption(conn: &mut SqliteConnection, payment: &str) -> Result<Consumption> {
    let order = sqlx::query(
        "SELECT kind,credit_micro,months,subscription_id,paid_at FROM payments WHERE id=?",
    )
    .bind(payment)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| Error::not_found("payment_not_found", "Payment not found"))?;
    let (granted, spent, whole): (i64, i64, i64) = match order
        .get::<Option<String>, _>("subscription_id")
    {
        Some(subscription) => {
            let row = sqlx::query(
                "SELECT COALESCE(SUM(c.amount_micro),0) AS granted,\
                 COALESCE(SUM(c.amount_micro-c.remaining_micro),0) AS spent,s.quota_micro*s.months AS whole \
                 FROM subscriptions s LEFT JOIN credits c ON c.subscription_id=s.id AND c.created_at>=? \
                 WHERE s.id=?",
            )
            .bind(order.get::<Option<i64>, _>("paid_at").unwrap_or(0))
            .bind(&subscription)
            .fetch_one(&mut *conn)
            .await?;
            (row.get("granted"), row.get("spent"), row.get("whole"))
        }
        None => {
            let (granted, spent): (i64, i64) = sqlx::query_as(
                "SELECT COALESCE(SUM(amount_micro),0),COALESCE(SUM(amount_micro-remaining_micro),0) \
                 FROM credits WHERE payment_id=?",
            )
            .bind(payment)
            .fetch_one(&mut *conn)
            .await?;
            (granted, spent, order.get("credit_micro"))
        }
    };
    let milli_percent = if whole <= 0 {
        0
    } else {
        i64::try_from((i128::from(spent) * 100_000 / i128::from(whole)).clamp(0, 100_000))
            .unwrap_or(100_000)
    };
    Ok(Consumption {
        granted,
        spent,
        milli_percent,
    })
}

/// Applies a store refund to an account: zeroes every credit lot, ends its
/// subscriptions, suspends the account (`suspended_reason = refund`), blocks
/// its active keys, and records identifiers that may not be reused: the
/// signup address for `ip_block_days` days (0 skips it) and, for App Store
/// subscriptions, the original transaction id.
pub async fn block_after_refund(
    conn: &mut SqliteConnection,
    account: &str,
    ip_block_days: u32,
    apple_original_transaction: Option<&str>,
) -> Result<()> {
    let t = now();
    sqlx::query("UPDATE credits SET remaining_micro=0 WHERE account_id=? AND remaining_micro>0")
        .bind(account)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE subscriptions SET status='refunded',auto_renew=0,\
         expires_at=MAX(MIN(expires_at,?),period_start+1) WHERE account_id=?",
    )
    .bind(t)
    .bind(account)
    .execute(&mut *conn)
    .await?;
    let ip: Option<String> = sqlx::query_scalar(
        "UPDATE accounts SET status='suspended',suspended_reason='refund' WHERE id=? RETURNING created_ip",
    )
    .bind(account)
    .fetch_optional(&mut *conn)
    .await?;
    sqlx::query("UPDATE api_keys SET status='blocked' WHERE account_id=? AND status='active'")
        .bind(account)
        .execute(&mut *conn)
        .await?;
    let mut blocks = Vec::new();
    if let Some(ip) = ip.filter(|_| ip_block_days > 0) {
        blocks.push(("ip", ip, Some(t + i64::from(ip_block_days) * db::DAY)));
    }
    if let Some(original) = apple_original_transaction {
        blocks.push(("apple_original_transaction", original.to_owned(), None));
    }
    for (kind, value, expires) in blocks {
        sqlx::query(
            "INSERT INTO refund_blocks(kind,value,account_id,created_at,expires_at) VALUES(?,?,?,?,?) \
             ON CONFLICT(kind,value) DO UPDATE SET account_id=excluded.account_id,\
             created_at=excluded.created_at,expires_at=excluded.expires_at",
        )
        .bind(kind)
        .bind(value)
        .bind(account)
        .bind(t)
        .bind(expires)
        .execute(&mut *conn)
        .await?;
    }
    tracing::warn!(account = %account, "account blocked after a store refund");
    Ok(())
}

/// Whether an account counts as a customer for the free tier: it ever paid
/// for an order, has an active subscription or spendable credit, or the
/// operator trusts it (`can_create_keys`). Such accounts get their own free
/// request budget. Every other key shares the budget of its client address, so
/// opening more accounts buys nothing.
pub async fn is_paying(conn: &mut SqliteConnection, account: &str) -> Result<bool> {
    let flagged: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE id=?1 AND can_create_keys=1) \
         OR EXISTS(SELECT 1 FROM payments WHERE account_id=?1 AND status='paid') \
         OR EXISTS(SELECT 1 FROM subscriptions WHERE account_id=?1 AND status='active' \
           AND expires_at>?2)",
    )
    .bind(account)
    .bind(now())
    .fetch_one(&mut *conn)
    .await?;
    Ok(flagged || db::balance(conn, account).await? > 0)
}

/// Whether `value` of `kind` is blocked after an earlier refund.
pub async fn refund_blocked(conn: &mut SqliteConnection, kind: &str, value: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM refund_blocks WHERE kind=? AND value=? \
         AND (expires_at IS NULL OR expires_at>?))",
    )
    .bind(kind)
    .bind(value)
    .bind(now())
    .fetch_one(conn)
    .await?)
}

/// JSON view of a `subscriptions` row joined with its plan name and the
/// remaining quota of the current month.
fn subscription_json(r: &SqliteRow) -> Value {
    json!({
        "id": r.get::<String, _>("id"),
        "plan_code": r.get::<String, _>("plan_code"),
        "plan_name": r.get::<Option<String>, _>("plan_name"),
        "provider": r.get::<String, _>("provider"),
        "monthly_quota_usd": money::display(r.get("quota_micro")),
        "quota_remaining_usd": money::display(r.get("quota_remaining_micro")),
        "quota_resets_at": r.get::<Option<i64>, _>("quota_resets_at"),
        "months": r.get::<i64, _>("months"),
        "started_at": r.get::<i64, _>("started_at"),
        "period_start": r.get::<i64, _>("period_start"),
        "expires_at": r.get::<i64, _>("expires_at"),
        "auto_renew": r.get::<bool, _>("auto_renew"),
        "status": r.get::<String, _>("status"),
    })
}

/// Account summary with balance, lifetime billed spend, active
/// subscriptions with their monthly quota, and recent credit lots.
pub async fn account(app: &App, aid: &str) -> Result<Value> {
    {
        let mut tx = db::write(&app.db).await?;
        grant_due(&mut tx, Some(aid)).await?;
        tx.commit().await?;
    }
    let mut conn = app.db.acquire().await?;
    let row = sqlx::query("SELECT * FROM accounts WHERE id=?")
        .bind(aid)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| Error::not_found("account_not_found", "Account not found"))?;
    let balance = db::balance(&mut conn, aid).await?;
    let billed: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(billed_micro),0) FROM api_keys WHERE account_id=?")
            .bind(aid)
            .fetch_one(&mut *conn)
            .await?;
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
                    "subscription_id": r.get::<Option<String>, _>("subscription_id"),
                    "note": r.get::<String, _>("note"),
                })
            })
            .collect();
    let t = now();
    let subscriptions: Vec<Value> = sqlx::query(
        "SELECT s.*,p.name AS plan_name,\
           COALESCE((SELECT SUM(c.remaining_micro) FROM credits c WHERE c.subscription_id=s.id \
             AND c.expires_at>?1),0) AS quota_remaining_micro,\
           (SELECT MIN(c.expires_at) FROM credits c WHERE c.subscription_id=s.id AND c.expires_at>?1) \
             AS quota_resets_at \
         FROM subscriptions s LEFT JOIN plans p ON p.code=s.plan_code \
         WHERE s.account_id=?2 AND s.status='active' AND s.expires_at>?1 ORDER BY s.expires_at DESC",
    )
    .bind(t)
    .bind(aid)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(subscription_json)
    .collect();
    // Purchased top-up credit and monthly quota left, shown separately.
    let (quota_left, topup_left): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(CASE WHEN subscription_id IS NOT NULL THEN remaining_micro END),0),\
         COALESCE(SUM(CASE WHEN subscription_id IS NULL AND expires_at IS NULL THEN remaining_micro END),0) \
         FROM credits WHERE account_id=? AND (expires_at IS NULL OR expires_at>?)",
    )
    .bind(aid)
    .bind(t)
    .fetch_one(&mut *conn)
    .await?;
    Ok(json!({
        "account": {
            "id": aid,
            "contact": row.get::<String, _>("contact"),
            "note": row.get::<String, _>("note"),
            "status": row.get::<String, _>("status"),
            "suspended_reason": row.get::<String, _>("suspended_reason"),
            "can_create_keys": row.get::<bool, _>("can_create_keys"),
            "markup_pct": row.get::<Option<String>, _>("markup_pct"),
            "balance_usd": money::display(balance),
            "quota_remaining_usd": money::display(quota_left),
            "topup_remaining_usd": money::display(topup_left),
            "debt_usd": money::display(row.get("debt_micro")),
            "billed_usd": money::display(billed),
            "consumption_consent": row.get::<bool, _>("consumption_consent"),
            "created_at": row.get::<i64, _>("created_at"),
            "subscription": subscriptions.first().cloned(),
            "subscriptions": subscriptions,
        },
        "credits": credits,
    }))
}

/// JSON view of a `plans` row. Subscriptions show their monthly quota,
/// top-ups their one-time credit.
pub fn plan_json(r: &SqliteRow) -> Value {
    let kind: String = r.get("kind");
    let mut plan = json!({
        "code": r.get::<String, _>("code"),
        "name": r.get::<String, _>("name"),
        "kind": kind,
        "price_usd": money::display(r.get("price_micro")),
        "active": r.get::<bool, _>("active"),
        "description": r.get::<String, _>("description"),
        "apple_product_id": r.get::<Option<String>, _>("apple_product_id"),
        "google_product_id": r.get::<Option<String>, _>("google_product_id"),
    });
    if kind == SUBSCRIPTION {
        plan["months"] = r.get::<Option<i64>, _>("months").into();
        plan["monthly_quota_usd"] = money::display(r.get("credit_micro")).into();
    } else {
        plan["credit_usd"] = money::display(r.get("credit_micro")).into();
    }
    plan
}

/// Plan catalogue (subscriptions first, then top-ups, each by price).
/// `active_only` hides retired plans.
pub async fn plans(app: &App, active_only: bool) -> Result<Value> {
    let rows = sqlx::query(
        "SELECT * FROM plans WHERE (?=0 OR active=1) ORDER BY kind='topup',price_micro,code",
    )
    .bind(active_only)
    .fetch_all(&app.db)
    .await?;
    Ok(json!({"plans": rows.iter().map(plan_json).collect::<Vec<_>>()}))
}

/// Redeems a single-use code (plan grant or plain credit) atomically. Plain
/// credit from a code never expires, like a purchased top-up.
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
    .ok_or_else(|| {
        Error::new(
            StatusCode::BAD_REQUEST,
            "invalid_code",
            "Code is invalid, expired, or already redeemed",
        )
    })?;
    match row.get::<Option<String>, _>("plan_code") {
        Some(plan) => {
            grant(&mut tx, account, &plan, Grantor::Code, None).await?;
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
