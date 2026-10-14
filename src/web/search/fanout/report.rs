//! What one fan-out call reports per engine (#104): the whole-call failure
//! message, with each engine listed once, and the per-engine status lines
//! the model sees when a call returns no Hits.

use super::leg::LegOutcome;
use crate::web::search::types::{EngineProblem, EngineReport, SearchProvider, SearchProviderError};

/// Format leg failures as `"id: msg; id: msg"` (Omp
/// `formatSearchProviderFailures`), one entry per engine in first-seen
/// order (#104: a multi-query call no longer repeats every engine once per
/// Query). An engine whose legs failed differently lists each distinct
/// message once, separated by ` / `.
pub fn all_failed_message(failures: &[(String, String)]) -> String {
    let mut engines: Vec<(&str, Vec<&str>)> = Vec::new();
    for (id, msg) in failures {
        match engines.iter_mut().find(|(seen, _)| *seen == id.as_str()) {
            Some((_, msgs)) => {
                if !msgs.contains(&msg.as_str()) {
                    msgs.push(msg);
                }
            }
            None => engines.push((id, vec![msg])),
        }
    }
    engines
        .iter()
        .map(|(id, msgs)| format!("{id}: {}", msgs.join(" / ")))
        .collect::<Vec<_>>()
        .join("; ")
}

/// One short line per engine in `enabled` (priority order), built from
/// that engine's legs in `legs`: `"<id>: <n> rows"`, `"<id>: no results"`
/// (every leg answered with a recognized results page and no row), and
/// each distinct leg error (`"<id>: suspended (challenge), 3412s left"`,
/// `"<id>: unrecognized markup"`). A row count and errors combine when
/// some of the engine's legs answered and others failed.
pub(super) fn engine_status(
    enabled: &[SearchProvider],
    legs: &[(SearchProvider, LegOutcome)],
) -> Vec<String> {
    enabled
        .iter()
        .map(|provider| {
            let mut rows = 0usize;
            let mut answered = false;
            let mut problems: Vec<String> = Vec::new();
            for (_, outcome) in legs.iter().filter(|(leg, _)| leg == provider) {
                match outcome {
                    Ok(found) => {
                        answered = true;
                        rows += found.len();
                    }
                    Err(err) => {
                        let detail = status_detail(err);
                        if !problems.contains(&detail) {
                            problems.push(detail);
                        }
                    }
                }
            }
            let mut parts: Vec<String> = Vec::with_capacity(problems.len() + 1);
            if answered {
                parts.push(match rows {
                    0 => "no results".to_string(),
                    1 => "1 row".to_string(),
                    n => format!("{n} rows"),
                });
            }
            parts.extend(problems);
            format!("{}: {}", provider.id(), parts.join(", "))
        })
        .collect()
}

/// The structured twin of [`engine_status`] (#148): one report per engine in
/// `enabled`, aggregated over that engine's legs. `cache_hit[i]` says leg
/// `i` (the same order as `legs`) was served from the disk cache.
pub(super) fn engine_reports(
    enabled: &[SearchProvider],
    legs: &[(SearchProvider, LegOutcome)],
    cache_hit: &[bool],
) -> Vec<EngineReport> {
    enabled
        .iter()
        .map(|provider| {
            let mut report = EngineReport {
                engine: provider.id().to_string(),
                rows: 0,
                answered_legs: 0,
                cached_legs: 0,
                problems: Vec::new(),
            };
            for (index, (_, outcome)) in legs
                .iter()
                .enumerate()
                .filter(|(_, (leg, _))| leg == provider)
            {
                match outcome {
                    Ok(found) => {
                        report.answered_legs += 1;
                        report.rows += found.len();
                        if cache_hit.get(index).copied().unwrap_or(false) {
                            report.cached_legs += 1;
                        }
                    }
                    Err(err) => {
                        let problem = engine_problem(err);
                        if !report.problems.contains(&problem) {
                            report.problems.push(problem);
                        }
                    }
                }
            }
            report
        })
        .collect()
}

fn engine_problem(err: &SearchProviderError) -> EngineProblem {
    match err {
        SearchProviderError::Challenge { detail, .. } => EngineProblem::Challenge {
            detail: detail.clone(),
        },
        SearchProviderError::Timeout { .. } => EngineProblem::Timeout,
        SearchProviderError::Suspended {
            remaining_secs,
            reason,
            ..
        } => EngineProblem::Suspended {
            reason: reason.clone(),
            remaining_secs: *remaining_secs,
        },
        SearchProviderError::Throttled { detail, .. } => EngineProblem::Throttled {
            detail: detail.clone(),
        },
        SearchProviderError::Upstream { detail, status, .. } => EngineProblem::Error {
            detail: detail.clone(),
            http_status: *status,
        },
        other => EngineProblem::Error {
            detail: error_detail(other),
            http_status: None,
        },
    }
}

/// [`error_detail`] without the Query a timeout names: the status line is
/// per engine, not per leg.
fn status_detail(err: &SearchProviderError) -> String {
    match err {
        SearchProviderError::Timeout { .. } => "timed out".to_string(),
        other => error_detail(other),
    }
}

pub(super) fn error_detail(err: &SearchProviderError) -> String {
    match err {
        SearchProviderError::EmptyQuery => "empty query".to_string(),
        SearchProviderError::TooManyQueries { got, max } => {
            format!("too many queries: {got} > {max}")
        }
        SearchProviderError::Challenge { detail, .. } => detail.clone(),
        SearchProviderError::Timeout { query, .. } => format!("timed out: {query}"),
        SearchProviderError::Upstream { detail, .. } => detail.clone(),
        SearchProviderError::Suspended {
            remaining_secs,
            reason,
            ..
        } => format!("suspended ({reason}), {remaining_secs}s left"),
        SearchProviderError::Throttled { detail, .. } => detail.clone(),
        SearchProviderError::AllFailed { failures, .. } => failures.clone(),
    }
}
