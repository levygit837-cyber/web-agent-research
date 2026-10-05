use std::net::IpAddr;

use super::*;
use crate::test_support::EnvGuard;

fn ip(text: &str) -> IpAddr {
    text.parse()
        .unwrap_or_else(|_| panic!("bad test address {text}"))
}

#[test]
fn forbidden_ipv4_ranges() {
    for addr in [
        "0.0.0.0",
        "0.1.2.3",
        "127.0.0.1",
        "127.255.255.254",
        "10.0.0.1",
        "10.255.255.255",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.1",
        "100.64.0.1",
        "100.127.255.255",
        "169.254.169.254",
        "169.254.0.1",
        "192.0.2.10",
        "198.51.100.7",
        "203.0.113.200",
        "224.0.0.1",
        "239.255.255.250",
        "255.255.255.255",
    ] {
        assert!(is_forbidden_ip(ip(addr)), "{addr} must be refused");
    }
}

#[test]
fn forbidden_ipv6_ranges() {
    for addr in [
        "::",
        "::1",
        "fe80::1",
        "febf:ffff::1",
        "fc00::1",
        "fd12:3456::1",
        "ff02::1",
        "ff0e::1",
        "2001:db8::1",
        "3fff::1",
        "3fff:0fff::1",
    ] {
        assert!(is_forbidden_ip(ip(addr)), "{addr} must be refused");
    }
}

#[test]
fn mapped_and_compatible_ipv4_forms_are_refused() {
    for addr in [
        "::ffff:127.0.0.1",
        "::ffff:7f00:1",
        "::ffff:169.254.169.254",
        "::ffff:10.0.0.1",
        "::ffff:192.168.0.1",
        "::ffff:100.64.0.1",
        "::127.0.0.1",
        "::10.1.2.3",
        "::169.254.169.254",
    ] {
        assert!(is_forbidden_ip(ip(addr)), "{addr} must be refused");
    }
}

#[test]
fn public_addresses_pass() {
    for addr in [
        "1.1.1.1",
        "8.8.8.8",
        "93.184.216.34",
        "100.63.255.255",
        "100.128.0.1",
        "172.15.255.255",
        "172.32.0.1",
        "169.253.0.1",
        "192.169.0.1",
        "198.51.101.1",
        "223.255.255.255",
        "240.0.0.1",
        "2606:4700:4700::1111",
        "2001:4860:4860::8888",
        "2001:db9::1",
        "3fff:1000::1",
        "fec0::1",
        "::ffff:8.8.8.8",
        "::8.8.8.8",
    ] {
        assert!(!is_forbidden_ip(ip(addr)), "{addr} must pass");
    }
}

#[test]
fn url_host_check_covers_ip_literals_only() {
    let check = |raw: &str| check_url_host(&Url::parse(raw).unwrap(), EgressPolicy::PublicOnly);
    for refused in [
        "http://127.0.0.1:8765/notes.html",
        "http://[::1]/",
        "http://[::ffff:169.254.169.254]/latest/meta-data",
        "http://169.254.169.254/latest/meta-data",
        "http://0x7f.1/",
        "http://2130706433/",
        "https://10.0.0.5/admin",
    ] {
        assert!(check(refused).is_err(), "{refused} must be refused");
    }
    for allowed in [
        "https://docs.rs/reqwest",
        "http://localhost:8080/",
        "https://1.1.1.1/",
    ] {
        assert!(
            check(allowed).is_ok(),
            "{allowed} must pass the literal check"
        );
    }
}

#[test]
fn allow_private_disables_every_check() {
    let url = Url::parse("http://127.0.0.1:9/").unwrap();
    assert!(check_url_host(&url, EgressPolicy::AllowPrivate).is_ok());
}

#[tokio::test]
async fn resolved_check_refuses_localhost_unless_allowed() {
    let url = Url::parse("http://localhost:9/").unwrap();
    let denied = check_resolved(&url, EgressPolicy::PublicOnly)
        .await
        .expect_err("localhost resolves to loopback");
    assert!(denied.reason.contains("localhost resolves to"), "{denied}");
    assert!(check_resolved(&url, EgressPolicy::AllowPrivate)
        .await
        .is_ok());
}

#[tokio::test]
async fn resolver_drops_private_answers() {
    let name: Name = "localhost".parse().unwrap();
    let err = match EgressResolver.resolve(name).await {
        Ok(_) => panic!("localhost must not resolve through the egress resolver"),
        Err(err) => err,
    };
    assert!(denied_in_chain(err.as_ref()).is_some(), "{err}");
}

#[test]
fn env_opt_out_is_exactly_one() {
    let _guard = EnvGuard::lock(vec![FETCH_ALLOW_PRIVATE_ENV]);
    assert_eq!(EgressPolicy::from_env(), EgressPolicy::PublicOnly);
    for value in ["0", "true", "yes", ""] {
        std::env::set_var(FETCH_ALLOW_PRIVATE_ENV, value);
        assert_eq!(
            EgressPolicy::from_env(),
            EgressPolicy::PublicOnly,
            "{value:?}"
        );
    }
    std::env::set_var(FETCH_ALLOW_PRIVATE_ENV, "1");
    assert_eq!(EgressPolicy::from_env(), EgressPolicy::AllowPrivate);
}
