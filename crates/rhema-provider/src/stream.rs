//! Server-sent-event decoding shared by the streaming adapters.
//!
//! Every rhema stream is one `data: <JSON chunk>` event per response
//! chunk; dialects differ only in the JSON and in sentinel payloads (a
//! final `data: [DONE]`, say). Real transports deliver byte fragments at
//! arbitrary boundaries and may interleave comments and keep-alive blank
//! lines; the decoder absorbs all of it and yields only the `data:`
//! payloads, in order.

/// The most bytes one unterminated line, or one event's data, may
/// hold. No rhema event comes near it; the cap turns a server gone
/// wrong — gigabytes with no line break — into a stream error instead
/// of unbounded memory.
const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Decodes SSE `data:` payloads out of arbitrary byte chunks.
///
/// Implements the SSE line rules the rhema streams rely on: `data:` lines
/// accumulate into an event, a blank line ends the event, `:`-prefixed
/// comments and other fields (`event`, `id`, `retry`) carry no payload.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    data: Vec<String>,
    error: Option<String>,
}

impl SseDecoder {
    /// A decoder with nothing buffered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes and returns the payloads of every event completed by
    /// them. Handles events whose lines are split across chunks.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        // Bound the buffer before extending it: a line that never ends
        // is not an event, it is a server gone wrong.
        if self.error.is_none()
            && !bytes.contains(&b'\n')
            && self.buffer.len() + bytes.len() > MAX_EVENT_BYTES
        {
            self.error = Some(format!(
                "the stream sent more than {MAX_EVENT_BYTES} bytes without a \
                 line break — not an SSE event; aborting"
            ));
            self.buffer.clear();
            return Vec::new();
        }
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=pos).collect();
            let mut end = line.len() - 1;
            if end > 0 && line[end - 1] == b'\r' {
                end -= 1;
            }
            let line = String::from_utf8_lossy(&line[..end]).into_owned();
            self.take_line(line, &mut events);
        }
        events
    }

    /// Flushes state left open at end of stream: a trailing line without a
    /// newline, and an event never closed by a blank line.
    pub fn finish(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let tail = String::from_utf8_lossy(&self.buffer).into_owned();
            self.buffer.clear();
            self.take_line(tail, &mut events);
        }
        if !self.data.is_empty() {
            events.push(std::mem::take(&mut self.data).join("\n"));
        }
        events
    }

    /// The cap breach, if one happened, taken so the caller can fail
    /// the stream with it. Checked after every push.
    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    /// Consumes one line: either it completes an event or it carries no
    /// payload.
    fn take_line(&mut self, line: String, events: &mut Vec<String>) {
        if line.is_empty() {
            if !self.data.is_empty() {
                events.push(std::mem::take(&mut self.data).join("\n"));
            }
            return;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            // The spec strips exactly one optional space after the colon.
            let payload = rest.strip_prefix(' ').unwrap_or(rest);
            self.data.push(payload.to_owned());
            let total: usize = self.data.iter().map(|s| s.len()).sum();
            if self.error.is_none() && total > MAX_EVENT_BYTES {
                self.error = Some(format!(
                    "the stream built an SSE event over {MAX_EVENT_BYTES} bytes — \
                     not a rhema chunk; aborting"
                ));
                self.data.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(bodies: &[&[u8]]) -> Vec<String> {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for body in bodies {
            events.extend(decoder.push(body));
        }
        events.extend(decoder.finish());
        events
    }

    #[test]
    fn a_line_past_the_cap_poisons_instead_of_growing() {
        let mut d = SseDecoder::new();
        let big = vec![b'x'; MAX_EVENT_BYTES + 1];
        assert!(d.push(&big).is_empty());
        let err = d.take_error().expect("the oversized line is rejected");
        assert!(err.contains("without a line break"));
        // The buffer is gone; the decoder keeps answering, empty.
        assert!(d.push(b"later").is_empty());
        assert!(d.take_error().is_none());
    }

    #[test]
    fn an_event_past_the_cap_poisons() {
        let mut d = SseDecoder::new();
        let half = vec![b'x'; MAX_EVENT_BYTES / 2 + 8];
        // Two data lines, each under the cap; the joined event is over.
        assert!(d.push(b"data: ").is_empty());
        assert!(d.push(&half).is_empty());
        assert!(d.push(&b"\ndata: "[..]).is_empty());
        assert!(d.push(&half).is_empty());
        let events = d.push(&b"\n"[..]);
        assert!(events.is_empty(), "the oversized event is dropped");
        let err = d.take_error().expect("the oversized event is rejected");
        assert!(err.contains("SSE event over"));
    }

    #[test]
    fn normal_traffic_never_poisons() {
        let mut d = SseDecoder::new();
        assert_eq!(d.push(b"data: {}\n\n"), vec!["{}".to_owned()]);
        assert!(d.take_error().is_none());
        // A long-but-legal stream of normal events.
        for _ in 0..10_000 {
            assert_eq!(d.push(b": ping\ndata: [1]\n\n").len(), 1);
        }
        assert!(d.take_error().is_none());
    }

    #[test]
    fn decodes_chunked_sse_bodies() {
        let body = b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n";
        assert_eq!(
            decoded(&[&body[..5], &body[5..17], &body[17..]]),
            vec!["{\"a\":1}".to_owned(), "{\"b\":2}".to_owned()]
        );
    }

    #[test]
    fn tolerates_crlf_comments_and_keepalives() {
        let body = b": keep-alive\r\n\r\ndata: one\r\n\r\n";
        assert_eq!(decoded(&[body]), vec!["one".to_owned()]);
    }

    #[test]
    fn joins_multi_line_data_events() {
        let body = b"data: first\ndata: second\n\n";
        assert_eq!(decoded(&[body]), vec!["first\nsecond".to_owned()]);
    }

    #[test]
    fn ignores_other_sse_fields() {
        let body = b"event: chunk\ndata: x\nid: 7\n\n";
        assert_eq!(decoded(&[body]), vec!["x".to_owned()]);
    }

    #[test]
    fn flushes_an_open_event_and_trailing_line_at_finish() {
        assert_eq!(decoded(&[b"data: x\n"]), vec!["x".to_owned()]);
        assert_eq!(decoded(&[b"data: x"]), vec!["x".to_owned()]);
        assert_eq!(decoded(&[b"data: x\ndata: y"]), vec!["x\ny".to_owned()]);
        assert!(decoded(&[b""]).is_empty());
    }

    #[test]
    fn only_complete_events_are_yielded_per_push() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: {\"a\":1}\n").is_empty());
        assert_eq!(decoder.push(b"\n"), vec!["{\"a\":1}".to_owned()]);
    }
}
