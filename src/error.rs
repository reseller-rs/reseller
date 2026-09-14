use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug)]
pub struct Error {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}
pub type Result<T> = std::result::Result<T, Error>;
impl Error {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            code,
            message: message.into(),
        }
    }
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message)
    }
    pub fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(error=%error,"operation failed");
        Self::new(500, "internal_error", "An internal error occurred")
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
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::bad(e.to_string())
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let mut r = (
            self.status,
            Json(json!({"error":{"code":self.code,"message":self.message}})),
        )
            .into_response();
        if self.status == StatusCode::TOO_MANY_REQUESTS {
            r.headers_mut().insert("retry-after", "60".parse().unwrap());
        }
        r
    }
}
