//! Tool and verification Events (#148, ADR-0010 §3): `search.*`, `fetch.*`,
//! `tool.failed`, `context.truncated`, `answer.verified`.
//!
//! Typed methods on [`Events`], split out of `events.rs` to keep it under
//! the file budget. Each builds the `data` of one catalogue type from what
//! the tools already return ([`ToolResult`], the structured engine reports);
//! `web/` stays unaware of Events (ADR-0006). All funnel through the one
//! private `emit` of the parent module, so they reach every output.

use serde_json::{json, Value};

use super::Events;
use crate::llm::RequestedToolCall;
use crate::research::agent_loop::result::ToolResult;
use crate::research::dto::VerificationDTO;
use crate::research::synthesis::Synthesis;
use crate::web::fetch::tool::FETCH_TOOL_NAME;
use crate::web::search::tool::SEARCH_TOOL_NAME;
use crate::web::search::types::EngineReport;

/// One failed tool call, as `tool.failed` reports it. `call_id` and `tool`
/// are `None` for the empty-answer repair, which is no call at all.
pub(crate) struct ToolFailure<'a> {
    pub call_id: Option<&'a str>,
    pub tool: Option<&'a str>,
    /// `dispatch`, `execution`, or `empty_answer`.
    pub kind: &'a str,
    pub reason: &'a str,
}

/// What the model asked for, echoed as sent: a missing or malformed
/// argument shows as `null`, so a dispatch failure still says what was
/// requested.
fn arg(args: &Value, key: &str) -> Value {
    args.get(key).cloned().unwrap_or(Value::Null)
}

/// One engine of `search.finished`. `status` is `rows`, `no_results`, or the
/// kind of the engine's first problem when no leg answered (`suspended`,
/// `challenge`, `timeout`, `throttled`, `error`); `problems` lists every
/// distinct way its other legs failed. `cached` is true when every leg that
/// answered came from the disk cache.
fn engine_json(report: &EngineReport) -> Value {
    json!({
        "engine": report.engine,
        "status": report.status(),
        "rows": report.rows,
        "cached": report.answered_legs > 0 && report.cached_legs == report.answered_legs,
        "problems": report.problems,
    })
}

impl Events {
    /// `search.started` or `fetch.started` for `call`, which is about to
    /// run; any other tool has no Event of its own (`tool.failed` covers it).
    /// Arguments that do not parse show as `null` fields.
    pub(crate) fn tool_started(&self, turn: u32, call: &RequestedToolCall) {
        if self.state.is_none() {
            return;
        }
        let args = call.parsed_arguments().unwrap_or(Value::Null);
        match call.name.as_str() {
            SEARCH_TOOL_NAME => self.search_started(turn, &call.id, &args),
            FETCH_TOOL_NAME => self.fetch_started(turn, &call.id, &args),
            _ => {}
        }
    }

    /// `search.finished` or `fetch.finished` for `call`, which returned (or
    /// timed out as) `result`.
    pub(crate) fn tool_finished(&self, turn: u32, call: &RequestedToolCall, result: &ToolResult) {
        match call.name.as_str() {
            SEARCH_TOOL_NAME | FETCH_TOOL_NAME if self.state.is_none() => {}
            SEARCH_TOOL_NAME => {
                let args = call.parsed_arguments().unwrap_or(Value::Null);
                self.search_finished(turn, &call.id, &args, result);
            }
            FETCH_TOOL_NAME => {
                let args = call.parsed_arguments().unwrap_or(Value::Null);
                self.fetch_finished(turn, &call.id, &args, result);
            }
            _ => {}
        }
    }

    fn search_started(&self, turn: u32, call_id: &str, args: &Value) {
        self.emit(
            Some(turn),
            "search.started",
            json!({
                "call_id": call_id,
                "queries": arg(args, "queries"),
                "recency": arg(args, "recency"),
            }),
        );
    }

    /// `search.finished`: the `search` call returned `result`. `hits` is the
    /// merged Hit count; `queries` repeats the request so a reader of the
    /// finish alone (Progress) knows what it answers; `engines` is empty when no leg ran (a `note`) or the
    /// whole call failed (then `error` carries every engine's failure).
    fn search_finished(&self, turn: u32, call_id: &str, args: &Value, result: &ToolResult) {
        let (hits, engines, note) = match result {
            ToolResult::Search { hits, engines, .. } => (
                hits.len(),
                engines.iter().map(engine_json).collect::<Vec<_>>(),
                None,
            ),
            ToolResult::SearchNote { note } => (0, Vec::new(), Some(note.as_str())),
            _ => (0, Vec::new(), None),
        };
        self.emit(
            Some(turn),
            "search.finished",
            json!({
                "call_id": call_id,
                "queries": arg(args, "queries"),
                "ok": result.is_success(),
                "hits": hits,
                "engines": engines,
                "note": note,
                "error": result.error_text(),
            }),
        );
    }

    /// `fetch.started`: a `fetch` call is about to run.
    fn fetch_started(&self, turn: u32, call_id: &str, args: &Value) {
        self.emit(
            Some(turn),
            "fetch.started",
            json!({
                "call_id": call_id,
                "url": arg(args, "url"),
                "part": arg(args, "part"),
            }),
        );
    }

    /// `fetch.finished`: the `fetch` call returned `result`. `chars` is the
    /// size of the delivered part; a repeat fetch without `part` delivers
    /// nothing (`chars` 0, `part` null) and is served from memory.
    fn fetch_finished(&self, turn: u32, call_id: &str, args: &Value, result: &ToolResult) {
        let mut data = json!({
            "call_id": call_id,
            "url": arg(args, "url"),
            "ok": result.is_success(),
            "final_url": Value::Null,
            "fetch_path": Value::Null,
            "part": Value::Null,
            "parts": Value::Null,
            "chars": 0,
            "served_from": Value::Null,
            "error": result.error_text(),
        });
        match result {
            ToolResult::Fetch {
                evidence,
                span,
                part,
                parts,
                served,
                ..
            } => {
                data["final_url"] = json!(evidence.source_url);
                data["fetch_path"] = json!(evidence.fetch_path);
                data["part"] = json!(part);
                data["parts"] = json!(parts);
                data["chars"] = json!(evidence.markdown[span.clone()].chars().count());
                data["served_from"] = json!(served.as_str());
            }
            ToolResult::AlreadyFetched {
                first_url, parts, ..
            } => {
                data["final_url"] = json!(first_url);
                data["parts"] = json!(parts);
                data["served_from"] = json!("memory");
            }
            _ => {}
        }
        self.emit(Some(turn), "fetch.finished", data);
    }

    /// `tool.failed`: one failed call (or, with `kind: "empty_answer"`, a
    /// reply that was neither a tool call nor an answer). `consecutive_failures`
    /// is the repair counter after this Turn, to compare with `max_repairs`:
    /// the Session aborts when it exceeds it.
    pub(crate) fn tool_failed(
        &self,
        turn: u32,
        failure: &ToolFailure<'_>,
        consecutive_failures: u32,
        max_repairs: u32,
    ) {
        self.emit(
            Some(turn),
            "tool.failed",
            json!({
                "call_id": failure.call_id,
                "tool": failure.tool,
                "kind": failure.kind,
                "reason": failure.reason,
                "consecutive_failures": consecutive_failures,
                "max_repairs": max_repairs,
            }),
        );
    }

    /// `context.truncated`: `dropped_turns` oldest Turns left the transcript
    /// to fit `max_context_chars`.
    pub(crate) fn context_truncated(
        &self,
        turn: u32,
        dropped_turns: usize,
        max_context_chars: usize,
    ) {
        self.emit(
            Some(turn),
            "context.truncated",
            json!({
                "dropped_turns": dropped_turns,
                "max_context_chars": max_context_chars,
            }),
        );
    }

    /// `answer.verified`: the grounding check ran over `synthesis`. Carries
    /// the `verification` counts and the citation count that survived.
    pub(crate) fn answer_verified(&self, turn: u32, synthesis: &Synthesis) {
        if self.state.is_none() {
            return;
        }
        let mut data = serde_json::to_value(VerificationDTO::from(synthesis.verification))
            .expect("verification counts serialize");
        data["citations"] = json!(synthesis.citations.len());
        self.emit(Some(turn), "answer.verified", data);
    }
}
