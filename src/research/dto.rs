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
use crate::research::synthesis::{Citation, Synthesis, SynthesisSize, ThemeSection};

/// Default turn budget when the caller omits `max_turns`.
pub fn default_max_turns() -> u32 {
    LoopBudget::default().max_turns
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
    /// Total gateway calls allowed. Must be >= 1.
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Defaults to generated `<unix-secs>-<pid>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Defaults to `<data root>/sessions/<id>.jsonl`, written best-effort; an
    /// explicit path must be writable (failure is `ResearchError::Io`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_out: Option<PathBuf>,
}

/// One inline `[title](url)` citation, in document order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CitationDTO {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// One `## ` section of the answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ThemeDTO {
    pub title: String,
    pub points: Vec<String>,
    pub citations: Vec<CitationDTO>,
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
    pub synthesis: SynthesisDTO,
    pub turns_used: u32,
    pub evidence_urls: Vec<String>,
    pub usage: UsageDTO,
}

impl From<Citation> for CitationDTO {
    fn from(citation: Citation) -> Self {
        Self {
            url: citation.url,
            title: citation.title,
        }
    }
}

impl From<ThemeSection> for ThemeDTO {
    fn from(section: ThemeSection) -> Self {
        Self {
            title: section.title,
            points: section.points,
            citations: section
                .citations
                .into_iter()
                .map(CitationDTO::from)
                .collect(),
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

    #[test]
    fn response_json_shape_is_client_ready() {
        let response = ResearchResponse {
            session_id: "1788825600-1234".to_owned(),
            synthesis: SynthesisDTO {
                size: SynthesisSize::Medium,
                summary: "summary".to_owned(),
                themes: vec![ThemeDTO {
                    title: "T".to_owned(),
                    points: vec!["p".to_owned()],
                    citations: vec![CitationDTO {
                        url: "https://example.com/t".to_owned(),
                        title: Some("t".to_owned()),
                    }],
                }],
                citations: vec![CitationDTO {
                    url: "https://example.com/t".to_owned(),
                    title: Some("t".to_owned()),
                }],
            },
            turns_used: 3,
            evidence_urls: vec!["https://example.com/t".to_owned()],
            usage: UsageDTO {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                reasoning_tokens: 0,
                cached_prompt_tokens: 0,
                cache_creation_prompt_tokens: 0,
            },
        };
        let value = serde_json::to_value(&response).expect("response serializes");
        assert_eq!(value["session_id"], "1788825600-1234");
        assert_eq!(value["synthesis"]["size"], "medium");
        assert_eq!(value["turns_used"], 3);
        assert_eq!(value["evidence_urls"][0], "https://example.com/t");
        assert_eq!(value["usage"]["total_tokens"], 0);
    }

    #[test]
    fn request_size_parses_lowercase() {
        let req: ResearchRequest =
            serde_json::from_str(r#"{"goal":"g","size":"large","max_turns":8}"#)
                .expect("request deserializes");
        assert_eq!(req.size, SynthesisSize::Large);
        assert_eq!(req.session_id, None);
        let back = serde_json::to_value(&req).expect("request serializes");
        assert_eq!(back["size"], "large");
    }
}
