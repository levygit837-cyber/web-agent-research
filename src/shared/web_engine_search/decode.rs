//! Shared decoding codecs for the fetch-only web search engine.
//!
//! Pure string transforms used by both provider legs: HTML-tag
//! stripping, entity decoding, whitespace collapsing and form-style
//! percent coding. No HTTP, no regex, no scraper.

/// Decode HTML text: strip inline tags (Omp `decodeHtmlText` `<b>`
/// highlights), decode named + numeric entities, normalise whitespace.
pub(crate) fn decode_html_text(raw: &str) -> String {
    let no_tags = strip_tags(raw);
    decode_entities(&no_tags)
}

pub(crate) fn strip_tags(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut inside = false;
    for ch in raw.chars() {
        match ch {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    out
}

pub(crate) fn decode_entities(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let semi = match after.find(';') {
            Some(pos) if pos <= 32 => pos,
            _ => {
                // No `;` within entity range: bare `&` (e.g. "AT&T
                // tails" or a truncated tail) stays literal.
                out.push('&');
                rest = after;
                continue;
            }
        };
        let entity = &after[..semi];
        let decoded = match entity {
            "amp" => Some("&".to_string()),
            "lt" => Some("<".to_string()),
            "gt" => Some(">".to_string()),
            "quot" => Some("\"".to_string()),
            "apos" => Some("'".to_string()),
            "nbsp" => Some(" ".to_string()),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(entity[2..].trim(), 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
            }
            _ if entity.starts_with('#') => entity[1..]
                .trim()
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string()),
            _ => None,
        };
        match decoded {
            Some(text) => {
                out.push_str(&text);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    collapse_whitespace(&out)
}

pub(crate) fn collapse_whitespace(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub(crate) fn percent_decode(raw: &str) -> String {
    let mut bytes_out: Vec<u8> = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                bytes_out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            bytes_out.push(b' ');
        } else {
            bytes_out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&bytes_out).into_owned()
}

pub(crate) fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
