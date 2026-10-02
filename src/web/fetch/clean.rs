//! Shared markdown noise-reduction pass (#79) applied to the result of both
//! fetch paths ([`super::fetcher::Fetcher`]'s static path and
//! [`super::obscura::Obscura`]) before it is wrapped in `Evidence`.
//!
//! Rules, in the order applied per non-fenced, non-table line:
//! - A data URI (`data:<mime>;base64,…` or `data:<mime>,…`) used as an
//!   image or link target is replaced by a short marker carrying the mime
//!   type and the decoded size; the payload never reaches the output.
//! - An image keeps only its alt text (the agent never fetches images, so
//!   the URL is dropped); an image with no alt text is dropped entirely.
//! - A link's target keeps its path and non-tracking query params but
//!   drops known tracking params (`utm_*`, `fbclid`, `gclid`, `ref_src`,
//!   …); a link whose visible text is empty (directly, or because its only
//!   content was an alt-less image) is dropped entirely, since the agent
//!   has nothing to cite it by.
//! - A single non-prose token (long base64/hex blob, minified JSON/JS
//!   leaking into text, …) of [`LONG_TOKEN_CHARS`] or more is collapsed to
//!   a short "omitted" marker. This never touches link/image targets
//!   (which keep their full URL beyond tracking-param stripping, per the
//!   rule above) or inline code spans.
//! - A run of 2+ blank lines collapses to one.
//!
//! Fenced code blocks (``` ``` ``` / `~~~`) and table rows (lines starting
//! with `|`) are copied through byte-identical: prose, code, and tables are
//! never altered, only the markup around them.
//!
//! This is a line-oriented regex pass, not a full CommonMark parser: link
//! text containing a literal `]`, or multi-line link targets, are outside
//! its scope. Real fetched pages (htmd's static-path output and Obscura's
//! markdown dump) do not produce those shapes.

use std::sync::OnceLock;

use regex::{Captures, Regex};

/// A single non-prose token at or above this length is collapsed. Matches
/// the issue's measured examples (MDN's 861-char "report a problem" URL,
/// GitHub's 214+-char `camo.githubusercontent.com` image-proxy tokens).
const LONG_TOKEN_CHARS: usize = 200;

/// Exact tracking-param names stripped from every kept link target, beyond
/// the `utm_*` prefix (checked separately in [`is_tracking_param`]).
const TRACKING_PARAM_NAMES: &[&str] = &[
    "fbclid", "gclid", "ref_src", "ref_url", "mc_cid", "mc_eid", "igshid", "icid", "cmpid",
];

/// `utm_*` by prefix, plus the exact names in [`TRACKING_PARAM_NAMES`],
/// case-insensitive.
fn is_tracking_param(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("utm_") || TRACKING_PARAM_NAMES.contains(&lower.as_str())
}

/// Matches, left to right: an inline code span, a link whose entire visible
/// content is one image (htmd/Obscura render `<a><img></a>` as
/// `[![alt](src)](href)`; tried before the plain `link`/`img` alternatives
/// so the inner image's own `]`/`)` never gets mis-parsed as the outer
/// link's closing delimiters), a standalone image, or a standalone link.
/// MSRV 1.75 predates `LazyLock` (stable 1.80): `OnceLock` + `get_or_init`,
/// matching `web::search::ddg`'s existing static-regex convention.
fn inline_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?P<code>`[^`\n]*`)|(?P<linkimg>\[!\[(?P<li_alt>[^\]]*)\]\((?P<li_imgurl>[^)\s]+)(?:\s+"[^"]*")?\)\]\((?P<li_url>[^)\s]+)(?:\s+"[^"]*")?\))|(?P<img>!\[(?P<img_alt>[^\]]*)\]\((?P<img_url>[^)\s]+)(?:\s+"[^"]*")?\))|(?P<link>\[(?P<link_text>[^\]]*)\]\((?P<link_url>[^)\s]+)(?:\s+"[^"]*")?\))"#,
        )
        .expect("static fetch cleanup inline regex")
    })
}

fn long_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(r"\S{{{LONG_TOKEN_CHARS},}}")).expect("static long-token regex")
    })
}

/// Clean fetched markdown in place of a direct pass-through: data URIs,
/// image/link markup, long non-prose tokens, and blank-line runs. Fenced
/// code and table lines are untouched.
pub(crate) fn clean_markdown(markdown: &str) -> String {
    let mut lines_out: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut pending_blank = false;

    for line in markdown.split('\n') {
        let trimmed = line.trim_start();
        let is_fence_delim = trimmed.starts_with("```") || trimmed.starts_with("~~~");

        if in_fence {
            lines_out.push(line.to_string());
            if is_fence_delim {
                in_fence = false;
            }
            continue;
        }

        if is_fence_delim {
            flush_pending_blank(&mut lines_out, &mut pending_blank);
            lines_out.push(line.to_string());
            in_fence = true;
            continue;
        }

        if trimmed.starts_with('|') {
            flush_pending_blank(&mut lines_out, &mut pending_blank);
            lines_out.push(line.to_string());
            continue;
        }

        if trimmed.is_empty() {
            if !lines_out.is_empty() {
                pending_blank = true;
            }
            continue;
        }

        flush_pending_blank(&mut lines_out, &mut pending_blank);
        lines_out.push(clean_inline(line));
    }

    lines_out.join("\n")
}

fn flush_pending_blank(lines_out: &mut Vec<String>, pending_blank: &mut bool) {
    if *pending_blank {
        lines_out.push(String::new());
        *pending_blank = false;
    }
}

/// Apply the image/link/data-URI/long-token rules to one non-fenced,
/// non-table line.
fn clean_inline(line: &str) -> String {
    let re = inline_re();
    let mut out = String::with_capacity(line.len());
    let mut last_end = 0;

    for caps in re.captures_iter(line) {
        let whole = caps.get(0).expect("regex match always has group 0");
        out.push_str(&collapse_long_tokens(&line[last_end..whole.start()]));

        if caps.name("code").is_some() {
            out.push_str(whole.as_str());
        } else if let Some(li_url) = caps.name("li_url") {
            let alt = caps.name("li_alt").map(|m| m.as_str()).unwrap_or("");
            let img_url = caps.name("li_imgurl").map(|m| m.as_str()).unwrap_or("");
            let text = image_text(alt, img_url);
            push_link_text(&mut out, &text, li_url.as_str());
        } else if let Some(img_url) = caps.name("img_url") {
            let alt = caps.name("img_alt").map(|m| m.as_str()).unwrap_or("");
            out.push_str(&image_text(alt, img_url.as_str()));
        } else if let Some(link_url) = caps.name("link_url") {
            let text = caps.name("link_text").map(|m| m.as_str()).unwrap_or("");
            push_link_text(&mut out, text, link_url.as_str());
        }

        last_end = whole.end();
    }
    out.push_str(&collapse_long_tokens(&line[last_end..]));
    out
}

/// Resolve one image to its final inline text: a data URI target becomes
/// `![alt](marker)` (per the issue's example), otherwise alt text alone,
/// or an empty string when there is no alt text (the agent never fetches
/// images, so an unlabeled one carries nothing to cite).
fn image_text(alt: &str, url: &str) -> String {
    if is_data_uri(url) {
        return format!("![{alt}]({})", data_uri_label(url));
    }
    if alt.trim().is_empty() {
        String::new()
    } else {
        alt.to_string()
    }
}

/// Append a resolved link (`text`, raw `url`) to `out`, applying the
/// data-URI-marker and tracking-param rules. A link whose resolved text is
/// empty (no visible text, or its only content was an alt-less image) is
/// dropped entirely rather than left as `[]()`.
fn push_link_text(out: &mut String, text: &str, url: &str) {
    if is_data_uri(url) {
        out.push_str(&format!("[{text}]({})", data_uri_label(url)));
        return;
    }
    if !text.trim().is_empty() {
        out.push_str(&format!("[{text}]({})", strip_tracking_params(url)));
    }
}

fn is_data_uri(url: &str) -> bool {
    url.len() >= 5 && url[..5].eq_ignore_ascii_case("data:")
}

/// `data:image/png;base64,AAAA…` -> `data:image/png omitted, N bytes|KB`.
/// Size is the decoded payload size (3/4 of the base64 length), not the
/// markup length, so the marker reflects what was actually inlined.
fn data_uri_label(url: &str) -> String {
    let rest = &url[5.min(url.len())..]; // strip "data:"
    let (meta, payload) = rest.split_once(',').unwrap_or((rest, ""));
    let mime = meta.split(';').next().unwrap_or("").trim();
    let mime = if mime.is_empty() { "unknown" } else { mime };
    let is_base64 = meta
        .split(';')
        .any(|part| part.eq_ignore_ascii_case("base64"));
    let bytes = if is_base64 {
        payload.len() * 3 / 4
    } else {
        payload.len()
    };
    format!("data:{mime} omitted, {}", human_size(bytes))
}

fn human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} bytes")
    } else {
        format!("{} KB", (bytes + 512) / 1024)
    }
}

/// Drop tracking query params from `url`, keeping its path, every other
/// param, and the fragment untouched. A URL with no `?` is returned as-is.
fn strip_tracking_params(url: &str) -> String {
    let Some((base, rest)) = url.split_once('?') else {
        return url.to_string();
    };
    let (query, fragment) = match rest.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (rest, None),
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let name = pair.split('=').next().unwrap_or("");
            !is_tracking_param(name)
        })
        .collect();

    let mut out = base.to_string();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// Replace every run of [`LONG_TOKEN_CHARS`]+ non-whitespace chars in plain
/// (non-link, non-code) text with a short marker.
fn collapse_long_tokens(text: &str) -> String {
    long_token_re()
        .replace_all(text, |caps: &Captures<'_>| {
            format!("[long token omitted, {} chars]", caps[0].chars().count())
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_uri_image_never_reaches_output() {
        let payload = "A".repeat(1000); // 1000 base64 chars -> 750 decoded bytes
        let md = format!("![logo](data:image/png;base64,{payload})");
        let cleaned = clean_markdown(&md);
        assert!(!cleaned.contains(&payload), "payload leaked: {cleaned}");
        assert!(!cleaned.to_ascii_lowercase().contains("base64"));
        assert_eq!(cleaned, "![logo](data:image/png omitted, 750 bytes)");
    }

    #[test]
    fn data_uri_link_never_reaches_output() {
        let payload = "B".repeat(20_000); // -> 15000 decoded bytes -> 15 KB
        let md = format!("[download](data:application/octet-stream;base64,{payload})");
        let cleaned = clean_markdown(&md);
        assert!(!cleaned.contains(&payload), "payload leaked: {cleaned}");
        assert_eq!(
            cleaned,
            "[download](data:application/octet-stream omitted, 15 KB)"
        );
    }

    #[test]
    fn non_base64_data_uri_uses_literal_payload_length() {
        let md = "[note](data:text/plain,hello%20world)";
        assert_eq!(
            clean_markdown(md),
            "[note](data:text/plain omitted, 13 bytes)"
        );
    }

    #[test]
    fn image_with_alt_keeps_only_alt_text() {
        let md = "![a cat sleeping](https://cdn.example.com/cat.jpg)";
        assert_eq!(clean_markdown(md), "a cat sleeping");
    }

    #[test]
    fn image_without_alt_is_dropped() {
        let md = "before ![](https://cdn.example.com/pixel.png) after";
        assert_eq!(clean_markdown(md), "before  after");
    }

    #[test]
    fn tracked_link_keeps_destination_without_tracking_params() {
        let md = "[docs](https://example.com/page?id=3&utm_source=newsletter&utm_medium=email&fbclid=abc#frag)";
        assert_eq!(
            clean_markdown(md),
            "[docs](https://example.com/page?id=3#frag)"
        );
    }

    #[test]
    fn link_with_only_tracking_params_drops_the_query_entirely() {
        let md = "[go](https://example.com/path?utm_source=x&gclid=y)";
        assert_eq!(clean_markdown(md), "[go](https://example.com/path)");
    }

    #[test]
    fn link_without_tracking_params_is_unchanged() {
        let md = "[docs](https://example.com/page?id=3&lang=en)";
        assert_eq!(clean_markdown(md), md);
    }

    #[test]
    fn link_with_empty_text_is_dropped() {
        let md = "before [](https://example.com/x) after";
        assert_eq!(clean_markdown(md), "before  after");
    }

    #[test]
    fn link_wrapping_an_alt_less_image_keeps_nothing() {
        // `[![](img)](page)`: the alt-less image drops first, leaving the
        // link with empty text, which drops too.
        let md = "[![](https://cdn.example.com/i.png)](https://example.com/page)";
        assert_eq!(clean_markdown(md), "");
    }

    #[test]
    fn link_wrapping_an_alt_image_keeps_alt_as_link_text() {
        let md = "[![a chart](https://cdn.example.com/i.png)](https://example.com/page)";
        assert_eq!(clean_markdown(md), "[a chart](https://example.com/page)");
    }

    #[test]
    fn fenced_code_with_data_uri_and_long_token_is_byte_identical() {
        let long_token = "x".repeat(300);
        let md = format!(
            "prose before\n\n```text\nsrc=\"data:image/png;base64,AAAA\"\n{long_token}\n```\n\nprose after"
        );
        let cleaned = clean_markdown(&md);
        assert!(
            cleaned.contains(&format!(
                "```text\nsrc=\"data:image/png;base64,AAAA\"\n{long_token}\n```"
            )),
            "fenced block was altered: {cleaned}"
        );
    }

    #[test]
    fn fenced_code_with_tilde_fence_is_byte_identical() {
        let md = "~~~\n[link](data:text/plain,should-not-change)\n~~~";
        assert_eq!(clean_markdown(md), md);
    }

    #[test]
    fn table_row_is_byte_identical() {
        let md = "| [x](https://example.com?utm_source=z) | ![](img.png) |\n| --- | --- |";
        assert_eq!(clean_markdown(md), md);
    }

    #[test]
    fn inline_code_span_is_untouched() {
        let token = "y".repeat(250);
        let md = format!("prose `data:x,{token}` more prose");
        assert_eq!(clean_markdown(&md), md);
    }

    #[test]
    fn long_non_prose_token_in_plain_text_is_collapsed() {
        let token = "deadbeef".repeat(40); // 320 chars, no spaces
        let md = format!("start {token} end");
        let cleaned = clean_markdown(&md);
        assert!(!cleaned.contains(&token), "token leaked: {cleaned}");
        assert_eq!(
            cleaned,
            format!("start [long token omitted, {} chars] end", token.len())
        );
    }

    #[test]
    fn short_tokens_are_never_collapsed() {
        let md = "a normal sentence with https://example.com/short?utm_source=z as prose";
        assert_eq!(clean_markdown(md), md);
    }

    #[test]
    fn link_target_long_token_is_never_collapsed() {
        // Long-token collapsing must not reach into a kept link target: the
        // issue's rule for links is tracking-param stripping only.
        let long_path = "a".repeat(300);
        let md = format!("[report](https://example.com/{long_path})");
        assert_eq!(clean_markdown(&md), md);
    }

    #[test]
    fn blank_line_run_collapses_to_one() {
        let md = "para one\n\n\n\n\npara two";
        assert_eq!(clean_markdown(md), "para one\n\npara two");
    }

    #[test]
    fn single_blank_line_is_unchanged() {
        let md = "para one\n\npara two";
        assert_eq!(clean_markdown(md), md);
    }

    #[test]
    fn prose_and_fenced_code_and_table_coexist_untouched_except_prose() {
        let md = "# Title\n\nSome prose with a [tracked link](https://example.com?utm_source=z).\n\n```rust\nlet utm_source = \"data:kept\";\n```\n\n| A | B |\n| --- | --- |\n| [x](https://example.com?utm_source=z) | 2 |";
        let cleaned = clean_markdown(md);
        assert!(cleaned.contains("[tracked link](https://example.com)"));
        assert!(cleaned.contains("let utm_source = \"data:kept\";"));
        assert!(cleaned.contains("| [x](https://example.com?utm_source=z) | 2 |"));
    }
}
