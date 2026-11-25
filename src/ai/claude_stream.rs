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

//! Rebuilds a streamed Anthropic Messages reply as the message a
//! non-streamed request returns.
//!
//! The content-block lifecycle decides whether a message is complete, so
//! the stream is folded by an explicit state machine.  `step` is pure: it
//! takes a state and one event and returns the next state, or an error
//! naming both.  Every pair not listed below is an illegal transition.
//!
//! | From                 | Event                                    | To           |
//! |----------------------|------------------------------------------|--------------|
//! | AwaitingMessageStart | message_start                            | Idle         |
//! | Idle                 | content_block_start (index = blocks)     | InBlock      |
//! | InBlock              | content_block_delta (same index, fits)   | InBlock      |
//! | InBlock              | content_block_stop (same index)          | Idle         |
//! | Idle, MessageDelta   | message_delta                            | MessageDelta |
//! | MessageDelta         | message_stop                             | Done         |
//! | any but Done         | ping, an event type this build lacks     | unchanged    |
//! | any but Done         | error                                    | upstream error |
//!
//! Blocks are kept as the JSON objects their start events carry, with
//! deltas applied, so a block type the parser does not model reaches it
//! unchanged and fails or passes exactly as in a non-streamed reply.

use crate::ai::sse::{Flow, SseEvent};
use serde_json::{Map, Value};
use std::borrow::Cow;

/// Why a stream cannot be rebuilt.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StreamError {
    /// An event that does not have the shape the format documents.
    #[error("malformed event: {0}")]
    Malformed(String),
    /// A well-formed event that cannot follow the events before it.
    #[error("{event} cannot follow state {from}")]
    IllegalTransition { from: &'static str, event: String },
    /// The endpoint reported a failure inside the stream.
    #[error("stream reported {error_type}: {message}")]
    Upstream { error_type: String, message: String },
}

fn malformed(detail: impl Into<String>) -> StreamError {
    StreamError::Malformed(detail.into())
}

/// One event, parsed from its data.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    MessageStart {
        message: Map<String, Value>,
    },
    ContentBlockStart {
        index: usize,
        block: Map<String, Value>,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: Map<String, Value>,
        usage: Option<Map<String, Value>>,
    },
    MessageStop,
    Ping,
    Error {
        error_type: String,
        message: String,
    },
    /// An event type Anthropic added after this build.  The API documents
    /// that new event types may appear and are to be skipped.
    Unknown {
        kind: String,
    },
}

/// The change one content_block_delta makes to its block.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Text(String),
    Citation(Value),
    Thinking(String),
    Signature(String),
    InputJson(String),
    /// A delta type this build does not know.  It cannot be skipped like
    /// an unknown event: it carries part of a block, and dropping it would
    /// hand the stage a message the endpoint never sent.
    Unknown(String),
}

impl Delta {
    fn name(&self) -> &str {
        match self {
            Delta::Text(_) => "text_delta",
            Delta::Citation(_) => "citations_delta",
            Delta::Thinking(_) => "thinking_delta",
            Delta::Signature(_) => "signature_delta",
            Delta::InputJson(_) => "input_json_delta",
            Delta::Unknown(kind) => kind,
        }
    }
}

impl Event {
    /// A short name for logs and errors.  It never includes content.
    pub fn name(&self) -> Cow<'_, str> {
        match self {
            Event::MessageStart { .. } => "message_start".into(),
            Event::ContentBlockStart { index, block } => format!(
                "content_block_start({index}, {})",
                block.get("type").and_then(Value::as_str).unwrap_or("?")
            )
            .into(),
            Event::ContentBlockDelta { index, delta } => {
                format!("content_block_delta({index}, {})", delta.name()).into()
            }
            Event::ContentBlockStop { index } => format!("content_block_stop({index})").into(),
            Event::MessageDelta { .. } => "message_delta".into(),
            Event::MessageStop => "message_stop".into(),
            Event::Ping => "ping".into(),
            Event::Error { error_type, .. } => format!("error({error_type})").into(),
            Event::Unknown { kind } => format!("unknown({kind})").into(),
        }
    }

    /// Parses an event's data.  The type comes from the data rather than
    /// the `event:` line, which a gateway may drop.
    pub fn parse(data: &str) -> Result<Self, StreamError> {
        let mut value: Value = serde_json::from_str(data)
            .map_err(|e| malformed(format!("event data is not JSON: {e}")))?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| malformed("event data is not a JSON object"))?;
        let kind = match object.get("type") {
            Some(Value::String(kind)) => kind.clone(),
            _ => return Err(malformed("event has no type")),
        };
        Ok(match kind.as_str() {
            "message_start" => Event::MessageStart {
                message: take_object(object, "message", &kind)?,
            },
            "content_block_start" => Event::ContentBlockStart {
                index: index_of(object, &kind)?,
                block: take_object(object, "content_block", &kind)?,
            },
            "content_block_delta" => Event::ContentBlockDelta {
                index: index_of(object, &kind)?,
                delta: parse_delta(take_object(object, "delta", &kind)?)?,
            },
            "content_block_stop" => Event::ContentBlockStop {
                index: index_of(object, &kind)?,
            },
            "message_delta" => Event::MessageDelta {
                delta: take_object(object, "delta", &kind)?,
                usage: match object.remove("usage") {
                    None | Some(Value::Null) => None,
                    Some(Value::Object(usage)) => Some(usage),
                    Some(_) => return Err(malformed("message_delta usage is not an object")),
                },
            },
            "message_stop" => Event::MessageStop,
            "ping" => Event::Ping,
            "error" => {
                let error = object.get("error");
                let field = |name: &str| {
                    error
                        .and_then(|e| e.get(name))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                };
                Event::Error {
                    error_type: field("type").unwrap_or_else(|| "unknown_error".to_string()),
                    message: field("message").unwrap_or_default(),
                }
            }
            _ => Event::Unknown { kind },
        })
    }
}

fn take_object(
    object: &mut Map<String, Value>,
    field: &str,
    kind: &str,
) -> Result<Map<String, Value>, StreamError> {
    match object.remove(field) {
        Some(Value::Object(inner)) => Ok(inner),
        _ => Err(malformed(format!("{kind} has no {field} object"))),
    }
}

fn index_of(object: &Map<String, Value>, kind: &str) -> Result<usize, StreamError> {
    object
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| malformed(format!("{kind} has no index")))
}

fn parse_delta(mut delta: Map<String, Value>) -> Result<Delta, StreamError> {
    let kind = match delta.get("type") {
        Some(Value::String(kind)) => kind.clone(),
        _ => return Err(malformed("content_block_delta has no delta type")),
    };
    let mut text = |field: &str| match delta.remove(field) {
        Some(Value::String(text)) => Ok(text),
        _ => Err(malformed(format!("{kind} has no {field} string"))),
    };
    Ok(match kind.as_str() {
        "text_delta" => Delta::Text(text("text")?),
        "thinking_delta" => Delta::Thinking(text("thinking")?),
        "signature_delta" => Delta::Signature(text("signature")?),
        "input_json_delta" => Delta::InputJson(text("partial_json")?),
        "citations_delta" => match delta.remove("citation") {
            Some(citation) if citation.is_object() => Delta::Citation(citation),
            _ => return Err(malformed("citations_delta has no citation object")),
        },
        _ => Delta::Unknown(kind),
    })
}

/// The message so far: the skeleton from message_start and the blocks
/// already closed.
#[derive(Debug, Clone, PartialEq)]
pub struct Partial {
    message: Map<String, Value>,
    blocks: Vec<Value>,
    /// Index of a closed tool block whose input never became valid JSON.
    /// Only a reply cut off by max_tokens may end that way, and only in
    /// its last block.
    incomplete_input: Option<usize>,
}

/// The block between its start and stop events.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenBlock {
    index: usize,
    block: Map<String, Value>,
    /// The input_json_delta fragments, parsed when the block closes.
    input_json: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    AwaitingMessageStart,
    Idle(Partial),
    InBlock(Partial, OpenBlock),
    MessageDelta(Partial),
    /// The rebuilt message.
    Done(Value),
}

impl State {
    pub fn name(&self) -> &'static str {
        match self {
            State::AwaitingMessageStart => "AwaitingMessageStart",
            State::Idle(_) => "Idle",
            State::InBlock(..) => "InBlock",
            State::MessageDelta(_) => "MessageDelta",
            State::Done(_) => "Done",
        }
    }
}

fn illegal(from: &State, event: &Event) -> StreamError {
    StreamError::IllegalTransition {
        from: from.name(),
        event: event.name().into_owned(),
    }
}

/// The transition function.  See the module documentation for the table.
pub fn step(state: State, event: Event) -> Result<State, StreamError> {
    match (state, event) {
        (state @ State::Done(_), event) => Err(illegal(&state, &event)),
        (
            _,
            Event::Error {
                error_type,
                message,
            },
        ) => Err(StreamError::Upstream {
            error_type,
            message,
        }),
        (state, Event::Ping | Event::Unknown { .. }) => Ok(state),
        (State::AwaitingMessageStart, Event::MessageStart { message }) => {
            Ok(State::Idle(Partial {
                message,
                blocks: Vec::new(),
                incomplete_input: None,
            }))
        }
        (State::Idle(partial), Event::ContentBlockStart { index, block })
            if index == partial.blocks.len() =>
        {
            if let Some(cut) = partial.incomplete_input {
                return Err(malformed(format!(
                    "block {index} follows block {cut}, whose tool input is not valid JSON"
                )));
            }
            Ok(State::InBlock(
                partial,
                OpenBlock {
                    index,
                    block,
                    input_json: String::new(),
                },
            ))
        }
        (State::InBlock(partial, mut open), Event::ContentBlockDelta { index, delta })
            if index == open.index =>
        {
            open.apply(delta)?;
            Ok(State::InBlock(partial, open))
        }
        (State::InBlock(mut partial, open), Event::ContentBlockStop { index })
            if index == open.index =>
        {
            partial.close(open);
            Ok(State::Idle(partial))
        }
        (
            State::Idle(mut partial) | State::MessageDelta(mut partial),
            Event::MessageDelta { delta, usage },
        ) => {
            partial.apply_message_delta(delta, usage);
            Ok(State::MessageDelta(partial))
        }
        (State::MessageDelta(partial), Event::MessageStop) => Ok(State::Done(partial.finish()?)),
        (state, event) => Err(illegal(&state, &event)),
    }
}

/// Appends to a string field of a block, creating it when absent.
fn append_to(
    block: &mut Map<String, Value>,
    field: &str,
    fragment: &str,
) -> Result<(), StreamError> {
    match block.get_mut(field) {
        Some(Value::String(text)) => text.push_str(fragment),
        None | Some(Value::Null) => {
            block.insert(field.to_string(), Value::String(fragment.to_string()));
        }
        Some(_) => return Err(malformed(format!("block field {field} is not a string"))),
    }
    Ok(())
}

impl OpenBlock {
    fn block_type(&self) -> &str {
        self.block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("untyped")
    }

    fn apply(&mut self, delta: Delta) -> Result<(), StreamError> {
        let block_type = self.block_type().to_owned();
        let mismatch = |delta: &Delta| {
            malformed(format!(
                "{} does not apply to a {block_type} block",
                delta.name()
            ))
        };
        match delta {
            Delta::Text(text) if block_type == "text" => append_to(&mut self.block, "text", &text),
            Delta::Citation(citation) if block_type == "text" => {
                match self
                    .block
                    .entry("citations")
                    .or_insert_with(|| Value::Array(Vec::new()))
                {
                    Value::Array(citations) => citations.push(citation),
                    citations @ Value::Null => *citations = Value::Array(vec![citation]),
                    _ => return Err(malformed("block field citations is not an array")),
                }
                Ok(())
            }
            Delta::Thinking(text) if block_type == "thinking" => {
                append_to(&mut self.block, "thinking", &text)
            }
            Delta::Signature(text) if block_type == "thinking" => {
                append_to(&mut self.block, "signature", &text)
            }
            Delta::InputJson(fragment) if self.block.contains_key("input") => {
                self.input_json.push_str(&fragment);
                Ok(())
            }
            other => Err(mismatch(&other)),
        }
    }
}

impl Partial {
    /// Closes a block.  Its input fragments, if any, replace the input of
    /// the start event.  Fragments that do not parse leave that input in
    /// place and mark the block, which only a max_tokens stop excuses.
    fn close(&mut self, open: OpenBlock) {
        let OpenBlock {
            index,
            mut block,
            input_json,
        } = open;
        if !input_json.is_empty() {
            match serde_json::from_str::<Value>(&input_json) {
                Ok(input) => {
                    block.insert("input".to_string(), input);
                }
                Err(_) => self.incomplete_input = Some(index),
            }
        }
        self.blocks.push(Value::Object(block));
    }

    /// Copies the top-level fields of a message_delta (stop_reason,
    /// stop_sequence) onto the message and overwrites the usage counters
    /// it carries.  Its output_tokens is cumulative, and a null counter
    /// means not reported rather than zero.
    fn apply_message_delta(
        &mut self,
        delta: Map<String, Value>,
        usage: Option<Map<String, Value>>,
    ) {
        self.message.extend(delta);
        let Some(usage) = usage else {
            return;
        };
        let target = self
            .message
            .entry("usage")
            .or_insert_with(|| Value::Object(Map::new()));
        if !target.is_object() {
            *target = Value::Object(Map::new());
        }
        if let Value::Object(target) = target {
            target.extend(usage.into_iter().filter(|(_, count)| !count.is_null()));
        }
    }

    fn finish(mut self) -> Result<Value, StreamError> {
        if let Some(index) = self.incomplete_input {
            let stop_reason = self.message.get("stop_reason").and_then(Value::as_str);
            if stop_reason != Some("max_tokens") {
                return Err(malformed(format!(
                    "tool input of block {index} is not valid JSON, and the reply was not cut off by max_tokens"
                )));
            }
        }
        self.message
            .insert("content".to_string(), Value::Array(self.blocks));
        Ok(Value::Object(self.message))
    }
}

/// Drives `step` over a stream, logging each transition.
#[derive(Debug)]
pub struct MessageStream {
    /// None only after a transition failed, when the stream is abandoned.
    state: Option<State>,
}

impl Default for MessageStream {
    fn default() -> Self {
        Self {
            state: Some(State::AwaitingMessageStart),
        }
    }
}

impl MessageStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_event(&mut self, event: SseEvent) -> Result<Flow, StreamError> {
        let event = Event::parse(&event.data)?;
        let event_name = event.name().into_owned();
        let state = self
            .state
            .take()
            .ok_or_else(|| malformed("event after the stream failed"))?;
        let from = state.name();
        let next = step(state, event).inspect_err(|e| {
            tracing::debug!("Claude stream: {from} --{event_name}--> error: {e}");
        })?;
        tracing::trace!("Claude stream: {from} --{event_name}--> {}", next.name());
        let flow = match next {
            State::Done(_) => Flow::Done,
            _ => Flow::Continue,
        };
        self.state = Some(next);
        Ok(flow)
    }

    /// The state the stream is in, for the error when it ends early.
    pub fn state_name(&self) -> &'static str {
        self.state.as_ref().map_or("Failed", State::name)
    }

    /// The rebuilt message, once message_stop has arrived.
    pub fn into_document(self) -> Option<Value> {
        match self.state {
            Some(State::Done(document)) => Some(document),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn message_start() -> Event {
        Event::MessageStart {
            message: object(json!({
                "id": "msg_1", "type": "message", "role": "assistant",
                "model": "claude-test", "content": [], "stop_reason": null,
                "usage": {"input_tokens": 12, "output_tokens": 1,
                          "cache_read_input_tokens": 5}
            })),
        }
    }

    fn block_start(index: usize, block: Value) -> Event {
        Event::ContentBlockStart {
            index,
            block: object(block),
        }
    }

    fn delta(index: usize, delta: Delta) -> Event {
        Event::ContentBlockDelta { index, delta }
    }

    fn stop(index: usize) -> Event {
        Event::ContentBlockStop { index }
    }

    fn message_delta(stop_reason: &str) -> Event {
        Event::MessageDelta {
            delta: object(json!({"stop_reason": stop_reason, "stop_sequence": null})),
            usage: Some(object(json!({"output_tokens": 42, "input_tokens": null}))),
        }
    }

    fn run(events: Vec<Event>) -> Result<State, StreamError> {
        events
            .into_iter()
            .try_fold(State::AwaitingMessageStart, step)
    }

    fn idle() -> State {
        run(vec![message_start()]).unwrap()
    }

    fn in_text_block() -> State {
        run(vec![
            message_start(),
            block_start(0, json!({"type": "text", "text": ""})),
        ])
        .unwrap()
    }

    fn in_message_delta() -> State {
        run(vec![message_start(), message_delta("end_turn")]).unwrap()
    }

    fn done() -> State {
        run(vec![
            message_start(),
            message_delta("end_turn"),
            Event::MessageStop,
        ])
        .unwrap()
    }

    fn upstream() -> Event {
        Event::Error {
            error_type: "overloaded_error".to_string(),
            message: "busy".to_string(),
        }
    }

    fn assert_illegal(state: State, event: Event) {
        let from = state.name();
        let name = event.name().into_owned();
        assert_eq!(
            step(state, event),
            Err(StreamError::IllegalTransition { from, event: name }),
        );
    }

    // One test per row of the transition table, then the rejections.

    #[test]
    fn row_awaiting_message_start_takes_message_start() {
        assert_eq!(idle().name(), "Idle");
    }

    #[test]
    fn row_idle_takes_the_next_block_start() {
        assert_eq!(in_text_block().name(), "InBlock");
    }

    #[test]
    fn row_in_block_takes_a_fitting_delta() {
        let state = step(in_text_block(), delta(0, Delta::Text("hi".into()))).unwrap();
        let State::InBlock(_, open) = state else {
            panic!("expected InBlock");
        };
        assert_eq!(open.block["text"], "hi");
    }

    #[test]
    fn row_in_block_takes_its_stop() {
        let state = step(in_text_block(), stop(0)).unwrap();
        let State::Idle(partial) = state else {
            panic!("expected Idle");
        };
        assert_eq!(partial.blocks, vec![json!({"type": "text", "text": ""})]);
    }

    #[test]
    fn row_idle_and_message_delta_take_message_delta() {
        assert_eq!(in_message_delta().name(), "MessageDelta");
        let again = step(in_message_delta(), message_delta("max_tokens")).unwrap();
        let State::MessageDelta(partial) = again else {
            panic!("expected MessageDelta");
        };
        assert_eq!(partial.message["stop_reason"], "max_tokens");
    }

    #[test]
    fn row_message_delta_takes_message_stop() {
        assert_eq!(done().name(), "Done");
    }

    #[test]
    fn row_ping_and_unknown_events_leave_the_state_alone() {
        for state in [
            State::AwaitingMessageStart,
            idle(),
            in_text_block(),
            in_message_delta(),
        ] {
            for event in [
                Event::Ping,
                Event::Unknown {
                    kind: "future_event".into(),
                },
            ] {
                assert_eq!(step(state.clone(), event).unwrap(), state);
            }
        }
    }

    #[test]
    fn row_error_fails_from_every_live_state() {
        for state in [
            State::AwaitingMessageStart,
            idle(),
            in_text_block(),
            in_message_delta(),
        ] {
            assert_eq!(
                step(state, upstream()),
                Err(StreamError::Upstream {
                    error_type: "overloaded_error".into(),
                    message: "busy".into(),
                })
            );
        }
    }

    #[test]
    fn done_takes_nothing_further() {
        for event in [Event::Ping, upstream(), Event::MessageStop, message_start()] {
            assert_illegal(done(), event);
        }
    }

    #[test]
    fn events_before_message_start_are_illegal() {
        for event in [
            block_start(0, json!({"type": "text", "text": ""})),
            delta(0, Delta::Text("x".into())),
            stop(0),
            message_delta("end_turn"),
            Event::MessageStop,
        ] {
            assert_illegal(State::AwaitingMessageStart, event);
        }
    }

    #[test]
    fn a_second_message_start_is_illegal() {
        for state in [idle(), in_text_block(), in_message_delta()] {
            assert_illegal(state, message_start());
        }
    }

    #[test]
    fn blocks_must_start_in_order() {
        assert_illegal(idle(), block_start(1, json!({"type": "text", "text": ""})));
        let after_first = step(in_text_block(), stop(0)).unwrap();
        assert_illegal(
            after_first,
            block_start(0, json!({"type": "text", "text": ""})),
        );
    }

    #[test]
    fn blocks_must_not_nest_or_interleave() {
        assert_illegal(
            in_text_block(),
            block_start(1, json!({"type": "text", "text": ""})),
        );
        assert_illegal(in_text_block(), delta(1, Delta::Text("x".into())));
        assert_illegal(in_text_block(), stop(1));
        assert_illegal(in_text_block(), message_delta("end_turn"));
        assert_illegal(in_text_block(), Event::MessageStop);
    }

    #[test]
    fn deltas_and_stops_outside_a_block_are_illegal() {
        assert_illegal(idle(), delta(0, Delta::Text("x".into())));
        assert_illegal(idle(), stop(0));
        assert_illegal(in_message_delta(), block_start(0, json!({"type": "text"})));
        assert_illegal(in_message_delta(), delta(0, Delta::Text("x".into())));
    }

    #[test]
    fn message_stop_needs_a_message_delta_first() {
        assert_illegal(idle(), Event::MessageStop);
    }

    #[test]
    fn a_delta_must_fit_its_block() {
        let thinking = || {
            run(vec![
                message_start(),
                block_start(0, json!({"type": "thinking", "thinking": ""})),
            ])
            .unwrap()
        };
        for (state, bad) in [
            (in_text_block(), Delta::Thinking("x".into())),
            (in_text_block(), Delta::Signature("x".into())),
            (in_text_block(), Delta::InputJson("{}".into())),
            (thinking(), Delta::Text("x".into())),
            (thinking(), Delta::Citation(json!({}))),
            (in_text_block(), Delta::Unknown("future_delta".into())),
        ] {
            let name = bad.name().to_owned();
            let err = step(state, delta(0, bad)).unwrap_err();
            assert!(
                matches!(&err, StreamError::Malformed(m) if m.contains(&name)),
                "{name}: {err}"
            );
        }
    }

    fn rebuild(events: Vec<Event>) -> Result<Value, StreamError> {
        match run(events)? {
            State::Done(document) => Ok(document),
            other => panic!("stream did not finish: {}", other.name()),
        }
    }

    #[test]
    fn a_full_stream_rebuilds_the_message() {
        let document = rebuild(vec![
            message_start(),
            Event::Ping,
            block_start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""}),
            ),
            delta(0, Delta::Thinking("Let me ".into())),
            delta(0, Delta::Thinking("look.".into())),
            delta(0, Delta::Signature("sig".into())),
            stop(0),
            block_start(1, json!({"type": "text", "text": ""})),
            delta(1, Delta::Text("Reading ".into())),
            delta(1, Delta::Citation(json!({"cited_text": "x"}))),
            delta(1, Delta::Text("the file.".into())),
            stop(1),
            block_start(
                2,
                json!({"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {}}),
            ),
            delta(2, Delta::InputJson("{\"path\": \"a".into())),
            delta(2, Delta::InputJson(".rs\"}".into())),
            stop(2),
            block_start(
                3,
                json!({"type": "tool_use", "id": "toolu_2", "name": "list", "input": {}}),
            ),
            stop(3),
            message_delta("tool_use"),
            Event::MessageStop,
        ])
        .unwrap();

        assert_eq!(
            document,
            json!({
                "id": "msg_1", "type": "message", "role": "assistant",
                "model": "claude-test", "stop_reason": "tool_use", "stop_sequence": null,
                "usage": {"input_tokens": 12, "output_tokens": 42,
                          "cache_read_input_tokens": 5},
                "content": [
                    {"type": "thinking", "thinking": "Let me look.", "signature": "sig"},
                    {"type": "text", "text": "Reading the file.",
                     "citations": [{"cited_text": "x"}]},
                    {"type": "tool_use", "id": "toolu_1", "name": "read_file",
                     "input": {"path": "a.rs"}},
                    {"type": "tool_use", "id": "toolu_2", "name": "list", "input": {}},
                ]
            })
        );
    }

    #[test]
    fn an_unmodelled_block_type_passes_through_unchanged() {
        let block = json!({"type": "redacted_thinking", "data": "opaque"});
        let document = rebuild(vec![
            message_start(),
            block_start(0, block.clone()),
            stop(0),
            message_delta("end_turn"),
            Event::MessageStop,
        ])
        .unwrap();
        assert_eq!(document["content"], json!([block]));
    }

    #[test]
    fn server_tool_blocks_take_input_deltas() {
        let document = rebuild(vec![
            message_start(),
            block_start(0, json!({"type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {}})),
            delta(0, Delta::InputJson("{\"query\":\"x\"}".into())),
            stop(0),
            message_delta("end_turn"),
            Event::MessageStop,
        ])
        .unwrap();
        assert_eq!(document["content"][0]["input"], json!({"query": "x"}));
    }

    fn cut_tool_call(stop_reason: &str) -> Result<Value, StreamError> {
        rebuild(vec![
            message_start(),
            block_start(
                0,
                json!({"type": "tool_use", "id": "toolu_1", "name": "f", "input": {}}),
            ),
            delta(0, Delta::InputJson("{\"path\": \"a".into())),
            stop(0),
            message_delta(stop_reason),
            Event::MessageStop,
        ])
    }

    /// The endpoint closes a tool block that max_tokens cut off.  The reply
    /// is then truncated, which the session already handles, so the block
    /// keeps the input of its start event rather than failing the stream.
    #[test]
    fn a_tool_input_cut_by_max_tokens_keeps_its_start_input() {
        let document = cut_tool_call("max_tokens").unwrap();
        assert_eq!(document["content"][0]["input"], json!({}));
        assert_eq!(document["stop_reason"], "max_tokens");
    }

    #[test]
    fn a_tool_input_that_does_not_parse_otherwise_is_malformed() {
        let err = cut_tool_call("tool_use").unwrap_err();
        assert!(matches!(err, StreamError::Malformed(_)), "{err}");
    }

    #[test]
    fn only_the_last_block_may_have_a_cut_input() {
        let err = run(vec![
            message_start(),
            block_start(
                0,
                json!({"type": "tool_use", "id": "a", "name": "f", "input": {}}),
            ),
            delta(0, Delta::InputJson("{".into())),
            stop(0),
            block_start(1, json!({"type": "text", "text": ""})),
        ])
        .unwrap_err();
        assert!(matches!(err, StreamError::Malformed(_)), "{err}");
    }

    #[test]
    fn parse_reads_every_documented_event() {
        let cases = [
            (
                r#"{"type":"message_start","message":{"id":"m"}}"#,
                Event::MessageStart {
                    message: object(json!({"id": "m"})),
                },
            ),
            (
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                block_start(0, json!({"type": "text", "text": ""})),
            ),
            (
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
                delta(0, Delta::Text("hi".into())),
            ),
            (
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a"}}"#,
                delta(1, Delta::InputJson("{\"a".into())),
            ),
            (
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"t"}}"#,
                delta(0, Delta::Thinking("t".into())),
            ),
            (
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"s"}}"#,
                delta(0, Delta::Signature("s".into())),
            ),
            (
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":{"x":1}}}"#,
                delta(0, Delta::Citation(json!({"x": 1}))),
            ),
            (
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"future_delta"}}"#,
                delta(0, Delta::Unknown("future_delta".into())),
            ),
            (r#"{"type":"content_block_stop","index":2}"#, stop(2)),
            (
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
                Event::MessageDelta {
                    delta: object(json!({"stop_reason": "end_turn"})),
                    usage: Some(object(json!({"output_tokens": 3}))),
                },
            ),
            (r#"{"type":"message_stop"}"#, Event::MessageStop),
            (r#"{"type":"ping"}"#, Event::Ping),
            (
                r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
                upstream(),
            ),
            (
                r#"{"type":"future_event","x":1}"#,
                Event::Unknown {
                    kind: "future_event".into(),
                },
            ),
        ];
        for (data, expected) in cases {
            assert_eq!(Event::parse(data).unwrap(), expected, "{data}");
        }
    }

    #[test]
    fn parse_rejects_malformed_events() {
        for data in [
            "not json",
            "[]",
            r#"{"no_type":1}"#,
            r#"{"type":"message_start"}"#,
            r#"{"type":"content_block_start","content_block":{}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta"}}"#,
            r#"{"type":"content_block_delta","index":0}"#,
            r#"{"type":"content_block_stop","index":-1}"#,
            r#"{"type":"message_delta"}"#,
            r#"{"type":"message_delta","delta":{},"usage":3}"#,
        ] {
            assert!(
                matches!(Event::parse(data), Err(StreamError::Malformed(_))),
                "{data}"
            );
        }
    }

    #[test]
    fn the_driver_stops_at_message_stop_and_names_its_state() {
        let mut stream = MessageStream::new();
        let feed = |stream: &mut MessageStream, data: &str| {
            stream.on_event(SseEvent {
                event: None,
                data: data.to_string(),
            })
        };
        assert_eq!(stream.state_name(), "AwaitingMessageStart");
        assert_eq!(
            feed(
                &mut stream,
                r#"{"type":"message_start","message":{"id":"m"}}"#
            ),
            Ok(Flow::Continue)
        );
        assert_eq!(
            feed(
                &mut stream,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#
            ),
            Ok(Flow::Continue)
        );
        assert_eq!(stream.state_name(), "InBlock");
        feed(&mut stream, r#"{"type":"content_block_stop","index":0}"#).unwrap();
        feed(
            &mut stream,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        )
        .unwrap();
        assert_eq!(
            feed(&mut stream, r#"{"type":"message_stop"}"#),
            Ok(Flow::Done)
        );
        let document = stream.into_document().unwrap();
        assert_eq!(document["content"], json!([{"type": "text", "text": ""}]));
    }

    #[test]
    fn the_driver_refuses_events_after_a_failure() {
        let mut stream = MessageStream::new();
        let event = |data: &str| SseEvent {
            event: None,
            data: data.to_string(),
        };
        assert!(
            stream
                .on_event(event(r#"{"type":"message_stop"}"#))
                .is_err()
        );
        assert_eq!(stream.state_name(), "Failed");
        assert!(stream.on_event(event(r#"{"type":"ping"}"#)).is_err());
        assert!(stream.into_document().is_none());
    }
}
