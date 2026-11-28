//! Administrator console API under `/admin/api/*`.
use super::{
    Editor, Page, Query, direction, expiry, json_body, list_keys, markup, number, object,
    owned_key, patch_key, query, requests, row_json, search, segments, status_filter, text,
    text_or,
};
use crate::{
    App, billing,
    config::{Config, SECRET_PATHS},
    db::{self, DAY, id, now},
    error::{Error, Result},
    identity,
    keys::{self, Requester},
    money, payments,
    stats::{self, Audience},
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, Row};

const MAX_CODES_PER_BATCH: usize = 100;

/// Routes an `/admin/api/` request.
///
/// Every request must pass the administrator token check first.
pub async fn handle(app: &App, req: Request<Body>, ip: &str) -> Result<Value> {
    identity::admin(app, req.headers()).await?;
    let path = segments(&req, "/admin/api/");
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    let method = req.method().as_str().to_owned();
    let q = query(&req);
    let days = || number(&q, "days", 30, 1, 365);
    match (path.as_slice(), method.as_str()) {
        (["overview"], "GET") => stats::overview(app).await,
        (["usage"], "GET") => stats::usage(app, Audience::Operator, None, None, days()).await,

        (["keys"], "GET") => list_keys(app, q.get("account").map(String::as_str), &q).await,
        (["keys", kid], "GET") => {
            let row = owned_key(app, kid, None).await?;
            Ok(json!({
                "key": keys::key_json(&row),
                "account": billing::account(app, row.get("account_id")).await?,
                "usage": stats::usage(app, Audience::Operator, None, Some(kid), 30).await?,
            }))
        }
        (["keys", kid], "PATCH") => patch_key(app, kid, Editor::Admin, &object(req).await?).await,
        (["keys", kid, "rotate"], "POST") => keys::rotate(app, kid, None).await,
        (["keys", kid, "revoke"], "POST") => {
            patch_key(app, kid, Editor::Admin, &json!({"status": "revoked"})).await
        }

        (["accounts"], "GET") => list_accounts(app, &q).await,
        (["accounts", aid], "GET") => account_detail(app, aid, &q).await,
        (["accounts", aid], "PATCH") => patch_account(app, aid, &object(req).await?).await,
        (["accounts", aid, "keys"], "POST") => {
            let body = object(req).await?;
            keys::create(app, Requester::Admin { account: aid }, ip, &body).await
        }
        (["accounts", aid, "credits"], "POST") => {
            let key = billing::idempotency_key(idempotency_header(&req))?.to_owned();
            let body = object(req).await?;
            billing::admin_credit(
                app,
                aid,
                &key,
                money::value(&body["amount_usd"])?,
                text_or(&body, "source", "admin")?,
                expiry(&body["expires_at"])?,
                text_or(&body, "note", "")?,
            )
            .await
        }
        (["accounts", aid, "subscriptions"], "POST") => {
            let key = billing::idempotency_key(idempotency_header(&req))?.to_owned();
            let body = object(req).await?;
            billing::admin_grant(app, aid, &key, text(&body, "plan_code")?).await
        }

        (["plans"], "GET") => billing::plans(app, false).await,
        (["plans"], "POST") => save_plan(app, &object(req).await?).await,
        (["plans", code], "DELETE") => delete_plan(app, code).await,

        (["codes"], "GET") => {
            let rows = sqlx::query("SELECT * FROM redeem_codes ORDER BY created_at DESC LIMIT 200")
                .fetch_all(&app.db)
                .await?;
            Ok(json!({"codes": rows.iter().map(row_json).collect::<Vec<_>>()}))
        }
        (["codes"], "POST") => create_codes(app, json_body(req).await?).await,
        (["codes", cid], "PATCH") => patch_code(app, cid, &object(req).await?).await,
        (["codes", cid], "DELETE") => delete_code(app, cid).await,

        (["payments"], "GET") => list_payments(app, &q).await,
        (["payments", pid], "GET") => payments::order(app, pid, None).await,
        (["payments", pid, "complete"], "POST") => payments::admin_complete(app, pid).await,
        (["payroof"], "GET") => payments::payroof::status(app).await,
        (["payroof", "webhook"], "POST") => payments::payroof::register_webhook(app).await,
        (["requests"], "GET") => requests(app, None, &q, false).await,
        (["requests", "groups"], "GET") => requests(app, None, &q, true).await,

        (["routing"], "GET") => routing(app).await,

        (["settings"], "GET") => Ok(json!({"settings": app.config().await.redacted()})),
        (["settings"], "PATCH") => update_settings(app, object(req).await?).await,

        _ => Err(Error::not_found(
            "not_found",
            "Unknown administrator endpoint",
        )),
    }
}

/// The `Idempotency-Key` request header, if present and valid UTF-8.
fn idempotency_header(req: &Request<Body>) -> Option<&str> {
    req.headers()
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
}

async fn list_accounts(app: &App, q: &Query) -> Result<Value> {
    let search = search(q);
    let status = status_filter(q);
    let page = Page::from(q);
    const FILTER: &str =
        "(id LIKE ? ESCAPE '\\' OR contact LIKE ? ESCAPE '\\') AND (?='' OR status=?)";
    let total: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM accounts WHERE {FILTER}"
    )))
    .bind(&search)
    .bind(&search)
    .bind(status)
    .bind(status)
    .fetch_one(&app.db)
    .await?;
    let order = match q.get("sort").map(String::as_str) {
        Some("contact") => "contact",
        Some("status") => "status",
        Some("balance") => "balance_micro",
        Some("keys") => "key_count",
        Some("spend") => "billed_micro",
        Some("usage") => "usage_30d_micro",
        _ => "created_at",
    };
    // `order` and the direction come from fixed whitelists. `balance_micro`
    // matches `db::balance`, and `billed_micro` is the lifetime spend of all keys.
    let sql = format!(
        "SELECT a.*,\
           COALESCE((SELECT SUM(c.remaining_micro) FROM credits c WHERE c.account_id=a.id \
             AND (c.expires_at IS NULL OR c.expires_at>?)),0)-a.debt_micro AS balance_micro,\
           (SELECT COUNT(*) FROM api_keys k WHERE k.account_id=a.id) AS key_count,\
           (SELECT COUNT(*) FROM api_keys k WHERE k.account_id=a.id AND k.status='active') AS active_key_count,\
           COALESCE((SELECT SUM(k.billed_micro) FROM api_keys k WHERE k.account_id=a.id),0) AS billed_micro,\
           COALESCE((SELECT SUM(u.billed_micro) FROM usage_buckets u WHERE u.account_id=a.id \
             AND u.hour>=?),0) AS usage_30d_micro,\
           (SELECT s.plan_code FROM subscriptions s WHERE s.account_id=a.id AND s.status='active' AND s.expires_at>? \
             ORDER BY s.expires_at DESC LIMIT 1) AS plan_code \
         FROM accounts a WHERE {FILTER} ORDER BY {order} {},id DESC LIMIT ? OFFSET ?",
        direction(q)
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(now())
        .bind(now() - 30 * DAY)
        .bind(now())
        .bind(&search)
        .bind(&search)
        .bind(status)
        .bind(status)
        .bind(page.limit)
        .bind(page.offset)
        .fetch_all(&app.db)
        .await?;
    Ok(page.wrap("accounts", rows.iter().map(row_json).collect(), total))
}

/// Account summary plus a page of its keys (`key_limit`, `key_offset`,
/// `key_sort`, `key_dir`) and 30-day usage.
async fn account_detail(app: &App, aid: &str, q: &Query) -> Result<Value> {
    let mut detail = billing::account(app, aid).await?;
    let key_query: Query = [
        ("key_limit", "limit"),
        ("key_offset", "offset"),
        ("key_sort", "sort"),
        ("key_dir", "dir"),
    ]
    .into_iter()
    .filter_map(|(from, to)| q.get(from).map(|v| (to.to_owned(), v.clone())))
    .collect();
    let mut key_page = list_keys(app, Some(aid), &key_query).await?;
    detail["keys"] = key_page["keys"].take();
    detail["key_total"] = key_page["total"].take();
    detail["usage"] = stats::usage(app, Audience::Operator, Some(aid), None, 30).await?;
    Ok(detail)
}

async fn patch_account(app: &App, aid: &str, b: &Value) -> Result<Value> {
    let mut tx = db::write(&app.db).await?;
    let row = sqlx::query("SELECT * FROM accounts WHERE id=?")
        .bind(aid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::not_found("account_not_found", "Account not found"))?;
    let status = text_or(b, "status", row.get("status"))?;
    if !["active", "suspended"].contains(&status) {
        return Err(Error::bad("Invalid account status"));
    }
    let markup_pct = match b.get("markup_pct") {
        Some(v) => markup(v)?,
        None => row.get("markup_pct"),
    };
    let can_create_keys = match b.get("can_create_keys") {
        Some(v) => v
            .as_bool()
            .ok_or_else(|| Error::bad("can_create_keys must be a boolean"))?,
        None => row.get("can_create_keys"),
    };
    sqlx::query(
        "UPDATE accounts SET status=?,contact=?,note=?,can_create_keys=?,markup_pct=? WHERE id=?",
    )
    .bind(status)
    .bind(keys::clip(text_or(b, "contact", row.get("contact"))?, 254))
    .bind(text_or(b, "note", row.get("note"))?)
    .bind(can_create_keys)
    .bind(markup_pct)
    .bind(aid)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    billing::account(app, aid).await
}

/// Creates or replaces a plan.
///
/// Body: `code`, `name`, `kind` (`subscription` or `topup`), `price_usd`,
/// `monthly_quota_usd` and `months` (1-12) for a subscription or
/// `credit_usd` for a top-up, optional `apple_product_id` and
/// `google_product_id` (null clears them), `active`, and `description`.
async fn save_plan(app: &App, b: &Value) -> Result<Value> {
    let code = text(b, "code")?;
    if code.is_empty()
        || code.len() > 80
        || !code
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err(Error::bad("Invalid plan code"));
    }
    let kind = text_or(b, "kind", billing::SUBSCRIPTION)?;
    let price = money::value(&b["price_usd"])?;
    let (credit, months) = match kind {
        billing::SUBSCRIPTION => {
            let months = b["months"].as_i64().unwrap_or(0);
            if !(1..=12).contains(&months) {
                return Err(Error::bad("A subscription runs for 1-12 months").with_param("months"));
            }
            (money::value(&b["monthly_quota_usd"])?, Some(months))
        }
        billing::TOPUP => (money::value(&b["credit_usd"])?, None),
        _ => return Err(Error::bad("kind must be subscription or topup").with_param("kind")),
    };
    if price <= 0 || credit <= 0 {
        return Err(Error::bad(
            "Positive price and credit (or monthly quota) required",
        ));
    }
    let product = |field: &'static str| -> Result<Option<&str>> {
        match &b[field] {
            Value::Null => Ok(None),
            Value::String(id) if !id.is_empty() && id.len() <= 255 => Ok(Some(id.as_str())),
            _ => Err(
                Error::bad(format!("{field} must be a non-empty string or null")).with_param(field),
            ),
        }
    };
    let saved = sqlx::query(
        "INSERT INTO plans(code,name,kind,price_micro,credit_micro,months,apple_product_id,\
         google_product_id,active,description) VALUES(?,?,?,?,?,?,?,?,?,?) \
         ON CONFLICT(code) DO UPDATE SET name=excluded.name,kind=excluded.kind,\
         price_micro=excluded.price_micro,credit_micro=excluded.credit_micro,months=excluded.months,\
         apple_product_id=excluded.apple_product_id,google_product_id=excluded.google_product_id,\
         active=excluded.active,description=excluded.description",
    )
    .bind(code)
    .bind(text(b, "name")?)
    .bind(kind)
    .bind(price)
    .bind(credit)
    .bind(months)
    .bind(product("apple_product_id")?)
    .bind(product("google_product_id")?)
    .bind(b["active"].as_bool().unwrap_or(true))
    .bind(text_or(b, "description", "")?)
    .execute(&app.db)
    .await;
    match saved {
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "product_id_in_use",
                "Another plan already uses this App Store or Google Play product id",
            ));
        }
        other => {
            other?;
        }
    }
    billing::plans(app, false).await
}

/// Deletes an unused plan. A plan that subscriptions, payments, or codes
/// reference must be deactivated instead.
async fn delete_plan(app: &App, code: &str) -> Result<Value> {
    let mut tx = db::write(&app.db).await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM plans WHERE code=?)")
        .bind(code)
        .fetch_one(&mut *tx)
        .await?;
    if !exists {
        return Err(Error::not_found("plan_not_found", "Plan not found"));
    }
    let referenced: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM subscriptions WHERE plan_code=?1) \
             OR EXISTS(SELECT 1 FROM payments WHERE plan_code=?1) \
             OR EXISTS(SELECT 1 FROM redeem_codes WHERE plan_code=?1)",
    )
    .bind(code)
    .fetch_one(&mut *tx)
    .await?;
    if referenced {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "plan_in_use",
            "Plan is referenced by subscriptions, payments, or codes. Deactivate it instead",
        ));
    }
    sqlx::query("DELETE FROM plans WHERE code=?")
        .bind(code)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    billing::plans(app, false).await
}

/// Issues one code from an object, or up to [`MAX_CODES_PER_BATCH`] in one
/// atomic batch from an array of objects. Codes are shown only once.
async fn create_codes(app: &App, b: Value) -> Result<Value> {
    let config = app.config().await;
    let (bulk, items) = match b {
        Value::Array(items) => (true, items),
        item @ Value::Object(_) => (false, vec![item]),
        _ => return Err(Error::bad("Expected a JSON object or an array of objects")),
    };
    if items.is_empty() {
        return Err(Error::bad("At least one code is required"));
    }
    if items.len() > MAX_CODES_PER_BATCH {
        return Err(Error::bad(format!(
            "At most {MAX_CODES_PER_BATCH} codes can be issued at once"
        )));
    }
    let mut tx = db::write(&app.db).await?;
    let mut codes = Vec::with_capacity(items.len());
    for item in &items {
        if !item.is_object() {
            return Err(Error::bad("Each code must be a JSON object"));
        }
        let plan = item["plan_code"].as_str().filter(|s| !s.is_empty());
        let amount = match plan {
            Some(plan) => {
                let active: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM plans WHERE code=? AND active=1)",
                )
                .bind(plan)
                .fetch_one(&mut *tx)
                .await?;
                if !active {
                    return Err(Error::bad("Active plan not found"));
                }
                None
            }
            None => {
                let amount = money::value(&item["credit_usd"])?;
                if amount <= 0 {
                    return Err(Error::bad("Credit must be positive"));
                }
                Some(amount)
            }
        };
        let expires = expiry(&item["expires_at"])?;
        if expires.is_some_and(|t| t <= now()) {
            return Err(Error::bad("Expiry must be in the future"));
        }
        let code = identity::secret("RES-");
        sqlx::query(
            "INSERT INTO redeem_codes(id,code_hash,code_prefix,plan_code,credit_micro,expires_at,created_at,note) \
             VALUES(?,?,?,?,?,?,?,?)",
        )
        .bind(id())
        .bind(identity::hash(&config.key_pepper, &code))
        .bind(&code[..12])
        .bind(plan)
        .bind(amount)
        .bind(expires)
        .bind(now())
        .bind(text_or(item, "note", "")?)
        .execute(&mut *tx)
        .await?;
        codes.push(code);
    }
    tx.commit().await?;
    Ok(if bulk {
        json!({
            "codes": codes,
            "count": codes.len(),
            "notice": "Save these codes now. They are shown only once.",
        })
    } else {
        json!({"code": codes.remove(0), "notice": "Save this code now. It is shown only once."})
    })
}

fn code_redeemed() -> Error {
    Error::new(
        StatusCode::BAD_REQUEST,
        "code_redeemed",
        "Redeemed codes are kept as-is for audit",
    )
}

async fn patch_code(app: &App, cid: &str, b: &Value) -> Result<Value> {
    let mut tx = db::write(&app.db).await?;
    let row = sqlx::query("SELECT redeemed_by,expires_at,note FROM redeem_codes WHERE id=?")
        .bind(cid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::not_found("code_not_found", "Code not found"))?;
    if row.get::<Option<String>, _>("redeemed_by").is_some() {
        return Err(code_redeemed());
    }
    let expires = match b.get("expires_at") {
        Some(v) => expiry(v)?,
        None => row.get("expires_at"),
    };
    if expires.is_some_and(|t| t <= now()) {
        return Err(Error::bad("Expiry must be in the future"));
    }
    sqlx::query("UPDATE redeem_codes SET expires_at=?,note=? WHERE id=?")
        .bind(expires)
        .bind(text_or(b, "note", row.get("note"))?)
        .bind(cid)
        .execute(&mut *tx)
        .await?;
    let updated = sqlx::query("SELECT * FROM redeem_codes WHERE id=?")
        .bind(cid)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({"code": row_json(&updated)}))
}

async fn delete_code(app: &App, cid: &str) -> Result<Value> {
    let mut tx = db::write(&app.db).await?;
    let redeemed: Option<String> =
        sqlx::query_scalar("SELECT redeemed_by FROM redeem_codes WHERE id=?")
            .bind(cid)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| Error::not_found("code_not_found", "Code not found"))?;
    if redeemed.is_some() {
        return Err(code_redeemed());
    }
    sqlx::query("DELETE FROM redeem_codes WHERE id=?")
        .bind(cid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({"deleted": true, "id": cid}))
}

async fn list_payments(app: &App, q: &Query) -> Result<Value> {
    let status = status_filter(q);
    let search = search(q);
    let page = Page::from(q);
    let account = q.get("account").map(String::as_str);
    const FILTER: &str = "(?='' OR status=?) AND (? IS NULL OR account_id=?) \
         AND (id LIKE ? ESCAPE '\\' OR account_id LIKE ? ESCAPE '\\' \
         OR plan_code LIKE ? ESCAPE '\\' OR provider LIKE ? ESCAPE '\\')";
    let total: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM payments WHERE {FILTER}"
    )))
    .bind(status)
    .bind(status)
    .bind(account)
    .bind(account)
    .bind(&search)
    .bind(&search)
    .bind(&search)
    .bind(&search)
    .fetch_one(&app.db)
    .await?;
    let order = match q.get("sort").map(String::as_str) {
        Some("status") => "status",
        Some("provider") => "provider",
        Some("account") => "account_id",
        Some("plan") => "plan_code",
        Some("amount") => "price_micro",
        _ => "created_at",
    };
    // `order` and the direction come from fixed whitelists.
    let sql = format!(
        "SELECT * FROM payments WHERE {FILTER} ORDER BY {order} {},id DESC LIMIT ? OFFSET ?",
        direction(q)
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(status)
        .bind(status)
        .bind(account)
        .bind(account)
        .bind(&search)
        .bind(&search)
        .bind(&search)
        .bind(&search)
        .bind(page.limit)
        .bind(page.offset)
        .fetch_all(&app.db)
        .await?;
    Ok(page.wrap(
        "payments",
        rows.iter().map(payments::summary).collect(),
        total,
    ))
}

/// Map-valued settings that a patch replaces wholesale. This lets a patch
/// remove entries. Lists (`providers`, `models`) are always replaced wholesale.
const REPLACED_MAPS: [&str; 5] = [
    "/prices",
    "/keys/default_limits",
    "/branding/css_vars",
    "/payroof/rate_ids",
    "/payroof/native_rates_usd",
];

/// Recursively merges a settings patch. Blank secrets keep the current value.
fn merge(base: &mut Value, patch: Value, path: &str) {
    let (Some(target), Value::Object(patch)) = (base.as_object_mut(), patch) else {
        return;
    };
    for (key, value) in patch {
        let pointer = format!("{path}/{key}");
        if SECRET_PATHS.contains(&pointer.as_str()) && value.as_str() == Some("") {
            continue;
        }
        let replace_whole = REPLACED_MAPS.contains(&pointer.as_str());
        match target.get_mut(&key) {
            Some(existing) if value.is_object() && existing.is_object() && !replace_whole => {
                merge(existing, value, &pointer);
            }
            _ => {
                target.insert(key, value);
            }
        }
    }
}

/// Validates and saves a settings patch, then swaps in the new configuration.
///
/// The response notes that CORS changes apply only after a restart.
async fn update_settings(app: &App, patch: Value) -> Result<Value> {
    let mut config = app.config_mut().await;
    let mut merged = serde_json::to_value(&*config)?;
    merge(&mut merged, patch, "");
    let mut updated: Config = serde_json::from_value(merged)?;
    // A blank provider key keeps the stored key of the provider with that id.
    updated.keep_provider_keys(&config);
    updated
        .validate()
        .map_err(|e| Error::bad(format!("{e:#}")))?;
    db::save_settings(&app.db, &updated).await?;
    app.router.retain(&updated);
    *config = updated;
    Ok(json!({
        "settings": config.redacted(),
        "notice": "Saved. CORS changes take effect after restart.",
    }))
}

/// `GET /admin/api/routing`: the live state of every provider and route (in
/// flight, circuit, cooldown, remaining budgets, latency and success rate),
/// the last 24 hours of `route_usage` per route and provider, and the free
/// tier's totals.
///
/// Shape: `{"providers": [{id, enabled, in_flight, cooldown_until, limits,
/// usage_24h}], "routes": [{provider, model, models, in_flight, circuit,
/// failures, cooldown_until, latency_ms, success_rate, limits, usage_24h}],
/// "free": {in_flight, subjects}}`.
async fn routing(app: &App) -> Result<Value> {
    let config = app.config().await;
    let mut out = app.router.snapshot(&config);
    let rows = sqlx::query(
        "SELECT provider,model,\
           SUM(requests) AS requests,SUM(failures) AS failures,SUM(rate_limited) AS rate_limited,\
           SUM(tokens) AS tokens,SUM(upstream_micro) AS upstream_micro,\
           SUM(latency_ms_sum) AS latency_ms_sum,\
           SUM(CASE WHEN tier='free' THEN requests ELSE 0 END) AS free_requests \
         FROM route_usage WHERE hour>=? GROUP BY provider,model",
    )
    .bind((now() - DAY) / 3600 * 3600)
    .fetch_all(&app.db)
    .await?;
    // (requests, failures, rate_limited, tokens, upstream_micro, latency_ms, free_requests)
    type Totals = [i64; 7];
    let mut per_route: std::collections::HashMap<(String, String), Totals> = Default::default();
    let mut per_provider: std::collections::HashMap<String, Totals> = Default::default();
    for r in &rows {
        let totals: Totals = [
            r.get("requests"),
            r.get("failures"),
            r.get("rate_limited"),
            r.get("tokens"),
            r.get("upstream_micro"),
            r.get("latency_ms_sum"),
            r.get("free_requests"),
        ];
        let provider: String = r.get("provider");
        for (slot, value) in per_provider
            .entry(provider.clone())
            .or_default()
            .iter_mut()
            .zip(totals)
        {
            *slot += value;
        }
        per_route.insert((provider, r.get("model")), totals);
    }
    let usage = |t: Totals| {
        json!({
            "requests": t[0],
            "failures": t[1],
            "rate_limited": t[2],
            "tokens": t[3],
            "upstream_cost_usd": money::display(t[4]),
            "avg_latency_ms": if t[0] > 0 { t[5] / t[0] } else { 0 },
            "free_requests": t[6],
        })
    };
    for provider in out["providers"].as_array_mut().into_iter().flatten() {
        let id = provider["id"].as_str().unwrap_or_default().to_owned();
        provider["usage_24h"] = usage(per_provider.get(&id).copied().unwrap_or_default());
    }
    for route in out["routes"].as_array_mut().into_iter().flatten() {
        let key = (
            route["provider"].as_str().unwrap_or_default().to_owned(),
            route["model"].as_str().unwrap_or_default().to_owned(),
        );
        route["usage_24h"] = usage(per_route.get(&key).copied().unwrap_or_default());
    }
    Ok(out)
}
