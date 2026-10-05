//! Env-gated live eval round trip (#107): the judge against the real
//! gateway. Runs only with `GATEWAY_LIVE=1` and `GATEWAY_API_KEY` set, so
//! `cargo test` stays offline. Judge model: `EVAL_JUDGE_MODEL`, else
//! [`DEFAULT_JUDGE_MODEL`].

#[allow(dead_code)]
mod eval;

use eval::golden;
use eval::judge::{Grade, Judge, DEFAULT_JUDGE_MODEL};

#[tokio::test]
async fn live_judge_grades_a_golden_answer() {
    if std::env::var("GATEWAY_LIVE").as_deref() != Ok("1") {
        eprintln!("skipping live eval test (GATEWAY_LIVE != 1)");
        return;
    }
    if std::env::var("GATEWAY_API_KEY").map_or(true, |key| key.trim().is_empty()) {
        eprintln!("skipping live eval test (GATEWAY_API_KEY not set)");
        return;
    }
    let model = std::env::var("EVAL_JUDGE_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_JUDGE_MODEL.to_owned());
    let goals = golden::load(&golden::default_dir()).expect("golden set loads");
    let goal = &goals[0].goal;
    // An answer that states every nugget verbatim must be CORRECT with
    // every nugget supported; one that declines must be NOT_ATTEMPTED.
    let full: String = goal
        .nuggets
        .iter()
        .map(|n| format!("- {}\n", n.text))
        .collect();
    let judge = Judge::from_env(&model).expect("judge configures");
    let graded = judge.grade(goal, &full).await.expect("judge answers");
    eprintln!("live judge {model} on {}: {graded:?}", goal.id);
    assert_eq!(graded.grade, Grade::Correct, "{graded:?}");
    assert!(graded.nugget_recall >= 0.99, "{graded:?}");
    let declined = judge
        .grade(goal, "I could not find any information about this.")
        .await
        .expect("judge answers");
    assert_eq!(declined.grade, Grade::NotAttempted, "{declined:?}");
    assert_eq!(declined.nugget_recall, 0.0, "{declined:?}");
}
