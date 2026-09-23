//! Fetch and pin the server's WebTransport certificate hash.
//!
//! The server publishes its cert hash at `/config.json` over plain HTTP on
//! the tailnet -- the same endpoint the browser client's
//! `ghostframe-web-client/src/bootstrap.ts` consumes, served by
//! `ghostbridge/web_server.go`. Body shape: `{"certHash":"<64 hex chars>"}`.
//!
//! There is deliberately no direct-socket path anywhere in this crate, and
//! none may be added: every byte crosses `GhostbridgeHandle::dial_tcp`, so
//! staying inside the tailnet is a structural property rather than a
//! convention to remember.

use std::io::{Read, Write};

use ghostframe_tsnet::GhostbridgeHandle;

use crate::ClientError;

/// Fetch `/config.json` over the tailnet and pin the server certificate.
///
/// Opens a plain HTTP/1.1 request over the `UnixStream` `dial_tcp` returns
/// -- one GET, `Connection: close`, read to EOF. No HTTP client crate: this
/// is the only request this library ever makes.
/// Port of the server's plain-HTTP tailnet listener.
///
/// The cert hash is fetched here, NOT from the QUIC/WebTransport port. The
/// `:443` listener speaks TLS -- it exists because browsers refuse
/// WebTransport over anything else -- so a plaintext GET to it blocks
/// forever: the server waits for a ClientHello while the client waits for a
/// response, and neither times out. That was a silent hang that read like a
/// network fault.
///
/// Both listeners are tsnet listeners, so traffic to either has already been
/// encrypted and authenticated by WireGuard; the hash is a public
/// fingerprint that grants nothing without the private key. See
/// `newRedirectHandler` in `ghostbridge/web_server.go`, which serves
/// `/config.json` directly on this port and redirects everything else.
pub const CONFIG_HTTP_PORT: u16 = 80;

pub fn fetch_cert_hash(
    bridge: &GhostbridgeHandle,
    host: &str,
    _quic_port: u16,
) -> Result<[u8; 32], ClientError> {
    let target = format!("{host}:{CONFIG_HTTP_PORT}");
    let mut stream = bridge.dial_tcp(&target)?;
    // `dial_tcp`'s fd comes back non-blocking (ghostbridge sets it for the
    // tokio AsyncFd consumer this library does not use); flip it back to a
    // blocking stream for this one-shot synchronous request rather than
    // looping on WouldBlock for a handful of small reads/writes.
    stream.set_nonblocking(false)?;

    let request = format!("GET /config.json HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes())?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;

    let split_at = find_subslice(&buf, b"\r\n\r\n").ok_or_else(|| {
        ClientError::Bootstrap("malformed HTTP response: no header/body separator".into())
    })?;
    let (head, rest) = buf.split_at(split_at);
    let body = &rest[4..];

    let head_str = String::from_utf8_lossy(head);
    let status_line = head_str.lines().next().unwrap_or("");
    if !status_line.contains(" 200 ") && !status_line.ends_with(" 200") {
        return Err(ClientError::Bootstrap(format!(
            "config.json request failed: {status_line}"
        )));
    }

    let body_str = std::str::from_utf8(body)
        .map_err(|_| ClientError::Bootstrap("config.json body is not valid UTF-8".into()))?;
    parse_cert_hash(body_str)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Extract and validate the `certHash` field out of a `/config.json` body,
/// without a JSON dependency for the one field this library needs.
///
/// A short or non-hex hash must fail loudly rather than be silently
/// accepted -- accepting one would disable certificate pinning, the
/// property protecting the whole session.
fn parse_cert_hash(body: &str) -> Result<[u8; 32], ClientError> {
    let key_pos = body
        .find("\"certHash\"")
        .ok_or_else(|| ClientError::Bootstrap("config.json missing certHash field".into()))?;
    let after_key = &body[key_pos + "\"certHash\"".len()..];

    let after_colon = after_key
        .trim_start()
        .strip_prefix(':')
        .ok_or_else(|| ClientError::Bootstrap("certHash field missing ':'".into()))?;

    let quoted = after_colon.trim_start();
    let unquoted = quoted
        .strip_prefix('"')
        .ok_or_else(|| ClientError::Bootstrap("certHash value is not a JSON string".into()))?;
    let end = unquoted
        .find('"')
        .ok_or_else(|| ClientError::Bootstrap("certHash value is not terminated".into()))?;
    let hex = &unquoted[..end];

    if hex.len() != 64 {
        return Err(ClientError::Bootstrap(format!(
            "certHash must be 64 hex chars, got {}",
            hex.len()
        )));
    }

    let mut out = [0u8; 32];
    for (i, chunk) in out.iter_mut().zip(hex.as_bytes().chunks(2)) {
        let s = std::str::from_utf8(chunk)
            .map_err(|_| ClientError::Bootstrap("certHash is not valid UTF-8".into()))?;
        *i = u8::from_str_radix(s, 16)
            .map_err(|_| ClientError::Bootstrap(format!("certHash contains non-hex byte: {s}")))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cert_hash_from_config_json() {
        let b =
            r#"{"certHash":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"}"#;
        let h = parse_cert_hash(b).expect("parse");
        assert_eq!(h[0], 0x00);
        assert_eq!(h[31], 0xff);
    }

    #[test]
    fn accepts_other_fields_and_whitespace() {
        let b = r#"{ "foo": 1, "certHash" : "aa00112233445566778899aabbccddeeff00112233445566778899aabbccdd11" }"#;
        assert!(parse_cert_hash(b).is_ok());
    }

    #[test]
    fn rejects_a_hash_of_the_wrong_length() {
        assert!(parse_cert_hash(r#"{"certHash":"00112233"}"#).is_err());
    }

    #[test]
    fn rejects_non_hex() {
        assert!(parse_cert_hash(
            r#"{"certHash":"zz112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"}"#
        )
        .is_err());
    }

    #[test]
    fn rejects_a_missing_field() {
        assert!(parse_cert_hash(r#"{"other":"x"}"#).is_err());
    }
}
