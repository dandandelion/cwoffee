//! Minimal SSE (text/event-stream) framing.
//!
//! Deliberately hand-rolled rather than a crate: we need to survive arbitrary
//! TCP chunk boundaries, and a parser that takes `&[u8]` and returns events is
//! unit-testable against adversarial splits, which a stream-wrapping crate is
//! not without a network.
//!
//! Per the SSE spec an event ends at a blank line, and multiple `data:` lines in
//! one event join with `\n`. OpenAI/llama.cpp emit one `data:` line per event;
//! Anthropic emits `event:` + `data:` pairs, so we surface the event name too.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SseEvent {
    /// The `event:` field if present (Anthropic uses it heavily).
    pub name: String,
    /// All `data:` lines joined with '\n'.
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseParser {
    /// Bytes received but not yet terminated by a newline.
    pending: Vec<u8>,
    /// Fields accumulated for the event currently being built.
    cur_name: String,
    cur_data: Vec<String>,
    saw_field: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a network chunk; returns every event completed by this chunk.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        self.pending.extend_from_slice(bytes);

        let mut scan = 0usize;
        while let Some(nl) = find_byte(&self.pending[scan..], b'\n') {
            let start = scan;
            let end = scan + nl;
            let raw = &self.pending[start..end];
            // Own the line: `self.line` needs &mut self, and the slice above
            // borrows self.pending.
            let line = line_of(raw);
            self.line(&line, &mut out);
            scan = end + 1;
        }
        // keep only the unterminated remainder
        if scan > 0 {
            self.pending.drain(..scan);
        }
        out
    }

    /// Flush a trailing event that was never terminated by a blank line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            let line = line_of(&rest);
            self.line(&line, &mut out);
        }
        self.flush(&mut out);
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.flush(out);
            return;
        }
        // comment line
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => {
                self.cur_name = value.to_string();
                self.saw_field = true;
            }
            "data" => {
                self.cur_data.push(value.to_string());
                self.saw_field = true;
            }
            // "id" and "retry" are irrelevant for our clients
            _ => {}
        }
    }

    fn flush(&mut self, out: &mut Vec<SseEvent>) {
        if self.saw_field && !self.cur_data.is_empty() {
            out.push(SseEvent {
                name: std::mem::take(&mut self.cur_name),
                data: self.cur_data.join("\n"),
            });
        }
        self.cur_name.clear();
        self.cur_data.clear();
        self.saw_field = false;
    }
}

fn find_byte(hay: &[u8], needle: u8) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

/// Splitting on b'\n' is always safe for UTF-8: continuation bytes are >= 0x80,
/// so a newline can never appear inside a multibyte sequence. A line that is
/// complete (i.e. terminated) is therefore whole. Lossy conversion is the
/// fallback for genuinely corrupt bytes: one bad byte must not discard an
/// otherwise good line, which is what a strict from_utf8 -> "" would do.
fn line_of(raw: &[u8]) -> String {
    let raw = if raw.last() == Some(&b'\r') { &raw[..raw.len() - 1] } else { raw };
    String::from_utf8_lossy(raw).into_owned()
}

/// The sentinel OpenAI-compatible servers send to end a stream.
pub const DONE: &str = "[DONE]";

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut p = SseParser::new();
        let mut all = Vec::new();
        for c in chunks {
            all.extend(p.feed(c));
        }
        all.extend(p.finish());
        all
    }

    #[test]
    fn single_event() {
        let ev = collect(&[b"data: {\"a\":1}\n\n"]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].data, "{\"a\":1}");
    }

    /// The important one: an event split across arbitrary TCP chunks must
    /// reassemble exactly.
    #[test]
    fn split_across_chunks_at_any_offset() {
        let full = b"data: {\"content\":\"hello world\"}\n\ndata: [DONE]\n\n";
        for cut in 1..full.len() {
            let ev = collect(&[&full[..cut], &full[cut..]]);
            assert_eq!(ev.len(), 2, "cut at {cut} gave {} events", ev.len());
            assert_eq!(ev[0].data, "{\"content\":\"hello world\"}");
            assert_eq!(ev[1].data, "[DONE]");
        }
    }

    #[test]
    fn crlf_and_multiple_data_lines() {
        let ev = collect(&[b"event: message_start\r\ndata: {\"a\":\r\ndata: 1}\r\n\r\n"]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].name, "message_start");
        assert_eq!(ev[0].data, "{\"a\":\n1}");
    }

    #[test]
    fn comments_ignored() {
        let ev = collect(&[b": keep-alive\ndata: x\n\n"]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].data, "x");
    }

    #[test]
    fn unterminated_tail_still_flushes() {
        let ev = collect(&[b"data: tail"]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].data, "tail");
    }

    #[test]
    fn utf8_multibyte_split_mid_sequence_survives() {
        // 'é' is 0xC3 0xA9. Cutting between the two bytes must reassemble
        // exactly - this is what protects CJK output from turning into mojibake
        // when TCP chunks land mid-character.
        let payload = "data: {\"t\":\"café 中文\"}\n\n";
        let bytes = payload.as_bytes();
        let at = bytes.iter().position(|&b| b == 0xC3).unwrap();
        let ev = collect(&[&bytes[..=at], &bytes[at + 1..]]);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].data, "{\"t\":\"café 中文\"}");
    }

    #[test]
    fn byte_at_a_time_reassembles() {
        // The most adversarial split possible.
        let payload = "event: a\ndata: {\"t\":\"héllo 世界\"}\n\n";
        let chunks: Vec<&[u8]> = payload.as_bytes().chunks(1).collect();
        let ev = collect(&chunks);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].name, "a");
        assert_eq!(ev[0].data, "{\"t\":\"héllo 世界\"}");
    }
}
