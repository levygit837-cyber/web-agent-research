//! Session persistence: Session, Turn and Evidence + JSONL append.
//!
//! Loose file until `jsonl.rs` / `store.rs` exist; becomes a folder then.
//! All Modes consume it via `shared/`.

use serde::{Deserialize, Serialize};
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
    use super::format_timestamp;

    #[test]
    fn known_timestamps_format_as_rfc3339_utc() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_timestamp(1_788_825_600), "2026-09-08T00:00:00Z");
    }
}
