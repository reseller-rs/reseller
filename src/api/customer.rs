//! Customer workspace API under `/v1/keys`, `/v1/account`, and `/v1/plans`.
use super::{
    Editor, list_keys, number, object, owned_key, patch_key, query, requests, segments, text,
};
use crate::{
    App, billing,
    error::{Error, Result},
    identity::{self, Scope},
    keys::{self, Requester},
    payments, stats,
};
use axum::{body::Body, http::Request};
use serde_json::Value;

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
        (["keys"], "POST") => {
            let creator = identity::authenticate(app, req.headers(), Scope::Manage).await?;
            let body = object(req).await?;
            return keys::create(app, Requester::Public(creator.as_ref()), ip, &body).await;
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
        (["keys", kid, "rotate"], "POST") => keys::rotate(app, kid, Some(account)).await,
        (["account"], "GET") => billing::account(app, account).await,
        (["account", "redeem"], "POST") => {
            let body = object(req).await?;
            billing::redeem(app, account, text(&body, "code")?).await
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
