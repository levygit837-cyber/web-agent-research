//! Server-Sent Events framing (WHATWG HTML §9.2), reduced to what the
//! two chat streams need: `data:` lines and blank-line dispatch.
//!
//! The parser works on bytes, not on decoded chunks: a UTF-8 sequence or an
//! event split across two network chunks is buffered until its line ends,
//! so the split point never corrupts a character or an event.
//!
//! Deliberate limits: `event:`, `id:` and `retry:` fields are ignored (OpenAI
//! sends none that matter; Anthropic repeats the event name as the JSON
//! `type`, which is what its assembler dispatches on), a lone `\r` is not a line break
//! (only `\n` and `\r\n` are), and an event still open at end of body is
//! dropped, as the spec says, so a body cut mid-event never yields a
//! half-parsed payload.

/// Incremental SSE decoder. Feed it body chunks with [`SseParser::push`];
/// it appends each completed event's `data` payload to the caller's list.
#[derive(Debug, Default)]
pub(super) struct SseParser {
    /// Bytes of the line being received (no `\n` seen yet).
    line: Vec<u8>,
    /// `data` lines of the open event, joined with `\n` on dispatch.
    data: String,
    has_data: bool,
}

impl SseParser {
    pub(super) fn push(&mut self, chunk: &[u8], payloads: &mut Vec<String>) {
        let mut rest = chunk;
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            self.line.extend_from_slice(&rest[..end]);
            rest = &rest[end + 1..];
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            self.take_line(payloads);
        }
        self.line.extend_from_slice(rest);
    }

    fn take_line(&mut self, payloads: &mut Vec<String>) {
        let bytes = std::mem::take(&mut self.line);
        let line = String::from_utf8_lossy(&bytes);
        if line.is_empty() {
            if self.has_data {
                payloads.push(std::mem::take(&mut self.data));
                self.has_data = false;
            }
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((&line, ""));
        if field != "data" {
            return;
        }
        let value = value.strip_prefix(' ').unwrap_or(value);
        if self.has_data {
            self.data.push('\n');
        }
        self.data.push_str(value);
        self.has_data = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(chunks: &[&[u8]]) -> Vec<String> {
        let mut parser = SseParser::default();
        let mut out = Vec::new();
        for chunk in chunks {
            parser.push(chunk, &mut out);
        }
        out
    }

    #[test]
    fn events_split_by_blank_lines_in_order() {
        assert_eq!(decode(&[b"data: one\n\ndata: two\n\n"]), vec!["one", "two"]);
    }

    #[test]
    fn multi_line_data_joins_with_newline() {
        assert_eq!(decode(&[b"data: a\ndata: b\n\n"]), vec!["a\nb"]);
    }

    #[test]
    fn comments_and_other_fields_are_ignored() {
        assert_eq!(
            decode(&[b": keep-alive\nevent: message\nid: 7\ndata: x\n\n"]),
            vec!["x"]
        );
    }

    #[test]
    fn crlf_line_endings_and_missing_space_after_colon() {
        assert_eq!(decode(&[b"data:x\r\n\r\ndata: y\r\n\r\n"]), vec!["x", "y"]);
    }

    #[test]
    fn only_one_leading_space_is_stripped() {
        assert_eq!(decode(&[b"data:  two spaces\n\n"]), vec![" two spaces"]);
    }

    #[test]
    fn event_split_at_every_byte_boundary_decodes_the_same() {
        let body = b"data: {\"a\":1}\r\n\r\ndata: [DONE]\r\n\r\n";
        let whole = decode(&[body]);
        let bytes: Vec<&[u8]> = body.chunks(1).collect();
        assert_eq!(decode(&bytes), whole);
        assert_eq!(whole, vec!["{\"a\":1}", "[DONE]"]);
    }

    #[test]
    fn multibyte_character_split_across_chunks_survives() {
        let body = "data: caf\u{e9} \u{1f980}\n\n".as_bytes();
        // Cut inside the crab emoji's four bytes.
        let cut = body.len() - 4;
        assert_eq!(
            decode(&[&body[..cut], &body[cut..]]),
            vec!["caf\u{e9} \u{1f980}"]
        );
    }

    #[test]
    fn event_left_open_at_end_of_body_is_dropped() {
        assert!(decode(&[b"data: {\"cut\":"]).is_empty());
        assert!(decode(&[b"data: complete line, no blank line\n"]).is_empty());
    }

    #[test]
    fn blank_lines_without_data_dispatch_nothing() {
        assert!(decode(&[b"\n\n: ping\n\n"]).is_empty());
    }
}
