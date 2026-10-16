//! Bounded inspection of upstream responses to extract billable usage while
//! the body streams through unchanged.
use crate::meter::Usage;
use serde_json::Value;

/// Largest SSE line or JSON body retained for parsing.
const CAPTURE_LIMIT: usize = 4 * 1024 * 1024;
/// Audio responses only need their header to determine the format.
const AUDIO_HEADER_LIMIT: usize = 64 * 1024;
/// OpenAI raw PCM output: 24 kHz, 16-bit, mono.
const PCM_BYTES_PER_SECOND: i64 = 48_000;

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
    bytes: i64,
    pub usage: Usage,
}

impl Inspector {
    pub fn new(content_type: &str) -> Self {
        Self {
            format: Format::of(content_type),
            line: Vec::new(),
            skipping_line: false,
            capture: Vec::new(),
            bytes: 0,
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
            Format::Json => self.capture_prefix(chunk, CAPTURE_LIMIT),
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
            Format::Json => {
                if let Ok(v) = serde_json::from_slice::<Value>(&self.capture) {
                    self.observe(&v);
                }
            }
            Format::Pcm => {
                self.usage.audio_ms = self.bytes.saturating_mul(1000) / PCM_BYTES_PER_SECOND;
            }
            Format::Wav => self.usage.audio_ms = wav_ms(&self.capture, self.bytes).unwrap_or(0),
            _ => {}
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
    fn crlf_terminated_sse_lines_are_parsed() {
        let mut inspector = Inspector::new("text/event-stream");
        inspector.feed(b"data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\r\n\r\n");
        inspector.finish();
        assert_eq!(inspector.usage.total, 3);
    }
}
