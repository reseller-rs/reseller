//! Streaming upstream proxy for `/v1/*`.
//!
//! JSON bodies are buffered (bounded) so the model/voice policy and fallbacks
//! can be applied; every other body, including multipart uploads, streams to
//! the upstream. Responses always stream back unchanged while an
//! [`Inspector`] extracts usage for the [`Meter`].
mod inspect;
mod multipart;
mod websocket;

pub use inspect::Inspector;
pub use multipart::Multipart;

use crate::{
    App,
    config::{Config, Policy, Upstream},
    db,
    error::{Error, Result},
    identity::{self, Scope},
    meter::{Meter, RequestMeta},
    money,
};
use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode, header, request},
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::Duration,
};

/// Upstream statuses after which the next fallback model is tried.
const RETRYABLE: [u16; 5] = [400, 402, 404, 408, 429];

/// Which upstream serves a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Llm,
    Tts,
    Stt,
}

impl Kind {
    pub fn of(path: &str) -> Self {
        match path {
            "/v1/audio/speech" => Self::Tts,
            "/v1/audio/transcriptions" | "/v1/audio/translations" => Self::Stt,
            _ => Self::Llm,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Tts => "tts",
            Self::Stt => "stt",
        }
    }

    fn upstream(self, config: &Config) -> &Upstream {
        match self {
            Self::Llm => &config.llm,
            Self::Tts => &config.tts,
            Self::Stt => &config.stt,
        }
    }
}

/// Maps `/v1/<suffix>` onto the upstream base URL, rejecting path traversal
/// (including percent-encoded forms) so the base path stays a boundary.
pub fn target(base: &str, path: &str, query: Option<&str>) -> Result<url::Url> {
    let invalid = || Error::bad("Invalid proxy path");
    let suffix = path.strip_prefix("/v1").ok_or_else(invalid)?;
    let decoded = suffix
        .to_ascii_lowercase()
        .replace("%2e", ".")
        .replace("%2f", "/")
        .replace("%5c", "/");
    if suffix.contains('\\') || decoded.split('/').any(|s| s == ".." || s == ".") {
        return Err(invalid());
    }
    let mut url = url::Url::parse(&format!("{}{suffix}", base.trim_end_matches('/')))
        .map_err(Error::internal)?;
    url.set_query(query);
    Ok(url)
}

/// Applies `policy` for one JSON field.
fn apply_policy(body: &mut Value, key: &str, configured: &str, policy: Policy) {
    let apply = match policy {
        Policy::Force => true,
        Policy::Default => body.get(key).is_none_or(Value::is_null),
        Policy::Passthrough => false,
    };
    if apply && !configured.is_empty() {
        body[key] = configured.into();
    }
}

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];
/// Client headers never forwarded upstream: credentials, routing, framing.
const STRIPPED_REQUEST: [&str; 12] = [
    "host",
    "authorization",
    "x-api-key",
    "x-admin-token",
    "cookie",
    "content-length",
    "accept-encoding",
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "cf-connecting-ip",
];
/// Upstream headers never returned to clients.
///
/// `content-length` is dropped because a declared length makes hyper stop
/// polling the relay stream once that many bytes are written, which would
/// drop the meter before end-of-stream usage is recorded; chunked framing
/// is used instead.
const STRIPPED_RESPONSE: [&str; 4] = [
    "set-cookie",
    "access-control-allow-origin",
    "access-control-allow-credentials",
    "content-length",
];

/// Copies end-to-end headers in either direction.
pub(crate) fn forward_headers(input: &HeaderMap, request: bool) -> HeaderMap {
    let listed_in_connection: Vec<String> = input
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();
    let mut out = HeaderMap::new();
    for (name, value) in input {
        let n = name.as_str();
        let drop = HOP_BY_HOP.contains(&n)
            || listed_in_connection.iter().any(|c| c == n)
            || if request {
                STRIPPED_REQUEST.contains(&n) || n.starts_with("sec-websocket-")
            } else {
                STRIPPED_RESPONSE.contains(&n)
            };
        if !drop {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// Request body after policy rewriting.
enum Payload {
    /// Buffered JSON object and its serialized form; re-serialized when a
    /// fallback model is substituted.
    Json(Value, Bytes),
    /// Streamed body; can be sent only once.
    Stream(Option<reqwest::Body>),
}

/// Byte accounting shared with the streaming upload.
#[derive(Clone, Default)]
struct UploadCounter {
    bytes: Arc<AtomicI64>,
    too_large: Arc<AtomicBool>,
}

/// Proxies one `/v1/*` request.
pub async fn handle(app: App, req: Request<Body>, ip: String) -> Result<Response<Body>> {
    let config = app.config().await;
    let identity = identity::authenticate(&app, req.headers(), Scope::Proxy).await?;
    let path = req.uri().path().to_owned();
    let kind = Kind::of(&path);
    let upstream = kind.upstream(&config);
    if upstream.api_key.is_empty() {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "proxy_configuration_error",
            "Upstream API key is not configured",
        ));
    }
    let url = target(&upstream.base_url, &path, req.uri().query())?;
    if req
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        return websocket::relay(
            app.clone(),
            req,
            ip,
            identity,
            upstream,
            config.model_policy,
            url,
        )
        .await;
    }

    let (parts, body) = req.into_parts();
    let counter = UploadCounter::default();
    let (model, mut payload) = prepare(&config, kind, &path, &parts, body, &counter).await?;
    let mut meter = Meter::begin(
        &app,
        identity.clone(),
        RequestMeta {
            method: parts.method.as_str(),
            path: &path,
            kind: kind.as_str(),
            model: &model,
            ip: &ip,
        },
    )
    .await?;
    let balance = match &identity {
        Some(i) => Some(db::balance(&mut *app.db.acquire().await?, &i.account_id).await?),
        None => None,
    };

    // Fallbacks apply only to JSON requests for the configured model, and
    // only to models the key may use.
    let mut models = vec![model.clone()];
    if matches!(payload, Payload::Json(..))
        && !upstream.model.is_empty()
        && model.eq_ignore_ascii_case(&upstream.model)
    {
        models.extend(
            upstream
                .fallbacks
                .iter()
                .filter(|m| identity.as_ref().is_none_or(|i| i.limits.allows(m)))
                .cloned(),
        );
    }

    let timeout = Duration::from_secs(config.upstream_timeout_seconds);
    let mut fallback = None;
    let mut index = 0;
    let response = loop {
        let model = &models[index];
        let last = index + 1 == models.len();
        meter.model.clone_from(model);
        let mut request = app
            .client
            .request(parts.method.clone(), url.clone())
            .headers(forward_headers(&parts.headers, true))
            .bearer_auth(&upstream.api_key)
            .header(header::ACCEPT_ENCODING, "identity")
            .timeout(timeout);
        request = match &mut payload {
            Payload::Json(value, bytes) => {
                if index > 0 {
                    value["model"] = model.as_str().into();
                    *bytes = Bytes::from(serde_json::to_vec(value)?);
                }
                request.body(bytes.clone())
            }
            Payload::Stream(body) => match body.take() {
                Some(body) => request.body(body),
                None => request,
            },
        };
        match request.send().await {
            Ok(response) => {
                let status = response.status();
                if !last && (status.is_server_error() || RETRYABLE.contains(&status.as_u16())) {
                    index += 1;
                    fallback = Some(models[index].clone());
                    continue;
                }
                break response;
            }
            Err(_) if !last => {
                index += 1;
                fallback = Some(models[index].clone());
            }
            Err(e) => {
                meter.bytes_in = counter.bytes.load(Ordering::Relaxed);
                let (status, code, detail) = if counter.too_large.load(Ordering::Relaxed) {
                    (
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request_too_large",
                        "upload exceeds limit",
                    )
                } else if e.is_timeout() {
                    (
                        StatusCode::GATEWAY_TIMEOUT,
                        "upstream_error",
                        "upstream timed out",
                    )
                } else {
                    (
                        StatusCode::BAD_GATEWAY,
                        "upstream_error",
                        "upstream connection failed",
                    )
                };
                meter.fail(status, detail);
                meter.finish().await?;
                return Err(Error::new(
                    status,
                    code,
                    "Upstream request could not be completed",
                ));
            }
        }
    };

    let mut headers = forward_headers(response.headers(), false);
    let mut set = |name: &'static str, value: &str| -> Result<()> {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_str(value).map_err(Error::internal)?,
        );
        Ok(())
    };
    set("x-request-id", &meter.id)?;
    if let Some(i) = &identity {
        set("x-key-prefix", &i.prefix)?;
    }
    if let Some(balance) = balance {
        set("x-account-balance-usd", &money::display(balance))?;
    }
    if let Some(model) = &fallback {
        set("x-model-fallback", model)?;
    }
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    meter.status = status.as_u16();
    let stream = relay_body(response, meter, counter, content_type);
    let mut out = Response::new(Body::from_stream(stream));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    Ok(out)
}

/// Reads and rewrites the request body. Returns the effective model (empty
/// when unknown) and the payload to send.
async fn prepare(
    config: &Config,
    kind: Kind,
    path: &str,
    parts: &request::Parts,
    body: Body,
    counter: &UploadCounter,
) -> Result<(String, Payload)> {
    let upstream = kind.upstream(config);
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mime = content_type.split(';').next().unwrap_or("").trim();
    let is_json = mime.eq_ignore_ascii_case("application/json") || mime.ends_with("+json");

    if is_json {
        let bytes = to_bytes(body, config.max_json_body_bytes)
            .await
            .map_err(|_| Error::too_large("JSON body exceeds limit"))?;
        counter.bytes.store(bytes.len() as i64, Ordering::Relaxed);
        let mut value: Value = serde_json::from_slice(&bytes)?;
        if !value.is_object() {
            return Err(Error::bad("JSON body must be an object"));
        }
        apply_policy(&mut value, "model", &upstream.model, config.model_policy);
        if kind == Kind::Tts {
            apply_policy(&mut value, "voice", &upstream.voice, config.voice_policy);
        }
        let model = match value.get("model") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(m)) => m.clone(),
            Some(_) => return Err(Error::bad("model must be a string")),
        };
        if path == "/v1/chat/completions" && value["stream"] == true {
            // Ask for the final usage chunk so streamed responses can be billed.
            if value.get("stream_options").is_none_or(Value::is_null) {
                value["stream_options"] = serde_json::json!({});
            }
            if !value["stream_options"].is_object() {
                return Err(Error::bad("stream_options must be an object"));
            }
            value["stream_options"]["include_usage"] = true.into();
        }
        let bytes = Bytes::from(serde_json::to_vec(&value)?);
        return Ok((model, Payload::Json(value, bytes)));
    }

    let max = config.max_stream_body_bytes;
    if parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|n| n > max)
    {
        return Err(Error::too_large("Upload exceeds limit"));
    }
    let mut rewriter = multipart::boundary(content_type)
        .filter(|_| config.model_policy != Policy::Passthrough && !upstream.model.is_empty())
        .map(|boundary| {
            Multipart::new(
                &boundary,
                &upstream.model,
                config.model_policy == Policy::Force,
                true,
            )
        });
    // A forced multipart model is known up front, so admission, metering,
    // and allowlists see it.
    let model = if rewriter.is_some() && config.model_policy == Policy::Force {
        upstream.model.clone()
    } else {
        String::new()
    };
    let counter = counter.clone();
    let mut source = body.into_data_stream();
    let upload = async_stream::stream! {
        while let Some(chunk) = source.next().await {
            let chunk = chunk.map_err(std::io::Error::other)?;
            let len = chunk.len() as i64;
            if counter.bytes.fetch_add(len, Ordering::Relaxed) + len > max as i64 {
                counter.too_large.store(true, Ordering::Relaxed);
                yield Err(std::io::Error::other("upload exceeds limit"));
                return;
            }
            match &mut rewriter {
                Some(r) => {
                    let out = r.push(&chunk).map_err(std::io::Error::other)?;
                    if !out.is_empty() {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(out));
                    }
                }
                None => yield Ok(chunk),
            }
        }
        if let Some(r) = &mut rewriter {
            let out = r.finish();
            if !out.is_empty() {
                yield Ok(Bytes::from(out));
            }
        }
    };
    Ok((
        model,
        Payload::Stream(Some(reqwest::Body::wrap_stream(upload))),
    ))
}

/// Streams the upstream body to the client while inspecting usage; the meter
/// settles at end of stream (or on drop if the client disconnects).
fn relay_body(
    response: reqwest::Response,
    mut meter: Meter,
    counter: UploadCounter,
    content_type: String,
) -> impl futures_util::Stream<Item = std::io::Result<Bytes>> {
    async_stream::stream! {
        let mut source = response.bytes_stream();
        let mut inspector = Inspector::new(&content_type);
        while let Some(chunk) = source.next().await {
            meter.bytes_in = counter.bytes.load(Ordering::Relaxed);
            match chunk {
                Ok(bytes) => {
                    meter.bytes_out = meter.bytes_out.saturating_add(bytes.len() as i64);
                    inspector.feed(&bytes);
                    meter.usage.clone_from(&inspector.usage);
                    yield Ok(bytes);
                }
                Err(_) => {
                    meter.fail(StatusCode::BAD_GATEWAY, "upstream stream interrupted");
                    yield Err(std::io::Error::other("upstream stream interrupted"));
                    break;
                }
            }
        }
        inspector.finish();
        if !inspector.usage.is_empty() {
            meter.usage = inspector.usage;
        }
        if let Err(e) = meter.finish().await {
            tracing::error!(error = %e, "request settlement failed");
        }
    }
}
