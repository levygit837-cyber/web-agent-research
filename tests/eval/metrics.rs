//! One metrics row per run, computed from the CLI's exit code and its
//! `--json` stdout (the Synthesis), plus what the runner measured.

use serde::{Deserialize, Serialize};
use web_agent_research::{render, ResearchResponse, VerificationDTO};

use super::claims::Claims;
use super::judge::Judgement;

/// Stderr marker of a `fetch` the replay fixture had no page for; mirrors
/// `research::agent_loop::tape::REPLAY_MISS`.
pub const REPLAY_MISS: &str = "eval replay miss";

/// The deterministic part of a run: everything but wall time and the
/// judge. Equal on an exact replay of a recorded transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    pub exit_code: i32,
    /// `None` when the run did not exit 0.
    pub turns_used: Option<u32>,
    pub turn_budget: Option<u32>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_prompt_tokens: Option<u64>,
    pub citations: Option<u32>,
    pub evidence_urls: Option<u32>,
    /// `synthesis.verification` when the `--json` carries it (#106).
    pub verification: Option<VerificationDTO>,
    /// `fetch` calls a replay fixture had no page for.
    pub replay_misses: u32,
}

/// One run of one goal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRow {
    pub goal_id: String,
    pub category: String,
    /// 1-based repeat index.
    pub repeat: u32,
    #[serde(flatten)]
    pub metrics: Metrics,
    pub wall_ms: u64,
    /// Rendered Synthesis (human output: summary, themes, sources) on exit 0.
    pub answer: Option<String>,
    /// Last stderr line on a non-zero exit.
    pub error: Option<String>,
    /// Filled by `eval judge`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<Judgement>,
    /// Filled by the claim pass of `eval judge` (#123); absent on a run
    /// that was not claim-judged (failed, live, or judged before #123).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claims: Option<Claims>,
}

impl Metrics {
    /// Metrics of one finished run. `stdout` must be the `--json` object
    /// when `exit_code == 0`.
    pub fn from_output(exit_code: i32, stdout: &str, stderr: &str) -> Result<Self, String> {
        let replay_misses = u32::try_from(stderr.matches(REPLAY_MISS).count()).unwrap_or(u32::MAX);
        let mut metrics = Self {
            exit_code,
            turns_used: None,
            turn_budget: None,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            cached_prompt_tokens: None,
            citations: None,
            evidence_urls: None,
            verification: None,
            replay_misses,
        };
        if exit_code != 0 {
            return Ok(metrics);
        }
        let response = parse_response(stdout)?;
        let raw: serde_json::Value =
            serde_json::from_str(stdout.trim()).map_err(|err| err.to_string())?;
        metrics.turns_used = Some(response.turns_used);
        metrics.turn_budget = Some(response.turn_budget);
        metrics.prompt_tokens = Some(response.usage.prompt_tokens);
        metrics.completion_tokens = Some(response.usage.completion_tokens);
        metrics.total_tokens = Some(response.usage.total_tokens);
        metrics.cached_prompt_tokens = Some(response.usage.cached_prompt_tokens);
        metrics.citations = Some(count(response.synthesis.citations.len()));
        metrics.evidence_urls = Some(count(response.evidence_urls.len()));
        metrics.verification = raw["synthesis"]
            .get("verification")
            .map(|_| response.synthesis.verification);
        Ok(metrics)
    }

    /// Share of cited bullets whose quote and tokens are on the page.
    pub fn exact_share(&self) -> Option<f64> {
        let verification = self.verification?;
        (verification.cited > 0)
            .then(|| f64::from(verification.exact) / f64::from(verification.cited))
    }
}

/// The `--json` stdout as the typed response.
pub fn parse_response(stdout: &str) -> Result<ResearchResponse, String> {
    serde_json::from_str(stdout.trim()).map_err(|err| format!("--json stdout: {err}"))
}

/// The rendered answer of a successful run, for the judge.
pub fn answer_text(stdout: &str) -> Result<String, String> {
    parse_response(stdout).map(|response| render(&response))
}

/// Last non-empty stderr line, the CLI's `error: …` on failure.
pub fn last_error_line(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().chars().take(400).collect())
}

fn count(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STDOUT: &str = r#"{"session_id":"s","content_trust":"untrusted-web","synthesis":{"size":"medium","summary":"S [a](https://a.example/)","themes":[{"title":"T","points":["p"],"citations":[]}],"citations":[{"url":"https://a.example/","support":"exact","quote":"q"},{"url":"https://b.example/","support":"none"}],"verification":{"bullets":3,"cited":2,"exact":1,"partial":0,"none":1}},"turns_used":4,"turn_budget":15,"evidence_urls":["https://a.example/","https://b.example/","https://c.example/"],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"reasoning_tokens":0,"cached_prompt_tokens":50}}"#;

    #[test]
    fn a_successful_run_reads_every_count_from_the_json() {
        let stderr = format!("{REPLAY_MISS}: no recorded page for x\n{REPLAY_MISS}: y\n");
        let metrics = Metrics::from_output(0, STDOUT, &stderr).expect("parses");
        assert_eq!(metrics.turns_used, Some(4));
        assert_eq!(metrics.turn_budget, Some(15));
        assert_eq!(metrics.total_tokens, Some(120));
        assert_eq!(metrics.cached_prompt_tokens, Some(50));
        assert_eq!(metrics.citations, Some(2));
        assert_eq!(metrics.evidence_urls, Some(3));
        assert_eq!(metrics.replay_misses, 2);
        let verification = metrics.verification.expect("verification present");
        assert_eq!((verification.cited, verification.exact), (2, 1));
        assert_eq!(metrics.exact_share(), Some(0.5));
        assert!(answer_text(STDOUT).expect("renders").starts_with("S "));
    }

    #[test]
    fn verification_is_absent_when_the_json_has_none() {
        let stdout = STDOUT.replace(
            r#","verification":{"bullets":3,"cited":2,"exact":1,"partial":0,"none":1}"#,
            "",
        );
        let metrics = Metrics::from_output(0, &stdout, "").expect("parses");
        assert_eq!(metrics.verification, None);
        assert_eq!(metrics.exact_share(), None);
    }

    #[test]
    fn a_failed_run_has_only_its_exit_code() {
        let metrics = Metrics::from_output(7, "", "error: search blocked\n").expect("ok");
        assert_eq!(metrics.exit_code, 7);
        assert_eq!(metrics.turns_used, None);
        assert_eq!(
            last_error_line("x\nerror: search blocked\n\n").as_deref(),
            Some("error: search blocked")
        );
    }
}
