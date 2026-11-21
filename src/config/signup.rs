//! Anonymous self-service signup settings.
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Largest accepted `pow_max_number`, the highest proof-of-work difficulty.
pub const MAX_POW_NUMBER: u32 = 10_000_000;

/// Anonymous self-service signup settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Signup {
    /// Whether anonymous account creation is offered at all.
    pub enabled: bool,
    /// USD trial credit granted to each new account, as a decimal string.
    pub free_credit_usd: String,
    /// Days the signup credit stays spendable. 0 means it never expires.
    pub free_credit_expiry_days: u32,
    /// Self-service keys per source IP per calendar month. 0 disables the guard.
    pub ip_limit_per_month: u32,
    /// Minimum seconds between creations from one IP. 0 disables the guard.
    pub cooldown_seconds: u32,
    /// Self-service key creations (signup, child keys, rotations) per hour
    /// across all accounts. 0 disables the guard.
    pub global_hourly_limit: u32,
    /// ALTCHA proof-of-work difficulty. 0 disables the challenge.
    pub pow_max_number: u32,
    /// Seconds a challenge stays solvable.
    pub pow_ttl_seconds: u32,
    /// Per-IP replacements for `ip_limit_per_month`. 0 means unlimited.
    pub ip_overrides: BTreeMap<String, u32>,
}

impl Default for Signup {
    fn default() -> Self {
        Self {
            enabled: true,
            free_credit_usd: "0.25".into(),
            free_credit_expiry_days: 30,
            ip_limit_per_month: 2,
            cooldown_seconds: 30,
            global_hourly_limit: 100,
            pow_max_number: 100_000,
            pow_ttl_seconds: 300,
            ip_overrides: BTreeMap::new(),
        }
    }
}

impl Signup {
    /// Checks the credit amount, expiry, proof-of-work difficulty, and challenge lifetime.
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        crate::money::usd(&self.free_credit_usd).context("signup.free_credit_usd")?;
        anyhow::ensure!(
            self.free_credit_expiry_days <= 3650,
            "signup.free_credit_expiry_days must be at most 3650"
        );
        anyhow::ensure!(
            self.pow_max_number <= MAX_POW_NUMBER,
            "signup.pow_max_number must be at most {MAX_POW_NUMBER}"
        );
        anyhow::ensure!(
            (10..=86_400).contains(&self.pow_ttl_seconds),
            "signup.pow_ttl_seconds must be between 10 and 86400"
        );
        Ok(())
    }
}
