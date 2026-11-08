use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use ghostframe_lib::framing::{encode_frame, parse_frame_rest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, UdpSocket, UnixStream as TokioUnixStream};
use tower_http::services::ServeDir;

/// Opens one upstream UDP flow to the server: a ghostbridge socketpair
/// carrying framed packets (`encode_frame` / `parse_frame_rest`).
pub type UdpDialer = Arc<dyn Fn() -> Result<std::os::unix::net::UnixStream> + Send + Sync>;

/// A [`UdpDialer`] that dials `remote` over the test node's tailnet, once per
/// call: each browser socket gets its own tailnet flow, as it would through
/// a real Tailscale client.
pub fn tailnet_udp_dialer(
    test_node: Arc<crate::harness::containers::TestNode>,
    remote: String,
) -> UdpDialer {
    Arc::new(move || {
        let fd = test_node.dial(&remote)?.into_raw_fd();
        // SAFETY: `into_raw_fd` hands over ownership of a fresh fd that
        // nothing else holds.
        Ok(unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) })
    })
}

/// Proxies between a local loopback UDP socket (what the browser sees) and
/// the server, over flows opened with `dial`.
///
/// Every browser source address gets its own upstream flow, and replies on a
/// flow go back to that address only. A browser can hold several QUIC
/// connections on several sockets at once (Firefox opens two per
/// WebTransport connect and keeps a navigated-away page's connection open);
/// answering whichever socket spoke last hands one connection's packets to
/// another, where they cannot be decrypted, and that connection stalls.
///
/// Returns the bound `SocketAddr` that the browser should connect to.
pub async fn start_forwarder(local_bind: &str, dial: UdpDialer) -> Result<SocketAddr> {
    let sock = Arc::new(UdpSocket::bind(local_bind).await?);
    let local = sock.local_addr()?;

    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        let mut flows: HashMap<SocketAddr, OwnedWriteHalf> = HashMap::new();
        loop {
            // Browser → server, on the sender's own flow.
            let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                return;
            };
            let writer = match flows.entry(from) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(slot) => {
                    let upstream = match dial().and_then(|s| {
                        s.set_nonblocking(true)?;
                        Ok(TokioUnixStream::from_std(s)?)
                    }) {
                        Ok(u) => u,
                        Err(e) => {
                            tracing::warn!(error = %e, %from, "udp forwarder: dial failed; packet dropped");
                            continue;
                        }
                    };
                    let (reader, writer) = upstream.into_split();
                    tokio::spawn(relay_replies(reader, sock.clone(), from));
                    slot.insert(writer)
                }
            };
            let frame = encode_frame(&buf[..n], &from);
            if writer.write_all(&frame).await.is_err() {
                // The flow is gone; the next packet from `from` dials anew.
                flows.remove(&from);
            }
        }
    });

    Ok(local)
}

/// Server → browser for one flow: every framed reply goes to `client`, the
/// browser socket the flow was opened for.
async fn relay_replies(mut upstream: OwnedReadHalf, sock: Arc<UdpSocket>, client: SocketAddr) {
    let mut header = [0u8; 8];
    loop {
        if upstream.read_exact(&mut header).await.is_err() {
            return;
        }
        let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
        let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
        let mut rest = vec![0u8; total_len - 8];
        if upstream.read_exact(&mut rest).await.is_err() {
            return;
        }
        let Ok(pkt) = parse_frame_rest(&rest, payload_len) else {
            continue;
        };
        let _ = sock.send_to(&pkt.payload, client).await;
    }
}

/// Serve a directory over HTTP on 127.0.0.1:<random>. Returns the bound address.
/// Used so Chromium loads the page from a secure-context origin (http://127.0.0.1),
/// which is required for WebTransport. `file://` origins are not a secure context.
pub async fn start_static_server(dir: impl AsRef<Path>) -> Result<SocketAddr> {
    let app = Router::new().fallback_service(ServeDir::new(dir.as_ref()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(addr)
}

/// Bridges a local loopback TCP listener to a tsnet TCP dial. Each
/// accepted browser connection opens a fresh tsnet-side dial via the
/// test node, and a bidirectional copy task ferries bytes both ways
/// until either side closes.
///
/// Returns the bound `SocketAddr` the browser should connect to.
pub async fn start_tcp_forwarder(
    local_bind: &str,
    test_node: std::sync::Arc<crate::harness::containers::TestNode>,
    remote: String,
) -> Result<SocketAddr> {
    let listener = TcpListener::bind(local_bind).await?;
    let local = listener.local_addr()?;

    tokio::spawn(async move {
        loop {
            let Ok((mut downstream, _)) = listener.accept().await else {
                return;
            };
            let test_node = test_node.clone();
            let remote = remote.clone();
            tokio::spawn(async move {
                let upstream_std = match test_node.dial_tcp(&remote) {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::warn!(error = %e, %remote, "tcp forwarder dial failed");
                        return;
                    }
                };
                if upstream_std.set_nonblocking(true).is_err() {
                    return;
                }
                let Ok(mut upstream) = tokio::net::UnixStream::from_std(upstream_std) else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
            });
        }
    });

    Ok(local)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// The far end of one dialed flow: echoes every framed payload back,
    /// framed the way ghostbridge frames what it receives from the tailnet.
    async fn echo_upstream(mut stream: TokioUnixStream) {
        let mut header = [0u8; 8];
        loop {
            if stream.read_exact(&mut header).await.is_err() {
                return;
            }
            let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
            let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
            let mut rest = vec![0u8; total_len - 8];
            if stream.read_exact(&mut rest).await.is_err() {
                return;
            }
            let pkt = parse_frame_rest(&rest, payload_len).expect("well-formed frame");
            let reply = encode_frame(&pkt.payload, &"100.64.0.1:443".parse().unwrap());
            if stream.write_all(&reply).await.is_err() {
                return;
            }
        }
    }

    /// Firefox opens several QUIC connections from several UDP sockets (and
    /// keeps a navigated-away page's connection open). Each browser socket
    /// must get its own upstream flow and only its own replies: the server
    /// tells connections apart by connection ID, but a reply delivered to
    /// another connection's socket is undecryptable there, and that
    /// connection stalls. Sending interleaved from two sockets makes a
    /// forwarder that answers "whoever spoke last" deliver the wrong replies.
    #[tokio::test]
    async fn each_browser_socket_gets_its_own_flow_and_only_its_replies() {
        let dials = Arc::new(AtomicUsize::new(0));
        let counter = dials.clone();
        let dial: UdpDialer = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
            theirs.set_nonblocking(true)?;
            tokio::spawn(echo_upstream(TokioUnixStream::from_std(theirs)?));
            Ok(ours)
        });
        let forwarder = start_forwarder("127.0.0.1:0", dial).await.unwrap();

        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        const N: u8 = 20;
        for i in 0..N {
            a.send_to(&[b'a', i], forwarder).await.unwrap();
            b.send_to(&[b'b', i], forwarder).await.unwrap();
        }
        for (sock, tag) in [(&a, b'a'), (&b, b'b')] {
            let mut buf = [0u8; 64];
            for _ in 0..N {
                let (n, _) = tokio::time::timeout(Duration::from_secs(5), sock.recv_from(&mut buf))
                    .await
                    .expect("an echo arrives")
                    .unwrap();
                assert_eq!(n, 2);
                assert_eq!(
                    buf[0] as char, tag as char,
                    "socket '{}' received another socket's reply",
                    tag as char
                );
            }
        }
        assert_eq!(
            dials.load(Ordering::SeqCst),
            2,
            "one upstream flow per browser socket"
        );
    }
}
