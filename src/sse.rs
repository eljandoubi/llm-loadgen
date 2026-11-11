//! Server-sent events, parsed incrementally as the bytes arrive.
//!
//! Follows the WHATWG rules: lines end in LF, CRLF or CR, a blank line ends an event, a line
//! starting with `:` is a comment, `data` lines of one event are joined with a newline.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub data: String,
    pub name: Option<String>,
}

#[derive(Debug, Default)]
pub struct Parser {
    pending: Vec<u8>,
    data: Vec<String>,
    name: Option<String>,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next bytes; returns the events they completed. Chunk boundaries can fall
    /// anywhere, also in the middle of a line, of a CRLF or of a multi-byte character.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.pending.extend_from_slice(bytes);
        let buf = std::mem::take(&mut self.pending);
        let mut events = Vec::new();
        let (mut start, mut i) = (0, 0);
        while i < buf.len() {
            match buf[i] {
                b'\n' => {
                    self.line(&buf[start..i], &mut events);
                    i += 1;
                    start = i;
                }
                b'\r' => {
                    if i + 1 == buf.len() {
                        break; // a CR at the very end may be the first half of a CRLF
                    }
                    self.line(&buf[start..i], &mut events);
                    i += if buf[i + 1] == b'\n' { 2 } else { 1 };
                    start = i;
                }
                _ => i += 1,
            }
        }
        self.pending = buf[start..].to_vec();
        events
    }

    fn line(&mut self, line: &[u8], events: &mut Vec<Event>) {
        let line = String::from_utf8_lossy(line);
        if line.is_empty() {
            if !self.data.is_empty() {
                events.push(Event {
                    data: self.data.join("\n"),
                    name: self.name.take(),
                });
            }
            self.data.clear();
            self.name = None;
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((&line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => self.data.push(value.to_string()),
            "event" => self.name = Some(value.to_string()),
            _ => {} // id and retry mean nothing to a one-shot client
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(chunks: &[&[u8]]) -> Vec<Event> {
        let mut parser = Parser::new();
        chunks.iter().flat_map(|c| parser.push(c)).collect()
    }

    fn data(events: &[Event]) -> Vec<&str> {
        events.iter().map(|e| e.data.as_str()).collect()
    }

    const STREAM: &[u8] = b"data: {\"a\":1}\n\ndata: {\"a\":2}\n\ndata: [DONE]\n\n";

    #[test]
    fn events_are_separated_by_a_blank_line() {
        assert_eq!(
            data(&all(&[STREAM])),
            vec!["{\"a\":1}", "{\"a\":2}", "[DONE]"]
        );
    }

    #[test]
    fn the_result_does_not_depend_on_where_the_bytes_are_cut() {
        let whole = all(&[STREAM]);
        for cut in 0..=STREAM.len() {
            let (a, b) = STREAM.split_at(cut);
            assert_eq!(all(&[a, b]), whole, "cut at {cut}");
        }
        let byte_by_byte: Vec<&[u8]> = STREAM.chunks(1).collect();
        assert_eq!(all(&byte_by_byte), whole);
    }

    #[test]
    fn crlf_and_lone_cr_line_ends_work_in_any_split() {
        for stream in [
            &b"data: x\r\n\r\ndata: y\r\n\r\n"[..],
            // a CR at the very end could be half a CRLF, so the last event of a lone-CR
            // stream waits for the next byte: here the start of a third one
            b"data: x\r\rdata: y\r\rdata: z",
        ] {
            let want = vec!["x", "y"];
            assert_eq!(data(&all(&[stream])), want);
            for cut in 0..=stream.len() {
                let (a, b) = stream.split_at(cut);
                assert_eq!(data(&all(&[a, b])), want, "cut at {cut} of {stream:?}");
            }
        }
    }

    #[test]
    fn an_event_is_only_delivered_once_its_blank_line_has_arrived() {
        let mut parser = Parser::new();
        assert_eq!(parser.push(b"data: partial\n"), vec![]);
        assert_eq!(parser.push(b"data: more"), vec![]);
        let events = parser.push(b"\n\n");
        assert_eq!(data(&events), vec!["partial\nmore"]);
    }

    #[test]
    fn several_data_lines_are_joined_with_a_newline() {
        assert_eq!(
            data(&all(&[b"data: a\ndata: b\ndata:\ndata: c\n\n"])),
            vec!["a\nb\n\nc"]
        );
    }

    #[test]
    fn comments_and_unknown_fields_are_ignored() {
        let events = all(&[b": keep-alive\nid: 7\nretry: 100\ndata: x\n\n: another\n\n"]);
        assert_eq!(data(&events), vec!["x"]);
    }

    #[test]
    fn only_one_space_after_the_colon_is_dropped() {
        assert_eq!(
            data(&all(&[b"data:  two\n\ndata:none\n\n"])),
            vec![" two", "none"]
        );
    }

    #[test]
    fn the_event_name_belongs_to_the_next_event_only() {
        let events = all(&[b"event: usage\ndata: 1\n\ndata: 2\n\n"]);
        assert_eq!(events[0].name.as_deref(), Some("usage"));
        assert_eq!(events[1].name, None);
    }

    #[test]
    fn a_blank_line_without_data_makes_no_event() {
        assert_eq!(all(&[b"\n\n\nevent: x\n\n"]), vec![]);
    }

    #[test]
    fn a_character_split_across_chunks_is_decoded_whole() {
        let text = "data: h\u{e9}llo \u{1f600}\n\n".as_bytes();
        for cut in 0..=text.len() {
            let (a, b) = text.split_at(cut);
            assert_eq!(
                data(&all(&[a, b])),
                vec!["h\u{e9}llo \u{1f600}"],
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn invalid_utf8_is_replaced_not_fatal() {
        let events = all(&[b"data: a\xffb\n\n"]);
        assert_eq!(data(&events), vec!["a\u{fffd}b"]);
    }

    #[test]
    fn a_line_without_a_colon_is_a_field_with_an_empty_value() {
        assert_eq!(data(&all(&[b"data\n\n"])), vec![""]);
    }
}
