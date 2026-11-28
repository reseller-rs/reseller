//! What an upstream tells us about its own limits: `Retry-After`,
//! `retry-after-ms`, the OpenAI-style `x-ratelimit-*` headers and Google's
//! `retryDelay` in 429 bodies. Hints are advisory and parsed leniently: a
//! value that does not parse is ignored.
use axum::http::HeaderMap;
use serde_json::Value;
use std::time::Duration;

/// Longest hint believed, so a hostile or broken upstream cannot park a route
/// for longer than a day.
const MAX_HINT: Duration = Duration::from_secs(86_400);

/// A rate-limit window the upstream reported (requests or tokens).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quota {
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    /// Time until the window resets.
    pub reset: Option<Duration>,
}

/// Everything an upstream response says about limits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hints {
    /// How long to leave the route alone.
    pub retry_after: Option<Duration>,
    pub requests: Quota,
    pub tokens: Quota,
}

impl Hints {
    /// Reads response headers and, for error responses, the JSON body.
    pub fn parse(headers: &HeaderMap, body: Option<&Value>) -> Self {
        let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let number = |name: &str| text(name).and_then(|v| v.trim().parse::<u64>().ok());
        let duration = |name: &str| text(name).and_then(duration);
        let retry_after = text("retry-after-ms")
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|ms| ms.is_finite() && *ms >= 0.0)
            .map(|ms| Duration::from_secs_f64(ms / 1000.0))
            .or_else(|| text("retry-after").and_then(retry_after))
            .or_else(|| body.and_then(body_delay));
        Self {
            retry_after: retry_after.map(|d| d.min(MAX_HINT)),
            requests: Quota {
                limit: number("x-ratelimit-limit-requests"),
                remaining: number("x-ratelimit-remaining-requests"),
                reset: duration("x-ratelimit-reset-requests").map(|d| d.min(MAX_HINT)),
            },
            tokens: Quota {
                limit: number("x-ratelimit-limit-tokens"),
                remaining: number("x-ratelimit-remaining-tokens"),
                reset: duration("x-ratelimit-reset-tokens").map(|d| d.min(MAX_HINT)),
            },
        }
    }

    /// How long a 429 should keep the route out: the explicit retry hint,
    /// else the reset of a window reported as empty.
    pub fn cooldown(&self) -> Option<Duration> {
        self.retry_after.or_else(|| self.exhausted_until())
    }

    /// Time until the window that reported zero remaining resets. A window
    /// with no reset time counts as a minute.
    pub fn exhausted_until(&self) -> Option<Duration> {
        [self.requests, self.tokens]
            .into_iter()
            .filter(|q| q.remaining == Some(0))
            .map(|q| q.reset.unwrap_or(Duration::from_secs(60)))
            .max()
    }
}

/// A `Retry-After` value: delta seconds (possibly fractional) or an HTTP date.
fn retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds));
    }
    let when = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let wait = when.timestamp() - chrono::Utc::now().timestamp();
    Some(Duration::from_secs(u64::try_from(wait).unwrap_or(0)))
}

/// A duration as upstreams write it: plain seconds (`"30"`, `"1.5"`) or
/// Go-style units (`"250ms"`, `"1s"`, `"6m0s"`, `"2m59.56s"`, `"1h2m"`).
pub fn duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(seconds) = value.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds));
    }
    let mut total = 0.0f64;
    let mut rest = value;
    while !rest.is_empty() {
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .filter(|end| *end > 0)?;
        let amount: f64 = rest[..end].parse().ok()?;
        rest = &rest[end..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        total += amount
            * match &rest[..unit_end] {
                "ms" => 0.001,
                "s" => 1.0,
                "m" => 60.0,
                "h" => 3600.0,
                "d" => 86_400.0,
                _ => return None,
            };
        rest = &rest[unit_end..];
    }
    (total.is_finite() && total >= 0.0).then(|| Duration::from_secs_f64(total))
}

/// The retry delay inside an error body: Google's `error.details[].retryDelay`
/// (`"30s"`), else a "try again in 6.5s" / "retry in 28s" sentence in the
/// message.
fn body_delay(body: &Value) -> Option<Duration> {
    let error = body.get("error").unwrap_or(body);
    let from_details = error
        .get("details")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|d| {
            d.get("retryDelay")
                .and_then(Value::as_str)
                .and_then(duration)
        });
    from_details.or_else(|| {
        let message = error.get("message").and_then(Value::as_str)?;
        let lower = message.to_ascii_lowercase();
        ["try again in ", "retry in "]
            .into_iter()
            .find_map(|marker| {
                let start = lower.find(marker)? + marker.len();
                let word: String = message[start..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '.')
                    .collect();
                duration(word.trim_end_matches('.'))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    fn secs(s: f64) -> Option<Duration> {
        Some(Duration::from_secs_f64(s))
    }

    #[test]
    fn durations_in_every_upstream_spelling() {
        assert_eq!(duration("1s"), secs(1.0));
        assert_eq!(duration("6m0s"), secs(360.0));
        assert_eq!(duration("2m59.56s"), secs(179.56));
        assert_eq!(duration("250ms"), secs(0.25));
        assert_eq!(duration("1h2m3s"), secs(3723.0));
        assert_eq!(duration("30"), secs(30.0));
        assert_eq!(duration("1.5"), secs(1.5));
        for bad in ["", "abc", "5x", "s", "1s2", "-3", "NaN", "inf"] {
            assert_eq!(duration(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn retry_after_headers_prefer_milliseconds() {
        let h = headers(&[("retry-after", "30"), ("retry-after-ms", "1500")]);
        assert_eq!(Hints::parse(&h, None).retry_after, secs(1.5));
        let h = headers(&[("retry-after", "30")]);
        assert_eq!(Hints::parse(&h, None).retry_after, secs(30.0));
        let h = headers(&[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")]);
        assert_eq!(Hints::parse(&h, None).retry_after, Some(Duration::ZERO));
        let future = (chrono::Utc::now() + chrono::Duration::seconds(90)).to_rfc2822();
        let h = headers(&[("retry-after", future.as_str())]);
        let wait = Hints::parse(&h, None).retry_after.unwrap();
        assert!((88..=90).contains(&wait.as_secs()), "{wait:?}");
        let h = headers(&[("retry-after", "soon"), ("retry-after-ms", "x")]);
        assert_eq!(Hints::parse(&h, None).retry_after, None);
        let h = headers(&[("retry-after", "99999999")]);
        assert_eq!(Hints::parse(&h, None).retry_after, Some(MAX_HINT));
    }

    #[test]
    fn ratelimit_headers_and_exhausted_windows() {
        let h = headers(&[
            ("x-ratelimit-limit-requests", "30"),
            ("x-ratelimit-remaining-requests", "0"),
            ("x-ratelimit-reset-requests", "2m59.56s"),
            ("x-ratelimit-remaining-tokens", "5000"),
            ("x-ratelimit-reset-tokens", "6m0s"),
        ]);
        let hints = Hints::parse(&h, None);
        assert_eq!(hints.requests.limit, Some(30));
        assert_eq!(hints.requests.remaining, Some(0));
        assert_eq!(hints.tokens.remaining, Some(5000));
        assert_eq!(hints.exhausted_until(), secs(179.56));
        assert_eq!(hints.cooldown(), secs(179.56));
        let explicit = headers(&[
            ("retry-after", "7"),
            ("x-ratelimit-remaining-requests", "0"),
            ("x-ratelimit-reset-requests", "60s"),
        ]);
        assert_eq!(Hints::parse(&explicit, None).cooldown(), secs(7.0));
        assert_eq!(Hints::parse(&HeaderMap::new(), None).cooldown(), None);
        let no_reset = headers(&[("x-ratelimit-remaining-tokens", "0")]);
        assert_eq!(Hints::parse(&no_reset, None).exhausted_until(), secs(60.0));
    }

    #[test]
    fn google_and_message_delays_in_bodies() {
        let google = json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "details": [
            {"@type": "type.googleapis.com/google.rpc.QuotaFailure"},
            {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "30s"}]}});
        assert_eq!(
            Hints::parse(&HeaderMap::new(), Some(&google)).retry_after,
            secs(30.0)
        );
        let groq = json!({"error": {"message": "Rate limit reached. Please try again in 6.5s. Need more tokens?"}});
        assert_eq!(
            Hints::parse(&HeaderMap::new(), Some(&groq)).retry_after,
            secs(6.5)
        );
        let minutes = json!({"error": {"message": "Please try again in 7m12.5s."}});
        assert_eq!(
            Hints::parse(&HeaderMap::new(), Some(&minutes)).retry_after,
            secs(432.5)
        );
        // Headers win over the body.
        let h = headers(&[("retry-after", "2")]);
        assert_eq!(Hints::parse(&h, Some(&google)).retry_after, secs(2.0));
        assert_eq!(
            Hints::parse(&HeaderMap::new(), Some(&json!({"error": "x"}))).retry_after,
            None
        );
    }
}
