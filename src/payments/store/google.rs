//! Google Play purchases: the Play Developer API and Real-time Developer
//! Notifications delivered through a Cloud Pub/Sub push subscription.
//!
//! The server authenticates to Google with a service account (an RS256
//! signed JWT exchanged for an access token) and authenticates Pub/Sub
//! pushes by their OIDC token. A purchase belongs to the account whose
//! `store_token` the app passed as `obfuscatedAccountId`. Purchases are
//! acknowledged (subscriptions) or consumed (top-ups) only after they are
//! credited, because Google refunds purchases left unacknowledged for three
//! days.
use super::{
    account_by_token, end_subscription, jose, plan_by_product, purchase_response, received,
    scaled_terms, settle, subscription_owner,
};
use crate::{
    App,
    config::Config,
    db::{self, now},
    error::{Error, Result},
    payments::{
        StoreCharge, provider_error, record_store_charge, refund_store_charge, unavailable,
    },
};
use axum::http::{HeaderMap, StatusCode, header};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};
use std::{collections::HashMap, time::Duration};

/// OAuth scope of the Play Developer API.
const SCOPE: &str = "https://www.googleapis.com/auth/androidpublisher";
/// Token endpoint used when the service account key names none.
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// Lifetime of the service account assertion, in seconds.
const ASSERTION_SECONDS: i64 = 3600;
/// Access tokens are renewed this long before they expire, in seconds.
const TOKEN_MARGIN: i64 = 60;
/// Clock skew tolerated on push tokens, in seconds.
const SKEW: i64 = 60;
/// Fewest seconds between fetches of Google's signing keys.
const JWKS_REFETCH: i64 = 60;
const API_TIMEOUT: Duration = Duration::from_secs(30);
/// Pub/Sub tokens are issued by either spelling of Google's issuer.
const ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];
/// `consumptionPercentageMilliunits` and the share thresholds use
/// thousandths of a percent.
const MILLI: i64 = 1000;
/// Chargeback review reason in a pending refund notification.
const CHARGEBACK: i64 = 7;
/// Characters kept as they are in a path segment.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_');

/// Google's OIDC signing keys by `kid`: RSA modulus and exponent.
#[derive(Default)]
pub struct Jwks {
    keys: HashMap<String, (Vec<u8>, Vec<u8>)>,
    fetched: i64,
}

/// A service account access token.
pub struct AccessToken {
    value: String,
    expires: i64,
}

/// Percent-encodes one path segment.
fn seg(value: &str) -> String {
    utf8_percent_encode(value, SEGMENT).to_string()
}

/// 401 for a push that does not carry Google's token.
fn unauthorized(reason: &str) -> Error {
    tracing::warn!(reason, "rejected Pub/Sub push");
    Error::unauthorized("invalid_push_token", "Invalid push credentials")
}

/// Fetches the signing key `kid`, refetching the JWKS for an unknown key at
/// most once a minute.
async fn signing_key(app: &App, config: &Config, kid: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    let refetch = {
        let mut cache = app
            .store
            .jwks
            .lock()
            .map_err(|_| Error::internal("jwks lock"))?;
        if let Some(key) = cache.keys.get(kid) {
            return Ok(key.clone());
        }
        let due = now() - cache.fetched >= JWKS_REFETCH;
        if due {
            cache.fetched = now();
        }
        due
    };
    if !refetch {
        return Err(unauthorized("unknown signing key"));
    }
    let response = app
        .client
        .get(&config.google.certs_url)
        .timeout(API_TIMEOUT)
        .send()
        .await
        .map_err(|_| provider_error("Could not fetch Google's signing keys"))?;
    if !response.status().is_success() {
        return Err(provider_error("Could not fetch Google's signing keys"));
    }
    let jwks: Value = response
        .json()
        .await
        .map_err(|_| provider_error("Invalid signing keys"))?;
    let mut keys = HashMap::new();
    for key in jwks["keys"].as_array().into_iter().flatten() {
        let part = |k: &str| key[k].as_str().and_then(|v| jose::from_b64url(v).ok());
        if let (Some(kid), Some(n), Some(e)) = (key["kid"].as_str(), part("n"), part("e"))
            && key["kty"] == "RSA"
        {
            keys.insert(kid.to_owned(), (n, e));
        }
    }
    let mut cache = app
        .store
        .jwks
        .lock()
        .map_err(|_| Error::internal("jwks lock"))?;
    cache.keys = keys;
    cache
        .keys
        .get(kid)
        .cloned()
        .ok_or_else(|| unauthorized("unknown signing key"))
}

/// The audience the push token must name.
fn expected_audience(config: &Config) -> String {
    match config.google.pubsub_audience.as_str() {
        "" => format!("{}/webhooks/google", config.public_base()),
        audience => audience.to_owned(),
    }
}

/// Verifies the OIDC bearer token of a Pub/Sub push.
async fn verify_push(app: &App, config: &Config, headers: &HeaderMap) -> Result<()> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| unauthorized("missing bearer token"))?;
    let jws = jose::split(token).map_err(unauthorized)?;
    if jws.header["alg"] != "RS256" {
        return Err(unauthorized("unexpected algorithm"));
    }
    let kid = jws.header["kid"]
        .as_str()
        .ok_or_else(|| unauthorized("missing kid"))?;
    let (n, e) = signing_key(app, config, kid).await?;
    jose::verify_rs256(&n, &e, &jws).map_err(unauthorized)?;
    let claims = &jws.payload;
    let t = now();
    let verified = claims["email_verified"] == true || claims["email_verified"] == "true";
    let checks = [
        (
            claims["iss"]
                .as_str()
                .is_some_and(|iss| ISSUERS.contains(&iss)),
            "wrong issuer",
        ),
        (
            claims["aud"].as_str() == Some(expected_audience(config).as_str()),
            "wrong audience",
        ),
        (
            !config.google.pubsub_service_account.is_empty()
                && claims["email"].as_str() == Some(config.google.pubsub_service_account.as_str()),
            "wrong service account",
        ),
        (verified, "email not verified"),
        (
            claims["exp"].as_i64().is_some_and(|exp| exp + SKEW >= t),
            "expired",
        ),
        (
            claims["iat"].as_i64().is_none_or(|iat| iat - SKEW <= t),
            "issued in the future",
        ),
    ];
    match checks.iter().find(|(ok, _)| !ok) {
        Some((_, reason)) => Err(unauthorized(reason)),
        None => Ok(()),
    }
}

/// Returns a valid service account access token, fetching one when needed.
async fn access_token(app: &App, config: &Config) -> Result<String> {
    let mut cached = app.store.token.lock().await;
    if let Some(token) = cached.as_ref().filter(|t| t.expires - TOKEN_MARGIN > now()) {
        return Ok(token.value.clone());
    }
    let bad_key = || provider_error("Invalid Google service account key");
    let key: Value =
        serde_json::from_str(&config.google.service_account_json).map_err(|_| bad_key())?;
    let token_uri = key["token_uri"].as_str().unwrap_or(DEFAULT_TOKEN_URI);
    let der = jose::pem_to_der(key["private_key"].as_str().ok_or_else(bad_key)?)
        .map_err(|_| bad_key())?;
    let iat = now();
    let assertion = jose::sign_rs256(
        &der,
        &json!({"alg": "RS256", "typ": "JWT"}),
        &json!({
            "iss": key["client_email"],
            "scope": SCOPE,
            "aud": token_uri,
            "iat": iat,
            "exp": iat + ASSERTION_SECONDS,
        }),
    )
    .map_err(|_| bad_key())?;
    let response = app
        .client
        .post(token_uri)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ])
        .timeout(API_TIMEOUT)
        .send()
        .await
        .map_err(|_| provider_error("Could not contact Google"))?;
    if !response.status().is_success() {
        return Err(super::status_error(
            response.status(),
            "google token exchange",
        ));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| provider_error("Invalid Google response"))?;
    let value = body["access_token"]
        .as_str()
        .ok_or_else(|| provider_error("Invalid Google response"))?
        .to_owned();
    let lifetime = body["expires_in"].as_i64().unwrap_or(ASSERTION_SECONDS);
    *cached = Some(AccessToken {
        value: value.clone(),
        expires: iat + lifetime,
    });
    Ok(value)
}

/// Sends an authorized request to the Play Developer API and returns the
/// JSON reply (empty replies become `null`).
async fn api(
    app: &App,
    config: &Config,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value> {
    let url = format!(
        "{}/androidpublisher/v3/applications/{}/{path}",
        config.google.api_base(),
        seg(&config.google.package_name)
    );
    let mut request = app
        .client
        .request(method, url)
        .bearer_auth(access_token(app, config).await?)
        .timeout(API_TIMEOUT);
    request = match body {
        Some(body) => request.json(body),
        None => request.header(header::CONTENT_LENGTH, 0),
    };
    let response = request
        .send()
        .await
        .map_err(|_| provider_error("Could not contact Google Play"))?;
    if !response.status().is_success() {
        return Err(super::status_error(response.status(), "google play api"));
    }
    Ok(response
        .bytes()
        .await
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null))
}

/// A `purchases.subscriptionsv2` resource, reduced to what this server uses.
#[derive(Clone, Debug)]
pub struct Subscription {
    pub state: String,
    pub product_id: Option<String>,
    pub expiry: Option<i64>,
    pub ack_pending: bool,
    pub order_id: Option<String>,
    pub linked_token: Option<String>,
    pub test: bool,
    pub account_id: Option<String>,
    pub auto_renew: bool,
}

impl Subscription {
    /// Reads the resource. `product` picks the line item when there are several.
    fn parse(v: &Value, product: Option<&str>) -> Self {
        let items = v["lineItems"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let item = items
            .iter()
            .find(|i| product.is_some_and(|p| i["productId"] == p))
            .or_else(|| items.first().filter(|_| product.is_none()));
        let text = |value: &Value| value.as_str().map(str::to_owned);
        Self {
            state: v["subscriptionState"].as_str().unwrap_or("").to_owned(),
            product_id: item.and_then(|i| text(&i["productId"])),
            expiry: item
                .and_then(|i| i["expiryTime"].as_str())
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.timestamp()),
            ack_pending: v["acknowledgementState"] == "ACKNOWLEDGEMENT_STATE_PENDING",
            order_id: text(&v["latestSuccessfulOrderId"]),
            linked_token: text(&v["linkedPurchaseToken"]),
            test: !v["testPurchase"].is_null(),
            account_id: text(&v["externalAccountIdentifiers"]["obfuscatedExternalAccountId"]),
            auto_renew: item
                .and_then(|i| i["autoRenewingPlan"]["autoRenewEnabled"].as_bool())
                .unwrap_or(true),
        }
    }

    /// Whether the customer currently has paid access.
    fn payable(&self) -> bool {
        matches!(
            self.state.as_str(),
            "SUBSCRIPTION_STATE_ACTIVE" | "SUBSCRIPTION_STATE_IN_GRACE_PERIOD"
        ) && self.expiry.is_some_and(|end| end > now())
    }
}

/// A `purchases.products` resource, reduced to what this server uses.
#[derive(Clone, Debug)]
pub struct OneTime {
    /// 0 purchased, 1 canceled, 2 pending.
    pub state: i64,
    pub order_id: Option<String>,
    pub quantity: i64,
    pub account_id: Option<String>,
    pub test: bool,
    pub consumed: bool,
}

impl OneTime {
    fn parse(v: &Value) -> Self {
        Self {
            state: v["purchaseState"].as_i64().unwrap_or(-1),
            order_id: v["orderId"].as_str().map(str::to_owned),
            quantity: v["quantity"].as_i64().unwrap_or(1),
            account_id: v["obfuscatedExternalAccountId"].as_str().map(str::to_owned),
            test: v["purchaseType"].as_i64() == Some(0),
            consumed: v["consumptionState"].as_i64() == Some(1),
        }
    }
}

/// Fetches a subscription purchase.
async fn fetch_subscription(
    app: &App,
    config: &Config,
    token: &str,
    product: Option<&str>,
) -> Result<Subscription> {
    let path = format!("purchases/subscriptionsv2/tokens/{}", seg(token));
    let v = api(app, config, reqwest::Method::GET, &path, None).await?;
    Ok(Subscription::parse(&v, product))
}

/// Fetches a one-time product purchase.
async fn fetch_one_time(app: &App, config: &Config, product: &str, token: &str) -> Result<OneTime> {
    let path = format!("purchases/products/{}/tokens/{}", seg(product), seg(token));
    Ok(OneTime::parse(
        &api(app, config, reqwest::Method::GET, &path, None).await?,
    ))
}

/// Acknowledges a subscription purchase.
async fn acknowledge(app: &App, product: &str, token: &str) -> Result<()> {
    let config = app.config().await;
    let path = format!(
        "purchases/subscriptions/{}/tokens/{}:acknowledge",
        seg(product),
        seg(token)
    );
    api(app, &config, reqwest::Method::POST, &path, None)
        .await
        .map(drop)
}

/// Consumes a one-time purchase so the product can be bought again.
async fn consume(app: &App, product: &str, token: &str) -> Result<()> {
    let config = app.config().await;
    let path = format!(
        "purchases/products/{}/tokens/{}:consume",
        seg(product),
        seg(token)
    );
    api(app, &config, reqwest::Method::POST, &path, None)
        .await
        .map(drop)
}

/// Acknowledges after crediting, retrying in the background on failure.
async fn settle_acknowledge(app: &App, product: &str, token: &str) {
    let (a, p, t) = (app.clone(), product.to_owned(), token.to_owned());
    settle(app, "google acknowledge", move || {
        let (a, p, t) = (a.clone(), p.clone(), t.clone());
        async move { acknowledge(&a, &p, &t).await }
    })
    .await;
}

/// Consumes after crediting, retrying in the background on failure.
async fn settle_consume(app: &App, product: &str, token: &str) {
    let (a, p, t) = (app.clone(), product.to_owned(), token.to_owned());
    settle(app, "google consume", move || {
        let (a, p, t) = (a.clone(), p.clone(), t.clone());
        async move { consume(&a, &p, &t).await }
    })
    .await;
}

/// Records a payable subscription for `account` and ends the subscription
/// it replaces, if any.
async fn credit_subscription(
    conn: &mut SqliteConnection,
    config: &Config,
    account: &str,
    token: &str,
    sub: &Subscription,
    plan: &SqliteRow,
) -> Result<(String, bool)> {
    let (Some(order), Some(product), Some(end)) = (&sub.order_id, &sub.product_id, sub.expiry)
    else {
        return Err(provider_error("Incomplete subscription from Google Play"));
    };
    let terms = scaled_terms(plan, 1)?;
    let environment = if sub.test { "Sandbox" } else { "Production" };
    let charge = StoreCharge {
        provider: "google",
        account,
        plan: &terms,
        external_id: order,
        original_transaction_id: None,
        purchase_token: Some(token),
        product_id: product,
        environment,
        period: Some((now(), end)),
        auto_renew: sub.auto_renew,
    };
    let recorded = record_store_charge(conn, &charge, config.billing.refund_ip_block_days).await?;
    if let Some(old) = &sub.linked_token {
        end_subscription(conn, old, false).await?;
    }
    Ok(recorded)
}

/// Records a completed one-time purchase for `account`.
async fn credit_one_time(
    conn: &mut SqliteConnection,
    config: &Config,
    account: &str,
    product: &str,
    token: &str,
    purchase: &OneTime,
    plan: &SqliteRow,
) -> Result<(String, bool)> {
    let terms = scaled_terms(plan, purchase.quantity)?;
    let external = purchase
        .order_id
        .clone()
        .unwrap_or_else(|| format!("token:{token}"));
    let charge = StoreCharge {
        provider: "google",
        account,
        plan: &terms,
        external_id: &external,
        original_transaction_id: None,
        purchase_token: Some(token),
        product_id: product,
        environment: if purchase.test {
            "Sandbox"
        } else {
            "Production"
        },
        period: None,
        auto_renew: false,
    };
    record_store_charge(conn, &charge, config.billing.refund_ip_block_days).await
}

/// 409 with a code.
fn conflict(code: &'static str, message: &str) -> Error {
    Error::new(StatusCode::CONFLICT, code, message)
}

/// 403 for a purchase made under another account's id.
fn mismatch() -> Error {
    Error::forbidden(
        "store_account_mismatch",
        "This purchase was made for another account",
    )
}

/// `POST /v1/account/store/google`: verifies a Play purchase with Google and
/// credits it to the account once, then acknowledges or consumes it.
pub async fn verify_purchase(app: &App, account: &str, body: &Value) -> Result<Value> {
    let config = app.config().await;
    if !config.google.enabled() {
        return Err(unavailable("Google Play purchases are not configured"));
    }
    let field = |k: &str| {
        body[k]
            .as_str()
            .filter(|v| !v.is_empty() && v.len() <= 4096)
            .ok_or_else(|| Error::bad(format!("{k} is required")))
    };
    let (product, token) = (field("product_id")?, field("purchase_token")?);
    let store_token = super::store_token(app, account).await?;
    let plan = {
        let mut conn = app.db.acquire().await?;
        plan_by_product(&mut conn, "google", product, true).await?
    }
    .ok_or_else(|| Error::bad("This product is not for sale").with_code("unknown_product"))?;
    if plan.get::<String, _>("kind") == "subscription" {
        let sub = fetch_subscription(app, &config, token, Some(product)).await?;
        if sub.account_id.as_deref() != Some(store_token.as_str()) {
            return Err(mismatch());
        }
        if sub.product_id.is_none() {
            return Err(
                Error::bad("Purchase is for another product").with_code("store_product_mismatch")
            );
        }
        if !sub.payable() {
            return Err(conflict(
                "subscription_not_active",
                "This subscription is not active",
            ));
        }
        let mut tx = db::write(&app.db).await?;
        let (order, credited) =
            credit_subscription(&mut tx, &config, account, token, &sub, &plan).await?;
        tx.commit().await?;
        if sub.ack_pending {
            settle_acknowledge(app, product, token).await;
        }
        return purchase_response(app, account, &order, credited).await;
    }
    let purchase = fetch_one_time(app, &config, product, token).await?;
    if purchase.account_id.as_deref() != Some(store_token.as_str()) {
        return Err(mismatch());
    }
    match purchase.state {
        0 => {}
        2 => {
            return Err(conflict(
                "purchase_pending",
                "This purchase is still pending",
            ));
        }
        _ => return Err(conflict("purchase_revoked", "This purchase was canceled")),
    }
    let mut tx = db::write(&app.db).await?;
    let (order, credited) =
        credit_one_time(&mut tx, &config, account, product, token, &purchase, &plan).await?;
    tx.commit().await?;
    if !purchase.consumed {
        settle_consume(app, product, token).await;
    }
    purchase_response(app, account, &order, credited).await
}

/// Handles `POST /webhooks/google`: authenticates the Pub/Sub push, then
/// applies the developer notification once per Pub/Sub `messageId`.
pub async fn webhook(app: &App, headers: &HeaderMap, body: &[u8]) -> Result<Value> {
    let config = app.config().await;
    if !config.google.enabled() {
        return Err(unavailable("Google Play notifications are not configured"));
    }
    verify_push(app, &config, headers).await?;
    let envelope: Value = serde_json::from_slice(body)?;
    let message = &envelope["message"];
    let id = message["messageId"]
        .as_str()
        .ok_or_else(|| Error::bad("messageId is required"))?;
    let note: Option<Value> = message["data"]
        .as_str()
        .and_then(|d| jose::from_b64(d).ok())
        .and_then(|d| serde_json::from_slice(&d).ok());
    let Some(note) = note else {
        // A message that cannot be read never will be: acknowledge it.
        tracing::warn!(message = id, "unreadable Google Play notification");
        return Ok(received(false));
    };
    if note["packageName"].as_str() != Some(config.google.package_name.as_str()) {
        return Ok(received(false));
    }
    let event = format!("google:{id}");
    if let Some(n) = note.get("subscriptionNotification") {
        return subscription_event(app, &config, &event, n).await;
    }
    if let Some(n) = note.get("oneTimeProductNotification") {
        return one_time_event(app, &config, &event, n).await;
    }
    if let Some(n) = note.get("voidedPurchaseNotification") {
        return voided_event(app, &config, &event, n).await;
    }
    if let Some(n) = note.get("pendingRefundReviewNotification") {
        return refund_review(app, &config, &event, n).await;
    }
    Ok(received(false))
}

/// Handles a subscription notification.
async fn subscription_event(app: &App, config: &Config, event: &str, n: &Value) -> Result<Value> {
    let (Some(token), kind) = (
        n["purchaseToken"].as_str(),
        n["notificationType"].as_i64().unwrap_or(0),
    ) else {
        return Ok(received(false));
    };
    match kind {
        // RECOVERED, RENEWED, PURCHASED, RESTARTED.
        1 | 2 | 4 | 7 => {
            subscription_charge(app, config, event, token, n["subscriptionId"].as_str()).await
        }
        // CANCELED: the customer keeps access but will not renew.
        3 => {
            let Some(mut tx) = super::claim(app, event).await? else {
                return Ok(received(true));
            };
            sqlx::query("UPDATE subscriptions SET auto_renew=0 WHERE purchase_token=?")
                .bind(token)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok(received(false))
        }
        // REVOKED.
        12 => refund(app, config, event, None, Some(token)).await,
        _ => Ok(received(false)),
    }
}

/// Fetches the subscription, credits it, and acknowledges it when pending.
async fn subscription_charge(
    app: &App,
    config: &Config,
    event: &str,
    token: &str,
    product: Option<&str>,
) -> Result<Value> {
    let sub = fetch_subscription(app, config, token, product).await?;
    let Some(mut tx) = super::claim(app, event).await? else {
        return Ok(received(true));
    };
    let account = match &sub.account_id {
        Some(id) => account_by_token(&mut tx, id).await?,
        None => None,
    };
    let account = match account {
        Some(account) => Some(account),
        None => subscription_owner(&mut tx, token).await?,
    };
    let plan = match &sub.product_id {
        Some(product) => plan_by_product(&mut tx, "google", product, false).await?,
        None => None,
    };
    let (Some(account), Some(plan)) = (account, plan) else {
        tracing::warn!("Google Play subscription for an unknown account or product");
        tx.commit().await?;
        return Ok(received(false));
    };
    if !sub.payable() {
        tx.commit().await?;
        return Ok(received(false));
    }
    credit_subscription(&mut tx, config, &account, token, &sub, &plan).await?;
    tx.commit().await?;
    if let (true, Some(product)) = (sub.ack_pending, &sub.product_id) {
        settle_acknowledge(app, product, token).await;
    }
    Ok(received(false))
}

/// Handles a one-time product notification: a purchase is credited and
/// consumed, and a cancellation of a pending purchase grants nothing.
async fn one_time_event(app: &App, config: &Config, event: &str, n: &Value) -> Result<Value> {
    let (Some(token), Some(product), 1) = (
        n["purchaseToken"].as_str(),
        n["sku"].as_str(),
        n["notificationType"].as_i64().unwrap_or(0),
    ) else {
        return Ok(received(false));
    };
    let purchase = fetch_one_time(app, config, product, token).await?;
    let Some(mut tx) = super::claim(app, event).await? else {
        return Ok(received(true));
    };
    let account = match &purchase.account_id {
        Some(id) => account_by_token(&mut tx, id).await?,
        None => None,
    };
    let plan = plan_by_product(&mut tx, "google", product, false).await?;
    let (Some(account), Some(plan), 0) = (account, plan, purchase.state) else {
        tracing::warn!(
            product,
            "Google Play purchase not credited: unknown account or product, or not completed"
        );
        tx.commit().await?;
        return Ok(received(false));
    };
    credit_one_time(&mut tx, config, &account, product, token, &purchase, &plan).await?;
    tx.commit().await?;
    if !purchase.consumed {
        settle_consume(app, product, token).await;
    }
    Ok(received(false))
}

/// Handles a voided purchase (refund, chargeback, or revocation).
async fn voided_event(app: &App, config: &Config, event: &str, n: &Value) -> Result<Value> {
    refund(
        app,
        config,
        event,
        n["orderId"].as_str(),
        n["purchaseToken"].as_str(),
    )
    .await
}

/// Marks matching charges refunded and blocks their accounts, once per event.
async fn refund(
    app: &App,
    config: &Config,
    event: &str,
    order: Option<&str>,
    token: Option<&str>,
) -> Result<Value> {
    let Some(mut tx) = super::claim(app, event).await? else {
        return Ok(received(true));
    };
    let days = config.billing.refund_ip_block_days;
    let accounts = refund_store_charge(&mut tx, "google", order, token, days).await?;
    tx.commit().await?;
    tracing::warn!(accounts = accounts.len(), "Google Play refund applied");
    Ok(received(false))
}

/// The refund preference Google is told for a chargeback review.
fn review_preference(milli_percent: i64, decline_pct: u32) -> &'static str {
    if milli_percent == 0 {
        "APPROVE"
    } else if milli_percent >= i64::from(decline_pct) * MILLI {
        "DECLINE"
    } else {
        "NEUTRAL"
    }
}

/// Answers a pending chargeback review with the share of the purchase the
/// customer used. The call happens outside any write lock and the event is
/// recorded afterwards, so a failed call is retried by Pub/Sub.
async fn refund_review(app: &App, config: &Config, event: &str, n: &Value) -> Result<Value> {
    let (Some(order), Some(pending)) = (n["orderId"].as_str(), n["pendingRefundToken"].as_str())
    else {
        return Ok(received(false));
    };
    if !config.google.refund_review || n["refundReason"].as_i64() != Some(CHARGEBACK) {
        return Ok(received(false));
    }
    let mut conn = app.db.acquire().await?;
    let seen: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payment_events WHERE id=?)")
        .bind(event)
        .fetch_one(&mut *conn)
        .await?;
    if seen {
        return Ok(received(true));
    }
    let payment: Option<String> =
        sqlx::query_scalar("SELECT id FROM payments WHERE provider='google' AND external_id=?")
            .bind(order)
            .fetch_optional(&mut *conn)
            .await?;
    let Some(payment) = payment else {
        tracing::info!(order, "chargeback review for an unknown order");
        return Ok(received(false));
    };
    let used = crate::billing::consumption(&mut conn, &payment).await?;
    drop(conn);
    let sample = config.offers_free_tier();
    let request = json!({
        "pendingRefundToken": pending,
        "sampleContentProvided": sample,
        "refundPreference": review_preference(used.milli_percent, config.billing.refund_decline_pct),
        "consumptionPercentageMilliunits": used.milli_percent,
    });
    let path = format!("orders/{}:reviewrefund", seg(order));
    api(app, config, reqwest::Method::POST, &path, Some(&request)).await?;
    let mut tx = db::write(&app.db).await?;
    let duplicate = !crate::payments::first_delivery(&mut tx, event).await?;
    tx.commit().await?;
    Ok(received(duplicate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_preference_follows_the_decline_threshold() {
        assert_eq!(review_preference(0, 50), "APPROVE");
        assert_eq!(review_preference(10_000, 50), "NEUTRAL");
        assert_eq!(review_preference(50_000, 50), "DECLINE");
    }

    #[test]
    fn subscriptions_parse_the_matching_line_item() {
        let v = json!({
            "subscriptionState": "SUBSCRIPTION_STATE_ACTIVE",
            "acknowledgementState": "ACKNOWLEDGEMENT_STATE_PENDING",
            "latestSuccessfulOrderId": "GPA.1",
            "testPurchase": {},
            "externalAccountIdentifiers": {"obfuscatedExternalAccountId": "acct"},
            "lineItems": [
                {"productId": "other", "expiryTime": "2030-01-01T00:00:00Z"},
                {"productId": "pro", "expiryTime": "2031-01-01T00:00:00Z",
                 "autoRenewingPlan": {"autoRenewEnabled": false}},
            ],
        });
        let sub = Subscription::parse(&v, Some("pro"));
        assert_eq!(sub.product_id.as_deref(), Some("pro"));
        assert_eq!(sub.expiry, Some(1_924_992_000));
        assert!(sub.ack_pending && sub.test && !sub.auto_renew && sub.payable());
        assert_eq!(sub.account_id.as_deref(), Some("acct"));
        assert!(
            Subscription::parse(&v, Some("missing"))
                .product_id
                .is_none()
        );
        assert_eq!(
            Subscription::parse(&v, None).product_id.as_deref(),
            Some("other")
        );
    }

    #[test]
    fn path_segments_keep_order_ids_and_escape_the_rest() {
        assert_eq!(seg("GPA.1234-5678_9"), "GPA.1234-5678_9");
        assert_eq!(seg("a/b:c"), "a%2Fb%3Ac");
    }
}
