//! Keyless access to the free tier.
use serde::{Deserialize, Serialize};

/// `[free]`: anonymous access to `tier = "free"` models. Every limit applies
/// per client (an IPv4 address, an IPv6 prefix, or a paying account). 0
/// disables a limit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Free {
    /// Serve free models without an API key. When false, anonymous requests
    /// get 401 `missing_api_key`, and keys still use free models.
    pub enabled: bool,
    /// IPv6 clients are grouped by this prefix length (32-128).
    pub ipv6_prefix: u8,
    /// Requests per minute (GCRA, burst of `rpm`).
    pub rpm: u32,
    /// Requests per day (GCRA). A client may use all of `rpd` at once, then
    /// the allowance refills continuously at `rpd` per 24 hours, so over any
    /// 24 hours it can make up to about twice `rpd` requests.
    pub rpd: u32,
    /// Tokens per day (GCRA, like `rpd`: a full burst, then a continuous
    /// refill of `tpd` per 24 hours), charged after each request completes.
    pub tpd: u32,
    /// Requests in flight per client.
    pub max_concurrent: u32,
    /// Free requests in flight across all clients.
    pub max_concurrent_total: u32,
    /// Cap on `max_tokens`, `max_completion_tokens` and `max_output_tokens`.
    /// It is injected when a chat, responses or completions request has none.
    /// Reasoning models spend output tokens on reasoning, so a very small cap
    /// can leave no room for the answer and yield an empty one.
    pub max_output_tokens: u32,
    /// Largest JSON body of a free request, in bytes.
    pub max_body_bytes: usize,
}

impl Default for Free {
    fn default() -> Self {
        Self {
            enabled: true,
            ipv6_prefix: 64,
            rpm: 10,
            rpd: 300,
            tpd: 300_000,
            max_concurrent: 2,
            max_concurrent_total: 64,
            max_output_tokens: 4096,
            max_body_bytes: 1024 * 1024,
        }
    }
}

impl Free {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (32..=128).contains(&self.ipv6_prefix),
            "free.ipv6_prefix must be between 32 and 128"
        );
        anyhow::ensure!(
            self.max_body_bytes <= 512 * 1024 * 1024,
            "free.max_body_bytes must be at most 512 MiB"
        );
        Ok(())
    }
}
