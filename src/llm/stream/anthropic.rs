//! Anthropic Messages stream assembly: `data:` payloads in, one
//! [`LlmReply`] out, equal to what `anthropic::parse_response` returns for
//! the equivalent non-streamed message.
//!
//! The assembler does not re-implement `parse_response`. It rebuilds the
//! `MessagesResponse` the gateway would have sent in one body (content
//! blocks in wire order, the final `stop_reason`, the merged `usage`) and
//! hands it to `parse_response`, so the two paths share one definition of
//! the `finish_reason` mapping, the thinking join, the `refusal` error,
//! the usage arithmetic and the replay blocks, and cannot drift apart.
//!
//! Dispatch is on the JSON `type` of each payload, not on the SSE `event:`
//! name: Anthropic sends both and they always agree, so `sse` keeps
//! ignoring `event:` lines.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use super::{Flow, LlmEvent};
use crate::llm::anthropic::{parse_response, MessagesResponse};
use crate::llm::client::{error_object, GatewayError};
use crate::llm::reply::LlmReply;

#[derive(Debug, Deserialize)]
struct StreamEvent {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    /// `message_start`: the message shell, with the input-side `usage`.
    #[serde(default)]
    message: Option<Value>,
    /// `content_block_start` and `content_block_delta`: the wire block index.
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    content_block: Option<Value>,
    /// `content_block_delta`: the fragment; `message_delta`: `stop_reason`.
    #[serde(default)]
    delta: Option<Value>,
    /// `message_delta`: cumulative usage.
    #[serde(default)]
    usage: Option<Value>,
    /// `error` event.
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Text,
    Thinking,
    Redacted,
    /// `call` is the tool call's position among the reply's calls, the
    /// `index` of its `LlmEvent::ToolArgs` (not the wire block index).
    ToolUse {
        call: usize,
    },
}

#[derive(Debug)]
struct Slot {
    kind: Kind,
    /// The block as `content_block_start` announced it; deltas are folded
    /// into it so any field the gateway sent survives into the replay.
    block: Value,
    /// Raw `partial_json` of a `tool_use` block.
    input: String,
}

/// Accumulates one attempt's events. Built fresh for every attempt: a retry
/// restarts the whole reply.
#[derive(Debug, Default)]
pub(super) struct Assembler {
    /// Kept blocks in wire order. Block types the single-body parser
    /// ignores (server tools, unknown) are never registered, so their
    /// deltas find no slot and are dropped.
    slots: Vec<Slot>,
    by_index: BTreeMap<usize, usize>,
    calls: usize,
    stop_reason: Option<String>,
    usage: Map<String, Value>,
    stopped: bool,
}

/// Append `fragment` to the string field `key` of a block.
fn append(block: &mut Value, key: &str, fragment: &str) {
    let Some(object) = block.as_object_mut() else {
        return;
    };
    match object
        .entry(key)
        .or_insert_with(|| Value::String(String::new()))
    {
        Value::String(text) => text.push_str(fragment),
        other => *other = Value::String(fragment.to_owned()),
    }
}

fn string_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

impl Assembler {
    pub(super) fn feed(
        &mut self,
        payload: &str,
        on_event: &mut (dyn FnMut(LlmEvent) + Send),
    ) -> Result<Flow, GatewayError> {
        let payload = payload.trim();
        if payload.is_empty() {
            return Ok(Flow::More);
        }
        let event: StreamEvent =
            serde_json::from_str(payload).map_err(|err| GatewayError::Parse(err.to_string()))?;
        match event.kind.as_deref() {
            Some("message_start") => {
                if let Some(message) = &event.message {
                    self.merge_usage(message.get("usage"));
                }
            }
            Some("content_block_start") => {
                if let (Some(index), Some(block)) = (event.index, event.content_block) {
                    self.start_block(index, block, on_event);
                }
            }
            Some("content_block_delta") => {
                if let (Some(index), Some(delta)) = (event.index, &event.delta) {
                    self.apply_delta(index, delta, on_event);
                }
            }
            Some("message_delta") => {
                let reason = event
                    .delta
                    .as_ref()
                    .and_then(|delta| string_of(delta, "stop_reason"))
                    .filter(|reason| !reason.is_empty());
                if let Some(reason) = reason {
                    self.stop_reason = Some(reason.to_owned());
                }
                self.merge_usage(event.usage.as_ref());
            }
            Some("message_stop") => {
                self.stopped = true;
                return Ok(Flow::Done);
            }
            // Same classification as an `error` object in a 2xx body.
            Some("error") => {
                return Err(error_object(&event.error.unwrap_or(Value::Null)));
            }
            // `ping`, `content_block_stop` and unknown event types carry
            // nothing the reply needs.
            _ => {}
        }
        Ok(Flow::More)
    }

    fn start_block(
        &mut self,
        index: usize,
        block: Value,
        on_event: &mut (dyn FnMut(LlmEvent) + Send),
    ) {
        let kind = match string_of(&block, "type") {
            Some("text") => Kind::Text,
            Some("thinking") => Kind::Thinking,
            Some("redacted_thinking") => Kind::Redacted,
            Some("tool_use") => {
                let call = self.calls;
                self.calls += 1;
                on_event(LlmEvent::ToolArgs {
                    index: call,
                    id: string_of(&block, "id").map(str::to_owned),
                    name: string_of(&block, "name").map(str::to_owned),
                    fragment: String::new(),
                });
                Kind::ToolUse { call }
            }
            _ => return,
        };
        self.by_index.insert(index, self.slots.len());
        self.slots.push(Slot {
            kind,
            block,
            input: String::new(),
        });
    }

    fn apply_delta(
        &mut self,
        index: usize,
        delta: &Value,
        on_event: &mut (dyn FnMut(LlmEvent) + Send),
    ) {
        let Some(&position) = self.by_index.get(&index) else {
            return;
        };
        let slot = &mut self.slots[position];
        match (string_of(delta, "type"), &slot.kind) {
            (Some("text_delta"), Kind::Text) => {
                if let Some(text) = string_of(delta, "text").filter(|text| !text.is_empty()) {
                    append(&mut slot.block, "text", text);
                    on_event(LlmEvent::Text(text.to_owned()));
                }
            }
            (Some("thinking_delta"), Kind::Thinking) => {
                if let Some(text) = string_of(delta, "thinking").filter(|text| !text.is_empty()) {
                    append(&mut slot.block, "thinking", text);
                    on_event(LlmEvent::Thinking(text.to_owned()));
                }
            }
            // The signature belongs to the replay block only: no event.
            (Some("signature_delta"), Kind::Thinking) => {
                if let Some(signature) = string_of(delta, "signature") {
                    append(&mut slot.block, "signature", signature);
                }
            }
            (Some("input_json_delta"), &Kind::ToolUse { call }) => {
                // Anthropic opens every tool block with an empty fragment.
                if let Some(fragment) =
                    string_of(delta, "partial_json").filter(|fragment| !fragment.is_empty())
                {
                    slot.input.push_str(fragment);
                    on_event(LlmEvent::ToolArgs {
                        index: call,
                        id: string_of(&slot.block, "id").map(str::to_owned),
                        name: string_of(&slot.block, "name").map(str::to_owned),
                        fragment: fragment.to_owned(),
                    });
                }
            }
            _ => {}
        }
    }

    /// Fold `message_start` / `message_delta` usage into one object. The
    /// counters are cumulative, so a number only ever grows: a gateway that
    /// repeats `input_tokens: 0` in `message_delta` cannot erase the count
    /// `message_start` reported. `null` never overwrites.
    fn merge_usage(&mut self, usage: Option<&Value>) {
        let Some(Value::Object(fields)) = usage else {
            return;
        };
        for (key, value) in fields {
            let keep_current = match (self.usage.get(key), value) {
                (_, Value::Null) => true,
                (Some(Value::Number(current)), Value::Number(new)) => {
                    current.as_u64().unwrap_or(0) >= new.as_u64().unwrap_or(0)
                }
                _ => false,
            };
            if !keep_current {
                self.usage.insert(key.clone(), value.clone());
            }
        }
    }

    /// The assembled reply, once the body is over. A body that ended with
    /// neither `message_stop` nor a `stop_reason` was cut: a retryable
    /// transport failure, never a half reply.
    pub(super) fn finish(self) -> Result<LlmReply, GatewayError> {
        if self.stop_reason.is_none() && !self.stopped {
            return Err(GatewayError::Transport(
                "stream ended before message_stop or a stop_reason (cut mid-body)".to_owned(),
            ));
        }
        let mut content = Vec::with_capacity(self.slots.len());
        // Tool inputs that are not valid JSON (a reply cut by `max_tokens`
        // mid-argument): the single-body path cannot see such a thing, so
        // keep the raw text, like the OpenAI path does, and let tool
        // dispatch reject it.
        let mut raw_inputs: Vec<(usize, String)> = Vec::new();
        for slot in self.slots {
            let mut block = slot.block;
            if let Kind::ToolUse { call } = slot.kind {
                let input = if slot.input.trim().is_empty() {
                    Value::Object(Map::new())
                } else if let Ok(parsed) = serde_json::from_str(&slot.input) {
                    parsed
                } else {
                    raw_inputs.push((call, slot.input));
                    Value::Object(Map::new())
                };
                if let Some(object) = block.as_object_mut() {
                    object.insert("input".to_owned(), input);
                }
            }
            content.push(block);
        }
        let message = serde_json::json!({
            "type": "message",
            "content": content,
            "stop_reason": self.stop_reason,
            "usage": self.usage,
        });
        let message: MessagesResponse =
            serde_json::from_value(message).map_err(|err| GatewayError::Parse(err.to_string()))?;
        let mut reply = parse_response(message)?;
        for (call, raw) in raw_inputs {
            if let Some(tool_call) = reply.tool_calls.get_mut(call) {
                tool_call.arguments = raw;
            }
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(payloads: &[&str]) -> (Result<LlmReply, GatewayError>, Vec<LlmEvent>) {
        let mut assembler = Assembler::default();
        let mut events = Vec::new();
        let mut sink = |event| events.push(event);
        for payload in payloads {
            match assembler.feed(payload, &mut sink) {
                Ok(Flow::More) => {}
                Ok(Flow::Done) => break,
                Err(err) => return (Err(err), events),
            }
        }
        (assembler.finish(), events)
    }

    fn args_event(index: usize, id: &str, name: &str, fragment: &str) -> LlmEvent {
        LlmEvent::ToolArgs {
            index,
            id: Some(id.to_owned()),
            name: Some(name.to_owned()),
            fragment: fragment.to_owned(),
        }
    }

    #[test]
    fn tool_call_index_is_the_call_position_not_the_block_index() {
        let (reply, events) = run(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t2","name":"fetch","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let reply = reply.expect("assembles");
        assert_eq!(reply.tool_calls.len(), 2);
        assert_eq!(reply.tool_calls[0].arguments, r#"{"q":"a"}"#);
        assert_eq!(reply.tool_calls[1].arguments, "{}");
        assert_eq!(
            events,
            vec![
                LlmEvent::Text("x".to_owned()),
                args_event(0, "t1", "search", ""),
                args_event(0, "t1", "search", "{\"q\":"),
                args_event(1, "t2", "fetch", ""),
                args_event(1, "t2", "fetch", "{}"),
                args_event(0, "t1", "search", "\"a\"}"),
            ]
        );
    }

    #[test]
    fn tool_use_without_any_input_delta_gets_an_empty_object() {
        let (reply, _) = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"get_time","input":{}}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}"#,
        ]);
        assert_eq!(reply.expect("assembles").tool_calls[0].arguments, "{}");
    }

    #[test]
    fn truncated_tool_json_keeps_the_raw_text_instead_of_failing_the_reply() {
        let (reply, _) = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\"par"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":1}}"#,
        ]);
        let reply = reply.expect("a length cut is still a reply");
        assert_eq!(reply.finish_reason.as_deref(), Some("length"));
        assert_eq!(reply.tool_calls[0].arguments, r#"{"q":"par"#);
    }

    #[test]
    fn unknown_blocks_and_deltas_are_ignored() {
        let (reply, events) = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"s","name":"web_search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":1}"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"citations_delta","citation":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ok"}}"#,
            r#"{"type":"future_event","anything":true}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
        ]);
        let reply = reply.expect("assembles");
        assert_eq!(reply.output, "ok");
        assert!(reply.tool_calls.is_empty());
        assert_eq!(events, vec![LlmEvent::Text("ok".to_owned())]);
    }

    #[test]
    fn usage_counters_only_grow_and_null_never_overwrites() {
        let (reply, _) = run(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_creation_input_tokens":5,"cache_read_input_tokens":100,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":0,"cache_read_input_tokens":null,"output_tokens":30}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let usage = reply.expect("assembles").usage;
        assert_eq!(usage.prompt_tokens, 115);
        assert_eq!(usage.completion_tokens, 30);
        assert_eq!(usage.cached_prompt_tokens, 100);
        assert_eq!(usage.cache_creation_prompt_tokens, 5);
    }

    #[test]
    fn error_event_keeps_the_error_body_classification() {
        let (retryable, events) = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"par"}}"#,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ]);
        assert!(
            matches!(
                retryable,
                Err(GatewayError::ErrorBody { retryable: true, ref kind, .. }) if kind == "overloaded_error"
            ),
            "{retryable:?}"
        );
        assert_eq!(events, vec![LlmEvent::Text("par".to_owned())]);
        let (rejected, _) =
            run(&[r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#]);
        assert!(matches!(
            rejected,
            Err(GatewayError::ErrorBody {
                retryable: false,
                ..
            })
        ));
    }

    #[test]
    fn refusal_stop_reason_is_the_same_error_as_the_json_path() {
        let (reply, _) = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"I cannot help"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{"output_tokens":3}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert!(matches!(
            reply,
            Err(GatewayError::Refused(text)) if text == "I cannot help"
        ));
    }

    #[test]
    fn body_without_message_stop_or_stop_reason_is_a_retryable_cut() {
        let (reply, _) = run(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"par"}}"#,
            r#"{"type":"ping"}"#,
        ]);
        let err = reply.expect_err("a cut stream is not a reply");
        assert!(matches!(err, GatewayError::Transport(_)), "{err}");
        assert!(err.is_retryable());
    }

    #[test]
    fn stop_reason_without_message_stop_is_complete() {
        let (reply, _) = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
        ]);
        assert_eq!(reply.expect("complete").output, "ok");
    }

    #[test]
    fn malformed_payload_is_a_parse_error() {
        let (reply, _) = run(&["{not json"]);
        assert!(matches!(reply, Err(GatewayError::Parse(_))));
    }

    /// Streaming a message must give the reply `parse_response` gives the
    /// same message in one body: text, joined thinking, replay blocks with
    /// `signature` and `data`, tool calls, `finish_reason`, usage.
    #[test]
    fn assembled_reply_equals_parse_response_of_the_equivalent_message() {
        let (streamed, _) = run(&[
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":12,"cache_creation_input_tokens":7,"cache_read_input_tokens":4096,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me "}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"think."}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQB"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"Cg=="}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"RED=="}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Looking "}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"it up."}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_1","name":"search","input":{}}}"#,
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"queries\": [\"a\","}}"#,
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":" \"b\"]}"}}"#,
            r#"{"type":"content_block_stop","index":3}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":55,"output_tokens_details":{"thinking_tokens":20}}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let single: MessagesResponse = serde_json::from_value(serde_json::json!({
            "type": "message",
            "content": [
                {"type": "thinking", "thinking": "Let me think.", "signature": "EqQBCg=="},
                {"type": "redacted_thinking", "data": "RED=="},
                {"type": "text", "text": "Looking it up."},
                {"type": "tool_use", "id": "toolu_1", "name": "search",
                 "input": {"queries": ["a", "b"]}}
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 12,
                "cache_creation_input_tokens": 7,
                "cache_read_input_tokens": 4096,
                "output_tokens": 55,
                "output_tokens_details": {"thinking_tokens": 20}
            }
        }))
        .expect("fixture is a MessagesResponse");
        let single = parse_response(single).expect("single body parses");
        let streamed = streamed.expect("stream assembles");
        assert_eq!(streamed.output, single.output);
        assert_eq!(streamed.thinking, single.thinking);
        assert_eq!(streamed.finish_reason, single.finish_reason);
        assert_eq!(streamed.tool_calls, single.tool_calls);
        assert_eq!(streamed.replay, single.replay);
        assert_eq!(
            format!("{:?}", streamed.usage),
            format!("{:?}", single.usage)
        );
        assert_eq!(streamed.usage.cached_prompt_tokens, 4096);
        assert_eq!(streamed.usage.cache_creation_prompt_tokens, 7);
        assert_eq!(streamed.usage.reasoning_tokens, 20);
    }
}
