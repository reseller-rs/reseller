//! JSON management APIs: the customer workspace (`/v1/keys`, `/v1/account`,
//! `/v1/plans`) and the administrator console (`/admin/api/*`).
mod admin;
mod customer;

pub use admin::handle as admin;
pub use customer::handle as customer;

use crate::{
    App,
    db::{DAY, now},
    error::{Error, Result},
    keys::{self, Limits},
    money,
};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use sqlx::{
    AssertSqlSafe, Column, QueryBuilder, Row, Sqlite, TypeInfo, ValueRef, sqlite::SqliteRow,
};
use std::collections::HashMap;

/// Decoded query-string parameters.
pub type Query = HashMap<String, String>;

const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_TEXT_BYTES: usize = 4096;

pub fn query(req: &Request<Body>) -> Query {
    url::form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes())
        .into_owned()
        .collect()
}

/// Splits `/prefix/a/b/` into `["a", "b"]`.
fn segments(req: &Request<Body>, prefix: &str) -> Vec<String> {
    req.uri()
        .path()
        .strip_prefix(prefix)
        .unwrap_or("")
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Reads a JSON object body; an empty body is treated as `{}`.
async fn object(req: Request<Body>) -> Result<Value> {
    let v = json_body(req).await?;
    if !v.is_object() {
        return Err(Error::bad("Expected a JSON object"));
    }
    Ok(v)
}

/// Reads any JSON body; an empty body is treated as `{}`.
async fn json_body(req: Request<Body>) -> Result<Value> {
    let bytes = to_bytes(req.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|_| Error::too_large("Management request exceeds 256 KiB"))?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// `limit`/`offset` pagination.
#[derive(Clone, Copy)]
struct Page {
    limit: i64,
    offset: i64,
}

impl Page {
    fn from(q: &Query) -> Self {
        Self {
            limit: number(q, "limit", 50, 1, 200),
            offset: number(q, "offset", 0, 0, 1_000_000),
        }
    }

    /// Wraps `items` under `key` with paging metadata.
    fn wrap(self, key: &str, items: Vec<Value>, total: i64) -> Value {
        let mut out = json!({"total": total, "limit": self.limit, "offset": self.offset});
        out[key] = Value::Array(items);
        out
    }
}

fn number(q: &Query, key: &str, default: i64, min: i64, max: i64) -> i64 {
    q.get(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

/// Sort direction from `dir=asc|desc` (default descending).
fn direction(q: &Query) -> &'static str {
    if q.get("dir").is_some_and(|v| v.eq_ignore_ascii_case("asc")) {
        "ASC"
    } else {
        "DESC"
    }
}

/// `%term%` for a LIKE search on `q` (matches everything when absent).
fn search(q: &Query) -> String {
    format!("%{}%", q.get("q").map(String::as_str).unwrap_or(""))
}

/// The `status` filter, or `""` for any.
fn status_filter(q: &Query) -> &str {
    q.get("status").map(String::as_str).unwrap_or("")
}

/// Required string field of bounded length.
fn text<'a>(b: &'a Value, k: &str) -> Result<&'a str> {
    b[k].as_str()
        .filter(|s| s.len() <= MAX_TEXT_BYTES)
        .ok_or_else(|| {
            Error::bad(format!(
                "{k} must be a string (at most {MAX_TEXT_BYTES} bytes)"
            ))
        })
}

/// Optional string field: `fallback` when absent, an error when malformed.
fn text_or<'a>(b: &'a Value, k: &str, fallback: &'a str) -> Result<&'a str> {
    if b.get(k).is_some() {
        text(b, k)
    } else {
        Ok(fallback)
    }
}

/// Parses an expiry given as Unix seconds, an RFC 3339 string, or null.
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

/// Parses an optional markup override: null clears it, a decimal sets it.
fn markup(v: &Value) -> Result<Option<String>> {
    if v.is_null() {
        Ok(None)
    } else {
        money::percent(v).map(Some)
    }
}

/// Generic JSON view of a row: secret hashes are omitted and `*_micro`
/// columns are rendered as `*_usd` decimal strings.
fn row_json(r: &SqliteRow) -> Value {
    let mut out = serde_json::Map::new();
    for column in r.columns() {
        let name = column.name();
        if matches!(name, "key_hash" | "dash_hash" | "code_hash") {
            continue;
        }
        let Ok(raw) = r.try_get_raw(column.ordinal()) else {
            continue;
        };
        let value = if raw.is_null() {
            Value::Null
        } else {
            match raw.type_info().name() {
                "INTEGER" => json!(r.get::<i64, _>(column.ordinal())),
                "REAL" => json!(r.get::<f64, _>(column.ordinal())),
                _ => json!(r.get::<String, _>(column.ordinal())),
            }
        };
        match name.strip_suffix("_micro") {
            Some(stem) => {
                let usd = value
                    .as_i64()
                    .map_or(Value::Null, |n| money::display(n).into());
                out.insert(format!("{stem}_usd"), usd);
            }
            None => {
                out.insert(name.into(), value);
            }
        }
    }
    Value::Object(out)
}

/// Paginated key listing, optionally scoped to one account.
async fn list_keys(app: &App, account: Option<&str>, q: &Query) -> Result<Value> {
    let search = search(q);
    let status = status_filter(q);
    let page = Page::from(q);
    const FILTER: &str = "(? IS NULL OR account_id=?) AND (?='' OR status=?) \
                          AND (name LIKE ? OR key_prefix LIKE ? OR id LIKE ?)";
    let total: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM api_keys WHERE {FILTER}"
    )))
    .bind(account)
    .bind(account)
    .bind(status)
    .bind(status)
    .bind(&search)
    .bind(&search)
    .bind(&search)
    .fetch_one(&app.db)
    .await?;
    let order = match q.get("sort").map(String::as_str) {
        Some("name") => "name",
        Some("status") => "status",
        Some("account") => "account_id",
        Some("last_used") => "last_used_at",
        Some("usage") => "usage_30d_micro",
        _ => "created_at",
    };
    // `order` and the direction come from fixed whitelists.
    let sql = format!(
        "SELECT k.*,COALESCE((SELECT SUM(u.billed_micro) FROM usage_buckets u \
           WHERE u.key_id=k.id AND u.hour>=?),0) AS usage_30d_micro \
         FROM api_keys k WHERE {FILTER} \
         ORDER BY {order} {} NULLS LAST,id DESC LIMIT ? OFFSET ?",
        direction(q)
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(now() - 30 * DAY)
        .bind(account)
        .bind(account)
        .bind(status)
        .bind(status)
        .bind(&search)
        .bind(&search)
        .bind(&search)
        .bind(page.limit)
        .bind(page.offset)
        .fetch_all(&app.db)
        .await?;
    let items = rows
        .iter()
        .map(|r| {
            let mut v = keys::key_json(r);
            v["usage_30d_usd"] = money::display(r.get("usage_30d_micro")).into();
            v
        })
        .collect();
    Ok(page.wrap("keys", items, total))
}

/// Fetches a key, scoped to `account` when given (404 otherwise).
async fn owned_key(app: &App, kid: &str, account: Option<&str>) -> Result<SqliteRow> {
    sqlx::query("SELECT * FROM api_keys WHERE id=? AND (? IS NULL OR account_id=?)")
        .bind(kid)
        .bind(account)
        .bind(account)
        .fetch_optional(&app.db)
        .await?
        .ok_or_else(|| Error::not_found("key_not_found", "Key not found"))
}

/// Who is editing a key.
#[derive(Clone, Copy)]
enum Editor<'a> {
    /// Customers may rename their keys and block them, nothing else.
    Customer(&'a str),
    /// Administrators may also set status, limits, markup, and expiry.
    Admin,
}

async fn patch_key(app: &App, kid: &str, editor: Editor<'_>, b: &Value) -> Result<Value> {
    let account = match editor {
        Editor::Customer(account) => Some(account),
        Editor::Admin => None,
    };
    let row = owned_key(app, kid, account).await?;
    let current: String = row.get("status");
    let name = keys::clip(text_or(b, "name", row.get("name"))?, 80);
    let status = text_or(b, "status", &current)?;
    let allowed = match editor {
        Editor::Admin => ["active", "blocked", "revoked"].contains(&status),
        // Customers may block a key but never (re)activate or revoke one.
        Editor::Customer(_) => b.get("status").is_none() || status == "blocked",
    };
    if !allowed {
        return Err(Error::bad("Invalid status"));
    }
    if current == "revoked" && status != "revoked" {
        return Err(Error::bad("Revocation is permanent"));
    }
    let mut limits: String = row.get("limits");
    let mut markup_pct: Option<String> = row.get("markup_pct");
    let mut expires: Option<i64> = row.get("expires_at");
    if matches!(editor, Editor::Admin) {
        if let Some(v) = b.get("limits") {
            let parsed: Limits = serde_json::from_value(v.clone())?;
            parsed.validate()?;
            limits = serde_json::to_string(&parsed)?;
        }
        if let Some(v) = b.get("markup_pct") {
            markup_pct = markup(v)?;
        }
        if let Some(v) = b.get("expires_at") {
            expires = expiry(v)?;
        }
    }
    sqlx::query(
        "UPDATE api_keys SET name=?,status=?,limits=?,markup_pct=?,expires_at=? \
         WHERE id=? AND (status<>'revoked' OR ?='revoked')",
    )
    .bind(name)
    .bind(status)
    .bind(limits)
    .bind(markup_pct)
    .bind(expires)
    .bind(kid)
    .bind(status)
    .execute(&app.db)
    .await?;
    Ok(json!({"key": keys::key_json(&owned_key(app, kid, account).await?)}))
}

/// Appends the request-history filters shared by listing and grouping.
fn request_filters(sql: &mut QueryBuilder<Sqlite>, account: Option<&str>, q: &Query) -> Result<()> {
    if let Some(account) = account {
        sql.push(" AND account_id=").push_bind(account.to_owned());
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
            sql.push(format_args!(" AND {column}="))
                .push_bind(v.clone());
        }
    }
    if let Some(v) = q.get("status") {
        match v.as_str() {
            "2xx" => sql.push(" AND status>=200 AND status<300"),
            "4xx" => sql.push(" AND status>=400 AND status<500"),
            "5xx" => sql.push(" AND status>=500"),
            "pending" => sql.push(" AND finished=0 AND status=0"),
            other => sql
                .push(" AND status=")
                .push_bind(other.parse::<i64>().unwrap_or(-1)),
        };
    }
    if let Some(v) = q.get("q") {
        let like = format!("%{v}%");
        sql.push(" AND (model LIKE ")
            .push_bind(like.clone())
            .push(" OR path LIKE ")
            .push_bind(like.clone())
            .push(" OR id LIKE ")
            .push_bind(like)
            .push(")");
    }
    if let Some(hour) = q.get("hour") {
        let start = hour
            .parse::<i64>()
            .map_err(|_| Error::bad("hour must be Unix seconds"))?;
        sql.push(" AND created_at>=")
            .push_bind(start)
            .push(" AND created_at<")
            .push_bind(start.saturating_add(3600));
    }
    Ok(())
}

/// Request history, either as a page of rows or grouped by `field`.
async fn requests(app: &App, account: Option<&str>, q: &Query, grouped: bool) -> Result<Value> {
    let page = Page::from(q);
    if grouped {
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
        let mut sql = QueryBuilder::<Sqlite>::new(format!(
            "SELECT {field} AS label,COUNT(*) AS requests,SUM(tokens) AS tokens,\
             SUM(billed_micro) AS billed_micro FROM requests WHERE 1=1"
        ));
        request_filters(&mut sql, account, q)?;
        sql.push(format_args!(
            " GROUP BY {field} ORDER BY requests DESC LIMIT "
        ))
        .push_bind(page.limit)
        .push(" OFFSET ")
        .push_bind(page.offset);
        let rows = sql.build().fetch_all(&app.db).await?;
        return Ok(json!({"groups": rows.iter().map(row_json).collect::<Vec<_>>()}));
    }
    let mut count = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) FROM requests WHERE 1=1");
    request_filters(&mut count, account, q)?;
    let total: i64 = count.build().fetch_one(&app.db).await?.get(0);
    let mut sql = QueryBuilder::<Sqlite>::new("SELECT * FROM requests WHERE 1=1");
    request_filters(&mut sql, account, q)?;
    sql.push(" ORDER BY created_at DESC LIMIT ")
        .push_bind(page.limit)
        .push(" OFFSET ")
        .push_bind(page.offset);
    let rows = sql.build().fetch_all(&app.db).await?;
    Ok(page.wrap("requests", rows.iter().map(row_json).collect(), total))
}
