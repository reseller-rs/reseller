//! Streaming `multipart/form-data` rewriter that applies the model policy to
//! uploads (for example speech-to-text) without buffering file payloads.
use crate::error::{Error, Result};

/// Maps the model name a client sent to the one forwarded.
type Resolver = Box<dyn Fn(&str) -> Result<String> + Send + Sync>;

/// Longest accepted `model` form value, in bytes (matches the allowlist entry
/// limit).
pub const MODEL_LIMIT: usize = 256;

/// Streaming parser of a multipart/form-data body. The `model` form field is
/// replaced (Force) or injected when missing (Default), or left alone
/// (passthrough: `replace` and `inject` both false). All other parts,
/// including file payloads, stream through untouched. Whatever the policy,
/// the effective model value is reported through [`Multipart::take_models`]
/// as soon as its part is complete, so it can be metered and checked against
/// key allowlists before the upload finishes.
pub struct Multipart {
    marker: Vec<u8>,
    delim: Vec<u8>,
    model: Vec<u8>,
    replace: bool,
    inject: bool,
    model_seen: bool,
    leading_crlf: bool,
    discard: bool,
    /// Client value of the model part being read (not replaced).
    capture: Option<Vec<u8>>,
    /// Effective model values completed since the last `take_models`.
    observed: Vec<String>,
    /// Maps the client's model, which is then held back until its part ends.
    resolver: Option<Resolver>,
    /// Models the resolver produced since the last `take_resolved`.
    resolved: Vec<String>,
    state: Part,
    buf: Vec<u8>,
}
#[derive(PartialEq)]
enum Part {
    Preamble,
    Marker,
    Headers,
    Body,
    Epilogue,
}
const PART_HEADER_LIMIT: usize = 64 * 1024;
impl Multipart {
    pub fn new(boundary: &str, model: &str, replace: bool, inject: bool) -> Self {
        Self {
            marker: format!("--{boundary}").into_bytes(),
            delim: format!("\r\n--{boundary}").into_bytes(),
            model: model.as_bytes().to_vec(),
            replace,
            inject,
            model_seen: false,
            leading_crlf: false,
            discard: false,
            capture: None,
            observed: Vec::new(),
            resolver: None,
            resolved: Vec::new(),
            state: Part::Preamble,
            buf: Vec::new(),
        }
    }
    /// Rewrites the model the client sent with `resolver`. Its error aborts
    /// the body. The value is held until its part is complete, then replaced.
    #[must_use]
    pub fn with_resolver(
        mut self,
        resolver: impl Fn(&str) -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        self.resolver = Some(Box::new(resolver));
        self
    }
    /// Effective model values whose parts completed since the last call, in
    /// body order. An injected model is reported when the body closes.
    pub fn take_models(&mut self) -> Vec<String> {
        std::mem::take(&mut self.observed)
    }
    /// Models the resolver produced for completed parts since the last call.
    /// They are not in [`Multipart::take_models`].
    pub fn take_resolved(&mut self) -> Vec<String> {
        std::mem::take(&mut self.resolved)
    }
    /// Feeds one input chunk. Returns the bytes that should be forwarded.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(chunk.len() + self.model.len() + 64);
        while self.step(&mut out)? {}
        Ok(out)
    }
    /// Flush at end of input. A truncated model part is dropped rather than
    /// forwarded after its replacement.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.state == Part::Body && self.discard {
            self.buf.clear();
        }
        std::mem::take(&mut self.buf)
    }
    fn step(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        match self.state {
            Part::Preamble => {
                let Some(i) = find(&self.buf, &self.marker) else {
                    // The preamble is ignored per RFC 2046. Retain only a tail that
                    // could still hold the first boundary.
                    if self.buf.len() > self.marker.len() {
                        let cut = self.buf.len() - self.marker.len();
                        self.buf.drain(..cut);
                    }
                    return Ok(false);
                };
                self.buf.drain(..i);
                self.leading_crlf = false;
                self.state = Part::Marker;
                Ok(true)
            }
            Part::Marker => {
                let start = if self.leading_crlf { 2 } else { 0 };
                let tail = start + self.marker.len();
                if self.buf.len() < tail + 2 {
                    return Ok(false);
                }
                if &self.buf[tail..tail + 2] == b"\r\n" {
                    out.extend_from_slice(&self.buf[..tail + 2]);
                    self.buf.drain(..tail + 2);
                    self.discard = false;
                    self.state = Part::Headers;
                } else if &self.buf[tail..tail + 2] == b"--" {
                    // Closing boundary: inject the configured model part when
                    // the client never supplied one.
                    if self.inject && !self.model_seen {
                        // Open a new part (delimiter line, then headers)
                        // before the closing delimiter.
                        out.extend_from_slice(&self.buf[..start]);
                        out.extend_from_slice(&self.marker);
                        out.extend_from_slice(
                            b"\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n",
                        );
                        out.extend_from_slice(&self.model);
                        out.extend_from_slice(b"\r\n");
                        out.extend_from_slice(&self.marker);
                        out.extend_from_slice(b"--");
                        self.observed
                            .push(String::from_utf8_lossy(&self.model).into_owned());
                    } else {
                        out.extend_from_slice(&self.buf[..tail + 2]);
                    }
                    self.buf.drain(..tail + 2);
                    self.state = Part::Epilogue;
                } else {
                    // Malformed body: stop rewriting and stream it unchanged.
                    out.append(&mut self.buf);
                    self.state = Part::Epilogue;
                }
                Ok(true)
            }
            Part::Headers => {
                let Some(i) = find(&self.buf, b"\r\n\r\n") else {
                    if self.buf.len() > PART_HEADER_LIMIT {
                        return Err(Error::bad("multipart part headers exceed limit"));
                    }
                    return Ok(false);
                };
                let name = part_name(&self.buf[..i]);
                out.extend_from_slice(&self.buf[..i + 4]);
                self.buf.drain(..i + 4);
                if name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case("model"))
                {
                    self.model_seen = true;
                    if self.replace {
                        out.extend_from_slice(&self.model);
                        self.discard = true;
                    } else {
                        self.capture = Some(Vec::new());
                        self.discard = self.resolver.is_some();
                    }
                }
                self.state = Part::Body;
                Ok(true)
            }
            Part::Body => {
                let Some(i) = find(&self.buf, &self.delim) else {
                    // Hold back a possible split delimiter before forwarding.
                    let keep = self.delim.len();
                    if self.buf.len() > keep {
                        let cut = self.buf.len() - keep;
                        if !self.discard {
                            out.extend_from_slice(&self.buf[..cut]);
                        }
                        self.capture_value(cut)?;
                        self.buf.drain(..cut);
                    }
                    return Ok(false);
                };
                if !self.discard {
                    out.extend_from_slice(&self.buf[..i]);
                }
                self.capture_value(i)?;
                if let Some(value) = self.capture.take() {
                    let value = String::from_utf8(value)
                        .map_err(|_| Error::bad("model must be valid UTF-8"))?;
                    match self.resolver.as_ref().filter(|_| !value.is_empty()) {
                        Some(resolve) => {
                            let upstream = resolve(&value)?;
                            out.extend_from_slice(upstream.as_bytes());
                            self.resolved.push(upstream);
                        }
                        None => {
                            if self.discard {
                                out.extend_from_slice(value.as_bytes());
                            }
                            self.observed.push(value);
                        }
                    }
                } else if self.discard {
                    self.observed
                        .push(String::from_utf8_lossy(&self.model).into_owned());
                }
                self.buf.drain(..i);
                self.leading_crlf = true;
                self.state = Part::Marker;
                Ok(true)
            }
            Part::Epilogue => {
                out.append(&mut self.buf);
                Ok(false)
            }
        }
    }
}
impl Multipart {
    /// Appends `self.buf[..end]` to the model value being captured.
    fn capture_value(&mut self, end: usize) -> Result<()> {
        if let Some(value) = &mut self.capture {
            value.extend_from_slice(&self.buf[..end]);
            if value.len() > MODEL_LIMIT {
                return Err(Error::bad(format!("model exceeds {MODEL_LIMIT} bytes")));
            }
        }
        Ok(())
    }
}
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        i += haystack[i..].iter().position(|&b| b == first)?;
        if i + needle.len() > haystack.len() {
            return None;
        }
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}
fn part_name(headers: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(headers).ok()?;
    for line in text.split("\r\n") {
        let lower = line.to_ascii_lowercase();
        if !lower.starts_with("content-disposition:") {
            continue;
        }
        if let Some(pos) = lower.find("name=") {
            let value = line[pos + 5..].trim_start();
            let name = match value.strip_prefix('"') {
                Some(v) => v.split('"').next().unwrap_or(""),
                None => value
                    .split(|c: char| c == ';' || c.is_whitespace())
                    .next()
                    .unwrap_or(""),
            };
            return Some(name.to_owned());
        }
    }
    None
}
/// Extracts the boundary parameter from a `multipart/form-data` content type.
pub fn boundary(content_type: &str) -> Option<String> {
    let mut params = content_type.split(';');
    if !params
        .next()?
        .trim()
        .eq_ignore_ascii_case("multipart/form-data")
    {
        return None;
    }
    for p in params {
        let Some((k, v)) = p.trim().split_once('=') else {
            continue;
        };
        if !k.trim().eq_ignore_ascii_case("boundary") {
            continue;
        }
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(v);
        if !v.is_empty() {
            return Some(v.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn body() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(
            b"--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n",
        );
        b.extend_from_slice(
            b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\n",
        );
        b.extend_from_slice(b"\x00\x01AUDIO\xff");
        b.extend_from_slice(b"\r\n--b--\r\n");
        b
    }
    fn rewrite(chunks: &[Vec<u8>], model: &str, replace: bool, inject: bool) -> Vec<u8> {
        let mut m = Multipart::new("b", model, replace, inject);
        let mut out = Vec::new();
        for c in chunks {
            out.extend(m.push(c).unwrap());
        }
        out.extend(m.finish());
        out
    }
    #[test]
    fn force_replaces_model_and_keeps_payload() {
        let out = rewrite(&[body()], "configured", true, true);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.contains("name=\"model\"\r\n\r\nconfigured\r\n--b\r\n"),
            "{s}"
        );
        assert!(!s.contains("whisper-1"), "{s}");
        assert!(
            out.windows(8).any(|w| w == &b"\x00\x01AUDIO\xff"[..]),
            "audio payload must survive"
        );
        assert!(s.ends_with("--b--\r\n"), "{s}");
    }
    #[test]
    fn force_injects_model_when_missing() {
        let mut b = Vec::new();
        b.extend_from_slice(
            b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\r\n",
        );
        b.extend_from_slice(b"AUDIO");
        b.extend_from_slice(b"\r\n--b--\r\n");
        let out = rewrite(&[b], "configured", true, true);
        assert_eq!(
            String::from_utf8_lossy(&out),
            "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\r\nAUDIO\r\n\
             --b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nconfigured\r\n--b--\r\n"
        );
        // A body without parts gains exactly one model part.
        let out = rewrite(&[b"--b--\r\n".to_vec()], "configured", true, true);
        assert_eq!(
            String::from_utf8_lossy(&out),
            "--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nconfigured\r\n--b--\r\n"
        );
    }
    #[test]
    fn injected_body_is_identical_for_every_split() {
        let body =
            b"--b\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nX\r\n--b--\r\n".to_vec();
        let expected = rewrite(std::slice::from_ref(&body), "configured", false, true);
        assert!(String::from_utf8_lossy(&expected).contains(
            "X\r\n--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nconfigured\r\n--b--"
        ));
        for split in 1..body.len() {
            let chunks = vec![body[..split].to_vec(), body[split..].to_vec()];
            assert_eq!(
                rewrite(&chunks, "configured", false, true),
                expected,
                "split {split}"
            );
        }
    }
    #[test]
    fn default_fills_missing_but_keeps_client_model() {
        let out = rewrite(&[body()], "configured", false, true);
        assert!(String::from_utf8_lossy(&out).contains("whisper-1"));
        let mut b = Vec::new();
        b.extend_from_slice(
            b"--b\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nX\r\n--b--\r\n",
        );
        let out = rewrite(&[b], "configured", false, true);
        assert!(String::from_utf8_lossy(&out).contains("name=\"model\"\r\n\r\nconfigured\r\n"));
    }
    #[test]
    fn every_chunk_split_reassembles_identically() {
        let body = body();
        let expected = rewrite(std::slice::from_ref(&body), "configured", true, true);
        for split in 1..body.len() {
            let chunks = vec![body[..split].to_vec(), body[split..].to_vec()];
            assert_eq!(
                rewrite(&chunks, "configured", true, true),
                expected,
                "split at {split}"
            );
        }
        // Byte-at-a-time streaming must also survive.
        let chunks: Vec<Vec<u8>> = body.iter().map(|b| vec![*b]).collect();
        assert_eq!(rewrite(&chunks, "configured", true, true), expected);
    }
    /// The model is reported once its part completes, under every policy and
    /// every chunking, and passthrough forwards the body byte for byte.
    #[test]
    fn model_is_observed_for_every_split_and_policy() {
        let body = body();
        for (model, replace, inject, want) in [
            ("configured", true, true, "configured"),
            ("configured", false, true, "whisper-1"),
            ("", false, false, "whisper-1"),
        ] {
            for split in 1..body.len() {
                let mut m = Multipart::new("b", model, replace, inject);
                let mut out = m.push(&body[..split]).unwrap();
                let mut seen = m.take_models();
                out.extend(m.push(&body[split..]).unwrap());
                out.extend(m.finish());
                seen.extend(m.take_models());
                assert_eq!(seen, [want], "split {split}");
                if !replace && !inject {
                    assert_eq!(out, body, "passthrough must not rewrite (split {split})");
                }
            }
        }
    }
    #[test]
    fn injected_model_is_observed_and_oversize_is_rejected() {
        let mut m = Multipart::new("b", "configured", false, true);
        m.push(b"--b\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nX\r\n--b--\r\n")
            .unwrap();
        assert_eq!(m.take_models(), ["configured"]);
        let mut m = Multipart::new("b", "", false, false);
        let mut huge = b"--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n".to_vec();
        huge.extend(vec![b'x'; MODEL_LIMIT + 32]);
        assert!(m.push(&huge).is_err());
    }
    #[test]
    fn quoted_boundary_is_parsed() {
        assert_eq!(
            boundary("multipart/form-data; boundary=\"a-1\"; charset=utf-8").as_deref(),
            Some("a-1")
        );
        assert_eq!(boundary("application/json"), None);
    }
}
