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

/// How well the cited page supports the bullets that cite it (#106),
/// decided model-free by `agent_loop::verify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Support {
    /// Not checked: no bullet cites this page (a summary-only link), or
    /// the page text is not available.
    #[default]
    Unchecked,
    /// The bullet's quote is not on the page, or the bullet has no quote.
    None,
    /// The quote is on the page, but a number, version, error code or
    /// identifier from the bullet is not.
    Partial,
    /// The quote and every checked token are on the page.
    Exact,
}

impl Support {
    /// Stable wire name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unchecked => "unchecked",
            Self::None => "none",
            Self::Partial => "partial",
            Self::Exact => "exact",
        }
    }
}

/// One inline `[title](url)` citation, in document order of the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub url: String,
    /// Link text; `None` when the link text is empty.
    pub title: Option<String>,
    /// Best support this page gives any bullet that cites it.
    pub support: Support,
    /// The quote of the bullet that decided `support`.
    pub quote: Option<String>,
}

impl Citation {
    /// A citation as parsed, before verification.
    pub fn new(url: String, title: Option<String>) -> Self {
        Self {
            url,
            title,
            support: Support::Unchecked,
            quote: None,
        }
    }
}

/// One bullet of a theme: its text, the links inside it and the verbatim
/// quote it ends with (`(quote: "…")`, stripped from `text`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Point {
    pub text: String,
    /// URLs of the inline links in this bullet, deduplicated, in order.
    pub links: Vec<String>,
    pub quote: Option<String>,
    /// Best support over this bullet's links; set by `agent_loop::verify`.
    pub support: Support,
}

/// One `## ` section of the answer; `points` is never empty.
#[derive(Debug, Clone, Default)]
pub struct ThemeSection {
    pub title: String,
    pub points: Vec<Point>,
    /// Links found inside this section, in document order.
    pub citations: Vec<Citation>,
}

/// Grounding counters over every bullet of the answer (#106). `cited`
/// counts bullets with at least one citation of a fetched page; `exact`,
/// `partial` and `none` split those by their best support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Verification {
    pub bullets: u32,
    pub cited: u32,
    pub exact: u32,
    pub partial: u32,
    pub none: u32,
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
    pub verification: Verification,
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
        let cited = Citation::new("https://example.com/a".to_owned(), None);
        assert_eq!(cited.title, None);
        assert!(!cited.url.is_empty());
    }
}
