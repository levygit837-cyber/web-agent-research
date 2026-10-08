//! The golden set (format v2, #122): `tests/golden/<category>.json`, one
//! file per category. A nugget is one atomic claim and carries a scope.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The seven categories, in report order.
pub const CATEGORIES: [&str; 7] = [
    "crate_docs",
    "api_reference",
    "exact_error",
    "recent_news",
    "how_to",
    "pt_br",
    "comparison",
];

/// How soon a nugget can go stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Volatility {
    Never,
    Slow,
    Fast,
}

/// How directly the goal asks for a nugget. Written in the golden file,
/// never inferred per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// The goal asks for this fact.
    Core,
    /// True, related detail the goal does not ask for.
    Supporting,
}

/// One atomic fact a correct answer states, checked against `url`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Nugget {
    pub text: String,
    pub scope: Scope,
    pub url: String,
    pub volatility: Volatility,
    /// `YYYY-MM-DD` the fact was read on `url`.
    pub verified_on: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Goal {
    pub id: String,
    pub goal: String,
    pub nuggets: Vec<Nugget>,
}

impl Goal {
    /// The nuggets of `scope`, in golden order.
    pub fn of_scope(&self, scope: Scope) -> impl Iterator<Item = &Nugget> {
        self.nuggets.iter().filter(move |n| n.scope == scope)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryFile {
    pub category: String,
    pub goals: Vec<Goal>,
}

/// One goal with its category, as the runner and judge use it.
#[derive(Debug, Clone)]
pub struct GoldenGoal {
    pub category: String,
    pub goal: Goal,
}

/// `tests/golden` of this checkout.
pub fn default_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// Every goal of every category file in `dir`, in [`CATEGORIES`] order.
pub fn load(dir: &Path) -> Result<Vec<GoldenGoal>, String> {
    let mut goals = Vec::new();
    for category in CATEGORIES {
        let path = dir.join(format!("{category}.json"));
        let text =
            std::fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let file: CategoryFile =
            serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        if file.category != category {
            return Err(format!(
                "{}: category {:?}, expected {category:?}",
                path.display(),
                file.category
            ));
        }
        for goal in &file.goals {
            validate(goal).map_err(|err| format!("{}: {err}", path.display()))?;
        }
        goals.extend(file.goals.into_iter().map(|goal| GoldenGoal {
            category: category.to_owned(),
            goal,
        }));
    }
    Ok(goals)
}

/// Longest nugget text, in characters.
const MAX_NUGGET_CHARS: usize = 200;

/// Clause joiners that mark two claims in one nugget.
const JOINERS: [&str; 6] = [
    ", but ",
    ", while ",
    ", whereas ",
    ", so ",
    ", and then ",
    " and also ",
];

/// `text` with every `code` and "quoted" span replaced by `_`; `None` when
/// a delimiter is unbalanced.
fn mask_spans(text: &str) -> Option<String> {
    let (mut out, mut code, mut quote) = (String::new(), false, false);
    for ch in text.chars() {
        match ch {
            '`' if !quote => {
                if code {
                    out.push('_');
                }
                code = !code;
            }
            '"' if !code => {
                if quote {
                    out.push('_');
                }
                quote = !quote;
            }
            _ if !code && !quote => out.push(ch),
            _ => {}
        }
    }
    (!code && !quote).then_some(out)
}

/// Why `text` is not one claim, if it is not.
fn claim_problem(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return Some("empty text".to_owned());
    }
    let Some(masked) = mask_spans(text) else {
        return Some("unbalanced backtick or double quote".to_owned());
    };
    if masked.contains(';') {
        return Some("a semicolon joins claims".to_owned());
    }
    let chars: Vec<char> = masked.chars().collect();
    let second_sentence = chars
        .windows(3)
        .any(|w| matches!(w[0], '.' | '!' | '?') && w[1].is_whitespace() && w[2].is_uppercase());
    if second_sentence {
        return Some("more than one sentence".to_owned());
    }
    if let Some(joiner) = JOINERS.iter().find(|j| masked.contains(**j)) {
        return Some(format!("joins clauses with {:?}", joiner.trim()));
    }
    let len = text.chars().count();
    (len > MAX_NUGGET_CHARS).then(|| format!("{len} chars, over {MAX_NUGGET_CHARS}"))
}

/// The v2 rules for one goal: at least 2 core nuggets, and every nugget
/// text one claim.
pub fn validate(goal: &Goal) -> Result<(), String> {
    let core = goal.of_scope(Scope::Core).count();
    if core < 2 {
        return Err(format!("{}: {core} core nuggets, need at least 2", goal.id));
    }
    for (index, nugget) in goal.nuggets.iter().enumerate() {
        if let Some(problem) = claim_problem(&nugget.text) {
            return Err(format!(
                "{} nugget {}: {problem}: {}",
                goal.id,
                index + 1,
                nugget.text
            ));
        }
    }
    Ok(())
}

/// Keep the goals whose id is in `ids` (all when `ids` is empty); an
/// unknown id is an error.
pub fn select(goals: Vec<GoldenGoal>, ids: &[String]) -> Result<Vec<GoldenGoal>, String> {
    if ids.is_empty() {
        return Ok(goals);
    }
    for id in ids {
        if !goals.iter().any(|g| &g.goal.id == id) {
            return Err(format!("unknown goal id {id:?}"));
        }
    }
    Ok(goals
        .into_iter()
        .filter(|g| ids.contains(&g.goal.id))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nugget(text: &str, scope: Scope) -> Nugget {
        Nugget {
            text: text.to_owned(),
            scope,
            url: "https://x.example/".to_owned(),
            volatility: Volatility::Slow,
            verified_on: "2026-10-08".to_owned(),
        }
    }

    fn goal(nuggets: Vec<Nugget>) -> Goal {
        Goal {
            id: "g-01".to_owned(),
            goal: "What is X?".to_owned(),
            nuggets,
        }
    }

    #[test]
    fn a_goal_needs_two_core_nuggets() {
        let one = goal(vec![
            nugget("X is a crate.", Scope::Core),
            nugget("X is old.", Scope::Supporting),
        ]);
        let err = validate(&one).expect_err("one core is too few");
        assert!(err.contains("1 core nuggets"), "{err}");
        let two = goal(vec![
            nugget("X is a crate.", Scope::Core),
            nugget("X needs `tokio`.", Scope::Core),
        ]);
        validate(&two).expect("two core nuggets pass");
    }

    #[test]
    fn a_compound_nugget_is_rejected_but_joiners_inside_spans_are_not() {
        for text in [
            "X does A; Y does B.",
            "X does A. Y does B.",
            "X does A, but Y does B.",
            "X does A and also B.",
            "Unbalanced `code.",
            "",
        ] {
            assert!(claim_problem(text).is_some(), "{text:?}");
        }
        assert!(claim_problem(&"a".repeat(201)).is_some());
        for text in [
            "The flag \"a; b\" enables `x, but y` mode.",
            "Version 1.2. is not a sentence break.",
            "`Result<T, E>` has two type parameters.",
        ] {
            assert_eq!(claim_problem(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_nugget_without_a_scope_does_not_load() {
        let json = r#"{"text":"t","url":"https://x.example/","volatility":"slow","verified_on":"2026-10-08"}"#;
        assert!(serde_json::from_str::<Nugget>(json).is_err());
        let json = r#"{"text":"t","scope":"extra","url":"https://x.example/","volatility":"slow","verified_on":"2026-10-08"}"#;
        assert!(serde_json::from_str::<Nugget>(json).is_err());
    }
}
