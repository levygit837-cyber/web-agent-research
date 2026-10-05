//! Cache root for persisted search state (ADR-0002: files, no DB).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// `$SEARCH_CACHE_DIR`, else `$XDG_CACHE_HOME/web-agent-research`, else
/// `$HOME/.cache/web-agent-research`. `None` when none is set.
pub(crate) fn cache_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SEARCH_CACHE_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir).join("web-agent-research"));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".cache")
                .join("web-agent-research")
        })
}

/// Process-wide counter so two concurrent `write_atomic` calls (e.g. two
/// fan-out legs whose normalized query/provider/recency/page collide onto
/// the same cache key) never share a temp filename: with a fixed
/// `tmp-<pid>` name, one writer's `std::fs::write` could interleave with
/// another's on the same path, and a `rename` racing a third could publish
/// a torn file. `fetch_add` gives each call a distinct, monotonically
/// increasing suffix within this process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Write via a sibling temp file + rename so readers never see a torn file.
/// On any write or rename failure the temp file is removed before the
/// error returns (#103), so a full disk or a read-only target never leaves
/// `*.tmp-<pid>-<n>` litter behind; a temp orphaned by a killed process is
/// evicted later by the search cache's size pass.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!(
        "{TEMP_EXTENSION_PREFIX}{}-{counter}",
        std::process::id()
    ));
    let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Extension prefix of [`write_atomic`]'s temp files (`<stem>.tmp-<pid>-<n>`).
pub(crate) const TEMP_EXTENSION_PREFIX: &str = "tmp-";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_rename_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("war-write-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // The target is a non-empty directory, so `rename` onto it fails
        // after the temp file was fully written.
        let target = dir.join("entry.json");
        std::fs::create_dir_all(target.join("occupied")).expect("mkdir");
        assert!(write_atomic(&target, b"{}").is_err());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .flatten()
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().contains(TEMP_EXTENSION_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
