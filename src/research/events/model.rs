//! Model stream Events (#154, ADR-0010 §2 and §6): `model.delta` and
//! `model.retry`, plus the observer that maps the gateway's `LlmEvent`s onto
//! them.
//!
//! [`Events::model_observer`] is what the loop hands to
//! `Gateway::chat_observed`. `research` never sees the wire format
//! (ADR-0007): the gateway already reduced it to `LlmEvent`.
//!
//! `model.retry` is low volume and goes through the common `emit`, so every
//! output and every `--events-detail` sees it. `model.delta` is high volume
//! and takes its own path:
//! - It is a stream Event only with `--events-detail tokens` and an
//!   `--events` output. Only then does it consume a `seq`, so the NDJSON
//!   stream has no holes at the default detail (a delta nobody reads is not
//!   an Event of the Session). With tokens on, the Session log skips it and
//!   shows the gap, as ADR-0010 §2 says a filtering output does.
//! - Progress gets it whatever the detail, but only when it draws a status
//!   line (a terminal); `plain` and piped Progress take none.
//! - It is serialized from borrowed fields into a reused buffer: no `Value`
//!   tree and no copy of the fragment per delta.

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{write_ndjson, Envelope, Events, ENVELOPE_VERSION};
use crate::llm::LlmEvent;

/// `--events-detail`: how much of the model stream `--events` carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventsDetail {
    /// Every Event but `model.delta`.
    #[default]
    Normal,
    /// Also `model.delta`: every streamed fragment, in order.
    Tokens,
}

impl EventsDetail {
    /// For serde: `normal` is the default and stays out of a serialized request.
    pub fn is_normal(&self) -> bool {
        *self == Self::Normal
    }
}

/// Which stream a `model.delta` fragment belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Channel {
    Text,
    Thinking,
    ToolArgs,
}

/// The `data` of one `model.delta`. `text` is the fragment on every
/// channel; `index` groups the fragments of one tool call, and `id` and
/// `name` are filled in once the model has said them. `attempt` is the
/// gateway attempt the fragment belongs to: a `model.retry` discards the
/// deltas of its `attempt` and the next ones carry `attempt + 1`.
#[derive(Serialize)]
pub(super) struct Delta<'a> {
    pub attempt: u32,
    pub channel: Channel,
    pub text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
}

impl Events {
    /// The closure for `Gateway::chat_observed` over the gateway call of
    /// `turn`: fragments become `model.delta`, a gateway retry becomes
    /// `model.retry` and moves the following deltas to the next `attempt`.
    pub(crate) fn model_observer(&self, turn: u32) -> impl FnMut(LlmEvent) + Send + '_ {
        let mut attempt: u32 = 1;
        move |event| {
            let delta = match &event {
                LlmEvent::Text(text) => Delta {
                    attempt,
                    channel: Channel::Text,
                    text,
                    index: None,
                    id: None,
                    name: None,
                },
                LlmEvent::Thinking(text) => Delta {
                    attempt,
                    channel: Channel::Thinking,
                    text,
                    index: None,
                    id: None,
                    name: None,
                },
                LlmEvent::ToolArgs {
                    index,
                    id,
                    name,
                    fragment,
                } => Delta {
                    attempt,
                    channel: Channel::ToolArgs,
                    text: fragment,
                    index: Some(*index),
                    id: id.as_deref(),
                    name: name.as_deref(),
                },
                LlmEvent::Retry {
                    attempt: failed,
                    delay_ms,
                    reason,
                } => {
                    self.model_retry(turn, *failed, *delay_ms, reason);
                    attempt = failed.saturating_add(1);
                    return;
                }
            };
            self.model_delta(turn, &delta);
        }
    }

    /// `model.retry`: the gateway attempt `attempt` failed and it sleeps
    /// `delay_ms` before the next one. Consumers discard that attempt's deltas.
    fn model_retry(&self, turn: u32, attempt: u32, delay_ms: u64, reason: &str) {
        self.emit(
            Some(turn),
            "model.retry",
            json!({ "attempt": attempt, "delay_ms": delay_ms, "reason": reason }),
        );
    }

    /// `model.delta`, to the outputs that want it. See the module docs.
    fn model_delta(&self, turn: u32, delta: &Delta<'_>) {
        let Some(state) = &self.state else {
            return;
        };
        let mut guard = Self::lock(state);
        let state = &mut *guard;
        let on_screen = state.progress.as_ref().is_some_and(|p| p.wants_deltas());
        if !state.deltas_in_stream && !on_screen {
            return;
        }
        if state.deltas_in_stream {
            state.seq += 1;
            // The writer may warn on stderr: erase the transient line first.
            if let Some(progress) = state.progress.as_mut() {
                progress.clear();
            }
            let envelope = Envelope {
                v: ENVELOPE_VERSION,
                seq: state.seq,
                ts: super::now_rfc3339_millis(),
                elapsed_ms: state.elapsed_ms(),
                session_id: &state.session_id,
                turn: Some(turn),
                kind: "model.delta",
                data: delta,
            };
            write_ndjson(&mut state.ndjson, &mut state.line, &envelope);
        }
        if let Some(progress) = state.progress.as_mut() {
            progress.delta(turn, delta);
        }
    }
}
