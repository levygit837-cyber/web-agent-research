//! Run the CLI once per goal and repeat, in one of three modes, and turn
//! each run into a [`RunRow`].
//!
//! - `record`: live search, fetch and gateway. The tools are taped into
//!   `<fixtures>/<goal id>/` (`WAR_EVAL_RECORD`), every gateway exchange
//!   into `gateway.r<k>.jsonl` there through a [`RecordingProxy`], and the
//!   run itself (goal, flags, [`Metrics`]) into `run.r<k>.json`, which the
//!   offline gate replays and asserts.
//! - `replay`: tools served from `<fixtures>/<goal id>/`
//!   (`WAR_EVAL_REPLAY`), live gateway: measures a change to the agent
//!   (prompt, loop, model) on fixed web content.
//! - `live`: no tape at all.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::golden::GoldenGoal;
use super::metrics::{answer_text, last_error_line, Metrics, RunRow};
use super::transcript::{self, RecordingProxy};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Record,
    Replay,
    Live,
}

impl std::str::FromStr for Mode {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, String> {
        match raw {
            "record" => Ok(Self::Record),
            "replay" => Ok(Self::Replay),
            "live" => Ok(Self::Live),
            other => Err(format!("unknown mode {other:?} (record, replay, live)")),
        }
    }
}

/// Everything one run needs besides its goal.
#[derive(Debug, Clone)]
pub struct RunPlan {
    pub mode: Mode,
    /// The `web-agent-research` binary.
    pub bin: PathBuf,
    /// Fixture root: one dir per goal id (unused in `live`).
    pub fixtures: PathBuf,
    /// Extra `research` flags, e.g. `--size small`.
    pub extra_args: Vec<String>,
}

/// The CLI's outcome for one run.
pub struct Output {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub wall: Duration,
}

/// One `research` invocation of `bin` with `env` added to the current
/// environment, awaited without blocking the runtime (the recording proxy
/// runs on it).
pub async fn invoke(
    bin: &Path,
    goal: &str,
    extra_args: &[String],
    env: &[(String, String)],
) -> Output {
    let started = Instant::now();
    let mut command = tokio::process::Command::new(bin);
    command
        .arg("research")
        .arg(goal)
        .arg("--json")
        .args(extra_args)
        .env_remove("WAR_EVAL_RECORD")
        .env_remove("WAR_EVAL_REPLAY");
    for (key, value) in env {
        command.env(key, value);
    }
    match command.output().await {
        Ok(output) => Output {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            wall: started.elapsed(),
        },
        Err(err) => Output {
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("error: cannot run {}: {err}", bin.display()),
            wall: started.elapsed(),
        },
    }
}

/// The row of one finished run.
pub fn row(goal: &GoldenGoal, repeat: u32, output: &Output) -> Result<RunRow, String> {
    let metrics = Metrics::from_output(output.exit_code, &output.stdout, &output.stderr)?;
    let answer = if output.exit_code == 0 {
        Some(answer_text(&output.stdout)?)
    } else {
        None
    };
    Ok(RunRow {
        goal_id: goal.goal.id.clone(),
        category: goal.category.clone(),
        repeat,
        metrics,
        wall_ms: u64::try_from(output.wall.as_millis()).unwrap_or(u64::MAX),
        answer,
        error: (output.exit_code != 0)
            .then(|| last_error_line(&output.stderr))
            .flatten(),
        judge: None,
    })
}

/// What a `record` run stores next to its transcript: enough to rerun it
/// offline (the replayed responses are in one wire format, for one model)
/// and the metrics the rerun must reproduce.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedRun {
    pub goal: String,
    pub extra_args: Vec<String>,
    /// `GATEWAY_API_FORMAT` of the recording.
    pub api_format: String,
    /// `GATEWAY_MODEL` of the recording.
    pub agent_model: String,
    pub metrics: Metrics,
}

/// `key` from the env, trimmed; empty when unset.
pub fn env_value(key: &str) -> String {
    std::env::var(key)
        .map(|v| v.trim().to_owned())
        .unwrap_or_default()
}

/// File of repeat `k`'s [`RecordedRun`].
pub fn recorded_run_file_name(repeat: u32) -> String {
    format!("run.r{repeat}.json")
}

/// Run repeat `repeat` of `goal` once in `plan.mode`. Errors only on a
/// setup failure, never on a failed run: a non-zero exit is a row.
pub async fn run_one(plan: &RunPlan, goal: &GoldenGoal, repeat: u32) -> Result<RunRow, String> {
    let dir = plan.fixtures.join(&goal.goal.id);
    let mut env = Vec::new();
    let mut proxy = None;
    match plan.mode {
        Mode::Record => {
            std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
            let started = RecordingProxy::start(
                &env_value("GATEWAY_BASE_URL"),
                dir.join(transcript::file_name(repeat)),
            )
            .await?;
            env.push(("GATEWAY_BASE_URL".to_owned(), started.base_url.clone()));
            env.push(("WAR_EVAL_RECORD".to_owned(), dir.display().to_string()));
            proxy = Some(started);
        }
        Mode::Replay => {
            if !dir.is_dir() {
                return Err(format!("{}: no fixture for this goal", dir.display()));
            }
            env.push(("WAR_EVAL_REPLAY".to_owned(), dir.display().to_string()));
        }
        Mode::Live => {}
    }
    eprintln!(
        "eval {:?} {} r{repeat}: {}",
        plan.mode, goal.goal.id, goal.goal.goal
    );
    let output = invoke(&plan.bin, &goal.goal.goal, &plan.extra_args, &env).await;
    drop(proxy);
    let row = row(goal, repeat, &output)?;
    if plan.mode == Mode::Record {
        let path = dir.join(recorded_run_file_name(repeat));
        let recorded = RecordedRun {
            goal: goal.goal.goal.clone(),
            extra_args: plan.extra_args.clone(),
            api_format: env_value("GATEWAY_API_FORMAT"),
            agent_model: env_value("GATEWAY_MODEL"),
            metrics: row.metrics.clone(),
        };
        let text = serde_json::to_string_pretty(&recorded).map_err(|e| e.to_string())?;
        std::fs::write(&path, text + "\n").map_err(|err| format!("{}: {err}", path.display()))?;
    }
    eprintln!(
        "  exit {} turns {:?} citations {:?} misses {} in {} ms{}",
        row.metrics.exit_code,
        row.metrics.turns_used,
        row.metrics.citations,
        row.metrics.replay_misses,
        row.wall_ms,
        row.error
            .as_deref()
            .map(|e| format!(" ({e})"))
            .unwrap_or_default()
    );
    Ok(row)
}
