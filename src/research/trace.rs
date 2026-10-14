//! `--trace` (#151, ADR-0010 §5): the `trace` rows of one Session.
//!
//! [`TraceState`] turns what the loop sent and received into
//! [`TraceRow`]s; `Events` owns it behind the same lock as the Session log
//! and appends the rows, so a trace is never an Event and never reaches
//! `--events`. `llm/` knows nothing about this: the loop hands over the
//! messages it already built.
//!
//! The system prompt and tool definitions are large and constant, so the
//! first `model` row carries them and later rows only the messages appended
//! since the previous request (`messages_delta`). Context truncation drops
//! old Turns from the middle of the transcript, so the new request is no
//! longer an extension of the old one; then the row carries the whole
//! request as `messages` with `full: true`. Rebuilding is therefore
//! mechanical: a `full` row replaces the transcript, any other appends.
//!
//! Secrets cannot leak by construction: a row is built only from the
//! messages, the tool definitions, the reply and the error text. The
//! `GatewayConfig` (API key) is never passed here.

use serde_json::{json, Value};

use crate::llm::{ChatMessage, GatewayError, LlmReply, ToolChoice, ToolDef};
use crate::research::agent_loop::context::ToolMessage;
use crate::research::dto::UsageDTO;
use crate::research::session::TraceRow;

/// What the previous `model` row left the reader with.
#[derive(Default)]
pub(crate) struct TraceState {
    /// The previous request without its system message, as written to the
    /// file. `None` before the first row, which is when `system` and `tools`
    /// are written.
    sent: Option<Vec<Value>>,
}

impl TraceState {
    /// The `model` row of `turn`: the request just sent and the gateway's
    /// outcome. `messages[0]` is the system prompt.
    pub(crate) fn model_row(
        &mut self,
        session_id: &str,
        turn: u32,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        choice: Option<&ToolChoice>,
        outcome: Result<&LlmReply, &GatewayError>,
    ) -> TraceRow {
        let mut row = TraceRow::new(session_id, turn, "model");
        let (system, rest) = match messages.split_first() {
            Some((system, rest)) if system.role == "system" => (Some(system), rest),
            _ => (None, messages),
        };
        let current: Vec<Value> = rest.iter().map(message_json).collect();
        match self.sent.take() {
            None => {
                row.system = system.map(|message| message.content_text().to_owned());
                row.tools = (!tools.is_empty())
                    .then(|| serde_json::to_value(tools).expect("tool defs serialize"));
                row.messages_delta = Some(current.clone());
            }
            Some(previous) if current.starts_with(&previous) => {
                row.messages_delta = Some(current[previous.len()..].to_vec());
            }
            Some(_) => {
                row.messages = Some(current.clone());
                row.full = true;
            }
        }
        self.sent = Some(current);
        row.tool_choice = choice.map(|choice| serde_json::to_value(choice).expect("choice"));
        row.reply = Some(match outcome {
            Ok(reply) => reply_json(reply),
            Err(err) => json!({ "error": err.to_string() }),
        });
        row
    }
}

/// The `tool_results` row of `turn`: each call id with the exact content the
/// model receives next, framed containers and turns-left note included.
pub(crate) fn tool_results_row(session_id: &str, turn: u32, results: &[ToolMessage]) -> TraceRow {
    let mut row = TraceRow::new(session_id, turn, "tool_results");
    row.results = Some(
        results
            .iter()
            .map(|result| json!({ "id": result.id, "content": result.content }))
            .collect(),
    );
    row
}

/// A request message as the transcript the model saw. `ChatMessage`
/// serializes to the wire shape, which leaves the provider replay blocks
/// out; they belong to the transcript, so they are added back here.
fn message_json(message: &ChatMessage) -> Value {
    let mut value = serde_json::to_value(message).expect("a message serializes");
    if !message.replay.blocks.is_empty() {
        value["replay"] = Value::Array(message.replay.blocks.clone());
    }
    value
}

fn reply_json(reply: &LlmReply) -> Value {
    let tool_calls: Vec<Value> = reply
        .tool_calls
        .iter()
        .map(|call| json!({ "id": call.id, "name": call.name, "arguments": call.arguments }))
        .collect();
    let mut value = json!({
        "output": reply.output,
        "thinking": reply.thinking,
        "tool_calls": tool_calls,
        "finish_reason": reply.finish_reason,
        "usage": UsageDTO::from(reply.usage.clone()),
    });
    if !reply.replay.blocks.is_empty() {
        value["replay"] = Value::Array(reply.replay.blocks.clone());
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ReplayBlocks, RequestedToolCall};

    fn call(id: &str) -> RequestedToolCall {
        RequestedToolCall {
            id: id.to_owned(),
            name: "search".to_owned(),
            arguments: r#"{"queries":["x"]}"#.to_owned(),
        }
    }

    fn row(state: &mut TraceState, turn: u32, messages: &[ChatMessage]) -> TraceRow {
        let reply = LlmReply::default();
        state.model_row("s", turn, messages, &[], None, Ok(&reply))
    }

    /// Context truncation rewrites the middle of the transcript: the row
    /// must say so instead of appending a delta that rebuilds the wrong list.
    #[test]
    fn a_truncated_request_is_written_whole_and_marked_full() {
        let mut state = TraceState::default();
        let turn_one = vec![ChatMessage::system("sys"), ChatMessage::user("goal")];
        let mut turn_two = turn_one.clone();
        turn_two.push(ChatMessage::assistant_tool_calls(
            "",
            &[call("a")],
            &ReplayBlocks::default(),
        ));
        turn_two.push(ChatMessage::tool("a", "page one"));
        // Turn 3 lost the oldest tool turn and gained a note and a new one.
        let turn_three = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("goal"),
            ChatMessage::user("[history truncated: 1 oldest turn(s) dropped]"),
            ChatMessage::assistant_tool_calls("", &[call("b")], &ReplayBlocks::default()),
            ChatMessage::tool("b", "page two"),
        ];

        let first = row(&mut state, 1, &turn_one);
        assert_eq!(first.system.as_deref(), Some("sys"));
        assert_eq!(first.messages_delta.as_ref().map(Vec::len), Some(1));

        let second = row(&mut state, 2, &turn_two);
        assert!(!second.full && second.system.is_none());
        assert_eq!(second.messages_delta.as_ref().map(Vec::len), Some(2));

        let third = row(&mut state, 3, &turn_three);
        assert!(third.full);
        assert_eq!(third.messages_delta, None);
        let messages = third.messages.expect("full row carries messages");
        assert_eq!(messages.len(), 4, "everything but the system prompt");
        assert_eq!(
            messages[1]["content"],
            "[history truncated: 1 oldest turn(s) dropped]"
        );
        assert_eq!(messages[3]["tool_call_id"], "b");
    }

    /// Anthropic replay blocks are not on the wire shape of `ChatMessage`
    /// but belong to the transcript the model saw.
    #[test]
    fn replay_blocks_stay_in_the_transcript_and_the_reply() {
        let blocks = vec![json!({"type": "thinking", "thinking": "hm", "signature": "sig"})];
        let replay = ReplayBlocks {
            blocks: blocks.clone(),
            chars: 4,
        };
        let message = ChatMessage::assistant_tool_calls("", &[call("a")], &replay);
        assert_eq!(message_json(&message)["replay"], json!(blocks));

        let reply = LlmReply {
            replay,
            ..LlmReply::default()
        };
        assert_eq!(reply_json(&reply)["replay"], json!(blocks));
        assert!(reply_json(&LlmReply::default()).get("replay").is_none());
    }
}
