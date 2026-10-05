//! Session persistence: Session header and Turn rows as JSONL.
//!
//! Evidence itself is owned by `web::fetch`; this module only records it.
//!
//! Record schema (ADR-0002): one JSON object per line in
//! `<data_root>/sessions/<id>.jsonl` (or the explicit `--session-out`), UTF-8,
//! `\n`-terminated, append-only, created with mode 0600 on unix.
//! `format_version` rides on EVERY row so a single row stays self-describing
//! and 1:1 migratable to the future `turns` table (`turn→id`,
//! `session_id→session_id`, `evidence[].source_url/collected_at→url/fetched_at`).

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

/// One decoded JSONL line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRow {
    Header(SessionHeader),
    Turn(TurnRow),
}

impl SessionRow {
    pub fn kind(&self) -> &str {
        match self {
            Self::Header(_) => "header",
            Self::Turn(_) => "turn",
        }
    }

    pub fn format_version(&self) -> u32 {
        match self {
            Self::Header(header) => header.format_version,
            Self::Turn(row) => row.format_version,
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            Self::Header(header) => &header.session_id,
            Self::Turn(row) => &row.session_id,
        }
    }
}

fn append_line<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
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

/// Append the session header (line 0).
pub fn append_header(path: &Path, header: &SessionHeader) -> io::Result<()> {
    append_line(path, header)
}

/// Append one turn row.
pub fn append_turn(path: &Path, row: &TurnRow) -> io::Result<()> {
    append_line(path, row)
}

/// Where one run writes its Session, and what a write failure means.
///
/// An explicit `--session-out` is the caller's contract: any failure is
/// returned (exit 6). The defaulted path under the data root is a courtesy
/// copy: the first failure (or no resolvable data root) prints one warning
/// to stderr, disables the sink, and the run still exits 0. Only the
/// defaulted path creates private (0700) directories and is pruned.
#[derive(Debug)]
pub(crate) struct SessionSink {
    path: Option<PathBuf>,
    explicit: bool,
    warning: Option<String>,
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

    fn write(&mut self, write: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<()> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let result = if self.explicit {
            write(&path)
        } else {
            path.parent()
                .map_or(Ok(()), data_dir::create_private_dir_all)
                .and_then(|()| write(&path))
        };
        match result {
            Err(err) if !self.explicit => {
                self.path = None;
                self.warn(format!("{}: {err}", path.display()));
                Ok(())
            }
            other => other,
        }
    }

    pub(crate) fn append_header(&mut self, header: &SessionHeader) -> io::Result<()> {
        self.write(|path| append_header(path, header))
    }

    pub(crate) fn append_turn(&mut self, row: &TurnRow) -> io::Result<()> {
        self.write(|path| append_turn(path, row))
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
/// Accepts `version <= SESSION_FORMAT_VERSION`; rejects `>` with a migration
/// error naming the offending version.
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
            "turn" => {
                let row: TurnRow = serde_json::from_value(value).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid session turn on line {line_no}: {err}"),
                    )
                })?;
                rows.push(SessionRow::Turn(row));
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

        append_header(&path, &header).expect("header appends");
        append_turn(&path, &first).expect("first turn appends");
        append_turn(&path, &second).expect("second turn appends");

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
        let line = format!(
            "{{\"kind\":\"turn\",\"format_version\":{},\"session_id\":\"s\",\"turn\":1,\
             \"evidence\":[],\"synthesis\":null,\
             \"usage\":{{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5,\"reasoning_tokens\":1}},\
             \"recorded_at\":\"2026-09-08T00:00:01Z\"}}\n",
            crate::SESSION_FORMAT_VERSION
        );
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
}
