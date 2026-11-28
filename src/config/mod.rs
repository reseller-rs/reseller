//! Runtime configuration.
//!
//! On first start, the TOML file and bootstrap environment variables seed the
//! `settings` row in SQLite. After that the database copy is authoritative.
//!
//! Change it in one of two ways:
//!
//! - Edit it through the administrator API.
//! - Replace it with `reseller config import`.
mod billing;
mod branding;
mod free;
mod interpolate;
mod keys;
mod mcp;
mod models;
mod payments;
mod providers;
mod rate_limits;
mod reasoning;
mod routing;
mod signup;
mod stores;

pub use billing::Billing;
pub use branding::{Branding, Link};
pub use free::Free;
pub use keys::KeyPolicy;
pub use mcp::Mcp;
pub use models::{Capability, Model, Resolved, Route, Strategy, Tier};
pub use payments::{Payroof, RateSource, Stripe};
pub use providers::{Budget, Provider};
pub use rate_limits::RateLimits;
pub use reasoning::{EFFORT_LEVELS, Reasoning, ReasoningModel, effort_rank};
pub use routing::Routing;
pub use signup::Signup;
pub use stores::{Apple, AppleEnvironment, Google};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// Minimum length of the administrator token and the key pepper.
pub const MIN_SECRET_LEN: usize = 24;
/// JSON pointers of every secret setting.
///
/// API responses redact these values. A blank value in a settings patch keeps
/// the stored secret. The API keys of `providers` are secrets as well. They
/// sit in a list, so [`Config::redacted`] and [`Config::keep_provider_keys`]
/// handle them by provider id.
pub const SECRET_PATHS: [&str; 8] = [
    "/admin_token",
    "/key_pepper",
    "/stripe/secret_key",
    "/stripe/webhook_secret",
    "/payroof/token",
    "/payroof/webhook_secret",
    "/apple/private_key",
    "/google/service_account_json",
];

/// The source of the client address used for per-IP limits (free tier,
/// signup, lockouts) and request logs. Only the header that the operator's
/// own proxy sets may be trusted: any other header is controlled by clients.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientIp {
    /// The TCP peer address. Use it when clients connect directly.
    #[default]
    Peer,
    /// The last `X-Forwarded-For` entry, which the nearest proxy appended
    /// (Caddy, nginx, Traefik, a load balancer). Earlier entries come from
    /// clients and are ignored.
    XForwardedFor,
    /// `CF-Connecting-IP`, set by Cloudflare. Only behind Cloudflare, with
    /// direct access to the origin blocked.
    CfConnectingIp,
}

/// Upstream price in USD (decimal strings).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Price {
    /// Per 1M input tokens.
    pub input: String,
    /// Per 1M cached input tokens. When absent, the `input` price applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input: Option<String>,
    /// Per 1M output tokens.
    pub output: String,
    /// Per second of audio.
    pub audio_second: String,
    /// Per 1M input characters (text-to-speech).
    pub characters: String,
    /// Per generated image.
    pub image: String,
    /// Flat fee per successful (2xx) request.
    pub request: String,
}

impl Default for Price {
    fn default() -> Self {
        Self {
            input: "0.5".into(),
            cached_input: None,
            output: "1.5".into(),
            audio_second: "0".into(),
            characters: "0".into(),
            image: "0".into(),
            request: "0".into(),
        }
    }
}

impl Price {
    /// Every configured amount, for validation.
    fn amounts(&self) -> impl Iterator<Item = &String> {
        [
            Some(&self.input),
            self.cached_input.as_ref(),
            Some(&self.output),
            Some(&self.audio_second),
            Some(&self.characters),
            Some(&self.image),
            Some(&self.request),
        ]
        .into_iter()
        .flatten()
    }
}

/// Complete runtime settings, stored as one JSON document in SQLite.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Externally reachable origin for links, redirects, and webhooks.
    pub public_url: String,
    pub branding: Branding,
    pub admin_token: String,
    pub key_pepper: String,
    pub markup_pct: String,
    pub signup: Signup,
    pub keys: KeyPolicy,
    pub rate_limits: RateLimits,
    pub mcp: Mcp,
    pub routing: Routing,
    pub free: Free,
    /// Price table keyed by upstream model name (case-insensitive) plus
    /// `default`.
    pub prices: BTreeMap<String, Price>,
    /// Where the client address comes from. See [`ClientIp`].
    pub client_ip: ClientIp,
    pub cors_origin: String,
    pub max_json_body_bytes: usize,
    pub max_stream_body_bytes: usize,
    pub upstream_timeout_seconds: u64,
    pub log_retention_days: u32,
    pub max_log_entries: u32,
    pub billing: Billing,
    pub reasoning: Reasoning,
    pub stripe: Stripe,
    pub payroof: Payroof,
    pub apple: Apple,
    pub google: Google,
    /// Upstream connections (`[[providers]]`), one per API key.
    pub providers: Vec<Provider>,
    /// Public model catalog (`[[models]]`): the only way to serve a model.
    pub models: Vec<Model>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            public_url: "http://localhost:56787".into(),
            branding: Branding::default(),
            admin_token: String::new(),
            key_pepper: String::new(),
            markup_pct: "30".into(),
            signup: Signup::default(),
            keys: KeyPolicy::default(),
            rate_limits: RateLimits::default(),
            mcp: Mcp::default(),
            routing: Routing::default(),
            free: Free::default(),
            prices: BTreeMap::from([("default".into(), Price::default())]),
            client_ip: ClientIp::Peer,
            cors_origin: String::new(),
            max_json_body_bytes: 50 * 1024 * 1024,
            max_stream_body_bytes: 512 * 1024 * 1024,
            upstream_timeout_seconds: 300,
            log_retention_days: 30,
            max_log_entries: 5000,
            billing: Billing::default(),
            reasoning: Reasoning::default(),
            stripe: Stripe::default(),
            payroof: Payroof::default(),
            apple: Apple::default(),
            google: Google::default(),
            providers: Vec::new(),
            models: Vec::new(),
        }
    }
}

/// Requires an http(s) URL without credentials, query, or fragment.
fn check_http_url(name: &str, value: &str) -> anyhow::Result<()> {
    let url = url::Url::parse(value).with_context(|| format!("{name} is not a valid URL"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "{name} must be an http(s) URL without credentials, query or fragment"
    );
    Ok(())
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

    /// Parses a TOML settings file, expanding `${NAME}` references from the
    /// process environment. Bootstrap variables are not applied.
    pub fn from_toml_file(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml_str(&text, |name| std::env::var(name).ok())
            .with_context(|| format!("parsing {}", path.display()))
    }

    /// Parses TOML settings, expanding `${NAME}` and `${NAME:-fallback}`
    /// references in string values with `var` (see the `interpolate` module).
    ///
    /// A provider whose `api_key` references variables and expands to an
    /// empty key is loaded disabled, so one file can list every provider
    /// and serve those whose keys the deployment sets.
    pub fn from_toml_str(text: &str, var: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let mut table: toml::Table = toml::from_str(text)?;
        let referenced: Vec<bool> = match table.get("providers") {
            Some(toml::Value::Array(providers)) => providers
                .iter()
                .map(|p| {
                    p.get("api_key")
                        .and_then(toml::Value::as_str)
                        .is_some_and(interpolate::references)
                })
                .collect(),
            _ => Vec::new(),
        };
        interpolate::expand_table(&mut table, &var, "")?;
        let mut config: Self = toml::Value::Table(table).try_into()?;
        for (provider, referenced) in config.providers.iter_mut().zip(referenced) {
            if referenced && provider.api_key.is_empty() && provider.enabled {
                provider.enabled = false;
                tracing::warn!(
                    provider = %provider.id,
                    "provider disabled: its api_key variable is not set"
                );
            }
        }
        Ok(config)
    }

    /// Applies bootstrap variables obtained from `var` (injectable for tests).
    ///
    /// `OPENAI_*`, `TTS_*`, `STT_*` and `IMG_*` create the provider `llm`,
    /// `tts`, `stt` or `image` and one paid catalog model on it. Fallback
    /// models become further routes of that model, tried in order.
    pub fn apply_env(&mut self, var: impl Fn(&str) -> Option<String>) {
        for (prefix, id, capability) in [
            ("OPENAI", "llm", Capability::Chat),
            ("TTS", "tts", Capability::Speech),
            ("STT", "stt", Capability::Transcription),
            ("IMG", "image", Capability::Image),
        ] {
            // Empty values count as unset (compose passes `${X:-}`), and a base
            // URL alone does not define a provider.
            let set = |name: &str| var(&format!("{prefix}_{name}")).filter(|v| !v.is_empty());
            let base_url = set("BASE_URL");
            let api_key = set("API_KEY");
            let model = set("MODEL");
            if api_key.is_none() && model.is_none() {
                continue;
            }
            let provider = match self.providers.iter().position(|p| p.id == id) {
                Some(i) => &mut self.providers[i],
                None => {
                    self.providers.push(Provider {
                        id: id.into(),
                        base_url: "https://api.openai.com/v1".into(),
                        ..Provider::default()
                    });
                    self.providers.last_mut().expect("just pushed")
                }
            };
            if let Some(v) = base_url {
                provider.base_url = v;
            }
            if let Some(v) = api_key {
                provider.api_key = v;
            }
            let Some(model) = model else { continue };
            let mut routes = vec![model.clone()];
            for fallback in list(&var(&format!("{prefix}_MODEL_FALLBACKS")).unwrap_or_default()) {
                if !routes.contains(&fallback) {
                    routes.push(fallback);
                }
            }
            let voice = (capability == Capability::Speech)
                .then(|| var("TTS_VOICE"))
                .flatten();
            let routes: Vec<Route> = routes
                .into_iter()
                .map(|model| Route {
                    provider: id.into(),
                    model,
                    ..Route::default()
                })
                .collect();
            match self.models.iter_mut().find(|m| m.id == model) {
                Some(existing) => {
                    existing.routes = routes;
                    if let Some(voice) = voice {
                        existing.voice = voice;
                    }
                }
                None => {
                    // The settings file's own servable default of the
                    // capability wins.
                    let default = !self.models.iter().any(|m| {
                        m.default
                            && m.tier == Tier::Paid
                            && m.capability() == capability
                            && self.serves(m)
                    });
                    self.models.push(Model {
                        id: model,
                        capability: Some(capability),
                        default,
                        strategy: Strategy::Ordered,
                        voice: voice.unwrap_or_default(),
                        routes,
                        ..Model::default()
                    });
                }
            }
        }
        for (name, field) in [
            ("ADMIN_TOKEN", &mut self.admin_token),
            ("KEY_PEPPER", &mut self.key_pepper),
            ("MARKUP_PCT", &mut self.markup_pct),
            ("PUBLIC_URL", &mut self.public_url),
            ("BRAND_NAME", &mut self.branding.name),
            ("STRIPE_SECRET_KEY", &mut self.stripe.secret_key),
            ("STRIPE_WEBHOOK_SECRET", &mut self.stripe.webhook_secret),
            ("PAYROOF_API_URL", &mut self.payroof.api_url),
            ("PAYROOF_PROJECT", &mut self.payroof.project),
            ("PAYROOF_TOKEN", &mut self.payroof.token),
            ("PAYROOF_WEBHOOK_SECRET", &mut self.payroof.webhook_secret),
            ("APPLE_BUNDLE_ID", &mut self.apple.bundle_id),
            ("APPLE_ISSUER_ID", &mut self.apple.issuer_id),
            ("APPLE_KEY_ID", &mut self.apple.key_id),
            ("APPLE_PRIVATE_KEY", &mut self.apple.private_key),
            ("GOOGLE_PACKAGE_NAME", &mut self.google.package_name),
            (
                "GOOGLE_SERVICE_ACCOUNT_JSON",
                &mut self.google.service_account_json,
            ),
            (
                "GOOGLE_PUBSUB_SERVICE_ACCOUNT",
                &mut self.google.pubsub_service_account,
            ),
        ] {
            if let Some(v) = var(name) {
                *field = v;
            }
        }
    }

    /// Checks every setting and returns the first violation.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.admin_token.len() >= MIN_SECRET_LEN && self.key_pepper.len() >= MIN_SECRET_LEN,
            "admin_token and key_pepper must be at least {MIN_SECRET_LEN} characters"
        );
        providers::validate(&self.providers)?;
        self.branding.validate()?;
        crate::money::decimal(&self.markup_pct).context("markup_pct")?;
        self.signup.validate()?;
        self.keys.validate()?;
        self.mcp.validate()?;
        models::validate(&self.models, &self.providers)?;
        self.routing.validate()?;
        self.free.validate()?;
        for (model, price) in &self.prices {
            for v in price.amounts() {
                crate::money::decimal(v).with_context(|| format!("prices.{model}"))?;
            }
        }
        anyhow::ensure!(
            self.max_json_body_bytes > 0
                && self.max_stream_body_bytes > 0
                && self.upstream_timeout_seconds > 0,
            "body limits and upstream_timeout_seconds must be positive"
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
        self.billing.validate()?;
        self.reasoning.validate()?;
        self.rate_limits.validate()?;
        self.apple.validate()?;
        self.google.validate()?;
        Ok(())
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
        for provider in v["providers"].as_array_mut().into_iter().flatten() {
            provider["api_key"] = "".into();
        }
        v
    }

    /// Gives every provider with a blank `api_key` the stored key of the
    /// provider with the same id in `stored`. A blank key in a settings patch
    /// or an imported file means "keep the secret".
    pub fn keep_provider_keys(&mut self, stored: &Self) {
        for provider in self.providers.iter_mut().filter(|p| p.api_key.is_empty()) {
            if let Some(old) = stored.provider(&provider.id) {
                provider.api_key.clone_from(&old.api_key);
            }
        }
    }

    /// TOML rendering, optionally with secrets blanked.
    ///
    /// Every `${` in a value is written as `$${`, so reading the file back
    /// ([`Config::from_toml_str`]) restores the settings unchanged.
    pub fn to_toml(&self, include_secrets: bool) -> anyhow::Result<String> {
        let mut value = if include_secrets {
            toml::Value::try_from(self)?
        } else {
            let redacted: Self = serde_json::from_value(self.redacted())?;
            toml::Value::try_from(&redacted)?
        };
        interpolate::escape_value(&mut value);
        Ok(toml::to_string_pretty(&value)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Config {
        Config {
            admin_token: "a".repeat(MIN_SECRET_LEN),
            key_pepper: "p".repeat(MIN_SECRET_LEN),
            ..Config::default()
        }
    }

    fn provider(id: &str, key: &str) -> Provider {
        Provider {
            id: id.into(),
            base_url: "https://api.example.com/v1".into(),
            api_key: key.into(),
            ..Provider::default()
        }
    }

    fn model(id: &str, provider: &str) -> Model {
        Model {
            id: id.into(),
            capability: Some(Capability::Chat),
            routes: vec![Route {
                provider: provider.into(),
                model: id.into(),
                ..Route::default()
            }],
            ..Model::default()
        }
    }

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
    fn env_bootstraps_a_provider_and_a_paid_model_per_kind() {
        let mut config = Config::default();
        config.apply_env(|name| match name {
            "OPENAI_API_KEY" => Some("sk-up".into()),
            "OPENAI_MODEL" => Some("gpt-x".into()),
            "OPENAI_MODEL_FALLBACKS" => Some(" a, ,b,gpt-x ".into()),
            "TTS_MODEL" => Some("tts-1".into()),
            "TTS_VOICE" => Some("alloy".into()),
            "STT_MODEL" => Some("whisper-1".into()),
            "IMG_BASE_URL" => Some("https://img.example/v1".into()),
            "IMG_MODEL" => Some("meta/muse-image".into()),
            "PAYROOF_PROJECT" => Some("my-store".into()),
            "STRIPE_SECRET_KEY" => Some("sk_test".into()),
            _ => None,
        });
        let ids: Vec<_> = config.providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["llm", "tts", "stt", "image"]);
        assert_eq!(config.providers[0].api_key, "sk-up");
        assert_eq!(config.providers[3].base_url, "https://img.example/v1");
        let chat = config.find_model("gpt-x").unwrap();
        assert_eq!(
            (chat.tier, chat.capability(), chat.default, chat.strategy),
            (Tier::Paid, Capability::Chat, true, Strategy::Ordered)
        );
        let models: Vec<_> = chat.routes.iter().map(|r| r.model.as_str()).collect();
        assert_eq!(models, ["gpt-x", "a", "b"]);
        assert!(chat.routes.iter().all(|r| r.provider == "llm"));
        assert_eq!(config.find_model("tts-1").unwrap().voice, "alloy");
        assert_eq!(
            config.find_model("whisper-1").unwrap().capability(),
            Capability::Transcription
        );
        assert_eq!(config.payroof.project, "my-store");
        assert_eq!(config.stripe.secret_key, "sk_test");
        let mut empty = Config::default();
        empty.apply_env(|_| None);
        assert!(empty.providers.is_empty() && empty.models.is_empty());
        // compose.yaml passes a default base URL and empty values for unset
        // variables: they define no provider.
        let mut compose = Config::default();
        compose.apply_env(|name| match name {
            n if n.ends_with("_BASE_URL") => Some("https://api.openai.com/v1".into()),
            n if n.ends_with("_API_KEY") || n.ends_with("_MODEL") => Some(String::new()),
            _ => None,
        });
        assert!(compose.providers.is_empty() && compose.models.is_empty());
        // A settings file whose paid chat default can be served keeps it: the
        // bootstrapped model joins the catalog without being the default. A
        // hidden file default (its provider is disabled) does not count.
        for (enabled, file_default) in [(true, true), (false, false)] {
            let mut filed = Config {
                providers: vec![Provider {
                    id: "openrouter".into(),
                    base_url: "https://openrouter.ai/api/v1".into(),
                    enabled,
                    ..Provider::default()
                }],
                models: vec![Model {
                    id: "gpt-6-luna".into(),
                    capability: Some(Capability::Chat),
                    default: true,
                    routes: vec![Route {
                        provider: "openrouter".into(),
                        model: "openai/gpt-6-luna".into(),
                        ..Route::default()
                    }],
                    ..Model::default()
                }],
                ..Config::default()
            };
            filed.apply_env(|name| match name {
                "OPENAI_API_KEY" => Some("sk-up".into()),
                "OPENAI_MODEL" => Some("gpt-x".into()),
                _ => None,
            });
            let bootstrapped = filed.models.iter().find(|m| m.id == "gpt-x").unwrap();
            assert_eq!(bootstrapped.default, !file_default);
            let chat = filed.default_model(Capability::Chat, Tier::Paid).unwrap();
            assert_eq!(chat.id, if file_default { "gpt-6-luna" } else { "gpt-x" });
            filed.admin_token = "a".repeat(MIN_SECRET_LEN);
            filed.key_pepper = "p".repeat(MIN_SECRET_LEN);
            filed.validate().unwrap();
        }
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
        let mut config = valid();
        config.providers = vec![
            provider("groq", "gsk-secret"),
            provider("other", "o-secret"),
        ];
        config.models = vec![model("m", "groq")];
        config.payroof.token = "prf_secret".into();
        config.providers[0]
            .headers
            .insert("X-Title".into(), "Reseller".into());
        config.validate().unwrap();
        let redacted = config.redacted();
        for path in SECRET_PATHS {
            assert_eq!(redacted.pointer(path).unwrap(), "", "{path}");
        }
        for i in 0..2 {
            assert_eq!(redacted["providers"][i]["api_key"], "");
        }
        assert_eq!(redacted["providers"][0]["id"], "groq");
        let full = Config::from_toml_str(&config.to_toml(true).unwrap(), |_| None).unwrap();
        assert_eq!(full, config);
        let safe = config.to_toml(false).unwrap();
        assert!(!safe.contains("gsk-secret") && !safe.contains("o-secret"));
        assert!(!safe.contains("prf_secret"));
        // A literal "${" survives export and import.
        config.branding.custom_css = ".a::after { content: \"${not-a-variable}\" }".into();
        let back = Config::from_toml_str(&config.to_toml(true).unwrap(), |_| None).unwrap();
        assert_eq!(back, config);
    }

    #[test]
    fn files_reference_the_environment_and_unset_keys_disable_providers() {
        let text = r#"
            [[providers]]
            id = "set"
            base_url = "https://${HOST:-api.example.com}/v1"
            api_key = "${SET_KEY}"
            [[providers]]
            id = "unset"
            base_url = "https://unset.example/v1"
            api_key = "${UNSET_KEY:-}"
            [[providers]]
            id = "local"
            base_url = "http://127.0.0.1:11434/v1"
            [[models]]
            id = "m"
            capability = "chat"
            routes = [{ provider = "unset", model = "x" }, { provider = "set", model = "y" }]
            [[models]]
            id = "dark"
            capability = "chat"
            routes = [{ provider = "unset", model = "x" }]
        "#;
        let c = Config::from_toml_str(text, |name| {
            (name == "SET_KEY").then(|| "sk-from-env".to_owned())
        })
        .unwrap();
        assert_eq!(c.providers[0].base_url, "https://api.example.com/v1");
        assert_eq!(c.providers[0].api_key, "sk-from-env");
        assert!(c.providers[0].enabled);
        assert!(!c.providers[1].enabled, "unset key variable");
        assert!(c.providers[2].enabled, "a literal empty key stays enabled");
        // A model keeps serving through its other routes. One without an
        // enabled provider is hidden like a disabled model.
        assert!(c.find_model("m").is_some());
        assert!(c.find_model("dark").is_none());
        let missing = Config::from_toml_str(text, |_| None).unwrap_err();
        assert!(format!("{missing:#}").contains("SET_KEY"), "{missing:#}");
    }

    #[test]
    fn blank_provider_keys_keep_the_stored_key_by_id() {
        let mut stored = valid();
        stored.providers = vec![provider("a", "key-a"), provider("b", "key-b")];
        let mut patched = valid();
        // Reordered, one blank, one replaced, one new.
        patched.providers = vec![provider("b", ""), provider("a", "new-a"), provider("c", "")];
        patched.keep_provider_keys(&stored);
        let keys: Vec<_> = patched
            .providers
            .iter()
            .map(|p| p.api_key.as_str())
            .collect();
        assert_eq!(keys, ["key-b", "new-a", ""]);
    }

    #[test]
    fn reserved_provider_headers_are_rejected() {
        let mut config = valid();
        config.providers = vec![provider("a", "k")];
        config.providers[0]
            .headers
            .insert("Authorization".into(), "Bearer x".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn routes_must_name_a_configured_provider() {
        let mut c = valid();
        c.providers = vec![provider("a", "k")];
        c.models = vec![model("m", "a")];
        c.validate().unwrap();
        c.models = vec![model("m", "missing")];
        assert!(c.validate().is_err());
        c.providers = vec![provider("a", "k"), provider("a", "k2")];
        c.models = vec![model("m", "a")];
        assert!(c.validate().is_err(), "duplicate provider ids");
    }

    #[test]
    fn new_sections_are_validated() {
        let mut c = valid();
        c.validate().unwrap();
        c.signup.pow_max_number = signup::MAX_POW_NUMBER + 1;
        assert!(c.validate().is_err());
        let mut c = valid();
        c.signup.pow_ttl_seconds = 1;
        assert!(c.validate().is_err());
        let mut c = valid();
        c.keys.max_per_account = 0;
        assert!(c.validate().is_err());
        let mut c = valid();
        c.keys.default_limits.requests = Some(crate::keys::Window {
            max: 0,
            window_hours: 1,
        });
        assert!(c.validate().is_err());
        let mut c = valid();
        c.mcp.max_media_bytes = 0;
        assert!(c.validate().is_err());
        let mut c = valid();
        c.prices.get_mut("default").unwrap().cached_input = Some("abc".into());
        assert!(c.validate().is_err());
        for bad in [
            |c: &mut Config| c.routing.max_attempts = 0,
            |c: &mut Config| c.routing.max_attempts = 11,
            |c: &mut Config| c.routing.failure_threshold = 0,
            |c: &mut Config| c.routing.cooldown_seconds = 0,
            |c: &mut Config| c.routing.cooldown_seconds = 601,
            |c: &mut Config| c.routing.max_cooldown_seconds = 86_401,
            |c: &mut Config| c.free.ipv6_prefix = 31,
            |c: &mut Config| c.free.ipv6_prefix = 129,
        ] {
            let mut c = valid();
            bad(&mut c);
            assert!(c.validate().is_err());
        }
        let mut c = valid();
        c.free.rpm = 0;
        c.free.rpd = 0;
        c.free.tpd = 0;
        c.free.max_concurrent = 0;
        c.free.max_concurrent_total = 0;
        c.free.max_output_tokens = 0;
        c.free.max_body_bytes = 0;
        c.validate().expect("0 disables each free limit");
    }

    #[test]
    fn removed_settings_are_refused_and_new_ones_parse() {
        for removed in [
            "require_key = true",
            "accept_any_token = false",
            "model_policy = \"force\"",
            "voice_policy = \"force\"",
            "[llm]\napi_key = \"x\"",
            "[tts]\nmodel = \"x\"",
            "[signup]\nfree_credit_usd = \"1\"",
            "[signup]\nfree_credit_halving = true",
            "[signup]\nfree_credit_expiry_days = 3",
            "[signup]\nip_limit_per_month = 1",
            "[signup.ip_overrides]\n\"1.2.3.4\" = 0",
        ] {
            assert!(toml::from_str::<Config>(removed).is_err(), "{removed}");
        }
        let parsed: Config = toml::from_str(
            "[[providers]]\nid = \"g\"\nbase_url = \"https://g.example/v1\"\n\
             limits = { rpm = 30, rpd = 1000 }\n\
             [[models]]\nid = \"free\"\ntier = \"free\"\ncapability = \"chat\"\n\
             strategy = \"ordered\"\n[[models.routes]]\nprovider = \"g\"\nmodel = \"m\"\n\
             weight = 5\nlimits = { rpm = 30 }\n[routing]\nmax_attempts = 2\n\
             [free]\nrpm = 5\n[keys.default_limits]\nrpm = 5\n",
        )
        .unwrap();
        assert_eq!(parsed.providers[0].limits.rpm, 30);
        assert!(parsed.providers[0].enabled);
        assert_eq!(parsed.models[0].tier, Tier::Free);
        assert_eq!(parsed.models[0].routes[0].weight, 5);
        assert_eq!(parsed.routing.max_attempts, 2);
        assert_eq!(parsed.free.rpm, 5);
        assert_eq!(parsed.keys.default_limits.rpm, Some(5));
    }

    #[test]
    fn price_fields_round_trip_and_optional_cached_input_is_omitted() {
        let mut c = valid();
        let toml = c.to_toml(true).unwrap();
        assert!(!toml.contains("cached_input"));
        c.prices.insert(
            "m".into(),
            Price {
                cached_input: Some("0.25".into()),
                characters: "15".into(),
                request: "0.001".into(),
                ..Price::default()
            },
        );
        c.validate().unwrap();
        let back = Config::from_toml_str(&c.to_toml(true).unwrap(), |_| None).unwrap();
        assert_eq!(back, c);
    }
}
