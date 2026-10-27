//! Administrator console API under `/admin/api/*`.
use super::{
    Editor, Page, Query, direction, expiry, json_body, list_keys, markup, number, object,
    owned_key, patch_key, query, requests, row_json, search, segments, status_filter, text,
    text_or,
};
use crate::{
    App, billing,
    config::{Config, SECRET_PATHS},
    db::{self, id, now},
    error::{Error, Result},
    identity,
    keys::{self, Requester},
    money, payments, stats,
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, Row};

const MAX_CODES_PER_BATCH: usize = 100;

pub async fn handle(app: &App, req: Request<Body>, ip: &str) -> Result<Value> {
    identity::admin(app, req.headers()).await?;
    let path = segments(&req, "/admin/api/");
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    let method = req.method().as_str().to_owned();
    let q = query(&req);
    let days = || number(&q, "days", 30, 1, 365);
    match (path.as_slice(), method.as_str()) {
        (["overview"], "GET") => stats::overview(app).await,
        (["usage"], "GET") => stats::usage(app, None, None, days()).await,

        (["keys"], "GET") => list_keys(app, q.get("account").map(String::as_str), &q).await,
        (["keys", kid], "GET") => {
            let row = owned_key(app, kid, None).await?;
            Ok(json!({
                "key": keys::key_json(&row),
                "account": billing::account(app, row.get("account_id")).await?,
                "usage": stats::usage(app, None, Some(kid), 30).await?,
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
            let body = object(req).await?;
            let mut tx = db::write(&app.db).await?;
            billing::credit(
                &mut tx,
                aid,
                money::value(&body["amount_usd"])?,
                text_or(&body, "source", "admin")?,
                expiry(&body["expires_at"])?,
                text_or(&body, "note", "")?,
            )
            .await?;
            tx.commit().await?;
            billing::account(app, aid).await
        }
        (["accounts", aid, "subscriptions"], "POST") => {
            let body = object(req).await?;
            billing::account(app, aid).await?; // 404 for unknown accounts
            let mut tx = db::write(&app.db).await?;
            billing::grant(&mut tx, aid, text(&body, "plan_code")?).await?;
            tx.commit().await?;
            billing::account(app, aid).await
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

        (["settings"], "GET") => Ok(json!({"settings": app.config().await.redacted()})),
        (["settings"], "PATCH") => update_settings(app, object(req).await?).await,

        _ => Err(Error::not_found(
            "not_found",
            "Unknown administrator endpoint",
        )),
    }
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
        _ => "created_at",
    };
    // `order` and the direction come from fixed whitelists.
    let sql = format!(
        "SELECT a.*,\
           COALESCE((SELECT SUM(c.remaining_micro) FROM credits c WHERE c.account_id=a.id \
             AND c.remaining_micro>0 AND (c.expires_at IS NULL OR c.expires_at>?)),0)-a.debt_micro AS balance_micro,\
           (SELECT COUNT(*) FROM api_keys k WHERE k.account_id=a.id) AS key_count,\
           (SELECT s.plan_code FROM subscriptions s WHERE s.account_id=a.id AND s.expires_at>? \
             ORDER BY s.expires_at DESC LIMIT 1) AS plan_code \
         FROM accounts a WHERE {FILTER} ORDER BY {order} {},id DESC LIMIT ? OFFSET ?",
        direction(q)
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(now())
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
    detail["usage"] = stats::usage(app, Some(aid), None, 30).await?;
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
    let price = money::value(&b["price_usd"])?;
    let credit = money::value(&b["credit_usd"])?;
    let days = b["duration_days"].as_i64().unwrap_or(0);
    if price <= 0 || credit <= 0 || !(1..=3650).contains(&days) {
        return Err(Error::bad(
            "Positive price, credit, and duration of 1–3650 days required",
        ));
    }
    sqlx::query(
        "INSERT INTO plans(code,name,price_micro,credit_micro,duration_days,active,description) \
         VALUES(?,?,?,?,?,?,?) ON CONFLICT(code) DO UPDATE SET name=excluded.name,\
         price_micro=excluded.price_micro,credit_micro=excluded.credit_micro,\
         duration_days=excluded.duration_days,active=excluded.active,description=excluded.description",
    )
    .bind(code)
    .bind(text(b, "name")?)
    .bind(price)
    .bind(credit)
    .bind(days)
    .bind(b["active"].as_bool().unwrap_or(true))
    .bind(text_or(b, "description", "")?)
    .execute(&app.db)
    .await?;
    billing::plans(app, false).await
}

/// Deletes an unused plan; referenced plans must be deactivated instead.
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
            "Plan is referenced by subscriptions, payments, or codes; deactivate it instead",
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
    const FILTER: &str = "(?='' OR status=?) AND (id LIKE ? ESCAPE '\\' OR account_id LIKE ? ESCAPE '\\' \
         OR plan_code LIKE ? ESCAPE '\\' OR provider LIKE ? ESCAPE '\\')";
    let total: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM payments WHERE {FILTER}"
    )))
    .bind(status)
    .bind(status)
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

/// Map-valued settings that a patch replaces wholesale, so entries can be
/// removed.
const REPLACED_MAPS: [&str; 10] = [
    "/prices",
    "/ip_overrides",
    "/default_limits",
    "/branding/css_vars",
    "/payroof/rate_ids",
    "/payroof/native_rates_usd",
    "/llm/headers",
    "/tts/headers",
    "/stt/headers",
    "/image/headers",
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

async fn update_settings(app: &App, patch: Value) -> Result<Value> {
    let mut config = app.config_mut().await;
    let mut merged = serde_json::to_value(&*config)?;
    merge(&mut merged, patch, "");
    let updated: Config = serde_json::from_value(merged)?;
    updated
        .validate()
        .map_err(|e| Error::bad(format!("{e:#}")))?;
    db::save_settings(&app.db, &updated).await?;
    *config = updated;
    Ok(json!({
        "settings": config.redacted(),
        "notice": "Saved. CORS changes take effect after restart.",
    }))
}
