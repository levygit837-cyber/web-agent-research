//! Offline eval gate (#107): no network.
//!
//! - The golden set is well-formed: 7 categories × 8 goals, 3-6 nuggets
//!   each, unique ids, dated nuggets with http(s) URLs.
//! - Every committed fixture (`tests/eval_fixtures/<goal id>/`) replays:
//!   the CLI runs with its tools served from the fixture
//!   (`WAR_EVAL_REPLAY`) against a stub gateway that answers with the
//!   recorded transcript, and the metrics computed on the replayed run
//!   equal the ones recorded live. At least one goal per category has a
//!   fixture.

#[allow(dead_code)]
mod eval;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use eval::golden::{self, CATEGORIES};
use eval::metrics::Metrics;
use eval::runner::{recorded_run_file_name, RecordedRun};
use eval::transcript::{self, ReplayStub};

#[test]
fn the_golden_set_has_eight_goals_per_category_with_three_to_six_nuggets() {
    let goals = golden::load(&golden::default_dir()).expect("golden set loads");
    let mut per_category: HashMap<&str, usize> = HashMap::new();
    let mut ids = HashSet::new();
    for goal in &goals {
        *per_category.entry(goal.category.as_str()).or_default() += 1;
        let id = &goal.goal.id;
        assert!(ids.insert(id.clone()), "duplicate goal id {id}");
        assert!(
            id.starts_with(&goal.category),
            "{id} outside {}",
            goal.category
        );
        assert!(!goal.goal.goal.trim().is_empty(), "{id}: empty goal");
        let nuggets = &goal.goal.nuggets;
        assert!(
            (3..=6).contains(&nuggets.len()),
            "{id}: {} nuggets",
            nuggets.len()
        );
        for nugget in nuggets {
            assert!(!nugget.text.trim().is_empty(), "{id}: empty nugget");
            assert!(
                nugget.url.starts_with("https://") || nugget.url.starts_with("http://"),
                "{id}: nugget url {}",
                nugget.url
            );
            let date = nugget.verified_on.as_bytes();
            let shaped = date.len() == 10
                && date[4] == b'-'
                && date[7] == b'-'
                && date
                    .iter()
                    .enumerate()
                    .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
            assert!(shaped, "{id}: verified_on {:?}", nugget.verified_on);
        }
    }
    for category in CATEGORIES {
        assert_eq!(per_category.get(category), Some(&8), "{category}");
    }
    assert_eq!(goals.len(), 56);
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("war-eval-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Replay recording `repeat` of the unpacked fixture `dir` and return the
/// replayed metrics, with the stub's counters checked.
fn replay(dir: &Path, repeat: u32, home: &Path) -> (RecordedRun, Metrics) {
    let recorded: RecordedRun = serde_json::from_str(
        &std::fs::read_to_string(dir.join(recorded_run_file_name(repeat))).expect("run file"),
    )
    .expect("run file parses");
    let exchanges =
        transcript::read(&dir.join(transcript::file_name(repeat))).expect("transcript reads");
    assert!(!exchanges.is_empty(), "{}: empty transcript", dir.display());
    let expected_calls = exchanges.len();
    let stub = ReplayStub::spawn(exchanges);
    // A clean env: only what the recording pinned, so a developer's
    // GATEWAY_* or SEARCH_* settings cannot change the replay.
    let output = Command::new(env!("CARGO_BIN_EXE_web-agent-research"))
        .arg("research")
        .arg(&recorded.goal)
        .arg("--json")
        .args(&recorded.extra_args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("WEB_AGENT_RESEARCH_HOME", home)
        .env("SEARCH_CACHE", "off")
        .env("WAR_EVAL_REPLAY", dir)
        .env("GATEWAY_BASE_URL", &stub.base_url)
        .env("GATEWAY_API_KEY", "offline-replay")
        .env("GATEWAY_API_FORMAT", &recorded.api_format)
        .env("GATEWAY_MODEL", &recorded.agent_model)
        .output()
        .expect("CLI runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);
    let metrics = Metrics::from_output(exit_code, &stdout, &stderr)
        .unwrap_or_else(|err| panic!("{}: {err}\nstderr:\n{stderr}", dir.display()));
    assert_eq!(
        stub.served(),
        expected_calls,
        "{} r{repeat}: replay made a different number of gateway calls\nstderr:\n{stderr}",
        dir.display()
    );
    assert_eq!(stub.path_mismatches(), 0, "{} r{repeat}", dir.display());
    (recorded, metrics)
}

#[test]
fn committed_fixtures_replay_to_the_recorded_metrics_in_every_category() {
    let committed = eval::pack::committed_dir();
    let goals = golden::load(&golden::default_dir()).expect("golden set loads");
    let category_of: HashMap<&str, &str> = goals
        .iter()
        .map(|g| (g.goal.id.as_str(), g.category.as_str()))
        .collect();
    let mut fixture_ids: Vec<String> = std::fs::read_dir(&committed)
        .expect("tests/eval_fixtures exists")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    fixture_ids.sort();
    let base = temp_dir("replay");
    let home = base.join("home");
    let mut covered = HashSet::new();
    let mut replayed = 0;
    for id in &fixture_ids {
        let category = category_of
            .get(id.as_str())
            .unwrap_or_else(|| panic!("fixture {id} is not a golden goal"));
        let dir = base.join(id);
        eval::pack::unpack(&committed.join(id), &dir).expect("fixture unpacks");
        let mut repeat = 1;
        while dir.join(recorded_run_file_name(repeat)).is_file() {
            let (recorded, metrics) = replay(&dir, repeat, &home);
            assert_eq!(metrics, recorded.metrics, "{id} r{repeat}");
            assert_eq!(metrics.replay_misses, 0, "{id} r{repeat}");
            if metrics.exit_code == 0 {
                covered.insert(*category);
            }
            replayed += 1;
            repeat += 1;
        }
        assert!(repeat > 1, "fixture {id} has no run.r1.json");
    }
    std::fs::remove_dir_all(&base).ok();
    for category in CATEGORIES {
        assert!(
            covered.contains(category),
            "no answered fixture for category {category}"
        );
    }
    assert!(replayed >= CATEGORIES.len());
}
