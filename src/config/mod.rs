//! Runtime configuration.
//!
//! On first start the TOML file and bootstrap environment variables seed the
//! `settings` row in SQLite; afterwards the database copy is authoritative and
//! is edited through the administrator API (or replaced with
//! `reseller config import`).
mod branding;
mod payments;

pub use branding::{Branding, Link};
pub use payments::{Payroof, RateSource, Stripe};

use crate::keys::Limits;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// Minimum length of the administrator token and the key pepper.
pub const MIN_SECRET_LEN: usize = 24;
/// JSON pointers of every secret setting (redacted in API responses; a blank
/// value in a settings patch keeps the stored secret).
pub const SECRET_PATHS: [&str; 10] = [
    "/admin_token",
    "/key_pepper",
    "/llm/api_key",
    "/tts/api_key",
    "/stt/api_key",
    "/image/api_key",
    "/stripe/secret_key",
    "/stripe/webhook_secret",
    "/payroof/token",
    "/payroof/webhook_secret",
];

/// One OpenAI-compatible upstream endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Upstream {
    pub base_url: String,
    pub api_key: String,
    /// Model applied according to [`Config::model_policy`].
    pub model: String,
    /// Voice applied to TTS requests according to [`Config::voice_policy`].
    pub voice: String,
    /// Ordered models tried after retryable upstream failures (JSON only).
    pub fallbacks: Vec<String>,
    /// Per-upstream timeout in seconds; 0 uses `upstream_timeout_seconds`.
    pub timeout_seconds: u64,
    /// Extra request headers sent to this upstream (for example
    /// `HTTP-Referer` / `X-Title` for OpenRouter attribution).
    pub headers: BTreeMap<String, String>,
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            model: String::new(),
            voice: String::new(),
            fallbacks: Vec::new(),
            timeout_seconds: 0,
            headers: BTreeMap::new(),
        }
    }
}

/// Request headers that upstream `headers` may not override.
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

impl Upstream {
    fn validate(&self, name: &str) -> anyhow::Result<()> {
        let url = url::Url::parse(&self.base_url)
            .with_context(|| format!("[{name}].base_url is not a valid URL"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "[{name}].base_url must be an http(s) URL without credentials, query or fragment"
        );
        anyhow::ensure!(
            self.fallbacks.len() <= 10,
            "[{name}] allows at most 10 fallback models"
        );
        anyhow::ensure!(
            self.timeout_seconds <= 3600,
            "[{name}].timeout_seconds must be at most 3600"
        );
        anyhow::ensure!(
            self.headers.len() <= 20,
            "[{name}].headers allows at most 20 entries"
        );
        for (header, value) in &self.headers {
            let parsed = header
                .parse::<axum::http::HeaderName>()
                .with_context(|| format!("[{name}].headers: invalid header name {header:?}"))?;
            anyhow::ensure!(
                !RESERVED_HEADERS.contains(&parsed.as_str()),
                "[{name}].headers may not set {header}"
            );
            value
                .parse::<axum::http::HeaderValue>()
                .with_context(|| format!("[{name}].headers.{header} is not a valid value"))?;
        }
        Ok(())
    }
}

/// How a configured value (model or voice) is applied to client requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Fill the value only when the client omits it.
    #[default]
    Default,
    /// Always overwrite the client's value.
    Force,
    /// Never modify the client's value.
    Passthrough,
}

/// Upstream price: USD per million input/output tokens, per audio second,
/// and per generated image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Price {
    pub input: String,
    pub output: String,
    pub audio_second: String,
    pub image: String,
}

impl Default for Price {
    fn default() -> Self {
        Self {
            input: "0.5".into(),
            output: "1.5".into(),
            audio_second: "0".into(),
            image: "0".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Externally reachable origin for links, redirects, and webhooks.
    pub public_url: String,
    pub branding: Branding,
    pub admin_token: String,
    pub key_pepper: String,
    pub llm: Upstream,
    pub tts: Upstream,
    pub stt: Upstream,
    pub image: Upstream,
    pub model_policy: Policy,
    pub voice_policy: Policy,
    pub markup_pct: String,
    pub free_credit_usd: String,
    /// Days the signup credit stays spendable; 0 means it never expires.
    pub free_credit_expiry_days: u32,
    /// Price table keyed by model name (case-insensitive) plus `default`.
    pub prices: BTreeMap<String, Price>,
    pub require_key: bool,
    pub accept_any_token: bool,
    /// Self-service keys per source IP per calendar month; 0 disables the guard.
    pub ip_limit_per_month: u32,
    pub cooldown_seconds: u32,
    pub global_hourly_limit: u32,
    pub max_keys_per_account: u32,
    /// Per-IP replacements for `ip_limit_per_month`; 0 means unlimited.
    pub ip_overrides: BTreeMap<String, u32>,
    pub default_limits: Limits,
    pub trust_proxy: bool,
    pub cors_origin: String,
    pub max_json_body_bytes: usize,
    pub max_stream_body_bytes: usize,
    pub upstream_timeout_seconds: u64,
    pub log_retention_days: u32,
    pub max_log_entries: u32,
    pub stripe: Stripe,
    pub payroof: Payroof,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            public_url: "http://localhost:56787".into(),
            branding: Branding::default(),
            admin_token: String::new(),
            key_pepper: String::new(),
            llm: Upstream::default(),
            tts: Upstream::default(),
            stt: Upstream::default(),
            image: Upstream::default(),
            model_policy: Policy::Default,
            voice_policy: Policy::Default,
            markup_pct: "30".into(),
            free_credit_usd: "0.25".into(),
            free_credit_expiry_days: 30,
            prices: BTreeMap::from([("default".into(), Price::default())]),
            require_key: true,
            accept_any_token: false,
            ip_limit_per_month: 2,
            cooldown_seconds: 30,
            global_hourly_limit: 100,
            max_keys_per_account: 10,
            ip_overrides: BTreeMap::new(),
            default_limits: Limits::default(),
            trust_proxy: false,
            cors_origin: String::new(),
            max_json_body_bytes: 50 * 1024 * 1024,
            max_stream_body_bytes: 512 * 1024 * 1024,
            upstream_timeout_seconds: 300,
            log_retention_days: 30,
            max_log_entries: 5000,
            stripe: Stripe::default(),
            payroof: Payroof::default(),
        }
    }
}

/// Splits a comma-separated list, dropping blanks.
fn list(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect()
}

impl Config {
    /// Reads the TOML file (if present) and applies bootstrap environment
    /// variables on top of it.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut config = if path.exists() {
            Self::from_toml_file(path)?
        } else {
            Self::default()
        };
        config.apply_env(|name| std::env::var(name).ok());
        Ok(config)
    }

    /// Parses a TOML settings file without applying the environment.
    pub fn from_toml_file(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Applies bootstrap variables obtained from `var` (injectable for tests).
    pub fn apply_env(&mut self, var: impl Fn(&str) -> Option<String>) {
        for (prefix, upstream) in [
            ("OPENAI", &mut self.llm),
            ("TTS", &mut self.tts),
            ("STT", &mut self.stt),
            ("IMG", &mut self.image),
        ] {
            if let Some(v) = var(&format!("{prefix}_BASE_URL")) {
                upstream.base_url = v;
            }
            if let Some(v) = var(&format!("{prefix}_API_KEY")) {
                upstream.api_key = v;
            }
            if let Some(v) = var(&format!("{prefix}_MODEL")) {
                upstream.model = v;
            }
            if let Some(v) = var(&format!("{prefix}_MODEL_FALLBACKS")) {
                upstream.fallbacks = list(&v);
            }
        }
        if let Some(v) = var("TTS_VOICE") {
            self.tts.voice = v;
        }
        for (name, field) in [
            ("ADMIN_TOKEN", &mut self.admin_token),
            ("KEY_PEPPER", &mut self.key_pepper),
            ("MARKUP_PCT", &mut self.markup_pct),
            ("FREE_CREDIT_USD", &mut self.free_credit_usd),
            ("PUBLIC_URL", &mut self.public_url),
            ("BRAND_NAME", &mut self.branding.name),
            ("STRIPE_SECRET_KEY", &mut self.stripe.secret_key),
            ("STRIPE_WEBHOOK_SECRET", &mut self.stripe.webhook_secret),
            ("PAYROOF_API_URL", &mut self.payroof.api_url),
            ("PAYROOF_PROJECT", &mut self.payroof.project),
            ("PAYROOF_TOKEN", &mut self.payroof.token),
            ("PAYROOF_WEBHOOK_SECRET", &mut self.payroof.webhook_secret),
        ] {
            if let Some(v) = var(name) {
                *field = v;
            }
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.admin_token.len() >= MIN_SECRET_LEN && self.key_pepper.len() >= MIN_SECRET_LEN,
            "admin_token and key_pepper must be at least {MIN_SECRET_LEN} characters"
        );
        for (name, upstream) in self.upstreams() {
            upstream.validate(name)?;
        }
        self.branding.validate()?;
        crate::money::decimal(&self.markup_pct).context("markup_pct")?;
        crate::money::usd(&self.free_credit_usd).context("free_credit_usd")?;
        for (model, price) in &self.prices {
            for v in [
                &price.input,
                &price.output,
                &price.audio_second,
                &price.image,
            ] {
                crate::money::decimal(v).with_context(|| format!("prices.{model}"))?;
            }
        }
        self.default_limits.validate().context("default_limits")?;
        anyhow::ensure!(
            self.max_keys_per_account > 0
                && self.max_json_body_bytes > 0
                && self.max_stream_body_bytes > 0
                && self.upstream_timeout_seconds > 0,
            "max_keys_per_account, body limits and upstream_timeout_seconds must be positive"
        );
        anyhow::ensure!(
            self.max_json_body_bytes <= 512 * 1024 * 1024
                && self.max_stream_body_bytes <= 2 * 1024 * 1024 * 1024,
            "body limits exceed the supported maximum (512 MiB JSON, 2 GiB streamed)"
        );
        anyhow::ensure!(
            self.log_retention_days > 0 && self.max_log_entries > 0,
            "log_retention_days and max_log_entries must be positive"
        );
        anyhow::ensure!(
            self.free_credit_expiry_days <= 3650,
            "free_credit_expiry_days must be at most 3650"
        );
        let public = url::Url::parse(&self.public_url).context("public_url")?;
        anyhow::ensure!(
            matches!(public.scheme(), "http" | "https")
                && public.host_str().is_some()
                && public.query().is_none()
                && public.fragment().is_none(),
            "public_url must be an http(s) URL without query or fragment"
        );
        if !self.cors_origin.is_empty() {
            self.cors_origin
                .parse::<axum::http::HeaderValue>()
                .context("cors_origin")?;
        }
        self.payroof.validate()?;
        Ok(())
    }

    /// Every upstream section with its name.
    pub fn upstreams(&self) -> [(&'static str, &Upstream); 4] {
        [
            ("llm", &self.llm),
            ("tts", &self.tts),
            ("stt", &self.stt),
            ("image", &self.image),
        ]
    }

    /// Public origin without a trailing slash, for building absolute links.
    pub fn public_base(&self) -> &str {
        self.public_url.trim_end_matches('/')
    }

    /// Price entry for `model` (case-insensitive), falling back to `default`.
    pub fn price(&self, model: &str) -> Option<&Price> {
        self.prices
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(model))
            .or_else(|| self.prices.get_key_value("default"))
            .map(|(_, price)| price)
    }

    /// JSON view with every secret replaced by an empty string.
    pub fn redacted(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).expect("Config is always serializable");
        for path in SECRET_PATHS {
            if let Some(secret) = v.pointer_mut(path) {
                *secret = "".into();
            }
        }
        v
    }

    /// TOML rendering, optionally with secrets blanked.
    pub fn to_toml(&self, include_secrets: bool) -> anyhow::Result<String> {
        if include_secrets {
            return Ok(toml::to_string_pretty(self)?);
        }
        let redacted: Self = serde_json::from_value(self.redacted())?;
        Ok(toml::to_string_pretty(&redacted)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_lookup_is_case_insensitive_with_default_fallback() {
        let mut config = Config::default();
        config.prices.insert(
            "GPT-4o".into(),
            Price {
                input: "2.5".into(),
                ..Price::default()
            },
        );
        assert_eq!(config.price("gpt-4o").unwrap().input, "2.5");
        assert_eq!(config.price("other").unwrap().input, "0.5");
        config.prices.remove("default");
        assert!(config.price("other").is_none());
    }

    #[test]
    fn env_overrides_apply_to_each_upstream() {
        let mut config = Config::default();
        config.apply_env(|name| match name {
            "STT_MODEL" => Some("whisper-1".into()),
            "OPENAI_MODEL_FALLBACKS" => Some(" a, ,b ".into()),
            "TTS_VOICE" => Some("alloy".into()),
            "IMG_MODEL" => Some("meta/muse-image".into()),
            "PAYROOF_PROJECT" => Some("my-store".into()),
            "STRIPE_SECRET_KEY" => Some("sk_test".into()),
            _ => None,
        });
        assert_eq!(config.stt.model, "whisper-1");
        assert_eq!(config.llm.fallbacks, ["a", "b"]);
        assert_eq!(config.tts.voice, "alloy");
        assert_eq!(config.image.model, "meta/muse-image");
        assert_eq!(config.payroof.project, "my-store");
        assert_eq!(config.stripe.secret_key, "sk_test");
        assert!(config.llm.model.is_empty());
    }

    #[test]
    fn short_secrets_are_rejected() {
        let config = Config {
            admin_token: "short".into(),
            key_pepper: "p".repeat(MIN_SECRET_LEN),
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn redaction_and_toml_round_trip() {
        let mut config = Config {
            admin_token: "a".repeat(MIN_SECRET_LEN),
            key_pepper: "p".repeat(MIN_SECRET_LEN),
            ..Config::default()
        };
        config.image.api_key = "img-secret".into();
        config.payroof.token = "prf_secret".into();
        config
            .llm
            .headers
            .insert("X-Title".into(), "Reseller".into());
        config.validate().unwrap();
        let redacted = config.redacted();
        for path in SECRET_PATHS {
            assert_eq!(redacted.pointer(path).unwrap(), "", "{path}");
        }
        let full: Config = toml::from_str(&config.to_toml(true).unwrap()).unwrap();
        assert_eq!(full, config);
        let safe = config.to_toml(false).unwrap();
        assert!(!safe.contains("img-secret") && !safe.contains("prf_secret"));
    }

    #[test]
    fn reserved_upstream_headers_are_rejected() {
        let mut config = Config {
            admin_token: "a".repeat(MIN_SECRET_LEN),
            key_pepper: "p".repeat(MIN_SECRET_LEN),
            ..Config::default()
        };
        config
            .image
            .headers
            .insert("Authorization".into(), "Bearer x".into());
        assert!(config.validate().is_err());
    }
}
