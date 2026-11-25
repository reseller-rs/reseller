//! Streaming upstream proxy for `/v1/*`.
//!
//! JSON bodies are buffered (bounded) so the model and voice policy and the
//! fallbacks can be applied. Every other body, including multipart uploads,
//! streams to the upstream. Responses always stream back unchanged while an
//! [`Inspector`] extracts usage for the [`Meter`].
mod inspect;
mod models;
mod multipart;
mod websocket;

pub use inspect::Inspector;
pub use models::list as model_list;
pub use multipart::Multipart;

use crate::{
    App,
    config::{Config, Policy, Upstream},
    db,
    error::{Error, Result},
    identity::{self, Identity, Scope},
    meter::{Meter, RequestMeta, Usage},
    money,
};
use axum::{
    body::{Body, to_bytes},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, header, request,
    },
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

/// Upstream statuses after which the next fallback model is tried.
const RETRYABLE: [u16; 5] = [400, 402, 404, 408, 429];

/// Which upstream serves a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Llm,
    Tts,
    Stt,
    Image,
}

impl Kind {
    pub fn of(path: &str) -> Self {
        match path {
            "/v1/audio/speech" => Self::Tts,
            "/v1/audio/transcriptions" | "/v1/audio/translations" => Self::Stt,
            "/v1/images" => Self::Image,
            p if p.starts_with("/v1/images/") => Self::Image,
            _ => Self::Llm,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Tts => "tts",
            Self::Stt => "stt",
            Self::Image => "image",
        }
    }

    pub fn upstream(self, config: &Config) -> &Upstream {
        match self {
            Self::Llm => &config.llm,
            Self::Tts => &config.tts,
            Self::Stt => &config.stt,
            Self::Image => &config.image,
        }
    }
}

/// Effective timeout for `upstream`.
pub(crate) fn timeout(config: &Config, upstream: &Upstream) -> Duration {
    Duration::from_secs(if upstream.timeout_seconds > 0 {
        upstream.timeout_seconds
    } else {
        config.upstream_timeout_seconds
    })
}

/// Client headers to forward plus the upstream's configured extra headers.
pub(crate) fn upstream_headers(client: &HeaderMap, upstream: &Upstream) -> HeaderMap {
    let mut headers = forward_headers(client, true);
    for (name, value) in &upstream.headers {
        // Validated when the settings were saved.
        if let (Ok(name), Ok(value)) = (name.parse::<HeaderName>(), HeaderValue::from_str(value)) {
            headers.insert(name, value);
        }
    }
    headers
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

/// Applies `policy` for one JSON field. Returns whether the configured value
/// was written, so the field no longer holds what the client sent.
fn apply_policy(body: &mut Value, key: &str, configured: &str, policy: Policy) -> bool {
    let apply = match policy {
        Policy::Force => true,
        Policy::Default => body.get(key).is_none_or(Value::is_null),
        Policy::Passthrough => false,
    } && !configured.is_empty();
    if apply {
        body[key] = configured.into();
    }
    apply
}

/// Whether a content type names JSON.
fn is_json(content_type: &str) -> bool {
    let mime = content_type.split(';').next().unwrap_or("").trim();
    mime.eq_ignore_ascii_case("application/json") || mime.ends_with("+json")
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
/// `content-length` is dropped. A declared length makes hyper stop polling
/// the relay stream once that many bytes are written, which would drop the
/// meter before end-of-stream usage is recorded. Chunked framing is used
/// instead.
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
    /// Buffered JSON object and its serialized form. The form is re-serialized
    /// when a fallback model is substituted.
    Json(Value, Bytes),
    /// Streamed body. It can be sent only once.
    Stream(Option<reqwest::Body>),
}

/// State shared between the streaming upload (polled by the HTTP client)
/// and the exchange task that owns the meter.
#[derive(Clone, Default)]
struct UploadCounter {
    bytes: Arc<AtomicI64>,
    too_large: Arc<AtomicBool>,
    observed: Arc<Mutex<Observed>>,
}

/// What the upload learned about the request while streaming.
#[derive(Default)]
struct Observed {
    /// Effective model from the multipart body.
    model: Option<String>,
    /// Why the upload was aborted (the client gets this error, not a 502).
    rejection: Option<Error>,
}

impl UploadCounter {
    fn observed(&self) -> std::sync::MutexGuard<'_, Observed> {
        self.observed.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records the abort reason (first one wins).
    fn reject(&self, error: Error) -> std::io::Error {
        let message = error.to_string();
        self.observed().rejection.get_or_insert(error);
        std::io::Error::other(message)
    }
}

/// Proxies one `/v1/*` request.
pub async fn handle(app: App, req: Request<Body>, ip: String) -> Result<Response<Body>> {
    let config = app.config().await;
    let identity =
        identity::authenticate_request(&app, req.headers(), req.extensions(), Scope::Proxy).await?;
    let path = req.uri().path().to_owned();
    if path == "/v1/models" || path.starts_with("/v1/models/") {
        return models::serve(&app, identity.as_ref(), &path).await;
    }
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
        return websocket::relay(app.clone(), req, ip, identity, upstream, &config, url).await;
    }

    let (parts, body) = req.into_parts();
    let counter = UploadCounter::default();
    let prepared = prepare(
        &config,
        kind,
        &path,
        &parts,
        body,
        &counter,
        identity.as_ref(),
    )
    .await?;
    let (model, payload) = (prepared.model, prepared.payload);
    let estimate = match &payload {
        Payload::Json(value, bytes) => inspect::worst_case(
            &path,
            value,
            bytes.len(),
            i64::from(config.billing.default_max_output_tokens),
        ),
        Payload::Stream(_) => Usage::NONE,
    };
    let meter = Meter::begin_with(
        &app,
        identity.clone(),
        RequestMeta {
            method: parts.method.as_str(),
            path: &path,
            kind: kind.as_str(),
            model: &model,
            ip: &ip,
            estimate: &estimate,
        },
        !prepared.checked,
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
                .filter(|m| {
                    identity
                        .as_ref()
                        .is_none_or(|i| config.upstream_allowed(&i.limits, m))
                })
                .cloned(),
        );
    }
    // Quantities known from the request: text-to-speech input characters,
    // and the input-token estimate used if an LLM upstream reports no usage.
    let (characters, estimate) = match &payload {
        Payload::Json(value, bytes) => (
            if path == "/v1/audio/speech" {
                value["input"]
                    .as_str()
                    .map_or(0, |s| i64::try_from(s.chars().count()).unwrap_or(i64::MAX))
            } else {
                0
            },
            matches!(
                path.as_str(),
                "/v1/chat/completions" | "/v1/responses" | "/v1/completions"
            )
            .then(|| inspect::estimate_input_tokens(value, bytes.len())),
        ),
        Payload::Stream(_) => (0, None),
    };

    // The upstream exchange runs on a tracked task, not on the request
    // future: a client that disconnects (even before the response head)
    // cannot cancel it, so usage the upstream already charged us for is
    // always observed and settled.
    let (head_tx, head_rx) = oneshot::channel();
    app.tasks.spawn(
        Exchange {
            app: app.clone(),
            config,
            kind,
            url,
            method: parts.method,
            headers: parts.headers,
            payload,
            counter,
            meter,
            identity,
            balance,
            models,
            characters,
            estimate,
        }
        .run(head_tx),
    );
    head_rx
        .await
        .map_err(|_| Error::internal("upstream exchange ended without a response"))?
}

/// One proxied upstream exchange, owned by its task.
struct Exchange {
    app: App,
    config: Arc<Config>,
    kind: Kind,
    url: url::Url,
    method: Method,
    headers: HeaderMap,
    payload: Payload,
    counter: UploadCounter,
    meter: Meter,
    identity: Option<Identity>,
    balance: Option<i64>,
    /// The requested model followed by its fallbacks.
    models: Vec<String>,
    characters: i64,
    estimate: Option<i64>,
}

/// Chunks buffered between the upstream reader and the client.
const RELAY_BUFFER: usize = 8;

impl Exchange {
    /// Sends the request (trying fallback models), hands the response head
    /// to the waiting handler, then relays and inspects the body until the
    /// upstream finishes, whether or not the client is still listening.
    async fn run(self, head: oneshot::Sender<Result<Response<Body>>>) {
        let Self {
            app,
            config,
            kind,
            url,
            method,
            headers: client_headers,
            mut payload,
            counter,
            mut meter,
            identity,
            balance,
            models,
            characters,
            estimate,
        } = self;
        let upstream = kind.upstream(&config);
        let timeout = timeout(&config, upstream);
        let deadline = tokio::time::Instant::now() + timeout;
        let mut fallback = None;
        let mut index = 0;
        let response = loop {
            let model = &models[index];
            let last = index + 1 == models.len();
            meter.model.clone_from(model);
            let mut request = app
                .client
                .request(method.clone(), url.clone())
                .headers(upstream_headers(&client_headers, upstream))
                .bearer_auth(&upstream.api_key)
                .header(header::ACCEPT_ENCODING, "identity")
                .timeout(timeout);
            request = match &mut payload {
                Payload::Json(value, bytes) => {
                    if index > 0 {
                        value["model"] = model.as_str().into();
                        match serde_json::to_vec(value) {
                            Ok(v) => *bytes = Bytes::from(v),
                            Err(e) => {
                                meter.fail(StatusCode::INTERNAL_SERVER_ERROR, "invalid request");
                                settle(meter).await;
                                let _ = head.send(Err(Error::internal(e)));
                                return;
                            }
                        }
                    }
                    request.body(bytes.clone())
                }
                Payload::Stream(body) => match body.take() {
                    Some(body) => request.body(body),
                    None => request,
                },
            };
            let sent = request.send().await;
            // The upload may have learned the model or aborted itself.
            let (observed_model, rejection) = {
                let mut observed = counter.observed();
                (observed.model.clone(), observed.rejection.take())
            };
            if let Some(model) = observed_model {
                meter.model = model;
            }
            if let Some(error) = rejection {
                meter.bytes_in = counter.bytes.load(Ordering::Relaxed);
                meter.fail(error.status, &error.to_string());
                settle(meter).await;
                let _ = head.send(Err(error));
                return;
            }
            match sent {
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
                    settle(meter).await;
                    let _ = head.send(Err(Error::new(
                        status,
                        code,
                        "Upstream request could not be completed",
                    )));
                    return;
                }
            }
        };

        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        meter.status = status.as_u16();
        meter.success = status.is_success();
        let mut inspector = Inspector::new(&content_type);
        if meter.success {
            inspector = inspector.with_characters(characters);
            if let Some(input) = estimate {
                inspector = inspector.with_estimate(input);
            }
        }

        // Upstream errors are rebuilt in the API's shape: an error page that
        // is not JSON (for example an HTML 502 from a gateway) gets a generic
        // message, and a JSON error keeps only message, type, param and code,
        // so upstream account details (user ids, provider metadata) never
        // reach the customer.
        let failed = !status.is_success() && (status.is_client_error() || status.is_server_error());
        let head_result = response_head(&response, &meter, identity.as_ref(), balance, fallback);
        let mut response = Some(response);
        let (out_status, replacement) = if failed {
            let raw = if is_json(&content_type)
                && let Some(response) = response.take()
            {
                read_capped(response, deadline, UPSTREAM_ERROR_LIMIT).await
            } else {
                None
            };
            let (out_status, body) = upstream_error(status, raw.as_deref());
            (out_status, Some(body))
        } else {
            (status, None)
        };
        let (out_tx, mut out_rx) = mpsc::channel::<std::io::Result<Bytes>>(RELAY_BUFFER);
        let gone = match head_result {
            Ok(mut headers) => {
                if out_status != status {
                    // A rewritten status (for example 429 to 502) must not
                    // keep the upstream's retry hint.
                    headers.remove(header::RETRY_AFTER);
                }
                if replacement.is_some() {
                    headers.remove(header::CONTENT_ENCODING);
                    headers.remove(header::CONTENT_LENGTH);
                    headers.insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                }
                let stream = async_stream::stream! {
                    while let Some(item) = out_rx.recv().await {
                        yield item;
                    }
                };
                let mut out = Response::new(Body::from_stream(stream));
                *out.status_mut() = out_status;
                *out.headers_mut() = headers;
                head.send(Ok(out)).is_err()
            }
            Err(e) => {
                meter.fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "invalid response headers",
                );
                let _ = head.send(Err(e));
                true
            }
        };

        if let Some(body) = replacement {
            let body = Bytes::from(body.to_string());
            meter.bytes_in = counter.bytes.load(Ordering::Relaxed);
            meter.bytes_out = body.len() as i64;
            let _ = out_tx.send(Ok(body)).await;
            settle(meter).await;
            return;
        }
        let Some(response) = response else {
            return;
        };
        let mut gone = gone;
        let mut source = response.bytes_stream();
        loop {
            let chunk = match tokio::time::timeout_at(deadline, source.next()).await {
                Ok(Some(Ok(bytes))) => bytes,
                Ok(None) => break,
                Ok(Some(Err(_))) | Err(_) => {
                    if !gone {
                        meter.fail(StatusCode::BAD_GATEWAY, "upstream stream interrupted");
                        let _ = out_tx
                            .send(Err(std::io::Error::other("upstream stream interrupted")))
                            .await;
                    }
                    break;
                }
            };
            meter.bytes_in = counter.bytes.load(Ordering::Relaxed);
            meter.bytes_out = meter.bytes_out.saturating_add(chunk.len() as i64);
            inspector.feed(&chunk);
            meter.usage = inspector.usage();
            // After the client leaves, keep draining (inspecting, not
            // buffering) so end-of-stream usage is still observed.
            if !gone && out_tx.send(Ok(chunk)).await.is_err() {
                gone = true;
            }
            if gone && meter.error.is_none() {
                meter.status = 499;
                meter.error = Some("client disconnected".into());
            }
        }
        if gone && meter.error.is_none() {
            meter.status = 499;
            meter.error = Some("client disconnected".into());
        }
        meter.bytes_in = counter.bytes.load(Ordering::Relaxed);
        inspector.finish();
        meter.usage = inspector.usage();
        // Settle before closing the body so a client that read to the end
        // can rely on the books being up to date.
        settle(meter).await;
        drop(out_tx);
    }
}

async fn settle(meter: Meter) {
    if let Err(e) = meter.finish().await {
        tracing::error!(error = %e, "request settlement failed");
    }
}

/// Response headers returned to the client.
/// Largest upstream error body read for rewriting.
const UPSTREAM_ERROR_LIMIT: usize = 64 * 1024;

/// Reads at most `limit` bytes of a response body before `deadline`.
/// Returns `None` when the body is larger, fails, or times out.
async fn read_capped(
    response: reqwest::Response,
    deadline: tokio::time::Instant,
    limit: usize,
) -> Option<Bytes> {
    let mut source = response.bytes_stream();
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, source.next()).await {
            Ok(Some(Ok(chunk))) => {
                if out.len() + chunk.len() > limit {
                    return None;
                }
                out.extend_from_slice(&chunk);
            }
            Ok(None) => return Some(Bytes::from(out)),
            Ok(Some(Err(_))) | Err(_) => return None,
        }
    }
}

/// Rebuilds an upstream error as `{"error":{"message","type","param","code"}}`.
///
/// Upstream 401, 402 and 403 concern the operator's upstream account, not
/// the caller's key, so they become 502 with a generic message.
fn upstream_error(status: StatusCode, raw: Option<&[u8]>) -> (StatusCode, Value) {
    let generic = || {
        format!(
            "The upstream provider returned an error (HTTP {})",
            status.as_u16()
        )
    };
    if matches!(status.as_u16(), 401..=403) {
        let error = Error::new(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "The upstream provider rejected the request. Please try again later",
        );
        return (StatusCode::BAD_GATEWAY, error.body());
    }
    let parsed: Option<Value> = raw.and_then(|b| serde_json::from_slice(b).ok());
    let error = parsed.as_ref().map(|v| v.get("error").unwrap_or(v));
    let text = |key: &str| {
        error
            .and_then(|e| e.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(1000).collect::<String>())
    };
    // An upstream `insufficient_quota` (OpenAI sends it as a 429) means the
    // operator's upstream account is out of credit, not the caller's: it
    // must not look like the caller's quota or rate limit.
    if text("code").as_deref() == Some("insufficient_quota")
        || text("type").as_deref() == Some("insufficient_quota")
    {
        let error = Error::new(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "The upstream provider is temporarily unavailable. Please try again later",
        );
        return (StatusCode::BAD_GATEWAY, error.body());
    }
    let message = text("message")
        .or_else(|| error.and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(generic);
    let kind = text("type").unwrap_or_else(|| {
        if status.is_server_error() {
            "server_error"
        } else if status == StatusCode::TOO_MANY_REQUESTS {
            "rate_limit_error"
        } else {
            "invalid_request_error"
        }
        .to_owned()
    });
    let code = match error.and_then(|e| e.get("code")) {
        Some(Value::String(c)) if !c.is_empty() => Value::String(c.chars().take(100).collect()),
        _ => Value::String("upstream_error".into()),
    };
    let param = text("param").map_or(Value::Null, Value::String);
    let body = serde_json::json!({
        "error": {"message": message, "type": kind, "param": param, "code": code}
    });
    (status, body)
}

fn response_head(
    response: &reqwest::Response,
    meter: &Meter,
    identity: Option<&Identity>,
    balance: Option<i64>,
    fallback: Option<String>,
) -> Result<HeaderMap> {
    let mut headers = forward_headers(response.headers(), false);
    let mut set = |name: &'static str, value: &str| -> Result<()> {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_str(value).map_err(Error::internal)?,
        );
        Ok(())
    };
    set("x-request-id", &meter.id)?;
    if let Some(i) = identity {
        set("x-key-prefix", &i.prefix)?;
    }
    if let Some(balance) = balance {
        set("x-account-balance-usd", &money::display(balance))?;
    }
    if let Some(model) = &fallback {
        set("x-model-fallback", model)?;
    }
    Ok(headers)
}

/// A request body ready to send.
struct Prepared {
    /// Effective model (empty when unknown).
    model: String,
    payload: Payload,
    /// The key's allowlist is already enforced, or will be while the upload
    /// streams, so admission skips it.
    checked: bool,
}

/// Reads and rewrites the request body.
async fn prepare(
    config: &Arc<Config>,
    kind: Kind,
    path: &str,
    parts: &request::Parts,
    body: Body,
    counter: &UploadCounter,
    identity: Option<&Identity>,
) -> Result<Prepared> {
    let upstream = kind.upstream(config);
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Everything except multipart uploads and speech-to-text is read as JSON,
    // whatever the Content-Type says, so a mislabelled body cannot skip the
    // model policy, allowlist, or billing. Only an empty body passes through.
    if is_json(content_type) || (kind != Kind::Stt && multipart::boundary(content_type).is_none()) {
        let bytes = to_bytes(body, config.max_json_body_bytes)
            .await
            .map_err(|_| Error::too_large("JSON body exceeds limit"))?;
        counter.bytes.store(bytes.len() as i64, Ordering::Relaxed);
        if bytes.is_empty() && !is_json(content_type) {
            return Ok(Prepared {
                model: String::new(),
                payload: Payload::Stream(Some(reqwest::Body::from(bytes))),
                checked: false,
            });
        }
        let mut value: Value = serde_json::from_slice(&bytes)?;
        if !value.is_object() {
            return Err(Error::bad("JSON body must be an object"));
        }
        let filled = apply_policy(&mut value, "model", &upstream.model, config.model_policy);
        if kind == Kind::Tts {
            apply_policy(&mut value, "voice", &upstream.voice, config.voice_policy);
        }
        let mut model = match value.get("model") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(m)) => m.clone(),
            Some(_) => return Err(Error::bad("model must be a string")),
        };
        // A model the client chose is a public name. The one the policy wrote
        // is already the upstream's.
        let resolved = !filled && !model.is_empty() && !config.models.is_empty();
        if resolved {
            model = models::resolve(config, identity, &model)?
                .upstream_name()
                .to_owned();
            value["model"] = model.clone().into();
        }
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
        return Ok(Prepared {
            model,
            payload: Payload::Json(value, bytes),
            checked: resolved,
        });
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
    // Multipart bodies are always parsed so the client's model is seen,
    // whatever the policy. Only the rewrite itself depends on it.
    let configured = upstream.model.as_str();
    let identity = identity.cloned();
    let mut parser = multipart::boundary(content_type).map(|boundary| {
        let parser = Multipart::new(
            &boundary,
            configured,
            config.model_policy == Policy::Force && !configured.is_empty(),
            config.model_policy != Policy::Passthrough && !configured.is_empty(),
        );
        if config.models.is_empty() {
            return parser;
        }
        let (config, identity) = (Arc::clone(config), identity.clone());
        parser.with_resolver(move |requested| {
            models::resolve(&config, identity.as_ref(), requested)
                .map(|m| m.upstream_name().to_owned())
        })
    });
    // A forced multipart model is known up front, so admission, metering,
    // and allowlists see it.
    let force = parser.is_some() && config.model_policy == Policy::Force && !configured.is_empty();
    let model = if force {
        configured.to_owned()
    } else {
        String::new()
    };
    let checked = parser.is_some() && !force;
    let counter = counter.clone();
    let config = Arc::clone(config);
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
            match &mut parser {
                Some(p) => {
                    let out = match p.push(&chunk) {
                        Ok(out) => out,
                        Err(e) => {
                            yield Err(counter.reject(e));
                            return;
                        }
                    };
                    if let Err(e) = observe_models(p, &counter, &config, identity.as_ref()) {
                        yield Err(e);
                        return;
                    }
                    if !out.is_empty() {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(out));
                    }
                }
                None => yield Ok(chunk),
            }
        }
        if let Some(p) = &mut parser {
            let out = p.finish();
            if counter.observed().model.is_none()
                && identity.as_ref().is_some_and(|i| !i.limits.allowed_models.is_empty())
            {
                yield Err(counter.reject(Error::model_not_found("")));
                return;
            }
            if !out.is_empty() {
                yield Ok(Bytes::from(out));
            }
        }
    };
    Ok(Prepared {
        model,
        payload: Payload::Stream(Some(reqwest::Body::wrap_stream(upload))),
        checked,
    })
}

/// Applies newly completed multipart model parts: records the effective
/// model for metering and enforces the key's allowlist, aborting the upload
/// with a `model_not_found` rejection when it does not pass. Models the
/// catalog resolved already passed it.
fn observe_models(
    parser: &mut Multipart,
    counter: &UploadCounter,
    config: &Config,
    identity: Option<&Identity>,
) -> std::result::Result<(), std::io::Error> {
    for model in parser.take_resolved() {
        counter.observed().model = Some(model);
    }
    for model in parser.take_models() {
        counter.observed().model = Some(model.clone());
        if let Some(identity) = identity
            && let Err(e) = crate::meter::check_model(config, identity, &model)
        {
            return Err(counter.reject(e));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_errors_keep_only_the_openai_fields() {
        let raw = br#"{"error":{"message":"x is not a valid model ID","code":400,"metadata":{"provider_name":"P"}},"user_id":"user_123"}"#;
        let (status, body) = upstream_error(StatusCode::BAD_REQUEST, Some(raw));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            serde_json::json!({"error": {
                "message": "x is not a valid model ID",
                "type": "invalid_request_error",
                "param": null,
                "code": "upstream_error",
            }})
        );
        assert!(!body.to_string().contains("user_"));
    }

    #[test]
    fn upstream_openai_errors_pass_their_fields() {
        let raw = br#"{"error":{"message":"bad","type":"invalid_request_error","param":"messages","code":"invalid_value"}}"#;
        let (_, body) = upstream_error(StatusCode::BAD_REQUEST, Some(raw));
        assert_eq!(body["error"]["param"], "messages");
        assert_eq!(body["error"]["code"], "invalid_value");
    }

    #[test]
    fn upstream_account_failures_become_bad_gateway() {
        for code in [401, 402, 403] {
            let status = StatusCode::from_u16(code).unwrap();
            let (out, body) = upstream_error(
                status,
                Some(br#"{"error":{"message":"No auth credentials found"}}"#),
            );
            assert_eq!(out, StatusCode::BAD_GATEWAY);
            assert_eq!(body["error"]["code"], "upstream_error");
            assert!(!body.to_string().contains("credentials"));
        }
    }

    #[test]
    fn upstream_insufficient_quota_is_the_operators_problem() {
        let raw = br#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#;
        let (out, body) = upstream_error(StatusCode::TOO_MANY_REQUESTS, Some(raw));
        assert_eq!(out, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"]["code"], "upstream_error");
        assert!(!body.to_string().contains("quota"));
        let limited = br#"{"error":{"message":"Rate limit reached","type":"requests","code":"rate_limit_exceeded"}}"#;
        let (out, body) = upstream_error(StatusCode::TOO_MANY_REQUESTS, Some(limited));
        assert_eq!(out, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["error"]["code"], "rate_limit_exceeded");
    }

    #[test]
    fn non_json_upstream_errors_get_a_generic_message() {
        let (out, body) = upstream_error(StatusCode::SERVICE_UNAVAILABLE, None);
        assert_eq!(out, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["type"], "server_error");
        assert!(body["error"]["message"].as_str().unwrap().contains("503"));
    }
}
