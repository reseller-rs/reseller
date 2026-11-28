//! Admission control of the free tier. Everything lives in memory and runs
//! before any database write. Over-limit requests fail fast with retry hints
//! instead of queueing.
use crate::{
    config::{Config, Free},
    error::Error,
    security::limiter::{Limiter, Remaining},
};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

const MINUTE: Duration = Duration::from_secs(60);
const DAY: Duration = Duration::from_secs(86_400);

/// The client a free request is charged to: an IPv4 address, an IPv6 prefix
/// of `ipv6_prefix` bits, or (for a paying account) the account itself.
pub fn ip_subject(ip: &str, ipv6_prefix: u8) -> String {
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => format!("ip:{v4}"),
        Ok(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => format!("ip:{v4}"),
            None => {
                let bits = u128::from(v6);
                let prefix = u32::from(ipv6_prefix.clamp(1, 128));
                let mask = u128::MAX << (128 - prefix);
                format!("ip:{}/{prefix}", std::net::Ipv6Addr::from(bits & mask))
            }
        },
        Err(_) => format!("ip:{ip}"),
    }
}

/// Counters of free requests in flight, and the per-client windows.
#[derive(Default)]
pub struct Gate {
    state: Mutex<Counts>,
    limiter: Limiter,
}

#[derive(Default)]
struct Counts {
    total: u32,
    per_subject: HashMap<String, u32>,
}

/// A free request in flight. Dropping it frees its concurrency slots.
pub struct Guard {
    gate: Arc<Gate>,
    subject: String,
    tpd: u32,
}

impl Guard {
    /// Charges the tokens a finished request used to the client's daily
    /// token budget.
    pub fn charge_tokens(&self, tokens: u64) {
        self.gate
            .limiter
            .record_n(&key(&self.subject, "tpd"), self.tpd, DAY, tokens);
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut counts = self.gate.counts();
        counts.total = counts.total.saturating_sub(1);
        if let Some(n) = counts.per_subject.get_mut(&self.subject) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.per_subject.remove(&self.subject);
            }
        }
    }
}

/// An admitted free request.
pub struct Admission {
    pub guard: Guard,
    pub rate: RateInfo,
}

impl std::fmt::Debug for Admission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admission")
            .field("subject", &self.guard.subject)
            .field("rate", &self.rate)
            .finish_non_exhaustive()
    }
}

/// The rate limit state of one client, for response headers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RateInfo {
    rpm: Option<Remaining>,
    rpd: Option<Remaining>,
}

fn key(subject: &str, window: &str) -> String {
    format!("{subject}\u{0}{window}")
}

fn configured(r: Remaining) -> Option<Remaining> {
    (r.limit > 0).then_some(r)
}

/// `6s`, `1m30s`, `250ms`: the duration style of the OpenAI reset headers.
fn reset_text(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        return format!("{}ms", d.as_millis());
    }
    let secs = crate::security::limiter::seconds(d);
    match secs / 60 {
        0 => format!("{secs}s"),
        m => format!("{m}m{}s", secs % 60),
    }
}

impl RateInfo {
    /// The rate limit headers: `x-ratelimit-*-requests` (OpenAI style, for the
    /// per-minute limit, else the daily one) and the IETF `RateLimit-Policy`
    /// and `RateLimit` fields (draft-ietf-httpapi-ratelimit-headers-12: `q`
    /// quota and `w` window in the policy, `a` available units and `w`
    /// seconds until the window resets in the state), which
    /// list every configured window.
    pub fn headers(&self) -> Vec<(HeaderName, HeaderValue)> {
        let mut out = Vec::new();
        let mut put = |name: &'static str, value: String| {
            if let Ok(value) = HeaderValue::from_str(&value) {
                out.push((HeaderName::from_static(name), value));
            }
        };
        let windows = [(self.rpm, "free", MINUTE), (self.rpd, "free-day", DAY)];
        if let Some(head) = self.rpm.or(self.rpd) {
            put("x-ratelimit-limit-requests", head.limit.to_string());
            put("x-ratelimit-remaining-requests", head.remaining.to_string());
            put("x-ratelimit-reset-requests", reset_text(head.reset));
        }
        let policy: Vec<String> = windows
            .iter()
            .filter_map(|(r, name, w)| {
                r.map(|r| format!("\"{name}\";q={};w={}", r.limit, w.as_secs()))
            })
            .collect();
        let state: Vec<String> = windows
            .iter()
            .filter_map(|(r, name, _)| {
                r.map(|r| {
                    let reset = if r.reset.is_zero() {
                        0
                    } else {
                        crate::security::limiter::seconds(r.reset)
                    };
                    format!("\"{name}\";a={};w={reset}", r.remaining)
                })
            })
            .collect();
        if !policy.is_empty() {
            put("ratelimit-policy", policy.join(", "));
            put("ratelimit", state.join(", "));
        }
        out
    }
}

impl Gate {
    fn counts(&self) -> std::sync::MutexGuard<'_, Counts> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn rate(&self, free: &Free, subject: &str) -> RateInfo {
        RateInfo {
            rpm: configured(
                self.limiter
                    .remaining(&key(subject, "rpm"), free.rpm, MINUTE),
            ),
            rpd: configured(self.limiter.remaining(&key(subject, "rpd"), free.rpd, DAY)),
        }
    }

    fn deny(&self, free: &Free, subject: &str, error: Error) -> Error {
        self.rate(free, subject)
            .headers()
            .into_iter()
            .fold(error, |e, (name, value)| e.with_header(name, value))
    }

    /// Admits one free request of `subject`, or fails with a retry hint. The
    /// checks run in this order: free requests in flight overall, in flight
    /// for this client, requests per minute, requests per day, tokens per day.
    pub fn admit(self: &Arc<Self>, config: &Config, subject: &str) -> Result<Admission, Error> {
        let free = &config.free;
        let mut counts = self.counts();
        if free.max_concurrent_total > 0 && counts.total >= free.max_concurrent_total {
            // Spread the retries of a stampede a little.
            let jitter = u64::from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.subsec_millis()),
            );
            return Err(Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "capacity_exhausted",
                "Free capacity is fully in use right now. Retry in a moment, or use an API key",
            )
            .retry_after_ms(1000 + jitter));
        }
        let busy = counts.per_subject.get(subject).copied().unwrap_or(0);
        if free.max_concurrent > 0 && busy >= free.max_concurrent {
            return Err(self.deny(
                free,
                subject,
                Error::too_many(
                    "concurrency_limit_exceeded",
                    "Too many free requests in flight. Wait for one to finish",
                    1,
                )
                .retry_after_ms(1000),
            ));
        }
        let (rpm, rpd, tpd) = (
            key(subject, "rpm"),
            key(subject, "rpd"),
            key(subject, "tpd"),
        );
        if let Some(wait) = self.limiter.blocked_for(&rpm, free.rpm, MINUTE, 1) {
            return Err(self.deny(
                free,
                subject,
                Error::too_many(
                    "rate_limit_exceeded",
                    format!(
                        "Free tier limit of {} requests per minute reached",
                        free.rpm
                    ),
                    1,
                )
                .retry_after_ms(wait.as_millis().try_into().unwrap_or(u64::MAX)),
            ));
        }
        let upgrade = format!(
            "Create an API key for higher limits: {}/workspace/",
            config.public_base()
        );
        if let Some(wait) = self.limiter.blocked_for(&rpd, free.rpd, DAY, 1) {
            return Err(self.deny(
                free,
                subject,
                Error::quota_window(
                    "free_quota_exceeded",
                    format!(
                        "Free tier limit of {} requests per day reached. {upgrade}",
                        free.rpd
                    ),
                    1,
                )
                .retry_after_ms(wait.as_millis().try_into().unwrap_or(u64::MAX)),
            ));
        }
        // A client that spent its tokens waits for 1% of the allowance to
        // refill, not for a single token.
        let refill = (free.tpd / 100).max(1);
        if let Some(wait) = self.limiter.blocked_for(&tpd, free.tpd, DAY, refill) {
            return Err(self.deny(
                free,
                subject,
                Error::quota_window(
                    "free_quota_exceeded",
                    format!(
                        "Free tier limit of {} tokens per day reached. {upgrade}",
                        free.tpd
                    ),
                    1,
                )
                .retry_after_ms(wait.as_millis().try_into().unwrap_or(u64::MAX)),
            ));
        }
        // Every check passed while holding the lock, so these cannot fail.
        let _ = self.limiter.try_acquire(&rpm, free.rpm, MINUTE, 1);
        let _ = self.limiter.try_acquire(&rpd, free.rpd, DAY, 1);
        counts.total += 1;
        *counts.per_subject.entry(subject.to_owned()).or_default() += 1;
        drop(counts);
        Ok(Admission {
            guard: Guard {
                gate: Arc::clone(self),
                subject: subject.to_owned(),
                tpd: free.tpd,
            },
            rate: self.rate(free, subject),
        })
    }

    /// Free requests in flight and distinct clients with a tracked window.
    pub fn stats(&self) -> (u32, usize) {
        (self.counts().total, self.limiter.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_is_per_address_and_ipv6_per_prefix() {
        assert_eq!(ip_subject("203.0.113.9", 64), "ip:203.0.113.9");
        assert_eq!(
            ip_subject("2001:db8:1:2:aaaa:bbbb:cccc:dddd", 64),
            "ip:2001:db8:1:2::/64"
        );
        assert_eq!(
            ip_subject("2001:db8:1:2:1111:2222:3333:4444", 64),
            ip_subject("2001:db8:1:2:aaaa:bbbb:cccc:dddd", 64)
        );
        assert_ne!(
            ip_subject("2001:db8:1:3::1", 64),
            ip_subject("2001:db8:1:2::1", 64)
        );
        // A shorter prefix groups more clients.
        assert_eq!(
            ip_subject("2001:db8:1:3::1", 48),
            ip_subject("2001:db8:1:2::1", 48)
        );
        assert_eq!(ip_subject("2001:db8::1", 128), "ip:2001:db8::1/128");
        assert_eq!(ip_subject("::ffff:198.51.100.7", 64), "ip:198.51.100.7");
        assert_eq!(ip_subject("unknown", 64), "ip:unknown");
    }

    fn config(free: Free) -> Config {
        Config {
            free,
            ..Config::default()
        }
    }

    fn gate() -> Arc<Gate> {
        Arc::new(Gate::default())
    }

    #[test]
    fn checks_run_in_order_with_retry_hints() {
        let g = gate();
        let c = config(Free {
            rpm: 2,
            rpd: 0,
            tpd: 0,
            max_concurrent: 1,
            max_concurrent_total: 2,
            ..Free::default()
        });
        let first = g.admit(&c, "ip:a").unwrap();
        assert_eq!(first.rate.rpm.unwrap().remaining, 1);
        // The client is at its own concurrency.
        let e = g.admit(&c, "ip:a").unwrap_err();
        assert_eq!(e.code, "concurrency_limit_exceeded");
        assert_eq!((e.status.as_u16(), e.retry_after), (429, Some(1)));
        let second = g.admit(&c, "ip:b").unwrap();
        // The whole tier is at capacity: 503 with a retry hint.
        let e = g.admit(&c, "ip:c").unwrap_err();
        assert_eq!((e.status.as_u16(), e.code), (503, "capacity_exhausted"));
        assert!(e.retry_after.is_some());
        drop((first, second));
        assert_eq!(g.stats().0, 0);
        // rpm: the second request used the burst, the third waits.
        drop(g.admit(&c, "ip:a").unwrap());
        let e = g.admit(&c, "ip:a").unwrap_err();
        assert_eq!((e.status.as_u16(), e.code), (429, "rate_limit_exceeded"));
        assert!(!e.no_auto_retry);
        assert!(e.retry_after.unwrap() >= 1);
        assert!(e.headers().iter().any(|(n, _)| n == "retry-after-ms"));
        assert!(
            e.headers()
                .iter()
                .any(|(n, v)| n == "x-ratelimit-remaining-requests" && v == "0")
        );
    }

    #[test]
    fn daily_requests_and_tokens_are_quota_errors() {
        let g = gate();
        let c = config(Free {
            rpm: 0,
            rpd: 2,
            tpd: 1000,
            max_concurrent: 0,
            max_concurrent_total: 0,
            ..Free::default()
        });
        drop(g.admit(&c, "ip:a").unwrap());
        drop(g.admit(&c, "ip:a").unwrap());
        let e = g.admit(&c, "ip:a").unwrap_err();
        assert_eq!((e.status.as_u16(), e.code), (429, "free_quota_exceeded"));
        assert!(e.no_auto_retry, "x-should-retry: false");
        assert!(e.message.contains("API key"));
        // Other clients are unaffected, and tokens are charged per client.
        let b = g.admit(&c, "ip:b").unwrap();
        b.guard.charge_tokens(1000);
        drop(b);
        let e = g.admit(&c, "ip:b").unwrap_err();
        assert_eq!(e.code, "free_quota_exceeded");
        assert!(e.message.contains("tokens per day"));
        assert!(e.no_auto_retry);
    }

    #[test]
    fn headers_follow_the_openai_and_ietf_shapes() {
        let g = gate();
        let c = config(Free {
            rpm: 10,
            rpd: 300,
            ..Free::default()
        });
        let admitted = g.admit(&c, "ip:a").unwrap();
        let headers: HashMap<String, String> = admitted
            .rate
            .headers()
            .into_iter()
            .map(|(n, v)| (n.to_string(), v.to_str().unwrap().to_owned()))
            .collect();
        assert_eq!(headers["x-ratelimit-limit-requests"], "10");
        assert_eq!(headers["x-ratelimit-remaining-requests"], "9");
        assert!(headers["x-ratelimit-reset-requests"].ends_with('s'));
        assert_eq!(
            headers["ratelimit-policy"],
            "\"free\";q=10;w=60, \"free-day\";q=300;w=86400"
        );
        assert!(headers["ratelimit"].starts_with("\"free\";a=9;w="));
        assert!(headers["ratelimit"].contains("\"free-day\";a=299;w="));
        // Nothing configured, nothing advertised.
        let none = config(Free {
            rpm: 0,
            rpd: 0,
            ..Free::default()
        });
        assert!(g.admit(&none, "ip:z").unwrap().rate.headers().is_empty());
    }

    #[test]
    fn durations_read_like_openai_reset_headers() {
        assert_eq!(reset_text(Duration::from_millis(250)), "250ms");
        assert_eq!(reset_text(Duration::from_secs(6)), "6s");
        assert_eq!(reset_text(Duration::from_millis(5100)), "6s");
        assert_eq!(reset_text(Duration::from_secs(90)), "1m30s");
    }
}
