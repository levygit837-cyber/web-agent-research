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

/// A link/image destination as htmd emits it: `<…>` when it contains spaces
/// (the content is not escaped, so it may hold `>`; the lazy run ends at the
/// first `>` that the rest of the pattern can close after), otherwise a run
/// of non-space chars in which `(` and `)` are backslash-escaped (`\(`, `\)`).
const DEST: &str = r#"(?:<[^\n]*?>|(?:\\.|[^)\s\\])+)"#;

/// Optional `"title"` after a destination.
const TITLE: &str = r#"(?:\s+"[^"]*")?"#;

/// Matches, left to right: an inline code span, a standalone image, or a
/// link. A link's text may itself contain images (htmd renders
/// `<a><img> Headline</a>` as `[![alt](src) Headline](href)`), so the text
/// group accepts image markup before any other char; the images in it are
/// resolved by [`replace_images`] afterwards.
/// MSRV 1.75 predates `LazyLock` (stable 1.80): `OnceLock` + `get_or_init`,
/// matching `web::search::ddg`'s existing static-regex convention.
fn inline_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"(?P<code>`[^`\n]*`)|(?P<img>!\[(?P<img_alt>[^\]]*)\]\((?P<img_dest>{DEST}){TITLE}\))|(?P<link>\[(?P<link_text>(?:!\[[^\]]*\]\({DEST}{TITLE}\)|[^\]])*)\]\((?P<link_dest>{DEST}){TITLE}\))"
        ))
        .expect("static fetch cleanup inline regex")
    })
}

/// A bare image, used to resolve images inside a link's text.
fn image_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(r"!\[(?P<alt>[^\]]*)\]\((?P<dest>{DEST}){TITLE}\)"))
            .expect("static fetch cleanup image regex")
    })
}

/// Only runs of printable ASCII count: base64/hex blobs, minified code and
/// image-proxy URLs are ASCII, while unspaced CJK prose (and its full-width
/// punctuation) breaks the run and is never collapsed.
fn long_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(r"[!-~]{{{LONG_TOKEN_CHARS},}}")).expect("static long-token regex")
    })
}

/// Clean fetched markdown in place of a direct pass-through: data URIs,
/// image/link markup, long non-prose tokens, and blank-line runs. Fenced
/// code and table lines are untouched.
pub(crate) fn clean_markdown(markdown: &str) -> String {
    let mut lines_out: Vec<String> = Vec::new();
    // Open fence: (marker char, run length). Closes only on a line of the
    // same char with at least that many, so a longer fence can carry a
    // shorter one inside it (htmd does exactly that for code showing fences).
    let mut fence: Option<(char, usize)> = None;
    let mut pending_blank = false;

    for line in markdown.split('\n') {
        let trimmed = line.trim_start();

        if let Some((ch, len)) = fence {
            lines_out.push(line.to_string());
            if is_fence_close(trimmed, ch, len) {
                fence = None;
            }
            continue;
        }

        if let Some(open) = fence_open(trimmed) {
            flush_pending_blank(&mut lines_out, &mut pending_blank);
            lines_out.push(line.to_string());
            fence = Some(open);
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

/// `Some((char, run))` when `trimmed` opens a fenced code block (3+ of `` ` ``
/// or `~`; a backtick fence's info string cannot contain a backtick).
fn fence_open(trimmed: &str) -> Option<(char, usize)> {
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let run = trimmed.chars().take_while(|&c| c == ch).count();
    if run < 3 || (ch == '`' && trimmed[run..].contains('`')) {
        return None;
    }
    Some((ch, run))
}

/// A closing fence is only `ch` repeated at least `min` times.
fn is_fence_close(trimmed: &str, ch: char, min: usize) -> bool {
    let run = trimmed.chars().take_while(|&c| c == ch).count();
    run >= min && trimmed[run..].trim().is_empty()
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
    let mut out = String::with_capacity(line.len());
    let mut last_end = 0;

    for caps in inline_re().captures_iter(line) {
        let whole = caps.get(0).expect("regex match always has group 0");
        out.push_str(&collapse_long_tokens(&line[last_end..whole.start()]));

        if caps.name("code").is_some() {
            out.push_str(whole.as_str());
        } else if let Some(dest) = caps.name("img_dest") {
            let alt = caps.name("img_alt").map_or("", |m| m.as_str());
            out.push_str(&image_text(alt, dest.as_str()));
        } else if let Some(dest) = caps.name("link_dest") {
            let raw = caps.name("link_text").map_or("", |m| m.as_str());
            let resolved = replace_images(raw);
            let text = if resolved == raw {
                resolved
            } else {
                resolved.trim().to_string()
            };
            push_link_text(&mut out, &text, dest.as_str());
        }

        last_end = whole.end();
    }
    out.push_str(&collapse_long_tokens(&line[last_end..]));
    out
}

/// Resolve every image in a link's text via [`image_text`].
fn replace_images(text: &str) -> String {
    image_re()
        .replace_all(text, |caps: &Captures<'_>| {
            image_text(&caps["alt"], &caps["dest"])
        })
        .into_owned()
}

/// A destination without htmd's `<…>` wrapper.
fn dest_inner(dest: &str) -> &str {
    dest.strip_prefix('<')
        .and_then(|d| d.strip_suffix('>'))
        .unwrap_or(dest)
}

/// Resolve one image to its final inline text: a data URI target becomes
/// `![alt](marker)` (per the issue's example), otherwise alt text alone,
/// or an empty string when there is no alt text (the agent never fetches
/// images, so an unlabeled one carries nothing to cite).
fn image_text(alt: &str, dest: &str) -> String {
    let url = dest_inner(dest);
    if is_data_uri(url) {
        return format!("![{alt}]({})", data_uri_label(url));
    }
    if alt.trim().is_empty() {
        String::new()
    } else {
        alt.to_string()
    }
}

/// Append a resolved link (`text`, raw `dest`) to `out`, applying the
/// data-URI-marker and tracking-param rules. A link whose resolved text is
/// empty (no visible text, or its only content was an alt-less image) is
/// dropped entirely rather than left as `[]()`.
fn push_link_text(out: &mut String, text: &str, dest: &str) {
    let url = dest_inner(dest);
    if is_data_uri(url) {
        out.push_str(&format!("[{text}]({})", data_uri_label(url)));
        return;
    }
    if text.trim().is_empty() {
        return;
    }
    let cleaned = strip_tracking_params(url);
    if url.len() == dest.len() {
        out.push_str(&format!("[{text}]({cleaned})"));
    } else {
        out.push_str(&format!("[{text}](<{cleaned}>)"));
    }
}

fn is_data_uri(url: &str) -> bool {
    // Byte comparison: slicing `url[..5]` would panic inside a multi-byte char.
    url.as_bytes()
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"data:"))
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

    #[test]
    fn non_ascii_link_targets_do_not_panic() {
        // Byte 5 of each target falls inside a multi-byte char.
        let md = "[x](/中文) and [](#見出し) and [y](/日本語のページ?utm_source=a)";
        assert_eq!(
            clean_markdown(md),
            "[x](/中文) and  and [y](/日本語のページ)"
        );
    }

    #[test]
    fn unspaced_cjk_prose_is_never_collapsed() {
        let md = "日本語の文章。".repeat(60); // 420 non-whitespace chars
        assert_eq!(clean_markdown(&md), md);
    }

    #[test]
    fn image_sharing_link_text_with_other_content_is_resolved() {
        let md = "[![logo](https://cdn.example/x.png) Headline](https://site.example/a?utm_source=x&id=1)";
        assert_eq!(
            clean_markdown(md),
            "[logo Headline](https://site.example/a?id=1)"
        );
    }

    #[test]
    fn escaped_parens_in_destinations_are_parsed() {
        let md = "![Ferris](https://x.example/File_\\(crab\\).png) text and [doc](https://x.example/a_\\(b\\)?utm_source=z)";
        assert_eq!(
            clean_markdown(md),
            "Ferris text and [doc](https://x.example/a_\\(b\\))"
        );
    }

    #[test]
    fn angle_wrapped_data_uri_with_spaces_never_reaches_output() {
        let md = "![icon](<data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' width='9'></svg>>)";
        let cleaned = clean_markdown(md);
        assert!(!cleaned.contains("<svg"), "payload leaked: {cleaned}");
        assert!(cleaned.starts_with("![icon](data:image/svg+xml omitted, "));
    }

    #[test]
    fn angle_wrapped_link_keeps_its_wrapper_and_strips_tracking() {
        let md = "[spaced](<https://x.example/a b?utm_source=z&q=1>)";
        assert_eq!(clean_markdown(md), "[spaced](<https://x.example/a b?q=1>)");
    }

    #[test]
    fn longer_fence_carries_a_shorter_fence_byte_identical() {
        let md = "````markdown\n```rust\n![x](data:image/png;base64,AAAA)\n```\n````\n\nafter ![i](data:image/png;base64,AAAA)";
        let cleaned = clean_markdown(md);
        assert!(cleaned.starts_with(
            "````markdown\n```rust\n![x](data:image/png;base64,AAAA)\n```\n````\n\nafter "
        ));
        assert!(cleaned.ends_with("![i](data:image/png omitted, 3 bytes)"));
    }

    #[test]
    fn tilde_fence_carrying_backtick_fences_is_byte_identical() {
        let md = "~~~\n```\n![x](data:image/png;base64,AAAA)\n```\n~~~";
        assert_eq!(clean_markdown(md), md);
    }
}
