//! Gateway configuration: endpoint, credentials, and per-model parameters.
//!
//! Defaults target the local dev gateway; the API key always comes from the
//! environment and is never logged (`Debug` redacts it).

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Standard reasoning-effort levels shared by OpenAI's `reasoning_effort`
/// and OpenRouter's unified `reasoning.effort`. Serializes lowercase to the
/// top-level `reasoning_effort` wire field; `GATEWAY_REASONING_EFFORT`
/// parses the same lowercase names via [`std::str::FromStr`], rejecting
/// anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl std::str::FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            other => Err(format!(
                "invalid reasoning effort {other:?}: expected one of \
                 none|minimal|low|medium|high|xhigh|max"
            )),
        }
    }
}

/// Wire format of the gateway endpoint. One enum, two branches inside
/// `Gateway` (ADR-0007): `research` never learns which one is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApiFormat {
    /// OpenAI Chat Completions: `POST {base}/chat/completions`, Bearer auth.
    #[default]
    Openai,
    /// Anthropic Messages: `POST {base}/messages`, `x-api-key` +
    /// `anthropic-version`, explicit `cache_control` breakpoints.
    Anthropic,
}

impl std::str::FromStr for ApiFormat {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "openai" => Ok(Self::Openai),
            "anthropic" => Ok(Self::Anthropic),
            other => Err(format!(
                "invalid GATEWAY_API_FORMAT {other:?}: expected openai|anthropic"
            )),
        }
    }
}

/// Per-model parameter overrides. All fields optional; unset falls back
/// to the gateway-level default.
#[derive(Debug, Clone, Default)]
pub struct ModelParams {
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Token cap sent as `max_completion_tokens`.
    pub max_tokens: Option<u32>,
    /// Thinking-token budget. `Some(0)` sends `thinking: {"type": "disabled"}`;
    /// `Some(n)` with `n > 0` sends `{"type": "enabled", "budget_tokens": n}`;
    /// `None` omits `thinking` entirely.
    pub thinking_budget: Option<u32>,
}

/// Fully resolved parameters for one request.
#[derive(Debug, Clone)]
pub struct ResolvedParams {
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_tokens: Option<u32>,
    pub thinking_budget: Option<u32>,
    /// Per-run cache routing hint (the Session id); gateway-wide, never a
    /// per-model override.
    pub prompt_cache_key: Option<String>,
}

#[derive(Clone)]
pub struct GatewayConfig {
    /// Wire format; `GATEWAY_API_FORMAT`, default OpenAI.
    pub api_format: ApiFormat,
    /// Base URL, e.g. "http://localhost:8317/v1". No trailing slash assumed;
    /// constructors trim one trailing `/`.
    pub base_url: String,
    /// Bearer token. Never logged; `Debug` redacts it (manual impl below).
    api_key: String,
    /// Default model id used when the caller does not pick one.
    pub model: String,
    /// Model context window in tokens; `GATEWAY_CONTEXT_WINDOW`, else a
    /// default by model id. Sizes the research loop's context budget.
    pub context_window_tokens: u32,
    /// Gateway-wide defaults; per-model entries in `model_overrides` win.
    pub default_reasoning_effort: Option<ReasoningEffort>,
    pub default_max_tokens: Option<u32>,
    pub default_thinking_budget: Option<u32>,
    /// model id -> overrides. Precedence: override > default > omitted.
    pub model_overrides: HashMap<String, ModelParams>,
    /// Per-attempt request timeout.
    pub request_timeout: Duration,
    /// Max total attempts including the first try.
    pub max_attempts: u32,
    /// Per-run prompt-cache routing hint, sent as `prompt_cache_key` on
    /// every request when set. Set via [`GatewayConfig::with_prompt_cache_key`].
    pub prompt_cache_key: Option<String>,
    /// `GATEWAY_PROMPT_CACHE_KEY=off` flips this false, making
    /// [`GatewayConfig::with_prompt_cache_key`] a no-op regardless of what
    /// the caller passes.
    prompt_cache_key_enabled: bool,
    /// Gateway-specific fields merged into the wire body (OpenAI SDK
    /// `extra_body` semantics). Always a JSON object; typed fields and
    /// `model`/`messages`/`tools`/`tool_choice` win on key collision.
    pub extra_body: Option<serde_json::Value>,
}

impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("api_format", &self.api_format)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("default_reasoning_effort", &self.default_reasoning_effort)
            .field("default_max_tokens", &self.default_max_tokens)
            .field("default_thinking_budget", &self.default_thinking_budget)
            .field("model_overrides", &self.model_overrides)
            .field("request_timeout", &self.request_timeout)
            .field("max_attempts", &self.max_attempts)
            .field("context_window_tokens", &self.context_window_tokens)
            .field("prompt_cache_key", &self.prompt_cache_key)
            .field("prompt_cache_key_enabled", &self.prompt_cache_key_enabled)
            .field("extra_body", &self.extra_body)
            .finish()
    }
}

fn trim_one_slash(url: &str) -> &str {
    url.strip_suffix('/').unwrap_or(url)
}

/// Context window, in tokens, assumed for a model when
/// `GATEWAY_CONTEXT_WINDOW` is unset: 200K for `claude*` ids, 128K for
/// anything else (the smallest window among common current models).
fn default_context_window(model: &str) -> u32 {
    if model.starts_with("claude") {
        200_000
    } else {
        128_000
    }
}

impl GatewayConfig {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            api_format: ApiFormat::Openai,
            base_url: trim_one_slash(&base_url).to_owned(),
            api_key,
            context_window_tokens: default_context_window(&model),
            model,
            default_reasoning_effort: None,
            default_max_tokens: None,
            default_thinking_budget: None,
            model_overrides: HashMap::new(),
            request_timeout: Duration::from_secs(60),
            max_attempts: 3,
            prompt_cache_key: None,
            prompt_cache_key_enabled: true,
            extra_body: None,
        }
    }

    pub fn from_env() -> anyhow::Result<Self> {
        fn env_or(key: &str, default: &str) -> String {
            std::env::var(key)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_owned())
        }

        let api_key = std::env::var("GATEWAY_API_KEY")
            .map(|v| v.trim().to_owned())
            .unwrap_or_default();
        if api_key.is_empty() {
            anyhow::bail!("GATEWAY_API_KEY is not set");
        }
        let api_format = std::env::var("GATEWAY_API_FORMAT")
            .map(|v| v.trim().to_ascii_lowercase())
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| v.parse::<ApiFormat>().map_err(|err| anyhow::anyhow!(err)))
            .transpose()?
            .unwrap_or_default();
        let base_url = env_or("GATEWAY_BASE_URL", "http://localhost:8317/v1");
        let model = env_or("GATEWAY_MODEL", "muse-spark-1.3");
        let default_reasoning_effort = std::env::var("GATEWAY_REASONING_EFFORT")
            .map(|v| v.trim().to_owned())
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| {
                v.parse::<ReasoningEffort>()
                    .map_err(|err| anyhow::anyhow!(err))
            })
            .transpose()?;
        let default_max_tokens = std::env::var("GATEWAY_MAX_TOKENS")
            .map(|v| v.trim().to_owned())
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| {
                v.parse::<u32>()
                    .map_err(|_| anyhow::anyhow!("GATEWAY_MAX_TOKENS is not a valid u32: {v:?}"))
            })
            .transpose()?;
        let default_thinking_budget = std::env::var("GATEWAY_THINKING_BUDGET")
            .map(|v| v.trim().to_owned())
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| {
                v.parse::<u32>().map_err(|_| {
                    anyhow::anyhow!("GATEWAY_THINKING_BUDGET is not a valid u32: {v:?}")
                })
            })
            .transpose()?;
        if let (Some(max_tokens), Some(budget)) = (default_max_tokens, default_thinking_budget) {
            if max_tokens <= budget {
                anyhow::bail!(
                    "GATEWAY_MAX_TOKENS ({max_tokens}) must be strictly greater than \
                     GATEWAY_THINKING_BUDGET ({budget}): reasoning tokens count against \
                     max_tokens"
                );
            }
        }
        if api_format == ApiFormat::Anthropic {
            if let Some(effort @ (ReasoningEffort::None | ReasoningEffort::Minimal)) =
                default_reasoning_effort
            {
                anyhow::bail!(
                    "GATEWAY_REASONING_EFFORT={effort:?} has no Anthropic Messages equivalent: \
                     use low|medium|high|xhigh|max, or unset it"
                );
            }
            if let Some(budget @ 1..=1023) = default_thinking_budget {
                anyhow::bail!(
                    "GATEWAY_THINKING_BUDGET ({budget}) is below the Anthropic minimum of \
                     {}; use 0 to disable thinking",
                    super::anthropic::MIN_THINKING_BUDGET
                );
            }
        }
        let request_timeout = std::env::var("GATEWAY_TIMEOUT_SECS")
            .map(|v| v.trim().to_owned())
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| {
                v.parse::<u64>()
                    .map_err(|_| {
                        anyhow::anyhow!("GATEWAY_TIMEOUT_SECS is not a valid integer: {v:?}")
                    })
                    .map(Duration::from_secs)
            })
            .transpose()?
            .unwrap_or_else(|| Duration::from_secs(60));
        let max_attempts = std::env::var("GATEWAY_MAX_ATTEMPTS")
            .map(|v| v.trim().to_owned())
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| {
                v.parse::<u32>()
                    .map_err(|_| anyhow::anyhow!("GATEWAY_MAX_ATTEMPTS is not a valid u32: {v:?}"))
            })
            .transpose()?
            .unwrap_or(3);
        // Only the literal value "off" disables the per-run cache key; unset
        // (default) and every other value leave it enabled.
        let prompt_cache_key_enabled = std::env::var("GATEWAY_PROMPT_CACHE_KEY")
            .ok()
            .map(|v| v.trim().to_owned())
            .map_or(true, |v| v != "off");
        let context_window_tokens = std::env::var("GATEWAY_CONTEXT_WINDOW")
            .map(|v| v.trim().to_owned())
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| {
                v.parse::<u32>().map_err(|_| {
                    anyhow::anyhow!("GATEWAY_CONTEXT_WINDOW is not a valid u32: {v:?}")
                })
            })
            .transpose()?
            .unwrap_or_else(|| default_context_window(&model));
        let extra_body = std::env::var("GATEWAY_EXTRA_BODY")
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .map(|v| -> anyhow::Result<serde_json::Value> {
                let value: serde_json::Value = serde_json::from_str(&v).map_err(|err| {
                    anyhow::anyhow!("GATEWAY_EXTRA_BODY is not valid JSON: {err}")
                })?;
                if !value.is_object() {
                    anyhow::bail!("GATEWAY_EXTRA_BODY must be a JSON object, got: {v}");
                }
                Ok(value)
            })
            .transpose()?;

        Ok(Self {
            api_format,
            base_url: trim_one_slash(&base_url).to_owned(),
            api_key,
            context_window_tokens,
            model,
            default_reasoning_effort,
            default_max_tokens,
            default_thinking_budget,
            model_overrides: HashMap::new(),
            request_timeout,
            max_attempts,
            prompt_cache_key: None,
            prompt_cache_key_enabled,
            extra_body,
        })
    }

    pub fn with_model_override(mut self, model: &str, params: ModelParams) -> Self {
        self.model_overrides.insert(model.to_owned(), params);
        self
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub fn with_max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n;
        self
    }

    /// Tokens the context must leave free for the model's reply:
    /// `default_max_tokens` if set, else the Anthropic default, plus the
    /// thinking budget when one is set. Deliberately conservative: it is
    /// also applied on the OpenAI branch, where the real cap may be larger
    /// or absent.
    pub fn reply_reserve_tokens(&self) -> u32 {
        self.default_max_tokens
            .unwrap_or(super::anthropic::DEFAULT_MAX_TOKENS)
            .saturating_add(self.default_thinking_budget.unwrap_or(0))
    }

    /// Per-run prompt-cache routing hint (typically the Session id), sent
    /// as `prompt_cache_key` on every request. A no-op when
    /// `GATEWAY_PROMPT_CACHE_KEY=off` disabled caching for this config.
    pub fn with_prompt_cache_key(mut self, key: impl Into<String>) -> Self {
        if self.prompt_cache_key_enabled {
            self.prompt_cache_key = Some(key.into());
        }
        self
    }

    pub fn effective_params(&self, model: &str) -> ResolvedParams {
        let override_params = self.model_overrides.get(model);
        ResolvedParams {
            model: model.to_owned(),
            reasoning_effort: override_params
                .and_then(|o| o.reasoning_effort)
                .or(self.default_reasoning_effort),
            max_tokens: override_params
                .and_then(|o| o.max_tokens)
                .or(self.default_max_tokens),
            thinking_budget: override_params
                .and_then(|o| o.thinking_budget)
                .or(self.default_thinking_budget),
            prompt_cache_key: self.prompt_cache_key.clone(),
        }
    }

    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::request::ChatRequest;
    use crate::test_support::EnvGuard;

    const ALL_GATEWAY_KEYS: [&str; 10] = [
        "GATEWAY_API_KEY",
        "GATEWAY_API_FORMAT",
        "GATEWAY_BASE_URL",
        "GATEWAY_MODEL",
        "GATEWAY_REASONING_EFFORT",
        "GATEWAY_MAX_TOKENS",
        "GATEWAY_THINKING_BUDGET",
        "GATEWAY_PROMPT_CACHE_KEY",
        "GATEWAY_EXTRA_BODY",
        "GATEWAY_CONTEXT_WINDOW",
    ];

    #[test]
    fn api_format_defaults_to_openai_and_parses_case_insensitively() {
        let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
        std::env::set_var("GATEWAY_API_KEY", "k");
        let cfg = GatewayConfig::from_env().expect("default config");
        assert_eq!(cfg.api_format, ApiFormat::Openai);
        std::env::set_var("GATEWAY_API_FORMAT", " Anthropic ");
        let cfg = GatewayConfig::from_env().expect("anthropic config");
        assert_eq!(cfg.api_format, ApiFormat::Anthropic);
    }

    #[test]
    fn invalid_api_format_errors() {
        let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
        std::env::set_var("GATEWAY_API_KEY", "k");
        std::env::set_var("GATEWAY_API_FORMAT", "gemini");
        let err = GatewayConfig::from_env().expect_err("unknown format must fail");
        assert!(err.to_string().contains("openai|anthropic"), "{err}");
    }

    #[test]
    fn anthropic_rejects_efforts_and_budgets_the_messages_api_refuses() {
        for (key, value) in [
            ("GATEWAY_REASONING_EFFORT", "none"),
            ("GATEWAY_REASONING_EFFORT", "minimal"),
            ("GATEWAY_THINKING_BUDGET", "512"),
        ] {
            let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
            std::env::set_var("GATEWAY_API_KEY", "k");
            std::env::set_var("GATEWAY_API_FORMAT", "anthropic");
            std::env::set_var(key, value);
            GatewayConfig::from_env().expect_err(&format!("{key}={value} must fail for anthropic"));
        }
        for (key, value) in [
            ("GATEWAY_REASONING_EFFORT", "low"),
            ("GATEWAY_THINKING_BUDGET", "0"),
            ("GATEWAY_THINKING_BUDGET", "1024"),
        ] {
            let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
            std::env::set_var("GATEWAY_API_KEY", "k");
            std::env::set_var("GATEWAY_API_FORMAT", "anthropic");
            std::env::set_var(key, value);
            GatewayConfig::from_env()
                .unwrap_or_else(|err| panic!("{key}={value} must pass for anthropic: {err}"));
        }
    }

    #[test]
    fn per_model_beats_default() {
        let cfg = GatewayConfig::new(
            "http://localhost:8317/v1".to_owned(),
            "key".to_owned(),
            "glm-5p2".to_owned(),
        )
        .with_model_override(
            "glm-5p2",
            ModelParams {
                reasoning_effort: Some(ReasoningEffort::High),
                max_tokens: Some(4096),
                thinking_budget: Some(1024),
            },
        );
        // Gateway-level default behind the override.
        let mut cfg = cfg;
        cfg.default_reasoning_effort = Some(ReasoningEffort::Low);
        cfg.default_thinking_budget = Some(256);

        let hit = cfg.effective_params("glm-5p2");
        assert_eq!(hit.model, "glm-5p2");
        assert_eq!(hit.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(hit.max_tokens, Some(4096));
        assert_eq!(hit.thinking_budget, Some(1024));

        let miss = cfg.effective_params("other");
        assert_eq!(miss.model, "other");
        assert_eq!(miss.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(miss.max_tokens, None);
        assert_eq!(miss.thinking_budget, Some(256));
    }

    #[test]
    fn unset_means_omitted() {
        let cfg = GatewayConfig::new(
            "http://localhost:8317/v1".to_owned(),
            "key".to_owned(),
            "m".to_owned(),
        );
        let params = cfg.effective_params("m");
        assert_eq!(params.reasoning_effort, None);
        assert_eq!(params.max_tokens, None);
        assert_eq!(params.thinking_budget, None);
        assert_eq!(params.prompt_cache_key, None);

        let body = serde_json::to_value(ChatRequest::from_resolved(&params, vec![]))
            .expect("request must serialize");
        let obj = body.as_object().expect("body is an object");
        for key in [
            "reasoning_effort",
            "max_completion_tokens",
            "thinking",
            "prompt_cache_key",
        ] {
            assert!(
                !obj.contains_key(key),
                "unset {key} must be omitted, got {body}"
            );
        }
    }

    #[test]
    fn base_url_trims_one_trailing_slash() {
        let cfg = GatewayConfig::new(
            "http://localhost:8317/v1/".to_owned(),
            "key".to_owned(),
            "m".to_owned(),
        );
        assert_eq!(cfg.base_url, "http://localhost:8317/v1");
    }

    #[test]
    fn invalid_reasoning_effort_errors() {
        let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
        std::env::set_var("GATEWAY_API_KEY", "k");
        std::env::set_var("GATEWAY_REASONING_EFFORT", "turbo");
        let err = GatewayConfig::from_env().expect_err("unknown effort must fail");
        let message = err.to_string();
        assert!(
            message.contains("turbo") && message.contains("none|minimal|low|medium|high"),
            "error must name the bad value and the allowed set, got {message}"
        );
    }

    #[test]
    fn every_reasoning_effort_value_parses() {
        for value in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
            std::env::set_var("GATEWAY_API_KEY", "k");
            std::env::set_var("GATEWAY_REASONING_EFFORT", value);
            let cfg = GatewayConfig::from_env()
                .unwrap_or_else(|err| panic!("{value} must parse, got {err}"));
            let body =
                serde_json::to_value(cfg.default_reasoning_effort).expect("effort serializes");
            assert_eq!(body, serde_json::json!(value));
        }
    }

    #[test]
    fn invalid_thinking_budget_errors() {
        let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
        std::env::set_var("GATEWAY_API_KEY", "k");
        std::env::set_var("GATEWAY_THINKING_BUDGET", "not-a-number");
        let err = GatewayConfig::from_env().expect_err("bad budget must fail");
        assert!(
            err.to_string().contains("GATEWAY_THINKING_BUDGET"),
            "error must name the offending var, got {err}"
        );
    }

    #[test]
    fn thinking_budget_at_or_above_max_tokens_errors() {
        for (max_tokens, budget) in [(100u32, 100u32), (100, 150)] {
            let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
            std::env::set_var("GATEWAY_API_KEY", "k");
            std::env::set_var("GATEWAY_MAX_TOKENS", max_tokens.to_string());
            std::env::set_var("GATEWAY_THINKING_BUDGET", budget.to_string());
            let err = GatewayConfig::from_env().expect_err(&format!(
                "max_tokens={max_tokens} <= budget={budget} must fail"
            ));
            let message = err.to_string();
            assert!(
                message.contains("GATEWAY_MAX_TOKENS")
                    && message.contains("GATEWAY_THINKING_BUDGET"),
                "error must name both vars, got {message}"
            );
        }
    }

    #[test]
    fn thinking_budget_below_max_tokens_ok() {
        let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
        std::env::set_var("GATEWAY_API_KEY", "k");
        std::env::set_var("GATEWAY_MAX_TOKENS", "200");
        std::env::set_var("GATEWAY_THINKING_BUDGET", "100");
        let cfg = GatewayConfig::from_env().expect("budget strictly below max_tokens must pass");
        assert_eq!(cfg.default_max_tokens, Some(200));
        assert_eq!(cfg.default_thinking_budget, Some(100));
    }

    #[test]
    fn extra_body_invalid_json_errors() {
        let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
        std::env::set_var("GATEWAY_API_KEY", "k");
        std::env::set_var("GATEWAY_EXTRA_BODY", "{not json");
        let err = GatewayConfig::from_env().expect_err("invalid JSON must fail");
        assert!(
            err.to_string().contains("GATEWAY_EXTRA_BODY"),
            "error must name the offending var, got {err}"
        );
    }

    #[test]
    fn extra_body_non_object_errors() {
        let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
        std::env::set_var("GATEWAY_API_KEY", "k");
        std::env::set_var("GATEWAY_EXTRA_BODY", "[1,2,3]");
        let err = GatewayConfig::from_env().expect_err("non-object JSON must fail");
        assert!(
            err.to_string().contains("JSON object"),
            "error must say it needs an object, got {err}"
        );
    }

    #[test]
    fn extra_body_valid_object_parses() {
        let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
        std::env::set_var("GATEWAY_API_KEY", "k");
        std::env::set_var("GATEWAY_EXTRA_BODY", r#"{"verbosity": "low"}"#);
        let cfg = GatewayConfig::from_env().expect("valid object must parse");
        assert_eq!(
            cfg.extra_body,
            Some(serde_json::json!({"verbosity": "low"}))
        );
    }

    #[test]
    fn prompt_cache_key_off_disables_builder() {
        let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
        std::env::set_var("GATEWAY_API_KEY", "k");
        std::env::set_var("GATEWAY_PROMPT_CACHE_KEY", "off");
        let cfg = GatewayConfig::from_env()
            .expect("off is a valid value")
            .with_prompt_cache_key("session-1");
        assert_eq!(cfg.prompt_cache_key, None, "off must disable the cache key");
    }

    #[test]
    fn prompt_cache_key_enabled_by_default() {
        let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
        std::env::set_var("GATEWAY_API_KEY", "k");
        let cfg = GatewayConfig::from_env()
            .expect("default must parse")
            .with_prompt_cache_key("session-1");
        assert_eq!(cfg.prompt_cache_key.as_deref(), Some("session-1"));
    }

    #[test]
    fn context_window_defaults_by_model_family() {
        let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
        std::env::set_var("GATEWAY_API_KEY", "k");
        std::env::set_var("GATEWAY_MODEL", "claude-haiku-4.5");
        let claude = GatewayConfig::from_env().expect("claude config");
        assert_eq!(claude.context_window_tokens, 200_000);
        std::env::set_var("GATEWAY_MODEL", "glm-5p2");
        let other = GatewayConfig::from_env().expect("non-claude config");
        assert_eq!(other.context_window_tokens, 128_000);
    }

    #[test]
    fn context_window_env_overrides_the_default() {
        let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
        std::env::set_var("GATEWAY_API_KEY", "k");
        std::env::set_var("GATEWAY_MODEL", "claude-haiku-4.5");
        std::env::set_var("GATEWAY_CONTEXT_WINDOW", " 1000000 ");
        let cfg = GatewayConfig::from_env().expect("override must parse");
        assert_eq!(cfg.context_window_tokens, 1_000_000);
    }

    #[test]
    fn invalid_context_window_errors_naming_the_var() {
        let _guard = EnvGuard::lock(ALL_GATEWAY_KEYS.to_vec());
        std::env::set_var("GATEWAY_API_KEY", "k");
        std::env::set_var("GATEWAY_CONTEXT_WINDOW", "200k");
        let err = GatewayConfig::from_env().expect_err("bad window must fail");
        assert!(err.to_string().contains("GATEWAY_CONTEXT_WINDOW"), "{err}");
    }

    #[test]
    fn reply_reserve_is_max_tokens_plus_thinking() {
        let mut cfg = GatewayConfig::new("http://h/v1".to_owned(), "k".to_owned(), "m".to_owned());
        assert_eq!(cfg.reply_reserve_tokens(), 8192);
        cfg.default_max_tokens = Some(4096);
        assert_eq!(cfg.reply_reserve_tokens(), 4096);
        cfg.default_thinking_budget = Some(2048);
        assert_eq!(cfg.reply_reserve_tokens(), 6144);
        cfg.default_max_tokens = None;
        assert_eq!(cfg.reply_reserve_tokens(), 8192 + 2048);
    }
}
