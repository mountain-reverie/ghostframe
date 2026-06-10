use std::net::SocketAddr;
use std::os::unix::io::FromRawFd;
use std::path::Path;

use anyhow::Result;
use axum::Router;
use ghostframe_lib::framing::{encode_frame, parse_frame_rest};
use ghostframe_lib::transport::ghostbridge::UdpPacketConn;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket, UnixStream as TokioUnixStream};
use tower_http::services::ServeDir;

/// Proxies between a local loopback UDP socket (what Chromium sees) and a
/// `UdpPacketConn` that forwards packets over tsnet to the server container.
///
/// Returns the bound `SocketAddr` that the browser should connect to.
pub async fn start_forwarder(local_bind: &str, upstream: UdpPacketConn) -> Result<SocketAddr> {
    let sock = UdpSocket::bind(local_bind).await?;
    let local = sock.local_addr()?;

    // Wrap the upstream UdpPacketConn fd in tokio for async I/O.
    let raw_fd = upstream.into_raw_fd();
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(raw_fd) };
    std_stream.set_nonblocking(true)?;
    let mut upstream = TokioUnixStream::from_std(std_stream)?;

    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        let mut header = [0u8; 8];
        let mut client: Option<SocketAddr> = None;

        loop {
            tokio::select! {
                // Browser → tsnet
                res = sock.recv_from(&mut buf) => {
                    let Ok((n, from)) = res else { return };
                    client = Some(from);
                    let frame = encode_frame(&buf[..n], &from);
                    if upstream.write_all(&frame).await.is_err() { return; }
                }
                // tsnet → browser
                res = upstream.read_exact(&mut header) => {
                    if res.is_err() { return }
                    let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
                    let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
                    let mut rest = vec![0u8; total_len - 8];
                    if upstream.read_exact(&mut rest).await.is_err() { return }
                    let Ok(pkt) = parse_frame_rest(&rest, payload_len) else { continue };
                    if let Some(dst) = client {
                        let _ = sock.send_to(&pkt.payload, dst).await;
                    }
                }
            }
        }
    });

    Ok(local)
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
