//! Session persistence: Session, Turn and Evidence + JSONL append.
//!
//! Loose file until `jsonl.rs` / `store.rs` exist; becomes a folder then.
//! All Modes consume it via `shared/`.
//!
//! Record schema (CONTRACT §3, ADR-0002): one JSON object per line in
//! `sessions/<id>.jsonl`, UTF-8, `\n`-terminated, append-only.
//! `format_version` rides on EVERY row so a single row stays self-describing
//! and 1:1 migratable to the future `turns` table (`turn→id`,
//! `session_id→session_id`, `evidence[].source_url/collected_at→url/fetched_at`).

use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Web-extracted content with source URL and collection time, used in synthesis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Normalized source URL (from `FetchedMarkdown::url`, never the raw input).
    pub source_url: String,
    /// Collection time, RFC 3339 UTC (stamped by `new` from `SystemTime`; no chrono).
    pub collected_at: String,
    /// Trimmed markdown body (from `FetchedMarkdown::markdown`, stored verbatim).
    pub markdown: String,
}

impl Evidence {
    /// Stamp `collected_at` with the current UTC time.
    pub fn new(source_url: String, markdown: String) -> Self {
        Self {
            source_url,
            collected_at: utc_now_rfc3339(),
            markdown,
        }
    }
}

/// Line 0 of `sessions/<id>.jsonl` (SHOULD exist): recalls goal/size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    /// Always `"header"`.
    pub kind: String,
    pub format_version: u32,
    pub session_id: String,
    pub goal: String,
    /// `"small"|"medium"|"large"` (plain string keeps `shared/` free of
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
}

/// Lines 1..N of `sessions/<id>.jsonl`: one per turn, `synthesis` null until
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
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
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

/// Current UTC time as `YYYY-MM-DDTHH:MM:SSZ` (second precision, std only).
fn utc_now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_timestamp(secs)
}

/// Unix seconds → RFC 3339 UTC via Howard Hinnant's civil-from-days algorithm.
fn format_timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let secs_of_day = (secs % 86_400) as i64;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_timestamps_format_as_rfc3339_utc() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_timestamp(1_788_825_600), "2026-09-08T00:00:00Z");
        assert_eq!(format_timestamp(951_782_400), "2000-02-29T00:00:00Z");
    }

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
}
