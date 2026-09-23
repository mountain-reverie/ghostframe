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
//!
//! # Why this module is split the way it is
//!
//! This one request shipped three separate defects, and each presented
//! identically -- a silent hang inside `Client::connect` with no output:
//!
//! 1. a plaintext GET aimed at the TLS port, so the server waited for a
//!    `ClientHello` while we waited for a response;
//! 2. no read timeout, leaving an unbounded read upstream of every caller's
//!    own deadline;
//! 3. framing the response by EOF, which blocks whenever the server keeps
//!    the socket alive -- as Go's `http.Server` does regardless of a
//!    `Connection: close` request header.
//!
//! The common cause was structural: dialing, the HTTP exchange, and parsing
//! were one function welded to a real socket, so only the innermost parsing
//! could be unit-tested and every framing bug needed Docker and a live
//! server to observe.
//!
//! So the transport-touching part ([`fetch_cert_hash`]) is now a thin shell
//! that dials and configures the socket, and everything protocol-shaped
//! lives in [`fetch_cert_hash_over`], which is generic over `Read + Write`.
//! The tests below exercise keep-alive, missing `Content-Length`, non-200,
//! truncation, oversized bodies and split reads against in-memory streams,
//! with no network at all.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use ghostframe_tsnet::GhostbridgeHandle;

use crate::ClientError;

/// Port of the server's plain-HTTP tailnet listener.
///
/// The cert hash is fetched here, NOT from the QUIC/WebTransport port. The
/// `:443` listener speaks TLS -- it exists because browsers refuse
/// WebTransport over anything else -- so a plaintext GET to it blocks
/// forever: the server waits for a ClientHello while the client waits for a
/// response, and neither times out.
///
/// Both listeners are tsnet listeners, so traffic to either has already been
/// encrypted and authenticated by WireGuard; the hash is a public
/// fingerprint that grants nothing without the private key. See
/// `newRedirectHandler` in `ghostbridge/web_server.go`, which serves
/// `/config.json` directly on this port and redirects everything else.
pub const CONFIG_HTTP_PORT: u16 = 80;

/// Overall budget for the fetch, enforced across all reads.
///
/// Distinct from the per-read socket timeout: that one resets on every
/// byte, so a peer dribbling one byte just under the limit could hold the
/// exchange open indefinitely. This is the deadline that actually bounds
/// `Client::connect`.
pub const CONFIG_FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Per-read socket timeout. Shorter than the overall budget so a stalled
/// peer surfaces promptly rather than at the very end of it.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest `/config.json` response we will read.
///
/// The body is a single 64-character hex field; 8 KiB is orders of
/// magnitude of slack. The cap exists because `Content-Length` is attacker-
/// or bug-controlled input: without it, a bogus length turns into an
/// unbounded read. `ghostframe-tsnet::MAX_FRAME_LEN` guards the same hazard
/// on the datagram path for the same reason.
const MAX_RESPONSE_BYTES: usize = 8 * 1024;

/// Fetch `/config.json` over the tailnet and pin the server certificate.
///
/// Dials, configures the socket, and delegates the exchange to
/// [`fetch_cert_hash_over`]. Everything testable lives there.
pub fn fetch_cert_hash(bridge: &GhostbridgeHandle, host: &str) -> Result<[u8; 32], ClientError> {
    let target = format!("{host}:{CONFIG_HTTP_PORT}");
    let stream = bridge.dial_tcp(&target)?;

    // `dial_tcp`'s fd comes back non-blocking (ghostbridge sets it for the
    // tokio AsyncFd consumer this library does not use); flip it back to a
    // blocking stream for this one-shot synchronous request rather than
    // looping on WouldBlock for a handful of small reads.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(READ_TIMEOUT))?;

    fetch_cert_hash_over(stream, host, Instant::now() + CONFIG_FETCH_TIMEOUT)
}

/// The HTTP exchange, independent of any socket.
///
/// `deadline` bounds the whole read loop, not each read.
fn fetch_cert_hash_over<S: Read + Write>(
    mut stream: S,
    host: &str,
    deadline: Instant,
) -> Result<[u8; 32], ClientError> {
    let request = format!("GET /config.json HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes())?;

    let raw = read_http_response(&mut stream, deadline)?;
    let (head, body) = split_http_response(&raw)?;
    check_status_ok(head)?;

    let body_str = std::str::from_utf8(body)
        .map_err(|_| ClientError::Bootstrap("config.json body is not valid UTF-8".into()))?;
    parse_cert_hash(body_str)
}

/// Read one HTTP/1.1 response: headers, then exactly `Content-Length` body
/// bytes.
///
/// Deliberately NOT `read_to_end`. That waits for the peer to close, and
/// Go's `http.Server` keeps the connection open regardless of a
/// `Connection: close` request header -- the complete response arrives and
/// the read then blocks until it times out, discarding a reply already in
/// hand. Framing an HTTP/1.1 response by EOF is wrong whenever the server
/// may keep the socket alive.
fn read_http_response<S: Read>(stream: &mut S, deadline: Instant) -> Result<Vec<u8>, ClientError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let mut header_end: Option<usize> = None;
    let mut body_len: Option<usize> = None;

    loop {
        if let (Some(h), Some(n)) = (header_end, body_len) {
            if buf.len() >= h + 4 + n {
                buf.truncate(h + 4 + n);
                return Ok(buf);
            }
        }

        if Instant::now() >= deadline {
            return Err(ClientError::Bootstrap(format!(
                "config.json fetch exceeded {CONFIG_FETCH_TIMEOUT:?} (got {} bytes, \
                 headers {}, content-length {:?})",
                buf.len(),
                if header_end.is_some() {
                    "complete"
                } else {
                    "incomplete"
                },
                body_len,
            )));
        }

        let read = stream.read(&mut chunk).map_err(|e| {
            ClientError::Bootstrap(format!(
                "reading config.json failed: {e} (got {} bytes)",
                buf.len()
            ))
        })?;
        if read == 0 {
            // Peer closed. Legal framing if we already have a complete body;
            // otherwise the response is truncated and must not be accepted.
            return match (header_end, body_len) {
                (Some(h), Some(n)) if buf.len() >= h + 4 + n => {
                    buf.truncate(h + 4 + n);
                    Ok(buf)
                }
                _ => Err(ClientError::Bootstrap(format!(
                    "config.json response truncated: peer closed after {} bytes",
                    buf.len()
                ))),
            };
        }
        buf.extend_from_slice(&chunk[..read]);

        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(ClientError::Bootstrap(format!(
                "config.json response exceeds {MAX_RESPONSE_BYTES} bytes"
            )));
        }

        if header_end.is_none() {
            if let Some(h) = find_subslice(&buf, b"\r\n\r\n") {
                header_end = Some(h);
                let head = String::from_utf8_lossy(&buf[..h]).to_ascii_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map(str::trim)
                    .ok_or_else(|| {
                        ClientError::Bootstrap(
                            "config.json response has no Content-Length; cannot frame \
                             the body without waiting for a close that may never come"
                                .into(),
                        )
                    })?
                    .parse::<usize>()
                    .map_err(|_| {
                        ClientError::Bootstrap("config.json Content-Length is not a number".into())
                    })?;
                if len > MAX_RESPONSE_BYTES {
                    return Err(ClientError::Bootstrap(format!(
                        "config.json declares {len} bytes, over the {MAX_RESPONSE_BYTES} cap"
                    )));
                }
                body_len = Some(len);
            }
        }
    }
}

/// Split a raw response into (headers, body).
fn split_http_response(raw: &[u8]) -> Result<(&[u8], &[u8]), ClientError> {
    let at = find_subslice(raw, b"\r\n\r\n").ok_or_else(|| {
        ClientError::Bootstrap("malformed HTTP response: no header/body separator".into())
    })?;
    let (head, rest) = raw.split_at(at);
    Ok((head, &rest[4..]))
}

/// Accept only a 200 status, parsing the code rather than substring-matching
/// the line (a reason phrase can contain anything, including "200").
fn check_status_ok(head: &[u8]) -> Result<(), ClientError> {
    let head_str = String::from_utf8_lossy(head);
    let status_line = head_str.lines().next().unwrap_or("");
    let code = status_line.split_whitespace().nth(1).ok_or_else(|| {
        ClientError::Bootstrap(format!("malformed HTTP status line: {status_line:?}"))
    })?;
    if code != "200" {
        return Err(ClientError::Bootstrap(format!(
            "config.json request failed: {status_line}"
        )));
    }
    Ok(())
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
    const KEY: &str = "\"certHash\"";
    let key_pos = body
        .find(KEY)
        .ok_or_else(|| ClientError::Bootstrap("config.json has no certHash field".into()))?;
    let after = &body[key_pos + KEY.len()..];
    let colon = after
        .find(':')
        .ok_or_else(|| ClientError::Bootstrap("config.json certHash has no value".into()))?;
    let rest = &after[colon + 1..];
    let open = rest
        .find('"')
        .ok_or_else(|| ClientError::Bootstrap("config.json certHash is not a string".into()))?;
    let tail = &rest[open + 1..];
    let close = tail
        .find('"')
        .ok_or_else(|| ClientError::Bootstrap("config.json certHash is unterminated".into()))?;
    let hex = &tail[..close];

    if hex.len() != 64 {
        return Err(ClientError::Bootstrap(format!(
            "config.json certHash is {} hex chars, expected 64",
            hex.len()
        )));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| ClientError::Bootstrap("config.json certHash is not valid hex".into()))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    /// A stream that replays canned bytes, optionally in fixed-size slices,
    /// and then either reports EOF or blocks forever (as a keep-alive peer
    /// does). `blocks_after` is what reproduces the bug that shipped.
    struct FakeStream {
        to_read: Vec<u8>,
        pos: usize,
        slice: usize,
        keep_alive: bool,
        written: Vec<u8>,
    }

    impl FakeStream {
        fn new(body: &str, keep_alive: bool) -> Self {
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            Self {
                to_read: resp.into_bytes(),
                pos: 0,
                slice: usize::MAX,
                keep_alive,
                written: Vec::new(),
            }
        }
        fn raw(resp: &str, keep_alive: bool) -> Self {
            Self {
                to_read: resp.as_bytes().to_vec(),
                pos: 0,
                slice: usize::MAX,
                keep_alive,
                written: Vec::new(),
            }
        }
        fn in_slices_of(mut self, n: usize) -> Self {
            self.slice = n;
            self
        }
    }

    impl Read for FakeStream {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.to_read.len() {
                if self.keep_alive {
                    // Mimic a peer holding the socket open: the per-read
                    // socket timeout is what surfaces this in production.
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "keep-alive: no more data",
                    ));
                }
                return Ok(0);
            }
            let n = out.len().min(self.slice).min(self.to_read.len() - self.pos);
            out[..n].copy_from_slice(&self.to_read[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl Write for FakeStream {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn keep_alive_peer_does_not_stall_the_fetch() {
        // THE REGRESSION TEST. The shipped version framed by EOF, so a peer
        // that sends a complete response and holds the socket open blocked
        // until timeout and discarded a reply it already had.
        let body = format!(r#"{{"certHash":"{HASH}"}}"#);
        let got = fetch_cert_hash_over(FakeStream::new(&body, true), "h", far()).expect("fetch");
        assert_eq!(got[0], 0x00);
        assert_eq!(got[31], 0xff);
    }

    #[test]
    fn closing_peer_also_works() {
        let body = format!(r#"{{"certHash":"{HASH}"}}"#);
        assert!(fetch_cert_hash_over(FakeStream::new(&body, false), "h", far()).is_ok());
    }

    #[test]
    fn body_split_across_many_reads_is_reassembled() {
        let body = format!(r#"{{"certHash":"{HASH}"}}"#);
        let s = FakeStream::new(&body, true).in_slices_of(7);
        assert!(fetch_cert_hash_over(s, "h", far()).is_ok());
    }

    #[test]
    fn the_request_is_well_formed() {
        let body = format!(r#"{{"certHash":"{HASH}"}}"#);
        let mut s = FakeStream::new(&body, true);
        let _ = fetch_cert_hash_over(&mut s, "myhost", far());
        let sent = String::from_utf8(s.written.clone()).expect("utf8");
        assert!(
            sent.starts_with("GET /config.json HTTP/1.1\r\n"),
            "{sent:?}"
        );
        assert!(sent.contains("Host: myhost\r\n"), "{sent:?}");
        assert!(sent.ends_with("\r\n\r\n"), "{sent:?}");
    }

    #[test]
    fn missing_content_length_is_rejected_not_read_to_eof() {
        let s = FakeStream::raw(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{}",
            true,
        );
        let err = fetch_cert_hash_over(s, "h", far()).expect_err("must fail");
        assert!(format!("{err}").contains("Content-Length"), "{err}");
    }

    #[test]
    fn a_truncated_body_is_rejected() {
        // Declares 200 bytes, sends far fewer, then closes.
        let s = FakeStream::raw(
            "HTTP/1.1 200 OK\r\nContent-Length: 200\r\n\r\n{\"cert",
            false,
        );
        let err = fetch_cert_hash_over(s, "h", far()).expect_err("must fail");
        assert!(format!("{err}").contains("truncated"), "{err}");
    }

    #[test]
    fn an_oversized_content_length_is_rejected_before_reading_it() {
        let s = FakeStream::raw("HTTP/1.1 200 OK\r\nContent-Length: 99999999\r\n\r\n", true);
        let err = fetch_cert_hash_over(s, "h", far()).expect_err("must fail");
        assert!(format!("{err}").contains("cap"), "{err}");
    }

    #[test]
    fn a_non_200_status_is_rejected() {
        let s = FakeStream::raw(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\n{}",
            false,
        );
        let err = fetch_cert_hash_over(s, "h", far()).expect_err("must fail");
        assert!(format!("{err}").contains("404"), "{err}");
    }

    #[test]
    fn a_reason_phrase_containing_200_is_not_mistaken_for_success() {
        // Substring-matching " 200 " on the status line accepted this.
        let s = FakeStream::raw(
            "HTTP/1.1 500 Error 200 something\r\nContent-Length: 2\r\n\r\n{}",
            false,
        );
        let err = fetch_cert_hash_over(s, "h", far()).expect_err("must reject");
        assert!(format!("{err}").contains("500"), "{err}");
    }

    #[test]
    fn an_expired_deadline_fails_with_context() {
        let body = format!(r#"{{"certHash":"{HASH}"}}"#);
        let s = FakeStream::new(&body, true).in_slices_of(1);
        let err = fetch_cert_hash_over(s, "h", Instant::now()).expect_err("must time out");
        let msg = format!("{err}");
        assert!(msg.contains("exceeded"), "{msg}");
        assert!(msg.contains("bytes"), "{msg}");
    }

    #[test]
    fn parses_cert_hash_from_config_json() {
        let h = parse_cert_hash(&format!(r#"{{"certHash":"{HASH}"}}"#)).expect("parse");
        assert_eq!(h[0], 0x00);
        assert_eq!(h[31], 0xff);
    }

    #[test]
    fn accepts_other_fields_and_whitespace() {
        assert!(parse_cert_hash(&format!(r#"{{ "foo": 1, "certHash" : "{HASH}" }}"#)).is_ok());
    }

    #[test]
    fn rejects_a_hash_of_the_wrong_length() {
        assert!(parse_cert_hash(r#"{"certHash":"00112233"}"#).is_err());
    }

    #[test]
    fn rejects_non_hex() {
        let bad = format!("zz{}", &HASH[2..]);
        assert!(parse_cert_hash(&format!(r#"{{"certHash":"{bad}"}}"#)).is_err());
    }

    #[test]
    fn rejects_a_missing_field() {
        assert!(parse_cert_hash(r#"{"other":"x"}"#).is_err());
    }
}
