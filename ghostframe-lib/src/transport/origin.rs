//! Origin check for the WebTransport CONNECT.
//!
//! A browser tab on a tailnet machine can open a WebTransport session to any
//! tailnet address it can reach, and the peer-access policy sees only the
//! machine, not the page. So any web page a tailnet user visits could drive
//! the daemon (input injection included) with the user's own identity. The
//! browser always labels such a request with the page's `Origin`, and a page
//! cannot forge it, so the daemon admits only the origin it serves its own
//! web client from.
//!
//! # The rule
//!
//! | `Origin` header | Verdict |
//! |---|---|
//! | absent | allowed: not a browser (see below) |
//! | `https://<this node's FQDN>` (port 443, the TLS web listener) | allowed |
//! | an entry of `GHOSTFRAME_WEB_ORIGINS` | allowed |
//! | `null`, anything malformed, a repeated header, any other origin | **403** |
//!
//! Scheme, host and port must all match; host comparison is case-insensitive
//! and a default port is the same as no port. The `:80` listener serves only
//! a redirect, never the client, so `http://` origins are not derived from it
//! (a page loaded over plain HTTP is not a secure context and cannot open a
//! WebTransport session anyway).
//!
//! # Absent `Origin`
//!
//! Browsers always send it on a WebTransport CONNECT, and a page cannot
//! remove it. The native client (`ghostframe-client-net`) is not a browser
//! and sends none. Requiring it would break the native client, and it would
//! protect nothing: a non-browser peer can send any `Origin` it likes, so the
//! check is a defence against *pages*, not against peers. Peers are gated by
//! the tailnet access policy (`transport::peer_auth`), which runs first.
//!
//! # `GHOSTFRAME_WEB_ORIGINS`
//!
//! Comma-separated extra origins, for a server reached under another name
//! (for example the e2e harness, which browses through a local forwarder).
//! The port may be `*` to match any port on that host. Malformed entries are
//! dropped and logged at startup; nothing admits every origin.

/// One admitted origin. `port: None` is the `*` wildcard.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AllowedOrigin {
    scheme: String,
    host: String,
    port: Option<u16>,
}

/// A parsed request origin, with the default port made explicit.
#[derive(Debug, PartialEq, Eq)]
struct ParsedOrigin {
    scheme: String,
    host: String,
    port: u16,
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "https" => Some(443),
        "http" => Some(80),
        _ => None,
    }
}

/// Split `scheme://host[:port]`. No userinfo, path, query or fragment: an
/// origin is exactly those three parts, so anything more is not one.
/// Returns `(scheme, lowercased host, port text if present)`.
fn split(raw: &str) -> Option<(String, String, Option<&str>)> {
    let (scheme, rest) = raw.split_once("://")?;
    if scheme != "https" && scheme != "http" {
        return None;
    }
    if rest.is_empty()
        || rest
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '?' | '#' | '@'))
    {
        return None;
    }
    let (host, port) = if let Some(after) = rest.strip_prefix('[') {
        // IPv6 literal: keep the brackets in the host.
        let (inner, tail) = after.split_once(']')?;
        let host = format!("[{inner}]");
        match tail {
            "" => (host, None),
            t => (host, Some(t.strip_prefix(':')?)),
        }
    } else {
        match rest.split_once(':') {
            Some((h, p)) => (h.to_owned(), Some(p)),
            None => (rest.to_owned(), None),
        }
    };
    let host_ok = if let Some(inner) = host.strip_prefix('[') {
        let inner = inner.trim_end_matches(']');
        !inner.is_empty()
            && inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
    } else {
        !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    };
    if !host_ok {
        return None;
    }
    Some((scheme.to_owned(), host.to_ascii_lowercase(), port))
}

fn parse_port(p: &str) -> Option<u16> {
    // `u16::from_str` accepts a leading '+'; an origin never has one.
    (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .then(|| p.parse().ok())
        .flatten()
        .filter(|&n| n != 0)
}

fn parse_origin(raw: &str) -> Option<ParsedOrigin> {
    let (scheme, host, port) = split(raw)?;
    let port = match port {
        Some(p) => parse_port(p)?,
        None => default_port(&scheme)?,
    };
    Some(ParsedOrigin { scheme, host, port })
}

fn parse_allowed(raw: &str) -> Option<AllowedOrigin> {
    let (scheme, host, port) = split(raw)?;
    let port = match port {
        Some("*") => None,
        Some(p) => Some(parse_port(p)?),
        None => Some(default_port(&scheme)?),
    };
    Some(AllowedOrigin { scheme, host, port })
}

/// The set of origins a CONNECT may carry. `Default` admits no browser origin
/// (only an absent header), which is the fail-closed state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OriginPolicy {
    allowed: Vec<AllowedOrigin>,
}

impl OriginPolicy {
    /// `node_fqdn` is this node's MagicDNS name (a trailing dot is fine;
    /// empty adds nothing). `extra` are `GHOSTFRAME_WEB_ORIGINS` entries.
    /// Returns the policy and the entries that were not valid origins.
    pub fn new(node_fqdn: &str, extra: &[String]) -> (Self, Vec<String>) {
        let mut allowed = Vec::new();
        let host = node_fqdn.trim_end_matches('.');
        if !host.is_empty() {
            if let Some(a) = parse_allowed(&format!("https://{host}")) {
                allowed.push(a);
            }
        }
        let mut rejected = Vec::new();
        for e in extra {
            match parse_allowed(e) {
                Some(a) => allowed.push(a),
                None => rejected.push(e.clone()),
            }
        }
        (Self { allowed }, rejected)
    }

    /// The admitted origins, for the startup log.
    pub fn describe(&self) -> Vec<String> {
        self.allowed
            .iter()
            .map(|a| match a.port {
                Some(p) => format!("{}://{}:{p}", a.scheme, a.host),
                None => format!("{}://{}:*", a.scheme, a.host),
            })
            .collect()
    }

    /// Decide a CONNECT from its `Origin` header values (all of them: a
    /// repeated header is refused, since the browser sends exactly one).
    pub fn permits(&self, origins: &[&[u8]]) -> bool {
        match origins {
            [] => true,
            [one] => std::str::from_utf8(one)
                .ok()
                .and_then(parse_origin)
                .is_some_and(|o| self.matches(&o)),
            _ => false,
        }
    }

    fn matches(&self, o: &ParsedOrigin) -> bool {
        self.allowed
            .iter()
            .any(|a| a.scheme == o.scheme && a.host == o.host && a.port.is_none_or(|p| p == o.port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> OriginPolicy {
        OriginPolicy::new("Box.tail1234.ts.net.", &[]).0
    }

    fn ok(p: &OriginPolicy, o: &str) -> bool {
        p.permits(&[o.as_bytes()])
    }

    #[test]
    fn the_served_origin_is_accepted() {
        let p = policy();
        assert!(ok(&p, "https://box.tail1234.ts.net"));
        assert!(ok(&p, "https://box.tail1234.ts.net:443"), "default port");
        assert!(
            ok(&p, "https://BOX.Tail1234.ts.net"),
            "host is case-insensitive"
        );
    }

    #[test]
    fn foreign_null_and_malformed_origins_are_rejected() {
        let p = policy();
        for bad in [
            "https://evil.example",
            "https://box.tail1234.ts.net.evil.example",
            "https://evil.example/box.tail1234.ts.net",
            "https://box.tail1234.ts.net@evil.example",
            "https://evil.example@box.tail1234.ts.net",
            "https://sub.box.tail1234.ts.net",
            "null",
            "",
            "*",
            "box.tail1234.ts.net",
            "https://",
            "https://box.tail1234.ts.net/",
            "https://box.tail1234.ts.net:",
            "https://box.tail1234.ts.net:+443",
            "https://box.tail1234.ts.net:0",
            "https://box.tail1234.ts.net:99999",
            "https://box.tail1234.ts.net ",
            "file://box.tail1234.ts.net",
        ] {
            assert!(!ok(&p, bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn scheme_and_port_mismatch_are_rejected() {
        let p = policy();
        assert!(!ok(&p, "http://box.tail1234.ts.net"), "scheme");
        assert!(!ok(&p, "http://box.tail1234.ts.net:443"), "scheme");
        assert!(!ok(&p, "https://box.tail1234.ts.net:8443"), "port");
        assert!(!ok(&p, "https://box.tail1234.ts.net:80"), "port");
    }

    #[test]
    fn absent_origin_is_the_native_client_and_is_allowed() {
        assert!(policy().permits(&[]));
        assert!(OriginPolicy::default().permits(&[]));
    }

    #[test]
    fn repeated_or_non_utf8_origin_is_rejected() {
        let p = policy();
        let a: &[u8] = b"https://box.tail1234.ts.net";
        assert!(!p.permits(&[a, a]));
        assert!(!p.permits(&[b"https://box.tail1234.ts.net\xff"]));
    }

    #[test]
    fn default_policy_admits_no_browser_origin() {
        let p = OriginPolicy::default();
        assert!(!ok(&p, "https://anything.example"));
        assert!(!ok(&OriginPolicy::new("", &[]).0, "https://"));
    }

    #[test]
    fn extra_origins_exact_and_wildcard_port() {
        let (p, rejected) = OriginPolicy::new(
            "",
            &[
                "https://127.0.0.1:*".into(),
                "https://[::1]:8443".into(),
                "ftp://nope".into(),
                "https://*".into(),
                "https://x.example/path".into(),
            ],
        );
        assert_eq!(
            rejected,
            ["ftp://nope", "https://*", "https://x.example/path"]
        );
        assert!(ok(&p, "https://127.0.0.1:41234"));
        assert!(ok(&p, "https://127.0.0.1"));
        assert!(!ok(&p, "http://127.0.0.1:41234"), "scheme still binds");
        assert!(!ok(&p, "https://127.0.0.2:41234"), "host still binds");
        assert!(ok(&p, "https://[::1]:8443"));
        assert!(!ok(&p, "https://[::1]:8444"));
    }
}
