//! Egress policy for every fetch (#97): the agent may only reach public
//! addresses. One address table ([`is_forbidden_ip`]) is applied at four
//! points, so a private target is refused however the URL reaches it:
//!
//! 1. IP-literal hosts in [`super::error::normalize_url`], before any I/O.
//! 2. [`EgressResolver`], the static client's DNS resolver: resolved
//!    addresses in the table are dropped at connect time, which catches
//!    private DNS names (`localhost`, split-horizon names) and rebinding.
//! 3. [`redirect_policy`], re-running the host check on every redirect hop
//!    (an IP-literal hop never reaches the resolver).
//! 4. [`check_resolved`], before Obscura is spawned: the browser resolves on
//!    its own, so its target host must resolve to public addresses only.
//!
//! `FETCH_ALLOW_PRIVATE=1` ([`EgressPolicy::from_env`]) turns all four off,
//! for local testing only. Residual risk: Obscura's own sub-requests (JS,
//! images, iframes of a public page) and a DNS answer that changes between
//! [`check_resolved`] and the browser's own lookup are not filtered.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::Url;

/// Env var that disables the egress policy when set to exactly `1`.
pub const FETCH_ALLOW_PRIVATE_ENV: &str = "FETCH_ALLOW_PRIVATE";

/// Whether fetches may reach private, loopback and other non-public
/// addresses. Production reads [`EgressPolicy::from_env`]; tests against a
/// local `127.0.0.1` server pass [`EgressPolicy::AllowPrivate`] explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressPolicy {
    /// Refuse every address in the table (the default).
    PublicOnly,
    /// `FETCH_ALLOW_PRIVATE=1`: no address filter at all.
    AllowPrivate,
}

impl EgressPolicy {
    /// `AllowPrivate` only when `FETCH_ALLOW_PRIVATE` is exactly `1`.
    pub fn from_env() -> Self {
        match std::env::var(FETCH_ALLOW_PRIVATE_ENV) {
            Ok(value) if value == "1" => Self::AllowPrivate,
            _ => Self::PublicOnly,
        }
    }

    fn enforced(self) -> bool {
        self == Self::PublicOnly
    }
}

/// Why a target was refused; carried inside `reqwest`'s error chain by the
/// resolver and the redirect policy, then mapped to `FetchError::Egress`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EgressDenied {
    pub(crate) reason: String,
    /// The refused redirect hop, set by [`redirect_policy`] only:
    /// `reqwest`'s error carries the original request URL, not the hop.
    pub(crate) hop: Option<String>,
}

impl std::fmt::Display for EgressDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for EgressDenied {}

/// First [`EgressDenied`] in `err`'s source chain, if the policy caused it.
pub(crate) fn denied_in_chain<'a>(
    err: &'a (dyn std::error::Error + 'static),
) -> Option<&'a EgressDenied> {
    let mut current = Some(err);
    while let Some(err) = current {
        if let Some(denied) = err.downcast_ref::<EgressDenied>() {
            return Some(denied);
        }
        current = err.source();
    }
    None
}

/// True for every address a fetch must never reach: unspecified, loopback,
/// link-local, RFC 1918, CGNAT, ULA, multicast, broadcast and documentation
/// ranges, plus the IPv4-mapped (`::ffff:a.b.c.d`) and IPv4-compatible
/// (`::a.b.c.d`) IPv6 forms of the IPv4 ranges.
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_v4(v4),
        IpAddr::V6(v6) => is_forbidden_v6(v6),
    }
}

fn is_forbidden_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0 // 0.0.0.0/8, "this network" (0.0.0.0 reaches the local host)
        || a == 10 // RFC 1918
        || a == 127 // loopback
        || (a == 100 && (64..=127).contains(&b)) // CGNAT 100.64.0.0/10
        || (a == 169 && b == 254) // link-local, cloud metadata (169.254.169.254)
        || (a == 172 && (16..=31).contains(&b)) // RFC 1918
        || (a == 192 && b == 168) // RFC 1918
        || (a == 192 && b == 0 && c == 2) // TEST-NET-1
        || (a == 198 && b == 51 && c == 100) // TEST-NET-2
        || (a == 203 && b == 0 && c == 113) // TEST-NET-3
        || (224..=239).contains(&a) // multicast 224.0.0.0/4
        || ip == Ipv4Addr::BROADCAST
}

fn is_forbidden_v6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    // IPv4-mapped ::ffff:0:0/96 and IPv4-compatible ::/96 carry an IPv4
    // address in the low 32 bits; `::` and `::1` map to 0.0.0.0/8 and are
    // refused with it.
    if seg[..5] == [0, 0, 0, 0, 0] && (seg[5] == 0 || seg[5] == 0xffff) {
        let v4 = Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            seg[6] as u8,
            (seg[7] >> 8) as u8,
            seg[7] as u8,
        );
        return is_forbidden_v4(v4);
    }
    (seg[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        || (seg[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
        || (seg[0] & 0xff00) == 0xff00 // multicast ff00::/8
        || (seg[0] == 0x2001 && seg[1] == 0x0db8) // documentation 2001:db8::/32
        || (seg[0] == 0x3fff && (seg[1] & 0xf000) == 0) // documentation 3fff::/20 (RFC 9637)
}

/// Refuse `url` when its host is an IP literal in the table. Domain names
/// pass here; [`EgressResolver`] and [`check_resolved`] judge them.
pub(crate) fn check_url_host(url: &Url, policy: EgressPolicy) -> Result<(), EgressDenied> {
    if !policy.enforced() {
        return Ok(());
    }
    let ip = match url.host() {
        Some(url::Host::Ipv4(v4)) => IpAddr::V4(v4),
        Some(url::Host::Ipv6(v6)) => IpAddr::V6(v6),
        _ => return Ok(()),
    };
    if is_forbidden_ip(ip) {
        return Err(denied_ip(ip));
    }
    Ok(())
}

fn denied_ip(ip: IpAddr) -> EgressDenied {
    EgressDenied {
        hop: None,
        reason: format!(
            "{ip} is a private or reserved address; set {FETCH_ALLOW_PRIVATE_ENV}=1 to allow it"
        ),
    }
}

/// Resolve `url`'s domain host and refuse it unless every address is
/// public. Used before spawning Obscura, which resolves on its own. A
/// lookup failure is left to the browser to report.
pub(crate) async fn check_resolved(url: &Url, policy: EgressPolicy) -> Result<(), EgressDenied> {
    check_url_host(url, policy)?;
    if !policy.enforced() {
        return Ok(());
    }
    let Some(url::Host::Domain(host)) = url.host() else {
        return Ok(());
    };
    let port = url.port_or_known_default().unwrap_or(80);
    let Ok(addrs) = tokio::net::lookup_host((host, port)).await else {
        return Ok(());
    };
    for addr in addrs {
        if is_forbidden_ip(addr.ip()) {
            return Err(EgressDenied {
                hop: None,
                reason: format!(
                    "{host} resolves to {}, a private or reserved address; set {FETCH_ALLOW_PRIVATE_ENV}=1 to allow it",
                    addr.ip()
                ),
            });
        }
    }
    Ok(())
}

/// The static client's DNS resolver: system lookup, then every address in
/// the table is dropped; a name left with no address fails the connect with
/// [`EgressDenied`]. Only installed under [`EgressPolicy::PublicOnly`].
#[derive(Debug, Default)]
pub(crate) struct EgressResolver;

impl Resolve for EgressResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let resolved: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = resolved
                .iter()
                .copied()
                .filter(|addr| !is_forbidden_ip(addr.ip()))
                .collect();
            if public.is_empty() {
                let shown = resolved
                    .first()
                    .map_or_else(|| "no address".to_owned(), |addr| addr.ip().to_string());
                return Err(Box::new(EgressDenied {
                    hop: None,
                    reason: format!(
                        "{host} resolves to {shown}, a private or reserved address; set {FETCH_ALLOW_PRIVATE_ENV}=1 to allow it"
                    ),
                }) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

/// Redirect hops `reqwest` follows before giving up, same as its default.
const MAX_REDIRECTS: usize = 10;

/// `reqwest`'s default limit of 10 hops, plus the IP-literal host check on
/// every hop under [`EgressPolicy::PublicOnly`]. Domain hops are judged by
/// [`EgressResolver`] when the hop connects.
pub(crate) fn redirect_policy(policy: EgressPolicy) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error(format!("too many redirects (over {MAX_REDIRECTS})"));
        }
        match check_url_host(attempt.url(), policy) {
            Ok(()) => attempt.follow(),
            Err(denied) => {
                let hop = Some(attempt.url().to_string());
                attempt.error(EgressDenied { hop, ..denied })
            }
        }
    })
}

#[cfg(test)]
mod tests;
