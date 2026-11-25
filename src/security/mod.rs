//! Abuse protection for public endpoints: in-memory rate limiting and the
//! ALTCHA-compatible proof-of-work used by anonymous signup.
pub mod limiter;
pub mod pow;
pub mod strikes;

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
            Error::too_many(
                "management_rate_limited",
                "Too many requests. Slow down",
                wait,
            )
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
                wait,
            )
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
            "Too many invalid credentials from this address. Try again later",
            wait,
        )),
        None => Ok(()),
    }
}

/// Charges one failed credential guess to `ip`: the auth-failure budget and
/// the adaptive proof-of-work difficulty.
pub async fn record_auth_failure(app: &App, ip: &str) {
    let limit = app.config().await.rate_limits.auth_failures_per_hour;
    app.limiter.record(&format!("authfail:{ip}"), limit, HOUR);
    record_business_failure(app, ip).await;
}

fn pow_key(ip: &str) -> String {
    format!("pow:{ip}")
}

/// Signup proof-of-work difficulty for `ip`.
///
/// With `signup.adaptive_pow`, each recent business failure from the IP
/// doubles `pow_max_number`, capped at `pow_max_number_ceiling`, so a real
/// user who mistypes a few times still solves the challenge quickly.
/// 0 means proof of work is off.
pub async fn pow_difficulty(app: &App, ip: &str) -> u32 {
    let config = app.config().await;
    let signup = &config.signup;
    if !signup.adaptive_pow || signup.pow_max_number == 0 {
        return signup.pow_max_number;
    }
    let ttl = Duration::from_secs(u64::from(signup.adaptive_pow_reset_minutes) * 60);
    let strikes = app.strikes.count(&pow_key(ip), ttl);
    let raised = u64::from(signup.pow_max_number) << strikes.min(32);
    u32::try_from(raised.min(u64::from(signup.pow_max_number_ceiling)))
        .unwrap_or(signup.pow_max_number_ceiling)
}

/// Records a business failure from `ip` (rejected signup, invalid
/// credential or redeem code) for adaptive proof of work. Callers must not
/// record server errors, rate limits, or network failures.
pub async fn record_business_failure(app: &App, ip: &str) {
    let config = app.config().await;
    if config.signup.adaptive_pow {
        let ttl = Duration::from_secs(u64::from(config.signup.adaptive_pow_reset_minutes) * 60);
        app.strikes.strike(&pow_key(ip), ttl);
    }
}

/// Resets the adaptive proof-of-work difficulty of `ip` after a successful
/// signup.
pub fn record_signup_success(app: &App, ip: &str) {
    app.strikes.clear(&pow_key(ip));
}

/// Whether a signup error is a business failure that raises the IP's
/// proof-of-work difficulty: a client error other than a rate limit or a
/// challenge that is merely easier than the IP now needs.
pub fn is_business_failure(error: &Error) -> bool {
    error.status.is_client_error()
        && error.status != axum::http::StatusCode::TOO_MANY_REQUESTS
        && !matches!(error.code, "pow_too_easy" | "pow_required")
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
            "Too many invalid redeem codes. Try again later",
            wait,
        )),
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

/// Charges one OAuth client registration to `ip`, or fails with a retry hint.
/// Registration is public, so each address gets a small hourly budget.
pub async fn oauth_register(app: &App, ip: &str) -> Result<()> {
    app.limiter
        .acquire(&format!("oauthreg:{ip}"), 30, HOUR)
        .map_err(|wait| {
            Error::too_many(
                "oauth_registration_rate_limited",
                "Too many client registrations. Try again later",
                wait,
            )
        })
}
