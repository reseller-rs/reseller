//! API-key policy: caps, creation budget, rotation cooldown, default limits.
use crate::keys::Limits;
use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeyPolicy {
    /// Active keys allowed per account (must be greater than 0).
    pub max_per_account: u32,
    /// Keys a customer may create or rotate per account in a rolling 24 hours.
    /// Administrators are exempt. 0 disables the guard.
    pub max_new_per_day: u32,
    /// A customer may rotate a key only once it is this many seconds old.
    /// 0 disables the guard.
    pub rotate_cooldown_seconds: u32,
    /// Copied onto every newly minted key.
    pub default_limits: Limits,
}

impl Default for KeyPolicy {
    fn default() -> Self {
        Self {
            max_per_account: 10,
            max_new_per_day: 20,
            rotate_cooldown_seconds: 300,
            default_limits: Limits::default(),
        }
    }
}

impl KeyPolicy {
    /// Checks the account key cap, the rotation cooldown, and the default limits.
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.max_per_account > 0,
            "keys.max_per_account must be positive"
        );
        anyhow::ensure!(
            self.rotate_cooldown_seconds <= 86_400,
            "keys.rotate_cooldown_seconds must be at most 86400"
        );
        self.default_limits
            .validate()
            .context("keys.default_limits")?;
        Ok(())
    }
}
