//! Incremental Server-Sent Events (SSE) decoder.
//!
//! The decoder is Sans I/O: callers push bytes with [`SseDecoder::feed`]
//! and pull completed events with [`SseDecoder::next_event`]. Event
//! output does not depend on how the input byte stream is split into
//! chunks; a property-based test verifies this invariant.
//!
//! Only the subset of SSE required by the DeepSeek Chat Completions
//! streaming API is surfaced as structured events:
//!
//! - Lines beginning with `data:` accumulate for the current event and
//!   dispatch as [`SseEvent::Message`] on the terminating empty line.
//! - Lines beginning with `:` (comments, typically used as keep-alive
//!   frames) are surfaced as [`SseEvent::Comment`] so the caller can
//!   log or ignore them.
//! - `event`, `id`, and `retry` fields are recognised and silently
//!   ignored — the target API does not use them.
//!
//! Line terminators accepted: `\n`, `\r\n`, and lone `\r`, matching the
//! WHATWG event stream algorithm.

use std::mem;

/// Event produced by [`SseDecoder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseEvent {
    /// A completed event with the accumulated `data:` field value.
    ///
    /// Multiple `data:` lines in the same event are joined with `\n`,
    /// following the WHATWG event stream algorithm.
    Message { data: String },
    /// A comment frame (line beginning with `:`), typically emitted as
    /// a keep-alive by upstream servers.
    Comment(String),
}

/// Errors that can occur while decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseError {
    /// A single line grew past the configured byte limit before a
    /// terminator was seen.
    LineTooLong { limit: usize },
    /// A line or field value contained bytes that are not valid UTF-8.
    InvalidUtf8,
}

impl std::fmt::Display for SseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SseError::LineTooLong { limit } => {
                write!(f, "SSE line exceeded {limit}-byte limit")
            }
            SseError::InvalidUtf8 => f.write_str("SSE line contained invalid UTF-8"),
        }
    }
}

impl std::error::Error for SseError {}

/// Incremental SSE decoder.
///
/// The decoder retains an internal buffer of unconsumed bytes. Callers
/// alternate [`Self::feed`] (append received bytes) and
/// [`Self::next_event`] (extract events) in any order.
///
/// Once [`Self::next_event`] returns an error, the same error is
/// returned by every subsequent call. Recovery requires constructing a
/// new decoder — mixing further events after a parse failure would
/// silently blur the boundary between the two.
pub struct SseDecoder {
    max_line_bytes: usize,
    buf: Vec<u8>,
    data: String,
    has_data_field: bool,
    error: Option<SseError>,
}

impl SseDecoder {
    /// Default upper bound applied to a single SSE line, in bytes.
    ///
    /// The limit is enforced in [`Self::next_event`], not in
    /// [`Self::feed`], so the transport layer never needs to know
    /// about it.
    pub const DEFAULT_MAX_LINE_BYTES: usize = 1024 * 1024;

    /// Create a decoder that uses [`Self::DEFAULT_MAX_LINE_BYTES`].
    pub fn new() -> Self {
        Self::with_max_line_bytes(Self::DEFAULT_MAX_LINE_BYTES)
    }

    /// Create a decoder with a custom per-line byte limit.
    pub fn with_max_line_bytes(max_line_bytes: usize) -> Self {
        Self {
            max_line_bytes,
            buf: Vec::new(),
            data: String::new(),
            has_data_field: false,
            error: None,
        }
    }

    /// The configured per-line byte limit.
    pub fn max_line_bytes(&self) -> usize {
        self.max_line_bytes
    }

    /// Append received bytes to the internal buffer.
    ///
    /// Never fails; validation happens in [`Self::next_event`]. After a
    /// prior error, `feed` still accepts bytes but they are never
    /// parsed — the sticky error is returned instead.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Attempt to extract the next event.
    ///
    /// Returns `Ok(None)` when more bytes are required to complete an
    /// event. Returns the sticky error once a parse failure has been
    /// observed.
    pub fn next_event(&mut self) -> Result<Option<SseEvent>, SseError> {
        if let Some(err) = &self.error {
            return Err(err.clone());
        }
        loop {
            let line = match self.take_line() {
                Ok(Some(line)) => line,
                Ok(None) => return Ok(None),
                Err(err) => return Err(self.fail(err)),
            };

            if line.is_empty() {
                if self.has_data_field {
                    let mut data = mem::take(&mut self.data);
                    if data.ends_with('\n') {
                        data.pop();
                    }
                    self.has_data_field = false;
                    return Ok(Some(SseEvent::Message { data }));
                }
                continue;
            }

            if line[0] == b':' {
                let content = match std::str::from_utf8(&line[1..]) {
                    Ok(s) => s,
                    Err(_) => return Err(self.fail(SseError::InvalidUtf8)),
                };
                let content = content.strip_prefix(' ').unwrap_or(content).to_string();
                return Ok(Some(SseEvent::Comment(content)));
            }

            let (field_bytes, value_bytes) = match line.iter().position(|&b| b == b':') {
                Some(i) => {
                    let field = &line[..i];
                    let value_start = if line.get(i + 1) == Some(&b' ') {
                        i + 2
                    } else {
                        i + 1
                    };
                    (field, &line[value_start..])
                }
                None => (&line[..], &b""[..]),
            };

            let field = match std::str::from_utf8(field_bytes) {
                Ok(s) => s,
                Err(_) => return Err(self.fail(SseError::InvalidUtf8)),
            };
            let value = match std::str::from_utf8(value_bytes) {
                Ok(s) => s,
                Err(_) => return Err(self.fail(SseError::InvalidUtf8)),
            };

            match field {
                "data" => {
                    self.data.push_str(value);
                    self.data.push('\n');
                    self.has_data_field = true;
                }
                "event" | "id" | "retry" => {}
                _ => {}
            }
        }
    }

    fn fail(&mut self, err: SseError) -> SseError {
        self.error = Some(err.clone());
        err
    }

    fn take_line(&mut self) -> Result<Option<Vec<u8>>, SseError> {
        let mut terminator = None;
        for (i, &b) in self.buf.iter().enumerate() {
            if b == b'\n' || b == b'\r' {
                terminator = Some((i, b));
                break;
            }
        }
        let (idx, kind) = match terminator {
            Some(t) => t,
            None => return self.pending_or_too_long(),
        };
        let consumed = if kind == b'\r' {
            match self.buf.get(idx + 1) {
                Some(&b'\n') => idx + 2,
                Some(_) => idx + 1,
                None => return self.pending_or_too_long(),
            }
        } else {
            idx + 1
        };
        if idx > self.max_line_bytes {
            return Err(SseError::LineTooLong {
                limit: self.max_line_bytes,
            });
        }
        let mut line: Vec<u8> = self.buf.drain(..consumed).collect();
        line.truncate(idx);
        Ok(Some(line))
    }

    fn pending_or_too_long(&self) -> Result<Option<Vec<u8>>, SseError> {
        if self.buf.len() > self.max_line_bytes {
            Err(SseError::LineTooLong {
                limit: self.max_line_bytes,
            })
        } else {
            Ok(None)
        }
    }
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(decoder: &mut SseDecoder) -> Result<Vec<SseEvent>, SseError> {
        let mut events = Vec::new();
        loop {
            match decoder.next_event()? {
                Some(e) => events.push(e),
                None => return Ok(events),
            }
        }
    }

    #[test]
    fn single_data_line_dispatches_on_empty_line() {
        let mut d = SseDecoder::new();
        d.feed(b"data: hello\n\n");
        assert_eq!(
            drain(&mut d).expect("drain"),
            vec![SseEvent::Message {
                data: "hello".to_string()
            }]
        );
    }

    #[test]
    fn multi_data_lines_joined_with_newline() {
        let mut d = SseDecoder::new();
        d.feed(b"data: a\ndata: b\ndata: c\n\n");
        assert_eq!(
            drain(&mut d).expect("drain"),
            vec![SseEvent::Message {
                data: "a\nb\nc".to_string()
            }]
        );
    }

    #[test]
    fn empty_data_field_dispatches_empty_message() {
        let mut d = SseDecoder::new();
        d.feed(b"data:\n\n");
        assert_eq!(
            drain(&mut d).expect("drain"),
            vec![SseEvent::Message {
                data: String::new()
            }]
        );
    }

    #[test]
    fn comment_line_is_surfaced() {
        let mut d = SseDecoder::new();
        d.feed(b": keep-alive\n");
        assert_eq!(
            drain(&mut d).expect("drain"),
            vec![SseEvent::Comment("keep-alive".to_string())]
        );
    }

    #[test]
    fn comment_without_space_prefix() {
        let mut d = SseDecoder::new();
        d.feed(b":ping\n");
        assert_eq!(
            drain(&mut d).expect("drain"),
            vec![SseEvent::Comment("ping".to_string())]
        );
    }

    #[test]
    fn unknown_field_is_ignored() {
        let mut d = SseDecoder::new();
        d.feed(b"event: message\ndata: x\n\n");
        assert_eq!(
            drain(&mut d).expect("drain"),
            vec![SseEvent::Message {
                data: "x".to_string()
            }]
        );
    }

    #[test]
    fn done_sentinel_is_returned_as_regular_data() {
        let mut d = SseDecoder::new();
        d.feed(b"data: [DONE]\n\n");
        assert_eq!(
            drain(&mut d).expect("drain"),
            vec![SseEvent::Message {
                data: "[DONE]".to_string()
            }]
        );
    }

    #[test]
    fn empty_line_between_events_does_not_produce_stray_events() {
        let mut d = SseDecoder::new();
        d.feed(b"\n\n\ndata: x\n\n");
        assert_eq!(
            drain(&mut d).expect("drain"),
            vec![SseEvent::Message {
                data: "x".to_string()
            }]
        );
    }

    #[test]
    fn crlf_line_endings_are_supported() {
        let mut d = SseDecoder::new();
        d.feed(b"data: hi\r\n\r\n");
        assert_eq!(
            drain(&mut d).expect("drain"),
            vec![SseEvent::Message {
                data: "hi".to_string()
            }]
        );
    }

    #[test]
    fn lone_cr_line_endings_are_supported() {
        let mut d = SseDecoder::new();
        // A non-`\r` byte after each `\r` lets the decoder confirm the
        // terminator immediately; a stream that ends on a lone `\r`
        // waits for more data because it might turn into `\r\n`.
        d.feed(b"data: hi\r\rdata: bye\r\n\r\n");
        assert_eq!(
            drain(&mut d).expect("drain"),
            vec![
                SseEvent::Message {
                    data: "hi".to_string()
                },
                SseEvent::Message {
                    data: "bye".to_string()
                },
            ]
        );
    }

    #[test]
    fn cr_at_buffer_boundary_waits_for_next_byte() {
        let mut d = SseDecoder::new();
        d.feed(b"data: hi\r");
        assert_eq!(d.next_event().expect("no-error"), None);
        d.feed(b"\n\r\n");
        assert_eq!(
            drain(&mut d).expect("drain"),
            vec![SseEvent::Message {
                data: "hi".to_string()
            }]
        );
    }

    #[test]
    fn invalid_utf8_returns_error_and_sticks() {
        let mut d = SseDecoder::new();
        d.feed(b"data: \xff\xfe\n\n");
        let err = d.next_event().expect_err("first call returns error");
        assert_eq!(err, SseError::InvalidUtf8);
        let err2 = d
            .next_event()
            .expect_err("subsequent call returns same error");
        assert_eq!(err2, SseError::InvalidUtf8);
    }

    #[test]
    fn line_too_long_returns_error_before_terminator() {
        let mut d = SseDecoder::with_max_line_bytes(8);
        d.feed(b"data: 0123456789\n\n");
        let err = d.next_event().expect_err("length limit breached");
        assert_eq!(err, SseError::LineTooLong { limit: 8 });
    }

    #[test]
    fn split_at_every_position_produces_same_events() {
        let input = b"data: hello\ndata: world\n\n: keep\n\ndata: end\n\n";
        let mut baseline = SseDecoder::new();
        baseline.feed(input);
        let expected = drain(&mut baseline).expect("baseline");

        for split in 0..=input.len() {
            let mut d = SseDecoder::new();
            let mut got = Vec::new();
            d.feed(&input[..split]);
            while let Some(event) = d.next_event().expect("first-half drain") {
                got.push(event);
            }
            d.feed(&input[split..]);
            while let Some(event) = d.next_event().expect("second-half drain") {
                got.push(event);
            }
            assert_eq!(got, expected, "diverged at split {split}");
        }
    }
}
