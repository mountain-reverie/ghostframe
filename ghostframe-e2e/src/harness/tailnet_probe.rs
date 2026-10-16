//! Browserless probes of a containerised server over the tailnet, for the
//! peer-access tests: a plain HTTP GET through `TestNode::dial_tcp`, and a
//! bare QUIC handshake through `TestNode::dial`, both seen from the test
//! node's tailnet identity.
//!
//! Neither needs a browser, so a refusal is observed directly (a status
//! code, a CONNECTION_REFUSED) rather than inferred from a page that never
//! renders.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use ghostframe_client_net::{ClientNet, ClientNetConfig, ClientNetEvent};
use ghostframe_lib::transport::ghostbridge::{encode_frame, parse_frame_rest, MAX_FRAME_LEN};

use crate::harness::containers::TestNode;

/// `GET path` over plain HTTP on `host:80`. Returns (status, body).
pub fn http_get(node: &TestNode, host: &str, path: &str) -> Result<(u16, String)> {
    let mut stream = node
        .dial_tcp(&format!("{host}:80"))
        .context("dial_tcp :80")?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )?;

    // Framed by Content-Length, not EOF: Go's http.Server may keep the
    // connection open after a complete response.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(h) = find(&raw, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..h]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if raw.len() >= h + 4 + len {
                let status = head
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse::<u16>().ok())
                    .ok_or_else(|| anyhow!("no status line in {head:?}"))?;
                let body = String::from_utf8_lossy(&raw[h + 4..h + 4 + len]).to_string();
                return Ok((status, body));
            }
        }
        let n = stream.read(&mut chunk).context("read HTTP response")?;
        if n == 0 {
            return Err(anyhow!(
                "connection closed mid-response: {:?}",
                String::from_utf8_lossy(&raw)
            ));
        }
        raw.extend_from_slice(&chunk[..n]);
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// What became of a QUIC handshake attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuicOutcome {
    Connected,
    Lost(String),
    TimedOut,
}

/// Try a QUIC handshake to `host:443` from the test node, pinning
/// `cert_sha256`. A refusal arrives before TLS completes, so a peer that
/// cannot fetch the real hash can pass any value and still observe it.
pub fn quic_handshake(
    node: &TestNode,
    host: &str,
    cert_sha256: [u8; 32],
    timeout: Duration,
) -> Result<QuicOutcome> {
    let conn = node.dial(&format!("{host}:443")).context("dial udp :443")?;
    // SAFETY: into_raw_fd hands over sole ownership of an open fd.
    let mut sock = unsafe { UnixStream::from_raw_fd(conn.into_raw_fd()) };
    sock.set_nonblocking(true)?;

    // Ghostbridge ignores the destination on a dialed conn; any address
    // works as quinn's peer label, and every inbound packet is re-labelled
    // with it so quinn sees one stable path.
    let remote: SocketAddr = "192.0.2.1:443".parse().unwrap();
    let mut client = ClientNet::new(
        ClientNetConfig {
            server_name: host.to_string(),
            server_cert_sha256: cert_sha256,
            indices_raw_enabled: false,
            supports_h264: false,
            display: None,
        },
        0,
    )
    .map_err(|e| anyhow!("ClientNet::new: {e}"))?;
    client
        .connect(remote, 0)
        .map_err(|e| anyhow!("connect: {e}"))?;

    let base = Instant::now();
    let mut inbuf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 65536];
    loop {
        let elapsed = Instant::now().saturating_duration_since(base);
        if elapsed >= timeout {
            break;
        }
        let now_us = elapsed.as_micros() as u64;
        while let Some(out) = client.poll_transmit() {
            write_all_nonblocking(&mut sock, &encode_frame(&out.payload, &out.destination))?;
        }
        loop {
            match sock.read(&mut chunk) {
                Ok(0) => return Err(anyhow!("tsnet socket closed")),
                Ok(n) => inbuf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        while inbuf.len() >= 8 {
            let total = u32::from_be_bytes(inbuf[0..4].try_into().unwrap()) as usize;
            let payload_len = u32::from_be_bytes(inbuf[4..8].try_into().unwrap()) as usize;
            if !(8 + payload_len + 3..=MAX_FRAME_LEN).contains(&total) {
                return Err(anyhow!(
                    "bad frame header total={total} payload={payload_len}"
                ));
            }
            if inbuf.len() < total {
                break;
            }
            let pkt = parse_frame_rest(&inbuf[8..total], payload_len)?;
            inbuf.drain(..total);
            client.handle_udp(&pkt.payload, remote, now_us);
        }
        if client.poll_timeout().is_some_and(|t| t <= now_us) {
            client.on_timeout(now_us);
        }
        for ev in client.take_events() {
            match ev {
                ClientNetEvent::Connected => return Ok(QuicOutcome::Connected),
                ClientNetEvent::ConnectionLost { reason } => return Ok(QuicOutcome::Lost(reason)),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(QuicOutcome::TimedOut)
}

fn write_all_nonblocking(sock: &mut UnixStream, mut buf: &[u8]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !buf.is_empty() {
        match sock.write(buf) {
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    return Err(anyhow!("tsnet socket write stalled"));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// The first cert hash in a `/config.json` body.
pub fn cert_hash_from_config(body: &str) -> Result<[u8; 32]> {
    let v: serde_json::Value = serde_json::from_str(body).context("config.json")?;
    let hex_str = v["certHash"]
        .as_str()
        .ok_or_else(|| anyhow!("no certHash in {body:?}"))?;
    let mut h = [0u8; 32];
    hex::decode_to_slice(hex_str, &mut h).context("certHash hex")?;
    Ok(h)
}
