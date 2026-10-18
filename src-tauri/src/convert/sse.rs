//! Incremental Server-Sent Events parser and writer.

use bytes::Bytes;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Feeds raw bytes in arbitrary chunks, yields complete events. Holds at most one partial line,
/// so nothing is buffered beyond the event that is still arriving.
#[derive(Debug, Default)]
pub struct SseParser {
    line: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for &b in chunk {
            if b == b'\n' {
                let mut line = std::mem::take(&mut self.line);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.process_line(&line, &mut out);
            } else {
                self.line.push(b);
            }
        }
        out
    }

    /// Flushes whatever is left when the stream ends without a trailing blank line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.process_line(&line, &mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn process_line(&mut self, line: &[u8], out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        let text = String::from_utf8_lossy(line);
        if text.starts_with(':') {
            return; // comment / keep-alive
        }
        let (field, value) = match text.find(':') {
            Some(i) => {
                let v = &text[i + 1..];
                (&text[..i], v.strip_prefix(' ').unwrap_or(v))
            }
            None => (&text[..], ""),
        };
        match field {
            "data" => self.data.push(value.to_string()),
            "event" => self.event = Some(value.to_string()),
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if self.data.is_empty() {
            self.event = None;
            return;
        }
        out.push(SseEvent { event: self.event.take(), data: self.data.join("\n") });
        self.data.clear();
    }
}

/// One Anthropic-style SSE event: `event: <name>\ndata: <json>\n\n`.
pub fn event_bytes(name: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {name}\ndata: {data}\n\n"))
}

/// Re-emits an upstream event unchanged (data lines kept verbatim).
pub fn raw_event_bytes(name: &str, data: &str) -> Bytes {
    let mut s = format!("event: {name}\n");
    for line in data.split('\n') {
        s.push_str("data: ");
        s.push_str(line);
        s.push('\n');
    }
    s.push('\n');
    Bytes::from(s)
}

pub fn ping_bytes() -> Bytes {
    Bytes::from_static(b"event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_split_at_any_byte() {
        let stream = "event: message_start\ndata: {\"a\":1}\n\n: keep-alive\n\ndata: {\"b\":\"привет\"}\r\n\r\ndata: [DONE]\n\n";
        let whole = SseParser::new().feed(stream.as_bytes());
        assert_eq!(whole.len(), 3);
        assert_eq!(whole[0], SseEvent { event: Some("message_start".into()), data: "{\"a\":1}".into() });
        assert_eq!(whole[1].data, "{\"b\":\"привет\"}");
        assert_eq!(whole[2].data, "[DONE]");

        // Byte-by-byte feeding (splits UTF-8 sequences too) must give the same result.
        let mut p = SseParser::new();
        let mut split = Vec::new();
        for b in stream.as_bytes() {
            split.extend(p.feed(std::slice::from_ref(b)));
        }
        split.extend(p.finish());
        assert_eq!(split, whole);
    }

    #[test]
    fn multi_line_data_and_unterminated_tail() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: line1\ndata: line2").is_empty());
        let tail = p.finish();
        assert_eq!(tail, vec![SseEvent { event: None, data: "line1\nline2".into() }]);
    }

    #[test]
    fn writes_events() {
        let b = event_bytes("ping", &serde_json::json!({"type": "ping"}));
        assert_eq!(&b[..], b"event: ping\ndata: {\"type\":\"ping\"}\n\n");
        let r = raw_event_bytes("x", "a\nb");
        assert_eq!(&r[..], b"event: x\ndata: a\ndata: b\n\n");
    }
}
