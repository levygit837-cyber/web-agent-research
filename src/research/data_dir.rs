//! Per-user data root: where this tool keeps state the user may want back.
//!
//! Today that is the Session JSONL files under `<root>/sessions/`. The
//! search cache and `engines.json` are disposable and live under the cache
//! root instead (`web::cache_dir`); the two roots are resolved separately.
//!
//! Also owns the rules that keep the Sessions directory safe to write into
//! from an arbitrary cwd: id validation (no path escapes), private
//! permissions, and a size cap that evicts the oldest files.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const APP_DIR: &str = "web-agent-research";
const SESSIONS_SUBDIR: &str = "sessions";
const SESSION_ID_MAX_LEN: usize = 64;
/// Default size cap for `<data_root>/sessions/`: 256 MiB, oldest first.
const DEFAULT_MAX_BYTES: u64 = 256 * 1_024 * 1_024;

/// Pure resolution order, so tests need no process env:
/// `WEB_AGENT_RESEARCH_HOME`, else `$XDG_DATA_HOME/web-agent-research`, else
/// `$HOME/.local/share/web-agent-research`. Empty values count as unset.
pub(crate) fn resolve_root(
    home_override: Option<OsString>,
    xdg_data_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    let set = |v: Option<OsString>| v.filter(|v| !v.is_empty());
    if let Some(dir) = set(home_override) {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = set(xdg_data_home) {
        return Some(PathBuf::from(dir).join(APP_DIR));
    }
    set(home).map(|home| {
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join(APP_DIR)
    })
}

/// The data root from the process environment; `None` when none of the
/// three variables is set.
pub(crate) fn data_root() -> Option<PathBuf> {
    resolve_root(
        std::env::var_os("WEB_AGENT_RESEARCH_HOME"),
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
    )
}

/// `<data_root>/sessions/<id>.jsonl`. `id` MUST already be validated.
pub(crate) fn default_session_path(session_id: &str) -> Option<PathBuf> {
    data_root().map(|root| {
        root.join(SESSIONS_SUBDIR)
            .join(format!("{session_id}.jsonl"))
    })
}

/// `[A-Za-z0-9._-]{1,64}` with no leading dot: the id becomes a file name
/// (`../x` must not escape) and the `prompt_cache_key`.
pub(crate) fn validate_session_id(id: &str) -> Result<(), String> {
    let valid_chars = id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if id.is_empty() || id.len() > SESSION_ID_MAX_LEN || id.starts_with('.') || !valid_chars {
        return Err(format!(
            "invalid --session-id {id:?}: use 1-{SESSION_ID_MAX_LEN} characters from \
             [A-Za-z0-9._-], not starting with a dot"
        ));
    }
    Ok(())
}

/// `create_dir_all` with mode 0700 on unix for the directories it creates.
pub(crate) fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// `SESSION_MAX_BYTES` overrides the cap; an invalid value falls back to the
/// default (the run already succeeded, so a bad override must not fail it).
fn max_bytes() -> u64 {
    std::env::var("SESSION_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_BYTES)
}

/// Delete the oldest `*.jsonl` files (by mtime, then name) until the
/// directory fits the cap. `keep` is the Session just written and is never
/// evicted, even when it alone exceeds the cap. Best effort: I/O errors
/// leave the file in place.
pub(crate) fn prune_sessions(dir: &Path, keep: &Path) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, PathBuf, u64)> = read
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|entry| {
            let meta = entry.metadata().ok().filter(std::fs::Metadata::is_file)?;
            Some((meta.modified().ok()?, entry.path(), meta.len()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, _, len)| len).sum();
    let cap = max_bytes();
    files.sort();
    for (_, path, len) in files {
        if total <= cap {
            break;
        }
        if path.file_name() == keep.file_name() {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EnvGuard;
    use std::time::Duration;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn root_prefers_override_then_xdg_then_home() {
        assert_eq!(
            resolve_root(os("/own"), os("/xdg"), os("/home/u")),
            Some(PathBuf::from("/own"))
        );
        assert_eq!(
            resolve_root(None, os("/xdg"), os("/home/u")),
            Some(PathBuf::from("/xdg/web-agent-research"))
        );
        assert_eq!(
            resolve_root(None, None, os("/home/u")),
            Some(PathBuf::from("/home/u/.local/share/web-agent-research"))
        );
        assert_eq!(resolve_root(None, None, None), None);
    }

    #[test]
    fn empty_values_count_as_unset() {
        assert_eq!(
            resolve_root(os(""), os(""), os("/home/u")),
            Some(PathBuf::from("/home/u/.local/share/web-agent-research"))
        );
        assert_eq!(resolve_root(os(""), os(""), os("")), None);
    }

    #[test]
    fn root_and_session_path_follow_the_environment() {
        let _guard = EnvGuard::lock(vec!["WEB_AGENT_RESEARCH_HOME", "XDG_DATA_HOME"]);
        std::env::set_var("WEB_AGENT_RESEARCH_HOME", "/tmp/war-root");
        assert_eq!(data_root(), Some(PathBuf::from("/tmp/war-root")));
        assert_eq!(
            default_session_path("abc"),
            Some(PathBuf::from("/tmp/war-root/sessions/abc.jsonl"))
        );
        std::env::remove_var("WEB_AGENT_RESEARCH_HOME");
        std::env::set_var("XDG_DATA_HOME", "/tmp/xdg");
        assert_eq!(
            data_root(),
            Some(PathBuf::from("/tmp/xdg/web-agent-research"))
        );
    }

    #[test]
    fn session_ids_accept_the_documented_alphabet() {
        for id in ["1788825600-1234", "a.b_c-D", "x", &"a".repeat(64)] {
            assert!(validate_session_id(id).is_ok(), "{id} must be valid");
        }
    }

    #[test]
    fn session_ids_reject_escapes_and_bad_shapes() {
        let too_long = "a".repeat(65);
        for id in [
            "",
            "../x",
            "a/b",
            "a\\b",
            ".hidden",
            "..",
            ".",
            "a b",
            "a\0b",
            "é",
            too_long.as_str(),
        ] {
            assert!(validate_session_id(id).is_err(), "{id:?} must be rejected");
        }
    }

    fn write_aged(dir: &Path, name: &str, bytes: usize, age_secs: u64) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, vec![b'x'; bytes]).expect("fixture writes");
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(when))
            .expect("mtime set");
        path
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "war-data-dir-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn retention_evicts_the_oldest_first_and_keeps_the_current_file() {
        let _guard = EnvGuard::lock(vec!["SESSION_MAX_BYTES"]);
        let dir = temp_dir("evict");
        let oldest = write_aged(&dir, "a.jsonl", 100, 300);
        let middle = write_aged(&dir, "b.jsonl", 100, 200);
        let newer = write_aged(&dir, "c.jsonl", 100, 100);
        let current = write_aged(&dir, "d.jsonl", 100, 0);
        let other = write_aged(&dir, "notes.txt", 100, 400);
        std::env::set_var("SESSION_MAX_BYTES", "250");
        prune_sessions(&dir, &current);
        assert!(!oldest.exists(), "oldest must go first");
        assert!(!middle.exists(), "then the next oldest");
        assert!(newer.exists() && current.exists(), "cap fits two files");
        assert!(other.exists(), "non-session files are never touched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_never_evicts_the_current_file_even_over_the_cap() {
        let _guard = EnvGuard::lock(vec!["SESSION_MAX_BYTES"]);
        let dir = temp_dir("keep");
        let old = write_aged(&dir, "old.jsonl", 10, 100);
        let current = write_aged(&dir, "cur.jsonl", 500, 0);
        std::env::set_var("SESSION_MAX_BYTES", "0");
        prune_sessions(&dir, &current);
        assert!(!old.exists());
        assert!(current.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_cap_falls_back_to_the_default() {
        let _guard = EnvGuard::lock(vec!["SESSION_MAX_BYTES"]);
        std::env::set_var("SESSION_MAX_BYTES", "lots");
        assert_eq!(max_bytes(), DEFAULT_MAX_BYTES);
        std::env::set_var("SESSION_MAX_BYTES", " 42 ");
        assert_eq!(max_bytes(), 42);
    }

    #[cfg(unix)]
    #[test]
    fn private_dirs_are_mode_0700() {
        use std::os::unix::fs::PermissionsExt;
        let base = temp_dir("mode");
        let nested = base.join("a").join("sessions");
        create_private_dir_all(&nested).expect("dirs created");
        let mode = std::fs::metadata(&nested)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(&base);
    }
}
