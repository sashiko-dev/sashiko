// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Server-sent events framing for providers that stream a reply.
//!
//! A provider asks for a streamed reply only to keep bytes flowing through
//! gateways that close idle requests; it folds the events back into the
//! document a non-streamed request returns.  This module turns the body's
//! bytes into events and nothing more.  What an event means, and which
//! one ends the stream, belongs to the provider.
//!
//! Framing follows the WHATWG event-stream format: lines end in LF, CRLF
//! or CR, a blank line ends an event, `data:` lines join with LF, `event:`
//! names the event, lines starting with a colon are comments, and every
//! other field is ignored.  An event counts only once its blank line has
//! arrived, so the partial event a cut connection leaves behind is never
//! handed to a provider.

use reqwest::Response;

/// One complete event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, when the server sent one.
    pub event: Option<String>,
    /// The `data:` lines joined with LF.
    pub data: String,
}

/// Incremental parser fed with body chunks as they arrive.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Bytes not yet consumed as a complete line.
    pending: Vec<u8>,
    /// How many leading bytes of `pending` are known to hold no line
    /// terminator, so a long line arriving in many chunks is scanned once
    /// rather than from its start on every chunk.
    scanned: usize,
    /// Set when the last chunk ended in a CR that ended a line.  An LF
    /// opening the next chunk is the rest of that CRLF, not a blank line.
    skip_lf: bool,
    /// Set once the first line has been read, after which a byte order
    /// mark is ordinary data.
    started: bool,
    event: Option<String>,
    data: Vec<String>,
}

/// A complete line that is not valid UTF-8.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("event stream line is not valid UTF-8")]
pub struct SseFramingError;

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes `bytes` and returns the events they complete, in order.
    pub fn push(&mut self, mut bytes: &[u8]) -> Result<Vec<SseEvent>, SseFramingError> {
        if self.skip_lf && !bytes.is_empty() {
            self.skip_lf = false;
            bytes = bytes.strip_prefix(b"\n").unwrap_or(bytes);
        }
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut start = 0;
        let mut from = self.scanned;
        while let Some((end, next)) = next_line(&self.pending[start..], from) {
            let line = std::str::from_utf8(&self.pending[start..start + end])
                .map_err(|_| SseFramingError)?
                .to_owned();
            start += next;
            from = 0;
            if let Some(event) = self.on_line(&line) {
                events.push(event);
            }
        }
        if start > 0 {
            self.skip_lf = start == self.pending.len() && self.pending[start - 1] == b'\r';
        }
        self.pending.drain(..start);
        self.scanned = self.pending.len();
        Ok(events)
    }

    fn on_line(&mut self, line: &str) -> Option<SseEvent> {
        let line = if self.started {
            line
        } else {
            self.started = true;
            line.strip_prefix('\u{feff}').unwrap_or(line)
        };
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
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        None
    }

    /// Ends the current event.  One without data lines is not an event,
    /// and its name does not carry over to the next one.
    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        if self.data.is_empty() {
            return None;
        }
        let data = std::mem::take(&mut self.data).join("\n");
        Some(SseEvent { event, data })
    }
}

/// Finds the first complete line in `buf`, returning the length of its
/// content and the offset just past its terminator.  The first `from`
/// bytes are known to hold no terminator.  A CR ends its line at once,
/// even as the last byte of `buf`: waiting for a possible LF would hold
/// back an event a bare CR completed until the body sent more bytes, or
/// lose it when the body ended there.  The caller skips the LF of a CRLF
/// split across chunks.
fn next_line(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let end = from + buf[from..].iter().position(|&b| b == b'\n' || b == b'\r')?;
    let crlf = buf[end] == b'\r' && buf.get(end + 1) == Some(&b'\n');
    Some((end, end + 1 + usize::from(crlf)))
}

/// Whether the provider handled an event as the last one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Done,
}

/// How reading a streamed body ended without the handler failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
    /// The handler returned `Flow::Done`.
    Terminal,
    /// The body ended before any event the handler took as the last one.
    Eof,
}

/// Why reading a streamed body failed.
#[derive(Debug, thiserror::Error)]
pub enum ReadError<E> {
    /// The connection failed while the body was being read.
    #[error("{0}")]
    Transport(reqwest::Error),
    #[error(transparent)]
    Framing(SseFramingError),
    /// The handler rejected an event.
    #[error(transparent)]
    Handler(E),
}

/// True when a reply carries an event stream rather than a document.
pub fn is_event_stream(res: &Response) -> bool {
    res.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/event-stream"))
}

/// Reads `res` chunk by chunk, handing each complete event to `on_event`
/// until it returns `Flow::Done` or the body ends.  Stopping at the
/// terminal event, rather than waiting for the server to close, keeps a
/// connection the server holds open from stalling the request.
pub async fn read_events<E>(
    mut res: Response,
    mut on_event: impl FnMut(SseEvent) -> Result<Flow, E>,
) -> Result<StreamEnd, ReadError<E>> {
    let mut parser = SseParser::new();
    while let Some(chunk) = res.chunk().await.map_err(ReadError::Transport)? {
        for event in parser.push(&chunk).map_err(ReadError::Framing)? {
            tracing::trace!(
                "SSE event {:?} ({} bytes of data)",
                event.event.as_deref().unwrap_or(""),
                event.data.len()
            );
            if on_event(event).map_err(ReadError::Handler)? == Flow::Done {
                return Ok(StreamEnd::Terminal);
            }
        }
    }
    Ok(StreamEnd::Eof)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::test_http;

    fn event(name: Option<&str>, data: &str) -> SseEvent {
        SseEvent {
            event: name.map(str::to_owned),
            data: data.to_owned(),
        }
    }

    fn parse_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        chunks
            .iter()
            .flat_map(|chunk| parser.push(chunk).unwrap())
            .collect()
    }

    const STREAM: &str = "event: message_start\ndata: {\"a\":1}\n\n\
        : keepalive comment\n\n\
        data: first\ndata: second\n\n\
        event: ping\r\ndata:\r\n\r\n\
        id: 7\rretry: 10\rdata:  two spaces\r\r\
        data: caf\u{e9} \u{2603}\n\n";

    fn expected() -> Vec<SseEvent> {
        vec![
            event(Some("message_start"), "{\"a\":1}"),
            event(None, "first\nsecond"),
            event(Some("ping"), ""),
            event(None, " two spaces"),
            event(None, "caf\u{e9} \u{2603}"),
        ]
    }

    #[test]
    fn parses_events_fields_comments_and_line_endings() {
        assert_eq!(parse_all(&[STREAM.as_bytes()]), expected());
    }

    #[test]
    fn every_split_point_yields_the_same_events() {
        let bytes = STREAM.as_bytes();
        for split in 0..=bytes.len() {
            let (a, b) = bytes.split_at(split);
            assert_eq!(parse_all(&[a, b]), expected(), "split at {split}");
        }
    }

    #[test]
    fn byte_at_a_time_yields_the_same_events() {
        let chunks: Vec<&[u8]> = STREAM.as_bytes().chunks(1).collect();
        assert_eq!(parse_all(&chunks), expected());
    }

    #[test]
    fn a_long_line_in_many_chunks_is_reassembled() {
        let data = "x".repeat(10_000);
        let stream = format!("data: {data}\r\n\r\ndata: tail\r\n\r\n");
        let chunks: Vec<&[u8]> = stream.as_bytes().chunks(7).collect();
        assert_eq!(
            parse_all(&chunks),
            vec![event(None, &data), event(None, "tail")]
        );
    }

    #[test]
    fn an_event_without_its_blank_line_is_not_dispatched() {
        assert_eq!(
            parse_all(&[b"data: done\n\ndata: cut off\n"]),
            vec![event(None, "done")]
        );
        assert_eq!(parse_all(&[b"data: cut off"]), vec![]);
    }

    #[test]
    fn a_trailing_cr_ends_its_line_and_its_lf_is_skipped() {
        let mut parser = SseParser::new();
        assert_eq!(
            parser.push(b"data: x\r\n\r").unwrap(),
            vec![event(None, "x")]
        );
        assert_eq!(parser.push(b"").unwrap(), vec![]);
        assert_eq!(parser.push(b"\ndata: y\n").unwrap(), vec![]);
        assert_eq!(parser.push(b"\n").unwrap(), vec![event(None, "y")]);
    }

    #[test]
    fn a_bare_cr_blank_line_at_the_end_dispatches() {
        assert_eq!(parse_all(&[b"data: x\r\r"]), vec![event(None, "x")]);
    }

    #[test]
    fn an_event_name_without_data_does_not_carry_over() {
        assert_eq!(
            parse_all(&[b"event: lonely\n\ndata: x\n\n"]),
            vec![event(None, "x")]
        );
    }

    #[test]
    fn a_leading_byte_order_mark_is_dropped_once() {
        assert_eq!(
            parse_all(&["\u{feff}data: x\n\n\u{feff}data: y\n\n".as_bytes()]),
            vec![event(None, "x")],
            "a later BOM starts a field the parser ignores"
        );
    }

    #[test]
    fn a_field_without_a_colon_has_an_empty_value() {
        assert_eq!(parse_all(&[b"data\n\n"]), vec![event(None, "")]);
    }

    #[test]
    fn invalid_utf8_in_a_complete_line_is_an_error() {
        let mut parser = SseParser::new();
        assert_eq!(parser.push(b"data: \xff\n\n"), Err(SseFramingError));
    }

    async fn read(
        reply: Vec<u8>,
        stop_at: &str,
    ) -> (Result<StreamEnd, ReadError<()>>, Vec<String>) {
        let server = test_http::serve(reply).await;
        let res = test_http::test_client()
            .get(&server.url)
            .send()
            .await
            .unwrap();
        let mut seen = Vec::new();
        let outcome = read_events(res, |event| {
            seen.push(event.data.clone());
            Ok(if event.data == stop_at {
                Flow::Done
            } else {
                Flow::Continue
            })
        })
        .await;
        server.finish().await;
        (outcome, seen)
    }

    #[tokio::test]
    async fn read_events_stops_at_the_terminal_event() {
        let (outcome, seen) = read(
            test_http::sse("data: a\n\ndata: end\n\ndata: after\n\n"),
            "end",
        )
        .await;
        assert!(matches!(outcome, Ok(StreamEnd::Terminal)), "{outcome:?}");
        assert_eq!(seen, ["a", "end"]);
    }

    #[tokio::test]
    async fn read_events_sees_an_event_ended_by_a_final_bare_cr() {
        let (outcome, seen) = read(test_http::sse("data: a\r\r"), "end").await;
        assert!(matches!(outcome, Ok(StreamEnd::Eof)), "{outcome:?}");
        assert_eq!(seen, ["a"]);
    }

    #[tokio::test]
    async fn read_events_reports_a_body_that_ends_early() {
        let (outcome, seen) = read(test_http::sse("data: a\n\ndata: end\n"), "end").await;
        assert!(matches!(outcome, Ok(StreamEnd::Eof)), "{outcome:?}");
        assert_eq!(seen, ["a"]);
    }

    #[tokio::test]
    async fn read_events_reports_a_dropped_connection() {
        let reply =
            test_http::truncated_with_type("200 OK", "text/event-stream", "", "data: a\n\n");
        let (outcome, seen) = read(reply, "end").await;
        assert!(
            matches!(outcome, Err(ReadError::Transport(_))),
            "{outcome:?}"
        );
        assert_eq!(seen, ["a"]);
    }

    #[tokio::test]
    async fn read_events_passes_handler_errors_through() {
        let server = test_http::serve(test_http::sse("data: a\n\n")).await;
        let res = test_http::test_client()
            .get(&server.url)
            .send()
            .await
            .unwrap();
        let outcome = read_events(res, |_| Err::<Flow, _>("rejected")).await;
        server.finish().await;
        assert!(matches!(outcome, Err(ReadError::Handler("rejected"))));
    }

    #[tokio::test]
    async fn content_type_decides_whether_a_reply_streams() {
        for (content_type, streams) in [
            ("text/event-stream", true),
            ("Text/Event-Stream; charset=utf-8", true),
            ("application/json", false),
        ] {
            let server = test_http::serve(test_http::complete_with_type(
                "200 OK",
                content_type,
                "",
                "",
            ))
            .await;
            let res = test_http::test_client()
                .get(&server.url)
                .send()
                .await
                .unwrap();
            assert_eq!(is_event_stream(&res), streams, "{content_type}");
            server.finish().await;
        }
    }
}
