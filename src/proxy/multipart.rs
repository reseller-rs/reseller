//! Streaming `multipart/form-data` rewriter that applies the model policy to
//! uploads (for example speech-to-text) without buffering file payloads.

/// Streaming rewrite of a multipart/form-data body: the `model` form field is
/// replaced (Force) or injected when missing (Default), while all other parts
/// — including file payloads — stream through untouched. Opaque multipart
/// uploads therefore follow the same model policy as JSON requests.
pub struct Multipart {
    marker: Vec<u8>,
    delim: Vec<u8>,
    model: Vec<u8>,
    replace: bool,
    inject: bool,
    model_seen: bool,
    leading_crlf: bool,
    discard: bool,
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
            state: Part::Preamble,
            buf: Vec::new(),
        }
    }
    pub fn model_seen(&self) -> bool {
        self.model_seen
    }
    /// Feed one input chunk; returns the bytes that should be forwarded.
    pub fn push(&mut self, chunk: &[u8]) -> std::result::Result<Vec<u8>, String> {
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
    fn step(&mut self, out: &mut Vec<u8>) -> std::result::Result<bool, String> {
        match self.state {
            Part::Preamble => {
                let Some(i) = find(&self.buf, &self.marker) else {
                    // Preamble is ignored per RFC 2046; retain only a tail that
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
                        if self.leading_crlf {
                            // Reuse the leading CRLF as the separator before
                            // the injected part.
                            out.extend_from_slice(b"\r\n");
                        } else {
                            // Empty body: open the injected part with the
                            // marker that is already buffered.
                            out.extend_from_slice(&self.buf[..tail]);
                        }
                        out.extend_from_slice(
                            b"Content-Disposition: form-data; name=\"model\"\r\n\r\n",
                        );
                        out.extend_from_slice(&self.model);
                        out.extend_from_slice(b"\r\n");
                        out.extend_from_slice(&self.marker);
                        out.extend_from_slice(b"--");
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
                        return Err("multipart part headers exceed limit".into());
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
                        self.buf.drain(..cut);
                    }
                    return Ok(false);
                };
                if !self.discard {
                    out.extend_from_slice(&self.buf[..i]);
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
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.contains(
                "\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nconfigured\r\n--b--\r\n"
            ),
            "{s}"
        );
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
    #[test]
    fn quoted_boundary_is_parsed() {
        assert_eq!(
            boundary("multipart/form-data; boundary=\"a-1\"; charset=utf-8").as_deref(),
            Some("a-1")
        );
        assert_eq!(boundary("application/json"), None);
    }
}
