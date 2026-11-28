//! White-labels successful JSON and SSE responses.
//!
//! An upstream names its own model (`"model":"vendor/model:free"`) and may add
//! provider metadata (OpenRouter's top-level `"provider"`). Customers see the
//! public model id instead, and no provider key. Everything else, including the
//! byte framing of an event stream, passes through untouched.
//!
//! The [`Rewriter`] sees the body chunk by chunk and returns the bytes to send.
//! The caller keeps feeding the original bytes to the usage inspector.

use bytes::Bytes;
use serde_json::Value;

/// Longest SSE line held back to be rewritten. A longer line is passed through
/// unmodified, so a single huge line is never buffered without limit.
const MAX_LINE: usize = 256 * 1024;

/// How a response body is rewritten.
pub enum Rewriter {
    /// Bytes are relayed as they are.
    Passthrough,
    /// A plain JSON body, buffered up to a cap and rewritten at the end.
    Json(Json),
    /// An event stream, rewritten line by line.
    Sse(Sse),
}

impl Rewriter {
    /// The rewriter for a response with `content_type` and `content_encoding`.
    ///
    /// Only JSON and `text/event-stream` bodies are rewritten, and only when
    /// they are not compressed. `json_cap` bounds the buffered JSON body.
    pub fn new(
        content_type: &str,
        content_encoding: Option<&str>,
        model: &str,
        json_cap: usize,
    ) -> Self {
        let encoded = content_encoding
            .map(str::trim)
            .is_some_and(|e| !e.is_empty() && !e.eq_ignore_ascii_case("identity"));
        let mime = content_type.split(';').next().unwrap_or("").trim();
        if encoded {
            Self::Passthrough
        } else if mime.eq_ignore_ascii_case("text/event-stream") {
            Self::Sse(Sse {
                model: model.to_owned(),
                line: Vec::new(),
                skipping: false,
            })
        } else if mime.eq_ignore_ascii_case("application/json") || mime.ends_with("+json") {
            Self::Json(Json {
                model: model.to_owned(),
                cap: json_cap,
                buf: Vec::new(),
                overflowed: false,
            })
        } else {
            Self::Passthrough
        }
    }

    /// Takes the next chunk of the upstream body and returns what to send now.
    pub fn feed(&mut self, chunk: Bytes) -> Bytes {
        match self {
            Self::Passthrough => chunk,
            Self::Json(json) => json.feed(chunk),
            Self::Sse(sse) => sse.feed(&chunk),
        }
    }

    /// The upstream body ended: returns what is still held back.
    pub fn finish(&mut self) -> Bytes {
        match self {
            Self::Passthrough => Bytes::new(),
            Self::Json(json) => json.finish(),
            Self::Sse(sse) => sse.finish(),
        }
    }
}

/// Whether the text may contain a key the rewrite touches. Cheap pre-check so
/// untouched payloads keep their exact bytes.
fn mentions(text: &[u8]) -> bool {
    let has = |needle: &[u8]| text.windows(needle.len()).any(|w| w == needle);
    has(b"\"model\"") || has(b"\"provider\"")
}

/// Sets the public model and drops the provider. Returns whether the value
/// changed. Covers the top level and `response.model` of Responses API events.
fn rewrite_value(value: &mut Value, model: &str) -> bool {
    let mut changed = false;
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    changed |= object.remove("provider").is_some();
    let set = |target: &mut serde_json::Map<String, Value>| {
        if let Some(slot) = target.get_mut("model")
            && slot.as_str().is_some_and(|m| m != model)
        {
            *slot = model.into();
            return true;
        }
        false
    };
    changed |= set(object);
    if let Some(inner) = object.get_mut("response").and_then(Value::as_object_mut) {
        changed |= set(inner);
    }
    changed
}

/// Rewrites a JSON document, or `None` when nothing changes or it is not JSON.
fn rewrite_document(text: &[u8], model: &str) -> Option<Vec<u8>> {
    if !mentions(text) {
        return None;
    }
    let mut value: Value = serde_json::from_slice(text).ok()?;
    if rewrite_value(&mut value, model) {
        serde_json::to_vec(&value).ok()
    } else {
        None
    }
}

/// A plain JSON body.
pub struct Json {
    model: String,
    cap: usize,
    buf: Vec<u8>,
    /// The body outgrew the cap: it streams through unmodified.
    overflowed: bool,
}

impl Json {
    fn feed(&mut self, chunk: Bytes) -> Bytes {
        if self.overflowed {
            return chunk;
        }
        if self.buf.len() + chunk.len() > self.cap {
            self.overflowed = true;
            let mut out = std::mem::take(&mut self.buf);
            out.extend_from_slice(&chunk);
            return Bytes::from(out);
        }
        self.buf.extend_from_slice(&chunk);
        Bytes::new()
    }

    fn finish(&mut self) -> Bytes {
        let body = std::mem::take(&mut self.buf);
        match rewrite_document(&body, &self.model) {
            Some(rewritten) => Bytes::from(rewritten),
            None => Bytes::from(body),
        }
    }
}

/// An event stream.
pub struct Sse {
    model: String,
    /// The current line, including its terminator once seen.
    line: Vec<u8>,
    /// The current line outgrew [`MAX_LINE`]: the rest of it is passed through.
    skipping: bool,
}

impl Sse {
    fn feed(&mut self, mut rest: &[u8]) -> Bytes {
        let mut out = Vec::with_capacity(rest.len());
        while !rest.is_empty() {
            let end = rest.iter().position(|&b| b == b'\n').map(|i| i + 1);
            let (segment, tail) = rest.split_at(end.unwrap_or(rest.len()));
            rest = tail;
            if self.skipping {
                out.extend_from_slice(segment);
                self.skipping = end.is_none();
            } else if self.line.len() + segment.len() > MAX_LINE {
                out.append(&mut self.line);
                out.extend_from_slice(segment);
                self.skipping = end.is_none();
            } else {
                self.line.extend_from_slice(segment);
                if end.is_some() {
                    self.flush_line(&mut out);
                }
            }
        }
        Bytes::from(out)
    }

    /// The stream ended inside a line: it is still rewritten.
    fn finish(&mut self) -> Bytes {
        let mut out = Vec::new();
        self.flush_line(&mut out);
        Bytes::from(out)
    }

    /// Appends the held line, rewritten when it is a JSON `data:` line.
    fn flush_line(&mut self, out: &mut Vec<u8>) {
        let line = std::mem::take(&mut self.line);
        let body_end = line
            .iter()
            .rposition(|&b| b != b'\n' && b != b'\r')
            .map_or(0, |i| i + 1);
        let (content, terminator) = line.split_at(body_end);
        if let Some(data) = content.strip_prefix(b"data:") {
            let spaces = data.iter().take_while(|&&b| b == b' ').count();
            let (gap, payload) = data.split_at(spaces);
            if payload.first() == Some(&b'{')
                && let Some(rewritten) = rewrite_document(payload, &self.model)
            {
                out.extend_from_slice(b"data:");
                out.extend_from_slice(gap);
                out.extend_from_slice(&rewritten);
                out.extend_from_slice(terminator);
                return;
            }
        }
        out.extend_from_slice(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPSTREAM: &str = "nvidia/nemotron-3:free";

    fn sse() -> Rewriter {
        Rewriter::new("text/event-stream; charset=utf-8", None, "public", 1024)
    }

    fn run(rewriter: &mut Rewriter, chunks: &[&[u8]]) -> String {
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend_from_slice(&rewriter.feed(Bytes::copy_from_slice(chunk)));
        }
        out.extend_from_slice(&rewriter.finish());
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn json_bodies_get_the_public_model_and_lose_the_provider() {
        let mut r = Rewriter::new("application/json", None, "public", 1024);
        let body = format!(
            r#"{{"id":"x","model":"{UPSTREAM}","provider":"Nvidia","usage":{{"total_tokens":3}}}}"#
        );
        let out = run(&mut r, &[&body.as_bytes()[..10], &body.as_bytes()[10..]]);
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["model"], "public");
        assert!(value.get("provider").is_none());
        assert_eq!(value["usage"]["total_tokens"], 3);
    }

    #[test]
    fn json_bodies_without_the_keys_keep_their_bytes() {
        let mut r = Rewriter::new("application/json", None, "public", 1024);
        let body = "{ \"data\" : [ 1.0 ,  2 ] }";
        assert_eq!(run(&mut r, &[body.as_bytes()]), body);
    }

    #[test]
    fn json_over_the_cap_passes_through_unmodified() {
        let mut r = Rewriter::new("application/json", None, "public", 16);
        let body = format!(r#"{{"model":"{UPSTREAM}","provider":"P"}}"#);
        assert_eq!(run(&mut r, &[body.as_bytes()]), body);
    }

    #[test]
    fn other_bodies_and_encodings_pass_through() {
        let body = format!(r#"{{"model":"{UPSTREAM}"}}"#);
        for (kind, encoding) in [
            ("audio/mpeg", None),
            ("text/plain", None),
            ("application/json", Some("gzip")),
        ] {
            let mut r = Rewriter::new(kind, encoding, "public", 1024);
            assert_eq!(run(&mut r, &[body.as_bytes()]), body);
        }
    }

    #[test]
    fn sse_lines_split_across_chunks_are_rewritten() {
        let event = format!(
            "data: {{\"model\":\"{UPSTREAM}\",\"provider\":\"Nvidia\",\"choices\":[]}}\n\n"
        );
        let bytes = event.as_bytes();
        let mut r = sse();
        let out = run(&mut r, &[&bytes[..7], &bytes[7..30], &bytes[30..]]);
        assert!(out.starts_with("data: {"));
        assert!(out.ends_with("}\n\n"));
        let value: Value =
            serde_json::from_str(out.trim().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(value["model"], "public");
        assert!(value.get("provider").is_none());
        assert_eq!(value["choices"], serde_json::json!([]));
    }

    #[test]
    fn sse_framing_done_and_non_json_lines_are_untouched() {
        let stream = format!(
            ": keep-alive\r\nevent: message\r\nid: 7\r\ndata: not json \"model\"\r\n\r\ndata: [DONE]\r\n\r\n{}",
            r#"data: {"model":"nvidia/x"}"#
        );
        let mut r = sse();
        // No trailing newline: the last line is still rewritten at the end.
        let out = run(&mut r, &[stream.as_bytes()]);
        assert_eq!(
            out,
            ": keep-alive\r\nevent: message\r\nid: 7\r\ndata: not json \"model\"\r\n\r\ndata: [DONE]\r\n\r\ndata: {\"model\":\"public\"}"
        );
        let crlf = "data: {\"model\":\"a\"}\r\n\r\n";
        assert_eq!(
            run(&mut sse(), &[crlf.as_bytes()]),
            "data: {\"model\":\"public\"}\r\n\r\n"
        );
    }

    #[test]
    fn responses_events_rewrite_the_nested_model() {
        let event = format!(
            "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"r\",\"model\":\"{UPSTREAM}\",\"status\":\"in_progress\"}}}}\n\n"
        );
        let out = run(&mut sse(), &[event.as_bytes()]);
        assert!(out.starts_with("event: response.created\ndata: "));
        assert!(!out.contains(UPSTREAM));
        let data = out.lines().nth(1).unwrap().strip_prefix("data: ").unwrap();
        let value: Value = serde_json::from_str(data).unwrap();
        assert_eq!(value["response"]["model"], "public");
        assert_eq!(value["response"]["status"], "in_progress");
    }

    #[test]
    fn an_oversize_line_is_passed_through_and_the_next_is_rewritten() {
        let huge = format!(
            "data: {{\"model\":\"{UPSTREAM}\",\"pad\":\"{}\"}}\n",
            "x".repeat(2 * MAX_LINE)
        );
        let next = "data: {\"model\":\"up\"}\n";
        let mut r = sse();
        let bytes = huge.as_bytes();
        let out = run(
            &mut r,
            &[
                &bytes[..1000],
                &bytes[1000..MAX_LINE + 500],
                &bytes[MAX_LINE + 500..],
                next.as_bytes(),
            ],
        );
        assert_eq!(out, format!("{huge}data: {{\"model\":\"public\"}}\n"));
    }

    #[test]
    fn the_model_is_only_replaced_when_it_is_a_string() {
        let mut value = serde_json::json!({"model": null, "provider": {"a": 1}});
        assert!(rewrite_value(&mut value, "public"));
        assert_eq!(value, serde_json::json!({"model": null}));
        let mut array = serde_json::json!([{"model": "x"}]);
        assert!(!rewrite_value(&mut array, "public"));
    }
}
