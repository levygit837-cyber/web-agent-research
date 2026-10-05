//! Wire DTOs of one Research: the request a caller builds, the response the
//! CLI prints as JSON, and the `From` impls that turn loop output
//! ([`Synthesis`], [`TokenUsage`]) into them.
//!
//! Serde shapes only; the run itself is in `run.rs`. The public names are
//! re-exported from `research` and the crate root.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::llm::TokenUsage;
use crate::research::agent_loop::LoopBudget;
use crate::research::session::SessionUsage;
use crate::research::synthesis::{
    Citation, Support, Synthesis, SynthesisSize, ThemeSection, Verification,
};

/// Default turn budget when the caller omits `max_turns`.
pub fn default_max_turns() -> u32 {
    LoopBudget::default().max_turns
}

/// Default ceiling the turn budget can grow to (#101).
pub fn default_max_turns_cap() -> u32 {
    LoopBudget::default().max_turns_cap
}

/// Default wall-clock bound on one run, in seconds (#100).
pub const DEFAULT_DEADLINE_SECS: u64 = 300;

fn default_deadline_secs() -> u64 {
    DEFAULT_DEADLINE_SECS
}

fn default_size() -> SynthesisSize {
    SynthesisSize::default()
}

mod size_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::research::synthesis::SynthesisSize;

    pub fn serialize<S>(size: &SynthesisSize, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(size.as_str())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SynthesisSize, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "small" => Ok(SynthesisSize::Small),
            "medium" => Ok(SynthesisSize::Medium),
            "large" => Ok(SynthesisSize::Large),
            "deep" => Ok(SynthesisSize::Deep),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["small", "medium", "large", "deep"],
            )),
        }
    }
}

mod support_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::research::synthesis::Support;

    pub fn serialize<S>(support: &Support, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(support.as_str())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Support, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "exact" => Ok(Support::Exact),
            "partial" => Ok(Support::Partial),
            "none" => Ok(Support::None),
            "unchecked" => Ok(Support::Unchecked),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["exact", "partial", "none", "unchecked"],
            )),
        }
    }
}

/// Content trust label of every `--json` response (#98): the Synthesis is
/// written from third-party web pages and must be treated as such.
pub const CONTENT_TRUST_UNTRUSTED_WEB: &str = "untrusted-web";

fn default_content_trust() -> String {
    CONTENT_TRUST_UNTRUSTED_WEB.to_owned()
}

/// Research input, built by `main.rs` from CLI args (also constructed
/// directly in tests).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResearchRequest {
    /// Research goal, user words. Non-empty after trim.
    pub goal: String,
    /// Requested synthesis size; echoed into the response unchanged.
    #[serde(with = "size_serde", default = "default_size")]
    pub size: SynthesisSize,
    /// Base turn budget (gateway calls). Must be >= 1.
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Ceiling the turn budget can grow to: each successful `search` call
    /// after the first adds 5 turns. Must be >= `max_turns`.
    #[serde(default = "default_max_turns_cap")]
    pub max_turns_cap: u32,
    /// Wall-clock bound on the whole run, in seconds. Must be >= 1.
    #[serde(default = "default_deadline_secs")]
    pub deadline_secs: u64,
    /// Defaults to generated `<unix-secs>-<pid>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Defaults to `<data root>/sessions/<id>.jsonl`, written best-effort; an
    /// explicit path must be writable (failure is `ResearchError::Io`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_out: Option<PathBuf>,
}

/// One inline `[title](url)` citation, in document order, with how well
/// the page supports the bullets that cite it (#106).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CitationDTO {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `exact`, `partial`, `none` or `unchecked`; `unchecked` when absent.
    #[serde(with = "support_serde", default)]
    pub support: Support,
    /// Verbatim quote of the bullet that decided `support`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
}

/// One `## ` section of the answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ThemeDTO {
    pub title: String,
    pub points: Vec<String>,
    pub citations: Vec<CitationDTO>,
}

/// Grounding counters over the answer's bullets (#106); see
/// [`Verification`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct VerificationDTO {
    pub bullets: u32,
    pub cited: u32,
    pub exact: u32,
    pub partial: u32,
    pub none: u32,
}

/// Typed final answer; mirrors [`Synthesis`] 1:1 on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SynthesisDTO {
    #[serde(with = "size_serde")]
    pub size: SynthesisSize,
    pub summary: String,
    pub themes: Vec<ThemeDTO>,
    pub citations: Vec<CitationDTO>,
    #[serde(default)]
    pub verification: VerificationDTO,
}

/// Per-run token counts; saturating sums.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UsageDTO {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub reasoning_tokens: u64,
    pub cached_prompt_tokens: u64,
    /// Prompt tokens written to the provider cache (Anthropic
    /// `cache_creation_input_tokens`); 0 where the provider does not report it.
    #[serde(default)]
    pub cache_creation_prompt_tokens: u64,
}

/// Research output; serializes to `--json` stdout, or renders via `render`
/// for human stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResearchResponse {
    pub session_id: String,
    /// Always `untrusted-web` (#98): the Synthesis is third-party content.
    #[serde(default = "default_content_trust")]
    pub content_trust: String,
    pub synthesis: SynthesisDTO,
    /// Gateway calls made.
    pub turns_used: u32,
    /// Turn budget when the run ended: `max_turns` plus any `search`
    /// extensions, at most `max_turns_cap` (#101). `0` from readers of
    /// responses written before the field existed.
    #[serde(default)]
    pub turn_budget: u32,
    pub evidence_urls: Vec<String>,
    pub usage: UsageDTO,
}

impl From<Citation> for CitationDTO {
    fn from(citation: Citation) -> Self {
        Self {
            url: citation.url,
            title: citation.title,
            support: citation.support,
            quote: citation.quote,
        }
    }
}

impl From<ThemeSection> for ThemeDTO {
    fn from(section: ThemeSection) -> Self {
        Self {
            title: section.title,
            points: section.points.into_iter().map(|point| point.text).collect(),
            citations: section
                .citations
                .into_iter()
                .map(CitationDTO::from)
                .collect(),
        }
    }
}

impl From<Verification> for VerificationDTO {
    fn from(verification: Verification) -> Self {
        Self {
            bullets: verification.bullets,
            cited: verification.cited,
            exact: verification.exact,
            partial: verification.partial,
            none: verification.none,
        }
    }
}

impl From<Synthesis> for SynthesisDTO {
    fn from(synthesis: Synthesis) -> Self {
        Self {
            size: synthesis.size,
            summary: synthesis.summary,
            themes: synthesis.themes.into_iter().map(ThemeDTO::from).collect(),
            citations: synthesis
                .citations
                .into_iter()
                .map(CitationDTO::from)
                .collect(),
            verification: VerificationDTO::from(synthesis.verification),
        }
    }
}

impl From<TokenUsage> for UsageDTO {
    fn from(usage: TokenUsage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            cached_prompt_tokens: usage.cached_prompt_tokens,
            cache_creation_prompt_tokens: usage.cache_creation_prompt_tokens,
        }
    }
}

impl From<UsageDTO> for SessionUsage {
    fn from(usage: UsageDTO) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            cached_prompt_tokens: usage.cached_prompt_tokens,
            cache_creation_prompt_tokens: usage.cache_creation_prompt_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn citation(support: Support, quote: Option<&str>) -> CitationDTO {
        CitationDTO {
            url: "https://example.com/t".to_owned(),
            title: Some("t".to_owned()),
            support,
            quote: quote.map(str::to_owned),
        }
    }

    fn response() -> ResearchResponse {
        ResearchResponse {
            session_id: "1788825600-1234".to_owned(),
            content_trust: CONTENT_TRUST_UNTRUSTED_WEB.to_owned(),
            synthesis: SynthesisDTO {
                size: SynthesisSize::Medium,
                summary: "summary".to_owned(),
                themes: vec![ThemeDTO {
                    title: "T".to_owned(),
                    points: vec!["p".to_owned()],
                    citations: vec![citation(Support::Exact, Some("q"))],
                }],
                citations: vec![citation(Support::Partial, Some("q"))],
                verification: VerificationDTO {
                    bullets: 2,
                    cited: 1,
                    exact: 0,
                    partial: 1,
                    none: 0,
                },
            },
            turns_used: 3,
            turn_budget: 15,
            evidence_urls: vec!["https://example.com/t".to_owned()],
            usage: UsageDTO {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                reasoning_tokens: 0,
                cached_prompt_tokens: 0,
                cache_creation_prompt_tokens: 0,
            },
        }
    }

    #[test]
    fn response_json_shape_is_client_ready() {
        let value = serde_json::to_value(response()).expect("response serializes");
        assert_eq!(value["session_id"], "1788825600-1234");
        assert_eq!(value["content_trust"], "untrusted-web");
        assert_eq!(value["synthesis"]["size"], "medium");
        assert_eq!(value["synthesis"]["citations"][0]["support"], "partial");
        assert_eq!(value["synthesis"]["citations"][0]["quote"], "q");
        assert_eq!(
            value["synthesis"]["themes"][0]["citations"][0]["support"],
            "exact"
        );
        assert_eq!(value["synthesis"]["verification"]["bullets"], 2);
        assert_eq!(value["synthesis"]["verification"]["partial"], 1);
        assert_eq!(value["turns_used"], 3);
        assert_eq!(value["turn_budget"], 15);
        assert_eq!(value["evidence_urls"][0], "https://example.com/t");
        assert_eq!(value["usage"]["total_tokens"], 0);
    }

    /// #106: the new fields round-trip, and a citation with no quote
    /// serializes without the key.
    #[test]
    fn grounding_fields_round_trip() {
        let json = serde_json::to_string(&response()).expect("serializes");
        let back: ResearchResponse = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back.content_trust, "untrusted-web");
        assert_eq!(back.synthesis.citations, response().synthesis.citations);
        assert_eq!(
            back.synthesis.verification,
            response().synthesis.verification
        );
        let bare = serde_json::to_value(citation(Support::Unchecked, None)).expect("serializes");
        assert_eq!(bare["support"], "unchecked");
        assert!(bare.get("quote").is_none(), "{bare}");
    }

    /// JSON written before #98/#106 (no `content_trust`, `support`,
    /// `quote` or `verification`) still parses, with the defaults.
    #[test]
    fn pre_grounding_json_still_parses() {
        let old = r#"{"session_id":"s","synthesis":{"size":"small","summary":"x","themes":[{"title":"T","points":["p"],"citations":[{"url":"https://e.com/a"}]}],"citations":[{"url":"https://e.com/a","title":"a"}]},"turns_used":1,"evidence_urls":[],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0,"reasoning_tokens":0,"cached_prompt_tokens":0}}"#;
        let parsed: ResearchResponse = serde_json::from_str(old).expect("old shape parses");
        assert_eq!(parsed.content_trust, "untrusted-web");
        assert_eq!(parsed.synthesis.citations[0].support, Support::Unchecked);
        assert_eq!(parsed.synthesis.citations[0].quote, None);
        assert_eq!(parsed.synthesis.verification, VerificationDTO::default());
    }

    /// Old readers ignore unknown keys: a pre-#106 shape (serde's default,
    /// no `deny_unknown_fields`) still reads today's JSON.
    #[test]
    fn old_readers_ignore_the_new_keys() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldCitation {
            url: String,
            #[serde(default)]
            title: Option<String>,
        }
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldSynthesis {
            summary: String,
            citations: Vec<OldCitation>,
        }
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OldResponse {
            session_id: String,
            synthesis: OldSynthesis,
        }
        let json = serde_json::to_string(&response()).expect("serializes");
        let old: OldResponse = serde_json::from_str(&json).expect("old reader parses");
        assert_eq!(old.synthesis.citations[0].url, "https://example.com/t");
    }

    #[test]
    fn request_size_parses_lowercase() {
        let req: ResearchRequest =
            serde_json::from_str(r#"{"goal":"g","size":"large","max_turns":8}"#)
                .expect("request deserializes");
        assert_eq!(req.size, SynthesisSize::Large);
        assert_eq!(req.session_id, None);
        assert_eq!(req.max_turns_cap, 25);
        assert_eq!(req.deadline_secs, 300);
        let back = serde_json::to_value(&req).expect("request serializes");
        assert_eq!(back["size"], "large");
    }
}
