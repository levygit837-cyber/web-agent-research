//! Chrome-family desktop browser profiles for `web::search` legs (#59): a
//! coherent UA + client-hint set, one drawn at random per request chain
//! (DDG pagination re-POSTs and the Startpage homepage -> search POST both
//! reuse the same profile, because DDG's `vqd` is bound to the UA).
//!
//! Firefox/Safari profiles are out of scope while the transport is plain
//! `reqwest`/rustls: a non-Chrome UA over a generic rustls TLS fingerprint
//! is a mismatch signal in itself (`docs/harness.md` "Browser profile
//! bump").
//!
//! The `sec-ch-ua` GREASE brand list ports Chromium's
//! `GetGreasedUserAgentBrandVersion`/`GenerateBrandVersionList`
//! (`components/embedder_support/user_agent_utils.cc`) via Obscura's Rust
//! reimplementation (`crates/obscura-net/src/client.rs:991-1029`,
//! Apache-2.0, h4ckf0r0day/obscura@542df14). See `THIRD-PARTY-NOTICES.md`.

use rand::seq::IndexedRandom;

/// Desktop platform a profile emulates. The UA's OS token and
/// `sec-ch-ua-platform` both derive from this so the two hints never
/// disagree (#59 coherence test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    MacOs,
    Windows,
    Linux,
}

impl Platform {
    fn ua_os_token(self) -> &'static str {
        match self {
            Platform::MacOs => "Macintosh; Intel Mac OS X 10_15_7",
            Platform::Windows => "Windows NT 10.0; Win64; x64",
            Platform::Linux => "X11; Linux x86_64",
        }
    }

    fn sec_ch_ua_platform(self) -> &'static str {
        match self {
            Platform::MacOs => "\"macOS\"",
            Platform::Windows => "\"Windows\"",
            Platform::Linux => "\"Linux\"",
        }
    }
}

/// One coherent Chrome-family header set. "Coherent" (#59 acceptance): the
/// UA's `Chrome/{major}` token equals `major`, and equals the `Chromium`/
/// `Google Chrome` versions inside `sec_ch_ua`; `platform` matches
/// `sec_ch_ua_platform` and the UA's OS token.
#[derive(Debug, Clone)]
pub(crate) struct BrowserProfile {
    /// Read only by the `#[cfg(test)]` coherence checks below; kept on the
    /// struct (not derived from `user_agent`/`sec_ch_ua` at use time) so a
    /// profile is one source of truth for "which Chrome major/platform is
    /// this", not a string to re-parse.
    #[allow(dead_code)]
    pub(crate) major: u32,
    #[allow(dead_code)]
    pub(crate) platform: Platform,
    pub(crate) user_agent: String,
    pub(crate) sec_ch_ua: String,
    pub(crate) sec_ch_ua_mobile: &'static str,
    pub(crate) sec_ch_ua_platform: &'static str,
    pub(crate) accept: &'static str,
    pub(crate) accept_encoding: &'static str,
    pub(crate) accept_language: &'static str,
}

/// Chrome stable + the 2 previous majors, observed 2026-09-28 via
/// `versionhistory.googleapis.com/v1/chrome/platforms/{mac,win,linux}/channels/stable/versions`
/// (stable `155.0.8059.12`). Bump procedure: `docs/harness.md` "Browser
/// profile bump" -- there's also an ignored live test
/// (`profile_table_matches_chrome_stable`) that fails once this lags.
const STABLE_MAJORS: [u32; 3] = [155, 154, 153];

const PLATFORMS: [Platform; 3] = [Platform::MacOs, Platform::Windows, Platform::Linux];

/// Build the full profile table: `STABLE_MAJORS` x `PLATFORMS`, 9 entries.
/// Built fresh per call (9 short owned `String`s); called once per request
/// chain, never in a hot loop.
pub(crate) fn profiles() -> Vec<BrowserProfile> {
    STABLE_MAJORS
        .iter()
        .flat_map(|&major| {
            PLATFORMS
                .iter()
                .map(move |&platform| build_profile(major, platform))
        })
        .collect()
}

fn build_profile(major: u32, platform: Platform) -> BrowserProfile {
    BrowserProfile {
        major,
        platform,
        user_agent: format!(
            "Mozilla/5.0 ({}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36",
            platform.ua_os_token(),
        ),
        sec_ch_ua: chrome_client_hints(major),
        sec_ch_ua_mobile: "?0",
        sec_ch_ua_platform: platform.sec_ch_ua_platform(),
        accept: "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        accept_encoding: "gzip, deflate, br, zstd",
        accept_language: crate::web::ACCEPT_LANGUAGE,
    }
}

/// Chrome's GREASE `sec-ch-ua` brand-list algorithm: deterministic per major
/// version (Chromium seeds both the placeholder brand string and the
/// 3-entry shuffle order from the major version number, not from a fresh
/// random draw per request). Ported from Obscura `chrome_client_hints`
/// (`crates/obscura-net/src/client.rs:991-1029`, Apache-2.0,
/// h4ckf0r0day/obscura@542df14), itself a port of Chromium's
/// `GetGreasedUserAgentBrandVersion` (`greasey_chars`/`greased_versions`)
/// and `GenerateBrandVersionList`/`ShuffleBrandList`/`GetRandomOrder`
/// (`components/embedder_support/user_agent_utils.cc`, size-3 case: the
/// same 6-permutation `orders` table).
fn chrome_client_hints(major: u32) -> String {
    const GREASE_CHARS: [char; 11] = [' ', '(', ':', '-', '.', '/', ')', ';', '=', '?', '_'];
    const GREASE_VER: [&str; 3] = ["8", "99", "24"];
    const PERMS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let seed = major as usize;
    let grease_brand = format!(
        "Not{}A{}Brand",
        GREASE_CHARS[seed % GREASE_CHARS.len()],
        GREASE_CHARS[(seed + 1) % GREASE_CHARS.len()],
    );
    let brands = [
        (
            grease_brand,
            GREASE_VER[seed % GREASE_VER.len()].to_string(),
        ),
        ("Chromium".to_string(), major.to_string()),
        ("Google Chrome".to_string(), major.to_string()),
    ];
    let order = PERMS[seed % PERMS.len()];
    order
        .iter()
        .map(|&i| format!("\"{}\";v=\"{}\"", brands[i].0, brands[i].1))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Pick one profile at random for a request chain (#59). Callers pick once
/// per chain (one `ddg_search`/`startpage_search` leg) and pass the same
/// profile to every request in that chain.
pub(crate) fn pick_profile() -> BrowserProfile {
    profiles()
        .choose(&mut rand::rng())
        .cloned()
        .expect("STABLE_MAJORS x PLATFORMS is never empty")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_is_internally_coherent() {
        for profile in profiles() {
            let major = profile.major;
            assert!(
                profile
                    .user_agent
                    .contains(&format!("Chrome/{major}.0.0.0")),
                "UA major mismatch: {}",
                profile.user_agent
            );
            assert!(
                profile
                    .sec_ch_ua
                    .contains(&format!("\"Chromium\";v=\"{major}\"")),
                "sec-ch-ua Chromium version mismatch: {}",
                profile.sec_ch_ua
            );
            assert!(
                profile
                    .sec_ch_ua
                    .contains(&format!("\"Google Chrome\";v=\"{major}\"")),
                "sec-ch-ua Google Chrome version mismatch: {}",
                profile.sec_ch_ua
            );
            let (expected_platform_hint, expected_os_token) = match profile.platform {
                Platform::MacOs => ("\"macOS\"", "Macintosh"),
                Platform::Windows => ("\"Windows\"", "Windows NT"),
                Platform::Linux => ("\"Linux\"", "X11; Linux"),
            };
            assert_eq!(profile.sec_ch_ua_platform, expected_platform_hint);
            assert!(
                profile.user_agent.contains(expected_os_token),
                "UA platform token mismatch: {}",
                profile.user_agent
            );
        }
    }

    #[test]
    fn table_has_nine_profiles_three_majors_three_platforms() {
        let all = profiles();
        assert_eq!(all.len(), 9);
        for major in STABLE_MAJORS {
            assert_eq!(all.iter().filter(|p| p.major == major).count(), 3);
        }
        for platform in PLATFORMS {
            assert_eq!(all.iter().filter(|p| p.platform == platform).count(), 3);
        }
    }

    #[test]
    fn pick_profile_always_returns_a_table_entry() {
        let table = profiles();
        for _ in 0..20 {
            let picked = pick_profile();
            assert!(table
                .iter()
                .any(|p| p.major == picked.major && p.platform == picked.platform));
        }
    }

    /// Ignored live test (#59): fails once `STABLE_MAJORS` lags behind
    /// Chrome stable. Bump procedure: `docs/harness.md` "Browser profile
    /// bump". Run with `cargo test -- --ignored profile_table_matches_chrome_stable`.
    #[test]
    #[ignore = "live network call to versionhistory.googleapis.com"]
    fn profile_table_matches_chrome_stable() {
        let body: String = std::process::Command::new("curl")
            .arg("-s")
            .arg("https://versionhistory.googleapis.com/v1/chrome/platforms/mac/channels/stable/versions?pageSize=1")
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .expect("curl available for this ignored live test");
        let live_major: u32 = body
            .split("\"version\": \"")
            .nth(1)
            .and_then(|rest| rest.split('.').next())
            .and_then(|s| s.parse().ok())
            .expect("versionhistory.googleapis.com response has a parseable version");
        let newest_in_table = STABLE_MAJORS.iter().copied().max().unwrap();
        assert_eq!(
            newest_in_table, live_major,
            "STABLE_MAJORS newest entry ({newest_in_table}) lags Chrome stable ({live_major}); bump per docs/harness.md"
        );
    }
}
