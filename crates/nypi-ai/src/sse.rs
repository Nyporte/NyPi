//! Incremental Server-Sent Events parser.

/// One dispatched SSE event.
#[derive(Clone, Debug, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Feed raw bytes with `push`; complete events come back as they finish.
#[derive(Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    last_id: Option<String>,
    retry_ms: Option<u64>,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(event) = self.line(&line) {
                out.push(event);
            }
        }
        out
    }

    /// The latest `id` field seen, which persists across events: what a reconnect sends as
    /// `Last-Event-ID` to resume the stream.
    pub fn last_event_id(&self) -> Option<&str> {
        self.last_id.as_deref()
    }

    /// The server's latest `retry` field: how long to wait before reconnecting.
    pub fn retry_ms(&self) -> Option<u64> {
        self.retry_ms
    }

    /// Flush a final event that was not followed by a blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buffer)).into_owned();
            if let Some(event) = self.line(rest.trim_end_matches('\r')) {
                return Some(event);
            }
        }
        self.dispatch()
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            "id" if !value.contains('\0') => self.last_id = Some(value.to_string()),
            "retry" => {
                if let Ok(ms) = value.parse() {
                    self.retry_ms = Some(ms);
                }
            }
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() && self.event.is_none() {
            return None;
        }
        let event = SseEvent { event: self.event.take(), data: self.data.join("\n") };
        self.data.clear();
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_split_across_chunks() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"event: ping\r\nda").is_empty());
        let events = parser.push(b"ta: {\"a\":1}\r\n\r\ndata: x\n: comment\ndata: y\n\n");
        assert_eq!(
            events,
            vec![
                SseEvent { event: Some("ping".into()), data: "{\"a\":1}".into() },
                SseEvent { event: None, data: "x\ny".into() },
            ]
        );
        assert!(parser.push(b"id: 7\nretry: 1500\ndata: z\n\nid: 8\n\n").len() == 1);
        assert_eq!(
            (parser.last_event_id(), parser.retry_ms()),
            (Some("8"), Some(1500)),
            "an id-only event still counts"
        );
        assert!(parser.push(b"data: [DONE]").is_empty());
        assert_eq!(parser.finish(), Some(SseEvent { event: None, data: "[DONE]".into() }));
    }
}
