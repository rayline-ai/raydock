//! Where the local process may send Decisions requests.
//!
//! A Decisions request carries the session's execution context and the router
//! API key, so it may go only to a Rayline router (HTTPS on a router's exact
//! host and port) or to a loopback address (tests and a local Decisions
//! service). Both `rayline local start` and the local process apply this
//! check, so a hand-edited core configuration cannot widen it.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

/// The router of the built-in prod environment. A configured environment's
/// router is passed in.
pub const BUILTIN_ROUTER_URLS: &[&str] = &["https://api.rayline.ai"];

/// Check the `decisions.url` of an execution-core configuration file as the
/// Go core will read it: duplicate object keys are refused anywhere (Go merges
/// them where serde keeps the last), then [`check_config`] applies.
pub fn check_config_bytes<'a>(
    bytes: &[u8],
    router_urls: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    let StrictValue(config) = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid execution core configuration: {error}"))?;
    check_config(&config, router_urls)
}

/// Check an execution-core configuration's `decisions.url` with
/// [`check_decisions_url`]. Go's JSON decoder matches field names
/// case-insensitively (including `ſ` for `s` and the Kelvin sign for `k`), so
/// a key that folds to `decisions` or `url` without being spelled exactly so
/// is refused: it could otherwise replace the checked URL in the core.
pub fn check_config<'a>(
    config: &Value,
    router_urls: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    let root = config
        .as_object()
        .ok_or("execution core configuration must be a JSON object")?;
    let url = exact_field(root, "decisions")?
        .and_then(Value::as_object)
        .map(|decisions| exact_field(decisions, "url"))
        .transpose()?
        .flatten()
        .and_then(Value::as_str)
        .ok_or("hybrid configuration requires a decisions.url string")?;
    check_decisions_url(url, router_urls)
}

/// `map[name]`, refusing any other key that Go's case-insensitive field
/// matching would also read as `name`.
pub fn exact_field<'v>(
    map: &'v Map<String, Value>,
    name: &str,
) -> Result<Option<&'v Value>, String> {
    if let Some(alias) = map.keys().find(|key| *key != name && go_fold_eq(key, name)) {
        return Err(format!(
            "configuration key {alias:?} is ambiguous with {name:?}; spell it exactly {name:?}"
        ));
    }
    Ok(map.get(name))
}

/// Go's `bytes.EqualFold` against an ASCII name: ASCII letters match either
/// case, `ſ` (U+017F) matches `s` and the Kelvin sign (U+212A) matches `k`.
fn go_fold_eq(key: &str, ascii_name: &str) -> bool {
    let fold = |c: char| match c {
        '\u{017F}' => 's',
        '\u{212A}' => 'k',
        c => c.to_ascii_lowercase(),
    };
    key.chars().map(fold).eq(ascii_name.chars().map(fold))
}

/// A JSON value whose objects have no duplicate keys.
struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(StrictValue)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }
    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::from(value))
    }
    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::from(value))
    }
    fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
        Ok(Value::from(value))
    }
    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }
    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(StrictValue(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut map = Map::new();
        while let Some(key) = access.next_key::<String>()? {
            if map.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate key {key:?}")));
            }
            let StrictValue(value) = access.next_value()?;
            map.insert(key, value);
        }
        Ok(Value::Object(map))
    }
}

/// What a Rayline router URL (an environment's `base_url`) must be: `https`,
/// or `http` on a loopback host, with no `user:password@`. The problem, when
/// it is not.
pub fn router_url_problem(url: &reqwest::Url) -> Option<&'static str> {
    if !url.username().is_empty() || url.password().is_some() {
        return Some("must not contain user:password@");
    }
    let loopback = url
        .host_str()
        .is_some_and(|host| is_loopback(&host.to_ascii_lowercase()));
    match url.scheme() {
        "https" if url.host_str().is_some() => None,
        "http" if loopback => None,
        _ => Some("must be https, or http on a loopback host"),
    }
}

/// `url` without its `user:password@`, for an error message.
pub fn without_credentials(url: &reqwest::Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

/// The `host:port` a router URL admits Decisions to over HTTPS, `None` for
/// a loopback `http` router (loopback is always admitted), or an error
/// naming an unusable router URL.
fn router_authority(router: &str) -> Result<Option<String>, String> {
    let url = reqwest::Url::parse(router)
        .map_err(|_| format!("router URL {router} is not a valid absolute URL"))?;
    if let Some(problem) = router_url_problem(&url) {
        return Err(format!(
            "router URL {} {problem}",
            without_credentials(&url)
        ));
    }
    if url.scheme() != "https" {
        return Ok(None);
    }
    Ok(url
        .host_str()
        .zip(url.port_or_known_default())
        .map(|(host, port)| format!("{}:{port}", host.to_ascii_lowercase())))
}

/// Accept `url` as a Decisions endpoint when it is HTTPS on the exact host and
/// port of a built-in router or one of `router_urls`, or http(s) on a loopback
/// address (`127.0.0.0/8`, `::1`, `localhost`). A router URL that is not
/// `https`, or `http` on a loopback host, without credentials is an error
/// naming it.
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
    let mut routers = Vec::new();
    for router in BUILTIN_ROUTER_URLS.iter().copied().chain(router_urls) {
        routers.extend(router_authority(router)?);
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
    if routers.contains(&authority) {
        Ok(())
    } else {
        Err(refused())
    }
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

/// Whether a lowercase URL host is a loopback name or address.
pub fn is_loopback(host: &str) -> bool {
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
            ("https://api-dev.rayline.ai/v1/routes", false),
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
    fn config_keys_go_would_fold_onto_decisions_are_refused() {
        let good = r#""decisions": {"url": "https://api.rayline.ai/v1/routes", "auth_env": "K"}"#;
        assert!(check_config_bytes(format!("{{{good}}}").as_bytes(), []).is_ok());
        for config in [
            // Unicode long s and Kelvin sign fold onto ASCII in Go.
            format!(r#"{{{good}, "deciſions": {{"url": "https://evil.test/"}}}}"#),
            format!(r#"{{{good}, "Decisions": {{"url": "https://evil.test/"}}}}"#),
            r#"{"decisions": {"url": "https://api.rayline.ai/v1/routes", "URL": "https://evil.test/"}}"#
                .to_owned(),
            r#"{"decisions": {"url": "https://api.rayline.ai/v1/routes", "Url": "https://evil.test/"}}"#
                .to_owned(),
            // Duplicate keys: Go merges them, serde keeps the last.
            format!(r#"{{"decisions": {{"url": "https://evil.test/"}}, {good}}}"#),
            r#"{"decisions": {"url": "https://evil.test/", "url": "https://api.rayline.ai/"}}"#
                .to_owned(),
            r#"{"x": [{"a": 1, "a": 2}], "decisions": {"url": "https://api.rayline.ai/"}}"#
                .to_owned(),
            // Missing, misspelt or non-string URL.
            r#"{"decisions": {"auth_env": "K"}}"#.to_owned(),
            r#"{"decisions": {"url": 1}}"#.to_owned(),
            r#"{"decisions": "https://api.rayline.ai/"}"#.to_owned(),
            r#"[]"#.to_owned(),
        ] {
            assert!(check_config_bytes(config.as_bytes(), []).is_err(), "{config}");
        }
        // Folding is only about the field name; other keys are the core's.
        assert!(go_fold_eq("deci\u{017F}ions", "decisions"));
        assert!(go_fold_eq("\u{212A}", "k"));
        assert!(!go_fold_eq("decision", "decisions"));
        assert!(!go_fold_eq("décisions", "decisions"));
    }

    #[test]
    fn an_unusable_router_url_is_an_error_naming_it() {
        for (router, error) in [
            (
                "http://plain.example.test",
                "router URL http://plain.example.test/ must be https, or http on a loopback host",
            ),
            (
                "https://user:secret@router.example.test",
                "router URL https://router.example.test/ must not contain user:password@",
            ),
            (
                "http://user@127.0.0.1:9",
                "router URL http://127.0.0.1:9/ must not contain user:password@",
            ),
            (
                "ftp://router.example.test",
                "router URL ftp://router.example.test/ must be https, or http on a loopback host",
            ),
            (
                "router.example.test",
                "router URL router.example.test is not a valid absolute URL",
            ),
        ] {
            // Even when the Decisions URL itself is loopback: the router URL
            // is never silently dropped.
            for decisions in [
                "https://router.example.test/v1/routes",
                "http://127.0.0.1:9/decisions",
            ] {
                assert_eq!(
                    check_decisions_url(decisions, [router]),
                    Err(error.to_owned()),
                    "{router}"
                );
            }
        }
        // A loopback http router admits only loopback, which is always admitted.
        check_decisions_url("http://127.0.0.1:9/decisions", ["http://127.0.0.1:9"]).unwrap();
        assert!(
            check_decisions_url(
                "https://router.example.test/v1/routes",
                ["http://127.0.0.1:9"]
            )
            .is_err()
        );
    }
}
