//! Route selection and failover settings.
use serde::{Deserialize, Serialize};

/// `[routing]`: how many routes a request may try and when a failing route
/// is taken out of rotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Routing {
    /// Routes tried per request (1-10). Only replayable (buffered JSON)
    /// bodies are retried on another route.
    pub max_attempts: u32,
    /// Consecutive failures that open a route's circuit.
    pub failure_threshold: u32,
    /// First circuit-open cooldown in seconds. It doubles on every reopen
    /// (with +/-20% jitter) up to `max_cooldown_seconds`. It is also the
    /// cooldown after a 429 that carries no retry hint.
    pub cooldown_seconds: u64,
    /// Cap of every cooldown. Provider-wide auth and credit failures
    /// (401, 402, 403) cool the provider down for this long.
    pub max_cooldown_seconds: u64,
}

impl Default for Routing {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            failure_threshold: 3,
            cooldown_seconds: 30,
            max_cooldown_seconds: 600,
        }
    }
}

impl Routing {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=10).contains(&self.max_attempts),
            "routing.max_attempts must be between 1 and 10"
        );
        anyhow::ensure!(
            (1..=1000).contains(&self.failure_threshold),
            "routing.failure_threshold must be between 1 and 1000"
        );
        anyhow::ensure!(
            self.cooldown_seconds >= 1
                && self.cooldown_seconds <= self.max_cooldown_seconds
                && self.max_cooldown_seconds <= 86_400,
            "routing.cooldown_seconds must be at least 1 and at most max_cooldown_seconds (at most 86400)"
        );
        Ok(())
    }
}
