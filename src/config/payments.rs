//! Payment provider settings: Stripe Checkout and Payroof crypto payments.
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Stripe {
    pub secret_key: String,
    pub webhook_secret: String,
}

impl Stripe {
    pub fn enabled(&self) -> bool {
        !self.secret_key.is_empty() && !self.webhook_secret.is_empty()
    }
}

/// Where native-coin USD rates come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateSource {
    /// CoinGecko-compatible `simple/price` endpoint, with `native_rates_usd`
    /// as per-symbol overrides.
    #[default]
    Coingecko,
    /// Only `native_rates_usd`; native coins without a rate are not offered.
    Static,
}

/// Payroof watch-only crypto payments (EVM native coins and ERC-20 tokens).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Payroof {
    /// Payroof API origin.
    pub api_url: String,
    /// Payroof project name.
    pub project: String,
    /// Project bearer token (`prf_…`) with the `references` permission
    /// (and `webhooks` to register the webhook from the console).
    pub token: String,
    /// Webhook signing secret (`whsec_…`).
    pub webhook_secret: String,
    /// Token symbols valued 1:1 with USD (for example USDC, USDT).
    pub stablecoins: Vec<String>,
    /// Offer native coins (ETH, POL, BNB, …) at the current USD rate.
    pub native_coins: bool,
    /// Restrict payments to these CAIP-2 chain ids (empty: every chain the
    /// project watches).
    pub chains: Vec<String>,
    pub rate_source: RateSource,
    /// CoinGecko-compatible `simple/price` endpoint.
    pub rate_url: String,
    /// Native symbol -> CoinGecko coin id.
    pub rate_ids: BTreeMap<String, String>,
    /// Fixed USD rates by native symbol (decimal strings); override the feed.
    pub native_rates_usd: BTreeMap<String, String>,
    /// How long a quote (addresses and amounts) is valid.
    pub quote_ttl_minutes: u32,
    /// Accepted underpayment, in percent of the order price.
    pub underpayment_tolerance_pct: String,
}

impl Default for Payroof {
    fn default() -> Self {
        Self {
            api_url: "https://api.payroof.top".into(),
            project: String::new(),
            token: String::new(),
            webhook_secret: String::new(),
            stablecoins: vec!["USDC".into(), "USDT".into()],
            native_coins: true,
            chains: Vec::new(),
            rate_source: RateSource::Coingecko,
            rate_url: "https://api.coingecko.com/api/v3/simple/price".into(),
            rate_ids: BTreeMap::from(
                [
                    ("ETH", "ethereum"),
                    ("POL", "polygon-ecosystem-token"),
                    ("MATIC", "polygon-ecosystem-token"),
                    ("BNB", "binancecoin"),
                    ("AVAX", "avalanche-2"),
                    ("XDAI", "xdai"),
                    ("FTM", "fantom"),
                    ("S", "sonic-3"),
                    ("CELO", "celo"),
                ]
                .map(|(k, v)| (k.to_owned(), v.to_owned())),
            ),
            native_rates_usd: BTreeMap::new(),
            quote_ttl_minutes: 60,
            underpayment_tolerance_pct: "0.5".into(),
        }
    }
}

impl Payroof {
    pub fn enabled(&self) -> bool {
        !self.project.is_empty() && !self.token.is_empty() && !self.webhook_secret.is_empty()
    }

    /// API origin without a trailing slash.
    pub fn api_base(&self) -> &str {
        self.api_url.trim_end_matches('/')
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for (name, value) in [("api_url", &self.api_url), ("rate_url", &self.rate_url)] {
            let url = url::Url::parse(value).with_context(|| format!("payroof.{name}"))?;
            anyhow::ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "payroof.{name} must be an http(s) URL without credentials, query or fragment"
            );
        }
        anyhow::ensure!(
            self.project.len() <= 63
                && self
                    .project
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "payroof.project must be 1-63 lowercase letters, digits or hyphens"
        );
        anyhow::ensure!(
            self.token.is_empty() || self.token.starts_with("prf_"),
            "payroof.token must be a project token (prf_…)"
        );
        anyhow::ensure!(
            self.webhook_secret.is_empty()
                || crate::payments::payroof::webhook_key(&self.webhook_secret).is_some(),
            "payroof.webhook_secret must be a whsec_ secret encoding 32 bytes"
        );
        anyhow::ensure!(
            (1..=1440).contains(&self.quote_ttl_minutes),
            "payroof.quote_ttl_minutes must be 1-1440"
        );
        let tolerance = crate::money::decimal(&self.underpayment_tolerance_pct)
            .context("payroof.underpayment_tolerance_pct")?;
        anyhow::ensure!(
            tolerance < rust_decimal::Decimal::from(50),
            "payroof.underpayment_tolerance_pct must be below 50"
        );
        anyhow::ensure!(
            self.stablecoins.len() <= 20 && self.chains.len() <= 100,
            "payroof.stablecoins/chains lists are too long"
        );
        for (symbol, rate) in &self.native_rates_usd {
            let rate = crate::money::decimal(rate)
                .with_context(|| format!("payroof.native_rates_usd.{symbol}"))?;
            anyhow::ensure!(
                !rate.is_zero(),
                "payroof.native_rates_usd.{symbol} must be positive"
            );
        }
        Ok(())
    }
}
