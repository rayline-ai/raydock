//! Where hybrid routing may send Decisions requests.
//!
//! A Decisions request carries the session's execution context and the router
//! API key, so it may go only to a Rayline router (HTTPS on a router's exact
//! host and port) or to a loopback address (tests and a local Decisions
//! service). Both `rayline hybrid start` and `rld hybrid` apply this check, so
//! a hand-edited core configuration cannot widen it.

use std::net::{Ipv4Addr, Ipv6Addr};

/// Routers of the built-in Rayline environments. Configured environments add
/// their own `router_url`.
pub const BUILTIN_ROUTER_URLS: &[&str] = &["https://api.rayline.ai", "https://api-dev.rayline.ai"];

/// The HTTPS origin authority (lowercase host and effective port) of a URL, or
/// `None` for any other scheme, a URL with credentials, or no host.
pub fn https_authority(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    Some(format!(
        "{}:{}",
        url.host_str()?.to_ascii_lowercase(),
        url.port_or_known_default()?
    ))
}

/// Accept `url` as a Decisions endpoint when it is HTTPS on the exact host and
/// port of a built-in router or one of `router_urls`, or http(s) on a loopback
/// address (`127.0.0.0/8`, `::1`, `localhost`).
///
/// The URL must be written in canonical form, so the core's own URL parser
/// cannot read a different host from it: ASCII only (an IDN must already be
/// punycode), no userinfo, no backslash, whitespace or control characters,
/// and a host and port spelled the way they parse. Hosts compare
/// case-insensitively; a trailing-dot host is not the router's host. Errors
/// name at most the host and port, never the rest of the URL.
pub fn check_decisions_url<'a>(
    url: &str,
    router_urls: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    if url
        .bytes()
        .any(|byte| !byte.is_ascii_graphic() || byte == b'\\')
    {
        return Err(
            "decisions URL must be plain ASCII without spaces, control characters or backslashes"
                .to_owned(),
        );
    }
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| "decisions URL is not a valid absolute URL".to_owned())?;
    let scheme = parsed.scheme();
    if scheme != "https" && scheme != "http" {
        return Err(format!(
            "decisions URL scheme {scheme:?} is not allowed; use https"
        ));
    }
    let authority =
        raw_authority(url).ok_or_else(|| "decisions URL is not a valid absolute URL".to_owned())?;
    if authority.contains('@') || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("decisions URL must not contain credentials (user:password@)".to_owned());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "decisions URL has no host".to_owned())?
        .to_ascii_lowercase();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| "decisions URL has no port".to_owned())?;
    let (raw_host, raw_port) = split_authority(authority);
    let canonical =
        raw_host.eq_ignore_ascii_case(&host) && raw_port.is_none_or(|raw| raw == port.to_string());
    if !canonical {
        return Err(format!(
            "decisions URL host {host}:{port} is not written in canonical form"
        ));
    }
    if is_loopback(&host) {
        return Ok(());
    }
    let refused = || {
        format!(
            "decisions URL host {host}:{port} is not a Rayline router; decisions go only over HTTPS to the router of a configured Rayline environment, or to a loopback address"
        )
    };
    if scheme != "https" {
        return Err(refused());
    }
    let authority = format!("{host}:{port}");
    let allowed = BUILTIN_ROUTER_URLS
        .iter()
        .copied()
        .chain(router_urls)
        .filter_map(https_authority)
        .any(|router| router == authority);
    if allowed { Ok(()) } else { Err(refused()) }
}

/// The text between `scheme://` and the first `/`, `?` or `#`.
fn raw_authority(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Split `host[:port]`, keeping an IPv6 literal's brackets with the host.
fn split_authority(authority: &str) -> (&str, Option<&str>) {
    let host_end = if authority.starts_with('[') {
        authority
            .find(']')
            .map_or(authority.len(), |index| index + 1)
    } else {
        authority.find(':').unwrap_or(authority.len())
    };
    let (host, rest) = authority.split_at(host_end);
    (host, rest.strip_prefix(':'))
}

fn is_loopback(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    if let Ok(address) = host.parse::<Ipv4Addr>() {
        return address.is_loopback();
    }
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .and_then(|host| host.parse::<Ipv6Addr>().ok())
        .is_some_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIGURED: &str = "https://router.example.test:8443";

    #[test]
    fn decisions_urls_reach_only_routers_and_loopback() {
        let cases: &[(&str, bool)] = &[
            // Built-in and configured routers, exact host and port.
            ("https://api.rayline.ai/v1/routes", true),
            ("https://api-dev.rayline.ai/v1/routes", true),
            ("https://API.Rayline.AI/v1/routes", true),
            ("HTTPS://api.rayline.ai/v1/routes", true),
            ("https://api.rayline.ai:443/v1/routes", true),
            ("https://api.rayline.ai/v1/routes?cell=x", true),
            ("https://router.example.test:8443/v1/routes", true),
            // Loopback, either scheme, any port.
            ("http://127.0.0.1:9/decisions", true),
            ("http://127.1.2.3:9/decisions", true),
            ("https://127.0.0.1:8443/decisions", true),
            ("http://[::1]:9/decisions", true),
            ("http://localhost:9/decisions", true),
            ("http://LOCALHOST:9/decisions", true),
            // Not a router: Modal and other hosts.
            ("https://rayline--decisions.modal.run/v1/routes", false),
            ("https://decisions.example.com/v1/routes", false),
            ("https://router.example.test/v1/routes", false),
            ("https://api.rayline.ai.evil.test/v1/routes", false),
            ("https://evilapi.rayline.ai/v1/routes", false),
            // A router host over plain HTTP or on another port.
            ("http://api.rayline.ai/v1/routes", false),
            ("http://api.rayline.ai:443/v1/routes", false),
            ("https://api.rayline.ai:8443/v1/routes", false),
            ("https://router.example.test:443/v1/routes", false),
            // Trailing dots name the host differently.
            ("https://api.rayline.ai./v1/routes", false),
            ("http://localhost./decisions", false),
            // Userinfo, before either host.
            ("https://user@api.rayline.ai/v1/routes", false),
            ("https://user:pass@api.rayline.ai/v1/routes", false),
            ("https://api.rayline.ai@evil.test/v1/routes", false),
            ("https://evil.test@api.rayline.ai/v1/routes", false),
            ("http://x@127.0.0.1:9/decisions", false),
            // Parser differentials: backslashes, whitespace, non-canonical hosts.
            ("https://api.rayline.ai\\@evil.test/v1/routes", false),
            ("https://evil.test\\@api.rayline.ai/v1/routes", false),
            (" https://api.rayline.ai/v1/routes", false),
            ("https://api.rayline.ai/v1/routes\n", false),
            ("https://api.rayline.ai%2e/v1/routes", false),
            ("https://api%2Erayline.ai/v1/routes", false),
            ("http://0x7f.1:9/decisions", false),
            ("http://2130706433:9/decisions", false),
            ("http://[0:0:0:0:0:0:0:1]:9/decisions", false),
            ("http://[::ffff:127.0.0.1]:9/decisions", false),
            ("https://api.rayline.ai:0443/v1/routes", false),
            ("https://api.rayline.ai:/v1/routes", false),
            // IDN: Unicode lookalikes and their punycode are other hosts.
            ("https://аpi.rayline.ai/v1/routes", false),
            ("https://xn--pi-7kc.rayline.ai/v1/routes", false),
            // Other schemes and malformed URLs.
            ("ftp://api.rayline.ai/v1/routes", false),
            ("file:///tmp/routes", false),
            ("api.rayline.ai/v1/routes", false),
            ("", false),
            ("http://0.0.0.0:9/decisions", false),
            ("http://localhost.evil.test:9/decisions", false),
            ("http://foo.localhost:9/decisions", false),
        ];
        for (url, allowed) in cases {
            let result = check_decisions_url(url, [CONFIGURED]);
            assert_eq!(result.is_ok(), *allowed, "{url:?}: {result:?}");
        }
    }

    #[test]
    fn refusals_name_the_host_only() {
        let error = check_decisions_url("https://evil.test/v1/routes?key=rlk-secret", [])
            .expect_err("refused");
        assert!(error.contains("evil.test:443"), "{error}");
        assert!(!error.contains("rlk-secret") && !error.contains("/v1/routes"));
        let error = check_decisions_url("https://rlk-secret@api.rayline.ai/v1/routes", [])
            .expect_err("refused");
        assert!(!error.contains("rlk-secret"), "{error}");
    }

    #[test]
    fn configured_router_urls_must_themselves_be_https() {
        // An http router URL in settings does not admit its host over HTTPS.
        let result = check_decisions_url(
            "https://plain.example.test/v1/routes",
            ["http://plain.example.test"],
        );
        assert!(result.is_err());
        // Nor does a router URL with credentials.
        let result = check_decisions_url(
            "https://router.example.test/v1/routes",
            ["https://user@router.example.test"],
        );
        assert!(result.is_err());
    }
}
