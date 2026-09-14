use crate::identity::Limits;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Upstream {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub voice: String,
    pub fallbacks: Vec<String>,
}
impl Default for Upstream {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            model: String::new(),
            voice: String::new(),
            fallbacks: vec![],
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    Default,
    Force,
    Passthrough,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Price {
    pub input: String,
    pub output: String,
    pub audio_second: String,
}
impl Default for Price {
    fn default() -> Self {
        Self {
            input: "0.5".into(),
            output: "1.5".into(),
            audio_second: "0".into(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub site_name: String,
    pub admin_token: String,
    pub key_pepper: String,
    pub llm: Upstream,
    pub tts: Upstream,
    pub stt: Upstream,
    pub model_policy: Policy,
    pub voice_policy: Policy,
    pub markup_pct: String,
    pub free_credit_usd: String,
    pub free_credit_expiry_days: u32,
    pub prices: BTreeMap<String, Price>,
    pub require_key: bool,
    pub accept_any_token: bool,
    pub ip_limit_per_month: u32,
    pub cooldown_seconds: u32,
    pub global_hourly_limit: u32,
    pub max_keys_per_account: u32,
    pub ip_overrides: BTreeMap<String, i32>,
    pub default_limits: Limits,
    pub trust_proxy: bool,
    pub cors_origin: String,
    pub max_json_body_bytes: usize,
    pub max_stream_body_bytes: usize,
    pub upstream_timeout_seconds: u64,
    pub log_retention_days: u32,
    pub max_log_entries: u32,
    pub public_url: String,
    pub stripe_secret_key: String,
    pub stripe_webhook_secret: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            site_name: "Reseller".into(),
            admin_token: String::new(),
            key_pepper: String::new(),
            llm: Upstream::default(),
            tts: Upstream::default(),
            stt: Upstream::default(),
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
            public_url: "http://localhost:56787".into(),
            stripe_secret_key: String::new(),
            stripe_webhook_secret: String::new(),
        }
    }
}
impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut c: Self = if path.exists() {
            toml::from_str(&std::fs::read_to_string(path)?)?
        } else {
            Self::default()
        };
        for (prefix, u) in [
            ("OPENAI", &mut c.llm),
            ("TTS", &mut c.tts),
            ("STT", &mut c.stt),
        ] {
            if let Ok(v) = std::env::var(format!("{prefix}_BASE_URL")) {
                u.base_url = v;
            }
            if let Ok(v) = std::env::var(format!("{prefix}_API_KEY")) {
                u.api_key = v;
            }
            if let Ok(v) = std::env::var(format!("{prefix}_MODEL")) {
                u.model = v;
            }
            if let Ok(v) = std::env::var(format!("{prefix}_MODEL_FALLBACKS")) {
                u.fallbacks = v
                    .split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            if prefix == "TTS"
                && let Ok(v) = std::env::var(format!("{prefix}_VOICE"))
            {
                u.voice = v;
            }
        }
        for (name, field) in [
            ("ADMIN_TOKEN", &mut c.admin_token),
            ("KEY_PEPPER", &mut c.key_pepper),
            ("MARKUP_PCT", &mut c.markup_pct),
            ("FREE_CREDIT_USD", &mut c.free_credit_usd),
            ("PUBLIC_URL", &mut c.public_url),
            ("STRIPE_SECRET_KEY", &mut c.stripe_secret_key),
            ("STRIPE_WEBHOOK_SECRET", &mut c.stripe_webhook_secret),
        ] {
            if let Ok(v) = std::env::var(name) {
                *field = v;
            }
        }
        Ok(c)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        for u in [&self.llm, &self.tts, &self.stt] {
            let url = url::Url::parse(&u.base_url)?;
            anyhow::ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "upstream must be an HTTP(S) base URL without credentials, query or fragment"
            );
            anyhow::ensure!(u.fallbacks.len() <= 10, "at most 10 fallback models");
        }
        crate::money::decimal(&self.markup_pct)?;
        crate::money::usd(&self.free_credit_usd)?;
        for p in self.prices.values() {
            for v in [&p.input, &p.output, &p.audio_second] {
                crate::money::decimal(v)?;
            }
        }
        self.default_limits.validate()?;
        anyhow::ensure!(
            self.max_keys_per_account > 0
                && self.max_json_body_bytes > 0
                && self.max_stream_body_bytes > 0
                && self.upstream_timeout_seconds > 0,
            "limits and timeout must be positive"
        );
        anyhow::ensure!(
            self.max_json_body_bytes <= 512 * 1024 * 1024
                && self.max_stream_body_bytes <= 2 * 1024 * 1024 * 1024usize,
            "body limits exceed supported maximum"
        );
        anyhow::ensure!(
            self.log_retention_days > 0
                && self.max_log_entries > 0
                && self.free_credit_expiry_days <= 3650,
            "invalid retention or credit expiry"
        );
        let public = url::Url::parse(&self.public_url)?;
        anyhow::ensure!(
            matches!(public.scheme(), "http" | "https")
                && public.host_str().is_some()
                && public.query().is_none()
                && public.fragment().is_none(),
            "invalid public_url"
        );
        if !self.cors_origin.is_empty() {
            self.cors_origin.parse::<axum::http::HeaderValue>()?;
        }
        Ok(())
    }
    pub fn redacted(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).expect("serializable config");
        for k in [
            "admin_token",
            "key_pepper",
            "stripe_secret_key",
            "stripe_webhook_secret",
        ] {
            v[k] = "".into();
        }
        for k in ["llm", "tts", "stt"] {
            v[k]["api_key"] = "".into();
        }
        v
    }
}
