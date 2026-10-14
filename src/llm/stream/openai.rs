//! OpenAI Chat Completions chunk assembly: `data:` payloads in, one
//! [`LlmReply`] out, with the same content and the same error classes the
//! single-body parser (`response::parse_reply`) gives for the equivalent
//! non-streamed body.
//!
//! Only choice 0 is read, like the single-body path. Reasoning arrives
//! under the field names `response.rs` already accepts (`reasoning_content`,
//! `reasoning`, `reasoning_details[].text`); the three are kept apart and
//! joined with `\n` at the end, exactly as `parse_reply` joins them.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::LlmEvent;
use crate::llm::client::{error_object, GatewayError};
use crate::llm::reply::{LlmReply, RequestedToolCall};
use crate::llm::request::ReplayBlocks;
use crate::llm::response::{
    null_to_default, token_usage, ReasoningDetail, ResponseUsage, ToolFunction,
};

#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default, deserialize_with = "null_to_default")]
    choices: Vec<ChunkChoice>,
    /// Present on the last chunk when `stream_options.include_usage` is set
    /// (with empty `choices`); some gateways repeat it on every chunk.
    #[serde(default)]
    usage: Option<ResponseUsage>,
    /// An error object mid-stream; wins over everything else in the chunk.
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default, deserialize_with = "null_to_default")]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    refusal: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default, deserialize_with = "null_to_default")]
    reasoning_details: Vec<ReasoningDetail>,
    #[serde(default, deserialize_with = "null_to_default")]
    tool_calls: Vec<DeltaToolCall>,
}

#[derive(Debug, Default, Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ToolFunction>,
}

#[derive(Debug, Default)]
struct PartialCall {
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
}

/// Whether the stream is over after a payload.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Flow {
    More,
    Done,
}

/// Accumulates one attempt's chunks. Built fresh for every attempt: a retry
/// restarts the whole reply.
#[derive(Debug, Default)]
pub(super) struct Assembler {
    output: String,
    refusal: String,
    reasoning_content: String,
    reasoning: String,
    reasoning_details: String,
    /// Tool calls by wire `index`, so interleaved fragments land in the
    /// right call and the reply keeps wire order.
    calls: BTreeMap<usize, PartialCall>,
    last_call: Option<usize>,
    finish_reason: Option<String>,
    usage: Option<ResponseUsage>,
    saw_choice: bool,
    done: bool,
}

fn non_empty(text: Option<String>) -> Option<String> {
    text.filter(|text| !text.is_empty())
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
        if payload == "[DONE]" {
            self.done = true;
            return Ok(Flow::Done);
        }
        let chunk: Chunk =
            serde_json::from_str(payload).map_err(|err| GatewayError::Parse(err.to_string()))?;
        if let Some(error) = chunk.error.as_ref().filter(|error| !error.is_null()) {
            return Err(error_object(error));
        }
        if chunk.usage.is_some() {
            self.usage = chunk.usage;
        }
        for choice in chunk.choices {
            if choice.index.unwrap_or(0) == 0 {
                self.saw_choice = true;
                self.apply(choice, on_event);
            }
        }
        Ok(Flow::More)
    }

    fn apply(&mut self, choice: ChunkChoice, on_event: &mut (dyn FnMut(LlmEvent) + Send)) {
        let delta = choice.delta;
        if let Some(text) = non_empty(delta.content) {
            self.output.push_str(&text);
            on_event(LlmEvent::Text(text));
        }
        if let Some(text) = non_empty(delta.refusal) {
            self.refusal.push_str(&text);
        }
        if let Some(text) = non_empty(delta.reasoning_content) {
            self.reasoning_content.push_str(&text);
            on_event(LlmEvent::Thinking(text));
        }
        if let Some(text) = non_empty(delta.reasoning) {
            self.reasoning.push_str(&text);
            on_event(LlmEvent::Thinking(text));
        }
        for detail in delta.reasoning_details {
            if let Some(text) = non_empty(detail.text) {
                self.reasoning_details.push_str(&text);
                on_event(LlmEvent::Thinking(text));
            }
        }
        for call in delta.tool_calls {
            self.apply_call(call, on_event);
        }
        if let Some(reason) = non_empty(choice.finish_reason) {
            self.finish_reason = Some(reason);
        }
    }

    fn apply_call(&mut self, call: DeltaToolCall, on_event: &mut (dyn FnMut(LlmEvent) + Send)) {
        let id = non_empty(call.id);
        let (name, arguments) = match call.function {
            Some(function) => (non_empty(function.name), function.arguments),
            None => (None, None),
        };
        // A gateway that omits `index` continues the last call unless the
        // fragment opens a new one with an `id`.
        let index = call.index.unwrap_or(match (&id, self.last_call) {
            (None, Some(last)) => last,
            _ => self.calls.len(),
        });
        self.last_call = Some(index);
        let entry = self.calls.entry(index).or_default();
        // `id` and `name` belong to the first fragment; later repeats of
        // them must not be glued on.
        let mut announced = false;
        if entry.id.is_none() {
            if let Some(id) = id {
                entry.id = Some(id);
                announced = true;
            }
        }
        if entry.name.is_none() {
            if let Some(name) = name {
                entry.name = Some(name);
                announced = true;
            }
        }
        let fragment = arguments.unwrap_or_default();
        // The buffer exists only once text arrives: a call announced with
        // empty (or no) arguments ends as `{}`, like a body with no
        // `arguments` field.
        if !fragment.is_empty() {
            entry
                .arguments
                .get_or_insert_with(String::new)
                .push_str(&fragment);
        }
        if announced || !fragment.is_empty() {
            on_event(LlmEvent::ToolArgs {
                index,
                id: entry.id.clone(),
                name: entry.name.clone(),
                fragment,
            });
        }
    }

    /// The assembled reply, once the body is over. A body that ended with
    /// neither `[DONE]` nor a `finish_reason` was cut: a retryable
    /// transport failure, never a half reply.
    pub(super) fn finish(self) -> Result<LlmReply, GatewayError> {
        if self.finish_reason.is_none() && !self.done {
            return Err(GatewayError::Transport(
                "stream ended before a finish_reason or [DONE] (cut mid-body)".to_owned(),
            ));
        }
        if !self.saw_choice {
            return Err(GatewayError::EmptyChoices);
        }
        if !self.refusal.is_empty() {
            return Err(GatewayError::Refused(self.refusal));
        }
        let tool_calls = self
            .calls
            .into_values()
            .map(|call| RequestedToolCall {
                id: call.id.unwrap_or_default(),
                name: call.name.unwrap_or_default(),
                arguments: call.arguments.unwrap_or_else(|| "{}".to_owned()),
            })
            .collect();
        let thinking = [
            self.reasoning_content,
            self.reasoning,
            self.reasoning_details,
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
        Ok(LlmReply {
            thinking,
            output: self.output,
            finish_reason: self.finish_reason,
            tool_calls,
            usage: self.usage.map(token_usage).unwrap_or_default(),
            replay: ReplayBlocks::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::response::{parse_reply, ChatResponse};

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

    fn args_event(index: usize, id: Option<&str>, name: Option<&str>, fragment: &str) -> LlmEvent {
        LlmEvent::ToolArgs {
            index,
            id: id.map(str::to_owned),
            name: name.map(str::to_owned),
            fragment: fragment.to_owned(),
        }
    }

    #[test]
    fn text_fragments_become_ordered_events_and_one_output() {
        let (reply, events) = run(&[
            r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);
        let reply = reply.expect("text stream assembles");
        assert_eq!(reply.output, "Hello");
        assert_eq!(reply.finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            events,
            vec![
                LlmEvent::Text("Hel".to_owned()),
                LlmEvent::Text("lo".to_owned())
            ]
        );
    }

    #[test]
    fn tool_calls_assemble_by_index_across_interleaved_fragments() {
        let (reply, events) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"search","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"fetch","arguments":"{\"url\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"queries\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"\"https://x\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"[\"q\"]}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]);
        let reply = reply.expect("tool call stream assembles");
        assert_eq!(reply.finish_reason.as_deref(), Some("tool_calls"));
        let calls: Vec<_> = reply
            .tool_calls
            .iter()
            .map(|c| (c.id.as_str(), c.name.as_str(), c.arguments.as_str()))
            .collect();
        assert_eq!(
            calls,
            vec![
                ("call_a", "search", r#"{"queries":["q"]}"#),
                ("call_b", "fetch", r#"{"url":"https://x"}"#),
            ]
        );
        assert_eq!(
            events,
            vec![
                args_event(0, Some("call_a"), Some("search"), ""),
                args_event(1, Some("call_b"), Some("fetch"), "{\"url\":"),
                args_event(0, Some("call_a"), Some("search"), "{\"queries\":"),
                args_event(1, Some("call_b"), Some("fetch"), "\"https://x\"}"),
                args_event(0, Some("call_a"), Some("search"), "[\"q\"]}"),
            ]
        );
    }

    #[test]
    fn repeated_id_and_name_are_not_glued_onto_the_call() {
        let (reply, _) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"get_time","arguments":"{"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"get_time","arguments":"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ]);
        let reply = reply.expect("finish_reason alone ends the stream");
        assert_eq!(reply.tool_calls[0].id, "c");
        assert_eq!(reply.tool_calls[0].name, "get_time");
        assert_eq!(reply.tool_calls[0].arguments, "{}");
    }

    #[test]
    fn call_without_arguments_defaults_to_empty_object_like_the_json_path() {
        let (reply, _) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"get_time"}}]},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_eq!(reply.expect("assembles").tool_calls[0].arguments, "{}");
    }

    #[test]
    fn reasoning_fields_stream_as_thinking_and_join_like_parse_reply() {
        let (reply, events) = run(&[
            r#"{"choices":[{"delta":{"reasoning_content":"a-1 "}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"a-2","reasoning":"b"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":"d1"},{"text":""}]}}]}"#,
            r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
        ]);
        let reply = reply.expect("assembles");
        assert_eq!(reply.thinking, "a-1 a-2\nb\nd1");
        assert_eq!(
            events,
            vec![
                LlmEvent::Thinking("a-1 ".to_owned()),
                LlmEvent::Thinking("a-2".to_owned()),
                LlmEvent::Thinking("b".to_owned()),
                LlmEvent::Thinking("d1".to_owned()),
                LlmEvent::Text("ok".to_owned()),
            ]
        );
    }

    #[test]
    fn final_usage_chunk_carries_cached_and_reasoning_details() {
        let (reply, _) = run(&[
            r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"completion_tokens_details":{"reasoning_tokens":7},"prompt_tokens_details":{"cached_tokens":64}}}"#,
            "[DONE]",
        ]);
        let usage = reply.expect("assembles").usage;
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 120);
        assert_eq!(usage.reasoning_tokens, 7);
        assert_eq!(usage.cached_prompt_tokens, 64);
    }

    #[test]
    fn refusal_is_the_same_error_as_the_json_path() {
        let (reply, _) = run(&[
            r#"{"choices":[{"delta":{"refusal":"I cannot "}}]}"#,
            r#"{"choices":[{"delta":{"refusal":"help"},"finish_reason":"stop"}]}"#,
        ]);
        assert!(matches!(
            reply,
            Err(GatewayError::Refused(text)) if text == "I cannot help"
        ));
    }

    #[test]
    fn error_object_mid_stream_keeps_the_error_body_classification() {
        let (retryable, events) = run(&[
            r#"{"choices":[{"delta":{"content":"par"}}]}"#,
            r#"{"error":{"type":"overloaded_error","message":"busy"}}"#,
        ]);
        let err = retryable.expect_err("error object ends the attempt");
        assert!(err.is_retryable(), "{err}");
        assert!(
            matches!(err, GatewayError::ErrorBody { ref kind, .. } if kind == "overloaded_error")
        );
        assert_eq!(events, vec![LlmEvent::Text("par".to_owned())]);

        let (fatal, _) = run(&[r#"{"error":{"code":"invalid_request","message":"bad"}}"#]);
        let err = fatal.expect_err("error object ends the attempt");
        assert!(!err.is_retryable(), "{err}");
    }

    #[test]
    fn body_without_done_or_finish_reason_is_a_retryable_cut() {
        let (reply, _) = run(&[r#"{"choices":[{"delta":{"content":"par"}}]}"#]);
        let err = reply.expect_err("a cut stream is not a reply");
        assert!(matches!(err, GatewayError::Transport(_)), "{err}");
        assert!(err.is_retryable());
    }

    #[test]
    fn done_without_any_choice_is_empty_choices() {
        let (reply, _) = run(&["[DONE]"]);
        assert!(matches!(reply, Err(GatewayError::EmptyChoices)));
    }

    #[test]
    fn malformed_payload_is_a_parse_error() {
        let (reply, _) = run(&["{not json"]);
        assert!(matches!(reply, Err(GatewayError::Parse(_))));
    }

    #[test]
    fn assembled_reply_equals_parse_reply_of_the_equivalent_body() {
        let (streamed, _) = run(&[
            r#"{"choices":[{"delta":{"reasoning_content":"think ","content":"Let me "}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"hard","content":"look","tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"search","arguments":"{\"q\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":4,"total_tokens":13,"completion_tokens_details":{"reasoning_tokens":2},"prompt_tokens_details":{"cached_tokens":5}}}"#,
            "[DONE]",
        ]);
        let body: ChatResponse = serde_json::from_str(
            r#"{"choices":[{"message":{"role":"assistant","content":"Let me look","reasoning_content":"think hard","tool_calls":[{"id":"c1","type":"function","function":{"name":"search","arguments":"{\"q\":1}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":9,"completion_tokens":4,"total_tokens":13,"completion_tokens_details":{"reasoning_tokens":2},"prompt_tokens_details":{"cached_tokens":5}}}"#,
        )
        .expect("fixture parses");
        let single = parse_reply(body).expect("single body parses");
        let streamed = streamed.expect("stream assembles");
        assert_eq!(streamed.output, single.output);
        assert_eq!(streamed.thinking, single.thinking);
        assert_eq!(streamed.finish_reason, single.finish_reason);
        assert_eq!(streamed.tool_calls, single.tool_calls);
        assert_eq!(
            format!("{:?}", streamed.usage),
            format!("{:?}", single.usage)
        );
        assert_eq!(
            format!("{:?}", streamed.replay),
            format!("{:?}", single.replay)
        );
    }
}
