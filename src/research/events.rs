//! Session Events (ADR-0010): one envelope per observable step, written as
//! NDJSON the moment it happens.
//!
//! [`Events`] is the one emitter handle of a Session. The loop and
//! `run_research` call its typed methods (`session_started`, `turn_started`,
//! `model_replied`, `session_finished`); each builds the `data` of one
//! catalogue type and funnels through a single private `emit`, which stamps
//! the envelope (`v`, `seq`, `ts`, `elapsed_ms`, `session_id`, `turn`,
//! `type`) and writes every enabled output before returning. Outputs are
//! concrete fields of the state, not implementations of a shared interface:
//! the NDJSON writer (`--events`), the Session log (#150) and the Progress
//! renderer (#149).
//!
//! The handle is shared by `&` across the loop; the mutable state sits behind
//! a `Mutex` held only for one stamp-and-write. [`Events::disabled`] has no
//! state at all, so every method returns on its first line.
//!
//! Progress (#149), the human view on stderr, is the third output: `emit`
//! hands it the same envelope fields, so it renders from the documented Event
//! `data` and has no instrumentation of its own. It clears its transient line
//! before any other output writes to stderr and renders last.
//!
//! The Session log is the Session file in format 2: `emit` appends every
//! Event (except `model.delta`) as an `event` row in the same call that
//! writes the NDJSON line, and [`Events::turn_ended`] appends the `turn` rows,
//! which are not Events. A write failure never reaches the loop: the
//! [`SessionSink`] warns (defaulted path) or records the failure (explicit
//! path) and the Session turns it into exit 6 when it ends.
//!
//! With `--trace` (#151) [`Events::trace_model`] and
//! [`Events::trace_tool_results`] append `trace` rows to the Session log
//! only, like `turn` rows: a trace is a record, not an Event.
//!
//! The emitter also keeps the running totals `session.finished` needs
//! (`turns_used`, `turn_budget`, `usage`), so a Session that fails after
//! paying for model calls still reports them.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::llm::{
    ApiFormat, ChatMessage, GatewayConfig, GatewayError, LlmReply, TokenUsage, ToolChoice, ToolDef,
};
use crate::research::agent_loop::context::ToolMessage;
use crate::research::agent_loop::ToolEvidence;
use crate::research::dto::{ResearchRequest, ResearchResponse, UsageDTO};
use crate::research::run::ResearchError;
use crate::research::session::{EventLine, SessionHeader, SessionSink, SessionUsage, TurnRow};
use crate::research::trace::{self, TraceState};
use crate::web::fetch::evidence::format_timestamp;

mod progress;
mod tools;
use progress::Progress;
pub use progress::ProgressMode;
pub use progress::CLEAR_LINE;
pub(crate) use tools::ToolFailure;

/// Envelope version, independent of `SESSION_FORMAT_VERSION`: adding a field
/// or a type keeps it, renaming or removing one bumps it (ADR-0010 §2).
pub const ENVELOPE_VERSION: u32 = 1;

/// Where `--events` writes the NDJSON stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventsSink {
    /// The process stderr (`--events -`).
    Stderr,
    /// A file, created `0600` on unix and appended to: Sessions never
    /// truncate what an earlier one wrote, and `session_id` tells them apart.
    File(PathBuf),
}

/// The Event emitter of one Session. See the module docs.
pub struct Events {
    state: Option<Mutex<State>>,
}

struct State {
    session_id: String,
    created: Instant,
    seq: u64,
    turns_used: u32,
    turn_budget: u32,
    usage: TokenUsage,
    /// `None` once a write failed: the output is disabled for the rest of
    /// the Session, `seq` keeps counting.
    ndjson: Option<Box<dyn Write + Send>>,
    /// The Session file (format 2), attached once the Session starts.
    session: Option<SessionSink>,
    /// The human view on stderr (`--progress`, #149); `None` when off.
    progress: Option<Progress>,
    /// `Some` when `--trace` is on (#151).
    trace: Option<TraceState>,
}

#[derive(Serialize)]
struct Envelope<'a> {
    v: u32,
    seq: u64,
    ts: String,
    elapsed_ms: u64,
    session_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn: Option<u32>,
    #[serde(rename = "type")]
    kind: &'a str,
    data: Value,
}

impl Events {
    /// An emitter with no output at all, not even a Session log: every
    /// method is a no-op. Used by `run_loop` and the tests of the loop.
    pub fn disabled() -> Self {
        Self { state: None }
    }

    /// Open the emitter of `session_id` with the requested `--events`
    /// output; `None` leaves only the Session log (attached later), which
    /// every Session has. `progress` is `--progress`: stderr is shared, so
    /// `--events -` turns it off. The `Err` carries the reason an output could not
    /// be opened; the caller maps it to `NotConfigured` before any gateway
    /// call.
    pub fn open(
        session_id: &str,
        sink: Option<&EventsSink>,
        progress: ProgressMode,
    ) -> Result<Self, String> {
        let writer: Option<Box<dyn Write + Send>> = match sink {
            None => None,
            Some(EventsSink::Stderr) => Some(Box::new(std::io::stderr())),
            Some(EventsSink::File(path)) => {
                let mut options = OpenOptions::new();
                options.append(true).create(true);
                // Events carry the goal and error text: owner-only on creation.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let file = options.open(path).map_err(|err| {
                    format!("cannot open the events file {}: {err}", path.display())
                })?;
                Some(Box::new(file))
            }
        };
        let mut events = Self::new(session_id, writer);
        if let Some(state) = events.state.as_mut() {
            state.get_mut().unwrap_or_else(|p| p.into_inner()).progress =
                Progress::for_stderr(progress, sink);
        }
        Ok(events)
    }

    #[cfg(test)]
    pub(crate) fn with_writer(session_id: &str, writer: Box<dyn Write + Send>) -> Self {
        Self::new(session_id, Some(writer))
    }

    /// An emitter whose only output is a Progress renderer over `out`.
    #[cfg(test)]
    pub(crate) fn with_progress(
        session_id: &str,
        style: progress::Style,
        out: Box<dyn Write + Send>,
    ) -> Self {
        let events = Self::new(session_id, None);
        Self::lock(events.state.as_ref().expect("enabled")).progress =
            Some(Progress::new(style, out));
        events
    }

    fn new(session_id: &str, ndjson: Option<Box<dyn Write + Send>>) -> Self {
        Self {
            state: Some(Mutex::new(State {
                session_id: session_id.to_owned(),
                created: Instant::now(),
                seq: 0,
                turns_used: 0,
                turn_budget: 0,
                usage: TokenUsage::default(),
                ndjson,
                session: None,
                progress: None,
                trace: None,
            })),
        }
    }

    fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
        state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The one place an Event is stamped and written to every enabled output.
    fn emit(&self, turn: Option<u32>, kind: &str, data: Value) {
        let Some(state) = &self.state else {
            return;
        };
        let mut guard = Self::lock(state);
        let state = &mut *guard;
        state.seq += 1;
        let envelope = Envelope {
            v: ENVELOPE_VERSION,
            seq: state.seq,
            ts: now_rfc3339_millis(),
            elapsed_ms: u64::try_from(state.created.elapsed().as_millis()).unwrap_or(u64::MAX),
            session_id: &state.session_id,
            turn,
            kind,
            data,
        };
        // The other outputs may warn on stderr: erase the transient line first.
        if let Some(progress) = state.progress.as_mut() {
            progress.clear();
        }
        let mut line = serde_json::to_string(&envelope).expect("an envelope serializes");
        line.push('\n');
        if let Some(writer) = state.ndjson.as_mut() {
            if let Err(err) = writer
                .write_all(line.as_bytes())
                .and_then(|()| writer.flush())
            {
                // A paid Session never fails on its audit output: say so once
                // and stop writing.
                eprintln!("warning: events not written: {err}");
                state.ndjson = None;
            }
        }
        // Deltas are fragments of a reply the `turn`/`trace` rows keep whole.
        if kind != "model.delta" {
            if let Some(sink) = state.session.as_mut() {
                // A failure is kept by the sink; the loop never sees it.
                let _ = sink.append(&EventLine::new(&envelope));
            }
        }
        if let Some(progress) = state.progress.as_mut() {
            progress.render(envelope.turn, kind, envelope.elapsed_ms, &envelope.data);
        }
    }

    /// Start the Session file with its `header` row (line 0). `Err` is an
    /// explicit `--session-out` that cannot be written: the caller ends the
    /// Session as `Io` before any gateway call. A defaulted path never
    /// fails: the sink warns once and stays disabled.
    pub(crate) fn attach_session_log(
        &self,
        sink: SessionSink,
        header: &SessionHeader,
    ) -> Result<(), String> {
        let Some(state) = &self.state else {
            return Ok(());
        };
        let mut state = Self::lock(state);
        let sink = state.session.insert(sink);
        sink.append(header).map_err(|err| err.to_string())
    }

    /// The first write failure of an explicit Session path, if any. A
    /// Session that otherwise succeeded ends with it as exit 6.
    pub(crate) fn session_log_failure(&self) -> Option<String> {
        let state = Self::lock(self.state.as_ref()?);
        state.session.as_ref()?.failure().map(str::to_owned)
    }

    /// After the Session: enforce the retention cap on a defaulted path.
    pub(crate) fn finish_session_log(&self) {
        let Some(state) = &self.state else {
            return;
        };
        if let Some(sink) = Self::lock(state).session.as_ref() {
            sink.finish();
        }
    }

    /// Append the `turn` row of `turn`, which just ended. It goes to the
    /// Session log only: a `turn` row is a record of Evidence, not an Event.
    /// The Evidence is the whole pages `turn` fetched (#80). `finish` is the
    /// final Turn's Synthesis and run usage.
    pub(crate) fn turn_ended(
        &self,
        turn: u32,
        evidence: &[ToolEvidence],
        finish: Option<(&Value, SessionUsage)>,
    ) {
        let Some(state) = &self.state else {
            return;
        };
        let mut state = Self::lock(state);
        let state = &mut *state;
        if let Some(progress) = state.progress.as_mut() {
            progress.clear();
        }
        let Some(sink) = state.session.as_mut() else {
            return;
        };
        let pages = evidence
            .iter()
            .filter(|item| item.turn == turn)
            .filter_map(|item| item.page.as_deref().cloned())
            .collect();
        let (synthesis, usage) = match finish {
            Some((synthesis, usage)) => (Some(synthesis.clone()), usage),
            None => (None, SessionUsage::default()),
        };
        let row = TurnRow::new(state.session_id.clone(), turn, pages, synthesis, usage);
        let _ = sink.append(&row);
    }

    /// Turn `--trace` on: from now on the trace methods write `trace` rows.
    pub(crate) fn enable_trace(&self) {
        if let Some(state) = &self.state {
            Self::lock(state).trace = Some(TraceState::default());
        }
    }

    /// `--trace`: the `model` row of `turn`, written right after the gateway
    /// call and before tool dispatch. `messages` is the request just sent
    /// (system prompt first); `outcome` is the reply or the gateway error.
    /// A no-op without `--trace`.
    pub(crate) fn trace_model(
        &self,
        turn: u32,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        choice: Option<&ToolChoice>,
        outcome: Result<&LlmReply, &GatewayError>,
    ) {
        let Some(state) = &self.state else {
            return;
        };
        let mut state = Self::lock(state);
        let state = &mut *state;
        let (Some(tracer), Some(sink)) = (state.trace.as_mut(), state.session.as_mut()) else {
            return;
        };
        let row = tracer.model_row(&state.session_id, turn, messages, tools, choice, outcome);
        let _ = sink.append(&row);
    }

    /// `--trace`: the `tool_results` row of `turn`, written after dispatch,
    /// with the content the model receives next. A no-op without `--trace`.
    pub(crate) fn trace_tool_results(&self, turn: u32, results: &[ToolMessage]) {
        let Some(state) = &self.state else {
            return;
        };
        let mut state = Self::lock(state);
        let state = &mut *state;
        if state.trace.is_none() {
            return;
        }
        if let Some(sink) = state.session.as_mut() {
            let _ = sink.append(&trace::tool_results_row(&state.session_id, turn, results));
        }
    }

    /// `session.started`: configuration is valid, the first Turn is next.
    pub(crate) fn session_started(&self, req: &ResearchRequest, gateway: &GatewayConfig) {
        self.emit(
            None,
            "session.started",
            json!({
                "goal": req.goal,
                "size": req.size.as_str(),
                "max_turns": req.max_turns,
                "max_turns_cap": req.max_turns_cap,
                "deadline_secs": req.deadline_secs,
                "model": gateway.model,
                "api_format": match gateway.api_format {
                    ApiFormat::Openai => "openai",
                    ApiFormat::Anthropic => "anthropic",
                },
                // The configured `GATEWAY_STREAM`; a format without a
                // streaming parser still answers single-body.
                "stream": gateway.stream,
            }),
        );
    }

    /// `turn.started`: just before the gateway call of `turn`.
    /// `turns_left` counts the Turns after this one.
    pub(crate) fn turn_started(
        &self,
        turn: u32,
        turn_budget: u32,
        final_turn: bool,
        final_reason: Option<&str>,
    ) {
        if let Some(state) = &self.state {
            let mut state = Self::lock(state);
            state.turns_used = turn;
            state.turn_budget = turn_budget;
        }
        self.emit(
            Some(turn),
            "turn.started",
            json!({
                "turn_budget": turn_budget,
                "turns_left": if final_turn { 0 } else { turn_budget.saturating_sub(turn) },
                "final_turn": final_turn,
                "final_reason": final_reason,
            }),
        );
    }

    /// `model.replied`: the gateway call of `turn` returned `reply`.
    pub(crate) fn model_replied(&self, turn: u32, reply: &LlmReply) {
        if let Some(state) = &self.state {
            Self::lock(state).usage.add(&reply.usage);
        }
        let tool_calls: Vec<Value> = reply
            .tool_calls
            .iter()
            .map(|call| json!({ "id": call.id, "name": call.name }))
            .collect();
        self.emit(
            Some(turn),
            "model.replied",
            json!({
                "finish_reason": reply.finish_reason,
                "tool_calls": tool_calls,
                "output_chars": reply.output.chars().count(),
                "thinking_chars": reply.thinking.chars().count(),
                "usage": UsageDTO::from(reply.usage.clone()),
            }),
        );
    }

    /// `session.finished`: the last Event of the Session, whatever its exit.
    /// On failure `turns_used`, `turn_budget` and `usage` are what the
    /// Session had consumed when it stopped; `evidence_urls` is only known
    /// for an `ok` Session.
    pub(crate) fn session_finished(&self, result: &Result<ResearchResponse, ResearchError>) {
        let Some(state) = &self.state else {
            return;
        };
        let data = match result {
            Ok(response) => json!({
                "outcome": "ok",
                "exit_code": 0,
                "turns_used": response.turns_used,
                "turn_budget": response.turn_budget,
                "usage": response.usage,
                "evidence_urls": response.evidence_urls,
                "error": Value::Null,
            }),
            Err(err) => {
                let state = Self::lock(state);
                json!({
                    "outcome": err.outcome(),
                    "exit_code": err.exit_code(),
                    "turns_used": state.turns_used,
                    "turn_budget": state.turn_budget,
                    "usage": UsageDTO::from(state.usage.clone()),
                    "evidence_urls": Vec::<String>::new(),
                    "error": err.to_string(),
                })
            }
        };
        self.emit(None, "session.finished", data);
    }
}

/// Now as `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC).
fn now_rfc3339_millis() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = format_timestamp(now.as_secs());
    let seconds = seconds.strip_suffix('Z').unwrap_or(&seconds);
    format!("{seconds}.{:03}Z", now.subsec_millis())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;

    struct FailingWriter(Arc<AtomicUsize>);

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other("disk full"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A write failure disables the output for the rest of the Session: the
    /// emitter neither panics nor retries the writer on every later Event.
    #[test]
    fn a_failed_write_disables_the_output_instead_of_failing_the_session() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let events = Events::with_writer("s", Box::new(FailingWriter(Arc::clone(&attempts))));
        events.turn_started(1, 10, false, None);
        events.turn_started(2, 10, false, None);
        events.turn_started(3, 10, true, Some("budget"));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn timestamps_are_utc_rfc3339_with_milliseconds() {
        let ts = now_rfc3339_millis();
        let bytes = ts.as_bytes();
        assert_eq!(ts.len(), "2026-10-14T19:40:02.118Z".len(), "{ts}");
        assert_eq!(
            (bytes[4], bytes[10], bytes[19], bytes[23]),
            (b'-', b'T', b'.', b'Z')
        );
        assert!(ts[20..23].chars().all(|c| c.is_ascii_digit()), "{ts}");
    }

    /// Tool calls on the final Turn are never dispatched, so no Turns are left
    /// after it; otherwise the budget minus the Turn number.
    #[test]
    fn turns_left_counts_the_turns_after_this_one() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        struct Shared(Arc<Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let events = Events::with_writer("s", Box::new(Shared(Arc::clone(&buffer))));
        events.turn_started(2, 10, false, None);
        events.turn_started(10, 10, true, Some("budget"));
        let text = String::from_utf8(buffer.lock().clone()).unwrap();
        let rows: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows[0]["data"]["turns_left"], 8);
        assert_eq!(rows[1]["data"]["turns_left"], 0);
        assert_eq!(rows[1]["data"]["final_reason"], "budget");
        assert_eq!(rows[0]["data"]["final_reason"], Value::Null);
    }
}
