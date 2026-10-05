//! The judge: one gateway call per answered run grades the answer with the
//! SimpleQA classes and marks each golden nugget supported or not.
//!
//! Eval-only. Uses the same gateway env as the agent (`GATEWAY_BASE_URL`,
//! `GATEWAY_API_KEY`, `GATEWAY_API_FORMAT`) with a pinned model id, which
//! the results file records. The default is from a different model family
//! than the agent's default (`claude-*`).

use serde::{Deserialize, Serialize};
use web_agent_research::llm::{ChatMessage, Gateway, GatewayConfig};

use super::golden::Goal;

/// Pinned judge model id (OpenAI family; the agent runs on `claude-*`).
pub const DEFAULT_JUDGE_MODEL: &str = "gpt-5.6-sol";

/// SimpleQA grade of one answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Grade {
    Correct,
    Incorrect,
    NotAttempted,
}

/// The judge's verdict on one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Judgement {
    pub grade: Grade,
    /// One flag per golden nugget, in golden order.
    pub nuggets: Vec<bool>,
    /// Supported nuggets / all nuggets.
    pub nugget_recall: f64,
    /// The judge's one-line reason; empty for a run with no answer.
    pub rationale: String,
}

impl Judgement {
    /// A run that produced no answer (non-zero exit): never sent to the
    /// judge, graded NOT_ATTEMPTED with no nugget supported.
    pub fn no_answer(goal: &Goal) -> Self {
        Self {
            grade: Grade::NotAttempted,
            nuggets: vec![false; goal.nuggets.len()],
            nugget_recall: 0.0,
            rationale: String::new(),
        }
    }
}

const SYSTEM: &str = "You grade answers of a web research agent against a list of reference facts (nuggets). You are strict and literal. You reply with one JSON object and nothing else.";

/// The grading prompt for one answer.
pub fn prompt(goal: &Goal, answer: &str) -> String {
    let mut nuggets = String::new();
    for (index, nugget) in goal.nuggets.iter().enumerate() {
        nuggets.push_str(&format!("{}. {}\n", index + 1, nugget.text));
    }
    format!(
        "Question:\n{question}\n\nReference nuggets (verified facts a complete answer states):\n{nuggets}\nAnswer to grade (between the markers):\n<<<ANSWER\n{answer}\nANSWER>>>\n\n\
Step 1, nuggets. For each nugget, decide whether the answer states it, in any wording or language. A nugget is supported only if the answer asserts the same fact with the same specific values (names, versions, numbers, flags, signatures). Partial or vaguer statements are not supported.\n\n\
Step 2, grade, with the SimpleQA classes:\n\
- CORRECT: the answer addresses the question, states its core fact(s) consistently with the nuggets, and contradicts none of them.\n\
- INCORRECT: the answer states something that contradicts a nugget, or its core claim is wrong. Hedged or caveated wrong claims are still INCORRECT.\n\
- NOT_ATTEMPTED: the answer does not answer the question (says it found nothing, answers another question, or stays too vague to check) and contradicts no nugget.\n\
Facts in the answer beyond the nuggets do not count against it unless they contradict a nugget. Ignore the citations and source list except as context.\n\n\
Reply with exactly this JSON shape:\n\
{{\"nuggets\": [true or false, one per nugget in order], \"grade\": \"CORRECT\" | \"INCORRECT\" | \"NOT_ATTEMPTED\", \"rationale\": \"one sentence\"}}",
        question = goal.goal,
    )
}

#[derive(Deserialize)]
struct Reply {
    nuggets: Vec<bool>,
    grade: Grade,
    #[serde(default)]
    rationale: String,
}

/// Parse the judge's reply: the outermost `{…}` of `text`, with exactly
/// one flag per nugget.
pub fn parse(text: &str, nugget_count: usize) -> Result<Judgement, String> {
    let start = text.find('{').ok_or("judge reply has no JSON object")?;
    let end = text.rfind('}').ok_or("judge reply has no JSON object")?;
    let reply: Reply = serde_json::from_str(&text[start..=end])
        .map_err(|err| format!("judge reply: {err}: {text}"))?;
    if reply.nuggets.len() != nugget_count {
        return Err(format!(
            "judge returned {} nugget flags, expected {nugget_count}",
            reply.nuggets.len()
        ));
    }
    let supported = reply.nuggets.iter().filter(|flag| **flag).count();
    Ok(Judgement {
        grade: reply.grade,
        nugget_recall: supported as f64 / nugget_count.max(1) as f64,
        nuggets: reply.nuggets,
        rationale: reply.rationale,
    })
}

/// A gateway client pinned to `model`.
pub struct Judge {
    gateway: Gateway,
    pub model: String,
}

impl Judge {
    /// From the gateway env, with the model replaced by `model`.
    pub fn from_env(model: &str) -> Result<Self, String> {
        let mut config = GatewayConfig::from_env().map_err(|err| err.to_string())?;
        config.model = model.to_owned();
        Ok(Self {
            gateway: Gateway::new(config),
            model: model.to_owned(),
        })
    }

    /// Grade one answer; one retry when the reply does not parse.
    pub async fn grade(&self, goal: &Goal, answer: &str) -> Result<Judgement, String> {
        let messages = [
            ChatMessage::system(SYSTEM),
            ChatMessage::user(prompt(goal, answer)),
        ];
        let mut last = String::new();
        for _ in 0..2 {
            let reply = self
                .gateway
                .chat(&messages)
                .await
                .map_err(|err| format!("judge gateway: {err}"))?;
            match parse(&reply.output, goal.nuggets.len()) {
                Ok(judgement) => return Ok(judgement),
                Err(err) => last = err,
            }
        }
        Err(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::golden::{Nugget, Volatility};

    fn goal() -> Goal {
        Goal {
            id: "g-01".to_owned(),
            goal: "What is X?".to_owned(),
            nuggets: ["X is a crate.", "X needs tokio."]
                .into_iter()
                .map(|text| Nugget {
                    text: text.to_owned(),
                    url: "https://x.example/".to_owned(),
                    volatility: Volatility::Slow,
                    verified_on: "2026-10-05".to_owned(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_fenced_reply_parses_and_counts_recall() {
        let text = "```json\n{\"nuggets\": [true, false], \"grade\": \"CORRECT\", \"rationale\": \"ok\"}\n```";
        let judgement = parse(text, 2).expect("parses");
        assert_eq!(judgement.grade, Grade::Correct);
        assert_eq!(judgement.nuggets, [true, false]);
        assert_eq!(judgement.nugget_recall, 0.5);
    }

    #[test]
    fn a_wrong_flag_count_or_grade_is_rejected() {
        assert!(parse("{\"nuggets\": [true], \"grade\": \"CORRECT\"}", 2).is_err());
        assert!(parse("{\"nuggets\": [true, true], \"grade\": \"MAYBE\"}", 2).is_err());
        let judgement = parse(
            "{\"nuggets\": [false, false], \"grade\": \"NOT_ATTEMPTED\"}",
            2,
        )
        .expect("ok");
        assert_eq!(judgement.grade, Grade::NotAttempted);
    }

    #[test]
    fn the_prompt_numbers_every_nugget_and_a_missing_answer_is_not_attempted() {
        let text = prompt(&goal(), "X is a crate.");
        assert!(
            text.contains("1. X is a crate.\n2. X needs tokio.\n"),
            "{text}"
        );
        let judgement = Judgement::no_answer(&goal());
        assert_eq!(judgement.grade, Grade::NotAttempted);
        assert_eq!(judgement.nuggets, [false, false]);
    }
}
