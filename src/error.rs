//! API error type rendered as `{"error":{"code","message"}}` JSON.
use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::json;

/// An error that is safe to show to API clients.
///
/// `code` is a stable machine-readable identifier; `message` is human-readable.
/// Internal failures are logged and replaced by a generic message so that
/// database or upstream details never leak to callers.
#[derive(Debug)]
pub struct Error {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
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

    pub fn too_many(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, code, message)
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

/// JSON errors originate from client input (request bodies, limits objects).
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::bad(e.to_string())
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response();
        if self.status == StatusCode::TOO_MANY_REQUESTS {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
        }
        response
    }
}
