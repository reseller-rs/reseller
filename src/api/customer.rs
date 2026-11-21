//! Customer workspace API under `/v1/keys`, `/v1/account`, and `/v1/plans`.
use super::{
    Editor, list_keys, number, object, owned_key, patch_key, query, requests, segments, text,
};
use crate::{
    App, billing,
    error::{Error, Result},
    identity,
    keys::{self, Requester},
    payments,
    security::{self, pow},
    stats,
};
use axum::{body::Body, http::Request};
use serde_json::{Value, json};

pub async fn handle(app: &App, req: Request<Body>, ip: &str) -> Result<Value> {
    let path = segments(&req, "/v1/");
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    let method = req.method().as_str().to_owned();
    let q = query(&req);

    // Public routes: the plan catalogue and self-service signup.
    match (path.as_slice(), method.as_str()) {
        (["plans"], "GET") => {
            let mut plans = billing::plans(app, true).await?;
            plans["payment_methods"] = payments::methods(&*app.config().await).into();
            return Ok(plans);
        }
        (["signup", "challenge"], "GET") => return signup_challenge(app, ip).await,
        (["signup"], "POST") => {
            let body = object(req).await?;
            let altcha = match body.get("altcha") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.as_str()),
                Some(_) => return Err(Error::bad("altcha must be a string")),
            };
            return keys::create(app, Requester::Signup { altcha }, ip, &body).await;
        }
        _ => {}
    }

    let me = identity::required(app, req.headers()).await?;
    let account = me.account_id.as_str();
    match (path.as_slice(), method.as_str()) {
        (["keys"], "GET") => list_keys(app, Some(account), &q).await,
        (["keys", kid], "PATCH") => {
            patch_key(app, kid, Editor::Customer(account), &object(req).await?).await
        }
        (["keys"], "POST") => {
            keys::create(app, Requester::Customer(&me), ip, &object(req).await?).await
        }
        (["keys", kid, "rotate"], "POST") => keys::rotate(app, kid, Some(account)).await,
        (["account"], "GET") => billing::account(app, account).await,
        (["account", "redeem"], "POST") => {
            let body = object(req).await?;
            security::check_redeem(app, account).await?;
            let redeemed = billing::redeem(app, account, text(&body, "code")?).await;
            if let Err(e) = &redeemed
                && e.code == "invalid_code"
            {
                security::record_redeem_failure(app, account, ip).await;
            }
            redeemed
        }
        (["account", "checkout"], "POST") => {
            payments::checkout(app, &me, &object(req).await?).await
        }
        (["account", "payments"], "GET") => payments::account_orders(app, account).await,
        (["account", "payments", pid], "GET") => payments::order(app, pid, Some(account)).await,
        (["account", "usage"], "GET") => {
            let key = q.get("key").map(String::as_str);
            if let Some(key) = key {
                owned_key(app, key, Some(account)).await?;
            }
            stats::usage(app, Some(account), key, number(&q, "days", 30, 1, 365)).await
        }
        (["account", "requests"], "GET") => requests(app, Some(account), &q, false).await,
        _ => Err(Error::not_found("not_found", "Unknown customer endpoint")),
    }
}

/// ALTCHA challenge for anonymous signup (`{"required":false}` when the
/// proof of work is disabled).
async fn signup_challenge(app: &App, ip: &str) -> Result<Value> {
    let config = app.config().await;
    if !config.signup.enabled {
        return Err(Error::forbidden(
            "signup_disabled",
            "Self-service signup is disabled",
        ));
    }
    if config.signup.pow_max_number == 0 {
        return Ok(json!({"required": false}));
    }
    security::challenge(app, ip).await?;
    Ok(serde_json::to_value(pow::create(
        &config.key_pepper,
        config.signup.pow_max_number,
        config.signup.pow_ttl_seconds,
        crate::db::now(),
    ))?)
}
