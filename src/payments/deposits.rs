//! Standing Payroof deposit addresses.
//!
//! Each account has one Payroof reference, `acct-<account id>`. It is
//! allocated the first time top-up information is needed and its addresses
//! are stored per chain. A confirmed transfer of a configured stablecoin to
//! one of them credits the account its face value, once per transfer.
//! Other assets are recorded with a note and not credited.
use super::{
    chains,
    payroof::{Chain, Event, call, catalog, display_units, project_path},
};
use crate::{
    App, billing,
    config::{Config, Payroof},
    db::{self, now},
    error::{Error, Result},
};
use axum::http::Method;
use serde_json::{Value, json};
use sqlx::{Row, SqliteConnection};
use std::time::Duration;

/// Prefix of the Payroof reference of an account.
const REFERENCE_PREFIX: &str = "acct-";
/// Longest wait for Payroof while an error message is being written.
const QUOTA_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const MICRO_PER_USD: u128 = 1_000_000;

/// The account id inside a deposit reference.
pub(super) fn account_of(reference: &str) -> Option<&str> {
    reference
        .strip_prefix(REFERENCE_PREFIX)
        .filter(|id| !id.is_empty())
}

/// Whether `account` exists.
pub(super) async fn account_exists(conn: &mut SqliteConnection, account: &str) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id=?)")
            .bind(account)
            .fetch_one(conn)
            .await?,
    )
}

/// One stablecoin an account can send.
struct Asset {
    chain: String,
    chain_name: String,
    /// CAIP-19 asset id.
    id: String,
    symbol: String,
    name: String,
    decimals: u32,
    confirmations: u64,
}

/// What a customer needs to top up by deposit.
struct Info {
    assets: Vec<Asset>,
    /// `(chain, address)` for every chain that has an asset.
    addresses: Vec<(String, String)>,
    /// The whole project catalog, to name assets of recent transfers.
    catalog: std::sync::Arc<Vec<Chain>>,
}

fn is_stablecoin(settings: &Payroof, symbol: &str) -> bool {
    settings
        .stablecoins
        .iter()
        .any(|s| s.eq_ignore_ascii_case(symbol))
}

/// The account's stored deposit addresses, allocating the reference first
/// when there are none. Allocation is idempotent at Payroof.
async fn addresses(app: &App, settings: &Payroof, account: &str) -> Result<Vec<(String, String)>> {
    let stored = || async {
        sqlx::query_as::<_, (String, String)>(
            "SELECT chain,address FROM deposit_addresses WHERE account_id=? ORDER BY chain",
        )
        .bind(account)
        .fetch_all(&app.db)
        .await
    };
    let existing = stored().await?;
    if !existing.is_empty() {
        return Ok(existing);
    }
    let reference = call(
        app,
        settings,
        Method::PUT,
        &format!(
            "{}/references/{REFERENCE_PREFIX}{account}",
            project_path(settings)
        ),
        None,
    )
    .await?;
    let mut tx = db::write(&app.db).await?;
    for destination in reference["destinations"].as_array().into_iter().flatten() {
        let (Some(chain), Some(address)) = (
            destination["chain"].as_str(),
            destination["address"].as_str(),
        ) else {
            continue;
        };
        sqlx::query(
            "INSERT INTO deposit_addresses(account_id,chain,address,created_at) VALUES(?,?,?,?) \
             ON CONFLICT DO NOTHING",
        )
        .bind(account)
        .bind(chain)
        .bind(address)
        .bind(now())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(stored().await?)
}

/// Loads the stablecoins and addresses of `account`. The caller checks that
/// deposits are enabled.
async fn info(app: &App, settings: &Payroof, account: &str) -> Result<Info> {
    let chains = catalog(app, settings).await?;
    let destinations = addresses(app, settings, account).await?;
    let mut assets = Vec::new();
    let mut addresses = Vec::new();
    for chain in chains.iter().filter(|c| settings.allows_chain(&c.id)) {
        let Some((_, address)) = destinations.iter().find(|(id, _)| *id == chain.id) else {
            continue;
        };
        let found = chain
            .assets
            .iter()
            .filter(|a| !a.is_native() && is_stablecoin(settings, &a.symbol))
            .map(|a| Asset {
                chain: chain.id.clone(),
                chain_name: chain.name.clone(),
                id: a.id.clone(),
                symbol: a.symbol.clone(),
                name: a.name.clone(),
                decimals: a.decimals,
                confirmations: chain.confirmations,
            });
        let before = assets.len();
        assets.extend(found);
        if assets.len() > before {
            addresses.push((chain.id.clone(), address.clone()));
        }
    }
    Ok(Info {
        assets,
        addresses,
        catalog: chains,
    })
}

/// The newest transfers to the account's deposit addresses, as customers see
/// them. `state` is `confirming`, `credited`, or `not_credited` (with `note`).
async fn recent(app: &App, info: &Info, account: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT id,chain,asset,tx_hash,amount,status,credited_micro,note,seen_at,confirmed_at \
         FROM deposit_transfers WHERE account_id=? ORDER BY seen_at DESC,id DESC LIMIT 20",
    )
    .bind(account)
    .fetch_all(&app.db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let chain: String = r.get("chain");
            let asset: String = r.get("asset");
            let amount: String = r.get("amount");
            let found = info.catalog.iter().find(|c| c.id == chain);
            let known = found.and_then(|c| c.assets.iter().find(|a| a.id == asset));
            let credited: i64 = r.get("credited_micro");
            let status: String = r.get("status");
            let state = match (status.as_str(), credited) {
                ("confirmed", 1..) => "credited",
                ("confirmed", _) => "not_credited",
                _ => "confirming",
            };
            json!({
                "id": r.get::<String, _>("id"),
                "chain": chain,
                "chain_name": found.map(|c| c.name.as_str()),
                "symbol": known.map(|a| a.symbol.as_str()),
                "amount": known.map(|a| display_units(&amount, a.decimals)),
                "transaction": r.get::<String, _>("tx_hash"),
                "explorer": chains::links(&chain),
                "status": status,
                "state": state,
                "credited_usd": crate::money::display(credited),
                "note": r.get::<String, _>("note"),
                "seen_at": r.get::<i64, _>("seen_at"),
                "confirmed_at": r.get::<Option<i64>, _>("confirmed_at"),
            })
        })
        .collect())
}

/// One network a customer can deposit on.
fn network(info: &Info, chain: &str, address: &str, eta_minutes: u32) -> Value {
    let tokens: Vec<Value> = info
        .assets
        .iter()
        .filter(|a| a.chain == chain)
        .map(|a| {
            json!({
                "symbol": a.symbol,
                "name": if a.name.is_empty() { &a.symbol } else { &a.name },
                "asset": a.id,
                "contract": chains::contract(&a.id),
                "native": false,
                "decimals": a.decimals,
                "rate_usd": "1",
                "credit": format!("1 {} = 1 USD of credit", a.symbol),
            })
        })
        .collect();
    let first = info.assets.iter().find(|a| a.chain == chain);
    json!({
        "chain": chain,
        "chain_id": chains::eip155(chain),
        "chain_name": first.map_or("", |a| a.chain_name.as_str()),
        "address": address,
        "confirmations": first.map(|a| a.confirmations),
        "eta_minutes": eta_minutes,
        "min_deposit_usd": null,
        "tokens": tokens,
        "explorer": chains::links(chain),
    })
}

/// The `top_up` object of `GET /v1/account`.
///
/// `networks` lists, per chain, the account's deposit address and the tokens
/// that are credited 1:1. Native coins and other tokens are not credited.
/// `recent` holds the latest transfers. `assets` and `addresses` repeat the
/// same data flat.
///
/// Without Payroof deposits it only carries the workspace link. When Payroof
/// cannot be reached, it adds `"available": false` and no addresses.
pub async fn top_up(app: &App, config: &Config, account: &str) -> Value {
    let settings = &config.payroof;
    let mut out = json!({
        "automatic": false,
        "eta_minutes": settings.deposit_eta_minutes,
        "assets": [],
        "addresses": [],
        "networks": [],
        "recent": [],
        "min_deposit_usd": null,
        "credited_assets": "stablecoins",
        "workspace_url": workspace_url(config),
    });
    if !settings.deposits_enabled() {
        return out;
    }
    match info(app, settings, account).await {
        Ok(info) => {
            out["automatic"] = (!info.addresses.is_empty()).into();
            out["assets"] = info
                .assets
                .iter()
                .map(|a| {
                    json!({
                        "chain": a.chain,
                        "chain_name": a.chain_name,
                        "symbol": a.symbol,
                        "confirmations": a.confirmations,
                    })
                })
                .collect();
            out["addresses"] = info
                .addresses
                .iter()
                .map(|(chain, address)| {
                    json!({
                        "chain": chain,
                        "chain_name": chain_name(&info.assets, chain),
                        "address": address,
                    })
                })
                .collect();
            out["networks"] = info
                .addresses
                .iter()
                .map(|(chain, address)| {
                    network(&info, chain, address, settings.deposit_eta_minutes)
                })
                .collect();
            match recent(app, &info, account).await {
                Ok(list) => out["recent"] = list.into(),
                Err(e) => tracing::warn!(error = %e, "recent deposits unavailable"),
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "deposit addresses unavailable");
            out["automatic"] = true.into();
            out["available"] = false.into();
        }
    }
    out
}

fn workspace_url(config: &Config) -> String {
    format!("{}/workspace/", config.public_base())
}

fn chain_name<'a>(assets: &'a [Asset], chain: &str) -> &'a str {
    assets
        .iter()
        .find(|a| a.chain == chain)
        .map_or("", |a| a.chain_name.as_str())
}

/// The 429 error of an account without balance.
pub fn quota_error(config: &Config) -> Error {
    Error::insufficient_quota(format!(
        "You exceeded your current quota. Your balance is empty. Top up at {}.",
        workspace_url(config)
    ))
}

/// Adds top-up instructions to an `insufficient_quota` error when the account
/// has deposit addresses. Any other error, and any failure to look them up
/// within a short time, leaves `error` as it is.
pub async fn explain_quota(app: &App, config: &Config, account: &str, mut error: Error) -> Error {
    let settings = &config.payroof;
    if error.code != "insufficient_quota" || !settings.deposits_enabled() {
        return error;
    }
    let loaded = tokio::time::timeout(QUOTA_LOOKUP_TIMEOUT, info(app, settings, account)).await;
    if let Ok(Ok(info)) = loaded
        && !info.addresses.is_empty()
    {
        error.message = quota_message(&info, settings.deposit_eta_minutes);
    }
    error
}

/// Joins names as `a`, `a or b`, or `a, b or c`.
fn join_or(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    }
}

/// Names in order of first appearance, without repeats.
fn unique(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in names {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

fn quota_message(info: &Info, eta_minutes: u32) -> String {
    let symbols = join_or(&unique(info.assets.iter().map(|a| a.symbol.clone())));
    let chains = join_or(&unique(info.assets.iter().map(|a| a.chain_name.clone())));
    // Payroof uses one EVM root, so the address is usually the same everywhere.
    let first = &info.addresses[0].1;
    let destination = if info
        .addresses
        .iter()
        .all(|(_, address)| address.eq_ignore_ascii_case(first))
    {
        format!("{symbols} on {chains} to {first}")
    } else {
        let places: Vec<String> = info
            .addresses
            .iter()
            .map(|(chain, address)| format!("{address} on {}", chain_name(&info.assets, chain)))
            .collect();
        format!("{symbols} to {}", join_or(&places))
    };
    let unit = if eta_minutes == 1 {
        "minute"
    } else {
        "minutes"
    };
    format!(
        "You exceeded your current quota. Your balance is empty. To top up, send {destination}. \
         Deposits are credited automatically in about {eta_minutes} {unit}. Details: GET /v1/account."
    )
}

/// Records a transfer to the deposit address of `account`. A confirmed
/// transfer of a supported stablecoin credits the account once.
///
/// `chains` is the project catalog, needed for confirmed events.
pub(super) async fn record(
    conn: &mut SqliteConnection,
    account: &str,
    event: &Event,
    settings: &Payroof,
    chains: Option<&[Chain]>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO deposit_transfers(id,account_id,chain,asset,address,tx_hash,amount,status,seen_at) \
         VALUES(?,?,?,?,?,?,?,'seen',?) ON CONFLICT(id) DO NOTHING",
    )
    .bind(&event.transfer)
    .bind(account)
    .bind(&event.chain)
    .bind(&event.asset)
    .bind(&event.address)
    .bind(&event.transaction)
    .bind(&event.amount)
    .bind(now())
    .execute(&mut *conn)
    .await?;
    if event.event != "confirmed" {
        return Ok(());
    }
    let transfer = sqlx::query("SELECT account_id,status FROM deposit_transfers WHERE id=?")
        .bind(&event.transfer)
        .fetch_one(&mut *conn)
        .await?;
    if transfer.get::<String, _>("account_id") != account {
        return Err(Error::bad("Transfer belongs to another account"));
    }
    if transfer.get::<String, _>("status") == "confirmed" {
        return Ok(());
    }
    let (credit, note, label) = value(settings, chains.unwrap_or_default(), event);
    sqlx::query(
        "UPDATE deposit_transfers SET status='confirmed',confirmed_at=?,credited_micro=?,note=?,\
         chain=?,asset=?,address=?,tx_hash=?,amount=? WHERE id=?",
    )
    .bind(now())
    .bind(credit)
    .bind(note)
    .bind(&event.chain)
    .bind(&event.asset)
    .bind(&event.address)
    .bind(&event.transaction)
    .bind(&event.amount)
    .bind(&event.transfer)
    .execute(&mut *conn)
    .await?;
    if credit > 0 {
        billing::credit(
            conn,
            account,
            credit,
            "purchase",
            None,
            &format!(
                "Crypto deposit of {label} in transaction {}",
                event.transaction
            ),
        )
        .await?;
        tracing::info!(account, transfer = %event.transfer, credited_micro = credit, "crypto deposit credited");
    } else {
        tracing::warn!(account, transfer = %event.transfer, note, "crypto deposit not credited");
    }
    Ok(())
}

/// The micro-USD to credit for a transfer, the note explaining a transfer
/// that is not credited, and a label such as `5 USDC on Base`.
fn value(settings: &Payroof, chains: &[Chain], event: &Event) -> (i64, &'static str, String) {
    let found = chains
        .iter()
        .filter(|c| c.id == event.chain && settings.allows_chain(&c.id))
        .find_map(|c| {
            c.assets
                .iter()
                .find(|a| a.id == event.asset && !a.is_native())
                .filter(|a| is_stablecoin(settings, &a.symbol))
                .map(|a| (c, a))
        });
    let Some((chain, asset)) = found else {
        return (0, "unsupported_asset", event.asset.clone());
    };
    let label = format!(
        "{} {} on {}",
        display_units(&event.amount, asset.decimals),
        asset.symbol,
        chain.name
    );
    let micro = event
        .amount
        .parse::<u128>()
        .ok()
        .and_then(|units| units.checked_mul(MICRO_PER_USD))
        .zip(10u128.checked_pow(asset.decimals))
        .and_then(|(scaled, unit)| i64::try_from(scaled / unit).ok())
        .unwrap_or(0);
    if micro == 0 {
        return (0, "invalid_amount", label);
    }
    (micro, "", label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(chain: &str, name: &str, symbol: &str) -> Asset {
        Asset {
            chain: chain.into(),
            chain_name: name.into(),
            id: format!("{chain}/erc20:0x{symbol}"),
            symbol: symbol.into(),
            name: String::new(),
            decimals: 6,
            confirmations: 1,
        }
    }

    fn info(addresses: &[(&str, &str)]) -> Info {
        Info {
            assets: vec![
                asset("eip155:8453", "Base", "USDC"),
                asset("eip155:8453", "Base", "USDT"),
                asset("eip155:137", "Polygon", "USDC"),
            ],
            addresses: addresses
                .iter()
                .map(|(c, a)| ((*c).into(), (*a).into()))
                .collect(),
            catalog: Default::default(),
        }
    }

    #[test]
    fn networks_list_tokens_with_contracts_and_explorers() {
        let n = network(
            &info(&[("eip155:8453", "0xabc")]),
            "eip155:8453",
            "0xabc",
            5,
        );
        assert_eq!(n["chain_id"], 8453);
        assert_eq!(n["chain_name"], "Base");
        assert_eq!(n["confirmations"], 1);
        assert_eq!(n["tokens"].as_array().unwrap().len(), 2);
        assert_eq!(n["tokens"][0]["contract"], "0xUSDC");
        assert_eq!(n["tokens"][0]["credit"], "1 USDC = 1 USD of credit");
        assert_eq!(
            n["explorer"]["address"],
            "https://basescan.org/address/{address}"
        );
        assert!(n["min_deposit_usd"].is_null());
    }

    #[test]
    fn references_name_the_account() {
        assert_eq!(account_of("acct-abc"), Some("abc"));
        assert_eq!(account_of("acct-"), None);
        assert_eq!(account_of("order-1"), None);
    }

    #[test]
    fn names_are_joined_with_or() {
        let names = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(join_or(&names(&[])), "");
        assert_eq!(join_or(&names(&["A"])), "A");
        assert_eq!(join_or(&names(&["A", "B"])), "A or B");
        assert_eq!(join_or(&names(&["A", "B", "C"])), "A, B or C");
    }

    #[test]
    fn quota_message_lists_symbols_chains_address_and_minutes() {
        let same = info(&[("eip155:8453", "0xabc"), ("eip155:137", "0xABC")]);
        assert_eq!(
            quota_message(&same, 5),
            "You exceeded your current quota. Your balance is empty. To top up, send USDC or USDT \
             on Base or Polygon to 0xabc. Deposits are credited automatically in about 5 minutes. \
             Details: GET /v1/account."
        );
        let apart = info(&[("eip155:8453", "0xabc"), ("eip155:137", "0xdef")]);
        assert!(
            quota_message(&apart, 1)
                .contains("send USDC or USDT to 0xabc on Base or 0xdef on Polygon. Deposits")
        );
        assert!(quota_message(&apart, 1).contains("about 1 minute."));
    }

    #[test]
    fn only_configured_stablecoins_on_allowed_chains_are_valued() {
        let chains: Vec<Chain> = serde_json::from_value(json!([
            {"id": "eip155:1", "name": "Ethereum", "assets": [
                {"id": "eip155:1/slip44:60", "symbol": "ETH", "decimals": 18},
                {"id": "eip155:1/erc20:0xa", "symbol": "USDC", "decimals": 6},
                {"id": "eip155:1/erc20:0xb", "symbol": "WETH", "decimals": 18}]},
            {"id": "eip155:56", "name": "BNB Smart Chain", "assets": [
                {"id": "eip155:56/erc20:0xc", "symbol": "USDC", "decimals": 18}]}
        ]))
        .unwrap();
        let settings = Payroof {
            chains: vec!["eip155:1".into(), "eip155:56".into()],
            ..Payroof::default()
        };
        let event = |chain: &str, asset: &str, amount: &str| Event {
            id: "e".into(),
            event: "confirmed".into(),
            transfer: "t".into(),
            project: "p".into(),
            reference: "acct-a".into(),
            chain: chain.into(),
            asset: asset.into(),
            address: "0x".into(),
            transaction: "0xtx".into(),
            amount: amount.into(),
        };
        let usdc = event("eip155:1", "eip155:1/erc20:0xa", "2500000");
        assert_eq!(value(&settings, &chains, &usdc).0, 2_500_000);
        assert_eq!(value(&settings, &chains, &usdc).2, "2.5 USDC on Ethereum");
        let bsc = event("eip155:56", "eip155:56/erc20:0xc", "3000000000000000000");
        assert_eq!(value(&settings, &chains, &bsc).0, 3_000_000);
        for unsupported in [
            event("eip155:1", "eip155:1/slip44:60", "1000000000000000000"),
            event("eip155:1", "eip155:1/erc20:0xb", "1000000000000000000"),
            event("eip155:1", "eip155:1/erc20:0xzz", "1"),
            event("eip155:10", "eip155:10/erc20:0xa", "1000000"),
        ] {
            assert_eq!(
                value(&settings, &chains, &unsupported).1,
                "unsupported_asset"
            );
        }
        let narrow = Payroof {
            chains: vec!["eip155:56".into()],
            ..settings.clone()
        };
        assert_eq!(value(&narrow, &chains, &usdc).1, "unsupported_asset");
        assert_eq!(
            value(
                &settings,
                &chains,
                &event("eip155:1", "eip155:1/erc20:0xa", "x")
            )
            .1,
            "invalid_amount"
        );
        assert_eq!(
            value(
                &settings,
                &chains,
                &event("eip155:1", "eip155:1/erc20:0xa", "0")
            )
            .1,
            "invalid_amount"
        );
    }
}
