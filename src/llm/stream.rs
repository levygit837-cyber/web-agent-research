//! Token streaming (ADR-0010 §6): the [`LlmEvent`] seam and the incremental
//! reader that turns an SSE body into the same [`LlmReply`] the single-body
//! path returns.
//!
//! `sse` frames bytes into `data` payloads; `openai` assembles OpenAI
//! Chat Completions chunks and `anthropic` assembles Anthropic Messages
//! events. The reader lives here so `client` only decides *whether* to
//! stream (response `Content-Type`) and which wire format it is, not how.

mod anthropic;
mod openai;
mod sse;

use std::time::Duration;

use super::client::{map_reqwest_error, GatewayError};
use super::config::ApiFormat;
use super::reply::LlmReply;

/// One observable step of a gateway call, handed to the caller's closure as
/// it happens. Names no `research` concept and no wire format (ADR-0007):
/// `research` maps it to `model.delta` and `model.retry`.
///
/// Fragments belong to the attempt in flight. After [`LlmEvent::Retry`] the
/// attempt restarts from scratch, so a consumer discards what it collected
/// for the failed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmEvent {
    /// A fragment of the assistant's answer text.
    Text(String),
    /// A fragment of the model's reasoning.
    Thinking(String),
    /// A fragment of one tool call's JSON arguments. `index` is the call's
    /// position in the reply; `id` and `name` are what is known about the
    /// call so far (usually set from the first fragment on).
    ToolArgs {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        fragment: String,
    },
    /// The attempt numbered `attempt` failed with `reason` and the gateway
    /// is about to sleep `delay_ms` before the next one.
    Retry {
        attempt: u32,
        delay_ms: u64,
        reason: String,
    },
}

/// Whether a response `Content-Type` announces an SSE body.
pub(super) fn is_event_stream(content_type: Option<&str>) -> bool {
    content_type
        .and_then(|value| value.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("text/event-stream"))
}

/// Whether the stream is over after a payload.
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    More,
    Done,
}

/// The per-format assembler of one attempt. An enum, not a `trait`
/// (ADR-0006, ADR-0007): the format is a branch inside `llm`.
enum Assembler {
    Openai(openai::Assembler),
    Anthropic(anthropic::Assembler),
}

impl Assembler {
    fn new(format: ApiFormat) -> Self {
        match format {
            ApiFormat::Openai => Self::Openai(openai::Assembler::default()),
            ApiFormat::Anthropic => Self::Anthropic(anthropic::Assembler::default()),
        }
    }

    fn feed(
        &mut self,
        payload: &str,
        on_event: &mut (dyn FnMut(LlmEvent) + Send),
    ) -> Result<Flow, GatewayError> {
        match self {
            Self::Openai(assembler) => assembler.feed(payload, on_event),
            Self::Anthropic(assembler) => assembler.feed(payload, on_event),
        }
    }

    fn finish(self) -> Result<LlmReply, GatewayError> {
        match self {
            Self::Openai(assembler) => assembler.finish(),
            Self::Anthropic(assembler) => assembler.finish(),
        }
    }
}

/// Read an SSE body of the given wire `format` to its end and assemble the
/// reply. Each wait for the next chunk is bounded by `idle`; a longer
/// silence is a [`GatewayError::Timeout`]. The per-attempt and run timeouts
/// bound the whole body through the `reqwest` request itself.
pub(super) async fn read_sse(
    mut resp: reqwest::Response,
    format: ApiFormat,
    idle: Duration,
    on_event: &mut (dyn FnMut(LlmEvent) + Send),
) -> Result<LlmReply, GatewayError> {
    let mut parser = sse::SseParser::default();
    let mut assembler = Assembler::new(format);
    let mut payloads = Vec::new();
    loop {
        let chunk = match tokio::time::timeout(idle, resp.chunk()).await {
            Err(_) => return Err(GatewayError::Timeout),
            Ok(chunk) => chunk.map_err(map_reqwest_error)?,
        };
        let Some(chunk) = chunk else {
            return assembler.finish();
        };
        parser.push(&chunk, &mut payloads);
        for payload in payloads.drain(..) {
            if assembler.feed(&payload, on_event)? == Flow::Done {
                return assembler.finish();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_decides_sse() {
        assert!(is_event_stream(Some("text/event-stream")));
        assert!(is_event_stream(Some("Text/Event-Stream; charset=utf-8")));
        assert!(!is_event_stream(Some("application/json")));
        assert!(!is_event_stream(Some("text/plain")));
        assert!(!is_event_stream(None));
    }
}
