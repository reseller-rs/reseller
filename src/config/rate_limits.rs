//! Abuse limits for public and credentialed endpoints.
//!
//! Setting a limit to 0 disables it.
use serde::{Deserialize, Serialize};

/// Per-client and per-account request budgets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimits {
    /// Management API requests per client IP per minute.
    pub management_per_minute: u32,
    /// Invalid-credential attempts per client IP per hour. Once the budget is
    /// spent, every credentialed request from that IP is refused.
    pub auth_failures_per_hour: u32,
    /// In-flight proxy requests per account.
    pub max_concurrent_requests: u32,
    /// Checkout orders per account per hour.
    pub checkouts_per_hour: u32,
    /// Invalid redeem codes per account per hour.
    pub redeem_failures_per_hour: u32,
    /// Signup challenges per client IP per minute.
    pub challenges_per_minute: u32,
    /// Hours after signup during which an account's daily spending is
    /// capped by `new_account_daily_spend_pct`. 0 disables the cap.
    pub new_account_hours: u32,
    /// Percent of the paid allowance (monthly subscription quota plus
    /// purchased top-ups) a new account may spend per rolling 24 hours.
    /// "0" disables the cap.
    pub new_account_daily_spend_pct: String,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            management_per_minute: 120,
            auth_failures_per_hour: 20,
            max_concurrent_requests: 8,
            checkouts_per_hour: 10,
            redeem_failures_per_hour: 10,
            challenges_per_minute: 10,
            new_account_hours: 48,
            new_account_daily_spend_pct: "20".into(),
        }
    }
}

impl RateLimits {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        let pct = crate::money::decimal(&self.new_account_daily_spend_pct)
            .map_err(|e| anyhow::anyhow!("rate_limits.new_account_daily_spend_pct: {e}"))?;
        anyhow::ensure!(
            pct <= rust_decimal::Decimal::from(100),
            "rate_limits.new_account_daily_spend_pct must be 0-100"
        );
        anyhow::ensure!(
            self.new_account_hours <= 24 * 365,
            "rate_limits.new_account_hours must be at most 8760"
        );
        Ok(())
    }
}
