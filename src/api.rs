use crate::{
    App, billing,
    db::{id, now},
    error::{Error, Result},
    identity, money, stats,
};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use sqlx::{Column, QueryBuilder, Row, Sqlite, TypeInfo, ValueRef};
use std::collections::HashMap;
pub type Query = HashMap<String, String>;
pub fn query(req: &Request<Body>) -> Query {
    url::form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes())
        .into_owned()
        .collect()
}
async fn body(req: Request<Body>) -> Result<Value> {
    let b = to_bytes(req.into_body(), 256 * 1024).await.map_err(|_| {
        Error::new(
            413,
            "request_too_large",
            "Management request exceeds 256 KiB",
        )
    })?;
    if b.is_empty() {
        return Ok(json!({}));
    }
    let v: Value = serde_json::from_slice(&b)?;
    if !v.is_object() {
        return Err(Error::bad("Expected a JSON object"));
    }
    Ok(v)
}
/// Like `body`, but also accepts a JSON array (used by bulk code issuance).
async fn body_value(req: Request<Body>) -> Result<Value> {
    let b = to_bytes(req.into_body(), 256 * 1024).await.map_err(|_| {
        Error::new(
            413,
            "request_too_large",
            "Management request exceeds 256 KiB",
        )
    })?;
    if b.is_empty() {
        return Ok(json!({}));
    }
    Ok(serde_json::from_slice(&b)?)
}
fn number(q: &Query, key: &str, default: i64, min: i64, max: i64) -> i64 {
    q.get(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
        .clamp(min, max)
}
fn direction(q: &Query) -> &'static str {
    if q.get("dir").is_some_and(|v| v.eq_ignore_ascii_case("asc")) {
        "ASC"
    } else {
        "DESC"
    }
}
fn text<'a>(b: &'a Value, k: &str) -> Result<&'a str> {
    b[k].as_str()
        .filter(|s| s.len() <= 4096)
        .ok_or_else(|| Error::bad(format!("{k} must be a string (at most 4096 bytes)")))
}
pub fn expiry(v: &Value) -> Result<Option<i64>> {
    if v.is_null() {
        return Ok(None);
    }
    if let Some(n) = v.as_i64() {
        return Ok(Some(n));
    }
    let s = v
        .as_str()
        .ok_or_else(|| Error::bad("Expiry must be Unix seconds or RFC3339"))?;
    Ok(Some(
        chrono::DateTime::parse_from_rfc3339(s)
            .map_err(|_| Error::bad("Invalid expiry"))?
            .timestamp(),
    ))
}
fn row_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    let mut out = serde_json::Map::new();
    for col in r.columns() {
        let name = col.name();
        if ["key_hash", "dash_hash", "code_hash"].contains(&name) {
            continue;
        }
        let raw = r.try_get_raw(name).expect("existing column");
        let v = if raw.is_null() {
            Value::Null
        } else {
            match raw.type_info().name() {
                "INTEGER" => json!(r.get::<i64, _>(name)),
                "REAL" => json!(r.get::<f64, _>(name)),
                _ => json!(r.get::<String, _>(name)),
            }
        };
        if name.ends_with("_micro") {
            out.insert(
                name.replace("_micro", "_usd"),
                v.as_i64()
                    .map(|n| json!(money::display(n)))
                    .unwrap_or(Value::Null),
            );
        } else {
            out.insert(name.into(), v);
        }
    }
    Value::Object(out)
}
async fn keys(app: &App, account: Option<&str>, q: &Query) -> Result<Value> {
    let search = format!("%{}%", q.get("q").map(String::as_str).unwrap_or(""));
    let status = q.get("status").map(String::as_str).unwrap_or("");
    let total: i64=sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE (? IS NULL OR account_id=?) AND (?='' OR status=?) AND (name LIKE ? OR key_prefix LIKE ? OR id LIKE ?)")
 .bind(account).bind(account).bind(status).bind(status).bind(&search).bind(&search).bind(&search).fetch_one(&app.db).await?;
    let order = match q.get("sort").map(String::as_str) {
        Some("name") => "name",
        Some("status") => "status",
        Some("account") => "account_id",
        Some("last_used") => "last_used_at",
        Some("usage") => "usage_30d_micro",
        _ => "created_at",
    };
    let sql = format!(
        "SELECT k.*,COALESCE((SELECT SUM(u.billed_micro) FROM usage_buckets u WHERE u.key_id=k.id AND u.hour>=?),0) AS usage_30d_micro FROM api_keys k WHERE (? IS NULL OR account_id=?) AND (?='' OR status=?) AND (name LIKE ? OR key_prefix LIKE ? OR id LIKE ?) ORDER BY {order} {} NULLS LAST,id DESC LIMIT ? OFFSET ?",
        direction(q)
    );
    let rows = sqlx::query(&sql)
        .bind(now() - 30 * 86400)
        .bind(account)
        .bind(account)
        .bind(status)
        .bind(status)
        .bind(&search)
        .bind(&search)
        .bind(&search)
        .bind(number(q, "limit", 50, 1, 200))
        .bind(number(q, "offset", 0, 0, 1_000_000))
        .fetch_all(&app.db)
        .await?;
    let mut items = Vec::new();
    for r in rows {
        let mut v = identity::key_json(&r);
        v["usage_30d_usd"] = json!(money::display(r.get("usage_30d_micro")));
        items.push(v);
    }
    Ok(
        json!({"keys":items,"total":total,"limit":number(q,"limit",50,1,200),"offset":number(q,"offset",0,0,1_000_000)}),
    )
}
async fn owned_key(app: &App, kid: &str, account: Option<&str>) -> Result<sqlx::sqlite::SqliteRow> {
    sqlx::query("SELECT * FROM api_keys WHERE id=? AND (? IS NULL OR account_id=?)")
        .bind(kid)
        .bind(account)
        .bind(account)
        .fetch_optional(&app.db)
        .await?
        .ok_or_else(|| Error::new(404, "key_not_found", "Key not found"))
}
async fn patch_key(app: &App, kid: &str, account: Option<&str>, b: Value) -> Result<Value> {
    let r = owned_key(app, kid, account).await?;
    let admin = account.is_none();
    let name = if b.get("name").is_some() {
        text(&b, "name")?
    } else {
        r.get("name")
    };
    let status = b["status"].as_str().unwrap_or(r.get("status"));
    if !["active", "blocked", "revoked"].contains(&status)
        || (!admin && b.get("status").is_some() && status != "blocked")
    {
        return Err(Error::bad("Invalid status"));
    }
    if r.get::<String, _>("status") == "revoked" && status != "revoked" {
        return Err(Error::bad("Revocation is permanent"));
    }
    let mut limits: String = r.get("limits");
    let mut markup: Option<String> = r.get("markup_pct");
    let mut expires: Option<i64> = r.get("expires_at");
    if admin {
        if let Some(v) = b.get("limits") {
            let l: identity::Limits = serde_json::from_value(v.clone())?;
            l.validate()?;
            limits = serde_json::to_string(&l)?;
        }
        if let Some(v) = b.get("markup_pct") {
            markup = if v.is_null() {
                None
            } else {
                let s = v
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string());
                money::decimal(&s)?;
                Some(s)
            };
        }
        if let Some(v) = b.get("expires_at") {
            expires = expiry(v)?;
        }
    }
    sqlx::query("UPDATE api_keys SET name=?,status=?,limits=?,markup_pct=?,expires_at=? WHERE id=? AND (status<>'revoked' OR ?='revoked')").bind(name.chars().take(80).collect::<String>()).bind(status).bind(limits).bind(markup).bind(expires).bind(kid).bind(status).execute(&app.db).await?;
    Ok(json!({"key":identity::key_json(&owned_key(app,kid,account).await?)}))
}
pub async fn customer(app: &App, req: Request<Body>, ip: &str) -> Result<Value> {
    let route = req
        .uri()
        .path()
        .trim_end_matches('/')
        .trim_start_matches("/v1/")
        .to_owned();
    let method = req.method().clone();
    let q = query(&req);
    if route == "plans" && method == "GET" {
        return billing::plans(app, true).await;
    }
    if route == "keys" && method == "POST" {
        let i = identity::authenticate(app, req.headers(), false).await?;
        return identity::create(app, i.as_ref(), None, ip, &body(req).await?).await;
    }
    let i = identity::required(app, req.headers()).await?;
    let parts: Vec<_> = route.split('/').collect();
    match (parts.as_slice(), method.as_str()) {
        (["keys"], "GET") => keys(app, Some(&i.account_id), &q).await,
        (["keys", kid], "PATCH") => {
            patch_key(app, kid, Some(&i.account_id), body(req).await?).await
        }
        (["keys", kid, "rotate"], "POST") => identity::rotate(app, kid, Some(&i.account_id)).await,
        (["account"], "GET") => billing::account(app, &i.account_id).await,
        (["account", "redeem"], "POST") => {
            let b = body(req).await?;
            billing::redeem(app, &i.account_id, text(&b, "code")?).await
        }
        (["account", "checkout"], "POST") => {
            crate::payments::checkout(app, &i, &body(req).await?).await
        }
        (["account", "usage"], "GET") => {
            let key = q.get("key").map(String::as_str);
            if let Some(k) = key {
                owned_key(app, k, Some(&i.account_id)).await?;
            }
            stats::usage(
                app,
                Some(&i.account_id),
                key,
                number(&q, "days", 30, 1, 365),
            )
            .await
        }
        (["account", "requests"], "GET") => requests(app, Some(&i.account_id), &q, false).await,
        _ => Err(Error::new(404, "not_found", "Unknown customer endpoint")),
    }
}
pub async fn admin(app: &App, req: Request<Body>, ip: &str) -> Result<Value> {
    identity::admin(app, req.headers()).await?;
    let route = req
        .uri()
        .path()
        .trim_end_matches('/')
        .trim_start_matches("/admin/api/")
        .to_owned();
    let method = req.method().clone();
    let q = query(&req);
    let parts: Vec<_> = route.split('/').collect();
    match (parts.as_slice(), method.as_str()) {
        (["overview"], "GET") => stats::overview(app).await,
        (["usage"], "GET") => stats::usage(app, None, None, number(&q, "days", 30, 1, 365)).await,
        (["keys"], "GET") => keys(app, q.get("account").map(String::as_str), &q).await,
        (["keys", kid], "GET") => {
            let r = owned_key(app, kid, None).await?;
            Ok(
                json!({"key":identity::key_json(&r),"account":billing::account(app,r.get("account_id")).await?,"usage":stats::usage(app,None,Some(kid),30).await?}),
            )
        }
        (["keys", kid], "PATCH") => patch_key(app, kid, None, body(req).await?).await,
        (["keys", kid, "rotate"], "POST") => identity::rotate(app, kid, None).await,
        (["keys", kid, "revoke"], "POST") => {
            patch_key(app, kid, None, json!({"status":"revoked"})).await
        }
        (["accounts"], "GET") => {
            let search = format!("%{}%", q.get("q").map(String::as_str).unwrap_or(""));
            let status = q.get("status").map(String::as_str).unwrap_or("");
            let total:i64=sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE (id LIKE ? OR contact LIKE ?) AND (?='' OR status=?)").bind(&search).bind(&search).bind(status).bind(status).fetch_one(&app.db).await?;
            let order = match q.get("sort").map(String::as_str) {
                Some("contact") => "contact",
                Some("status") => "status",
                Some("balance") => "balance_micro",
                Some("keys") => "key_count",
                _ => "created_at",
            };
            let sql = format!(
                "SELECT a.*,COALESCE((SELECT SUM(c.remaining_micro) FROM credits c WHERE c.account_id=a.id AND c.remaining_micro>0 AND (c.expires_at IS NULL OR c.expires_at>?)),0) AS balance_micro,(SELECT COUNT(*) FROM api_keys k WHERE k.account_id=a.id) AS key_count,(SELECT s.plan_code FROM subscriptions s WHERE s.account_id=a.id AND s.expires_at>? ORDER BY s.expires_at DESC LIMIT 1) AS plan_code FROM accounts a WHERE (id LIKE ? OR contact LIKE ?) AND (?='' OR status=?) ORDER BY {order} {},id DESC LIMIT ? OFFSET ?",
                direction(&q)
            );
            let rows = sqlx::query(&sql)
                .bind(now())
                .bind(now())
                .bind(&search)
                .bind(&search)
                .bind(status)
                .bind(status)
                .bind(number(&q, "limit", 50, 1, 200))
                .bind(number(&q, "offset", 0, 0, 1_000_000))
                .fetch_all(&app.db)
                .await?;
            Ok(
                json!({"accounts":rows.iter().map(row_json).collect::<Vec<_>>(),"total":total,"limit":number(&q,"limit",50,1,200),"offset":number(&q,"offset",0,0,1_000_000)}),
            )
        }
        (["accounts", aid], "GET") => {
            let mut v = billing::account(app, aid).await?;
            let mut key_query = Query::new();
            for (source, target) in [
                ("key_limit", "limit"),
                ("key_offset", "offset"),
                ("key_sort", "sort"),
                ("key_dir", "dir"),
            ] {
                if let Some(value) = q.get(source) {
                    key_query.insert(target.into(), value.clone());
                }
            }
            let key_page = keys(app, Some(aid), &key_query).await?;
            v["keys"] = key_page["keys"].clone();
            v["key_total"] = key_page["total"].clone();
            v["usage"] = stats::usage(app, Some(aid), None, 30).await?;
            Ok(v)
        }
        (["accounts", aid], "PATCH") => patch_account(app, aid, body(req).await?).await,
        (["accounts", aid, "keys"], "POST") => {
            identity::create(app, None, Some(aid), ip, &body(req).await?).await
        }
        (["accounts", aid, action], "POST")
            if *action == "credits" || *action == "subscriptions" =>
        {
            let b = body(req).await?;
            billing::account(app, aid).await?;
            let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
            if *action == "credits" {
                billing::credit(
                    &mut tx,
                    aid,
                    money::value(&b["amount_usd"])?,
                    b["source"].as_str().unwrap_or("admin"),
                    expiry(&b["expires_at"])?,
                    b["note"].as_str().unwrap_or(""),
                )
                .await?;
            } else {
                billing::grant(&mut tx, aid, text(&b, "plan_code")?).await?;
            }
            tx.commit().await?;
            billing::account(app, aid).await
        }
        (["plans"], "GET") => billing::plans(app, false).await,
        (["plans"], "POST") => save_plan(app, body(req).await?).await,
        (["plans", code], "DELETE") => delete_plan(app, code).await,
        (["codes"], "POST") => create_code(app, body_value(req).await?).await,
        (["codes"], "GET") => {
            let r = sqlx::query("SELECT * FROM redeem_codes ORDER BY created_at DESC LIMIT 200")
                .fetch_all(&app.db)
                .await?;
            Ok(json!({"codes":r.iter().map(row_json).collect::<Vec<_>>()}))
        }
        (["codes", cid], "PATCH") => patch_code(app, cid, body(req).await?).await,
        (["codes", cid], "DELETE") => delete_code(app, cid).await,
        (["payments"], "GET") => {
            let status = q.get("status").map(String::as_str).unwrap_or("");
            let search = format!("%{}%", q.get("q").map(String::as_str).unwrap_or(""));
            let total:i64=sqlx::query_scalar("SELECT COUNT(*) FROM payments WHERE (?='' OR status=?) AND (id LIKE ? OR account_id LIKE ? OR plan_code LIKE ?)").bind(status).bind(status).bind(&search).bind(&search).bind(&search).fetch_one(&app.db).await?;
            let order = match q.get("sort").map(String::as_str) {
                Some("status") => "status",
                Some("account") => "account_id",
                Some("plan") => "plan_code",
                Some("amount") => "price_micro",
                _ => "created_at",
            };
            let sql = format!(
                "SELECT * FROM payments WHERE (?='' OR status=?) AND (id LIKE ? OR account_id LIKE ? OR plan_code LIKE ?) ORDER BY {order} {},id DESC LIMIT ? OFFSET ?",
                direction(&q)
            );
            let r = sqlx::query(&sql)
                .bind(status)
                .bind(status)
                .bind(&search)
                .bind(&search)
                .bind(&search)
                .bind(number(&q, "limit", 50, 1, 200))
                .bind(number(&q, "offset", 0, 0, 1_000_000))
                .fetch_all(&app.db)
                .await?;
            Ok(
                json!({"payments":r.iter().map(row_json).collect::<Vec<_>>(),"total":total,"limit":number(&q,"limit",50,1,200),"offset":number(&q,"offset",0,0,1_000_000)}),
            )
        }
        (["requests"], "GET") => requests(app, None, &q, false).await,
        (["requests", "groups"], "GET") => requests(app, None, &q, true).await,
        (["settings"], "GET") => Ok(json!({"settings":app.config.read().await.redacted()})),
        (["settings"], "PATCH") => update_settings(app, body(req).await?).await,
        _ => Err(Error::new(
            404,
            "not_found",
            "Unknown administrator endpoint",
        )),
    }
}
async fn patch_account(app: &App, aid: &str, b: Value) -> Result<Value> {
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    let r = sqlx::query("SELECT * FROM accounts WHERE id=?")
        .bind(aid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::new(404, "account_not_found", "Account not found"))?;
    let status = b["status"].as_str().unwrap_or(r.get("status"));
    if !["active", "suspended"].contains(&status) {
        return Err(Error::bad("Invalid account status"));
    }
    let mut markup: Option<String> = r.get("markup_pct");
    if let Some(v) = b.get("markup_pct") {
        markup = if v.is_null() {
            None
        } else {
            let s = v
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| v.to_string());
            money::decimal(&s)?;
            Some(s)
        };
    }
    sqlx::query(
        "UPDATE accounts SET status=?,contact=?,note=?,can_create_keys=?,markup_pct=? WHERE id=?",
    )
    .bind(status)
    .bind(b["contact"].as_str().unwrap_or(r.get("contact")))
    .bind(b["note"].as_str().unwrap_or(r.get("note")))
    .bind(
        b["can_create_keys"]
            .as_bool()
            .unwrap_or(r.get("can_create_keys")),
    )
    .bind(markup)
    .bind(aid)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    billing::account(app, aid).await
}
async fn save_plan(app: &App, b: Value) -> Result<Value> {
    let code = text(&b, "code")?;
    if code.is_empty()
        || code.len() > 80
        || !code
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
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
    sqlx::query("INSERT INTO plans(code,name,price_micro,credit_micro,duration_days,active,description) VALUES(?,?,?,?,?,?,?) ON CONFLICT(code) DO UPDATE SET name=excluded.name,price_micro=excluded.price_micro,credit_micro=excluded.credit_micro,duration_days=excluded.duration_days,active=excluded.active,description=excluded.description")
 .bind(code).bind(text(&b,"name")?).bind(price).bind(credit).bind(days).bind(b["active"].as_bool().unwrap_or(true)).bind(b["description"].as_str().unwrap_or("")).execute(&app.db).await?;
    billing::plans(app, false).await
}
/// Issues one code from an object, or up to 100 in a single atomic batch when
/// given an array of objects.
async fn create_code(app: &App, b: Value) -> Result<Value> {
    let c = app.config.read().await.clone();
    let bulk = matches!(b, Value::Array(_));
    let items = match b {
        Value::Array(v) => v,
        Value::Object(_) => vec![b],
        _ => return Err(Error::bad("Expected a JSON object or an array of objects")),
    };
    if items.is_empty() {
        return Err(Error::bad("At least one code is required"));
    }
    if items.len() > 100 {
        return Err(Error::bad("At most 100 codes can be issued at once"));
    }
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    let mut codes = Vec::with_capacity(items.len());
    for item in &items {
        if !item.is_object() {
            return Err(Error::bad("Each code must be a JSON object"));
        }
        let code = identity::secret("RES-");
        let plan = item["plan_code"].as_str().filter(|s| !s.is_empty());
        let amount = if plan.is_none() {
            Some(money::value(&item["credit_usd"])?)
        } else {
            None
        };
        if amount.is_some_and(|n| n <= 0) {
            return Err(Error::bad("Credit must be positive"));
        }
        if let Some(p) = plan {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plans WHERE code=? AND active=1")
                .bind(p)
                .fetch_one(&mut *tx)
                .await?;
            if n == 0 {
                return Err(Error::bad("Active plan not found"));
            }
        }
        let expires = expiry(&item["expires_at"])?;
        if expires.is_some_and(|t| t <= now()) {
            return Err(Error::bad("Expiry must be in the future"));
        }
        sqlx::query("INSERT INTO redeem_codes(id,code_hash,code_prefix,plan_code,credit_micro,expires_at,created_at,note) VALUES(?,?,?,?,?,?,?,?)")
            .bind(id())
            .bind(identity::hash(&c.key_pepper, &code))
            .bind(&code[..12])
            .bind(plan)
            .bind(amount)
            .bind(expires)
            .bind(now())
            .bind(item["note"].as_str().unwrap_or(""))
            .execute(&mut *tx)
            .await?;
        codes.push(code);
    }
    tx.commit().await?;
    if bulk {
        Ok(
            json!({"codes":codes,"count":codes.len(),"notice":"Save these codes now. They are shown only once."}),
        )
    } else {
        Ok(json!({"code":codes.remove(0),"notice":"Save this code now. It is shown only once."}))
    }
}
async fn delete_plan(app: &App, code: &str) -> Result<Value> {
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plans WHERE code=?")
        .bind(code)
        .fetch_one(&mut *tx)
        .await?;
    if exists == 0 {
        return Err(Error::new(404, "plan_not_found", "Plan not found"));
    }
    let refs: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM subscriptions WHERE plan_code=?) + (SELECT COUNT(*) FROM payments WHERE plan_code=?) + (SELECT COUNT(*) FROM redeem_codes WHERE plan_code=?)",
    )
    .bind(code)
    .bind(code)
    .bind(code)
    .fetch_one(&mut *tx)
    .await?;
    if refs > 0 {
        return Err(Error::new(
            409,
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
async fn patch_code(app: &App, cid: &str, b: Value) -> Result<Value> {
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    let row = sqlx::query("SELECT redeemed_by,expires_at,note FROM redeem_codes WHERE id=?")
        .bind(cid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::new(404, "code_not_found", "Code not found"))?;
    if row.get::<Option<String>, _>("redeemed_by").is_some() {
        return Err(Error::new(
            400,
            "code_redeemed",
            "Redeemed codes are kept as-is for audit",
        ));
    }
    let expires: Option<i64> = if let Some(v) = b.get("expires_at") {
        expiry(v)?
    } else {
        row.get("expires_at")
    };
    if expires.is_some_and(|t| t <= now()) {
        return Err(Error::bad("Expiry must be in the future"));
    }
    let note = b["note"].as_str().unwrap_or(row.get("note"));
    sqlx::query("UPDATE redeem_codes SET expires_at=?,note=? WHERE id=?")
        .bind(expires)
        .bind(note)
        .bind(cid)
        .execute(&mut *tx)
        .await?;
    let updated = sqlx::query("SELECT * FROM redeem_codes WHERE id=?")
        .bind(cid)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({"code":row_json(&updated)}))
}
async fn delete_code(app: &App, cid: &str) -> Result<Value> {
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    let row = sqlx::query("SELECT redeemed_by FROM redeem_codes WHERE id=?")
        .bind(cid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::new(404, "code_not_found", "Code not found"))?;
    if row.get::<Option<String>, _>("redeemed_by").is_some() {
        return Err(Error::new(
            400,
            "code_redeemed",
            "Redeemed codes are kept as audit records",
        ));
    }
    sqlx::query("DELETE FROM redeem_codes WHERE id=?")
        .bind(cid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({"deleted":true,"id":cid}))
}
fn merge(base: &mut Value, patch: Value) {
    if let (Some(dst), Value::Object(src)) = (base.as_object_mut(), patch) {
        for (k, v) in src {
            if [
                "admin_token",
                "key_pepper",
                "stripe_secret_key",
                "stripe_webhook_secret",
                "api_key",
            ]
            .contains(&k.as_str())
                && v.as_str() == Some("")
            {
                continue;
            }
            if v.is_object()
                && dst.get(&k).is_some_and(Value::is_object)
                && !["prices", "ip_overrides", "default_limits"].contains(&k.as_str())
            {
                merge(dst.get_mut(&k).unwrap(), v);
            } else {
                dst.insert(k, v);
            }
        }
    }
}
async fn update_settings(app: &App, b: Value) -> Result<Value> {
    let mut lock = app.config.write().await;
    let mut v = serde_json::to_value(&*lock)?;
    merge(&mut v, b);
    let c: crate::config::Config = serde_json::from_value(v)?;
    c.validate().map_err(|e| Error::bad(e.to_string()))?;
    if c.admin_token.len() < 24 || c.key_pepper.len() < 24 {
        return Err(Error::bad(
            "Administrator token and key pepper must be at least 24 characters",
        ));
    }
    sqlx::query("UPDATE settings SET value=? WHERE id=1")
        .bind(serde_json::to_string(&c)?)
        .execute(&app.db)
        .await?;
    *lock = c;
    Ok(
        json!({"settings":lock.redacted(),"notice":"Saved. CORS changes take effect after restart."}),
    )
}
/// Appends the shared request-history filters to a query builder.
fn request_filters<'a>(
    sql: &mut QueryBuilder<'a, Sqlite>,
    account: Option<&'a str>,
    q: &'a Query,
) -> Result<()> {
    if let Some(a) = account {
        sql.push(" AND account_id=").push_bind(a);
    }
    for (param, column) in [
        ("key", "key_id"),
        ("account", "account_id"),
        ("model", "model"),
        ("kind", "kind"),
        ("path", "path"),
        ("ip", "ip"),
        ("method", "method"),
    ] {
        if let Some(v) = q.get(param) {
            sql.push(format!(" AND {column}=")).push_bind(v);
        }
    }
    if let Some(v) = q.get("status") {
        match v.as_str() {
            "2xx" => {
                sql.push(" AND status>=200 AND status<300");
            }
            "4xx" => {
                sql.push(" AND status>=400 AND status<500");
            }
            "5xx" => {
                sql.push(" AND status>=500");
            }
            "pending" => {
                sql.push(" AND finished=0 AND status=0");
            }
            other => {
                sql.push(" AND status=")
                    .push_bind(other.parse::<i64>().unwrap_or(-1));
            }
        }
    }
    if let Some(v) = q.get("q") {
        sql.push(" AND (model LIKE ")
            .push_bind(format!("%{v}%"))
            .push(" OR path LIKE ")
            .push_bind(format!("%{v}%"))
            .push(" OR id LIKE ")
            .push_bind(format!("%{v}%"))
            .push(")");
    }
    if let Some(hour) = q.get("hour") {
        let t = hour
            .parse::<i64>()
            .map_err(|_| Error::bad("hour must be Unix seconds"))?;
        sql.push(" AND created_at>=")
            .push_bind(t)
            .push(" AND created_at<")
            .push_bind(t.saturating_add(3600));
    }
    Ok(())
}
async fn requests(app: &App, account: Option<&str>, q: &Query, group: bool) -> Result<Value> {
    let field = match q.get("field").map(String::as_str).unwrap_or("model") {
        "model" => "model",
        "ip" => "ip",
        "status" => "status",
        "kind" => "kind",
        "path" => "path",
        "key" => "key_id",
        "hour" => "created_at/3600*3600",
        _ => return Err(Error::bad("Invalid group field")),
    };
    let total = if group {
        0
    } else {
        let mut count = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) FROM requests WHERE 1=1");
        request_filters(&mut count, account, q)?;
        let row = count.build().fetch_one(&app.db).await?;
        row.get::<i64, _>(0)
    };
    let select = if group {
        format!(
            "SELECT {field} AS label,COUNT(*) AS requests,SUM(tokens) AS tokens,SUM(billed_micro) AS billed_micro FROM requests WHERE 1=1"
        )
    } else {
        "SELECT * FROM requests WHERE 1=1".into()
    };
    let mut sql = QueryBuilder::<Sqlite>::new(select);
    request_filters(&mut sql, account, q)?;
    if group {
        sql.push(format!(" GROUP BY {field} ORDER BY requests DESC"));
    } else {
        sql.push(" ORDER BY created_at DESC");
    }
    sql.push(" LIMIT ")
        .push_bind(number(q, "limit", 50, 1, 200))
        .push(" OFFSET ")
        .push_bind(number(q, "offset", 0, 0, 1_000_000));
    let rows = sql.build().fetch_all(&app.db).await?;
    let values: Vec<_> = rows.iter().map(row_json).collect();
    Ok(if group {
        json!({"groups":values})
    } else {
        json!({"requests":values,"total":total,"limit":number(q,"limit",50,1,200),"offset":number(q,"offset",0,0,1_000_000)})
    })
}
