//! Cache root for persisted search state (ADR-0002: files, no DB).

use std::path::{Path, PathBuf};

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

/// Write via a sibling temp file + rename so readers never see a torn file.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
