//! The results file of one eval batch and its per-category summary.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::golden::CATEGORIES;
use super::judge::Grade;
use super::metrics::RunRow;
use super::runner::Mode;

/// Results file format version.
pub const FORMAT: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultsFile {
    pub format: u32,
    /// Start of the batch, Unix seconds (UTC).
    pub started_unix: u64,
    pub mode: Mode,
    /// `GATEWAY_MODEL` the agent ran with.
    pub agent_model: String,
    /// Pinned judge model id; `None` until `eval judge` ran.
    pub judge_model: Option<String>,
    pub repeats: u32,
    /// `git rev-parse --short HEAD` of the checkout, when available.
    pub git_rev: Option<String>,
    /// Extra `research` flags of every run.
    pub extra_args: Vec<String>,
    pub rows: Vec<RunRow>,
}

impl ResultsFile {
    pub fn read(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
        let file: Self =
            serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        if file.format != FORMAT {
            return Err(format!(
                "{}: results format {}, expected {FORMAT}",
                path.display(),
                file.format
            ));
        }
        Ok(file)
    }

    /// Pretty JSON, written through a temp file so a crash mid-batch keeps
    /// the last complete file.
    pub fn write(&self, path: &Path) -> Result<(), String> {
        let mut text = serde_json::to_string_pretty(self).map_err(|err| err.to_string())?;
        text.push('\n');
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|err| format!("{}: {err}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|err| format!("{}: {err}", path.display()))
    }
}

/// Aggregates of a set of runs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub runs: usize,
    /// Runs that exited 0.
    pub ok: usize,
    /// Means over exit-0 runs.
    pub turns: Option<f64>,
    pub total_tokens: Option<f64>,
    pub citations: Option<f64>,
    pub wall_secs: Option<f64>,
    /// Exact cited bullets / cited bullets, pooled over runs with
    /// `verification`.
    pub exact_share: Option<f64>,
    /// Judged runs (every run once `eval judge` ran).
    pub judged: usize,
    pub correct: usize,
    pub incorrect: usize,
    pub not_attempted: usize,
    /// Mean nugget recall over judged runs.
    pub nugget_recall: Option<f64>,
}

fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = values.fold((0.0, 0usize), |(sum, n), v| (sum + v, n + 1));
    (n > 0).then(|| sum / n as f64)
}

pub fn summarize<'a>(rows: impl Iterator<Item = &'a RunRow> + Clone) -> Summary {
    let ok = || rows.clone().filter(|row| row.metrics.exit_code == 0);
    let (exact, cited) = rows
        .clone()
        .filter_map(|row| row.metrics.verification)
        .fold((0u64, 0u64), |(exact, cited), v| {
            (exact + u64::from(v.exact), cited + u64::from(v.cited))
        });
    let judged = || rows.clone().filter_map(|row| row.judge.as_ref());
    let grades = |grade: Grade| judged().filter(|j| j.grade == grade).count();
    Summary {
        runs: rows.clone().count(),
        ok: ok().count(),
        turns: mean(ok().filter_map(|r| r.metrics.turns_used).map(f64::from)),
        total_tokens: mean(
            ok().filter_map(|r| r.metrics.total_tokens)
                .map(|t| t as f64),
        ),
        citations: mean(ok().filter_map(|r| r.metrics.citations).map(f64::from)),
        wall_secs: mean(ok().map(|r| r.wall_ms as f64 / 1000.0)),
        exact_share: (cited > 0).then(|| exact as f64 / cited as f64),
        judged: judged().count(),
        correct: grades(Grade::Correct),
        incorrect: grades(Grade::Incorrect),
        not_attempted: grades(Grade::NotAttempted),
        nugget_recall: mean(judged().map(|j| j.nugget_recall)),
    }
}

fn cell(value: Option<f64>, digits: usize) -> String {
    value.map_or_else(|| "-".to_owned(), |v| format!("{v:.digits$}"))
}

fn share(part: usize, whole: usize) -> String {
    if whole == 0 {
        "-".to_owned()
    } else {
        format!("{:.0}%", 100.0 * part as f64 / whole as f64)
    }
}

/// Markdown table: one row per category in [`CATEGORIES`] order, then
/// `all`.
pub fn summary_table(rows: &[RunRow]) -> String {
    let mut out = String::from(
        "| category | runs | exit 0 | correct | incorrect | not attempted | nugget recall | exact quotes | citations | turns | tokens | wall s |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    let groups = CATEGORIES
        .iter()
        .map(|category| (*category, Some(*category)))
        .chain(std::iter::once(("all", None)));
    for (label, filter) in groups {
        let selected = rows
            .iter()
            .filter(|row| filter.is_none_or(|category| row.category == category));
        let s = summarize(selected);
        if s.runs == 0 {
            continue;
        }
        out.push_str(&format!(
            "| {label} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            s.runs,
            share(s.ok, s.runs),
            share(s.correct, s.judged),
            share(s.incorrect, s.judged),
            share(s.not_attempted, s.judged),
            cell(s.nugget_recall, 2),
            cell(s.exact_share, 2),
            cell(s.citations, 1),
            cell(s.turns, 1),
            cell(s.total_tokens, 0),
            cell(s.wall_secs, 0),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::judge::Judgement;
    use crate::eval::metrics::Metrics;
    use web_agent_research::VerificationDTO;

    fn row(category: &str, exit_code: i32, grade: Option<Grade>) -> RunRow {
        let ok = exit_code == 0;
        RunRow {
            goal_id: format!("{category}-01"),
            category: category.to_owned(),
            repeat: 1,
            metrics: Metrics {
                exit_code,
                turns_used: ok.then_some(4),
                turn_budget: ok.then_some(15),
                prompt_tokens: ok.then_some(90),
                completion_tokens: ok.then_some(10),
                total_tokens: ok.then_some(100),
                cached_prompt_tokens: ok.then_some(0),
                citations: ok.then_some(3),
                evidence_urls: ok.then_some(5),
                verification: ok.then_some(VerificationDTO {
                    bullets: 4,
                    cited: 4,
                    exact: 3,
                    partial: 1,
                    none: 0,
                }),
                replay_misses: 0,
            },
            wall_ms: 2000,
            answer: None,
            error: None,
            judge: grade.map(|grade| Judgement {
                grade,
                nuggets: vec![true, false],
                nugget_recall: 0.5,
                rationale: String::new(),
            }),
        }
    }

    #[test]
    fn the_summary_pools_runs_and_skips_failed_ones_in_means() {
        let rows = [
            row("pt_br", 0, Some(Grade::Correct)),
            row("pt_br", 7, Some(Grade::NotAttempted)),
        ];
        let s = summarize(rows.iter());
        assert_eq!(
            (s.runs, s.ok, s.judged, s.correct, s.not_attempted),
            (2, 1, 2, 1, 1)
        );
        assert_eq!(s.turns, Some(4.0));
        assert_eq!(s.exact_share, Some(0.75));
        assert_eq!(s.nugget_recall, Some(0.5));
        let table = summary_table(&rows);
        assert!(
            table.contains(
                "| pt_br | 2 | 50% | 50% | 0% | 50% | 0.50 | 0.75 | 3.0 | 4.0 | 100 | 2 |"
            ),
            "{table}"
        );
        assert!(table.contains("| all | 2 |"), "{table}");
        assert!(!table.contains("| how_to |"), "{table}");
    }
}
