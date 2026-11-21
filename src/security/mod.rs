//! Abuse protection for public endpoints: in-memory rate limiting and the
//! ALTCHA-compatible proof-of-work used by anonymous signup.
pub mod limiter;
pub mod pow;

use crate::{
    App,
    error::{Error, Result},
};
use std::time::Duration;

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);

/// Error codes that mean "a presented credential did not match anything".
/// Only these count against the per-IP auth-failure budget.
const AUTH_FAILURE_CODES: [&str; 2] = ["invalid_api_key", "dashboard_unauthorized"];

/// Whether `error` is a failed credential guess.
pub fn is_auth_failure(error: &Error) -> bool {
    AUTH_FAILURE_CODES.contains(&error.code)
}

/// Charges one management API request to `ip`, or fails with a retry hint.
pub async fn management(app: &App, ip: &str) -> Result<()> {
    let limit = app.config().await.rate_limits.management_per_minute;
    app.limiter
        .acquire(&format!("mgmt:{ip}"), limit, MINUTE)
        .map_err(|wait| {
            Error::too_many("management_rate_limited", "Too many requests; slow down")
                .retry_after(wait)
        })
}

/// Charges one signup challenge request to `ip`, or fails with a retry hint.
pub async fn challenge(app: &App, ip: &str) -> Result<()> {
    let limit = app.config().await.rate_limits.challenges_per_minute;
    app.limiter
        .acquire(&format!("challenge:{ip}"), limit, MINUTE)
        .map_err(|wait| {
            Error::too_many(
                "challenge_rate_limited",
                "Too many signup challenges requested",
            )
            .retry_after(wait)
        })
}

/// Refuses credentialed requests from an IP that spent its failure budget.
///
/// This runs before any database lookup.
pub async fn check_auth_failures(app: &App, ip: &str) -> Result<()> {
    let limit = app.config().await.rate_limits.auth_failures_per_hour;
    match app.limiter.blocked(&format!("authfail:{ip}"), limit, HOUR) {
        Some(wait) => Err(Error::too_many(
            "too_many_auth_failures",
            "Too many invalid credentials from this address; try again later",
        )
        .retry_after(wait)),
        None => Ok(()),
    }
}

/// Charges one failed credential guess to `ip`.
pub async fn record_auth_failure(app: &App, ip: &str) {
    let limit = app.config().await.rate_limits.auth_failures_per_hour;
    app.limiter.record(&format!("authfail:{ip}"), limit, HOUR);
}

/// Refuses redeem attempts from an account that keeps submitting bad codes.
pub async fn check_redeem(app: &App, account: &str) -> Result<()> {
    let limit = app.config().await.rate_limits.redeem_failures_per_hour;
    match app
        .limiter
        .blocked(&format!("redeem:{account}"), limit, HOUR)
    {
        Some(wait) => Err(Error::too_many(
            "redeem_rate_limited",
            "Too many invalid redeem codes; try again later",
        )
        .retry_after(wait)),
        None => Ok(()),
    }
}

/// Charges one invalid redeem code to the account and to the client IP's
/// credential budget.
pub async fn record_redeem_failure(app: &App, account: &str, ip: &str) {
    let limit = app.config().await.rate_limits.redeem_failures_per_hour;
    app.limiter
        .record(&format!("redeem:{account}"), limit, HOUR);
    record_auth_failure(app, ip).await;
}
