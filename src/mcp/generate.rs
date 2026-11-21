//! Generation tools: each builds an internal `/v1/*` request and runs it
//! through [`proxy::handle`], so admission, limits, metering, fallbacks and
//! model policy are exactly those of the HTTP API. The caller's credential
//! authenticates the internal request and is never sent upstream.
use super::{Ctx, Output, ToolError, internal, tools::Args};
use crate::{db, error::Error, money, proxy};
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use serde_json::{Value, json};
use sqlx::Row;

const MAX_TEXT_CHARS: usize = 200_000;

struct Reply {
    request_id: Option<String>,
    body: Bytes,
}

/// Largest response body read for JSON replies that may carry base64 media.
fn json_cap(max_media: usize) -> usize {
    max_media.saturating_mul(4) / 3 + 64 * 1024
}

async fn media_limit(ctx: &Ctx) -> usize {
    ctx.app.config().await.mcp.max_media_bytes
}

/// Runs `req` through the proxy and reads the whole response (at most
/// `cap` bytes). Non-2xx upstream answers become tool errors.
async fn send(ctx: &Ctx, req: Request<Body>, cap: usize) -> Result<Reply, ToolError> {
    let response = proxy::handle(ctx.app.clone(), req, ctx.ip.clone()).await?;
    let (parts, body) = response.into_parts();
    let request_id = parts
        .headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // The proxy settles the request before it closes the body, so once the
    // body has been read the billing row is final.
    let body = to_bytes(body, cap).await.map_err(|_| {
        ToolError::new(
            "response_too_large",
            "The response exceeded the server's MCP media limit or was interrupted; \
             the request may still have been billed (see list_requests)",
        )
    })?;
    if !parts.status.is_success() {
        return Err(upstream_error(parts.status, &body, request_id.as_deref()));
    }
    Ok(Reply { request_id, body })
}

fn upstream_error(status: StatusCode, body: &[u8], request_id: Option<&str>) -> ToolError {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let detail = parsed
        .as_ref()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .or_else(|| v["message"].as_str())
        })
        .map_or_else(|| String::from_utf8_lossy(body).into_owned(), str::to_owned);
    let detail: String = detail.chars().take(500).collect();
    let mut message = format!("The provider returned HTTP {}: {detail}", status.as_u16());
    if let Some(id) = request_id {
        message.push_str(&format!(" (request {id})"));
    }
    ToolError::new("upstream_error", message)
}

/// Billed cost, tokens and remaining balance for a finished request.
async fn billing(ctx: &Ctx, request_id: Option<&str>) -> Result<Value, ToolError> {
    let account = ctx
        .identity
        .as_ref()
        .map(|i| i.account_id.as_str())
        .unwrap_or_default();
    let mut conn = ctx.app.db.acquire().await?;
    let mut out = json!({});
    if let Some(id) = request_id {
        out["request_id"] = id.into();
        let row = sqlx::query(
            "SELECT model,tokens,billed_micro,estimated FROM requests WHERE id=? AND account_id=?",
        )
        .bind(id)
        .bind(account)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(row) = row {
            out["model"] = row.get::<String, _>("model").into();
            out["tokens"] = row.get::<i64, _>("tokens").into();
            out["billed_usd"] = money::display(row.get("billed_micro")).into();
            out["usage_estimated"] = (row.get::<i64, _>("estimated") != 0).into();
        }
    }
    out["balance_usd"] = money::display(db::balance(&mut conn, account).await?).into();
    Ok(out)
}

fn billing_line(billing: &Value) -> String {
    let mut line = String::new();
    if let Some(cost) = billing["billed_usd"].as_str() {
        line.push_str(&format!("Billed ${cost}"));
        if let Some(tokens) = billing["tokens"].as_i64().filter(|t| *t > 0) {
            line.push_str(&format!(" ({tokens} tokens)"));
        }
        line.push_str(". ");
    }
    line.push_str(&format!(
        "Remaining balance: ${}.",
        billing["balance_usd"].as_str().unwrap_or("unknown")
    ));
    line
}

fn post_json(ctx: &Ctx, path: &str, body: &Value) -> Result<Request<Body>, ToolError> {
    Ok(internal(
        Method::POST,
        path,
        ctx.bearer.as_deref(),
        Some("application/json"),
        Body::from(serde_json::to_vec(body).map_err(Error::internal)?),
    )?)
}

fn parse_json(reply: &Reply) -> Result<Value, ToolError> {
    serde_json::from_slice(&reply.body).map_err(|_| {
        ToolError::new(
            "invalid_upstream_response",
            "The provider returned a response that is not valid JSON",
        )
    })
}

/// Message text from a chat completion (string or content parts).
fn message_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
        _ => String::new(),
    }
}

pub(super) async fn text(ctx: &Ctx, arguments: &Value) -> Result<Output, ToolError> {
    let a = Args::new(
        arguments,
        &[
            "prompt",
            "messages",
            "system",
            "model",
            "max_tokens",
            "temperature",
        ],
    )?;
    let mut messages = Vec::new();
    if let Some(system) = a.str("system", 1, 50_000)? {
        messages.push(json!({"role": "system", "content": system}));
    }
    match (a.get("prompt"), a.get("messages")) {
        (Some(_), Some(_)) => {
            return Err(ToolError::invalid(
                "Provide either prompt or messages, not both",
            ));
        }
        (None, None) => return Err(ToolError::invalid("Provide prompt or messages")),
        (Some(_), None) => {
            let prompt = a.required_str("prompt", 1, MAX_TEXT_CHARS)?;
            messages.push(json!({"role": "user", "content": prompt}));
        }
        (None, Some(list)) => {
            let list = list
                .as_array()
                .filter(|l| (1..=100).contains(&l.len()))
                .ok_or_else(|| ToolError::invalid("messages must be an array of 1 to 100 items"))?;
            for item in list {
                let item = Args::new(item, &["role", "content"])?;
                let role = item
                    .one_of("role", &["system", "developer", "user", "assistant"])?
                    .ok_or_else(|| ToolError::invalid("every message needs a role"))?;
                let content = item.required_str("content", 0, MAX_TEXT_CHARS)?;
                messages.push(json!({"role": role, "content": content}));
            }
        }
    }
    let mut body = json!({"messages": messages, "stream": false});
    if let Some(model) = a.str("model", 1, 200)? {
        body["model"] = model.into();
    }
    if let Some(n) = a.int("max_tokens", 1, 131_072)? {
        body["max_tokens"] = n.into();
    }
    if let Some(t) = a.number("temperature", 0.0, 2.0)? {
        body["temperature"] = t.into();
    }
    let cap = json_cap(media_limit(ctx).await);
    let reply = send(ctx, post_json(ctx, "/v1/chat/completions", &body)?, cap).await?;
    let value = parse_json(&reply)?;
    let choice = &value["choices"][0];
    let text = message_text(&choice["message"]["content"]);
    let billing = billing(ctx, reply.request_id.as_deref()).await?;
    Ok(Output {
        content: vec![
            json!({"type": "text", "text": text}),
            json!({"type": "text", "text": billing_line(&billing)}),
        ],
        structured: Some(json!({
            "text": text,
            "model": value["model"],
            "finish_reason": choice["finish_reason"],
            "usage": value["usage"],
            "billing": billing,
        })),
    })
}

pub(super) async fn speech(ctx: &Ctx, arguments: &Value) -> Result<Output, ToolError> {
    const FORMATS: [(&str, &str); 6] = [
        ("mp3", "audio/mpeg"),
        ("wav", "audio/wav"),
        ("opus", "audio/ogg"),
        ("aac", "audio/aac"),
        ("flac", "audio/flac"),
        ("pcm", "audio/pcm"),
    ];
    let a = Args::new(arguments, &["text", "voice", "model", "format"])?;
    let format = a
        .one_of("format", &FORMATS.map(|(f, _)| f))?
        .unwrap_or("mp3");
    let mime = FORMATS
        .iter()
        .find(|(f, _)| *f == format)
        .map_or("audio/mpeg", |(_, m)| m);
    let mut body = json!({
        "input": a.required_str("text", 1, 4096)?,
        "response_format": format,
    });
    if let Some(voice) = a.str("voice", 1, 100)? {
        body["voice"] = voice.into();
    }
    if let Some(model) = a.str("model", 1, 200)? {
        body["model"] = model.into();
    }
    let max = media_limit(ctx).await;
    let reply = send(ctx, post_json(ctx, "/v1/audio/speech", &body)?, max).await?;
    if reply.body.is_empty() {
        return Err(ToolError::new(
            "invalid_upstream_response",
            "The provider returned no audio",
        ));
    }
    let billing = billing(ctx, reply.request_id.as_deref()).await?;
    Ok(Output {
        content: vec![
            json!({"type": "audio", "data": STANDARD.encode(&reply.body), "mimeType": mime}),
            json!({"type": "text", "text": billing_line(&billing)}),
        ],
        structured: Some(json!({
            "format": format,
            "mime_type": mime,
            "bytes": reply.body.len(),
            "billing": billing,
        })),
    })
}

/// A conservative MIME type from the file extension.
fn audio_mime(filename: &str) -> &'static str {
    match filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("mp3" | "mpga" | "mpeg") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("m4a" | "mp4") => "audio/mp4",
        Some("ogg" | "oga" | "opus") => "audio/ogg",
        Some("flac") => "audio/flac",
        Some("webm") => "audio/webm",
        _ => "application/octet-stream",
    }
}

/// File name safe to place in a Content-Disposition header.
fn safe_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '))
        .collect();
    if cleaned.trim_matches(['.', ' ']).is_empty() {
        "audio".to_owned()
    } else {
        cleaned
    }
}

pub(super) async fn transcribe(ctx: &Ctx, arguments: &Value) -> Result<Output, ToolError> {
    let a = Args::new(
        arguments,
        &["audio_base64", "filename", "model", "language"],
    )?;
    let max = media_limit(ctx).await;
    let encoded = a
        .get("audio_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid("audio_base64 is required"))?;
    if encoded.len() > max.saturating_mul(4) / 3 + 8 {
        return Err(ToolError::new(
            "media_too_large",
            format!("Audio exceeds the server limit of {max} bytes"),
        ));
    }
    let compact: String = encoded
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let audio = STANDARD
        .decode(compact.as_bytes())
        .map_err(|_| ToolError::invalid("audio_base64 is not valid base64"))?;
    if audio.is_empty() || audio.len() > max {
        return Err(ToolError::new(
            "media_too_large",
            format!("Audio must be between 1 and {max} bytes"),
        ));
    }
    let filename = safe_filename(a.required_str("filename", 1, 255)?);
    let mut fields = vec![("response_format", "json".to_owned())];
    if let Some(model) = a.str("model", 1, 200)? {
        if model.chars().any(char::is_control) {
            return Err(ToolError::invalid("model contains control characters"));
        }
        fields.push(("model", model.to_owned()));
    }
    if let Some(language) = a.str("language", 2, 12)? {
        if !language
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c == '-')
        {
            return Err(ToolError::invalid(
                "language must be an ISO-639 code such as en",
            ));
        }
        fields.push(("language", language.to_owned()));
    }
    let boundary = format!("reseller-mcp-{}", uuid::Uuid::new_v4().simple());
    let mut body = Vec::with_capacity(audio.len() + 512);
    for (name, value) in &fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"{filename}\"\r\nContent-Type: {}\r\n\r\n",
            audio_mime(&filename)
        )
        .as_bytes(),
    );
    body.extend_from_slice(&audio);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let req = internal(
        Method::POST,
        "/v1/audio/transcriptions",
        ctx.bearer.as_deref(),
        Some(&format!("multipart/form-data; boundary={boundary}")),
        Body::from(body),
    )?;
    let reply = send(ctx, req, json_cap(max)).await?;
    let (text, extra) = match serde_json::from_slice::<Value>(&reply.body) {
        Ok(v) => (v["text"].as_str().unwrap_or_default().to_owned(), Some(v)),
        Err(_) => (String::from_utf8_lossy(&reply.body).into_owned(), None),
    };
    let billing = billing(ctx, reply.request_id.as_deref()).await?;
    Ok(Output {
        content: vec![
            json!({"type": "text", "text": text}),
            json!({"type": "text", "text": billing_line(&billing)}),
        ],
        structured: Some(json!({
            "text": text,
            "language": extra.as_ref().map(|v| v["language"].clone()),
            "duration": extra.as_ref().map(|v| v["duration"].clone()),
            "billing": billing,
        })),
    })
}

/// MIME type of a generated image: a declared allowed type, else sniffed.
fn image_mime(declared: Option<&str>, bytes: &[u8]) -> &'static str {
    const ALLOWED: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];
    if let Some(m) = ALLOWED.iter().find(|m| Some(**m) == declared) {
        return m;
    }
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'G', b'I', b'F', ..] => "image/gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "image/webp",
        _ => "image/png",
    }
}

pub(super) async fn image(ctx: &Ctx, arguments: &Value) -> Result<Output, ToolError> {
    let a = Args::new(arguments, &["prompt", "model", "size", "n"])?;
    let mut body = json!({
        "prompt": a.required_str("prompt", 1, 4000)?,
        "n": a.int("n", 1, 4)?.unwrap_or(1),
    });
    if let Some(model) = a.str("model", 1, 200)? {
        body["model"] = model.into();
    }
    if let Some(size) = a.str("size", 1, 16)? {
        let (w, h) = size.split_once('x').unwrap_or(("", ""));
        let dimension =
            |s: &str| (2..=5).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
        if size != "auto" && !(dimension(w) && dimension(h)) {
            return Err(ToolError::invalid(
                "size must be auto or WIDTHxHEIGHT, e.g. 1024x1024",
            ));
        }
        body["size"] = size.into();
    }
    let max = media_limit(ctx).await;
    let reply = send(
        ctx,
        post_json(ctx, "/v1/images/generations", &body)?,
        json_cap(max),
    )
    .await?;
    let value = parse_json(&reply)?;
    let items = value["data"]
        .as_array()
        .filter(|d| !d.is_empty())
        .ok_or_else(|| {
            ToolError::new(
                "invalid_upstream_response",
                "The provider returned no images",
            )
        })?;
    let mut content = Vec::new();
    let mut described: Vec<Value> = Vec::new();
    let mut total = 0usize;
    for (index, item) in items.iter().enumerate() {
        let before = described.len();
        if let Some(data) = item["b64_json"].as_str() {
            let compact: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
            let bytes = STANDARD.decode(compact.as_bytes()).map_err(|_| {
                ToolError::new(
                    "invalid_upstream_response",
                    "The provider returned invalid image data",
                )
            })?;
            total = total.saturating_add(bytes.len());
            if total > max {
                return Err(ToolError::new(
                    "response_too_large",
                    format!(
                        "Generated images exceed the server limit of {max} bytes; \
                         the request was billed"
                    ),
                ));
            }
            let mime = image_mime(
                item["media_type"]
                    .as_str()
                    .or_else(|| item["mime_type"].as_str()),
                &bytes,
            );
            content.push(json!({"type": "image", "data": compact, "mimeType": mime}));
            described.push(json!({"index": index, "mime_type": mime, "bytes": bytes.len()}));
        } else if let Some(link) = item["url"].as_str().and_then(|u| url::Url::parse(u).ok()) {
            // Returned as a link only: fetching provider URLs here would be SSRF.
            if matches!(link.scheme(), "http" | "https") {
                content.push(
                    json!({"type": "text", "text": format!("Image {} URL: {link}", index + 1)}),
                );
                described.push(json!({"index": index, "url": link.as_str()}));
            }
        }
        if described.len() > before
            && let Some(prompt) = item["revised_prompt"].as_str()
        {
            described[before]["revised_prompt"] = prompt.into();
        }
    }
    if described.is_empty() {
        return Err(ToolError::new(
            "invalid_upstream_response",
            "The provider returned no usable images",
        ));
    }
    let billing = billing(ctx, reply.request_id.as_deref()).await?;
    content.push(json!({"type": "text", "text": billing_line(&billing)}));
    Ok(Output {
        content,
        structured: Some(json!({"images": described, "billing": billing})),
    })
}

pub(super) async fn models(ctx: &Ctx, arguments: &Value) -> Result<Output, ToolError> {
    Args::new(arguments, &[])?;
    let req = internal(
        Method::GET,
        "/v1/models",
        ctx.bearer.as_deref(),
        None,
        Body::empty(),
    )?;
    let reply = send(ctx, req, json_cap(media_limit(ctx).await)).await?;
    let value = parse_json(&reply)?;
    let models: Vec<Value> = value["data"]
        .as_array()
        .map(|items| items.iter().filter_map(|m| m.get("id").cloned()).collect())
        .unwrap_or_default();
    Ok(Output::json(json!({"models": models})))
}
