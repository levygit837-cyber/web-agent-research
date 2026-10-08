//! Detection of a rendered anti-bot interstitial returned as page content (#126).

/// Longest markdown, in chars, still treated as an interstitial. The
/// recorded Cloudflare page is about 400 chars; a real article that quotes
/// the same phrase is far longer.
const MAX_INTERSTITIAL_CHARS: usize = 1_500;

/// True when `markdown` is a short Cloudflare "security verification" page
/// rather than content: the verification heading, the "waiting for" status
/// line, or the Ray ID footer with Cloudflare's attribution.
pub(super) fn is_interstitial_markdown(markdown: &str) -> bool {
    let trimmed = markdown.trim();
    if trimmed.chars().count() > MAX_INTERSTITIAL_CHARS {
        return false;
    }
    let lowered = trimmed.to_lowercase();
    lowered.contains("performing security verification")
        || lowered.contains("verification successful. waiting for")
        || (lowered.contains("ray id:") && lowered.contains("performance and security by"))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const RECORDED: &str = "# stackoverflow.com\n## Performing security verification\nThis website uses a security service to protect against malicious bots. This page is displayed while the website verifies you are not a bot.\n## Verification successful. Waiting for stackoverflow.com to respond\nRay ID: `a45f0daa58bc0675`\nPerformance and Security by [Cloudflare](https://www.cloudflare.com)";

    #[test]
    fn recorded_interstitial_is_detected() {
        assert!(is_interstitial_markdown(RECORDED));
    }

    #[test]
    fn footer_alone_is_detected() {
        assert!(is_interstitial_markdown(
            "Ray ID: `abc`\nPerformance and Security by [Cloudflare](https://www.cloudflare.com)"
        ));
    }

    #[test]
    fn long_article_mentioning_the_phrase_is_not() {
        let article = format!(
            "# Bot defenses\n\nCloudflare shows \"Performing security verification\" and \"Verification successful. Waiting for example.com to respond\". {}",
            "Real article prose about bot mitigation. ".repeat(60)
        );
        assert!(!is_interstitial_markdown(&article));
    }

    #[test]
    fn short_unrelated_page_is_not() {
        assert!(!is_interstitial_markdown("# Title\n\nbody"));
    }
}
