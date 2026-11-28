//! Anonymous self-service signup settings.
use serde::{Deserialize, Serialize};

/// Largest accepted `pow_max_number`, the highest proof-of-work difficulty.
pub const MAX_POW_NUMBER: u32 = 10_000_000;

/// Anonymous self-service signup settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Signup {
    /// Whether anonymous account creation is offered at all.
    pub enabled: bool,
    /// Minimum seconds between two signups from one IP. 0 disables the guard.
    pub cooldown_seconds: u32,
    /// Self-service key creations (signup, child keys, rotations) per hour
    /// across all accounts. 0 disables the guard.
    pub global_hourly_limit: u32,
    /// ALTCHA proof-of-work difficulty. 0 disables the challenge.
    pub pow_max_number: u32,
    /// Seconds a challenge stays solvable.
    pub pow_ttl_seconds: u32,
    /// Adaptive proof of work: each business failure from a client IP
    /// (rejected signup, invalid credential, invalid redeem code) doubles
    /// the difficulty of that IP's next signup challenge, up to
    /// `pow_max_number_ceiling`. A successful signup or
    /// `adaptive_pow_reset_minutes` without failures resets it. Server
    /// errors and rate limits never count.
    pub adaptive_pow: bool,
    /// Highest difficulty adaptive proof of work may reach. Keep it low
    /// enough for old phones (about 1000000 solves in a few seconds).
    pub pow_max_number_ceiling: u32,
    /// Minutes after the last failure at which an IP's difficulty resets.
    pub adaptive_pow_reset_minutes: u32,
}

impl Default for Signup {
    fn default() -> Self {
        Self {
            enabled: true,
            cooldown_seconds: 30,
            global_hourly_limit: 100,
            pow_max_number: 100_000,
            pow_ttl_seconds: 300,
            adaptive_pow: true,
            pow_max_number_ceiling: 1_000_000,
            adaptive_pow_reset_minutes: 30,
        }
    }
}

impl Signup {
    /// Highest difficulty a challenge may carry: the adaptive ceiling, or
    /// the fixed difficulty when adaptivity is off.
    pub fn pow_ceiling(&self) -> u32 {
        if self.adaptive_pow {
            self.pow_max_number_ceiling
        } else {
            self.pow_max_number
        }
    }

    /// Checks the proof-of-work difficulty and challenge lifetime.
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.pow_max_number <= MAX_POW_NUMBER,
            "signup.pow_max_number must be at most {MAX_POW_NUMBER}"
        );
        anyhow::ensure!(
            self.pow_max_number_ceiling >= self.pow_max_number
                && self.pow_max_number_ceiling <= MAX_POW_NUMBER,
            "signup.pow_max_number_ceiling must be between pow_max_number and {MAX_POW_NUMBER}"
        );
        anyhow::ensure!(
            (1..=1440).contains(&self.adaptive_pow_reset_minutes),
            "signup.adaptive_pow_reset_minutes must be between 1 and 1440"
        );
        anyhow::ensure!(
            (10..=86_400).contains(&self.pow_ttl_seconds),
            "signup.pow_ttl_seconds must be between 10 and 86400"
        );
        Ok(())
    }
}
