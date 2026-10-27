//! Bounded inspection of upstream responses to extract billable usage while
//! the body streams through unchanged.
use crate::meter::Usage;
use serde_json::Value;

/// Largest SSE line or JSON body retained for parsing.
const CAPTURE_LIMIT: usize = 4 * 1024 * 1024;
/// Bytes kept from the end of a larger JSON body (for example inline base64
/// images) to find its trailing `usage` object.
const TAIL_LIMIT: usize = 256 * 1024;
/// Marks one inline generated image in an images API response.
const IMAGE_MARKER: &[u8] = b"\"b64_json\"";
/// Audio responses only need their header to determine the format.
const AUDIO_HEADER_LIMIT: usize = 64 * 1024;
/// OpenAI raw PCM output: 24 kHz, 16-bit, mono.
const PCM_BYTES_PER_SECOND: i64 = 48_000;

/// Bytes per second of 16-bit PCM from `rate=`/`channels=` content-type
/// parameters (for example `audio/pcm;rate=16000;channels=2`).
fn pcm_bytes_per_second(content_type: &str) -> i64 {
    let param = |name: &str| {
        content_type
            .split(';')
            .skip(1)
            .filter_map(|p| p.split_once('='))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .and_then(|(_, v)| v.trim().trim_matches('"').parse::<i64>().ok())
            .filter(|n| (1..=1_000_000).contains(n))
    };
    match (param("rate"), param("channels")) {
        (None, None) => PCM_BYTES_PER_SECOND,
        (rate, channels) => rate.unwrap_or(24_000) * channels.unwrap_or(1).min(16) * 2,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    EventStream,
    Json,
    Pcm,
    Wav,
    OtherAudio,
    Opaque,
}

impl Format {
    fn of(content_type: &str) -> Self {
        let ct = content_type.to_ascii_lowercase();
        if ct.contains("text/event-stream") {
            Self::EventStream
        } else if ct.contains("json") {
            Self::Json
        } else if ct.contains("pcm") || ct.contains("l16") {
            Self::Pcm
        } else if ct.contains("wav") || ct.contains("wave") {
            Self::Wav
        } else if ct.starts_with("audio/") {
            Self::OtherAudio
        } else {
            Self::Opaque
        }
    }
}

/// Observes response chunks and reports the usage they contain.
pub struct Inspector {
    format: Format,
    /// Current, incomplete SSE line.
    line: Vec<u8>,
    /// Set while skipping the rest of an oversized SSE line.
    skipping_line: bool,
    /// Bounded prefix of a JSON body or audio header.
    capture: Vec<u8>,
    /// Set once a JSON body outgrows [`CAPTURE_LIMIT`]; `tail` then holds
    /// its last bytes.
    overflow: bool,
    tail: Vec<u8>,
    /// Inline images seen so far, and the bytes that may start a marker
    /// split across chunks.
    images: i64,
    marker_carry: Vec<u8>,
    bytes: i64,
    pcm_rate: i64,
    pub usage: Usage,
}

impl Inspector {
    pub fn new(content_type: &str) -> Self {
        Self {
            format: Format::of(content_type),
            line: Vec::new(),
            skipping_line: false,
            capture: Vec::new(),
            overflow: false,
            tail: Vec::new(),
            images: 0,
            marker_carry: Vec::new(),
            bytes: 0,
            pcm_rate: pcm_bytes_per_second(content_type),
            usage: Usage::default(),
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.bytes = self.bytes.saturating_add(chunk.len() as i64);
        match self.format {
            Format::EventStream => {
                for &byte in chunk {
                    if byte == b'\n' {
                        if !self.skipping_line {
                            self.parse_line();
                        }
                        self.line.clear();
                        self.skipping_line = false;
                    } else if !self.skipping_line {
                        if self.line.len() < CAPTURE_LIMIT {
                            self.line.push(byte);
                        } else {
                            self.line.clear();
                            self.skipping_line = true;
                        }
                    }
                }
            }
            Format::Json => {
                self.count_images(chunk);
                self.capture_json(chunk);
            }
            Format::Wav | Format::Pcm | Format::OtherAudio => {
                self.capture_prefix(chunk, AUDIO_HEADER_LIMIT);
            }
            Format::Opaque => {}
        }
    }

    /// Completes inspection at end of stream.
    pub fn finish(&mut self) {
        match self.format {
            Format::EventStream if !self.skipping_line => self.parse_line(),
            Format::Json if !self.overflow => {
                if let Ok(v) = serde_json::from_slice::<Value>(&self.capture) {
                    self.observe(&v);
                    self.usage.images = inline_images(&v);
                }
            }
            Format::Json => {
                if let Some(usage) = trailing_usage(&self.tail) {
                    self.usage = Usage::parse(&usage);
                }
                self.usage.images = self.images;
            }
            Format::Pcm => {
                self.usage.audio_ms = self.bytes.saturating_mul(1000) / self.pcm_rate;
            }
            Format::Wav => self.usage.audio_ms = wav_ms(&self.capture, self.bytes).unwrap_or(0),
            _ => {}
        }
    }

    /// Keeps the whole JSON body up to [`CAPTURE_LIMIT`], then only a tail.
    fn capture_json(&mut self, chunk: &[u8]) {
        if !self.overflow && self.capture.len() + chunk.len() <= CAPTURE_LIMIT {
            self.capture.extend_from_slice(chunk);
            return;
        }
        if !self.overflow {
            self.overflow = true;
            self.tail = std::mem::take(&mut self.capture);
        }
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > 2 * TAIL_LIMIT {
            let cut = self.tail.len() - TAIL_LIMIT;
            self.tail.drain(..cut);
        }
    }

    /// Counts [`IMAGE_MARKER`] occurrences across chunk boundaries.
    fn count_images(&mut self, chunk: &[u8]) {
        let keep = IMAGE_MARKER.len() - 1;
        let head = &chunk[..chunk.len().min(keep)];
        let mut window = std::mem::take(&mut self.marker_carry);
        window.extend_from_slice(head);
        // Markers that start in the carried bytes and end in this chunk.
        let mut found = count(&window, IMAGE_MARKER);
        found += count(chunk, IMAGE_MARKER);
        self.images = self.images.saturating_add(found as i64);
        let carry_from = chunk.len().saturating_sub(keep);
        if chunk.len() >= keep {
            self.marker_carry = chunk[carry_from..].to_vec();
        } else {
            window.drain(..window.len().saturating_sub(keep));
            self.marker_carry = window;
        }
    }

    fn capture_prefix(&mut self, chunk: &[u8], limit: usize) {
        let n = chunk.len().min(limit.saturating_sub(self.capture.len()));
        self.capture.extend_from_slice(&chunk[..n]);
    }

    fn parse_line(&mut self) {
        let line = self.line.strip_suffix(b"\r").unwrap_or(&self.line);
        if let Some(data) = line.strip_prefix(b"data:")
            && let Ok(v) = serde_json::from_slice::<Value>(data)
        {
            self.observe(&v);
        }
    }

    /// The last usage object wins: streaming APIs report cumulative usage.
    fn observe(&mut self, v: &Value) {
        if let Some(usage) = usage_object(v) {
            self.usage = Usage::parse(usage);
        }
    }
}

/// Finds a usage object in chat/completions, responses, or wrapped payloads.
fn usage_object(v: &Value) -> Option<&Value> {
    v.get("usage")
        .or_else(|| v.pointer("/response/usage"))
        .or_else(|| v.pointer("/data/usage"))
        .filter(|u| u.is_object())
}

/// Non-overlapping occurrences of `needle` in `haystack`.
fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

/// Generated images in an images API response (`data[]` entries carrying
/// `b64_json` or `url`).
fn inline_images(v: &Value) -> i64 {
    v["data"].as_array().map_or(0, |items| {
        items
            .iter()
            .filter(|i| i.get("b64_json").is_some() || i.get("url").is_some_and(Value::is_string))
            .count() as i64
    })
}

/// The last top-level-looking `"usage": {…}` object in the tail of a JSON
/// body too large to buffer.
fn trailing_usage(tail: &[u8]) -> Option<Value> {
    const KEY: &[u8] = b"\"usage\"";
    let mut end = tail.len();
    while let Some(pos) = tail[..end].windows(KEY.len()).rposition(|w| w == KEY) {
        let rest = &tail[pos + KEY.len()..];
        let rest = &rest[rest.iter().position(|b| !b.is_ascii_whitespace())?..];
        if let Some(object) = rest.strip_prefix(b":")
            && let Some(Ok(value)) = serde_json::Deserializer::from_slice(object)
                .into_iter::<Value>()
                .next()
            && value.is_object()
        {
            return Some(value);
        }
        end = pos;
    }
    None
}

/// Usage carried by a realtime WebSocket server event. Each `response.done`
/// reports the usage of one response, so callers accumulate them.
pub fn realtime_usage(event: &str) -> Option<Usage> {
    // Cheap pre-filter: most realtime events are audio deltas.
    if !event.contains("response.done") {
        return None;
    }
    let v: Value = serde_json::from_str(event).ok()?;
    if v["type"] != "response.done" {
        return None;
    }
    usage_object(&v).map(Usage::parse)
}

/// Duration of a (possibly truncated) WAV stream in milliseconds, using the
/// `fmt ` chunk's sample rate and the total streamed byte count.
fn wav_ms(header: &[u8], total_bytes: i64) -> Option<i64> {
    if header.get(..4)? != b"RIFF" || header.get(8..12)? != b"WAVE" {
        return None;
    }
    let mut pos = 12;
    let mut byte_rate = None;
    while pos + 8 <= header.len() {
        let len = u32::from_le_bytes(header.get(pos + 4..pos + 8)?.try_into().ok()?) as usize;
        match &header[pos..pos + 4] {
            b"fmt " if len >= 16 => {
                byte_rate = Some(i64::from(u32::from_le_bytes(
                    header.get(pos + 16..pos + 20)?.try_into().ok()?,
                )));
            }
            b"data" => {
                let rate = byte_rate.filter(|n| *n > 0)?;
                return Some((total_bytes - (pos + 8) as i64).max(0) * 1000 / rate);
            }
            _ => {}
        }
        pos = pos.checked_add(8 + len + (len % 2))?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realtime_usage_reads_response_done_only() {
        let done = r#"{"type":"response.done","response":{"usage":{"input_tokens":5,"output_tokens":7,"total_tokens":12}}}"#;
        let usage = realtime_usage(done).unwrap();
        assert_eq!((usage.input, usage.output, usage.total), (5, 7, 12));
        assert!(realtime_usage(r#"{"type":"response.audio.delta","delta":"AAAA"}"#).is_none());
        assert!(
            realtime_usage(r#"{"type":"conversation.item.created","note":"response.done"}"#)
                .is_none()
        );
    }

    #[test]
    fn large_image_responses_keep_trailing_usage_and_count_images() {
        let image = "A".repeat(CAPTURE_LIMIT + 1024);
        let body = format!(
            r#"{{"created":1,"data":[{{"b64_json":"{image}","media_type":"image/webp"}},{{"b64_json":"{image}"}}],"usage":{{"prompt_tokens":9,"completion_tokens":642,"total_tokens":651,"cost":0.01}}}}"#
        );
        let mut inspector = Inspector::new("application/json");
        // Odd chunk sizes split the image marker across chunk boundaries.
        for chunk in body.as_bytes().chunks(4093) {
            inspector.feed(chunk);
        }
        inspector.finish();
        assert_eq!(inspector.usage.total, 651);
        assert_eq!(inspector.usage.cost, Some(10_000));
        assert_eq!(inspector.usage.images, 2);
    }

    #[test]
    fn small_image_responses_count_data_entries() {
        let mut inspector = Inspector::new("application/json");
        inspector.feed(
            br#"{"data":[{"url":"https://x/1.png"},{"b64_json":"AA"}],"usage":{"total_tokens":3}}"#,
        );
        inspector.finish();
        assert_eq!(inspector.usage.images, 2);
        let mut models = Inspector::new("application/json");
        models.feed(br#"{"data":[{"id":"m","object":"model"}]}"#);
        models.finish();
        assert_eq!(models.usage.images, 0);
    }

    #[test]
    fn marker_counting_handles_every_split() {
        let body = br#"{"data":[{"b64_json":"x"},{"b64_json":"y"}]}"#;
        for size in 1..body.len() {
            let mut inspector = Inspector::new("application/json");
            for chunk in body.chunks(size) {
                inspector.count_images(chunk);
            }
            assert_eq!(inspector.images, 2, "chunk size {size}");
        }
    }

    #[test]
    fn pcm_duration_uses_content_type_parameters() {
        for (content_type, ms) in [
            ("audio/pcm", 2800),
            ("audio/pcm;rate=24000;channels=1", 2800),
            ("audio/L16; rate=16000; channels=2", 2100),
        ] {
            let mut inspector = Inspector::new(content_type);
            inspector.feed(&vec![0; 134_400]);
            inspector.finish();
            assert_eq!(inspector.usage.audio_ms, ms, "{content_type}");
        }
    }

    #[test]
    fn crlf_terminated_sse_lines_are_parsed() {
        let mut inspector = Inspector::new("text/event-stream");
        inspector.feed(b"data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\r\n\r\n");
        inspector.finish();
        assert_eq!(inspector.usage.total, 3);
    }
}
