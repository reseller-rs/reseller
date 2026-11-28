//! What an upstream answer means for routing, read from its status and its
//! error body together.
//!
//! The status alone is not enough. Providers report the same condition with
//! different statuses, and the same status for different conditions:
//!
//! - Groq answers a request that is larger than its per-minute token limit
//!   with `413` and `code: rate_limit_exceeded` ("Limit 7000, Requested
//!   8012"). The request will never fit that route, but it may fit another.
//! - Gemini answers from an unsupported region with `400 FAILED_PRECONDITION`
//!   ("User location is not supported"), and wraps its errors in an array.
//! - OpenAI sends an exhausted account as `429 insufficient_quota`.
//! - OpenRouter sends a moderation refusal as `403`, which is not an account
//!   problem: treating it as one would let one caller disable the provider.
//!
//! Every function here is pure and lenient: an unknown body falls back to the
//! status.
use serde_json::Value;

/// How an attempt ended, as far as routing is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Success,
    /// A momentary limit of the route (requests or tokens per window). The
    /// route cools down until the upstream's hint.
    RateLimited,
    /// The request alone is larger than this route accepts: a per-minute
    /// token limit below the request, a context window, a body size. Another
    /// route may take it. The route's health is not affected.
    TooLarge,
    /// The provider's account cannot serve anything: authentication, credit,
    /// or the region the server runs in. The whole provider cools down.
    ProviderDown,
    /// The route's model is missing or retired at that provider.
    NotFound,
    /// A server error, a timeout, or a connection failure.
    Failure,
    /// The content was refused by the provider's moderation. Another
    /// provider may accept it. The route's health is not affected.
    Refused,
    /// A `400` or `422` that may be about the request or about the route (an
    /// unsupported parameter, OpenRouter's unknown model). The next route is
    /// tried, and the route cools down if a later route serves the request.
    Rejected,
    /// `408`, `409`, `425`: worth another route, says nothing about health.
    Transient,
    /// A client error that no other route would answer differently.
    Final,
}

impl Outcome {
    /// Classifies an answer: `None` for a timeout or connection error, else
    /// the status and the JSON body when one was read.
    pub fn of(status: Option<u16>, body: Option<&Value>) -> Self {
        let Some(status) = status else {
            return Self::Failure;
        };
        if (200..300).contains(&status) {
            return Self::Success;
        }
        let e = Fields::of(body);
        if e.is_any(&["insufficient_quota"]) || e.says(CREDIT) {
            return Self::ProviderDown;
        }
        if e.says(REGION) || e.is_any(REGION_CODES) {
            return Self::ProviderDown;
        }
        if e.says(MODERATION) || e.is_any(MODERATION_CODES) {
            return Self::Refused;
        }
        if status == 429 || e.is_any(RATE_CODES) || e.says(RATE) {
            return if e.exceeds_limit() {
                Self::TooLarge
            } else {
                Self::RateLimited
            };
        }
        if status == 413 || e.is_any(TOO_LARGE_CODES) || e.says(TOO_LARGE) {
            return Self::TooLarge;
        }
        if status == 404 || e.is_any(NOT_FOUND_CODES) || e.says(RETIRED) {
            return Self::NotFound;
        }
        match status {
            401..=403 => Self::ProviderDown,
            400 | 422 => Self::Rejected,
            408 | 409 | 425 => Self::Transient,
            500.. => Self::Failure,
            _ => Self::Final,
        }
    }

    /// Whether the next route is worth trying.
    pub fn retries(self) -> bool {
        !matches!(self, Self::Success | Self::Final)
    }

    /// How useful the failure is as the answer to the client when every
    /// attempt failed: a temporary capacity problem (the client should retry
    /// later) beats a server error, which beats a request that is too large
    /// everywhere, which beats a plain client error. On a tie the later
    /// attempt wins.
    pub fn rank(self) -> u8 {
        match self {
            Self::RateLimited | Self::ProviderDown | Self::NotFound => 3,
            Self::Failure | Self::Transient => 2,
            Self::TooLarge => 1,
            Self::Success | Self::Refused | Self::Rejected | Self::Final => 0,
        }
    }

    /// Whether the failure is about the operator's capacity or catalog, not
    /// about the request: the client gets `503 capacity_exhausted` with a
    /// retry hint instead of the upstream's text, which may name the
    /// operator's upstream account, plan or model.
    pub fn is_capacity(self) -> bool {
        matches!(
            self,
            Self::RateLimited | Self::ProviderDown | Self::NotFound
        )
    }
}

/// Codes and types (lowercase) that mark a rate limit.
const RATE_CODES: &[&str] = &[
    "rate_limit_exceeded",
    "rate_limit_error",
    "rate_limited",
    "too_many_requests",
    "resource_exhausted",
    "tokens",
    "requests",
];
/// Phrases (lowercase) that mark a rate limit.
const RATE: &[&str] = &[
    "rate limit",
    "rate-limit",
    "ratelimit",
    "too many requests",
    "tokens per minute",
    "requests per minute",
    "tokens per day",
    "requests per day",
    "per minute (",
    "per day (",
    "(tpm)",
    "(rpm)",
    "(itpm)",
    "(otpm)",
    "(tpd)",
    "(rpd)",
];
const TOO_LARGE_CODES: &[&str] = &[
    "context_length_exceeded",
    "string_above_max_length",
    "request_too_large",
    "payload_too_large",
    "tokens_exceeded",
];
const TOO_LARGE: &[&str] = &[
    "context length",
    "context window",
    "context_length",
    "maximum context",
    "prompt is too long",
    "input is too long",
    "too many input tokens",
    "request too large",
    "request entity too large",
    "payload too large",
];
const NOT_FOUND_CODES: &[&str] = &["model_not_found", "model_decommissioned", "not_found"];
const RETIRED: &[&str] = &["has been decommissioned", "model is deprecated"];
const CREDIT: &[&str] = &[
    "insufficient credit",
    "insufficient balance",
    "insufficient funds",
    "out of credits",
    "credit balance is too low",
    "exceeded your current quota",
];
const REGION_CODES: &[&str] = &[
    "unsupported_country_region_territory",
    "unsupported_country",
];
const REGION: &[&str] = &[
    "location is not supported",
    "country, region, or territory not supported",
    "not available in your country",
    "not available in your region",
    "region is not supported",
];
const MODERATION_CODES: &[&str] = &[
    "content_filter",
    "content_policy_violation",
    "moderation_blocked",
];
const MODERATION: &[&str] = &[
    "requires moderation",
    "flagged by",
    "was flagged",
    "content management policy",
    "content policy",
];

/// The fields of an error body that classification reads, lowercased.
#[derive(Default)]
struct Fields {
    message: String,
    /// `code`, `type` and Google's `status`.
    codes: Vec<String>,
}

impl Fields {
    fn of(body: Option<&Value>) -> Self {
        let Some(error) = body.map(error_object) else {
            return Self::default();
        };
        let text = |key: &str| match error.get(key) {
            Some(Value::String(s)) => Some(s.trim().to_ascii_lowercase()),
            _ => None,
        };
        let message = text("message")
            .or_else(|| error.as_str().map(str::to_ascii_lowercase))
            .unwrap_or_default();
        let codes = ["code", "type", "status"]
            .into_iter()
            .filter_map(text)
            .collect();
        Self { message, codes }
    }

    fn is_any(&self, codes: &[&str]) -> bool {
        self.codes.iter().any(|c| codes.contains(&c.as_str()))
    }

    fn says(&self, phrases: &[&str]) -> bool {
        phrases.iter().any(|p| self.message.contains(p))
    }

    /// Whether a rate-limit message says the request alone is larger than
    /// the window ("Limit 7000, Requested 8012"), so waiting cannot help.
    fn exceeds_limit(&self) -> bool {
        match (
            number_after(&self.message, "limit"),
            number_after(&self.message, "requested"),
        ) {
            (Some(limit), Some(requested)) => requested > limit,
            _ => false,
        }
    }
}

/// The error object of a body: `{"error": {...}}`, Google's
/// `[{"error": {...}}]`, or the body itself.
pub fn error_object(body: &Value) -> &Value {
    let body = match body {
        Value::Array(items) => items.first().unwrap_or(body),
        _ => body,
    };
    body.get("error").unwrap_or(body)
}

/// The first number that directly follows `word` (and optional `:`, `=` or
/// spaces), with `,` and `_` digit separators allowed.
fn number_after(text: &str, word: &str) -> Option<u64> {
    let mut rest = text;
    while let Some(at) = rest.find(word) {
        let after = &rest[at + word.len()..];
        let trimmed = after.trim_start_matches([' ', ':', '=']);
        let digits: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '_')
            .filter(char::is_ascii_digit)
            .collect();
        if let Ok(n) = digits.parse() {
            return Some(n);
        }
        rest = after;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn of(status: u16, body: Value) -> Outcome {
        Outcome::of(Some(status), Some(&body))
    }

    #[test]
    fn statuses_without_a_body() {
        for (status, outcome) in [
            (Some(200), Outcome::Success),
            (None, Outcome::Failure),
            (Some(429), Outcome::RateLimited),
            (Some(413), Outcome::TooLarge),
            (Some(401), Outcome::ProviderDown),
            (Some(402), Outcome::ProviderDown),
            (Some(403), Outcome::ProviderDown),
            (Some(404), Outcome::NotFound),
            (Some(400), Outcome::Rejected),
            (Some(422), Outcome::Rejected),
            (Some(408), Outcome::Transient),
            (Some(409), Outcome::Transient),
            (Some(500), Outcome::Failure),
            (Some(529), Outcome::Failure),
            (Some(405), Outcome::Final),
            (Some(415), Outcome::Final),
        ] {
            assert_eq!(Outcome::of(status, None), outcome, "{status:?}");
        }
    }

    #[test]
    fn a_request_above_a_token_window_is_too_large_not_rate_limited() {
        let groq = json!({"error": {
            "message": "Request too large for model `qwen/qwen3.8-27b` in organization `org_x` service tier `on_demand` on input tokens per minute (ITPM): Limit 7000, Requested 7503, please reduce your message size and try again. Need more tokens? Upgrade to Dev Tier today at https://console.groq.com/settings/billing",
            "type": "tokens", "code": "rate_limit_exceeded"}});
        assert_eq!(of(413, groq.clone()), Outcome::TooLarge);
        assert_eq!(of(429, groq), Outcome::TooLarge);
        // Within the window, the same error is a momentary limit.
        let busy = json!({"error": {
            "message": "Rate limit reached for model `m` on tokens per minute (TPM): Limit 6000, Used 5000, Requested 2500. Please try again in 15s.",
            "type": "tokens", "code": "rate_limit_exceeded"}});
        assert_eq!(of(429, busy.clone()), Outcome::RateLimited);
        assert_eq!(of(413, busy), Outcome::RateLimited);
        assert_eq!(
            of(
                503,
                json!({"error": {"message": "Too many requests, slow down"}})
            ),
            Outcome::RateLimited
        );
        assert_eq!(
            of(
                429,
                json!([{"error": {"code": 429, "status": "RESOURCE_EXHAUSTED"}}])
            ),
            Outcome::RateLimited
        );
    }

    #[test]
    fn context_and_size_errors_are_too_large() {
        assert_eq!(
            of(
                400,
                json!({"error": {"code": "context_length_exceeded", "message": "This model's maximum context length is 8192 tokens"}})
            ),
            Outcome::TooLarge
        );
        assert_eq!(
            of(
                400,
                json!({"error": {"message": "prompt is too long: 210000 tokens > 200000 maximum"}})
            ),
            Outcome::TooLarge
        );
        assert_eq!(
            of(413, json!("Request Entity Too Large")),
            Outcome::TooLarge
        );
    }

    #[test]
    fn account_and_region_problems_take_the_provider_down() {
        assert_eq!(
            of(
                429,
                json!({"error": {"type": "insufficient_quota", "message": "You exceeded your current quota"}})
            ),
            Outcome::ProviderDown
        );
        assert_eq!(
            of(
                400,
                json!([{"error": {"code": 400, "message": "User location is not supported for the API use.", "status": "FAILED_PRECONDITION"}}])
            ),
            Outcome::ProviderDown
        );
        assert_eq!(
            of(
                403,
                json!({"error": {"code": "unsupported_country_region_territory", "message": "Country, region, or territory not supported"}})
            ),
            Outcome::ProviderDown
        );
        assert_eq!(
            of(
                403,
                json!({"error": {"message": "Access denied. Please check your network settings."}})
            ),
            Outcome::ProviderDown
        );
        assert_eq!(
            of(402, json!({"error": {"message": "Insufficient credits"}})),
            Outcome::ProviderDown
        );
    }

    #[test]
    fn moderation_is_not_an_account_problem() {
        assert_eq!(
            of(
                403,
                json!({"error": {"code": 403, "message": "gpt-x requires moderation on OpenAI. Your input was flagged for \"violence\"."}})
            ),
            Outcome::Refused
        );
        assert_eq!(
            of(
                400,
                json!({"error": {"code": "content_filter", "message": "filtered"}})
            ),
            Outcome::Refused
        );
    }

    #[test]
    fn missing_and_retired_models_are_not_found() {
        assert_eq!(
            of(
                400,
                json!({"error": {"code": "model_decommissioned", "message": "The model `x` has been decommissioned"}})
            ),
            Outcome::NotFound
        );
        assert_eq!(
            of(
                404,
                json!([{"error": {"code": 404, "status": "NOT_FOUND"}}])
            ),
            Outcome::NotFound
        );
    }

    #[test]
    fn numbers_follow_their_words() {
        assert_eq!(
            number_after("limit 7,000, requested 7503", "limit"),
            Some(7000)
        );
        assert_eq!(
            number_after("rate limit reached: limit=12", "limit"),
            Some(12)
        );
        assert_eq!(number_after("no numbers", "limit"), None);
        assert!(
            !Fields {
                message: "limit 10, requested 9".into(),
                codes: vec![]
            }
            .exceeds_limit()
        );
    }
}
