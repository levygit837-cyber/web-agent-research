//! Gateway configuration: endpoint, credentials, and per-model parameters.
//!
//! Defaults target the local dev gateway; the API key always comes from the
//! environment and is never logged (`Debug` redacts it).

use std::collections::HashMap;
use std::time::Duration;

/// Per-model parameter overrides. All fields optional; unset falls back
/// to the gateway-level default.
#[derive(Debug, Clone, Default)]
pub struct ModelParams {
    /// Passthrough reasoning-effort string, e.g. "high", "low".
    pub reasoning_effort: Option<String>,
    /// Token cap sent as `max_completion_tokens`.
    pub max_tokens: Option<u32>,
}

/// Fully resolved parameters for one request.
#[derive(Debug, Clone)]
pub struct ResolvedParams {
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub max_tokens: Option<u32>,
}

#[derive(Clone)]
pub struct GatewayConfig {
    /// Base URL, e.g. "http://localhost:8317/v1". No trailing slash assumed;
    /// constructors trim one trailing `/`.
    pub base_url: String,
    /// Bearer token. Never logged; `Debug` redacts it (manual impl below).
    api_key: String,
    /// Default model id used when the caller does not pick one.
    pub model: String,
    /// Gateway-wide defaults; per-model entries in `model_overrides` win.
    pub default_reasoning_effort: Option<String>,
    pub default_max_tokens: Option<u32>,
    /// model id -> overrides. Precedence: override > default > omitted.
    pub model_overrides: HashMap<String, ModelParams>,
    /// Per-attempt request timeout.
    pub request_timeout: Duration,
    /// Max total attempts including the first try.
    pub max_attempts: u32,
}

impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("default_reasoning_effort", &self.default_reasoning_effort)
            .field("default_max_tokens", &self.default_max_tokens)
            .field("model_overrides", &self.model_overrides)
            .field("request_timeout", &self.request_timeout)
            .field("max_attempts", &self.max_attempts)
            .finish()
    }
}

fn trim_one_slash(url: &str) -> &str {
    url.strip_suffix('/').unwrap_or(url)
}

impl GatewayConfig {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            base_url: trim_one_slash(&base_url).to_owned(),
            api_key,
            model,
            default_reasoning_effort: None,
            default_max_tokens: None,
            model_overrides: HashMap::new(),
            request_timeout: Duration::from_secs(60),
            max_attempts: 3,
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
        let base_url = env_or("GATEWAY_BASE_URL", "http://localhost:8317/v1");
        let model = env_or("GATEWAY_MODEL", "glm-5p2");
        let default_reasoning_effort = std::env::var("GATEWAY_REASONING_EFFORT")
            .map(|v| v.trim().to_owned())
            .ok()
            .filter(|v| !v.is_empty());
        let default_max_tokens = std::env::var("GATEWAY_MAX_TOKENS")
            .map(|v| v.trim().to_owned())
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| {
                v.parse::<u32>()
                    .map_err(|_| anyhow::anyhow!("GATEWAY_MAX_TOKENS is not a valid u32: {v:?}"))
            })
            .transpose()?;
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

        Ok(Self {
            base_url: trim_one_slash(&base_url).to_owned(),
            api_key,
            model,
            default_reasoning_effort,
            default_max_tokens,
            model_overrides: HashMap::new(),
            request_timeout,
            max_attempts,
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

    pub fn effective_params(&self, model: &str) -> ResolvedParams {
        let override_params = self.model_overrides.get(model);
        ResolvedParams {
            model: model.to_owned(),
            reasoning_effort: override_params
                .and_then(|o| o.reasoning_effort.clone())
                .or_else(|| self.default_reasoning_effort.clone()),
            max_tokens: override_params
                .and_then(|o| o.max_tokens)
                .or(self.default_max_tokens),
        }
    }

    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::gateway::reply::ChatRequest;

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
                reasoning_effort: Some("high".to_owned()),
                max_tokens: Some(4096),
            },
        );
        // Gateway-level default behind the override.
        let mut cfg = cfg;
        cfg.default_reasoning_effort = Some("low".to_owned());

        let hit = cfg.effective_params("glm-5p2");
        assert_eq!(hit.model, "glm-5p2");
        assert_eq!(hit.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(hit.max_tokens, Some(4096));

        let miss = cfg.effective_params("other");
        assert_eq!(miss.model, "other");
        assert_eq!(miss.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(miss.max_tokens, None);
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

        let body = serde_json::to_value(ChatRequest::from_resolved(&params, vec![]))
            .expect("request must serialize");
        assert!(
            !body
                .as_object()
                .expect("body is an object")
                .contains_key("reasoning_effort"),
            "unset reasoning_effort must be omitted, got {body}"
        );
        assert!(
            !body
                .as_object()
                .expect("body is an object")
                .contains_key("max_completion_tokens"),
            "unset max_completion_tokens must be omitted, got {body}"
        );
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
}
