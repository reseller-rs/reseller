//! App Store purchases: StoreKit 2 signed transactions, the App Store
//! Server API, and App Store Server Notifications V2.
//!
//! Every payload is a JWS signed by a certificate chain that ends in the
//! pinned Apple Root CA - G3 (see [`super::x509`]). A purchase belongs to the
//! account whose `store_token` the app passed as `appAccountToken`.
use super::{
    account_by_token, end_subscription, jose, plan_by_product, purchase_response, received,
    scaled_terms, subscription_owner, x509,
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
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};
use std::time::Duration;

/// Apple Root CA - G3 (DER), pinned as the only production trust anchor.
/// SHA-256 fingerprint 63:34:3A:BF:B8:9A:6A:03:EB:B5:7E:9B:3F:5F:A7:BE:7C:4F:5C:75:6F:30:17:B3:A8:C4:88:C3:65:3E:91:79.
pub const ROOT_CA_G3: &[u8] = include_bytes!("../../../assets/apple-root-ca-g3.cer");

/// Lifetime of the App Store Server API token, in seconds (Apple allows an hour).
const API_TOKEN_SECONDS: i64 = 20 * 60;
/// Longest a signed payload may claim to be from the future, in seconds.
const SIGNED_SKEW: i64 = 24 * 3600;
const API_TIMEOUT: Duration = Duration::from_secs(30);
/// `consumptionPercentage` and the share thresholds use thousandths of a percent.
const MILLI: i64 = 1000;

/// The fields of a `JWSTransactionDecodedPayload` this server uses.
#[derive(Clone, Debug)]
pub struct Transaction {
    pub transaction_id: String,
    pub original_transaction_id: String,
    pub bundle_id: String,
    pub product_id: String,
    /// `Consumable`, `Auto-Renewable Subscription`, and so on.
    pub kind: String,
    /// Purchase date in Unix seconds.
    pub purchased: i64,
    /// Expiry in Unix seconds (subscriptions).
    pub expires: Option<i64>,
    pub quantity: i64,
    pub account_token: Option<String>,
    pub environment: String,
    pub revoked: bool,
}

impl Transaction {
    /// Reads a verified transaction payload.
    pub fn from_payload(p: &Value) -> Result<Self> {
        let text = |k: &str| {
            p[k].as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid(&format!("transaction lacks {k}")))
        };
        Ok(Self {
            transaction_id: text("transactionId")?,
            original_transaction_id: text("originalTransactionId")?,
            bundle_id: text("bundleId")?,
            product_id: text("productId")?,
            kind: text("type")?,
            purchased: p["purchaseDate"]
                .as_i64()
                .ok_or_else(|| invalid("transaction lacks purchaseDate"))?
                / 1000,
            expires: p["expiresDate"].as_i64().map(|ms| ms / 1000),
            quantity: p["quantity"].as_i64().unwrap_or(1),
            account_token: p["appAccountToken"].as_str().map(str::to_owned),
            environment: text("environment")?,
            revoked: p["revocationDate"].is_number(),
        })
    }

    /// Whether the transaction carries this account's token.
    fn belongs_to(&self, token: &str) -> bool {
        self.account_token
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(token))
    }
}

/// 400 for a payload that does not verify or lacks a field.
fn invalid(reason: &str) -> Error {
    tracing::debug!(reason, "rejected App Store payload");
    Error::new(
        StatusCode::BAD_REQUEST,
        "invalid_store_receipt",
        "The App Store payload could not be verified",
    )
}

/// Verifies a JWS signed by an Apple certificate chain and returns its
/// payload. The chain is checked at the payload's own `signedDate`.
pub fn verify_signed(app: &App, token: &str) -> Result<Value> {
    let check = || -> jose::Checked<Value> {
        let jws = jose::split(token)?;
        if jws.header["alg"] != "ES256" {
            return Err("unexpected algorithm");
        }
        let chain = jws.header["x5c"]
            .as_array()
            .ok_or("missing certificate chain")?
            .iter()
            .map(|c| jose::from_b64(c.as_str().ok_or("invalid certificate")?))
            .collect::<jose::Checked<Vec<_>>>()?;
        let signed = jws.payload["signedDate"]
            .as_i64()
            .ok_or("missing signedDate")?
            / 1000;
        if signed > now() + SIGNED_SKEW {
            return Err("signed in the future");
        }
        let key = x509::verify_chain(&chain, &app.apple_roots, signed)?;
        jose::verify_es256(&key, &jws)?;
        Ok(jws.payload)
    };
    check().map_err(invalid)
}

/// Builds the App Store Server API bearer token.
fn api_token(config: &Config) -> Result<String> {
    let apple = &config.apple;
    let key = jose::pem_to_der(&apple.private_key)
        .map_err(|_| provider_error("Invalid App Store API key"))?;
    let iat = now();
    jose::sign_es256(
        &key,
        &json!({"alg": "ES256", "kid": apple.key_id, "typ": "JWT"}),
        &json!({
            "iss": apple.issuer_id,
            "iat": iat,
            "exp": iat + API_TOKEN_SECONDS,
            "aud": "appstoreconnect-v1",
            "bid": apple.bundle_id,
        }),
    )
    .map_err(|_| provider_error("Invalid App Store API key"))
}

/// Fetches and verifies a transaction through Get Transaction Info.
async fn fetch_transaction(app: &App, config: &Config, id: &str) -> Result<Value> {
    if !config.apple.api_enabled() {
        return Err(unavailable("App Store lookups are not configured"));
    }
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(Error::bad("transaction_id is invalid"));
    }
    let response = app
        .client
        .get(format!(
            "{}/inApps/v1/transactions/{id}",
            config.apple.api_base()
        ))
        .bearer_auth(api_token(config)?)
        .timeout(API_TIMEOUT)
        .send()
        .await
        .map_err(|_| provider_error("Could not contact the App Store"))?;
    if !response.status().is_success() {
        return Err(super::status_error(
            response.status(),
            "apple transaction lookup",
        ));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| provider_error("Invalid App Store response"))?;
    let signed = body["signedTransactionInfo"]
        .as_str()
        .ok_or_else(|| provider_error("Invalid App Store response"))?;
    verify_signed(app, signed)
}

/// Checks that a transaction is for this app and environment.
fn check_scope(config: &Config, tx: &Transaction) -> Result<()> {
    if tx.bundle_id != config.apple.bundle_id {
        return Err(Error::bad("Purchase is for another app").with_code("store_bundle_mismatch"));
    }
    if tx.environment != config.apple.environment.as_apple() {
        return Err(Error::bad("Purchase is from another App Store environment")
            .with_code("store_environment_mismatch"));
    }
    Ok(())
}

/// Checks that the product type matches the plan kind.
fn check_kind(plan: &SqliteRow, tx: &Transaction) -> Result<()> {
    let expected = match plan.get::<String, _>("kind").as_str() {
        "topup" => "Consumable",
        _ => "Auto-Renewable Subscription",
    };
    if tx.kind == expected {
        Ok(())
    } else {
        Err(Error::bad("Product type does not match the plan").with_code("store_product_mismatch"))
    }
}

/// Records a verified transaction for `account`.
async fn record(
    conn: &mut SqliteConnection,
    config: &Config,
    account: &str,
    tx: &Transaction,
    plan: &SqliteRow,
) -> Result<(String, bool)> {
    let terms = scaled_terms(plan, tx.quantity)?;
    let period = match (terms.is_subscription(), tx.expires) {
        (true, Some(end)) => Some((tx.purchased, end)),
        (true, None) => return Err(invalid("subscription lacks expiresDate")),
        (false, _) => None,
    };
    let charge = StoreCharge {
        provider: "apple",
        account,
        plan: &terms,
        external_id: &tx.transaction_id,
        original_transaction_id: Some(&tx.original_transaction_id),
        purchase_token: None,
        product_id: &tx.product_id,
        environment: &tx.environment,
        period,
        auto_renew: true,
    };
    record_store_charge(conn, &charge, config.billing.refund_ip_block_days).await
}

/// `POST /v1/account/store/apple`: verifies a StoreKit 2 transaction
/// (`signed_transaction`, or `transaction_id` looked up at Apple) and credits
/// it to the account once.
pub async fn verify_purchase(app: &App, account: &str, body: &Value) -> Result<Value> {
    let config = app.config().await;
    if !config.apple.enabled() {
        return Err(unavailable("App Store purchases are not configured"));
    }
    let payload = match (
        body["signed_transaction"].as_str(),
        body["transaction_id"].as_str(),
    ) {
        (Some(jws), _) => verify_signed(app, jws)?,
        (None, Some(id)) => fetch_transaction(app, &config, id).await?,
        (None, None) => {
            return Err(Error::bad(
                "signed_transaction or transaction_id is required",
            ));
        }
    };
    let tx = Transaction::from_payload(&payload)?;
    check_scope(&config, &tx)?;
    let token = super::store_token(app, account).await?;
    if !tx.belongs_to(&token) {
        return Err(Error::forbidden(
            "store_account_mismatch",
            "This purchase was made for another account",
        ));
    }
    let mut dbtx = db::write(&app.db).await?;
    let plan = plan_by_product(&mut dbtx, "apple", &tx.product_id, true)
        .await?
        .ok_or_else(|| Error::bad("This product is not for sale").with_code("unknown_product"))?;
    check_kind(&plan, &tx)?;
    if tx.revoked {
        return Err(conflict(
            "purchase_revoked",
            "This purchase was refunded or revoked",
        ));
    }
    if tx.expires.is_some_and(|end| end <= now()) {
        return Err(conflict(
            "subscription_expired",
            "This subscription has expired",
        ));
    }
    let (order, credited) = record(&mut dbtx, &config, account, &tx, &plan).await?;
    dbtx.commit().await?;
    purchase_response(app, account, &order, credited).await
}

/// 409 with a code.
fn conflict(code: &'static str, message: &str) -> Error {
    Error::new(StatusCode::CONFLICT, code, message)
}

/// Handles `POST /webhooks/apple`: verifies the signed notification, then
/// applies it once per `notificationUUID`.
pub async fn webhook(app: &App, body: &[u8]) -> Result<Value> {
    let config = app.config().await;
    if !config.apple.enabled() {
        return Err(unavailable("App Store notifications are not configured"));
    }
    let envelope: Value = serde_json::from_slice(body)?;
    let signed = envelope["signedPayload"]
        .as_str()
        .ok_or_else(|| Error::bad("signedPayload is required"))?;
    let payload = verify_signed(app, signed)?;
    let (kind, subtype) = (
        payload["notificationType"].as_str().unwrap_or(""),
        payload["subtype"].as_str().unwrap_or(""),
    );
    let uuid = payload["notificationUUID"]
        .as_str()
        .ok_or_else(|| Error::bad("notificationUUID is required"))?;
    let data = &payload["data"];
    if !for_this_app(&config, data) {
        tracing::info!(
            kind,
            "ignoring App Store notification for another app or environment"
        );
        return Ok(received(false));
    }
    let tx = match data["signedTransactionInfo"].as_str() {
        Some(jws) => Some(Transaction::from_payload(&verify_signed(app, jws)?)?),
        None => None,
    };
    let event = format!("apple:{uuid}");
    match (kind, tx) {
        ("CONSUMPTION_REQUEST", Some(tx)) => consumption_request(app, &config, &event, &tx).await,
        (
            "SUBSCRIBED"
            | "DID_RENEW"
            | "ONE_TIME_CHARGE"
            | "DID_CHANGE_RENEWAL_STATUS"
            | "REFUND"
            | "REVOKE"
            | "REFUND_REVERSED",
            Some(tx),
        ) => apply(app, &config, &event, kind, subtype, &tx).await,
        _ => Ok(received(false)),
    }
}

/// Whether a notification's `data` is for the configured app and environment.
fn for_this_app(config: &Config, data: &Value) -> bool {
    let apple = &config.apple;
    data["bundleId"].as_str() == Some(apple.bundle_id.as_str())
        && data["environment"].as_str() == Some(apple.environment.as_apple())
        && apple
            .app_apple_id
            .is_none_or(|id| data["appAppleId"].as_u64().is_none_or(|got| got == id))
}

/// Applies a purchase, renewal, refund, or renewal-status notification in
/// one write transaction together with its dedupe record.
async fn apply(
    app: &App,
    config: &Config,
    event: &str,
    kind: &str,
    subtype: &str,
    tx: &Transaction,
) -> Result<Value> {
    if check_scope(config, tx).is_err() {
        return Ok(received(false));
    }
    let Some(mut dbtx) = super::claim(app, event).await? else {
        return Ok(received(true));
    };
    let days = config.billing.refund_ip_block_days;
    match kind {
        "SUBSCRIBED" | "DID_RENEW" | "ONE_TIME_CHARGE" => {
            charge_from_notification(&mut dbtx, config, tx).await?;
        }
        "DID_CHANGE_RENEWAL_STATUS" => {
            let enabled = subtype == "AUTO_RENEW_ENABLED";
            if enabled || subtype == "AUTO_RENEW_DISABLED" {
                sqlx::query(
                    "UPDATE subscriptions SET auto_renew=? WHERE original_transaction_id=? AND status='active'",
                )
                .bind(enabled)
                .bind(&tx.original_transaction_id)
                .execute(&mut *dbtx)
                .await?;
            }
        }
        "REFUND" => {
            let accounts =
                refund_store_charge(&mut dbtx, "apple", Some(&tx.transaction_id), None, days)
                    .await?;
            tracing::warn!(transaction = %tx.transaction_id, accounts = accounts.len(), "App Store refund applied");
        }
        "REVOKE" => end_subscription(&mut dbtx, &tx.original_transaction_id, true).await?,
        _ => tracing::warn!(
            transaction = %tx.transaction_id,
            "App Store refund reversed. The account stays suspended until an operator reviews it"
        ),
    }
    dbtx.commit().await?;
    Ok(received(false))
}

/// Records the charge of a SUBSCRIBED, DID_RENEW, or ONE_TIME_CHARGE
/// notification for the account it belongs to. Charges that cannot be
/// matched or that already ended are logged and skipped.
async fn charge_from_notification(
    conn: &mut SqliteConnection,
    config: &Config,
    tx: &Transaction,
) -> Result<()> {
    let account = match &tx.account_token {
        Some(token) => account_by_token(conn, token).await?,
        None => None,
    };
    let account = match account {
        Some(account) => Some(account),
        None => subscription_owner(conn, &tx.original_transaction_id).await?,
    };
    let Some(account) = account else {
        tracing::warn!(transaction = %tx.transaction_id, "App Store charge for an unknown account");
        return Ok(());
    };
    let Some(plan) = plan_by_product(conn, "apple", &tx.product_id, false).await? else {
        tracing::warn!(product = %tx.product_id, "App Store charge for an unknown product");
        return Ok(());
    };
    if check_kind(&plan, tx).is_err() || tx.revoked || tx.expires.is_some_and(|end| end <= now()) {
        tracing::warn!(transaction = %tx.transaction_id, "skipping an ended or mismatched App Store charge");
        return Ok(());
    }
    record(conn, config, &account, tx, &plan).await?;
    Ok(())
}

/// The refund preference Apple is told, from the share consumed in
/// thousandths of a percent.
fn refund_preference(milli_percent: i64, decline_pct: u32) -> &'static str {
    if milli_percent == 0 {
        "GRANT_FULL"
    } else if milli_percent >= i64::from(decline_pct) * MILLI {
        "DECLINE"
    } else {
        "GRANT_PRORATED"
    }
}

/// The Send Consumption Information (v2) request body.
fn consumption_body(
    milli_percent: i64,
    subscription: bool,
    decline_pct: u32,
    sample: bool,
) -> Value {
    let mut body = json!({
        "customerConsented": true,
        "deliveryStatus": "DELIVERED",
        "refundPreference": refund_preference(milli_percent, decline_pct),
        "sampleContentProvided": sample,
    });
    if !subscription {
        body["consumptionPercentage"] = milli_percent.into();
    }
    body
}

/// Answers a CONSUMPTION_REQUEST with the share of the purchase the customer
/// used, but only when reporting is on and the customer consented. The call
/// happens outside any write lock, and the event is recorded afterwards, so
/// a failed call is retried by Apple.
async fn consumption_request(
    app: &App,
    config: &Config,
    event: &str,
    tx: &Transaction,
) -> Result<Value> {
    if !config.apple.consumption_reporting || !config.apple.api_enabled() {
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
    let payment = sqlx::query(
        "SELECT p.id,p.kind,a.consumption_consent FROM payments p JOIN accounts a ON a.id=p.account_id \
         WHERE p.provider='apple' AND p.external_id=?",
    )
    .bind(&tx.transaction_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(payment) = payment.filter(|p| p.get::<bool, _>("consumption_consent")) else {
        tracing::info!(transaction = %tx.transaction_id, "no consumption data sent: unknown purchase or no consent");
        return Ok(received(false));
    };
    let used = crate::billing::consumption(&mut conn, &payment.get::<String, _>("id")).await?;
    drop(conn);
    let sample = config.offers_free_tier();
    let body = consumption_body(
        used.milli_percent,
        payment.get::<String, _>("kind") == "subscription",
        config.billing.refund_decline_pct,
        sample,
    );
    let response = app
        .client
        .put(format!(
            "{}/inApps/v2/transactions/consumption/{}",
            config.apple.api_base(),
            tx.transaction_id
        ))
        .bearer_auth(api_token(config)?)
        .json(&body)
        .timeout(API_TIMEOUT)
        .send()
        .await
        .map_err(|_| provider_error("Could not contact the App Store"))?;
    if !response.status().is_success() {
        return Err(super::status_error(
            response.status(),
            "apple consumption information",
        ));
    }
    let mut dbtx = db::write(&app.db).await?;
    let duplicate = !crate::payments::first_delivery(&mut dbtx, event).await?;
    dbtx.commit().await?;
    Ok(received(duplicate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refund_preference_follows_the_decline_threshold() {
        assert_eq!(refund_preference(0, 50), "GRANT_FULL");
        assert_eq!(refund_preference(1, 50), "GRANT_PRORATED");
        assert_eq!(refund_preference(49_999, 50), "GRANT_PRORATED");
        assert_eq!(refund_preference(50_000, 50), "DECLINE");
        assert_eq!(refund_preference(100_000, 50), "DECLINE");
    }

    #[test]
    fn consumption_body_omits_the_percentage_for_subscriptions() {
        let sub = consumption_body(25_000, true, 50, false);
        assert_eq!(
            sub,
            json!({"customerConsented": true, "deliveryStatus": "DELIVERED",
                   "refundPreference": "GRANT_PRORATED", "sampleContentProvided": false})
        );
        let topup = consumption_body(60_000, false, 50, true);
        assert_eq!(topup["consumptionPercentage"], 60_000);
        assert_eq!(topup["refundPreference"], "DECLINE");
        assert_eq!(topup["sampleContentProvided"], true);
    }

    #[test]
    fn transactions_need_their_identifying_fields() {
        let ok = json!({"transactionId": "1", "originalTransactionId": "1", "bundleId": "b",
            "productId": "p", "type": "Consumable", "purchaseDate": 5000, "environment": "Sandbox",
            "appAccountToken": "ABC"});
        let tx = Transaction::from_payload(&ok).unwrap();
        assert_eq!((tx.purchased, tx.quantity, tx.revoked), (5, 1, false));
        assert!(tx.belongs_to("abc"));
        assert!(!tx.belongs_to("abd"));
        assert!(Transaction::from_payload(&json!({"transactionId": "1"})).is_err());
    }
}
