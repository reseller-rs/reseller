//! Payroof crypto payments.
//!
//! Each order is a Payroof *reference* with its own deposit address per EVM
//! chain. At checkout the order is quoted once per accepted asset (USD-pegged
//! stablecoins 1:1, native coins at the current USD rate); quotes are fixed
//! for `quote_ttl_minutes`. Payroof posts signed `seen` and `confirmed`
//! webhooks; confirmed transfers are valued against their quote, and the order
//! is paid once the confirmed value covers the price (within the configured
//! underpayment tolerance). Payments in several assets or several transfers
//! add up.
use super::{complete, create_order, first_delivery, provider_error, unavailable};
use crate::{
    App,
    config::{Config, Payroof, RateSource},
    db::{self, id, now},
    error::{Error, Result},
    identity::Identity,
    money,
};
use axum::http::{HeaderMap, Method, StatusCode};
use hmac::{Hmac, KeyInit, Mac};
use rust_decimal::{Decimal, RoundingStrategy, prelude::ToPrimitive};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};
use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Accepted clock skew for webhook timestamps, in seconds.
const SIGNATURE_TOLERANCE: u64 = 300;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CATALOG_TTL: Duration = Duration::from_secs(600);
const RATE_TTL: Duration = Duration::from_secs(120);
/// Native quotes are rounded up to at most this many decimal places so they
/// are practical to type into a wallet.
const NATIVE_QUOTE_DECIMALS: u32 = 8;
const PPM: u128 = 1_000_000;
/// Transfers are valued at most at 1000x their quote.
const MAX_VALUE_PPM: u128 = 1_000 * PPM;
pub const WEBHOOK_PATH: &str = "/webhooks/payroof";

/// One asset in the project's public catalog.
#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub id: String,
    pub symbol: String,
    pub decimals: u32,
}

/// One chain in the project's public catalog.
#[derive(Clone, Debug, Deserialize)]
pub struct Chain {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub confirmations: u64,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Asset {
    pub fn is_native(&self) -> bool {
        self.id.contains("/slip44:")
    }
}

/// A project catalog fetched at `at` for the API/project pair `key`.
struct CachedCatalog {
    at: Instant,
    key: String,
    chains: Arc<Vec<Chain>>,
}

/// Short-lived caches for the project catalog and native-coin rates.
#[derive(Default)]
pub struct Cache {
    catalog: Mutex<Option<CachedCatalog>>,
    rates: Mutex<HashMap<String, (Instant, Decimal)>>,
}

/// Decodes the 32-byte HMAC key from a `whsec_<base64url>` secret.
pub fn webhook_key(secret: &str) -> Option<[u8; 32]> {
    base64url(secret.strip_prefix("whsec_")?)?.try_into().ok()
}

/// Unpadded (padding tolerated) base64url decoding.
fn base64url(input: &str) -> Option<Vec<u8>> {
    let input = input.trim_end_matches('=');
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for byte in input.bytes() {
        let v = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be zero padding of a canonical encoding.
    (bits < 6 && acc == 0).then_some(out)
}

/// Verifies `Payroof-Signature: v1=<hex HMAC-SHA256(key, "<ts>." || body)>`
/// in constant time at Unix time `now`.
pub fn verify(secret: &str, timestamp: &str, signature: &str, body: &[u8], now: i64) -> Result<()> {
    let fail = || {
        Error::new(
            StatusCode::BAD_REQUEST,
            "invalid_signature",
            "Invalid or expired Payroof signature",
        )
    };
    let key = webhook_key(secret).ok_or_else(fail)?;
    let signed_at: i64 = timestamp.trim().parse().map_err(|_| fail())?;
    if now.abs_diff(signed_at) > SIGNATURE_TOLERANCE {
        return Err(fail());
    }
    let expected = signature
        .split(',')
        .filter_map(|s| s.trim().strip_prefix("v1="))
        .filter_map(|s| hex::decode(s).ok());
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|_| fail())?;
    mac.update(timestamp.trim().as_bytes());
    mac.update(b".");
    mac.update(body);
    let mut valid = false;
    for sig in expected {
        valid |= mac.clone().verify_slice(&sig).is_ok();
    }
    if valid { Ok(()) } else { Err(fail()) }
}

/// Calls the Payroof API with the project token.
async fn call(
    app: &App,
    config: &Payroof,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value> {
    let url = format!("{}{path}", config.api_base());
    let mut request = app
        .client
        .request(method, &url)
        .bearer_auth(&config.token)
        .timeout(REQUEST_TIMEOUT);
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send().await.map_err(|e| {
        tracing::warn!(error = %e, "Payroof request failed");
        provider_error("Could not contact the crypto payment provider")
    })?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(value);
    }
    let code = value["error"].as_str().unwrap_or("unknown");
    tracing::warn!(%status, code, path, "Payroof rejected request");
    Err(match status.as_u16() {
        401 | 403 => provider_error("The crypto payment provider rejected the project token"),
        429 | 503 => unavailable("Crypto payments are temporarily unavailable; try again shortly"),
        _ => provider_error("The crypto payment provider rejected the request"),
    })
}

fn project_path(config: &Payroof) -> String {
    format!("/v1/projects/{}", config.project)
}

/// The project's chain/asset catalog (cached for ten minutes).
pub async fn catalog(app: &App, config: &Payroof) -> Result<Arc<Vec<Chain>>> {
    let key = format!("{}|{}", config.api_base(), config.project);
    if let Some(cached) = app.payroof.catalog.lock().expect("cache lock").as_ref()
        && cached.key == key
        && cached.at.elapsed() < CATALOG_TTL
    {
        return Ok(cached.chains.clone());
    }
    let view = call(app, config, Method::GET, &project_path(config), None).await?;
    let chains: Vec<Chain> =
        serde_json::from_value(view["chains"].clone()).map_err(Error::internal)?;
    let chains = Arc::new(chains);
    *app.payroof.catalog.lock().expect("cache lock") = Some(CachedCatalog {
        at: Instant::now(),
        key,
        chains: chains.clone(),
    });
    Ok(chains)
}

/// USD rates for native `symbols`: static overrides first, then the feed.
/// Symbols without a usable rate are omitted.
async fn rates(app: &App, config: &Payroof, symbols: &[String]) -> HashMap<String, Decimal> {
    let mut out = HashMap::new();
    let mut wanted: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for symbol in symbols {
        let upper = symbol.to_ascii_uppercase();
        if let Some(rate) = config
            .native_rates_usd
            .iter()
            .find(|(s, _)| s.eq_ignore_ascii_case(symbol))
            .and_then(|(_, r)| money::decimal(r).ok())
            .filter(|r| !r.is_zero())
        {
            out.insert(upper, rate);
            continue;
        }
        if config.rate_source != RateSource::Coingecko {
            continue;
        }
        if let Some((at, rate)) = app.payroof.rates.lock().expect("cache lock").get(&upper)
            && at.elapsed() < RATE_TTL
        {
            out.insert(upper, *rate);
            continue;
        }
        if let Some((_, id)) = config
            .rate_ids
            .iter()
            .find(|(s, _)| s.eq_ignore_ascii_case(symbol))
        {
            wanted.entry(id.clone()).or_default().push(upper);
        }
    }
    if wanted.is_empty() {
        return out;
    }
    let ids = wanted.keys().cloned().collect::<Vec<_>>().join(",");
    let mut url = match url::Url::parse(&config.rate_url) {
        Ok(url) => url,
        Err(_) => return out,
    };
    url.query_pairs_mut()
        .append_pair("ids", &ids)
        .append_pair("vs_currencies", "usd");
    let response = app
        .client
        .get(url)
        .header("accept", "application/json")
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    let body: Value = match response {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or(Value::Null),
        Ok(r) => {
            tracing::warn!(status = %r.status(), "native coin rate feed rejected request");
            return out;
        }
        Err(e) => {
            tracing::warn!(error = %e, "native coin rate feed unavailable");
            return out;
        }
    };
    let mut cache = app.payroof.rates.lock().expect("cache lock");
    for (coin, symbols) in wanted {
        let Some(rate) = body[&coin]["usd"]
            .as_number()
            .and_then(|n| {
                let s = n.to_string();
                Decimal::from_str(&s)
                    .or_else(|_| Decimal::from_scientific(&s))
                    .ok()
            })
            .filter(|r| r.is_sign_positive() && !r.is_zero())
        else {
            continue;
        };
        for symbol in symbols {
            cache.insert(symbol.clone(), (Instant::now(), rate));
            out.insert(symbol, rate);
        }
    }
    out
}

/// Price in base units for a stablecoin with `decimals`, rounded up.
fn stable_amount(price_micro: i64, decimals: u32) -> Option<u128> {
    let price = u128::try_from(price_micro).ok()?;
    let scaled = price.checked_mul(10u128.checked_pow(decimals)?)?;
    Some(scaled.div_ceil(PPM))
}

/// Price in base units for a native coin at `rate` USD, rounded up to
/// [`NATIVE_QUOTE_DECIMALS`] places.
fn native_amount(price_micro: i64, rate: Decimal, decimals: u32) -> Option<u128> {
    let usd = Decimal::from(price_micro) / Decimal::from(1_000_000);
    let coins = usd.checked_div(rate)?.round_dp_with_strategy(
        decimals.min(NATIVE_QUOTE_DECIMALS),
        RoundingStrategy::ToPositiveInfinity,
    );
    let mantissa = u128::try_from(coins.mantissa()).ok()?;
    mantissa.checked_mul(10u128.checked_pow(decimals.checked_sub(coins.scale())?)?)
}

/// Formats base units as a decimal string (`1500000`, 6 -> `"1.5"`).
pub fn display_units(amount: &str, decimals: u32) -> String {
    let digits = amount.trim_start_matches('0');
    let decimals = decimals as usize;
    if digits.is_empty() {
        return "0".into();
    }
    let padded = format!("{digits:0>width$}", width = decimals + 1);
    let (whole, frac) = padded.split_at(padded.len() - decimals);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole}.{frac}")
    }
}

/// Value in micro-USD of `amount` against a quote of `quote` base units for
/// an order priced at `price_micro`.
fn value_micro(amount: u128, quote: u128, price_micro: i64) -> i64 {
    if quote == 0 {
        return 0;
    }
    let ppm = amount
        .checked_mul(PPM)
        .map_or(MAX_VALUE_PPM, |n| n / quote)
        .min(MAX_VALUE_PPM);
    let price = u128::try_from(price_micro).unwrap_or(0);
    i64::try_from(price * ppm / PPM).unwrap_or(i64::MAX)
}

/// Starts a crypto checkout: allocates per-chain deposit addresses and quotes
/// the order in every accepted asset.
pub(super) async fn checkout(
    app: &App,
    config: &Config,
    identity: &Identity,
    plan: &SqliteRow,
) -> Result<Value> {
    let settings = &config.payroof;
    let price: i64 = plan.get("price_micro");
    let chains = catalog(app, settings).await?;
    let order = id();
    let reference = call(
        app,
        settings,
        Method::PUT,
        &format!("{}/references/{order}", project_path(settings)),
        None,
    )
    .await?;
    let destinations: HashMap<String, String> = reference["destinations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| {
            Some((
                d["chain"].as_str()?.to_owned(),
                d["address"].as_str()?.to_owned(),
            ))
        })
        .collect();
    let allowed_chain = |chain: &str| {
        settings.chains.is_empty()
            || settings
                .chains
                .iter()
                .any(|c| c.eq_ignore_ascii_case(chain))
    };
    let natives: Vec<String> = if settings.native_coins {
        chains
            .iter()
            .filter(|c| allowed_chain(&c.id) && destinations.contains_key(&c.id))
            .flat_map(|c| {
                c.assets
                    .iter()
                    .filter(|a| a.is_native())
                    .map(|a| a.symbol.clone())
            })
            .collect()
    } else {
        Vec::new()
    };
    let native_rates = rates(app, settings, &natives).await;
    let mut quotes = Vec::new();
    for chain in chains.iter().filter(|c| allowed_chain(&c.id)) {
        let Some(address) = destinations.get(&chain.id) else {
            continue;
        };
        for asset in &chain.assets {
            let stable = !asset.is_native()
                && settings
                    .stablecoins
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(&asset.symbol));
            let (amount, rate) = if stable {
                (stable_amount(price, asset.decimals), Decimal::ONE)
            } else if asset.is_native() && settings.native_coins {
                let Some(rate) = native_rates.get(&asset.symbol.to_ascii_uppercase()) else {
                    continue;
                };
                (native_amount(price, *rate, asset.decimals), *rate)
            } else {
                continue;
            };
            let Some(amount) = amount.filter(|a| *a > 0) else {
                continue;
            };
            quotes.push((chain, asset, address, amount, rate));
        }
    }
    if quotes.is_empty() {
        return Err(unavailable(
            "No crypto asset can be quoted right now. Try again later or choose another method.",
        ));
    }
    let expires = now() + i64::from(settings.quote_ttl_minutes) * 60;
    let mut tx = db::write(&app.db).await?;
    create_order(
        &mut tx,
        &order,
        &identity.account_id,
        plan,
        "payroof",
        Some(expires),
    )
    .await?;
    sqlx::query("UPDATE payments SET external_id=? WHERE id=?")
        .bind(&order)
        .bind(&order)
        .execute(&mut *tx)
        .await?;
    for (chain, asset, address, amount, rate) in quotes {
        sqlx::query(
            "INSERT INTO payment_quotes(payment_id,chain,chain_name,asset,symbol,decimals,address,amount,rate_usd) \
             VALUES(?,?,?,?,?,?,?,?,?)",
        )
        .bind(&order)
        .bind(&chain.id)
        .bind(&chain.name)
        .bind(&asset.id)
        .bind(&asset.symbol)
        .bind(i64::from(asset.decimals))
        .bind(address)
        .bind(amount.to_string())
        .bind(rate.normalize().to_string())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let mut out = super::order(app, &order, Some(&identity.account_id)).await?;
    out["method"] = super::CRYPTO.into();
    out["order_id"] = order.into();
    Ok(out)
}

/// Customer view of a quote.
pub fn quote_json(r: &SqliteRow) -> Value {
    let amount: String = r.get("amount");
    let decimals = u32::try_from(r.get::<i64, _>("decimals")).unwrap_or(0);
    json!({
        "chain": r.get::<String, _>("chain"),
        "chain_name": r.get::<String, _>("chain_name"),
        "asset": r.get::<String, _>("asset"),
        "symbol": r.get::<String, _>("symbol"),
        "decimals": decimals,
        "address": r.get::<String, _>("address"),
        "amount": display_units(&amount, decimals),
        "amount_units": amount,
        "rate_usd": r.get::<String, _>("rate_usd"),
    })
}

/// Customer view of an observed transfer.
pub fn transfer_json(r: &SqliteRow) -> Value {
    json!({
        "id": r.get::<String, _>("id"),
        "chain": r.get::<String, _>("chain"),
        "asset": r.get::<String, _>("asset"),
        "transaction": r.get::<String, _>("tx_hash"),
        "amount_units": r.get::<String, _>("amount"),
        "status": r.get::<String, _>("status"),
        "value_usd": money::display(r.get("value_micro")),
        "note": r.get::<String, _>("note"),
        "seen_at": r.get::<i64, _>("seen_at"),
        "confirmed_at": r.get::<Option<i64>, _>("confirmed_at"),
    })
}

/// A Payroof webhook delivery.
#[derive(Debug, Deserialize)]
struct Event {
    id: String,
    event: String,
    transfer: String,
    project: String,
    reference: String,
    chain: String,
    asset: String,
    address: String,
    transaction: String,
    amount: String,
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// Handles a Payroof webhook: verifies the signature before parsing,
/// deduplicates by delivery id, records the transfer, and completes the order
/// once confirmed transfers cover its price.
pub async fn webhook(app: &App, headers: &HeaderMap, body: &[u8]) -> Result<Value> {
    let config = app.config().await;
    let settings = &config.payroof;
    verify(
        &settings.webhook_secret,
        header(headers, "payroof-timestamp"),
        header(headers, "payroof-signature"),
        body,
        now(),
    )?;
    let event: Event = serde_json::from_slice(body)?;
    let delivery = header(headers, "payroof-id");
    if !delivery.is_empty() && delivery != event.id {
        return Err(Error::bad("Payroof-Id does not match the event"));
    }
    if event.project != settings.project || !matches!(event.event.as_str(), "seen" | "confirmed") {
        return Ok(json!({"received": true, "ignored": true}));
    }
    let mut tx = db::write(&app.db).await?;
    if !first_delivery(&mut tx, &format!("payroof:{}", event.id)).await? {
        return Ok(json!({"received": true, "duplicate": true}));
    }
    let Some(order) = sqlx::query("SELECT * FROM payments WHERE id=? AND provider='payroof'")
        .bind(&event.reference)
        .fetch_optional(&mut *tx)
        .await?
    else {
        // References may belong to other applications sharing the project.
        tx.commit().await?;
        return Ok(json!({"received": true, "ignored": true}));
    };
    record_transfer(&mut tx, &order, &event, settings).await?;
    tx.commit().await?;
    Ok(json!({"received": true}))
}

async fn record_transfer(
    conn: &mut SqliteConnection,
    order: &SqliteRow,
    event: &Event,
    settings: &Payroof,
) -> Result<()> {
    let order_id: String = order.get("id");
    let price: i64 = order.get("price_micro");
    let confirmed = event.event == "confirmed";
    let at = now();
    sqlx::query(
        "INSERT INTO payment_transfers(id,payment_id,chain,asset,address,tx_hash,amount,status,seen_at) \
         VALUES(?,?,?,?,?,?,?,'seen',?) ON CONFLICT(id) DO NOTHING",
    )
    .bind(&event.transfer)
    .bind(&order_id)
    .bind(&event.chain)
    .bind(&event.asset)
    .bind(&event.address)
    .bind(&event.transaction)
    .bind(&event.amount)
    .bind(at)
    .execute(&mut *conn)
    .await?;
    if !confirmed {
        return Ok(());
    }
    let transfer =
        sqlx::query("SELECT payment_id,status,seen_at FROM payment_transfers WHERE id=?")
            .bind(&event.transfer)
            .fetch_one(&mut *conn)
            .await?;
    if transfer.get::<String, _>("payment_id") != order_id {
        return Err(Error::bad("Transfer belongs to another order"));
    }
    if transfer.get::<String, _>("status") == "confirmed" {
        return Ok(());
    }
    let quote = sqlx::query("SELECT * FROM payment_quotes WHERE payment_id=? AND asset=?")
        .bind(&order_id)
        .bind(&event.asset)
        .fetch_optional(&mut *conn)
        .await?;
    let amount = event.amount.parse::<u128>().ok();
    let (value, note) = match (&quote, amount) {
        (None, _) => (0, "unquoted_asset"),
        (_, None) => (0, "invalid_amount"),
        (Some(q), Some(amount)) => {
            let seen_at: i64 = transfer.get("seen_at");
            let late = order
                .get::<Option<i64>, _>("expires_at")
                .is_some_and(|e| seen_at > e);
            if q.get::<String, _>("chain") != event.chain
                || !q
                    .get::<String, _>("address")
                    .eq_ignore_ascii_case(&event.address)
            {
                (0, "address_mismatch")
            } else if late && q.get::<String, _>("rate_usd") != "1" {
                // The native-coin rate is only guaranteed while the quote lasts.
                (0, "late")
            } else {
                let quoted = q.get::<String, _>("amount").parse::<u128>().unwrap_or(0);
                (
                    value_micro(amount, quoted, price),
                    if late { "late" } else { "" },
                )
            }
        }
    };
    if !note.is_empty() {
        tracing::warn!(order = %order_id, transfer = %event.transfer, note, "crypto transfer needs review");
    }
    sqlx::query(
        "UPDATE payment_transfers SET status='confirmed',confirmed_at=?,value_micro=?,note=?,\
         chain=?,asset=?,address=?,tx_hash=?,amount=? WHERE id=?",
    )
    .bind(at)
    .bind(value)
    .bind(note)
    .bind(&event.chain)
    .bind(&event.asset)
    .bind(&event.address)
    .bind(&event.transaction)
    .bind(&event.amount)
    .bind(&event.transfer)
    .execute(&mut *conn)
    .await?;
    let received: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(value_micro),0) FROM payment_transfers WHERE payment_id=? AND status='confirmed'",
    )
    .bind(&order_id)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query("UPDATE payments SET received_micro=? WHERE id=?")
        .bind(received)
        .bind(&order_id)
        .execute(&mut *conn)
        .await?;
    let tolerance = money::decimal(&settings.underpayment_tolerance_pct).unwrap_or_default();
    let required = (Decimal::from(price) * (Decimal::ONE_HUNDRED - tolerance)
        / Decimal::ONE_HUNDRED)
        .ceil()
        .to_i64()
        .unwrap_or(price);
    if received >= required && complete(conn, &order_id, "payroof").await? {
        tracing::info!(order = %order_id, received_micro = received, "crypto payment completed");
    }
    Ok(())
}

/// Console status: configuration, project catalog, and webhooks.
pub async fn status(app: &App) -> Result<Value> {
    let config = app.config().await;
    let settings = &config.payroof;
    let webhook_url = format!("{}{WEBHOOK_PATH}", config.public_base());
    let mut out = json!({
        "enabled": settings.enabled(),
        "project": settings.project,
        "api_url": settings.api_base(),
        "token_configured": !settings.token.is_empty(),
        "webhook_secret_configured": !settings.webhook_secret.is_empty(),
        "webhook_url": webhook_url,
    });
    if settings.project.is_empty() || settings.token.is_empty() {
        return Ok(out);
    }
    match call(app, settings, Method::GET, &project_path(settings), None).await {
        Ok(view) => out["chains"] = view["chains"].clone(),
        Err(e) => out["project_error"] = e.message.into(),
    }
    match call(
        app,
        settings,
        Method::GET,
        &format!("{}/webhooks", project_path(settings)),
        None,
    )
    .await
    {
        Ok(list) => {
            out["webhooks"] = list["webhooks"].clone();
            out["webhook_registered"] = list["webhooks"]
                .as_array()
                .is_some_and(|w| w.iter().any(|h| h["url"] == webhook_url))
                .into();
        }
        Err(e) => out["webhooks_error"] = e.message.into(),
    }
    Ok(out)
}

/// Subscribes `{public_url}/webhooks/payroof` to `seen` and `confirmed` and
/// stores the returned signing secret in the settings.
pub async fn register_webhook(app: &App) -> Result<Value> {
    let config = app.config().await;
    let settings = &config.payroof;
    if settings.project.is_empty() || settings.token.is_empty() {
        return Err(Error::bad("Set payroof.project and payroof.token first"));
    }
    let webhook_url = format!("{}{WEBHOOK_PATH}", config.public_base());
    if !webhook_url.starts_with("https://") {
        return Err(Error::bad(
            "Payroof delivers webhooks only to HTTPS URLs; set public_url to your public https:// origin",
        ));
    }
    let created = call(
        app,
        settings,
        Method::POST,
        &format!("{}/webhooks", project_path(settings)),
        Some(&json!({"url": webhook_url, "events": ["seen", "confirmed"]})),
    )
    .await?;
    let secret = created["secret"]
        .as_str()
        .filter(|s| webhook_key(s).is_some())
        .ok_or_else(|| Error::internal("Payroof webhook response missing a valid secret"))?
        .to_owned();
    let mut current = app.config_mut().await;
    let mut updated = current.clone();
    updated.payroof.webhook_secret = secret;
    db::save_settings(&app.db, &updated).await?;
    *current = updated;
    Ok(json!({
        "id": created["id"],
        "url": webhook_url,
        "events": created["events"],
        "notice": "Webhook registered; its signing secret was saved to the settings.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_known_vectors() {
        assert_eq!(base64url("").unwrap(), b"");
        assert_eq!(base64url("Zg").unwrap(), b"f");
        assert_eq!(base64url("Zm8").unwrap(), b"fo");
        assert_eq!(base64url("Zm9v").unwrap(), b"foo");
        assert_eq!(base64url("-_8").unwrap(), [0xfb, 0xff]);
        assert!(base64url("Zh").is_none(), "non-canonical trailing bits");
        assert!(base64url("a+b/").is_none(), "standard alphabet rejected");
        let key = webhook_key("whsec_aXM6kkfVj6PFUBgwA3JVX9I18Rgp-ziMIuRMtjfwEhA").unwrap();
        assert_eq!(key.len(), 32);
        assert!(webhook_key("whsec_short").is_none());
        assert!(webhook_key("aXM6kkfVj6PFUBgwA3JVX9I18Rgp-ziMIuRMtjfwEhA").is_none());
    }

    #[test]
    fn signature_verification_matches_reference_algorithm() {
        let secret = "whsec_aXM6kkfVj6PFUBgwA3JVX9I18Rgp-ziMIuRMtjfwEhA";
        let body = br#"{"id":"evt_1"}"#;
        let key = webhook_key(secret).unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(&key).unwrap();
        mac.update(b"1793005112.");
        mac.update(body);
        let sig = format!("v1={}", hex::encode(mac.finalize().into_bytes()));
        verify(secret, "1793005112", &sig, body, 1_793_005_112).unwrap();
        verify(secret, "1793005112", &sig, body, 1_793_005_112 + 299).unwrap();
        assert!(verify(secret, "1793005112", &sig, body, 1_793_005_112 + 301).is_err());
        assert!(verify(secret, "1793005112", &sig, b"{}", 1_793_005_112).is_err());
        assert!(verify(secret, "1793005113", &sig, body, 1_793_005_112).is_err());
        assert!(verify("", "1793005112", &sig, body, 1_793_005_112).is_err());
    }

    #[test]
    fn quotes_round_up_and_display() {
        // $20 in a 6-decimal stablecoin and an 18-decimal one (BSC USDC).
        assert_eq!(stable_amount(20_000_000, 6), Some(20_000_000));
        assert_eq!(stable_amount(20_000_000, 18), Some(20 * 10u128.pow(18)));
        assert_eq!(stable_amount(1, 0), Some(1), "rounds up");
        // $20 of ETH at $3000 = 0.0066666… rounded up to 8 places.
        let eth = native_amount(20_000_000, Decimal::from(3000), 18).unwrap();
        assert_eq!(eth, 666_667 * 10u128.pow(10));
        assert_eq!(display_units(&eth.to_string(), 18), "0.00666667");
        assert_eq!(display_units("1500000", 6), "1.5");
        assert_eq!(display_units("20000000", 6), "20");
        assert_eq!(display_units("5", 6), "0.000005");
        assert_eq!(display_units("0", 6), "0");
    }

    #[test]
    fn transfer_values_are_proportional_and_capped() {
        assert_eq!(value_micro(20_000_000, 20_000_000, 20_000_000), 20_000_000);
        assert_eq!(value_micro(10_000_000, 20_000_000, 20_000_000), 10_000_000);
        assert_eq!(value_micro(u128::MAX, 1, 20_000_000), 20_000_000_000);
        assert_eq!(value_micro(5, 0, 20_000_000), 0);
    }
}
