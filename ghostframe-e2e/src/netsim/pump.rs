//! Carries datagrams over a Unix socketpair using ghostbridge's real wire
//! framing.
//!
//! `SocketPairPump` does framing only — no impairment logic. `NetSim`
//! (see [`crate::netsim`]) decides datagram fates elsewhere; the pump's job
//! is purely to move bytes across a `tokio::net::UnixStream` using the same
//! `[total_len u32 BE][payload_len u32 BE][payload][port u16 BE][host\0]`
//! frame format the production I/O bridge speaks, so the harness cannot
//! drift from what ghostbridge actually produces on the wire.
//!
//! This reuses [`encode_frame`] and [`parse_frame_rest`] from
//! `ghostframe_lib::transport::ghostbridge` rather than reimplementing the
//! framing — those are `pub` specifically so this harness can share them.

use ghostframe_lib::transport::ghostbridge::{
    encode_frame, parse_frame_rest, UdpPacket, MAX_FRAME_LEN,
};
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Moves framed UDP datagrams across a `tokio::net::UnixStream`, mirroring
/// the framing the production `io_bridge::run` reader speaks
/// (`process_inbound`, ghostframe-lib `src/transport/io_bridge.rs`).
pub struct SocketPairPump {
    stream: UnixStream,
}

impl SocketPairPump {
    /// Wrap an already-connected `UnixStream` half of a socketpair.
    pub fn new(stream: UnixStream) -> Self {
        Self { stream }
    }

    /// Read one framed datagram off the stream.
    ///
    /// Mirrors `io_bridge::IoBridge::process_inbound`'s framing exactly:
    /// reads the 8-byte header, validates `total_len` against the declared
    /// `payload_len`, then reads exactly `total_len - 8` more bytes —
    /// `total_len` includes its own 8-byte header, so the remainder is
    /// `total_len - 8`, not `total_len`.
    ///
    /// Like `process_inbound`, this rejects a `total_len` above
    /// [`MAX_FRAME_LEN`] before allocating the remainder buffer: the
    /// too-short check alone only rejects frames that are too *small*, so a
    /// corrupted or fuzzed `total_len` would otherwise drive a
    /// multi-gigabyte allocation and then hang in `read_exact` waiting for
    /// bytes that will never arrive. Both sides share [`MAX_FRAME_LEN`],
    /// defined next to the framing functions, so the harness and the
    /// production reader cannot drift apart on what a legal frame is.
    pub async fn recv(&mut self) -> io::Result<UdpPacket> {
        let mut header = [0u8; 8];
        self.stream.read_exact(&mut header).await?;

        let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
        let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;

        // Minimum valid frame: header (8) + payload + port (2) + NUL (1).
        if total_len < 8 + payload_len + 3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too short",
            ));
        }
        // Reject an oversized total_len before allocating the remainder
        // buffer. Without this, a corrupted or fuzzed length field would
        // drive a multi-gigabyte `vec![0u8; rest_len]` and then hang in
        // `read_exact` waiting for bytes that will never arrive, since the
        // `total_len < 8 + payload_len + 3` check above only rejects frames
        // that are too small, not ones that are absurdly large.
        if total_len > MAX_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame total_len exceeds maximum",
            ));
        }
        let rest_len = total_len.checked_sub(8).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "frame total_len underflow")
        })?;
        let mut rest = vec![0u8; rest_len];
        self.stream.read_exact(&mut rest).await?;

        parse_frame_rest(&rest, payload_len)
    }

    /// Encode `payload` + `addr` as a framed packet and write it to the
    /// stream, using the same `encode_frame` the production sync/async
    /// paths use.
    pub async fn send(&mut self, payload: &[u8], addr: &SocketAddr) -> io::Result<()> {
        let frame = encode_frame(payload, addr);
        self.stream.write_all(&frame).await
    }
}
