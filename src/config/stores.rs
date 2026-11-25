//! Native app store billing: App Store (StoreKit 2) and Google Play.
use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Which App Store environment's transactions are accepted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppleEnvironment {
    /// Live purchases only.
    #[default]
    Production,
    /// Sandbox and TestFlight purchases only.
    Sandbox,
}

impl AppleEnvironment {
    /// The `environment` value of Apple's signed payloads.
    pub fn as_apple(self) -> &'static str {
        match self {
            Self::Production => "Production",
            Self::Sandbox => "Sandbox",
        }
    }
}

/// App Store in-app purchases: signed transaction verification, the App
/// Store Server API, and App Store Server Notifications V2.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Apple {
    /// The app's bundle id. Setting it enables App Store purchases.
    pub bundle_id: String,
    /// The app's Apple ID (numeric). Production notifications must carry it.
    pub app_apple_id: Option<u64>,
    pub environment: AppleEnvironment,
    /// App Store Connect API issuer id (In-App Purchase key).
    pub issuer_id: String,
    /// In-App Purchase key id.
    pub key_id: String,
    /// In-App Purchase private key (the `.p8` PEM text).
    pub private_key: String,
    /// App Store Server API origin. Empty uses Apple's host for `environment`.
    pub api_url: String,
    /// Answer `CONSUMPTION_REQUEST` notifications with consumption data for
    /// customers who consented. Needs the API key.
    pub consumption_reporting: bool,
}

impl Default for Apple {
    fn default() -> Self {
        Self {
            bundle_id: String::new(),
            app_apple_id: None,
            environment: AppleEnvironment::Production,
            issuer_id: String::new(),
            key_id: String::new(),
            private_key: String::new(),
            api_url: String::new(),
            consumption_reporting: false,
        }
    }
}

impl Apple {
    /// Whether App Store purchases are accepted.
    pub fn enabled(&self) -> bool {
        !self.bundle_id.is_empty()
    }

    /// Whether the App Store Server API can be called (key configured).
    pub fn api_enabled(&self) -> bool {
        self.enabled()
            && !self.issuer_id.is_empty()
            && !self.key_id.is_empty()
            && !self.private_key.is_empty()
    }

    /// App Store Server API origin without a trailing slash.
    pub fn api_base(&self) -> &str {
        match (self.api_url.is_empty(), self.environment) {
            (false, _) => self.api_url.trim_end_matches('/'),
            (true, AppleEnvironment::Production) => "https://api.storekit.apple.com",
            (true, AppleEnvironment::Sandbox) => "https://api.storekit-sandbox.apple.com",
        }
    }

    pub(super) fn validate(&self) -> anyhow::Result<()> {
        if !self.api_url.is_empty() {
            super::check_http_url("apple.api_url", &self.api_url)?;
        }
        anyhow::ensure!(
            !self.consumption_reporting || self.api_enabled(),
            "apple.consumption_reporting needs bundle_id, issuer_id, key_id, and private_key"
        );
        anyhow::ensure!(
            self.private_key.is_empty() || self.private_key.contains("PRIVATE KEY"),
            "apple.private_key must be the PEM text of the .p8 key"
        );
        Ok(())
    }
}

/// Google Play Billing: the Google Play Developer API and Real-time
/// Developer Notifications through a Cloud Pub/Sub push subscription.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Google {
    /// The app's package name. Setting it (with the service account)
    /// enables Google Play purchases.
    pub package_name: String,
    /// Service account JSON key with access to the Play Console app.
    pub service_account_json: String,
    /// Audience of the Pub/Sub push OIDC token. Empty uses the public
    /// notification URL (`<public_url>/webhooks/google`).
    pub pubsub_audience: String,
    /// Service account email that signs the Pub/Sub push tokens.
    pub pubsub_service_account: String,
    /// Answer pending chargeback reviews through the Review Refund API.
    pub refund_review: bool,
    /// Google Play Developer API origin.
    pub api_url: String,
    /// Google's OIDC signing keys (JWKS).
    pub certs_url: String,
}

impl Default for Google {
    fn default() -> Self {
        Self {
            package_name: String::new(),
            service_account_json: String::new(),
            pubsub_audience: String::new(),
            pubsub_service_account: String::new(),
            refund_review: false,
            api_url: "https://androidpublisher.googleapis.com".into(),
            certs_url: "https://www.googleapis.com/oauth2/v3/certs".into(),
        }
    }
}

impl Google {
    /// Whether Google Play purchases are accepted.
    pub fn enabled(&self) -> bool {
        !self.package_name.is_empty() && !self.service_account_json.is_empty()
    }

    /// Google Play Developer API origin without a trailing slash.
    pub fn api_base(&self) -> &str {
        self.api_url.trim_end_matches('/')
    }

    pub(super) fn validate(&self) -> anyhow::Result<()> {
        super::check_http_url("google.api_url", &self.api_url)?;
        super::check_http_url("google.certs_url", &self.certs_url)?;
        if !self.service_account_json.is_empty() {
            let key: serde_json::Value = serde_json::from_str(&self.service_account_json)
                .context("google.service_account_json is not JSON")?;
            anyhow::ensure!(
                key["client_email"].is_string() && key["private_key"].is_string(),
                "google.service_account_json must be a service account key with client_email and private_key"
            );
        }
        anyhow::ensure!(
            self.package_name.is_empty() || !self.pubsub_service_account.is_empty(),
            "google.pubsub_service_account is required to authenticate notifications"
        );
        Ok(())
    }
}
