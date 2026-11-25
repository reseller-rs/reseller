//! Admission holds and refund protection.
use anyhow::Context;
use serde::{Deserialize, Serialize};

/// How admitted requests reserve balance, and how store refunds are answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Billing {
    /// Output tokens held for an LLM request that sets no `max_tokens`,
    /// `max_completion_tokens`, or `max_output_tokens`.
    pub default_max_output_tokens: u32,
    /// Smallest hold per request in USD. It also covers requests whose cost
    /// cannot be estimated up front (uploads, realtime sessions).
    pub min_hold_usd: String,
    /// Days a refunded customer's signup address may not open new accounts.
    /// 0 disables the address block.
    pub refund_ip_block_days: u32,
    /// Share of a purchase (percent) that must be consumed before refund
    /// answers to Apple and Google ask to decline the refund. Below it they
    /// ask for a prorated refund (Apple) or stay neutral (Google). An unused
    /// purchase always asks for a full refund.
    pub refund_decline_pct: u32,
    /// Typical billed cost of one API request in USD. Used only to show
    /// customers credit and quotas as approximate request counts.
    pub request_estimate_usd: String,
}

impl Default for Billing {
    fn default() -> Self {
        Self {
            default_max_output_tokens: 4096,
            min_hold_usd: "0.01".into(),
            refund_ip_block_days: 30,
            refund_decline_pct: 50,
            request_estimate_usd: "0.001".into(),
        }
    }
}

impl Billing {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        crate::money::usd(&self.min_hold_usd).context("billing.min_hold_usd")?;
        anyhow::ensure!(
            crate::money::usd(&self.request_estimate_usd)
                .context("billing.request_estimate_usd")?
                > 0,
            "billing.request_estimate_usd must be positive"
        );
        anyhow::ensure!(
            (1..=1_000_000).contains(&self.default_max_output_tokens),
            "billing.default_max_output_tokens must be 1-1000000"
        );
        anyhow::ensure!(
            self.refund_ip_block_days <= 3650,
            "billing.refund_ip_block_days must be at most 3650"
        );
        anyhow::ensure!(
            (1..=100).contains(&self.refund_decline_pct),
            "billing.refund_decline_pct must be 1-100"
        );
        Ok(())
    }
}
