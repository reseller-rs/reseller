//! Streaming upstream proxy for `/v1/*`.
//!
//! The request names a public model of the catalog. JSON bodies are buffered
//! (bounded) so the model can be resolved, free-tier limits applied and the
//! request replayed on another route. Multipart uploads stream: the prefix up
//! to the `model` part is buffered, the route is chosen from it, and the rest
//! streams to that single route. Responses stream back while an [`Inspector`]
//! extracts usage for the [`Meter`] from the upstream's own bytes. Successful
//! JSON and event-stream bodies are white-labeled on the way out (see
//! `rewrite`); every other body is relayed unchanged.
mod inspect;
mod models;
mod multipart;
pub mod reasoning;
mod rewrite;
mod websocket;

pub use inspect::Inspector;
pub use models::list as model_list;
pub use multipart::Multipart;

use crate::{
    App,
    config::{Capability, Config, Model, Provider, Tier},
    db,
    error::{Error, Result},
    identity::{self, Caller, Identity},
    meter::{Meter, RequestMeta, Usage},
    money,
    router::{
        Permit,
        free::{self, Admission},
        hints::Hints,
        outcome::{self, Outcome},
    },
};
use axum::{
    body::{Body, BodyDataStream, to_bytes},
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
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};

/// Which kind of endpoint serves a path.
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

    /// The capability a model needs to serve this kind of endpoint.
    pub fn capability(self) -> Capability {
        match self {
            Self::Llm => Capability::Chat,
            Self::Tts => Capability::Speech,
            Self::Stt => Capability::Transcription,
            Self::Image => Capability::Image,
        }
    }
}

/// Client headers to forward plus the provider's configured extra headers.
pub(crate) fn upstream_headers(client: &HeaderMap, provider: &Provider) -> HeaderMap {
    let mut headers = forward_headers(client, true);
    for (name, value) in &provider.headers {
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

/// Whether an upstream response header describes the operator's upstream
/// account: its rate limits and retry hints (the router reads them, the
/// client gets its own) or its organization and project.
fn upstream_private(name: &str) -> bool {
    matches!(
        name,
        "retry-after" | "retry-after-ms" | "openai-organization" | "openai-project"
    ) || name.starts_with("x-ratelimit-")
        || name.starts_with("ratelimit")
}

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
                STRIPPED_RESPONSE.contains(&n) || upstream_private(n)
            };
        if !drop {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// The body of a request while it waits for a route.
enum Payload {
    /// Buffered JSON object and its serialized form. The model (and the
    /// reasoning effort) is rewritten for each attempt.
    Json(Value, Bytes),
    /// Streamed body. It can be sent only once.
    Stream(Option<Upload>),
}

/// A streamed request body.
enum Upload {
    /// Forwarded unchanged.
    Raw(reqwest::Body),
    /// A multipart form whose `model` part is rewritten to the route's model.
    Multipart(Box<PendingUpload>),
}

/// State shared between the streaming upload (polled by the HTTP client)
/// and the exchange task that owns the meter.
#[derive(Clone, Default)]
struct UploadCounter {
    bytes: Arc<AtomicI64>,
    too_large: Arc<AtomicBool>,
    rejection: Arc<Mutex<Option<Error>>>,
}

impl UploadCounter {
    /// Takes the reason the upload was aborted (the client gets this error,
    /// not a 502).
    fn take_rejection(&self) -> Option<Error> {
        self.rejection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Records the abort reason (first one wins).
    fn reject(&self, error: Error) -> std::io::Error {
        let message = error.to_string();
        self.rejection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(error);
        std::io::Error::other(message)
    }

    /// Counts `len` more uploaded bytes. False when that exceeds `max`.
    fn add(&self, len: usize, max: usize) -> bool {
        let len = i64::try_from(len).unwrap_or(i64::MAX);
        let over = self
            .bytes
            .fetch_add(len, Ordering::Relaxed)
            .saturating_add(len)
            > i64::try_from(max).unwrap_or(i64::MAX);
        if over {
            self.too_large.store(true, Ordering::Relaxed);
        }
        !over
    }
}

/// Proxies one `/v1/*` request.
pub async fn handle(app: App, req: Request<Body>, ip: String) -> Result<Response<Body>> {
    let config = app.config().await;
    let caller = identity::caller(&app, req.headers(), req.extensions()).await?;
    // With the free tier off, a request without a key has nothing to use.
    if matches!(caller, Caller::Anonymous) && !config.free.enabled {
        return Err(identity::missing_credential());
    }
    let path = req.uri().path().to_owned();
    if path == "/v1/models" || path.starts_with("/v1/models/") {
        return models::serve(&app, &caller, &path).await;
    }
    let kind = Kind::of(&path);
    if req
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        return websocket::relay(app.clone(), req, ip, caller, config).await;
    }

    let query = req.uri().query().map(str::to_owned);
    let (parts, body) = req.into_parts();
    let counter = UploadCounter::default();
    let prepared = prepare(&config, kind, &path, &parts, body, &counter, &caller).await?;
    let model = &config.models[prepared.model];
    let mut payload = prepared.payload;
    // Reasoning effort: validated for the model's first route here (a 400
    // before anything is admitted), and mapped again for the model of each
    // attempt.
    let mut effort = Effort::default();
    if reasoning::applies(&path)
        && let Payload::Json(value, _) = &mut payload
    {
        effort.original = reasoning::requested(value)?;
        effort.free = model.tier == Tier::Free;
        effort.path = Some(path.clone());
        let mut probe = value.clone();
        reasoning::apply(
            &mut probe,
            &path,
            &config,
            &model.routes[0].model,
            effort.original.as_ref(),
            effort.free,
            true,
        )?;
    }
    let estimate = match &payload {
        Payload::Json(value, bytes) => inspect::worst_case(
            &path,
            value,
            bytes.len(),
            i64::from(config.billing.default_max_output_tokens),
        ),
        Payload::Stream(_) => Usage::NONE,
    };
    // Free-tier admission is in memory and comes before any database write.
    let free = free_admission(&app, &config, &caller, model, &ip).await?;
    let identity = caller.identity().cloned();
    let route_models: Vec<&str> = model.routes.iter().map(|r| r.model.as_str()).collect();
    let meter = Meter::begin(
        &app,
        identity.clone(),
        RequestMeta {
            method: parts.method.as_str(),
            path: &path,
            kind: kind.as_str(),
            model: &model.id,
            tier: model.tier,
            routes: &route_models,
            ip: &ip,
            estimate: &estimate,
        },
    )
    .await
    .map_err(|e| rate_headers(e, free.as_ref()))?;
    let balance = match &identity {
        Some(i) => Some(db::balance(&mut *app.db.acquire().await?, &i.account_id).await?),
        None => None,
    };

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
            config: Arc::clone(&config),
            model: prepared.model,
            path,
            query,
            method: parts.method,
            headers: parts.headers,
            payload,
            counter,
            meter,
            identity,
            balance,
            characters,
            estimate,
            effort,
            free,
        }
        .run(head_tx),
    );
    head_rx
        .await
        .map_err(|_| Error::internal("upstream exchange ended without a response"))?
}

/// Free-tier admission of a request to `model`, or `None` when the request is
/// not subject to it (a paid model, or the operator).
///
/// The client is charged by address, except for a key of a paying or trusted
/// account, which has a budget of its own.
async fn free_admission(
    app: &App,
    config: &Config,
    caller: &Caller,
    model: &Model,
    ip: &str,
) -> Result<Option<Admission>> {
    if model.tier != Tier::Free || matches!(caller, Caller::Operator) {
        return Ok(None);
    }
    let subject = match caller {
        Caller::Key(identity)
            if crate::billing::is_paying(&mut *app.db.acquire().await?, &identity.account_id)
                .await? =>
        {
            format!("acct:{}", identity.account_id)
        }
        _ => free::ip_subject(ip, config.free.ipv6_prefix),
    };
    app.router.free.admit(config, &subject).map(Some)
}

/// 503 `capacity_exhausted`: no route can take the request for `wait`.
fn unavailable(wait: Duration) -> Error {
    Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "capacity_exhausted",
        "No upstream capacity is available for this model right now. Retry shortly",
    )
    .retry_after_ms(u64::try_from(wait.as_millis()).unwrap_or(u64::MAX))
}

/// Adds the free-tier rate limit headers to an error.
fn rate_headers(mut error: Error, free: Option<&Admission>) -> Error {
    if let Some(free) = free {
        for (name, value) in free.rate.headers() {
            error = error.with_header(name, value);
        }
    }
    error
}

/// One proxied upstream exchange, owned by its task.
struct Exchange {
    app: App,
    config: Arc<Config>,
    /// Index of the model in `config.models`.
    model: usize,
    path: String,
    query: Option<String>,
    method: Method,
    headers: HeaderMap,
    payload: Payload,
    counter: UploadCounter,
    meter: Meter,
    identity: Option<Identity>,
    balance: Option<i64>,
    characters: i64,
    estimate: Option<i64>,
    effort: Effort,
    /// The free-tier slot of the request. Dropping it frees the client's
    /// concurrency.
    free: Option<Admission>,
}

/// The reasoning effort of a request, kept to apply the policy again for the
/// model of each attempt.
#[derive(Default)]
struct Effort {
    /// The route, when it carries a reasoning level.
    path: Option<String>,
    original: Option<reasoning::Requested>,
    /// Requests to free models are capped at `reasoning.free_effort`.
    free: bool,
}

/// An attempt that did not produce a response to stream.
enum Failed {
    /// The upstream answered with an error status.
    Http {
        status: StatusCode,
        outcome: Outcome,
        /// The body, when it was JSON and small enough to read.
        raw: Option<Bytes>,
        headers: HeaderMap,
    },
    /// No response head arrived.
    Transport { timeout: bool },
}

impl Failed {
    fn rank(&self) -> u8 {
        match self {
            Self::Http { outcome, .. } => outcome.rank(),
            Self::Transport { .. } => Outcome::Failure.rank(),
        }
    }

    /// Keeps the failure that best explains the request to the client: the
    /// higher [`Outcome::rank`], the later one on a tie.
    fn keep(slot: &mut Option<Self>, new: Self) {
        if slot.as_ref().is_none_or(|old| new.rank() >= old.rank()) {
            *slot = Some(new);
        }
    }
}

/// How the attempts ended.
enum Finished {
    /// A response to relay, with the permit of its route and the deadline of
    /// its provider.
    Stream(reqwest::Response, Permit, tokio::time::Instant),
    Failed(Failed),
    /// No route could take the request. The wait is how long until one can.
    Unavailable(Duration),
}

/// Chunks buffered between the upstream reader and the client.
const RELAY_BUFFER: usize = 8;

impl Exchange {
    /// Picks routes and sends the request (trying the next route after a
    /// retryable failure), hands the response head to the waiting handler,
    /// then relays and inspects the body until the upstream finishes, whether
    /// or not the client is still listening.
    async fn run(self, head: oneshot::Sender<Result<Response<Body>>>) {
        let Self {
            app,
            config,
            model,
            path,
            query,
            method,
            headers: client_headers,
            mut payload,
            counter,
            mut meter,
            identity,
            balance,
            characters,
            estimate,
            effort,
            free,
        } = self;
        let model = &config.models[model];
        let replayable = matches!(payload, Payload::Json(..));
        let max_attempts = if replayable {
            usize::try_from(config.routing.max_attempts).unwrap_or(1)
        } else {
            1
        };
        let mut tried: Vec<usize> = Vec::new();
        // Fallback routes get their own attempts once the others' are spent.
        let fallbacks = if replayable {
            model.routes.iter().filter(|r| r.fallback).count()
        } else {
            0
        };
        let (mut primary_tries, mut fallback_tries) = (0usize, 0usize);
        let mut failed: Option<Failed> = None;
        // Routes that refused this request with 400. When a later route
        // serves it, the refusals were about those routes.
        let mut refused: Vec<usize> = Vec::new();
        let finished = loop {
            let picked =
                match app
                    .router
                    .pick(&config, model, &tried, primary_tries >= max_attempts)
                {
                    Ok(picked) => picked,
                    Err(wait) => {
                        break match failed.take() {
                            Some(f) => Finished::Failed(f),
                            None => Finished::Unavailable(wait),
                        };
                    }
                };
            tried.push(picked.index);
            let route = &model.routes[picked.index];
            if route.fallback {
                fallback_tries += 1;
            } else {
                primary_tries += 1;
            }
            let can_retry = primary_tries < max_attempts || fallback_tries < fallbacks;
            let mut permit = picked.permit;
            let Some(provider) = config.provider(&route.provider) else {
                break Finished::Unavailable(Duration::from_secs(1));
            };
            let url = match target(&provider.base_url, &path, query.as_deref()) {
                Ok(url) => url,
                Err(e) => {
                    meter.fail(e.status, "invalid proxy path");
                    settle(meter).await;
                    let _ = head.send(Err(e));
                    return;
                }
            };
            let timeout = provider.timeout(config.upstream_timeout_seconds);
            let deadline = tokio::time::Instant::now() + timeout;
            let mut request = app
                .client
                .request(method.clone(), url)
                .headers(upstream_headers(&client_headers, provider))
                .header(header::ACCEPT_ENCODING, "identity")
                .timeout(timeout);
            // A provider without a key (a local server) gets no credential.
            if !provider.api_key.is_empty() {
                request = request.bearer_auth(&provider.api_key);
            }
            request = match &mut payload {
                Payload::Json(value, bytes) => {
                    value["model"] = route.model.as_str().into();
                    if let Some(path) = &effort.path {
                        // Lenient: a level the model lacks maps down.
                        let _ = reasoning::apply(
                            value,
                            path,
                            &config,
                            &route.model,
                            effort.original.as_ref(),
                            effort.free,
                            false,
                        );
                    }
                    match serde_json::to_vec(value) {
                        Ok(v) => *bytes = Bytes::from(v),
                        Err(e) => {
                            meter.fail(StatusCode::INTERNAL_SERVER_ERROR, "invalid request");
                            settle(meter).await;
                            let _ = head.send(Err(Error::internal(e)));
                            return;
                        }
                    }
                    request.body(bytes.clone())
                }
                Payload::Stream(upload) => match upload.take() {
                    Some(Upload::Raw(body)) => request.body(body),
                    Some(Upload::Multipart(pending)) => request.body((*pending).into_body(
                        &route.model,
                        counter.clone(),
                        config.max_stream_body_bytes,
                    )),
                    None => request,
                },
            };
            let started = Instant::now();
            let sent = request.send().await;
            let latency = started.elapsed();
            // The upload may have aborted itself.
            if let Some(error) = counter.take_rejection() {
                permit.finish(Outcome::Failure, &Hints::default());
                meter.attempt(&route.provider, &route.model, 0, latency);
                meter.bytes_in = counter.bytes.load(Ordering::Relaxed);
                meter.fail(error.status, &error.to_string());
                settle(meter).await;
                let _ = head.send(Err(rate_headers(error, free.as_ref())));
                return;
            }
            match sent {
                Ok(response) => {
                    let status = response.status();
                    let errored = status.is_client_error() || status.is_server_error();
                    if !errored {
                        permit.finish(Outcome::Success, &Hints::parse(response.headers(), None));
                        meter.attempt(&route.provider, &route.model, status.as_u16(), latency);
                        for index in refused.drain(..) {
                            let other = &model.routes[index];
                            app.router.refused_where_others_served(
                                &config.routing,
                                &other.provider,
                                &other.model,
                            );
                        }
                        break Finished::Stream(response, permit, deadline);
                    }
                    let response_headers = response.headers().clone();
                    let content_type = response_headers
                        .get(header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let raw = if is_json(content_type) {
                        read_capped(response, deadline, UPSTREAM_ERROR_LIMIT).await
                    } else {
                        None
                    };
                    let parsed: Option<Value> =
                        raw.as_deref().and_then(|b| serde_json::from_slice(b).ok());
                    let outcome = Outcome::of(Some(status.as_u16()), parsed.as_ref());
                    let hints = Hints::parse(&response_headers, parsed.as_ref());
                    permit.finish(outcome, &hints);
                    meter.attempt(&route.provider, &route.model, status.as_u16(), latency);
                    if outcome != Outcome::Success {
                        tracing::info!(
                            provider = %route.provider,
                            model = %route.model,
                            status = status.as_u16(),
                            ?outcome,
                            "upstream attempt failed"
                        );
                    }
                    let error = Failed::Http {
                        status,
                        outcome,
                        raw,
                        headers: response_headers,
                    };
                    if outcome == Outcome::Final {
                        // No route would answer it differently.
                        break Finished::Failed(error);
                    }
                    if outcome == Outcome::Rejected {
                        refused.push(picked.index);
                    }
                    Failed::keep(&mut failed, error);
                    if outcome.retries() && can_retry {
                        continue;
                    }
                    break Finished::Failed(failed.take().expect("a failure was kept"));
                }
                Err(e) => {
                    permit.finish(Outcome::Failure, &Hints::default());
                    meter.attempt(&route.provider, &route.model, 0, latency);
                    let error = Failed::Transport {
                        timeout: e.is_timeout(),
                    };
                    if counter.too_large.load(Ordering::Relaxed) {
                        break Finished::Failed(error);
                    }
                    Failed::keep(&mut failed, error);
                    if can_retry {
                        continue;
                    }
                    break Finished::Failed(failed.take().expect("a failure was kept"));
                }
            }
        };

        let (response, permit, deadline) = match finished {
            Finished::Stream(response, permit, deadline) => (response, permit, deadline),
            Finished::Unavailable(wait) => {
                meter.fail(StatusCode::SERVICE_UNAVAILABLE, "no route available");
                settle(meter).await;
                let _ = head.send(Err(rate_headers(unavailable(wait), free.as_ref())));
                return;
            }
            Finished::Failed(Failed::Transport { timeout }) => {
                meter.bytes_in = counter.bytes.load(Ordering::Relaxed);
                let (status, code, detail) = if counter.too_large.load(Ordering::Relaxed) {
                    (
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request_too_large",
                        "upload exceeds limit",
                    )
                } else if timeout {
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
                let _ = head.send(Err(rate_headers(
                    Error::new(status, code, "Upstream request could not be completed"),
                    free.as_ref(),
                )));
                return;
            }
            Finished::Failed(Failed::Http { outcome, .. }) if outcome.is_capacity() => {
                // Rate limits, unusable provider accounts and missing models
                // are the operator's capacity, not the caller's mistake: the
                // client gets a retry hint, never the upstream's text (which
                // may name the operator's account, plan, or upstream model).
                meter.bytes_in = counter.bytes.load(Ordering::Relaxed);
                meter.fail(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "upstream capacity exhausted",
                );
                settle(meter).await;
                let wait = app.router.soonest(&config, model);
                let _ = head.send(Err(rate_headers(unavailable(wait), free.as_ref())));
                return;
            }
            Finished::Failed(Failed::Http {
                outcome: Outcome::TooLarge,
                ..
            }) => {
                meter.bytes_in = counter.bytes.load(Ordering::Relaxed);
                meter.fail(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request too large for every route",
                );
                settle(meter).await;
                let _ = head.send(Err(rate_headers(
                    Error::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request_too_large",
                        "The request is larger than this model accepts right now. \
                         Shorten the messages or lower max_tokens and try again",
                    ),
                    free.as_ref(),
                )));
                return;
            }
            Finished::Failed(Failed::Http {
                status,
                outcome,
                raw,
                headers,
            }) => {
                // A moderation refusal is about the content, even when the
                // upstream sent it as 403.
                let status = if outcome == Outcome::Refused && status.as_u16() != 400 {
                    StatusCode::BAD_REQUEST
                } else {
                    status
                };
                // Upstream errors are rebuilt in the API's shape: an error
                // page that is not JSON (for example an HTML 502 from a
                // gateway) gets a generic message, and a JSON error keeps only
                // message, type, param and code, so upstream account details
                // never reach the customer.
                meter.status = status.as_u16();
                meter.success = false;
                let body = upstream_error(status, raw.as_deref());
                let mut out_headers =
                    response_head(&headers, &meter, identity.as_ref(), balance, free.as_ref());
                if let Ok(h) = &mut out_headers {
                    h.remove(header::CONTENT_ENCODING);
                    h.remove(header::CONTENT_LENGTH);
                    h.insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                }
                let body = Bytes::from(body.to_string());
                meter.bytes_in = counter.bytes.load(Ordering::Relaxed);
                meter.bytes_out = body.len() as i64;
                match out_headers {
                    Ok(h) => {
                        let mut out = Response::new(Body::from(body));
                        *out.status_mut() = status;
                        *out.headers_mut() = h;
                        settle(meter).await;
                        let _ = head.send(Ok(out));
                    }
                    Err(e) => {
                        meter.fail(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "invalid response headers",
                        );
                        settle(meter).await;
                        let _ = head.send(Err(e));
                    }
                }
                return;
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
        // Successful JSON and event streams name the public model, not the
        // upstream's, and carry no provider metadata.
        let mut rewriter = if meter.success {
            rewrite::Rewriter::new(
                &content_type,
                response
                    .headers()
                    .get(header::CONTENT_ENCODING)
                    .and_then(|v| v.to_str().ok()),
                &model.id,
                config.max_json_body_bytes,
            )
        } else {
            rewrite::Rewriter::Passthrough
        };
        if meter.success {
            inspector = inspector.with_characters(characters);
            if let Some(input) = estimate {
                inspector = inspector.with_estimate(input);
            }
        }
        let head_result = response_head(
            response.headers(),
            &meter,
            identity.as_ref(),
            balance,
            free.as_ref(),
        );
        let (out_tx, mut out_rx) = mpsc::channel::<std::io::Result<Bytes>>(RELAY_BUFFER);
        let gone = match head_result {
            Ok(headers) => {
                let stream = async_stream::stream! {
                    while let Some(item) = out_rx.recv().await {
                        yield item;
                    }
                };
                let mut out = Response::new(Body::from_stream(stream));
                *out.status_mut() = status;
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
            inspector.feed(&chunk);
            meter.usage = inspector.usage();
            // Metering saw the upstream's bytes; the client gets the
            // rewritten ones, which are what bytes_out counts.
            let chunk = rewriter.feed(chunk);
            meter.bytes_out = meter.bytes_out.saturating_add(chunk.len() as i64);
            // After the client leaves, keep draining (inspecting, not
            // buffering) so end-of-stream usage is still observed.
            if !gone && !chunk.is_empty() && out_tx.send(Ok(chunk)).await.is_err() {
                gone = true;
            }
            if gone && meter.error.is_none() {
                meter.status = 499;
                meter.error = Some("client disconnected".into());
            }
        }
        // Whatever the rewriter held back (a buffered JSON body, a last
        // unterminated line) goes out unless the stream broke.
        if meter.error.is_none() {
            let rest = rewriter.finish();
            meter.bytes_out = meter.bytes_out.saturating_add(rest.len() as i64);
            if !gone && !rest.is_empty() && out_tx.send(Ok(rest)).await.is_err() {
                gone = true;
            }
        }
        if gone && meter.error.is_none() {
            meter.status = 499;
            meter.error = Some("client disconnected".into());
        }
        meter.bytes_in = counter.bytes.load(Ordering::Relaxed);
        inspector.finish();
        meter.usage = inspector.usage();
        // Token budgets are charged once the usage is known.
        let tokens = u64::try_from(meter.usage.total).unwrap_or(0);
        permit.charge_tokens(tokens);
        if let Some(free) = &free {
            free.guard.charge_tokens(tokens);
        }
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
/// Only errors about the request reach here: capacity, account and catalog
/// failures are answered by the proxy itself (see [`Outcome::is_capacity`]).
fn upstream_error(status: StatusCode, raw: Option<&[u8]>) -> Value {
    let generic = || {
        format!(
            "The upstream provider returned an error (HTTP {})",
            status.as_u16()
        )
    };
    let parsed: Option<Value> = raw.and_then(|b| serde_json::from_slice(b).ok());
    let error = parsed.as_ref().map(outcome::error_object);
    let text = |key: &str| {
        error
            .and_then(|e| e.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(1000).collect::<String>())
    };
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
    serde_json::json!({
        "error": {"message": message, "type": kind, "param": param, "code": code}
    })
}

/// Response headers returned to the client: the upstream's end-to-end
/// headers, the request id, the key prefix and balance, and (for free
/// requests) the rate limit state.
fn response_head(
    upstream: &HeaderMap,
    meter: &Meter,
    identity: Option<&Identity>,
    balance: Option<i64>,
    free: Option<&Admission>,
) -> Result<HeaderMap> {
    let mut headers = forward_headers(upstream, false);
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
    if let Some(free) = free {
        for (name, value) in free.rate.headers() {
            headers.insert(name, value);
        }
    }
    Ok(headers)
}

/// A request body ready to send.
struct Prepared {
    /// Index of the resolved model in `config.models`.
    model: usize,
    payload: Payload,
}

fn index_of(config: &Config, model: &Model) -> usize {
    config
        .models
        .iter()
        .position(|m| std::ptr::eq(m, model))
        .expect("the model comes from the catalog")
}

/// Caps every output token limit of a free request at `max`, and sets one
/// when the request has none. Chat, completions and responses requests only.
fn clamp_output_tokens(body: &mut Value, path: &str, max: u32) {
    let field = match path {
        "/v1/chat/completions" | "/v1/completions" => "max_tokens",
        "/v1/responses" => "max_output_tokens",
        _ => return,
    };
    let max = i64::from(max);
    let mut present = false;
    for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        if let Some(n) = body.get(key).filter(|v| !v.is_null()) {
            present = true;
            if n.as_i64().is_none_or(|n| n > max) {
                body[key] = max.into();
            }
        }
    }
    if !present {
        body[field] = max.into();
    }
}

/// Reads the request body and resolves its model.
async fn prepare(
    config: &Arc<Config>,
    kind: Kind,
    path: &str,
    parts: &request::Parts,
    body: Body,
    counter: &UploadCounter,
    caller: &Caller,
) -> Result<Prepared> {
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let capability = kind.capability();
    // The free-tier rules do not apply to the operator.
    let limited = !matches!(caller, Caller::Operator);

    // Everything except multipart uploads and speech-to-text is read as JSON,
    // whatever the Content-Type says, so a mislabelled body cannot skip model
    // resolution, allowlists, or billing. Only an empty body passes through.
    if is_json(content_type) || (kind != Kind::Stt && multipart::boundary(content_type).is_none()) {
        // Anonymous callers can only use free models: bound the read by the
        // free body limit.
        let read_limit = match caller {
            Caller::Anonymous if config.free.max_body_bytes > 0 => {
                config.max_json_body_bytes.min(config.free.max_body_bytes)
            }
            _ => config.max_json_body_bytes,
        };
        let bytes = to_bytes(body, read_limit)
            .await
            .map_err(|_| Error::too_large("JSON body exceeds limit"))?;
        counter.bytes.store(bytes.len() as i64, Ordering::Relaxed);
        if bytes.is_empty() && !is_json(content_type) {
            let model = models::resolve(config, caller, capability, None)?;
            return Ok(Prepared {
                model: index_of(config, model),
                payload: Payload::Stream(Some(Upload::Raw(reqwest::Body::from(bytes)))),
            });
        }
        let mut value: Value = serde_json::from_slice(&bytes)?;
        if !value.is_object() {
            return Err(Error::bad("JSON body must be an object"));
        }
        let requested = match value.get("model") {
            None | Some(Value::Null) => None,
            Some(Value::String(m)) => Some(m.clone()),
            Some(_) => return Err(Error::bad("model must be a string")),
        };
        let model = models::resolve(config, caller, capability, requested.as_deref())?;
        let free = model.tier == Tier::Free && limited;
        if free && config.free.max_body_bytes > 0 && bytes.len() > config.free.max_body_bytes {
            return Err(Error::too_large(format!(
                "Free requests accept bodies of at most {} bytes",
                config.free.max_body_bytes
            )));
        }
        value["model"] = model.id.clone().into();
        if kind == Kind::Tts
            && !model.voice.is_empty()
            && value.get("voice").is_none_or(Value::is_null)
        {
            value["voice"] = model.voice.clone().into();
        }
        if free && config.free.max_output_tokens > 0 {
            clamp_output_tokens(&mut value, path, config.free.max_output_tokens);
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
            model: index_of(config, model),
            payload: Payload::Json(value, bytes),
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
    let Some(boundary) = multipart::boundary(content_type) else {
        // Not a form: the model cannot be read or rewritten.
        let model = models::resolve(config, caller, capability, None)?;
        let counter = counter.clone();
        let mut source = body.into_data_stream();
        let upload = async_stream::stream! {
            while let Some(chunk) = source.next().await {
                let chunk = chunk.map_err(std::io::Error::other)?;
                if !counter.add(chunk.len(), max) {
                    yield Err(std::io::Error::other("upload exceeds limit"));
                    return;
                }
                yield Ok::<Bytes, std::io::Error>(chunk);
            }
        };
        return Ok(Prepared {
            model: index_of(config, model),
            payload: Payload::Stream(Some(Upload::Raw(reqwest::Body::wrap_stream(upload)))),
        });
    };
    let mut pending = PendingUpload::read(config, &boundary, body, counter).await?;
    let model = models::resolve(config, caller, capability, pending.requested.as_deref())?;
    // Later `model` parts must name the same model.
    let bound = Bound {
        config: Arc::clone(config),
        caller: caller.clone(),
        capability,
        id: model.id.clone(),
    };
    for later in &pending.later {
        check_same_model(&bound, later)?;
    }
    pending.bound = Some(bound);
    Ok(Prepared {
        model: index_of(config, model),
        payload: Payload::Stream(Some(Upload::Multipart(Box::new(pending)))),
    })
}

/// The start of a multipart upload, read until its `model` part completed
/// (or a bounded amount of it), and the rest of the stream.
struct PendingUpload {
    boundary: String,
    /// Everything read so far, unmodified.
    prefix: Vec<u8>,
    /// The rest of the body, or `None` when it ended while the prefix was read.
    rest: Option<BodyDataStream>,
    /// Follows the model parts of the rest of the body.
    sniff: Multipart,
    /// The first `model` part, when the prefix holds one.
    requested: Option<String>,
    /// Further `model` parts of the prefix.
    later: Vec<String>,
    /// The resolved model, for checking later parts. Set once it is known.
    bound: Option<Bound>,
}

/// What later `model` parts are compared with.
struct Bound {
    config: Arc<Config>,
    caller: Caller,
    capability: Capability,
    id: String,
}

impl PendingUpload {
    /// Reads the body until the first `model` part is complete, or until
    /// `max_json_body_bytes` were buffered, or the body ends.
    async fn read(
        config: &Arc<Config>,
        boundary: &str,
        body: Body,
        counter: &UploadCounter,
    ) -> Result<Self> {
        let max = config.max_stream_body_bytes;
        let mut sniff = Multipart::new(boundary, "", false, false);
        let mut rest = body.into_data_stream();
        let mut prefix = Vec::new();
        let mut models = Vec::new();
        let mut ended = false;
        loop {
            let Some(chunk) = rest.next().await else {
                ended = true;
                break;
            };
            let chunk = chunk.map_err(|_| Error::bad("The upload was interrupted"))?;
            if !counter.add(chunk.len(), max) {
                return Err(Error::too_large("Upload exceeds limit"));
            }
            sniff.push(&chunk)?;
            prefix.extend_from_slice(&chunk);
            models.extend(sniff.take_models());
            if !models.is_empty() || prefix.len() > config.max_json_body_bytes {
                break;
            }
        }
        let mut models = models.into_iter();
        Ok(Self {
            boundary: boundary.to_owned(),
            prefix,
            rest: (!ended).then_some(rest),
            sniff,
            requested: models.next(),
            later: models.collect(),
            bound: None,
        })
    }

    /// The upload body, with the `model` part rewritten to `route_model`.
    /// A form without one gains it at the end.
    fn into_body(self, route_model: &str, counter: UploadCounter, max: usize) -> reqwest::Body {
        let Self {
            boundary,
            prefix,
            mut rest,
            mut sniff,
            bound,
            ..
        } = self;
        let mut rewrite = Multipart::new(&boundary, route_model, true, true);
        let upload = async_stream::stream! {
            match rewrite.push(&prefix) {
                Ok(out) if !out.is_empty() => yield Ok::<Bytes, std::io::Error>(Bytes::from(out)),
                Ok(_) => {}
                Err(e) => {
                    yield Err(counter.reject(e));
                    return;
                }
            }
            loop {
                let next = match rest.as_mut() {
                    Some(rest) => rest.next().await,
                    None => None,
                };
                let Some(chunk) = next else { break };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(e) => {
                        yield Err(std::io::Error::other(e));
                        return;
                    }
                };
                if !counter.add(chunk.len(), max) {
                    yield Err(std::io::Error::other("upload exceeds limit"));
                    return;
                }
                if let Err(e) = sniff.push(&chunk) {
                    yield Err(counter.reject(e));
                    return;
                }
                if let Some(bound) = &bound {
                    for value in sniff.take_models() {
                        if let Err(e) = check_same_model(bound, &value) {
                            yield Err(counter.reject(e));
                            return;
                        }
                    }
                }
                match rewrite.push(&chunk) {
                    Ok(out) if !out.is_empty() => yield Ok(Bytes::from(out)),
                    Ok(_) => {}
                    Err(e) => {
                        yield Err(counter.reject(e));
                        return;
                    }
                }
            }
            let out = rewrite.finish();
            if !out.is_empty() {
                yield Ok(Bytes::from(out));
            }
        };
        reqwest::Body::wrap_stream(upload)
    }
}

/// Fails with `model_must_precede_file` when `value` (a `model` part) names
/// another model than the one the request resolved to.
fn check_same_model(bound: &Bound, value: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    let model = models::resolve(&bound.config, &bound.caller, bound.capability, Some(value))?;
    if model.id == bound.id {
        Ok(())
    } else {
        Err(
            Error::bad("The model part must come before the file and may not change")
                .with_code("model_must_precede_file")
                .with_param("model"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_errors_keep_only_the_openai_fields() {
        let raw = br#"{"error":{"message":"x is not a valid model ID","code":400,"metadata":{"provider_name":"P"}},"user_id":"user_123"}"#;
        let body = upstream_error(StatusCode::BAD_REQUEST, Some(raw));
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
        let body = upstream_error(StatusCode::BAD_REQUEST, Some(raw));
        assert_eq!(body["error"]["param"], "messages");
        assert_eq!(body["error"]["code"], "invalid_value");
        // Google wraps its errors in an array.
        let raw = br#"[{"error":{"code":400,"message":"Invalid value at 'temperature'","status":"INVALID_ARGUMENT"}}]"#;
        let body = upstream_error(StatusCode::BAD_REQUEST, Some(raw));
        assert_eq!(body["error"]["message"], "Invalid value at 'temperature'");
    }

    #[test]
    fn non_json_upstream_errors_get_a_generic_message() {
        let body = upstream_error(StatusCode::SERVICE_UNAVAILABLE, None);
        assert_eq!(body["error"]["type"], "server_error");
        assert!(body["error"]["message"].as_str().unwrap().contains("503"));
    }

    #[test]
    fn upstream_account_headers_are_not_returned() {
        let mut upstream = HeaderMap::new();
        for name in [
            "retry-after",
            "retry-after-ms",
            "x-ratelimit-remaining-tokens",
            "ratelimit-policy",
            "openai-organization",
            "x-request-id",
            "content-type",
        ] {
            upstream.insert(HeaderName::from_static(name), HeaderValue::from_static("1"));
        }
        let out = forward_headers(&upstream, false);
        let names: Vec<&str> = out.keys().map(HeaderName::as_str).collect();
        assert_eq!(names, ["x-request-id", "content-type"]);
    }

    #[test]
    fn output_tokens_are_clamped_or_injected_for_free_requests() {
        use serde_json::json;
        let chat = "/v1/chat/completions";
        let mut body = json!({"max_tokens": 100000, "max_completion_tokens": 50});
        clamp_output_tokens(&mut body, chat, 4096);
        assert_eq!(
            body,
            json!({"max_tokens": 4096, "max_completion_tokens": 50})
        );
        let mut body = json!({"messages": []});
        clamp_output_tokens(&mut body, chat, 4096);
        assert_eq!(body["max_tokens"], 4096);
        let mut body = json!({"input": "x", "max_output_tokens": 9000});
        clamp_output_tokens(&mut body, "/v1/responses", 4096);
        assert_eq!(body["max_output_tokens"], 4096);
        let mut body = json!({"input": "x"});
        clamp_output_tokens(&mut body, "/v1/responses", 4096);
        assert_eq!(body["max_output_tokens"], 4096);
        // A nonsense value is replaced, and other routes are untouched.
        let mut body = json!({"max_tokens": "lots"});
        clamp_output_tokens(&mut body, "/v1/completions", 10);
        assert_eq!(body["max_tokens"], 10);
        let mut body = json!({"input": "x"});
        clamp_output_tokens(&mut body, "/v1/embeddings", 10);
        assert_eq!(body, json!({"input": "x"}));
    }

    #[test]
    fn failures_fall_through_to_the_next_route() {
        let retries = |code: u16| Outcome::of(Some(code), None).retries();
        for code in [
            400, 401, 402, 403, 404, 408, 409, 413, 422, 425, 429, 500, 502, 503, 504,
        ] {
            assert!(retries(code), "{code}");
        }
        for code in [200, 301, 405, 415] {
            assert!(!retries(code), "{code}");
        }
        assert!(Outcome::of(None, None).retries());
    }

    #[test]
    fn endpoints_need_the_matching_capability() {
        for (path, capability) in [
            ("/v1/chat/completions", Capability::Chat),
            ("/v1/embeddings", Capability::Chat),
            ("/v1/audio/speech", Capability::Speech),
            ("/v1/audio/transcriptions", Capability::Transcription),
            ("/v1/images/generations", Capability::Image),
        ] {
            assert_eq!(Kind::of(path).capability(), capability, "{path}");
        }
    }
}
