//! Streaming transport and bounded usage inspection, independent of the ledger.
use crate::{
    App,
    config::{Policy, Upstream},
    error::{Error, Result},
    identity,
    stats::{Meter, RequestMeta, Usage},
};
use axum::{
    body::{Body, to_bytes},
    extract::{
        FromRequestParts,
        ws::{Message, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, Request, Response},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI64, Ordering},
};

pub fn target(base: &str, path: &str, query: Option<&str>) -> Result<url::Url> {
    let suffix = path
        .strip_prefix("/v1")
        .ok_or_else(|| Error::bad("Invalid proxy path"))?;
    // Reject traversal before URL normalization; the configured base path is a boundary.
    let lower = suffix
        .to_ascii_lowercase()
        .replace("%2e", ".")
        .replace("%2f", "/")
        .replace("%5c", "/");
    if lower.split('/').any(|s| s == ".." || s == ".") || suffix.contains('\\') {
        return Err(Error::bad("Invalid proxy path"));
    }
    let mut u = url::Url::parse(&format!("{}{suffix}", base.trim_end_matches('/')))
        .map_err(Error::internal)?;
    u.set_query(query);
    Ok(u)
}
pub fn kind(path: &str) -> &'static str {
    if path == "/v1/audio/speech" {
        "tts"
    } else if matches!(path, "/v1/audio/transcriptions" | "/v1/audio/translations") {
        "stt"
    } else {
        "llm"
    }
}
fn patch(v: &mut Value, key: &str, value: &str, policy: &Policy) {
    if !value.is_empty()
        && (matches!(policy, Policy::Force)
            || (matches!(policy, Policy::Default) && v.get(key).is_none_or(Value::is_null)))
    {
        v[key] = value.into();
    }
}
fn headers(input: &HeaderMap, request: bool) -> HeaderMap {
    let mut out = HeaderMap::new();
    let connection: Vec<_> = input
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();
    for (k, v) in input {
        let n = k.as_str();
        if [
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
        ]
        .contains(&n)
            || connection.iter().any(|s| s == n)
        {
            continue;
        }
        if request
            && ([
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
            ]
            .contains(&n)
                || n.starts_with("sec-websocket-"))
        {
            continue;
        }
        if !request
            && [
                "set-cookie",
                "access-control-allow-origin",
                "access-control-allow-credentials",
                // The body is re-streamed, so the upstream length no longer
                // matches framing. More importantly, a declared Content-Length
                // makes hyper stop polling the relay stream once that many
                // bytes are written, dropping the meter before usage is
                // finalized. Let hyper use chunked framing instead.
                "content-length",
            ]
            .contains(&n)
        {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    out
}
#[derive(Default)]
pub struct Inspector {
    pending: Vec<u8>,
    capture: Vec<u8>,
    pub usage: Usage,
    bytes: i64,
    discard_line: bool,
}
const CAPTURE: usize = 4 * 1024 * 1024;
impl Inspector {
    pub fn feed(&mut self, chunk: &[u8], ct: &str) {
        self.bytes = self.bytes.saturating_add(chunk.len() as i64);
        if ct.contains("text/event-stream") {
            for &b in chunk {
                if b == b'\n' {
                    if !self.discard_line {
                        self.line();
                    }
                    self.pending.clear();
                    self.discard_line = false;
                } else if !self.discard_line {
                    if self.pending.len() < CAPTURE {
                        self.pending.push(b);
                    } else {
                        self.pending.clear();
                        self.discard_line = true;
                    }
                }
            }
        } else if ct.contains("json") || ct.starts_with("audio/") {
            let max = if ct.starts_with("audio/") {
                65536
            } else {
                CAPTURE
            };
            let n = chunk.len().min(max.saturating_sub(self.capture.len()));
            self.capture.extend_from_slice(&chunk[..n]);
        }
    }
    fn line(&mut self) {
        if let Some(raw) = self.pending.strip_prefix(b"data:")
            && let Ok(v) = serde_json::from_slice::<Value>(raw)
        {
            self.inspect(&v);
        }
    }
    fn inspect(&mut self, v: &Value) {
        if let Some(u) = v
            .get("usage")
            .or_else(|| v.pointer("/response/usage"))
            .or_else(|| v.pointer("/data/usage"))
            .filter(|v| v.is_object())
        {
            self.usage = Usage::parse(u);
        }
    }
    pub fn finish(&mut self, ct: &str) {
        if ct.contains("text/event-stream") {
            if !self.discard_line {
                self.line();
            }
        } else if ct.contains("json") {
            if let Ok(v) = serde_json::from_slice::<Value>(&self.capture) {
                self.inspect(&v);
            }
        } else if ct.contains("pcm") || ct.contains("l16") {
            self.usage.audio_ms = self.bytes.saturating_mul(1000) / 48000;
        } else if ct.contains("wav") || ct.contains("wave") {
            self.usage.audio_ms = wav_ms(&self.capture, self.bytes).unwrap_or(0);
        }
    }
}
fn wav_ms(b: &[u8], bytes: i64) -> Option<i64> {
    if b.get(..4)? != b"RIFF" || b.get(8..12)? != b"WAVE" {
        return None;
    }
    let mut pos = 12;
    let mut rate = None;
    while pos + 8 <= b.len() {
        let len = u32::from_le_bytes(b.get(pos + 4..pos + 8)?.try_into().ok()?) as usize;
        match &b[pos..pos + 4] {
            b"fmt " if len >= 16 => {
                rate = Some(u32::from_le_bytes(b.get(pos + 16..pos + 20)?.try_into().ok()?) as i64);
            }
            b"data" => {
                let rate = rate.filter(|n| *n > 0)?;
                return Some((bytes - (pos + 8) as i64).max(0) * 1000 / rate);
            }
            _ => {}
        }
        pos = pos.checked_add(8 + len + (len % 2))?;
    }
    None
}
pub async fn handle(app: App, req: Request<Body>, ip: String) -> Result<Response<Body>> {
    let c = app.config.read().await.clone();
    let identity = identity::authenticate(&app, req.headers(), true).await?;
    let path = req.uri().path().to_owned();
    let k = kind(&path);
    let u: Upstream = match k {
        "tts" => c.tts.clone(),
        "stt" => c.stt.clone(),
        _ => c.llm.clone(),
    };
    if u.api_key.is_empty() {
        return Err(Error::new(
            503,
            "proxy_configuration_error",
            "Upstream API key is not configured",
        ));
    }
    let url = target(&u.base_url, &path, req.uri().query())?;
    if req
        .headers()
        .get("upgrade")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        return websocket(app, req, ip, identity, u, url).await;
    }
    let (parts, body) = req.into_parts();
    let ct = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_json = ct
        .split(';')
        .next()
        .is_some_and(|s| s.trim() == "application/json" || s.trim().ends_with("+json"));
    let mut value = None;
    let mut raw = None;
    let mut model = String::new();
    let input_count = Arc::new(AtomicI64::new(0));
    let too_large = Arc::new(AtomicBool::new(false));
    let mut upload = None;
    if is_json {
        let b = to_bytes(body, c.max_json_body_bytes)
            .await
            .map_err(|_| Error::new(413, "request_too_large", "JSON body exceeds limit"))?;
        input_count.store(b.len() as i64, Ordering::Relaxed);
        let mut v: Value = serde_json::from_slice(&b)?;
        if !v.is_object() {
            return Err(Error::bad("JSON body must be an object"));
        }
        patch(&mut v, "model", &u.model, &c.model_policy);
        if k == "tts" {
            patch(&mut v, "voice", &u.voice, &c.voice_policy);
        }
        if v.get("model").is_some_and(|v| !v.is_string()) {
            return Err(Error::bad("model must be a string"));
        }
        model = v["model"].as_str().unwrap_or("").to_owned();
        if path == "/v1/chat/completions" && v["stream"] == true {
            if v.get("stream_options").is_none_or(Value::is_null) {
                v["stream_options"] = serde_json::json!({});
            }
            if !v["stream_options"].is_object() {
                return Err(Error::bad("stream_options must be an object"));
            }
            v["stream_options"]["include_usage"] = true.into();
        }
        raw = Some(serde_json::to_vec(&v)?);
        value = Some(v);
    } else {
        if parts
            .headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            .is_some_and(|n| n > c.max_stream_body_bytes)
        {
            return Err(Error::new(413, "request_too_large", "Upload exceeds limit"));
        }
        let count = input_count.clone();
        let flag = too_large.clone();
        let max = c.max_stream_body_bytes;
        upload = Some(reqwest::Body::wrap_stream(body.into_data_stream().map(
            move |chunk| {
                let chunk = chunk.map_err(std::io::Error::other)?;
                let n = count.fetch_add(chunk.len() as i64, Ordering::Relaxed) + chunk.len() as i64;
                if n > max as i64 {
                    flag.store(true, Ordering::Relaxed);
                    return Err(std::io::Error::other("upload exceeds limit"));
                }
                Ok(chunk)
            },
        )));
    }
    let mut meter = Meter::begin(
        &app,
        identity.clone(),
        RequestMeta {
            method: parts.method.as_str(),
            path: &path,
            kind: k,
            model: &model,
            ip: &ip,
        },
    )
    .await?;
    let pre_balance = if let Some(i) = &identity {
        Some(crate::db::balance(&mut *app.db.acquire().await?, &i.account_id).await?)
    } else {
        None
    };
    let mut models = vec![model.clone()];
    if is_json && !u.model.is_empty() && model.eq_ignore_ascii_case(&u.model) {
        models.extend(u.fallbacks.clone());
    }
    let mut response = None;
    let mut fallback = None;
    for (index, m) in models.iter().enumerate() {
        if let Some(i) = &identity {
            crate::stats::check_model(i, m)?;
        }
        if index > 0 {
            if let Some(v) = &mut value {
                v["model"] = m.clone().into();
                raw = Some(serde_json::to_vec(v)?);
            }
            fallback = Some(m.clone());
        }
        meter.model = m.clone();
        let mut request = app
            .client
            .request(parts.method.clone(), url.clone())
            .headers(headers(&parts.headers, true))
            .bearer_auth(&u.api_key)
            .header("accept-encoding", "identity")
            .timeout(std::time::Duration::from_secs(c.upstream_timeout_seconds));
        if let Some(b) = &raw {
            request = request.body(b.clone());
        } else if let Some(b) = upload.take() {
            request = request.body(b);
        }
        match request.send().await {
            Ok(r) => {
                let retry = r.status().is_server_error()
                    || [400, 402, 404, 408, 429].contains(&r.status().as_u16());
                if retry && index + 1 < models.len() {
                    continue;
                }
                response = Some(r);
                break;
            }
            Err(e) => {
                if index + 1 < models.len() {
                    continue;
                }
                meter.bytes_in = input_count.load(Ordering::Relaxed);
                let status = if too_large.load(Ordering::Relaxed) {
                    413
                } else if e.is_timeout() {
                    504
                } else {
                    502
                };
                meter.status = status;
                meter.error = Some(
                    if status == 413 {
                        "upload exceeds limit"
                    } else {
                        "upstream connection failed"
                    }
                    .into(),
                );
                meter.finish().await?;
                return Err(Error::new(
                    status,
                    if status == 413 {
                        "request_too_large"
                    } else {
                        "upstream_error"
                    },
                    "Upstream request could not be completed",
                ));
            }
        }
    }
    let r = response.ok_or_else(|| Error::new(502, "upstream_error", "No upstream response"))?;
    let status = r.status();
    let mut out = headers(r.headers(), false);
    let ct = r
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    out.insert(
        "x-request-id",
        HeaderValue::from_str(&meter.id).map_err(Error::internal)?,
    );
    if let Some(i) = identity {
        out.insert(
            "x-key-prefix",
            HeaderValue::from_str(&i.prefix).map_err(Error::internal)?,
        );
    }
    if let Some(b) = pre_balance {
        out.insert(
            "x-account-balance-usd",
            HeaderValue::from_str(&crate::money::display(b)).map_err(Error::internal)?,
        );
    }
    if let Some(m) = fallback {
        out.insert(
            "x-model-fallback",
            HeaderValue::from_str(&m).map_err(Error::internal)?,
        );
    }
    let stream = async_stream::stream! {
     let mut source=r.bytes_stream();let mut inspect=Inspector::default();meter.status=status.as_u16();
     while let Some(chunk)=source.next().await {
      meter.bytes_in=input_count.load(Ordering::Relaxed);
      match chunk {
       Ok(b)=>{meter.bytes_out=meter.bytes_out.saturating_add(b.len() as i64);inspect.feed(&b,&ct);meter.usage=inspect.usage.clone();yield Ok::<_,std::io::Error>(b);},
       Err(_)=>{meter.error=Some("upstream stream interrupted".into());meter.status=502;yield Err(std::io::Error::other("upstream stream interrupted"));break;}
      }
     }
     inspect.finish(&ct);
     if inspect.usage.cost.is_some()||inspect.usage.total>0||inspect.usage.audio_ms>0 {meter.usage=inspect.usage;}
     if let Err(e)=meter.finish().await {tracing::error!(error=%e,"request settlement failed");}
    };
    let mut res = Response::new(Body::from_stream(stream));
    *res.status_mut() = status;
    *res.headers_mut() = out;
    Ok(res)
}
async fn websocket(
    app: App,
    req: Request<Body>,
    ip: String,
    i: Option<identity::Identity>,
    u: Upstream,
    mut url: url::Url,
) -> Result<Response<Body>> {
    use tokio_tungstenite::tungstenite::{Message as UpMessage, client::IntoClientRequest};
    let (mut parts, _) = req.into_parts();
    let model = url
        .query_pairs()
        .find(|(k, _)| k == "model")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| u.model.clone());
    let mut meter = Meter::begin(
        &app,
        i,
        RequestMeta {
            method: "WS",
            path: parts.uri.path(),
            kind: kind(parts.uri.path()),
            model: &model,
            ip: &ip,
        },
    )
    .await?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|_| Error::bad("Invalid WebSocket URL"))?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(Error::internal)?;
    request.headers_mut().extend(headers(&parts.headers, true));
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", u.api_key)).map_err(Error::internal)?,
    );
    // Forward requested subprotocols; never accept a protocol not selected upstream.
    if let Some(v) = parts.headers.get("sec-websocket-protocol") {
        request
            .headers_mut()
            .insert("sec-websocket-protocol", v.clone());
    }
    let (up, response) = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| Error::new(504, "upstream_error", "WebSocket connection timed out"))?
    .map_err(|_| Error::new(502, "upstream_error", "WebSocket connection failed"))?;
    let mut ws = WebSocketUpgrade::from_request_parts(&mut parts, &app)
        .await
        .map_err(|_| Error::bad("Invalid WebSocket upgrade"))?
        .max_message_size(16 * 1024 * 1024);
    if let Some(p) = response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
    {
        ws = ws.protocols([p.to_owned()]);
    }
    let timeout = app.config.read().await.upstream_timeout_seconds;
    Ok(ws.on_upgrade(move |down|async move {
  meter.status=101;let (mut ds,mut dr)=down.split();let (mut us,mut ur)=up.split();
  let relay=async {
   loop {tokio::select! {
    msg=dr.next()=>{match msg {
     Some(Ok(msg))=>{let converted=match msg {Message::Text(t)=>{meter.bytes_in+=t.len() as i64;UpMessage::Text(t.to_string().into())},Message::Binary(b)=>{meter.bytes_in+=b.len() as i64;UpMessage::Binary(b)},Message::Ping(b)=>UpMessage::Ping(b),Message::Pong(b)=>UpMessage::Pong(b),Message::Close(_)=>break};if us.send(converted).await.is_err(){break;}},_=>break
    }},
    msg=ur.next()=>{match msg {
     Some(Ok(msg))=>{let converted=match msg {UpMessage::Text(t)=>{meter.bytes_out+=t.len() as i64;Message::Text(t.to_string().into())},UpMessage::Binary(b)=>{meter.bytes_out+=b.len() as i64;Message::Binary(b)},UpMessage::Ping(b)=>Message::Ping(b),UpMessage::Pong(b)=>Message::Pong(b),UpMessage::Close(_)=>break,UpMessage::Frame(_)=>continue};if ds.send(converted).await.is_err(){break;}},_=>break
    }}
   }}
  };
  let _=tokio::time::timeout(std::time::Duration::from_secs(timeout),relay).await;
  let _=ds.close().await;let _=us.close().await;
  if let Err(e)=meter.finish().await {tracing::error!(error=%e,"WebSocket settlement failed");}
 }).into_response())
}
