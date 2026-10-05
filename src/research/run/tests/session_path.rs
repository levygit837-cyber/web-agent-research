//! Where the Session lands (#102): data-root default, best-effort when
//! defaulted, strict when explicit, and the `--session-id` gate.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::super::*;
use super::helpers::{local_web, point_env_at, spawn_server, text_body};
use crate::research::dto::ResearchRequest;
use crate::research::session::{read_session_rows, SessionRow, SessionSink};
use crate::research::synthesis::SynthesisSize;
use crate::test_support::EnvGuard;

const ENV_KEYS: [&str; 5] = [
    "GATEWAY_BASE_URL",
    "GATEWAY_API_KEY",
    "GATEWAY_MODEL",
    "WEB_AGENT_RESEARCH_HOME",
    "XDG_DATA_HOME",
];

fn request(session_id: Option<&str>, session_out: Option<PathBuf>) -> ResearchRequest {
    ResearchRequest {
        goal: "What is the Obscura headless browser?".to_owned(),
        size: SynthesisSize::Small,
        max_turns: 8,
        max_turns_cap: 25,
        deadline_secs: 300,
        session_id: session_id.map(str::to_owned),
        session_out,
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "war-session-path-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A path whose parent is a regular file: `create_dir_all` and `open` both fail.
fn unwritable_path(base: &std::path::Path) -> PathBuf {
    let blocker = base.join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("blocker writes");
    blocker.join("deeper").join("session.jsonl")
}

fn final_answer() -> String {
    text_body("Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust.\n")
}

#[tokio::test]
async fn default_session_lands_in_the_data_root_with_private_modes() {
    let _guard = EnvGuard::lock(ENV_KEYS.to_vec());
    let root = temp_dir("default");
    std::env::set_var("WEB_AGENT_RESEARCH_HOME", &root);
    let (tools, _page_url) = local_web().await;
    point_env_at(&spawn_server(vec![final_answer()]));

    let response = run_research_with(request(Some("my-session.1"), None), tools)
        .await
        .expect("run must succeed");

    let path = root.join("sessions").join("my-session.1.jsonl");
    let rows = read_session_rows(&path).expect("session is under the data root");
    assert!(matches!(rows.first(), Some(SessionRow::Header(_))));
    assert_eq!(response.session_id, "my-session.1");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600, "session file is owner-only");
        assert_eq!(
            mode(&root.join("sessions")),
            0o700,
            "sessions dir is private"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn defaulted_write_failure_warns_and_still_succeeds() {
    let _guard = EnvGuard::lock(ENV_KEYS.to_vec());
    let base = temp_dir("default-fail");
    // The data root sits under a regular file, so no directory can be made.
    let blocker = base.join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("blocker writes");
    std::env::set_var("WEB_AGENT_RESEARCH_HOME", blocker.join("home"));
    let (tools, _page_url) = local_web().await;
    point_env_at(&spawn_server(vec![final_answer()]));

    let response = run_research_with(request(Some("defaulted"), None), tools)
        .await
        .expect("a defaulted Session write failure must not fail the run");
    assert_eq!(response.session_id, "defaulted");
    assert!(!blocker.join("home").exists());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn defaulted_sink_failure_sets_one_warning_and_disables_itself() {
    let _guard = EnvGuard::lock(ENV_KEYS.to_vec());
    let base = temp_dir("sink-warn");
    let blocker = base.join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("blocker writes");
    std::env::set_var("WEB_AGENT_RESEARCH_HOME", blocker.join("home"));
    let mut sink = SessionSink::new(None, "abc");
    let header = SessionHeader::new("abc".to_owned(), "goal".to_owned(), "small".to_owned());
    sink.append_header(&header)
        .expect("defaulted failure is swallowed");
    let warning = sink.warning().expect("a warning was recorded").to_owned();
    assert!(
        warning.starts_with("warning: Session not saved:"),
        "{warning}"
    );
    sink.append_header(&header)
        .expect("disabled sink is a no-op");
    assert_eq!(sink.warning(), Some(warning.as_str()), "warned once");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn missing_data_root_warns_instead_of_failing() {
    let _guard = EnvGuard::lock(vec!["WEB_AGENT_RESEARCH_HOME", "XDG_DATA_HOME", "HOME"]);
    let home = std::env::var_os("HOME");
    std::env::remove_var("HOME");
    let mut sink = SessionSink::new(None, "abc");
    if let Some(home) = home {
        std::env::set_var("HOME", home);
    }
    let header = SessionHeader::new("abc".to_owned(), "goal".to_owned(), "small".to_owned());
    sink.append_header(&header)
        .expect("no root is not an error");
    assert!(sink
        .warning()
        .expect("warned")
        .contains("no data directory"));
}

#[tokio::test]
async fn explicit_session_out_failure_is_io() {
    let _guard = EnvGuard::lock(ENV_KEYS.to_vec());
    let base = temp_dir("explicit-fail");
    std::env::set_var("WEB_AGENT_RESEARCH_HOME", base.join("root"));
    let (tools, _page_url) = local_web().await;
    point_env_at(&spawn_server(vec![final_answer()]));

    let err = run_research_with(
        request(Some("explicit"), Some(unwritable_path(&base))),
        tools,
    )
    .await
    .expect_err("an explicit --session-out failure must surface");
    assert!(matches!(err, ResearchError::Io(_)), "got {err:?}");
    assert!(
        !base.join("root").exists(),
        "an explicit path must not also write under the data root"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn invalid_session_ids_are_not_configured_before_any_io() {
    let _guard = EnvGuard::lock(ENV_KEYS.to_vec());
    let root = temp_dir("bad-id");
    std::env::set_var("WEB_AGENT_RESEARCH_HOME", &root);
    std::env::set_var("GATEWAY_API_KEY", "dummy");
    let too_long = "a".repeat(65);
    for id in ["../x", "a/b", ".hidden", "", "has space", too_long.as_str()] {
        let (tools, _page_url) = local_web().await;
        let err = run_research_with(request(Some(id), None), tools)
            .await
            .expect_err("invalid id must fail");
        assert!(
            matches!(err, ResearchError::NotConfigured(_)),
            "{id:?} must be NotConfigured, got {err:?}"
        );
    }
    assert!(
        std::fs::read_dir(&root).unwrap().next().is_none(),
        "rejected ids must not touch the data root"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn default_run_prunes_the_oldest_sessions_over_the_cap() {
    let _guard = EnvGuard::lock({
        let mut keys = ENV_KEYS.to_vec();
        keys.push("SESSION_MAX_BYTES");
        keys
    });
    let root = temp_dir("prune");
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let old = sessions.join("old.jsonl");
    std::fs::write(&old, vec![b'x'; 4096]).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&old)
        .and_then(|f| f.set_modified(SystemTime::now() - std::time::Duration::from_secs(3600)))
        .unwrap();
    std::env::set_var("WEB_AGENT_RESEARCH_HOME", &root);
    std::env::set_var("SESSION_MAX_BYTES", "4096");
    let (tools, _page_url) = local_web().await;
    point_env_at(&spawn_server(vec![final_answer()]));

    run_research_with(request(Some("fresh"), None), tools)
        .await
        .expect("run must succeed");

    assert!(!old.exists(), "the oldest Session is evicted over the cap");
    assert!(
        sessions.join("fresh.jsonl").exists(),
        "the new Session stays"
    );
    let _ = std::fs::remove_dir_all(&root);
}
