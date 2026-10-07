//! Web layer: search (Hits) and fetch (Evidence). No LLM, no Session.
//!
//! ADR-0006: `research` depends on `web`; `web` never imports `research` or `llm`.

pub(crate) mod cache_dir;
pub(crate) mod crates_io;
pub mod fetch;
pub(crate) mod profile;
pub mod search;

/// `Accept-Language` header value shared by every `web/` HTTP client.
pub(crate) const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

/// Env var that overrides the contact in [`api_user_agent`].
pub(crate) const CONTACT_ENV: &str = "WEB_AGENT_RESEARCH_CONTACT";

/// Contact sent to keyless APIs when [`CONTACT_ENV`] is unset or blank.
pub(crate) const DEFAULT_CONTACT: &str = "https://github.com/levygit837-cyber/web-agent-research";

/// Honest User-Agent for documented keyless APIs (crates.io, #109), whose
/// data-access policy asks for the application name and a contact:
/// `web-agent-research/<version> (+<contact>)`. Never sent to the scraped
/// HTML engines, which get a Chrome navigation profile instead.
pub(crate) fn api_user_agent() -> String {
    let contact = std::env::var(CONTACT_ENV).unwrap_or_default();
    let contact = match contact.trim() {
        "" => DEFAULT_CONTACT,
        set => set,
    };
    format!(
        "web-agent-research/{} (+{contact})",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EnvGuard;

    #[test]
    fn api_user_agent_names_the_app_and_a_contact() {
        let _guard = EnvGuard::lock(vec![CONTACT_ENV]);
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(
            api_user_agent(),
            format!("web-agent-research/{version} (+{DEFAULT_CONTACT})")
        );
        std::env::set_var(CONTACT_ENV, "  ");
        assert!(api_user_agent().ends_with(&format!("(+{DEFAULT_CONTACT})")));
        std::env::set_var(CONTACT_ENV, " mailto:ops@example.org ");
        assert_eq!(
            api_user_agent(),
            format!("web-agent-research/{version} (+mailto:ops@example.org)")
        );
    }
}
