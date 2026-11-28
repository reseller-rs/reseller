//! WebSocket relay for realtime APIs (`/v1/realtime?model=<name>`).
use super::{
    Kind,
    inspect::{RealtimeEvent, realtime_event},
    models, rate_headers, target, unavailable, upstream_headers,
};
use crate::{
    App,
    config::Config,
    error::{Error, Result},
    identity::Caller,
    meter::{Meter, RequestMeta, Usage},
    router::{Permit, free::Admission, hints::Hints},
};
use axum::{
    body::Body,
    extract::{
        FromRequestParts,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderValue, Request, Response, StatusCode, header},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message as UpstreamMessage, client::IntoClientRequest},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// How long to keep reading upstream after the client has left, so a
/// response that is still being generated can report its usage.
const SETTLE_GRACE: Duration = Duration::from_secs(5);

type UpstreamSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// The `model` query parameter, if it is not empty.
fn requested_model(url: &url::Url) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == "model")
        .map(|(_, v)| v.into_owned())
        .filter(|m| !m.is_empty())
}

/// Sets the `model` query parameter.
fn set_model(url: &mut url::Url, model: &str) {
    let others: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "model")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(others)
        .append_pair("model", model);
}

/// Relays a realtime session. The model comes from the query, the route is
/// chosen up front, and there is a single attempt.
pub(super) async fn relay(
    app: App,
    req: Request<Body>,
    ip: String,
    caller: Caller,
    config: Arc<Config>,
) -> Result<Response<Body>> {
    let (mut parts, _) = req.into_parts();
    let path = parts.uri.path().to_owned();
    let kind = Kind::of(&path);
    // Only the query matters: any base works to read it.
    let incoming = url::Url::parse(&format!("http://x{}", parts.uri))
        .map_err(|_| Error::bad("Invalid request URL"))?;
    let model = models::resolve(
        &config,
        &caller,
        kind.capability(),
        requested_model(&incoming).as_deref(),
    )?;
    let free = super::free_admission(&app, &config, &caller, model, &ip).await?;
    let route_models: Vec<&str> = model.routes.iter().map(|r| r.model.as_str()).collect();
    let mut meter = Meter::begin(
        &app,
        caller.identity().cloned(),
        RequestMeta {
            method: "WS",
            path: &path,
            kind: kind.as_str(),
            model: &model.id,
            tier: model.tier,
            routes: &route_models,
            ip: &ip,
            // A realtime session's usage is unknown up front: it holds the
            // configured minimum.
            estimate: &Usage::NONE,
        },
    )
    .await
    .map_err(|e| rate_headers(e, free.as_ref()))?;
    let picked = match app.router.pick(&config, model, &[], false) {
        Ok(picked) => picked,
        Err(wait) => {
            return Err(settle_failure(
                meter,
                rate_headers(unavailable(wait), free.as_ref()),
                "no route available",
            )
            .await);
        }
    };
    let route = &model.routes[picked.index];
    let mut permit = picked.permit;
    let Some(provider) = config.provider(&route.provider) else {
        return Err(settle_failure(
            meter,
            Error::internal("unknown provider"),
            "no route available",
        )
        .await);
    };
    let mut url = target(&provider.base_url, &path, parts.uri.query())?;
    set_model(&mut url, &route.model);
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|()| Error::bad("Invalid WebSocket URL"))?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(Error::internal)?;
    request
        .headers_mut()
        .extend(upstream_headers(&parts.headers, provider));
    if !provider.api_key.is_empty() {
        request.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", provider.api_key))
                .map_err(Error::internal)?,
        );
    }
    // Offer the client's subprotocols. Accept only the one upstream selects.
    if let Some(v) = parts.headers.get(header::SEC_WEBSOCKET_PROTOCOL) {
        request
            .headers_mut()
            .insert(header::SEC_WEBSOCKET_PROTOCOL, v.clone());
    }
    let started = Instant::now();
    let connected = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(request)).await;
    let (up, response) = match connected {
        Ok(Ok(pair)) => {
            permit.finish(Some(200), &Hints::default(), false);
            meter.attempt(&route.provider, &route.model, 101, started.elapsed());
            pair
        }
        Ok(Err(_)) => {
            permit.finish(None, &Hints::default(), false);
            meter.attempt(&route.provider, &route.model, 0, started.elapsed());
            return Err(settle_failure(
                meter,
                rate_headers(
                    Error::new(
                        StatusCode::BAD_GATEWAY,
                        "upstream_error",
                        "WebSocket connection failed",
                    ),
                    free.as_ref(),
                ),
                "WebSocket connection failed",
            )
            .await);
        }
        Err(_) => {
            permit.finish(None, &Hints::default(), false);
            meter.attempt(&route.provider, &route.model, 0, started.elapsed());
            return Err(settle_failure(
                meter,
                rate_headers(
                    Error::new(
                        StatusCode::GATEWAY_TIMEOUT,
                        "upstream_error",
                        "WebSocket connection timed out",
                    ),
                    free.as_ref(),
                ),
                "WebSocket connection timed out",
            )
            .await);
        }
    };
    let mut upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &app)
        .await
        .map_err(|_| Error::bad("Invalid WebSocket upgrade"))?
        .max_message_size(MAX_MESSAGE_BYTES);
    if let Some(protocol) = response
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
    {
        upgrade = upgrade.protocols([protocol.to_owned()]);
    }
    let session_limit = provider.timeout(config.upstream_timeout_seconds);
    Ok(upgrade
        .on_upgrade(move |down| session(down, up, meter, session_limit, permit, free))
        .into_response())
}

/// Settles a request that never got a session and returns its error.
async fn settle_failure(mut meter: Meter, error: Error, message: &str) -> Error {
    meter.fail(error.status, message);
    if let Err(e) = meter.finish().await {
        tracing::error!(error = %e, "WebSocket settlement failed");
    }
    error
}

/// Responses the upstream has started but not yet finished, and the usage
/// of those that finished.
fn observe_event(meter: &mut Meter, pending: &mut i64, text: &str) {
    match realtime_event(text) {
        Some(RealtimeEvent::Created) => *pending += 1,
        Some(RealtimeEvent::Done(usage)) => {
            *pending = (*pending - 1).max(0);
            if let Some(usage) = usage {
                meter.usage.add(&usage);
            }
        }
        None => {}
    }
}

/// Relays frames both ways until either side closes or the session limit
/// elapses, metering bytes and realtime usage.
async fn session(
    down: WebSocket,
    up: UpstreamSocket,
    mut meter: Meter,
    limit: Duration,
    permit: Permit,
    free: Option<Admission>,
) {
    meter.status = StatusCode::SWITCHING_PROTOCOLS.as_u16();
    // An established session counts as a successful request.
    meter.success = true;
    let (mut down_tx, mut down_rx) = down.split();
    let (mut up_tx, mut up_rx) = up.split();
    let mut pending = 0i64;
    let mut client_left = false;
    let relay = async {
        loop {
            tokio::select! {
                message = down_rx.next() => {
                    let Some(Ok(message)) = message else { client_left = true; break };
                    let message = match message {
                        Message::Text(text) => {
                            meter.bytes_in += text.len() as i64;
                            UpstreamMessage::Text(text.as_str().into())
                        }
                        Message::Binary(data) => {
                            meter.bytes_in += data.len() as i64;
                            UpstreamMessage::Binary(data)
                        }
                        Message::Ping(data) => UpstreamMessage::Ping(data),
                        Message::Pong(data) => UpstreamMessage::Pong(data),
                        Message::Close(_) => { client_left = true; break }
                    };
                    if up_tx.send(message).await.is_err() {
                        break;
                    }
                }
                message = up_rx.next() => {
                    let Some(Ok(message)) = message else { break };
                    let message = match message {
                        UpstreamMessage::Text(text) => {
                            meter.bytes_out += text.len() as i64;
                            observe_event(&mut meter, &mut pending, text.as_str());
                            Message::Text(text.as_str().into())
                        }
                        UpstreamMessage::Binary(data) => {
                            meter.bytes_out += data.len() as i64;
                            Message::Binary(data)
                        }
                        UpstreamMessage::Ping(data) => Message::Ping(data),
                        UpstreamMessage::Pong(data) => Message::Pong(data),
                        UpstreamMessage::Close(_) => break,
                        UpstreamMessage::Frame(_) => continue,
                    };
                    if down_tx.send(message).await.is_err() {
                        client_left = true;
                        break;
                    }
                }
            }
        }
    };
    let timed_out = tokio::time::timeout(limit, relay).await.is_err();
    if timed_out {
        meter.error = Some("session time limit reached".into());
    } else if client_left && pending > 0 {
        // The client left mid-response: the upstream still bills it, so give
        // it a bounded moment to deliver `response.done` before closing.
        let _ = tokio::time::timeout(SETTLE_GRACE, async {
            while pending > 0 {
                match up_rx.next().await {
                    Some(Ok(UpstreamMessage::Text(text))) => {
                        meter.bytes_out += text.len() as i64;
                        observe_event(&mut meter, &mut pending, text.as_str());
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
        })
        .await;
    }
    let _ = down_tx.close().await;
    let _ = up_tx.close().await;
    let tokens = u64::try_from(meter.usage.total).unwrap_or(0);
    permit.charge_tokens(tokens);
    if let Some(free) = &free {
        free.guard.charge_tokens(tokens);
    }
    if let Err(e) = meter.finish().await {
        tracing::error!(error = %e, "WebSocket settlement failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(query: &str) -> url::Url {
        url::Url::parse(&format!("https://up.example/v1/realtime{query}")).unwrap()
    }

    #[test]
    fn the_model_query_parameter_is_read_and_replaced() {
        assert_eq!(
            requested_model(&url("?model=gpt&x=1")).as_deref(),
            Some("gpt")
        );
        assert_eq!(requested_model(&url("?model=")), None);
        assert_eq!(requested_model(&url("")), None);
        let mut u = url("?model=public&x=1");
        set_model(&mut u, "upstream/name");
        assert_eq!(u.query(), Some("x=1&model=upstream%2Fname"));
        let mut u = url("");
        set_model(&mut u, "m");
        assert_eq!(u.query(), Some("model=m"));
    }
}
