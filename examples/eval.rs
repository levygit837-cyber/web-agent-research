//! Offline eval driver (#107). Eval-only: an example, never part of the
//! shipped binary. See `docs/eval.md`.
//!
//! ```text
//! cargo build --release
//! cargo run --example eval -- run --mode record --fixtures /tmp/war-fix --out base.json
//! cargo run --example eval -- judge base.json
//! cargo run --example eval -- summary base.json
//! cargo run --example eval -- compare a.json b.json
//! cargo run --example eval -- pack /tmp/war-fix crate_docs-01 how_to-03
//! ```

#[path = "../tests/eval/mod.rs"]
#[allow(dead_code)]
mod eval;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};

use eval::golden;
use eval::judge::{Judge, Judgement, DEFAULT_JUDGE_MODEL};
use eval::results::{summary_table, ResultsFile, FORMAT};
use eval::runner::{env_value, run_one, Mode, RunPlan};

#[derive(Parser)]
#[command(about = "Offline eval of web-agent-research (eval-only, see docs/eval.md)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the golden set; resumes `--out` when it exists.
    Run {
        /// record (live, taped), replay (taped tools, live gateway) or live.
        #[arg(long)]
        mode: Mode,
        /// Fixture root, one dir per goal id (record/replay).
        #[arg(long, default_value = "target/eval/fixtures")]
        fixtures: PathBuf,
        /// Repeats per goal.
        #[arg(long, default_value_t = 3)]
        repeats: u32,
        /// Goal ids to run (default: all).
        #[arg(long = "goal")]
        goals: Vec<String>,
        /// Results file to write.
        #[arg(long)]
        out: PathBuf,
        /// Seconds between runs that touch the live web.
        #[arg(long, default_value_t = 20)]
        pause_secs: u64,
        /// The CLI binary to run.
        #[arg(long, default_value = "target/release/web-agent-research")]
        bin: PathBuf,
        /// Extra `research` flags, after `--`.
        #[arg(last = true)]
        extra_args: Vec<String>,
    },
    /// Grade every unjudged run of a results file in place.
    Judge {
        results: PathBuf,
        /// Pinned judge model id.
        #[arg(long, default_value = DEFAULT_JUDGE_MODEL)]
        model: String,
    },
    /// Per-category summary table (Markdown).
    Summary { results: PathBuf },
    /// Paired comparison of two results files (Markdown).
    Compare { a: PathBuf, b: PathBuf },
    /// Gzip recorded goal dirs into `tests/eval_fixtures/<goal id>/`.
    Pack {
        /// Fixture root the goals were recorded into.
        from: PathBuf,
        /// Goal ids to pack.
        #[arg(required = true)]
        goals: Vec<String>,
    },
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn git_rev() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[allow(clippy::too_many_arguments)]
async fn run(
    mode: Mode,
    fixtures: PathBuf,
    repeats: u32,
    goals: Vec<String>,
    out: PathBuf,
    pause_secs: u64,
    bin: PathBuf,
    extra_args: Vec<String>,
) -> Result<(), String> {
    if !bin.is_file() {
        return Err(format!(
            "{}: no binary (cargo build --release first)",
            bin.display()
        ));
    }
    if mode == Mode::Record && env_value("GATEWAY_BASE_URL").is_empty() {
        return Err("record needs GATEWAY_BASE_URL (the proxy's upstream)".to_owned());
    }
    let selected = golden::select(golden::load(&golden::default_dir())?, &goals)?;
    let mut file = if out.is_file() {
        let file = ResultsFile::read(&out)?;
        if file.mode != mode || file.extra_args != extra_args {
            return Err(format!(
                "{}: recorded with another mode or flags; pick a new --out",
                out.display()
            ));
        }
        file
    } else {
        ResultsFile {
            format: FORMAT,
            started_unix: unix_now(),
            mode,
            agent_model: env_value("GATEWAY_MODEL"),
            judge_model: None,
            repeats,
            git_rev: git_rev(),
            extra_args: extra_args.clone(),
            rows: Vec::new(),
        }
    };
    let done: HashSet<(String, u32)> = file
        .rows
        .iter()
        .map(|row| (row.goal_id.clone(), row.repeat))
        .collect();
    let plan = RunPlan {
        mode,
        bin,
        fixtures,
        extra_args,
    };
    let pause = Duration::from_secs(pause_secs);
    let touches_web = matches!(mode, Mode::Record | Mode::Live);
    let mut first = true;
    for goal in &selected {
        for repeat in 1..=repeats {
            if done.contains(&(goal.goal.id.clone(), repeat)) {
                continue;
            }
            if touches_web && !first {
                tokio::time::sleep(pause).await;
            }
            first = false;
            file.rows.push(run_one(&plan, goal, repeat).await?);
            // Saved after every run, so an interrupted batch resumes.
            file.write(&out)?;
        }
    }
    println!("{}", summary_table(&file.rows));
    Ok(())
}

async fn judge(path: &Path, model: &str) -> Result<(), String> {
    let mut file = ResultsFile::read(path)?;
    if let Some(pinned) = &file.judge_model {
        if pinned != model {
            return Err(format!(
                "{}: already judged by {pinned}; judge a copy to use {model}",
                path.display()
            ));
        }
    }
    file.judge_model = Some(model.to_owned());
    let goals = golden::load(&golden::default_dir())?;
    let judge = Judge::from_env(model)?;
    for index in 0..file.rows.len() {
        if file.rows[index].judge.is_some() {
            continue;
        }
        let row = &file.rows[index];
        let goal = &goals
            .iter()
            .find(|g| g.goal.id == row.goal_id)
            .ok_or_else(|| format!("goal {:?} not in the golden set", row.goal_id))?
            .goal;
        let judgement = match &row.answer {
            Some(answer) => judge.grade(goal, answer).await?,
            None => Judgement::no_answer(goal),
        };
        eprintln!(
            "judge {} r{}: {:?} recall {:.2}",
            row.goal_id, row.repeat, judgement.grade, judgement.nugget_recall
        );
        file.rows[index].judge = Some(judgement);
        file.write(path)?;
    }
    file.write(path)?;
    println!("{}", summary_table(&file.rows));
    Ok(())
}

fn pack(from: &Path, goals: &[String]) -> Result<(), String> {
    for goal in goals {
        let to = eval::pack::committed_dir().join(goal);
        eval::pack::pack(&from.join(goal), &to)?;
        eprintln!("packed {goal} into {}", to.display());
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Run {
            mode,
            fixtures,
            repeats,
            goals,
            out,
            pause_secs,
            bin,
            extra_args,
        } => {
            run(
                mode, fixtures, repeats, goals, out, pause_secs, bin, extra_args,
            )
            .await
        }
        Command::Judge { results, model } => judge(&results, &model).await,
        Command::Summary { results } => {
            ResultsFile::read(&results).map(|file| println!("{}", summary_table(&file.rows)))
        }
        Command::Compare { a, b } => ResultsFile::read(&a).and_then(|a| {
            let b = ResultsFile::read(&b)?;
            println!("{}", eval::compare::table(&eval::compare::compare(&a, &b)));
            Ok(())
        }),
        Command::Pack { from, goals } => pack(&from, &goals),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
