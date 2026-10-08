//! The judge: one gateway call per answered run grades the answer with the
//! SimpleQA classes and marks each golden nugget supported or not.
//!
//! Eval-only. Uses the same gateway env as the agent (`GATEWAY_BASE_URL`,
//! `GATEWAY_API_KEY`, `GATEWAY_API_FORMAT`) with a pinned model id, which
//! the results file records. The default is from a different model family
//! than the agent's default (`claude-*`).

use serde::{Deserialize, Serialize};
use web_agent_research::llm::{ChatMessage, Gateway, GatewayConfig};

use super::golden::{Goal, Scope};

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
    /// Supported `core` nuggets / `core` nuggets: the headline recall.
    pub core_recall: f64,
    /// Supported `supporting` nuggets / `supporting` nuggets; `None` for a
    /// goal with no supporting nugget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supporting_recall: Option<f64>,
    /// The judge's one-line reason; empty for a run with no answer.
    pub rationale: String,
}

impl Judgement {
    /// A judgement of `goal` from one flag per nugget; the recalls are
    /// derived from the flags and the scopes in the golden file.
    pub fn new(goal: &Goal, grade: Grade, nuggets: Vec<bool>, rationale: String) -> Self {
        let recall = |scope: Option<Scope>| {
            let (mut hit, mut all) = (0usize, 0usize);
            for (nugget, flag) in goal.nuggets.iter().zip(&nuggets) {
                if scope.is_none_or(|s| nugget.scope == s) {
                    all += 1;
                    hit += usize::from(*flag);
                }
            }
            (all > 0).then(|| hit as f64 / all as f64)
        };
        Self {
            grade,
            nugget_recall: recall(None).unwrap_or(0.0),
            core_recall: recall(Some(Scope::Core)).unwrap_or(0.0),
            supporting_recall: recall(Some(Scope::Supporting)),
            nuggets,
            rationale,
        }
    }

    /// A run that produced no answer (non-zero exit): never sent to the
    /// judge, graded NOT_ATTEMPTED with no nugget supported.
    pub fn no_answer(goal: &Goal) -> Self {
        Self::new(
            goal,
            Grade::NotAttempted,
            vec![false; goal.nuggets.len()],
            String::new(),
        )
    }
}

const SYSTEM: &str = "You grade answers of a web research agent against a list of reference facts (nuggets), each one atomic claim. You are strict and literal. You reply with one JSON object and nothing else.";

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
/// one flag per nugget of `goal`.
pub fn parse(text: &str, goal: &Goal) -> Result<Judgement, String> {
    let nugget_count = goal.nuggets.len();
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
    Ok(Judgement::new(
        goal,
        reply.grade,
        reply.nuggets,
        reply.rationale,
    ))
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

    /// One free-form call with the pinned model: the raw reply text. The
    /// claim pass (`claims`) parses its own reply shape.
    pub async fn ask(&self, system: &str, user: &str) -> Result<String, String> {
        let messages = [ChatMessage::system(system), ChatMessage::user(user)];
        self.gateway
            .chat(&messages)
            .await
            .map(|reply| reply.output)
            .map_err(|err| format!("judge gateway: {err}"))
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
            match parse(&reply.output, goal) {
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
            nuggets: [
                ("X is a crate.", Scope::Core),
                ("X needs tokio.", Scope::Core),
                ("X is old.", Scope::Supporting),
            ]
            .into_iter()
            .map(|(text, scope)| Nugget {
                text: text.to_owned(),
                scope,
                url: "https://x.example/".to_owned(),
                volatility: Volatility::Slow,
                verified_on: "2026-10-08".to_owned(),
            })
            .collect(),
        }
    }

    #[test]
    fn a_fenced_reply_parses_and_counts_recall_per_scope() {
        let text = "```json\n{\"nuggets\": [true, false, true], \"grade\": \"CORRECT\", \"rationale\": \"ok\"}\n```";
        let judgement = parse(text, &goal()).expect("parses");
        assert_eq!(judgement.grade, Grade::Correct);
        assert_eq!(judgement.nuggets, [true, false, true]);
        assert_eq!(judgement.nugget_recall, 2.0 / 3.0);
        assert_eq!(judgement.core_recall, 0.5);
        assert_eq!(judgement.supporting_recall, Some(1.0));
    }

    #[test]
    fn a_goal_without_supporting_nuggets_has_no_supporting_recall() {
        let mut core_only = goal();
        core_only.nuggets.truncate(2);
        let judgement = Judgement::new(&core_only, Grade::Correct, vec![true, true], String::new());
        assert_eq!(judgement.core_recall, 1.0);
        assert_eq!(judgement.supporting_recall, None);
    }

    #[test]
    fn a_wrong_flag_count_or_grade_is_rejected() {
        let goal = goal();
        assert!(parse("{\"nuggets\": [true], \"grade\": \"CORRECT\"}", &goal).is_err());
        assert!(parse(
            "{\"nuggets\": [true, true, true], \"grade\": \"MAYBE\"}",
            &goal
        )
        .is_err());
        let judgement = parse(
            "{\"nuggets\": [false, false, false], \"grade\": \"NOT_ATTEMPTED\"}",
            &goal,
        )
        .expect("ok");
        assert_eq!(judgement.grade, Grade::NotAttempted);
    }

    #[test]
    fn the_prompt_numbers_every_nugget_and_a_missing_answer_is_not_attempted() {
        let text = prompt(&goal(), "X is a crate.");
        assert!(
            text.contains("1. X is a crate.\n2. X needs tokio.\n3. X is old.\n"),
            "{text}"
        );
        let judgement = Judgement::no_answer(&goal());
        assert_eq!(judgement.grade, Grade::NotAttempted);
        assert_eq!(judgement.nuggets, [false, false, false]);
        assert_eq!(judgement.core_recall, 0.0);
    }
}
