//! The golden set: `tests/golden/<category>.json`, one file per category.

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

/// One atomic fact a correct answer states, checked against `url`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Nugget {
    pub text: String,
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
        goals.extend(file.goals.into_iter().map(|goal| GoldenGoal {
            category: category.to_owned(),
            goal,
        }));
    }
    Ok(goals)
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
