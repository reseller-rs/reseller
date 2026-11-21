//! Model Context Protocol support.
//!
//! - `POST /mcp` ([`handle`]): a stateless Streamable HTTP server. Every POST
//!   carries one JSON-RPC message and is answered with `application/json`.
//!   Tools reuse the existing HTTP handlers (`api::customer`, `proxy::handle`)
//!   through internal requests, so admission, limits, metering, fallbacks and
//!   model policy are identical to the HTTP API.
//! - [`bridge`]: the `reseller mcp` stdio bridge that runs next to an agent,
//!   keeps credentials and proof-of-work local, and forwards to a remote
//!   server.
pub mod bridge;
mod generate;
mod tools;

use crate::{
    App,
    config::Config,
    error::{Error, Result},
    identity::{self, Identity, Scope},
};
use axum::{
    Json,
    body::{Body, to_bytes},
    http::{HeaderValue, Method, Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

pub use tools::requires_api_key;

/// Protocol revisions this server speaks, newest first.
pub const SUPPORTED_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Chooses the protocol version for a client's request: its own when
/// supported, otherwise the newest.
pub fn negotiate(requested: Option<&str>) -> &'static str {
    SUPPORTED_VERSIONS
        .iter()
        .copied()
        .find(|v| Some(*v) == requested)
        .unwrap_or(SUPPORTED_VERSIONS[0])
}

/// Everything a tool needs to know about the caller.
pub(crate) struct Ctx {
    pub app: App,
    pub ip: String,
    /// The presented credential, forwarded to internal requests only.
    pub bearer: Option<String>,
    pub identity: Option<Identity>,
}

/// A failure reported to the model as a tool result with `isError: true`.
#[derive(Debug)]
pub(crate) struct ToolError {
    pub code: String,
    pub message: String,
    pub retry_after: Option<u64>,
}

impl ToolError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retry_after: None,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_arguments", message)
    }

    fn result(&self) -> Value {
        let mut text = format!("{} (error code: {})", self.message, self.code);
        if let Some(wait) = self.retry_after {
            text.push_str(&format!(" Retry after {wait} seconds."));
        }
        json!({
            "content": [{"type": "text", "text": text}],
            "structuredContent": {"error": {
                "code": self.code,
                "message": self.message,
                "retry_after_seconds": self.retry_after,
            }},
            "isError": true,
        })
    }
}

impl From<Error> for ToolError {
    fn from(e: Error) -> Self {
        Self {
            code: e.code.into(),
            message: e.message,
            retry_after: e.retry_after,
        }
    }
}

impl From<sqlx::Error> for ToolError {
    fn from(e: sqlx::Error) -> Self {
        Error::from(e).into()
    }
}

/// Successful tool output: content blocks plus optional structured data.
pub(crate) struct Output {
    pub content: Vec<Value>,
    pub structured: Option<Value>,
}

impl Output {
    /// Structured data, mirrored as text for clients that only show content.
    pub fn json(value: Value) -> Self {
        Self {
            content: vec![json!({"type": "text", "text": value.to_string()})],
            structured: Some(value),
        }
    }

    fn result(self) -> Value {
        let mut out = json!({"content": self.content, "isError": false});
        if let Some(s) = self.structured {
            out["structuredContent"] = s;
        }
        out
    }
}

/// A JSON-RPC error object.
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn rpc_response(
    status: StatusCode,
    id: Value,
    outcome: std::result::Result<Value, RpcError>,
) -> Response {
    let body = match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(e) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": e.code, "message": e.message}})
        }
    };
    (status, Json(body)).into_response()
}

/// `Origin` must be this deployment (or the configured CORS origin); other
/// origins are refused so a web page cannot drive a local or private server
/// through the user's browser (DNS rebinding).
fn origin_allowed(config: &Config, origin: &str) -> bool {
    if config.cors_origin == "*" {
        return true;
    }
    let normalize = |s: &str| {
        url::Url::parse(s)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .map(|u| u.origin().ascii_serialization())
    };
    let Some(origin) = normalize(origin) else {
        return false;
    };
    normalize(&config.public_url).as_deref() == Some(origin.as_str())
        || (!config.cors_origin.is_empty()
            && normalize(&config.cors_origin).as_deref() == Some(origin.as_str()))
}

/// Serves one `/mcp` request.
pub async fn handle(app: &App, req: Request<Body>, ip: &str) -> Result<Response> {
    let config = app.config().await;
    if !config.mcp.enabled {
        return Err(Error::not_found(
            "mcp_disabled",
            "The MCP endpoint is disabled",
        ));
    }
    if req.method() != Method::POST {
        let mut response = (
            StatusCode::METHOD_NOT_ALLOWED,
            Json(json!({"error": {
                "code": "method_not_allowed",
                "message": "The MCP endpoint accepts POST only; there is no server-initiated stream",
            }})),
        )
            .into_response();
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("POST"));
        return Ok(response);
    }
    let headers = req.headers();
    if let Some(origin) = headers.get(header::ORIGIN)
        && !origin
            .to_str()
            .is_ok_and(|origin| origin_allowed(&config, origin))
    {
        return Err(Error::forbidden(
            "origin_not_allowed",
            "Origin is not allowed to use this MCP endpoint",
        ));
    }
    if let Some(version) = headers.get("mcp-protocol-version")
        && !version
            .to_str()
            .is_ok_and(|v| SUPPORTED_VERSIONS.contains(&v))
    {
        return Err(Error::bad(format!(
            "Unsupported MCP-Protocol-Version; supported: {}",
            SUPPORTED_VERSIONS.join(", ")
        )));
    }
    // Invalid credentials fail the whole request (and count against the
    // per-IP auth-failure budget in `web::route`).
    let identity = identity::authenticate(app, headers, Scope::Manage).await?;
    let bearer = identity::token(headers).map(str::to_owned);

    let limit = config.mcp.max_media_bytes.saturating_mul(4) / 3 + 256 * 1024;
    let bytes = to_bytes(req.into_body(), limit)
        .await
        .map_err(|_| Error::too_large("MCP message exceeds the configured size limit"))?;
    let bad = |code, message: &str| {
        rpc_response(
            StatusCode::BAD_REQUEST,
            Value::Null,
            Err(RpcError::new(code, message)),
        )
    };
    let message: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Ok(bad(PARSE_ERROR, "Parse error")),
    };
    if message.is_array() {
        return Ok(bad(
            INVALID_REQUEST,
            "JSON-RPC batches are not supported; send one message per request",
        ));
    }
    if message["jsonrpc"] != "2.0" || !message.is_object() {
        return Ok(bad(INVALID_REQUEST, "Expected a JSON-RPC 2.0 message"));
    }
    let Some(method) = message["method"].as_str() else {
        // A response to a server request (this server sends none).
        return if message.get("result").is_some() || message.get("error").is_some() {
            Ok(StatusCode::ACCEPTED.into_response())
        } else {
            Ok(bad(INVALID_REQUEST, "Missing method"))
        };
    };
    let id = match message.get("id") {
        None => return Ok(StatusCode::ACCEPTED.into_response()),
        Some(id) if id.is_string() || id.is_i64() || id.is_u64() => id.clone(),
        Some(_) => return Ok(bad(INVALID_REQUEST, "id must be a string or an integer")),
    };
    let params = match message.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(p) if p.is_object() => p.clone(),
        Some(_) => {
            return Ok(rpc_response(
                StatusCode::OK,
                id,
                Err(RpcError::new(INVALID_PARAMS, "params must be an object")),
            ));
        }
    };
    let ctx = Ctx {
        app: app.clone(),
        ip: ip.to_owned(),
        bearer,
        identity,
    };
    let outcome = dispatch(&ctx, &config, method, &params).await;
    Ok(rpc_response(StatusCode::OK, id, outcome))
}

async fn dispatch(
    ctx: &Ctx,
    config: &Config,
    method: &str,
    params: &Value,
) -> std::result::Result<Value, RpcError> {
    match method {
        "initialize" => {
            let requested = params["protocolVersion"]
                .as_str()
                .ok_or_else(|| RpcError::new(INVALID_PARAMS, "protocolVersion is required"))?;
            Ok(json!({
                "protocolVersion": negotiate(Some(requested)),
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {
                    "name": env!("CARGO_PKG_NAME"),
                    "title": config.branding.name,
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": tools::instructions(config),
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools::definitions()})),
        "tools/call" => {
            let name = params["name"]
                .as_str()
                .ok_or_else(|| RpcError::new(INVALID_PARAMS, "name is required"))?;
            if !tools::exists(name) {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    format!("Unknown tool: {name}"),
                ));
            }
            let arguments = params.get("arguments").unwrap_or(&Value::Null);
            Ok(match tools::call(ctx, name, arguments).await {
                Ok(output) => output.result(),
                Err(e) => e.result(),
            })
        }
        other => Err(RpcError::new(
            METHOD_NOT_FOUND,
            format!("Method not found: {other}"),
        )),
    }
}

/// Builds an internal request carrying the caller's credential.
pub(crate) fn internal(
    method: Method,
    uri: &str,
    bearer: Option<&str>,
    content_type: Option<&str>,
    body: Body,
) -> std::result::Result<Request<Body>, Error> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    builder.body(body).map_err(Error::internal)
}
