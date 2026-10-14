//! Session persistence: Session header, Event and Turn rows as JSONL.
//!
//! Evidence itself is owned by `web::fetch`; this module only records it.
//!
//! Record schema (ADR-0002, amended by ADR-0010 §5): one JSON object per line
//! in `<data_root>/sessions/<id>.jsonl` (or the explicit `--session-out`),
//! UTF-8, `\n`-terminated, append-only, created with mode 0600 on unix.
//! `format_version` rides on EVERY row so a single row stays self-describing
//! and 1:1 migratable to the future `turns` table (`turn→id`,
//! `session_id→session_id`, `evidence[].source_url/collected_at→url/fetched_at`).
//!
//! Format 2 is an incremental log, written by the Event emitter as the
//! Session happens: the `header` row first, then `event` rows (the Event
//! envelope of `--events` flattened next to `kind` and `format_version`, so
//! each maps 1:1 to a future `events` table, `model.delta` never stored) and
//! `turn` rows, in the order they happened. A `turn` row is written when its
//! Turn ends; the final one carries the Synthesis and the run usage. Format 1
//! files (header + `turn` rows, written after the answer) still read.
//!
//! With `--trace` (#151) the log also holds `trace` rows: per Turn, the
//! messages sent and the reply, then the tool results the model will see.
//! They are not Events (no `seq`, never in `--events`), and a Session
//! without `--trace` writes none. `research::trace` builds them.

use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::research::data_dir;
use crate::web::fetch::evidence::utc_now_rfc3339;
use crate::web::fetch::Evidence;

/// Line 0 of the Session file (SHOULD exist): recalls goal/size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    /// Always `"header"`.
    pub kind: String,
    pub format_version: u32,
    pub session_id: String,
    pub goal: String,
    /// `"small"|"medium"|"large"` (plain string keeps the session row free of
    /// synthesis-type serde; the handler passes `SynthesisSize::as_str()`).
    pub size: String,
    pub started_at: String,
}

impl SessionHeader {
    pub fn new(session_id: String, goal: String, size: String) -> Self {
        Self {
            kind: "header".to_owned(),
            format_version: crate::SESSION_FORMAT_VERSION,
            session_id,
            goal,
            size,
            started_at: utc_now_rfc3339(),
        }
    }
}

/// Per-turn token counts; saturating sums live in the handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub reasoning_tokens: u64,
    /// `#[serde(default)]`: old JSONL rows predate this field and must
    /// still parse without bumping `SESSION_FORMAT_VERSION`.
    #[serde(default)]
    pub cached_prompt_tokens: u64,
    /// Prompt tokens written to the provider cache; `#[serde(default)]` for
    /// rows written before #47.
    #[serde(default)]
    pub cache_creation_prompt_tokens: u64,
}

/// Lines 1..N of the Session file: one per turn, `synthesis` null until
/// the final turn (which carries the response object as opaque JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRow {
    /// Always `"turn"`.
    pub kind: String,
    pub format_version: u32,
    pub session_id: String,
    pub turn: u32,
    pub evidence: Vec<Evidence>,
    pub synthesis: Option<serde_json::Value>,
    pub usage: SessionUsage,
    pub recorded_at: String,
}

impl TurnRow {
    pub fn new(
        session_id: String,
        turn: u32,
        evidence: Vec<Evidence>,
        synthesis: Option<serde_json::Value>,
        usage: SessionUsage,
    ) -> Self {
        Self {
            kind: "turn".to_owned(),
            format_version: crate::SESSION_FORMAT_VERSION,
            session_id,
            turn,
            evidence,
            synthesis,
            usage,
            recorded_at: utc_now_rfc3339(),
        }
    }
}

/// A `trace` row (`--trace`, #151, ADR-0010 §5): what the model saw and said
/// on one Turn. Two rows per tool Turn, told apart by `phase`:
///
/// - `model`: written right after the gateway call, before tool dispatch.
///   Carries the request (`messages_delta`, or `messages` + `full: true`
///   after context truncation; `system` and `tools` on the first row only;
///   `tool_choice`) and the `reply` (or `{"error": ...}`).
/// - `tool_results`: written after dispatch, with each call's `id` and the
///   exact `content` string the model receives.
///
/// `docs/harness.md` ("Trace rows") has the rebuild rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRow {
    /// Always `"trace"`.
    pub kind: String,
    pub format_version: u32,
    pub session_id: String,
    pub turn: u32,
    /// `"model"` or `"tool_results"`.
    pub phase: String,
    /// System prompt, first `model` row only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Tool definitions offered (OpenAI shape), first `model` row only and
    /// only when any were offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<serde_json::Value>,
    /// Messages appended to the request since the previous `model` row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_delta: Option<Vec<serde_json::Value>>,
    /// The whole request (system prompt apart) on a `full: true` row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<serde_json::Value>>,
    /// `true` when `messages` replaces the transcript instead of extending it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub full: bool,
    /// `"auto"`/`"none"`; absent when the Turn offered no tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<serde_json::Value>,
    /// `tool_results` rows: `{id, content}` per call, in call order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<Vec<serde_json::Value>>,
    pub recorded_at: String,
}

impl TraceRow {
    /// An empty row of `phase`; the caller fills the fields that phase has.
    pub(crate) fn new(session_id: &str, turn: u32, phase: &str) -> Self {
        Self {
            kind: "trace".to_owned(),
            format_version: crate::SESSION_FORMAT_VERSION,
            session_id: session_id.to_owned(),
            turn,
            phase: phase.to_owned(),
            system: None,
            tools: None,
            messages_delta: None,
            messages: None,
            full: false,
            tool_choice: None,
            reply: None,
            results: None,
            recorded_at: utc_now_rfc3339(),
        }
    }
}

/// An `event` row (format 2): one Event envelope of `--events` (ADR-0010 §2)
/// next to `kind` and `format_version`. `v` is the envelope version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRow {
    /// Always `"event"`.
    pub kind: String,
    pub format_version: u32,
    pub v: u32,
    pub seq: u64,
    pub ts: String,
    pub elapsed_ms: u64,
    pub session_id: String,
    /// Absent on Session-level Events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: serde_json::Value,
}

/// The write side of [`EventRow`]: borrows the emitter's envelope and
/// flattens it, so the file holds exactly the fields `--events` prints.
#[derive(Serialize)]
pub(crate) struct EventLine<'a, E: Serialize> {
    kind: &'static str,
    format_version: u32,
    #[serde(flatten)]
    envelope: &'a E,
}

impl<'a, E: Serialize> EventLine<'a, E> {
    pub(crate) fn new(envelope: &'a E) -> Self {
        Self {
            kind: "event",
            format_version: crate::SESSION_FORMAT_VERSION,
            envelope,
        }
    }
}

/// One decoded JSONL line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRow {
    Header(SessionHeader),
    Event(EventRow),
    Turn(TurnRow),
    Trace(TraceRow),
}

impl SessionRow {
    pub fn kind(&self) -> &str {
        match self {
            Self::Header(_) => "header",
            Self::Event(_) => "event",
            Self::Turn(_) => "turn",
            Self::Trace(_) => "trace",
        }
    }

    pub fn format_version(&self) -> u32 {
        match self {
            Self::Header(header) => header.format_version,
            Self::Event(row) => row.format_version,
            Self::Turn(row) => row.format_version,
            Self::Trace(row) => row.format_version,
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            Self::Header(header) => &header.session_id,
            Self::Event(row) => &row.session_id,
            Self::Turn(row) => &row.session_id,
            Self::Trace(row) => &row.session_id,
        }
    }
}

/// Append one row (a header, event, turn or trace) as a line, creating the file
/// `0600` on unix.
pub fn append_row<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut line = serde_json::to_string(value)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    line.push('\n');
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    // Sessions hold fetched page bodies and the goal: owner-only on creation.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(line.as_bytes())
}

/// Where one run writes its Session, and what a write failure means.
///
/// An explicit `--session-out` is the caller's contract: the first failure
/// is returned and kept in [`SessionSink::failure`] (the Session ends with
/// exit 6 once it finished), and the sink stops writing. The defaulted path
/// under the data root is a courtesy copy: the first failure (or no
/// resolvable data root) prints one warning to stderr, disables the sink,
/// and the run still exits 0. Only the defaulted path creates private (0700)
/// directories and is pruned.
#[derive(Debug)]
pub(crate) struct SessionSink {
    path: Option<PathBuf>,
    explicit: bool,
    warning: Option<String>,
    failure: Option<String>,
}

impl SessionSink {
    pub(crate) fn new(explicit: Option<PathBuf>, session_id: &str) -> Self {
        Self::at(explicit, data_dir::default_session_path(session_id))
    }

    fn at(explicit: Option<PathBuf>, default: Option<PathBuf>) -> Self {
        let is_explicit = explicit.is_some();
        let mut sink = Self {
            path: explicit.or(default),
            explicit: is_explicit,
            warning: None,
            failure: None,
        };
        if sink.path.is_none() {
            sink.warn(
                "no data directory: set WEB_AGENT_RESEARCH_HOME, XDG_DATA_HOME or HOME".to_owned(),
            );
        }
        sink
    }

    fn warn(&mut self, reason: String) {
        let message = format!("warning: Session not saved: {reason}");
        eprintln!("{message}");
        self.warning = Some(message);
    }

    /// The warning printed for a swallowed defaulted-path failure, if any.
    #[cfg(test)]
    pub(crate) fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    /// The first write failure of an explicit path, which the Session turns
    /// into exit 6 when it ends.
    pub(crate) fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Append one row. After a failure the sink is disabled and later calls
    /// are no-ops: a defaulted path warned once, an explicit one kept the
    /// error in [`SessionSink::failure`].
    pub(crate) fn append<T: Serialize>(&mut self, row: &T) -> io::Result<()> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let result = if self.explicit {
            append_row(&path, row)
        } else {
            path.parent()
                .map_or(Ok(()), data_dir::create_private_dir_all)
                .and_then(|()| append_row(&path, row))
        };
        match result {
            Err(err) if !self.explicit => {
                self.path = None;
                self.warn(format!("{}: {err}", path.display()));
                Ok(())
            }
            Err(err) => {
                self.path = None;
                self.failure = Some(err.to_string());
                Err(err)
            }
            ok => ok,
        }
    }

    /// Enforce the retention cap on the data-root sessions directory. No-op
    /// for an explicit path or a disabled sink.
    pub(crate) fn finish(&self) {
        if self.explicit {
            return;
        }
        if let Some(path) = self.path.as_deref() {
            if let Some(dir) = path.parent() {
                data_dir::prune_sessions(dir, path);
            }
        }
    }
}

/// Read every row back, gating on `format_version`.
///
/// Accepts `version <= SESSION_FORMAT_VERSION` (format 1 files hold no
/// `event` rows); rejects `>` with a migration error naming the offending
/// version.
pub fn read_session_rows(path: &Path) -> io::Result<Vec<SessionRow>> {
    let content = std::fs::read_to_string(path)?;
    let mut rows = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let line_no = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid session JSON on line {line_no}: {err}"),
            )
        })?;
        let version = value
            .get("format_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("session JSON on line {line_no} misses format_version"),
                )
            })? as u32;
        if version > crate::SESSION_FORMAT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported session format version {version} (max {}): migrate",
                    crate::SESSION_FORMAT_VERSION
                ),
            ));
        }
        let kind = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("session JSON on line {line_no} misses kind"),
                )
            })?;
        match kind {
            "header" => {
                let header: SessionHeader = serde_json::from_value(value).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid session header on line {line_no}: {err}"),
                    )
                })?;
                rows.push(SessionRow::Header(header));
            }
            "event" => {
                let row: EventRow = serde_json::from_value(value).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid session event on line {line_no}: {err}"),
                    )
                })?;
                rows.push(SessionRow::Event(row));
            }
            "turn" => {
                let row: TurnRow = serde_json::from_value(value).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid session turn on line {line_no}: {err}"),
                    )
                })?;
                rows.push(SessionRow::Turn(row));
            }
            "trace" => {
                let row: TraceRow = serde_json::from_value(value).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid session trace on line {line_no}: {err}"),
                    )
                })?;
                rows.push(SessionRow::Trace(row));
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown session row kind {other:?} on line {line_no}"),
                ));
            }
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "web-agent-research-session-{name}-{}-{nanos}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn header_plus_two_turns_round_trip() {
        let path = temp_path("roundtrip");
        let header = SessionHeader::new(
            "1788825600-1234".to_owned(),
            "test goal".to_owned(),
            "medium".to_owned(),
        );
        let first_evidence =
            Evidence::new("https://example.com/a".to_owned(), "first body".to_owned());
        let second_evidence =
            Evidence::new("https://example.com/b".to_owned(), "second body".to_owned());
        let first = TurnRow::new(
            header.session_id.clone(),
            1,
            vec![first_evidence.clone()],
            None,
            SessionUsage::default(),
        );
        let synthesis = serde_json::json!({
            "size": "medium",
            "summary": "done",
            "themes": [],
            "citations": [],
        });
        let usage = SessionUsage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
            reasoning_tokens: 0,
            cached_prompt_tokens: 0,
            cache_creation_prompt_tokens: 0,
        };
        let second = TurnRow::new(
            header.session_id.clone(),
            2,
            vec![second_evidence.clone()],
            Some(synthesis.clone()),
            usage.clone(),
        );

        append_row(&path, &header).expect("header appends");
        append_row(&path, &first).expect("first turn appends");
        append_row(&path, &second).expect("second turn appends");

        let rows = read_session_rows(&path).expect("rows read back");
        assert_eq!(rows.len(), 3);
        for row in &rows {
            assert_eq!(row.format_version(), crate::SESSION_FORMAT_VERSION);
            assert_eq!(row.session_id(), "1788825600-1234");
        }
        assert_eq!(rows[0].kind(), "header");
        let [SessionRow::Header(back_header), SessionRow::Turn(back_first), SessionRow::Turn(back_second)] =
            rows.as_slice()
        else {
            panic!("expected header + 2 turns, got {rows:?}");
        };
        assert_eq!(back_header, &header);
        assert_eq!(back_first.evidence, vec![first_evidence]);
        assert_eq!(back_first.synthesis, None);
        assert_eq!(back_second.evidence, vec![second_evidence]);
        assert_eq!(back_second.synthesis, Some(synthesis));
        assert_eq!(back_second.usage, usage);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn future_format_version_rejects_with_migration_hint() {
        let path = temp_path("future");
        let next = crate::SESSION_FORMAT_VERSION + 1;
        let line = format!(
            "{{\"kind\":\"turn\",\"format_version\":{next},\"session_id\":\"s\",\"turn\":1,\
             \"evidence\":[],\"synthesis\":null,\
             \"usage\":{{\"prompt_tokens\":0,\"completion_tokens\":0,\"total_tokens\":0,\"reasoning_tokens\":0}},\
             \"recorded_at\":\"2026-09-08T00:00:01Z\"}}\n"
        );
        std::fs::write(&path, line).expect("fixture writes");
        let err = read_session_rows(&path).expect_err("future version must reject");
        let message = err.to_string();
        assert!(
            message.contains(&format!("unsupported session format version {next}")),
            "gate must name the version, got {message}"
        );
        assert!(
            message.contains("migrate"),
            "gate must hint migration, got {message}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pre_41_turn_row_without_cached_prompt_tokens_still_deserializes() {
        // Fixture shape predates `cached_prompt_tokens` (#41): no such key
        // in `usage` at all. `#[serde(default)]` on the new field must let
        // this row keep parsing without a `SESSION_FORMAT_VERSION` bump.
        let path = temp_path("pre-41-usage");
        let line = "{\"kind\":\"turn\",\"format_version\":1,\"session_id\":\"s\",\"turn\":1,\
             \"evidence\":[],\"synthesis\":null,\
             \"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5,\"reasoning_tokens\":1},\
             \"recorded_at\":\"2026-09-08T00:00:01Z\"}\n";
        std::fs::write(&path, line).expect("fixture writes");
        let rows = read_session_rows(&path).expect("pre-#41 row must still parse");
        let [SessionRow::Turn(row)] = rows.as_slice() else {
            panic!("expected exactly one turn row, got {rows:?}");
        };
        assert_eq!(row.usage.prompt_tokens, 3);
        assert_eq!(row.usage.reasoning_tokens, 1);
        assert_eq!(
            row.usage.cached_prompt_tokens, 0,
            "missing cached_prompt_tokens must default to 0, not fail to parse"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// An explicit path that fails mid-Session keeps the first error for the
    /// Session to report as exit 6, and stops writing instead of retrying.
    #[test]
    fn explicit_sink_keeps_its_first_failure_and_stops_writing() {
        let path = temp_path("explicit-fail");
        let mut sink = SessionSink::at(Some(path.clone()), None);
        let header = SessionHeader::new("s".to_owned(), "goal".to_owned(), "small".to_owned());
        sink.append(&header).expect("the first row writes");
        // The file is replaced by a directory: the next open fails.
        std::fs::remove_file(&path).expect("file removes");
        std::fs::create_dir(&path).expect("directory takes its place");
        sink.append(&header)
            .expect_err("an explicit failure is returned");
        let failure = sink.failure().expect("the failure is kept").to_owned();
        sink.append(&header).expect("a failed sink is a no-op");
        assert_eq!(sink.failure(), Some(failure.as_str()));
        assert_eq!(sink.warning(), None, "an explicit path never only warns");
        let _ = std::fs::remove_dir(&path);
    }
}
