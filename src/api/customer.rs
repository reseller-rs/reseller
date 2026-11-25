//! Customer workspace API under `/v1/`: keys, account, plans, and
//! self-service signup.
use super::{
    Editor, list_keys, number, object, owned_key, patch_key, query, requests, segments, text,
};
use crate::{
    App, billing,
    error::{Error, Result},
    identity,
    keys::{self, Requester},
    oauth, payments, proxy,
    security::{self, pow},
    stats::{self, Audience},
};
use axum::{body::Body, http::Request};
use serde_json::{Value, json};

/// Routes a `/v1/` request.
///
/// The plan catalogue, signup, and signup challenge routes are public. All
/// other routes require a signed-in customer.
pub async fn handle(app: &App, req: Request<Body>, ip: &str) -> Result<Value> {
    let path = segments(&req, "/v1/");
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    let method = req.method().as_str().to_owned();
    let q = query(&req);

    // Public routes: the plan catalogue and self-service signup.
    match (path.as_slice(), method.as_str()) {
        (["plans"], "GET") => {
            let config = app.config().await;
            let mut plans = billing::plans(app, true).await?;
            plans["payment_methods"] = payments::methods(&config).into();
            plans["request_estimate_usd"] = config.billing.request_estimate_usd.clone().into();
            plans["trial_credit_usd"] = config.signup.free_credit_usd.clone().into();
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
            let created = keys::create(app, Requester::Signup { altcha }, ip, &body).await;
            match &created {
                Ok(_) => security::record_signup_success(app, ip),
                Err(e) if security::is_business_failure(e) => {
                    security::record_business_failure(app, ip).await;
                }
                Err(_) => {}
            }
            return created;
        }
        _ => {}
    }

    let me = identity::required_request(app, req.headers(), req.extensions()).await?;
    let account = me.account_id.as_str();
    match (path.as_slice(), method.as_str()) {
        (["keys"], "GET") => list_keys(app, Some(account), &q).await,
        (["keys", kid], "PATCH") => {
            me.require_active()?;
            patch_key(app, kid, Editor::Customer(account), &object(req).await?).await
        }
        (["keys"], "POST") => {
            keys::create(app, Requester::Customer(&me), ip, &object(req).await?).await
        }
        (["keys", kid], "GET") => {
            let row = owned_key(app, kid, Some(account)).await?;
            Ok(json!({
                "key": keys::key_json(&row),
                "usage": stats::usage(app, Audience::Customer, Some(account), Some(kid), 30).await?,
            }))
        }
        (["keys", kid, "rotate"], "POST") => {
            me.require_active()?;
            keys::rotate(app, kid, Some(account)).await
        }
        (["account"], "GET") => {
            let mut overview = billing::account(app, account).await?;
            overview["request_estimate_usd"] = app
                .config()
                .await
                .billing
                .request_estimate_usd
                .clone()
                .into();
            overview["top_up"] =
                payments::deposits::top_up(app, &*app.config().await, account).await;
            Ok(overview)
        }
        (["account"], "PATCH") => {
            let body = object(req).await?;
            let consent = body["consumption_consent"]
                .as_bool()
                .ok_or_else(|| Error::bad("consumption_consent must be true or false"))?;
            payments::store::set_consent(app, account, consent).await
        }
        (["account", "store"], "GET") => payments::store::account_info(app, account).await,
        (["account", "store", "apple"], "POST") => {
            me.require_active()?;
            payments::store::apple::verify_purchase(app, account, &object(req).await?).await
        }
        (["account", "store", "google"], "POST") => {
            me.require_active()?;
            payments::store::google::verify_purchase(app, account, &object(req).await?).await
        }
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
            let days = number(&q, "days", 30, 1, 365);
            stats::usage(app, Audience::Customer, Some(account), key, days).await
        }
        // OAuth grants (connected MCP apps) on the account's keys.
        (["account", "apps"], "GET") => oauth::list_apps(app, account).await,
        (["account", "apps", id], "DELETE") => oauth::revoke_app(app, account, id).await,
        (["account", "requests"], "GET") => requests(app, Some(account), &q, false).await,
        // The model list of the signed-in key, also for dashboard tokens,
        // which cannot call `/v1/models`.
        (["account", "models"], "GET") => Ok(proxy::model_list(&*app.config().await, Some(&me))),
        _ => Err(Error::not_found("not_found", "Unknown customer endpoint")),
    }
}

/// Issues an ALTCHA challenge for anonymous signup.
///
/// Returns `{"required":false}` when the proof of work is disabled. Fails when
/// self-service signup is disabled.
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
        security::pow_difficulty(app, ip).await,
        config.signup.pow_ttl_seconds,
        crate::db::now(),
    ))?)
}
