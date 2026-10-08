//! Paired comparison of two results files.
//!
//! Per metric, each goal present in both files gives one difference
//! `d = mean(B over its repeats) - mean(A over its repeats)`; the report is
//! the mean of `d` over goals with a two-sided 95% Student-t interval
//! (`mean ± t(0.975, n-1) · sd / √n`). Pairing by goal removes the
//! between-goal variance, which dwarfs the effect of most changes;
//! averaging repeats first damps the run-to-run noise.

use std::collections::BTreeMap;

use super::claims::{rate, Verdict};
use super::judge::Grade;
use super::metrics::RunRow;
use super::results::ResultsFile;

/// One compared metric: a name and its per-run value (`None` = not
/// defined for that run, left out of the goal's mean).
pub struct Metric {
    pub name: &'static str,
    pub value: fn(&RunRow) -> Option<f64>,
}

/// The metrics `eval compare` reports, quality first.
pub const METRICS: [Metric; 15] = [
    Metric {
        name: "correct",
        value: |row| {
            row.judge
                .as_ref()
                .map(|j| if j.grade == Grade::Correct { 1.0 } else { 0.0 })
        },
    },
    Metric {
        name: "core_recall",
        value: |row| row.judge.as_ref().map(|j| j.core_recall),
    },
    Metric {
        name: "supporting_recall",
        value: |row| row.judge.as_ref().and_then(|j| j.supporting_recall),
    },
    Metric {
        name: "nugget_recall",
        value: |row| row.judge.as_ref().map(|j| j.nugget_recall),
    },
    Metric {
        name: "exact_share",
        value: |row| row.metrics.exact_share(),
    },
    Metric {
        name: "claim_precision",
        value: |row| rate(row, Verdict::Supported),
    },
    Metric {
        name: "claim_partly",
        value: |row| rate(row, Verdict::Partly),
    },
    Metric {
        name: "claim_unsupported",
        value: |row| rate(row, Verdict::Unsupported),
    },
    Metric {
        name: "claim_contradicted",
        value: |row| rate(row, Verdict::Contradicted),
    },
    Metric {
        name: "uncited_share",
        value: |row| rate(row, Verdict::Uncited),
    },
    Metric {
        name: "exit_0",
        value: |row| Some(if row.metrics.exit_code == 0 { 1.0 } else { 0.0 }),
    },
    Metric {
        name: "citations",
        value: |row| row.metrics.citations.map(f64::from),
    },
    Metric {
        name: "turns",
        value: |row| row.metrics.turns_used.map(f64::from),
    },
    Metric {
        name: "total_tokens",
        value: |row| row.metrics.total_tokens.map(|t| t as f64),
    },
    Metric {
        name: "wall_secs",
        value: |row| Some(row.wall_ms as f64 / 1000.0),
    },
];

/// Paired difference of one metric.
#[derive(Debug, Clone, PartialEq)]
pub struct Paired {
    pub metric: &'static str,
    /// Goals with a value in both files.
    pub goals: usize,
    pub mean_a: f64,
    pub mean_b: f64,
    /// Mean of per-goal `B - A`.
    pub diff: f64,
    /// 95% interval of `diff`; `None` with fewer than 2 goals.
    pub ci: Option<(f64, f64)>,
}

/// Two-sided 97.5% Student-t quantiles for 1..=30 degrees of freedom.
const T975: [f64; 30] = [
    12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
    2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
    2.052, 2.048, 2.045, 2.042,
];

/// `t(0.975, df)`; past 30, the 40/60/120 table points, then the normal.
pub fn t975(df: usize) -> f64 {
    match df {
        0 => f64::NAN,
        1..=30 => T975[df - 1],
        31..=40 => 2.021,
        41..=60 => 2.000,
        61..=120 => 1.980,
        _ => 1.960,
    }
}

/// Mean and 95% interval of `diffs`.
pub fn mean_ci(diffs: &[f64]) -> (f64, Option<(f64, f64)>) {
    let n = diffs.len();
    if n == 0 {
        return (f64::NAN, None);
    }
    let mean = diffs.iter().sum::<f64>() / n as f64;
    if n < 2 {
        return (mean, None);
    }
    let var = diffs.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
    let half = t975(n - 1) * (var / n as f64).sqrt();
    (mean, Some((mean - half, mean + half)))
}

/// Per-goal mean of `metric` over the goal's repeats.
fn per_goal(rows: &[RunRow], metric: &Metric) -> BTreeMap<String, f64> {
    let mut sums: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for row in rows {
        if let Some(value) = (metric.value)(row) {
            let entry = sums.entry(row.goal_id.clone()).or_default();
            entry.0 += value;
            entry.1 += 1;
        }
    }
    sums.into_iter()
        .map(|(goal, (sum, n))| (goal, sum / n as f64))
        .collect()
}

pub fn paired(a: &[RunRow], b: &[RunRow], metric: &Metric) -> Paired {
    let (goals_a, goals_b) = (per_goal(a, metric), per_goal(b, metric));
    let pairs: Vec<(f64, f64)> = goals_a
        .iter()
        .filter_map(|(goal, va)| goals_b.get(goal).map(|vb| (*va, *vb)))
        .collect();
    let n = pairs.len().max(1) as f64;
    let diffs: Vec<f64> = pairs.iter().map(|(va, vb)| vb - va).collect();
    let (diff, ci) = mean_ci(&diffs);
    Paired {
        metric: metric.name,
        goals: pairs.len(),
        mean_a: pairs.iter().map(|p| p.0).sum::<f64>() / n,
        mean_b: pairs.iter().map(|p| p.1).sum::<f64>() / n,
        diff,
        ci,
    }
}

/// Every [`METRICS`] entry with at least one paired goal.
pub fn compare(a: &ResultsFile, b: &ResultsFile) -> Vec<Paired> {
    METRICS
        .iter()
        .map(|metric| paired(&a.rows, &b.rows, metric))
        .filter(|p| p.goals > 0)
        .collect()
}

/// Markdown table of [`compare`].
pub fn table(paired: &[Paired]) -> String {
    let mut out = String::from(
        "| metric | goals | mean A | mean B | B - A | 95% CI |\n|---|---|---|---|---|---|\n",
    );
    for p in paired {
        let ci = p.ci.map_or_else(
            || "-".to_owned(),
            |(lo, hi)| format!("[{lo:+.3}, {hi:+.3}]"),
        );
        out.push_str(&format!(
            "| {} | {} | {:.3} | {:.3} | {:+.3} | {ci} |\n",
            p.metric, p.goals, p.mean_a, p.mean_b, p.diff
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::metrics::Metrics;

    fn row(goal: &str, citations: u32) -> RunRow {
        RunRow {
            goal_id: goal.to_owned(),
            category: "how_to".to_owned(),
            repeat: 1,
            metrics: Metrics {
                exit_code: 0,
                turns_used: Some(3),
                turn_budget: Some(15),
                prompt_tokens: Some(1),
                completion_tokens: Some(1),
                total_tokens: Some(2),
                cached_prompt_tokens: Some(0),
                citations: Some(citations),
                evidence_urls: Some(1),
                verification: None,
                replay_misses: 0,
            },
            wall_ms: 1000,
            answer: None,
            error: None,
            judge: None,
            claims: None,
        }
    }

    #[test]
    fn the_interval_matches_a_hand_computed_t_interval() {
        // diffs 1, 2, 3: mean 2, sd 1, half-width 4.303 / sqrt(3).
        let (mean, ci) = mean_ci(&[1.0, 2.0, 3.0]);
        let half = 4.303 / 3f64.sqrt();
        let (lo, hi) = ci.expect("n = 3 has an interval");
        assert!((mean - 2.0).abs() < 1e-12);
        assert!((lo - (2.0 - half)).abs() < 1e-9 && (hi - (2.0 + half)).abs() < 1e-9);
        assert_eq!(mean_ci(&[5.0]).1, None);
        assert_eq!(t975(200), 1.960);
    }

    #[test]
    fn repeats_are_averaged_per_goal_and_unpaired_goals_dropped() {
        let metric = METRICS
            .iter()
            .find(|m| m.name == "citations")
            .expect("citations is compared");
        let a = [row("g1", 2), row("g1", 4), row("g2", 10), row("only-a", 99)];
        let b = [row("g1", 5), row("g2", 12)];
        let p = paired(&a, &b, metric);
        assert_eq!(p.goals, 2);
        // g1: 3 -> 5, g2: 10 -> 12.
        assert_eq!((p.mean_a, p.mean_b, p.diff), (6.5, 8.5, 2.0));
        assert_eq!(p.ci, Some((2.0, 2.0)));
        assert!(
            table(&[p]).contains("| citations | 2 | 6.500 | 8.500 | +2.000 | [+2.000, +2.000] |")
        );
    }
}
