//! Server-sent-event decoding for OpenRouter's streaming mode.
//!
//! The stream is one `data: <JSON chunk>` event per delta and a final
//! `data: [DONE]` sentinel. Real transports deliver byte fragments at
//! arbitrary boundaries and may interleave comments and keep-alive blank
//! lines — OpenRouter pings with `: OPENROUTER PROCESSING`; the decoder
//! absorbs all of it and yields only the `data:` payloads, in order.
//!
//! Copied from `rhema-provider-google/src/stream.rs` (same crate license
//! and owner): every rhema provider relies on the same SSE line rules.

/// Decodes SSE `data:` payloads out of arbitrary byte chunks.
///
/// Implements the SSE line rules the OpenRouter stream relies on: `data:`
/// lines accumulate into an event, a blank line ends the event,
/// `:`-prefixed comments and other fields (`event`, `id`, `retry`) carry no
/// payload.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    data: Vec<String>,
}

impl SseDecoder {
    /// A decoder with nothing buffered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes and returns the payloads of every event completed by
    /// them. Handles events whose lines are split across chunks.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
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
    fn decodes_chunked_sse_bodies() {
        let body = b"data: {\"a\":1}\n\ndata: [DONE]\n\n";
        assert_eq!(
            decoded(&[&body[..5], &body[5..17], &body[17..]]),
            vec!["{\"a\":1}".to_owned(), "[DONE]".to_owned()]
        );
    }

    #[test]
    fn tolerates_crlf_comments_and_keepalives() {
        // OpenRouter's documented keep-alive comment.
        let body = b": OPENROUTER PROCESSING\n\n\r\ndata: one\r\n\r\n";
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
