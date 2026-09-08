//! Typed synthesis output: catalog data for the agent loop's final answer.
//!
//! Pure data: this module imports nothing from `gateway`, `agent_loop`,
//! `prompts`, or `tools`. The parser lives in `agent_loop::synthesis` next
//! to the behavior that calls it; sizes reserve vocabulary (`deep` behavior
//! belongs to a later Mode, not this ticket).

/// Requested synthesis size; echoed into [`Synthesis::size`] unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SynthesisSize {
    /// Summary plus 1-4 themed sections.
    #[default]
    Medium,
    /// Summary plus a single theme, at least one citation.
    Small,
    /// Summary plus multi-point themed sections.
    Large,
    /// Same shape as large; reserves the deep-Mode vocabulary.
    Deep,
}

impl SynthesisSize {
    /// Stable name used in prompts and the goal message.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::Deep => "deep",
        }
    }
}

/// One inline `[title](url)` citation, in document order of the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub url: String,
    /// Link text; `None` when the link text is empty.
    pub title: Option<String>,
}

/// One `## ` section of the answer; `points` is never empty.
#[derive(Debug, Clone, Default)]
pub struct ThemeSection {
    pub title: String,
    pub points: Vec<String>,
    /// Links found inside this section, in document order.
    pub citations: Vec<Citation>,
}

/// Typed final answer of one Research run.
#[derive(Debug, Clone)]
pub struct Synthesis {
    /// Echoes the requested size; the parser never upgrades or downgrades.
    pub size: SynthesisSize,
    /// Never empty (parser guarantee).
    pub summary: String,
    /// Always non-empty; heading-free answers yield one `"Findings"` theme.
    pub themes: Vec<ThemeSection>,
    /// Whole-answer set, deduplicated by exact URL, in document order.
    pub citations: Vec<Citation>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_names_match_contract() {
        assert_eq!(SynthesisSize::Small.as_str(), "small");
        assert_eq!(SynthesisSize::Medium.as_str(), "medium");
        assert_eq!(SynthesisSize::Large.as_str(), "large");
        assert_eq!(SynthesisSize::Deep.as_str(), "deep");
    }

    #[test]
    fn default_size_is_medium() {
        assert_eq!(SynthesisSize::default(), SynthesisSize::Medium);
        assert_eq!(SynthesisSize::default().as_str(), "medium");
    }

    #[test]
    fn citation_empty_title_is_none() {
        let cited = Citation {
            url: "https://example.com/a".to_owned(),
            title: None,
        };
        assert_eq!(cited.title, None);
        assert!(!cited.url.is_empty());
    }
}
