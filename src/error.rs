//! API error type rendered as an OpenAI-style
//! `{"error":{"message","type","param","code"}}` JSON body.
use axum::{
    Json,
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::json;

/// An error that is safe to show to API clients.
///
/// `code` is a stable machine-readable identifier, and `message` is human-readable.
/// `param` names the request parameter at fault, when there is one.
/// Internal failures are logged and replaced by a generic message so that
/// database or upstream details never leak to callers.
#[derive(Debug)]
pub struct Error {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub param: Option<&'static str>,
    /// Seconds a client should wait before retrying. Every 429 carries one,
    /// and it becomes the `Retry-After` header.
    pub retry_after: Option<u64>,
    /// An immediate automatic retry cannot succeed (a usage quota, not a
    /// momentary throttle). Sent as `x-should-retry: false`, which the
    /// OpenAI and Anthropic SDKs honor instead of their own backoff.
    pub no_auto_retry: bool,
    /// Methods the path accepts (`Allow` header of 405 responses).
    pub allow: Option<&'static str>,
    /// Replaces the `WWW-Authenticate` header of a 401 response.
    pub challenge: Option<String>,
}

/// Result alias whose error type defaults to [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// Creates an error with an explicit status, code, and message.
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            param: None,
            retry_after: None,
            no_auto_retry: false,
            allow: None,
            challenge: None,
        }
    }

    /// Replaces the machine-readable code.
    #[must_use]
    pub fn with_code(mut self, code: &'static str) -> Self {
        self.code = code;
        self
    }

    /// Names the request parameter at fault.
    #[must_use]
    pub fn with_param(mut self, param: &'static str) -> Self {
        self.param = Some(param);
        self
    }

    /// Sets the `Allow` header of a 405 response.
    #[must_use]
    pub fn with_allow(mut self, allow: &'static str) -> Self {
        self.allow = Some(allow);
        self
    }

    /// Sets the `WWW-Authenticate` header of a 401 response.
    #[must_use]
    pub fn with_challenge(mut self, challenge: String) -> Self {
        self.challenge = Some(challenge);
        self
    }

    /// Sets the `Retry-After` hint in seconds, with a minimum of one.
    #[must_use]
    pub fn retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds.max(1));
        self
    }

    /// 400 `invalid_request`.
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    pub fn unauthorized(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code, message)
    }

    pub fn forbidden(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, message)
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, message)
    }

    /// 429 for a momentary throttle (request rate, concurrency, cooldowns):
    /// retrying after `retry_after` seconds succeeds.
    pub fn too_many(code: &'static str, message: impl Into<String>, retry_after: u64) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, code, message).retry_after(retry_after)
    }

    /// 429 for an exhausted usage quota over a time window (request, token,
    /// or spend windows, monthly allowances). The request succeeds again
    /// only after `retry_after` seconds, so clients must not retry it
    /// automatically right away.
    pub fn quota_window(code: &'static str, message: impl Into<String>, retry_after: u64) -> Self {
        let mut error = Self::too_many(code, message, retry_after);
        error.no_auto_retry = true;
        error
    }

    /// 405 `method_not_allowed` for a path that accepts only `allow`.
    pub fn method_not_allowed(allow: &'static str, message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            message,
        )
        .with_allow(allow)
    }

    /// 404 `model_not_found` for a model that is unknown, disabled, or not
    /// allowed for the caller. The three cases look the same.
    pub fn model_not_found(model: &str) -> Self {
        let message = if model.is_empty() {
            "This key is restricted to specific models. The request must name one".to_owned()
        } else {
            format!("The model '{model}' does not exist or you do not have access to it.")
        };
        Self::not_found("model_not_found", message).with_param("model")
    }

    /// 402 `insufficient_quota`: the account cannot pay for the request.
    /// Waiting does not help, only a top-up or a smaller request does, so
    /// this is not a 429 and carries no `Retry-After`.
    pub fn insufficient_quota(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYMENT_REQUIRED, "insufficient_quota", message)
    }

    pub fn too_large(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", message)
    }

    /// Logs `error` and returns an opaque 500.
    pub fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(error = %error, "operation failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "An internal error occurred",
        )
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::internal(e)
    }
}

/// Maps a JSON parse or shape error from client input to a 400 `invalid_request`.
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::bad(e.to_string())
    }
}

impl Error {
    /// The OpenAI-style `type` of this error, derived from status and code.
    pub fn kind(&self) -> &'static str {
        match self.status.as_u16() {
            402 => "insufficient_quota",
            403 => "permission_error",
            429 => "rate_limit_error",
            500..=599 => "server_error",
            _ => "invalid_request_error",
        }
    }

    /// The JSON response body.
    pub fn body(&self) -> serde_json::Value {
        json!({"error": {
            "message": self.message,
            "type": self.kind(),
            "param": self.param,
            "code": self.code,
        }})
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(self.body())).into_response();
        if let Some(allow) = self.allow {
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static(allow));
        }
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        if self.no_auto_retry {
            response.headers_mut().insert(
                HeaderName::from_static("x-should-retry"),
                HeaderValue::from_static("false"),
            );
        }
        if self.status == StatusCode::UNAUTHORIZED {
            // RFC 6750 section 3: a request without credentials carries no
            // error attribute. Rejected credentials are `invalid_token`.
            let custom = self
                .challenge
                .as_deref()
                .and_then(|c| HeaderValue::from_str(c).ok());
            let challenge = if let Some(custom) = custom {
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, custom);
                return response;
            } else if self.code == "missing_api_key" {
                "Bearer realm=\"reseller\""
            } else {
                "Bearer realm=\"reseller\", error=\"invalid_token\""
            };
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static(challenge),
            );
        }
        response
    }
}
