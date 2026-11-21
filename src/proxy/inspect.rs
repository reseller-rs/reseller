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
/// Marks one inline image of a chat-completion response (a data URL).
const CHAT_IMAGE_MARKER: &[u8] = b"data:image/";
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

/// Counts occurrences of a byte marker across chunk boundaries.
struct MarkerCounter {
    marker: &'static [u8],
    /// Trailing bytes of the previous chunk that may begin a marker.
    carry: Vec<u8>,
    found: i64,
}

impl MarkerCounter {
    fn new(marker: &'static [u8]) -> Self {
        Self {
            marker,
            carry: Vec::new(),
            found: 0,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        let keep = self.marker.len() - 1;
        let head = &chunk[..chunk.len().min(keep)];
        let mut window = std::mem::take(&mut self.carry);
        window.extend_from_slice(head);
        // Markers that start in the carried bytes and end in this chunk.
        let found = count(&window, self.marker) + count(chunk, self.marker);
        self.found = self.found.saturating_add(found as i64);
        if chunk.len() >= keep {
            self.carry = chunk[chunk.len() - keep..].to_vec();
        } else {
            window.drain(..window.len().saturating_sub(keep));
            self.carry = window;
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
    /// Image markers in a JSON body, used only when it is too large to parse.
    b64_markers: MarkerCounter,
    url_markers: MarkerCounter,
    bytes: i64,
    pcm_rate: i64,
    /// The last `usage` object the upstream reported.
    reported: Option<Usage>,
    audio_ms: i64,
    images: i64,
    characters: i64,
    /// Estimated request input tokens; `Some` enables the estimation
    /// fallback when the upstream reports no usage.
    estimate_input: Option<i64>,
    /// Unicode characters of generated text seen so far (estimation only).
    generated_chars: i64,
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
            b64_markers: MarkerCounter::new(IMAGE_MARKER),
            url_markers: MarkerCounter::new(CHAT_IMAGE_MARKER),
            bytes: 0,
            pcm_rate: pcm_bytes_per_second(content_type),
            reported: None,
            audio_ms: 0,
            images: 0,
            characters: 0,
            estimate_input: None,
            generated_chars: 0,
        }
    }

    /// Bills `characters` of text-to-speech input.
    #[must_use]
    pub fn with_characters(mut self, characters: i64) -> Self {
        self.characters = characters.max(0);
        self
    }

    /// Estimates usage from `input_tokens` and the generated text when the
    /// upstream reports none.
    #[must_use]
    pub fn with_estimate(mut self, input_tokens: i64) -> Self {
        self.estimate_input = Some(input_tokens.max(0));
        self
    }

    /// Usage observed so far.
    pub fn usage(&self) -> Usage {
        let mut usage = self.reported.clone().unwrap_or_default();
        if self.audio_ms > 0 {
            usage.audio_ms = self.audio_ms;
        }
        usage.images = self.images;
        usage.characters = self.characters;
        if self.reported.is_none()
            && let Some(input) = self.estimate_input
        {
            usage.input = input;
            usage.output = tokens_for_chars(self.generated_chars);
            usage.total = usage.input.saturating_add(usage.output);
            usage.estimated = true;
        }
        usage
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
                self.b64_markers.feed(chunk);
                self.url_markers.feed(chunk);
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
                    self.images = inline_images(&v);
                    if self.estimate_input.is_some() {
                        self.generated_chars = final_chars(&v);
                    }
                }
            }
            Format::Json => {
                if let Some(usage) = trailing_usage(&self.tail) {
                    self.reported = Some(Usage::parse(&usage));
                }
                self.images = self
                    .b64_markers
                    .found
                    .saturating_add(self.url_markers.found);
                // Too large to read: bound the output estimate by body size.
                self.generated_chars = self.bytes;
            }
            Format::Pcm => {
                self.audio_ms = self.bytes.saturating_mul(1000) / self.pcm_rate;
            }
            Format::Wav => self.audio_ms = wav_ms(&self.capture, self.bytes).unwrap_or(0),
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
            self.images = self.images.saturating_add(delta_images(&v));
            if self.estimate_input.is_some() {
                self.generated_chars = self.generated_chars.saturating_add(delta_chars(&v));
            }
        }
    }

    /// The last usage object wins: streaming APIs report cumulative usage.
    fn observe(&mut self, v: &Value) {
        if let Some(usage) = usage_object(v) {
            self.reported = Some(Usage::parse(usage));
        }
    }
}

/// Tokens in `chars` characters at roughly four characters per token.
fn tokens_for_chars(chars: i64) -> i64 {
    chars.max(0).saturating_add(3) / 4
}

/// Estimated input tokens of an LLM request body: the message, prompt,
/// input, and instruction text (inline `data:` payloads excluded), or the
/// body size when none can be extracted.
pub fn estimate_input_tokens(body: &Value, body_bytes: usize) -> i64 {
    fn walk(v: &Value) -> i64 {
        match v {
            Value::String(s) if s.starts_with("data:") => 0,
            Value::String(s) => s.chars().count() as i64,
            Value::Array(items) => items.iter().map(walk).sum(),
            // Only text-bearing fields: roles, types, and media payloads
            // do not count.
            Value::Object(map) => map
                .iter()
                .filter(|(k, _)| {
                    matches!(
                        k.as_str(),
                        "content" | "text" | "arguments" | "tool_calls" | "function" | "input"
                    )
                })
                .map(|(_, v)| walk(v))
                .sum(),
            _ => 0,
        }
    }
    let chars: i64 = ["messages", "prompt", "input", "instructions", "system"]
        .iter()
        .map(|k| walk(&body[*k]))
        .sum();
    if chars > 0 {
        tokens_for_chars(chars)
    } else {
        tokens_for_chars(i64::try_from(body_bytes).unwrap_or(i64::MAX))
    }
}

/// Characters of text in a string or an array of `{text}` content parts.
fn text_chars(v: &Value) -> i64 {
    match v {
        Value::String(s) => s.chars().count() as i64,
        Value::Array(parts) => parts.iter().map(|p| text_chars(&p["text"])).sum(),
        _ => 0,
    }
}

/// Characters of generated text in one streaming event: chat/completions
/// deltas, legacy completion text, and Responses API `*.delta` events.
fn delta_chars(v: &Value) -> i64 {
    let mut n = 0;
    for choice in v["choices"].as_array().into_iter().flatten() {
        n += message_chars(&choice["delta"]) + text_chars(&choice["text"]);
    }
    if let Some(kind) = v["type"].as_str()
        && kind.starts_with("response.")
        && kind.ends_with(".delta")
        && !kind.contains("audio")
    {
        n += text_chars(&v["delta"]);
    }
    n
}

/// Characters of generated text in a complete (non-streaming) response.
fn final_chars(v: &Value) -> i64 {
    let mut n = 0;
    for choice in v["choices"].as_array().into_iter().flatten() {
        n += message_chars(&choice["message"]) + text_chars(&choice["text"]);
    }
    for item in v["output"].as_array().into_iter().flatten() {
        n += text_chars(&item["content"])
            + text_chars(&item["summary"])
            + text_chars(&item["arguments"]);
    }
    n
}

/// Text, reasoning, and tool-call arguments of a chat message or delta.
fn message_chars(m: &Value) -> i64 {
    let calls: i64 = m["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| text_chars(&c["function"]["arguments"]))
        .sum();
    text_chars(&m["content"])
        + text_chars(&m["reasoning_content"])
        + text_chars(&m["reasoning"])
        + text_chars(&m["function_call"]["arguments"])
        + calls
}

/// Images attached to streamed chat deltas (`delta.images[]`).
fn delta_images(v: &Value) -> i64 {
    v["choices"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            c["delta"]["images"]
                .as_array()
                .map_or(0, |i| i.len() as i64)
        })
        .sum()
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

/// Generated images in a JSON response: images API `data[]` entries
/// carrying `b64_json` or `url`, plus chat-completion `message.images[]`.
fn inline_images(v: &Value) -> i64 {
    let data = v["data"].as_array().map_or(0, |items| {
        items
            .iter()
            .filter(|i| i.get("b64_json").is_some() || i.get("url").is_some_and(Value::is_string))
            .count() as i64
    });
    let chat: i64 = v["choices"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            c["message"]["images"]
                .as_array()
                .map_or(0, |i| i.len() as i64)
        })
        .sum();
    data + chat
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

/// A realtime server event relevant to metering.
#[derive(Debug, PartialEq)]
pub enum RealtimeEvent {
    /// A response started; its `response.done` is still to come.
    Created,
    /// A response finished, with its usage if reported. Each `response.done`
    /// reports one response, so callers accumulate them.
    Done(Option<Usage>),
}

/// Classifies a realtime WebSocket server event.
pub fn realtime_event(event: &str) -> Option<RealtimeEvent> {
    // Cheap pre-filter: most realtime events are audio deltas.
    if !event.contains("response.done") && !event.contains("response.created") {
        return None;
    }
    let v: Value = serde_json::from_str(event).ok()?;
    match v["type"].as_str()? {
        "response.created" => Some(RealtimeEvent::Created),
        "response.done" => Some(RealtimeEvent::Done(usage_object(&v).map(Usage::parse))),
        _ => None,
    }
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
    use serde_json::json;

    #[test]
    fn realtime_usage_reads_response_done_only() {
        let done = r#"{"type":"response.done","response":{"usage":{"input_tokens":5,"output_tokens":7,"total_tokens":12}}}"#;
        let Some(RealtimeEvent::Done(Some(usage))) = realtime_event(done) else {
            panic!("response.done must carry usage");
        };
        assert_eq!((usage.input, usage.output, usage.total), (5, 7, 12));
        assert_eq!(
            realtime_event(r#"{"type":"response.created","response":{}}"#),
            Some(RealtimeEvent::Created)
        );
        assert!(realtime_event(r#"{"type":"response.audio.delta","delta":"AAAA"}"#).is_none());
        assert!(
            realtime_event(r#"{"type":"conversation.item.created","note":"response.done"}"#)
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
        assert_eq!(inspector.usage().total, 651);
        assert_eq!(inspector.usage().cost, Some(10_000));
        assert_eq!(inspector.usage().images, 2);
    }

    #[test]
    fn small_image_responses_count_data_entries() {
        let mut inspector = Inspector::new("application/json");
        inspector.feed(
            br#"{"data":[{"url":"https://x/1.png"},{"b64_json":"AA"}],"usage":{"total_tokens":3}}"#,
        );
        inspector.finish();
        assert_eq!(inspector.usage().images, 2);
        let mut models = Inspector::new("application/json");
        models.feed(br#"{"data":[{"id":"m","object":"model"}]}"#);
        models.finish();
        assert_eq!(models.usage().images, 0);
    }

    #[test]
    fn marker_counting_handles_every_split() {
        let body = br#"{"data":[{"b64_json":"x"},{"b64_json":"y"}]}"#;
        for size in 1..body.len() {
            let mut inspector = Inspector::new("application/json");
            for chunk in body.chunks(size) {
                inspector.b64_markers.feed(chunk);
            }
            assert_eq!(inspector.b64_markers.found, 2, "chunk size {size}");
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
            assert_eq!(inspector.usage().audio_ms, ms, "{content_type}");
        }
    }

    #[test]
    fn crlf_terminated_sse_lines_are_parsed() {
        let mut inspector = Inspector::new("text/event-stream");
        inspector.feed(b"data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\r\n\r\n");
        inspector.finish();
        assert_eq!(inspector.usage().total, 3);
    }

    fn sse(events: &[&str], estimate: Option<i64>) -> Usage {
        let mut inspector = Inspector::new("text/event-stream");
        if let Some(input) = estimate {
            inspector = inspector.with_estimate(input);
        }
        for event in events {
            inspector.feed(format!("data: {event}\n\n").as_bytes());
        }
        inspector.finish();
        inspector.usage()
    }

    #[test]
    fn cached_tokens_are_read_from_every_usage_shape() {
        for usage in [
            r#"{"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":40}}"#,
            r#"{"input_tokens":100,"output_tokens":5,"input_tokens_details":{"cached_tokens":40}}"#,
            r#"{"input_tokens":100,"output_tokens":5,"input_token_details":{"cached_tokens":40}}"#,
        ] {
            let u = Usage::parse(&serde_json::from_str(usage).unwrap());
            assert_eq!(
                (u.input, u.cached_input, u.output, u.total),
                (100, 40, 5, 105)
            );
        }
        // Cached can never exceed the input it is part of.
        let u =
            Usage::parse(&json!({"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":99}}));
        assert_eq!(u.cached_input, 10);
    }

    #[test]
    fn transcription_usage_shapes_parse() {
        let whisper = Usage::parse(&json!({"type":"duration","seconds":12.5}));
        assert_eq!((whisper.audio_ms, whisper.total), (12_500, 0));
        let whisper_int = Usage::parse(&json!({"type":"duration","seconds":3}));
        assert_eq!(whisper_int.audio_ms, 3000);
        let tokens = Usage::parse(&json!({
            "type":"tokens","input_tokens":14,"output_tokens":45,"total_tokens":59,
            "input_token_details":{"audio_tokens":10,"text_tokens":4}
        }));
        assert_eq!(
            (tokens.input, tokens.output, tokens.total, tokens.audio_ms),
            (14, 45, 59, 0)
        );
        let mut json = Inspector::new("application/json");
        json.feed(br#"{"text":"hi","usage":{"type":"duration","seconds":2}}"#);
        json.finish();
        assert_eq!(json.usage().audio_ms, 2000);
    }

    #[test]
    fn speech_sse_done_event_usage_is_read() {
        let u = sse(
            &[
                r#"{"type":"speech.audio.delta","audio":"AAAA"}"#,
                r#"{"type":"speech.audio.done","usage":{"input_tokens":12,"output_tokens":80,"total_tokens":92}}"#,
            ],
            None,
        );
        assert_eq!((u.input, u.output, u.total), (12, 80, 92));
    }

    #[test]
    fn characters_are_unicode_scalar_counts() {
        let inspector =
            Inspector::new("audio/mpeg").with_characters("héllo 世界".chars().count() as i64);
        assert_eq!(inspector.usage().characters, 8);
        assert_eq!(inspector.usage().audio_ms, 0);
    }

    #[test]
    fn streamed_chat_is_estimated_only_without_reported_usage() {
        let chunks = [
            r#"{"choices":[{"delta":{"content":"Hello wor"}}]}"#,
            r#"{"choices":[{"delta":{"content":"ld!!"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":null}"#,
        ];
        let u = sse(&chunks, Some(10));
        // 13 chars -> 4 tokens.
        assert_eq!((u.input, u.output, u.total, u.estimated), (10, 4, 14, true));
        let mut reported = chunks.to_vec();
        reported.push(
            r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":0,"total_tokens":3}}"#,
        );
        let u = sse(&reported, Some(10));
        assert_eq!((u.input, u.output, u.total, u.estimated), (3, 0, 3, false));
        // A reported zero cost is still a report.
        let u = sse(&[r#"{"usage":{"cost":0}}"#], Some(10));
        assert!(!u.estimated);
        assert_eq!(u.cost, Some(0));
        // Estimation is opt-in.
        assert!(!sse(&chunks, None).estimated);
    }

    #[test]
    fn responses_deltas_count_once_and_audio_deltas_are_skipped() {
        let u = sse(
            &[
                r#"{"type":"response.output_text.delta","delta":"abcdefgh"}"#,
                r#"{"type":"response.audio.delta","delta":"QUJDREVGR0g="}"#,
                r#"{"type":"response.completed","response":{"usage":null,"output":[{"content":[{"text":"abcdefgh"}]}]}}"#,
            ],
            Some(4),
        );
        assert_eq!((u.output, u.estimated), (2, true));
    }

    #[test]
    fn json_chat_response_is_estimated_from_message_content() {
        let mut inspector = Inspector::new("application/json").with_estimate(7);
        inspector.feed(
            br#"{"choices":[{"message":{"content":"twelve chars","tool_calls":[{"function":{"arguments":"{\"a\":1}"}}]}}]}"#,
        );
        inspector.finish();
        let u = inspector.usage();
        // 12 + 7 = 19 chars -> 5 tokens.
        assert_eq!((u.input, u.output, u.estimated), (7, 5, true));
    }

    #[test]
    fn request_input_estimate_uses_text_not_inline_data() {
        let body = json!({"model":"m","messages":[
            {"role":"user","content":[{"type":"text","text":"12345678"},
              {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]},
            {"role":"system","content":"1234"}]});
        assert_eq!(estimate_input_tokens(&body, 9999), 3);
        // No extractable text: fall back to body size.
        assert_eq!(estimate_input_tokens(&json!({"model":"m"}), 41), 11);
    }

    #[test]
    fn chat_images_are_counted_in_json_and_sse() {
        let mut json = Inspector::new("application/json");
        json.feed(
            br#"{"choices":[{"message":{"content":"","images":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA"}},{"type":"image_url","image_url":{"url":"data:image/png;base64,BB"}}]}}],"usage":{"total_tokens":5}}"#,
        );
        json.finish();
        assert_eq!(json.usage().images, 2);
        let u = sse(
            &[
                r#"{"choices":[{"delta":{"images":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA"}}]}}]}"#,
                r#"{"choices":[{"delta":{"content":"x"}}]}"#,
            ],
            None,
        );
        assert_eq!(u.images, 1);
    }

    #[test]
    fn oversized_chat_json_counts_image_markers_without_buffering() {
        let image = "A".repeat(CAPTURE_LIMIT / 2 + 10);
        let body = format!(
            r#"{{"choices":[{{"message":{{"images":[{{"type":"image_url","image_url":{{"url":"data:image/png;base64,{image}"}}}},{{"type":"image_url","image_url":{{"url":"data:image/png;base64,{image}"}}}},{{"type":"image_url","image_url":{{"url":"data:image/png;base64,{image}"}}}}]}}}}],"usage":{{"total_tokens":9}}}}"#
        );
        let mut inspector = Inspector::new("application/json");
        for chunk in body.as_bytes().chunks(65_537) {
            inspector.feed(chunk);
        }
        inspector.finish();
        assert!(inspector.overflow);
        assert!(inspector.tail.len() <= 2 * TAIL_LIMIT + 65_537);
        let u = inspector.usage();
        assert_eq!((u.images, u.total), (3, 9));
    }
}
