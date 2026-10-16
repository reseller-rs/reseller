//! WebSocket relay for realtime APIs (`/v1/realtime?model=…`).
use super::{Kind, forward_headers, inspect::realtime_usage};
use crate::{
    App,
    config::{Policy, Upstream},
    error::{Error, Result},
    identity::Identity,
    meter::{Meter, RequestMeta},
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
use std::time::Duration;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message as UpstreamMessage, client::IntoClientRequest},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

type UpstreamSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Applies the model policy to the `model` query parameter and returns the
/// effective model.
fn apply_model_policy(url: &mut url::Url, configured: &str, policy: Policy) -> String {
    let requested = url
        .query_pairs()
        .find(|(k, _)| k == "model")
        .map(|(_, v)| v.into_owned())
        .filter(|m| !m.is_empty());
    let model = match (policy, requested) {
        (Policy::Force, _) | (Policy::Default, None) if !configured.is_empty() => {
            configured.to_owned()
        }
        (_, Some(requested)) => requested,
        (_, None) => return String::new(),
    };
    let others: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "model")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(others)
        .append_pair("model", &model);
    model
}

pub(super) async fn relay(
    app: App,
    req: Request<Body>,
    ip: String,
    identity: Option<Identity>,
    upstream: &Upstream,
    policy: Policy,
    mut url: url::Url,
) -> Result<Response<Body>> {
    let (mut parts, _) = req.into_parts();
    let model = apply_model_policy(&mut url, &upstream.model, policy);
    let path = parts.uri.path().to_owned();
    let meter = Meter::begin(
        &app,
        identity,
        RequestMeta {
            method: "WS",
            path: &path,
            kind: Kind::of(&path).as_str(),
            model: &model,
            ip: &ip,
        },
    )
    .await?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|()| Error::bad("Invalid WebSocket URL"))?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(Error::internal)?;
    request
        .headers_mut()
        .extend(forward_headers(&parts.headers, true));
    request.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", upstream.api_key)).map_err(Error::internal)?,
    );
    // Offer the client's subprotocols; accept only the one upstream selects.
    if let Some(v) = parts.headers.get(header::SEC_WEBSOCKET_PROTOCOL) {
        request
            .headers_mut()
            .insert(header::SEC_WEBSOCKET_PROTOCOL, v.clone());
    }
    let connected = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(request)).await;
    let (up, response) = match connected {
        Ok(Ok(pair)) => pair,
        Ok(Err(_)) => {
            return Err(settle_failure(
                meter,
                StatusCode::BAD_GATEWAY,
                "WebSocket connection failed",
            )
            .await);
        }
        Err(_) => {
            return Err(settle_failure(
                meter,
                StatusCode::GATEWAY_TIMEOUT,
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
    let session_limit = Duration::from_secs(app.config().await.upstream_timeout_seconds);
    Ok(upgrade
        .on_upgrade(move |down| session(down, up, meter, session_limit))
        .into_response())
}

async fn settle_failure(mut meter: Meter, status: StatusCode, message: &'static str) -> Error {
    meter.fail(status, message);
    if let Err(e) = meter.finish().await {
        tracing::error!(error = %e, "WebSocket settlement failed");
    }
    Error::new(status, "upstream_error", message)
}

/// Relays frames both ways until either side closes or the session limit
/// elapses, metering bytes and realtime usage.
async fn session(down: WebSocket, up: UpstreamSocket, mut meter: Meter, limit: Duration) {
    meter.status = StatusCode::SWITCHING_PROTOCOLS.as_u16();
    let (mut down_tx, mut down_rx) = down.split();
    let (mut up_tx, mut up_rx) = up.split();
    let relay = async {
        loop {
            tokio::select! {
                message = down_rx.next() => {
                    let Some(Ok(message)) = message else { break };
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
                        Message::Close(_) => break,
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
                            if let Some(usage) = realtime_usage(text.as_str()) {
                                meter.usage.add(&usage);
                            }
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
                        break;
                    }
                }
            }
        }
    };
    if tokio::time::timeout(limit, relay).await.is_err() {
        meter.error = Some("session time limit reached".into());
    }
    let _ = down_tx.close().await;
    let _ = up_tx.close().await;
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
    fn force_overrides_requested_model() {
        let mut u = url("?model=expensive&x=1");
        assert_eq!(apply_model_policy(&mut u, "cheap", Policy::Force), "cheap");
        assert_eq!(u.query(), Some("x=1&model=cheap"));
    }

    #[test]
    fn default_fills_only_missing_model() {
        let mut u = url("");
        assert_eq!(
            apply_model_policy(&mut u, "cheap", Policy::Default),
            "cheap"
        );
        let mut u = url("?model=chosen");
        assert_eq!(
            apply_model_policy(&mut u, "cheap", Policy::Default),
            "chosen"
        );
    }

    #[test]
    fn passthrough_keeps_client_model() {
        let mut u = url("?model=chosen");
        assert_eq!(
            apply_model_policy(&mut u, "cheap", Policy::Passthrough),
            "chosen"
        );
        let mut u = url("");
        assert_eq!(apply_model_policy(&mut u, "cheap", Policy::Passthrough), "");
        assert_eq!(u.query(), None);
    }
}
