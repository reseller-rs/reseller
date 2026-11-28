//! Upstream providers: one OpenAI-compatible connection per API key.
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

/// Most providers.
const MAX_PROVIDERS: usize = 50;
/// Longest provider id, in characters.
const MAX_ID_CHARS: usize = 32;
/// Largest accepted value of any budget field.
const MAX_BUDGET: u32 = 1_000_000_000;

/// Request headers that provider `headers` may not override.
const RESERVED_HEADERS: [&str; 9] = [
    "authorization",
    "host",
    "content-length",
    "content-type",
    "transfer-encoding",
    "connection",
    "upgrade",
    "accept-encoding",
    "te",
];

/// A usage budget of one provider key or one route. 0 means unlimited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Budget {
    /// Requests per minute (GCRA, burst of `rpm`).
    pub rpm: u32,
    /// Requests per day (GCRA). The route may use all of `rpd` at once, then
    /// the allowance refills continuously at `rpd` per 24 hours, so over any
    /// 24 hours it can take up to about twice `rpd`.
    pub rpd: u32,
    /// Tokens per minute (GCRA, charged after each request completes).
    pub tpm: u32,
    /// Tokens per day (GCRA like `rpd`, charged after each request completes).
    pub tpd: u32,
    /// Requests in flight.
    pub concurrent: u32,
}

impl Budget {
    /// Whether every limit is off.
    pub fn is_unlimited(&self) -> bool {
        *self == Self::default()
    }

    /// Checks that every value is sane.
    pub(super) fn validate(&self, what: &str) -> anyhow::Result<()> {
        for (field, value) in [
            ("rpm", self.rpm),
            ("rpd", self.rpd),
            ("tpm", self.tpm),
            ("tpd", self.tpd),
            ("concurrent", self.concurrent),
        ] {
            anyhow::ensure!(
                value <= MAX_BUDGET,
                "{what}.limits.{field} must be at most {MAX_BUDGET}"
            );
        }
        Ok(())
    }
}

/// One OpenAI-compatible upstream connection (one API key).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Provider {
    /// Unique id, 1-32 characters of `a-z`, `0-9`, `_` and `-`.
    pub id: String,
    pub base_url: String,
    /// Secret. Redacted in API responses. A blank value in a settings patch
    /// keeps the stored key of the provider with the same id.
    pub api_key: String,
    /// Disabled providers are never picked.
    pub enabled: bool,
    /// Per-request timeout in seconds. Zero uses `upstream_timeout_seconds`.
    pub timeout_seconds: u64,
    /// Extra request headers sent to this provider (for example
    /// `HTTP-Referer` / `X-Title` for OpenRouter attribution).
    pub headers: BTreeMap<String, String>,
    /// Account-wide budget of this key, shared by every route on it.
    pub limits: Budget,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            id: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            enabled: true,
            timeout_seconds: 0,
            headers: BTreeMap::new(),
            limits: Budget::default(),
        }
    }
}

impl Provider {
    /// Effective timeout, given the global `default_seconds`.
    pub fn timeout(&self, default_seconds: u64) -> Duration {
        Duration::from_secs(if self.timeout_seconds > 0 {
            self.timeout_seconds
        } else {
            default_seconds
        })
    }

    fn validate(&self) -> anyhow::Result<()> {
        let name = format!("providers.{}", self.id);
        anyhow::ensure!(
            (1..=MAX_ID_CHARS).contains(&self.id.len())
                && self.id.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'_'
                    || b == b'-'),
            "provider id {:?} must be 1-{MAX_ID_CHARS} characters of a-z, 0-9, _ and -",
            self.id
        );
        let url = url::Url::parse(&self.base_url)
            .with_context(|| format!("{name}.base_url is not a valid URL"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "{name}.base_url must be an http(s) URL without credentials, query or fragment"
        );
        anyhow::ensure!(
            self.timeout_seconds <= 3600,
            "{name}.timeout_seconds must be at most 3600"
        );
        anyhow::ensure!(
            self.headers.len() <= 20,
            "{name}.headers allows at most 20 entries"
        );
        for (header, value) in &self.headers {
            let parsed = header
                .parse::<axum::http::HeaderName>()
                .with_context(|| format!("{name}.headers: invalid header name {header:?}"))?;
            anyhow::ensure!(
                !RESERVED_HEADERS.contains(&parsed.as_str()),
                "{name}.headers may not set {header}"
            );
            value
                .parse::<axum::http::HeaderValue>()
                .with_context(|| format!("{name}.headers.{header} is not a valid value"))?;
        }
        self.limits.validate(&name)
    }
}

/// Checks the provider list: sizes, fields, and unique ids.
pub(super) fn validate(providers: &[Provider]) -> anyhow::Result<()> {
    anyhow::ensure!(
        providers.len() <= MAX_PROVIDERS,
        "at most {MAX_PROVIDERS} providers are allowed"
    );
    let mut ids = std::collections::HashSet::new();
    for provider in providers {
        provider.validate()?;
        anyhow::ensure!(
            ids.insert(provider.id.as_str()),
            "provider id {:?} is used twice",
            provider.id
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str) -> Provider {
        Provider {
            id: id.into(),
            base_url: "https://api.example.com/v1".into(),
            ..Provider::default()
        }
    }

    #[test]
    fn ids_are_bounded_and_unique() {
        assert!(validate(&[provider("groq"), provider("open_router-2")]).is_ok());
        for bad in ["", "Groq", "a b", &"x".repeat(33), "é"] {
            assert!(validate(&[provider(bad)]).is_err(), "{bad:?}");
        }
        assert!(validate(&[provider("a"), provider("a")]).is_err());
    }

    #[test]
    fn urls_headers_and_budgets_are_checked() {
        let mut p = provider("a");
        p.base_url = "ftp://x".into();
        assert!(validate(&[p]).is_err());
        let mut p = provider("a");
        p.base_url = String::new();
        assert!(validate(&[p]).is_err());
        let mut p = provider("a");
        p.headers.insert("Authorization".into(), "x".into());
        assert!(validate(&[p]).is_err());
        let mut p = provider("a");
        p.limits.rpm = MAX_BUDGET + 1;
        assert!(validate(&[p]).is_err());
        let mut p = provider("a");
        p.timeout_seconds = 3601;
        assert!(validate(&[p]).is_err());
        assert_eq!(provider("a").timeout(300), Duration::from_secs(300));
    }
}
