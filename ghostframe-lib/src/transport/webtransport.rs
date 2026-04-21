//! Minimal HTTP/3 + WebTransport handshake, implemented as a sans-IO layer on
//! top of quinn-proto.
//!
//! [`WebTransportServer`] owns no QUIC state; its methods receive a mutable
//! reference to a `quinn_proto::Connection` and advance the handshake by
//! reading/writing HTTP/3 streams.
//!
//! # State machine (per connection)
//!
//! ```text
//! new() ──► [idle]
//!   on_new_connection:  open control stream, write SETTINGS  → [settings_sent]
//!   on_stream_readable (uni, peer control): buffer + decode Settings
//!       → peer_settings_received = true
//!   on_stream_opened   (bi):  accept stream id
//!   on_stream_readable (bi, session candidate): buffer + decode CONNECT, send 200
//!       → connected = true, session_stream = Some(id)
//! ```
//!
//! # Datagram framing (RFC 9297)
//!
//! WebTransport datagrams prepend a "quarter stream ID" VarInt:
//! `quarter_id = session_stream_id / 4`.

use std::collections::HashMap;

use bytes::{Bytes, BytesMut};
use quinn_proto::{Connection, Dir, SendDatagramError, StreamId};
use web_transport_proto::{ConnectRequest, ConnectResponse, Settings, VarInt};

// ─── per-stream read buffer ───────────────────────────────────────────────────

/// Accumulated bytes for an in-progress stream that hasn't been fully decoded yet.
#[derive(Default)]
struct StreamBuf(BytesMut);

impl StreamBuf {
    fn extend(&mut self, data: &[u8]) {
        self.0.extend_from_slice(data);
    }

    fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

// ─── main struct ─────────────────────────────────────────────────────────────

/// Sans-IO WebTransport handshake state for one QUIC connection.
pub struct WebTransportServer {
    /// We have sent our HTTP/3 SETTINGS on our unidirectional control stream.
    settings_sent: bool,
    /// We have received and decoded the peer's HTTP/3 SETTINGS.
    peer_settings_received: bool,
    /// Stream ID of our outgoing unidirectional control stream.
    our_control_stream: Option<StreamId>,
    /// Stream ID of the peer's unidirectional control stream (detected by
    /// seeing it carry a SETTINGS frame).
    peer_control_stream: Option<StreamId>,
    /// Stream ID of the accepted WebTransport CONNECT bidirectional stream.
    session_stream: Option<StreamId>,
    /// Set to `true` once the 200 response has been written and the session is live.
    connected: bool,
    /// Per-stream read buffers for in-progress decodes.
    stream_bufs: HashMap<StreamId, StreamBuf>,
    /// Stream IDs of peer-opened bidirectional streams that have been accepted
    /// but not yet seen in `on_stream_readable`.  We queue them here from
    /// `on_stream_opened` (which doesn't yet have data).
    pending_bidi_streams: Vec<StreamId>,
    /// Data received on non-session bidi streams after the session is established.
    /// Used for ReceiverFeedback messages from the client.
    feedback_queue: Vec<Vec<u8>>,
}

impl WebTransportServer {
    /// Create a fresh, disconnected session handler.
    pub fn new() -> Self {
        Self {
            settings_sent: false,
            peer_settings_received: false,
            our_control_stream: None,
            peer_control_stream: None,
            session_stream: None,
            connected: false,
            stream_bufs: HashMap::new(),
            pending_bidi_streams: Vec::new(),
            feedback_queue: Vec::new(),
        }
    }

    /// Returns `true` once the WebTransport CONNECT handshake has completed.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Return all stream IDs known to this session (for force-read polling).
    pub fn all_known_stream_ids(&self) -> Vec<StreamId> {
        let mut ids: Vec<StreamId> = self.stream_bufs.keys().copied().collect();
        ids.extend(self.pending_bidi_streams.iter().copied());
        if let Some(s) = self.peer_control_stream {
            ids.push(s);
        }
        if let Some(s) = self.session_stream {
            ids.push(s);
        }
        ids
    }

    // ── Called by the I/O bridge ─────────────────────────────────────────────

    /// Called immediately after a new QUIC connection is accepted.
    ///
    /// Opens a unidirectional control stream and writes our HTTP/3 SETTINGS
    /// (ENABLE_CONNECT_PROTOCOL, ENABLE_DATAGRAM, WEBTRANSPORT_MAX_SESSIONS).
    pub fn on_new_connection(&mut self, conn: &mut Connection) {
        if self.settings_sent {
            return;
        }

        // Open a unidirectional stream for our HTTP/3 control stream.
        let sid = match conn.streams().open(Dir::Uni) {
            Some(id) => id,
            None => {
                tracing::warn!("could not open control stream: stream limit reached");
                return;
            }
        };

        // Build SETTINGS payload using web-transport-proto.
        let mut settings = Settings::default();
        settings.enable_webtransport(1);

        let mut buf = BytesMut::new();
        settings.encode(&mut buf);

        match conn.send_stream(sid).write(&buf) {
            Ok(n) if n == buf.len() => {
                tracing::debug!(stream_id = ?sid, bytes = n, "HTTP/3 SETTINGS sent");
            }
            Ok(n) => {
                // Partial write: the stream is flow-controlled but the bytes
                // are queued in quinn-proto's send buffer.  For the control
                // stream this is fine; the rest will be flushed automatically.
                tracing::debug!(stream_id = ?sid, written = n, total = buf.len(),
                    "HTTP/3 SETTINGS partial write");
            }
            Err(e) => {
                tracing::warn!(stream_id = ?sid, error = ?e, "failed to write SETTINGS");
                return;
            }
        }

        self.our_control_stream = Some(sid);
        self.settings_sent = true;
    }

    /// Called when quinn-proto reports `StreamEvent::Opened { dir }`.
    ///
    /// We must drain all newly-opened remote streams via
    /// `conn.streams().accept(dir)` before returning.
    pub fn on_stream_opened(&mut self, conn: &mut Connection, dir: Dir) {
        // Accept *all* newly opened streams in this direction.
        let mut new_streams = Vec::new();
        while let Some(sid) = conn.streams().accept(dir) {
            tracing::debug!(stream_id = ?sid, ?dir, "peer opened stream");
            if dir == Dir::Bi {
                self.pending_bidi_streams.push(sid);
            }
            new_streams.push(sid);
        }
        // Quinn-proto may buffer initial data with the stream-open event
        // without generating a separate Readable event.  Eagerly try to
        // read from every newly-accepted stream so the handshake doesn't
        // stall waiting for an event that will never come.
        for sid in new_streams {
            self.on_stream_readable(conn, sid);
        }
    }

    /// Called when quinn-proto reports `StreamEvent::Readable { id }`.
    ///
    /// Drains available bytes, then tries to decode the appropriate HTTP/3
    /// message depending on what we know about the stream.
    pub fn on_stream_readable(&mut self, conn: &mut Connection, sid: StreamId) {
        // Drain all available bytes from this stream.
        let bytes = match drain_stream(conn, sid) {
            Ok(b) => b,
            Err(DrainError::Read(quinn_proto::ReadError::Blocked)) => {
                tracing::debug!(stream_id = ?sid, "stream read: blocked (no data yet)");
                return;
            }
            Err(DrainError::Readable(ref e)) => {
                tracing::warn!(stream_id = ?sid, error = ?e, "stream read: readable error");
                return;
            }
            Err(e) => {
                tracing::warn!(stream_id = ?sid, error = %e, "stream read error");
                return;
            }
        };

        if bytes.is_empty() {
            tracing::trace!(stream_id = ?sid, "stream read: 0 bytes");
            return;
        }

        tracing::debug!(stream_id = ?sid, bytes = bytes.len(), "stream data read");

        // Determine what this stream carries.
        let is_known_peer_control = self.peer_control_stream == Some(sid);
        let is_known_session = self.session_stream == Some(sid);
        let is_pending_bidi = self.pending_bidi_streams.contains(&sid);

        if is_known_peer_control {
            // Should not happen in steady state, but handle gracefully.
            tracing::debug!(stream_id = ?sid, "extra bytes on peer control stream (ignored)");
            return;
        }

        if is_known_session {
            // Data on the session bidi stream after CONNECT: ignore for now
            // (Task 8 will use capsule framing here).
            tracing::debug!(stream_id = ?sid, bytes = bytes.len(), "data on session stream (queued for Task 8)");
            return;
        }

        // Accumulate into stream buffer.
        let buf = self.stream_bufs.entry(sid).or_default();
        buf.extend(&bytes);

        // Decide what to try to decode.
        if is_pending_bidi || sid.dir() == Dir::Bi {
            self.try_decode_connect(conn, sid);
        } else {
            // Unidirectional: try to decode as SETTINGS.
            self.try_decode_settings(sid);
        }
    }

    /// Called when quinn-proto reports `Event::DatagramReceived`.
    ///
    /// Drains one WebTransport datagram from the connection.  Strips the
    /// quarter-stream-id VarInt prefix (RFC 9297) and returns the raw payload,
    /// or `None` if no datagrams are available or the session is not yet live.
    pub fn recv_datagram(&mut self, conn: &mut Connection) -> Option<Vec<u8>> {
        if !self.connected {
            return None;
        }
        let raw = conn.datagrams().recv()?;
        let mut slice: &[u8] = &raw;
        // Strip quarter-stream-id prefix.
        match VarInt::decode(&mut slice) {
            Ok(_quarter_id) => Some(slice.to_vec()),
            Err(e) => {
                tracing::warn!(error = ?e, "datagram VarInt prefix decode failed");
                None
            }
        }
    }

    /// Send a WebTransport datagram.
    ///
    /// Prepends the quarter-stream-id VarInt per RFC 9297, then hands the
    /// buffer to `conn.datagrams().send(...)`.
    pub fn send_datagram(
        &mut self,
        conn: &mut Connection,
        payload: &[u8],
    ) -> Result<(), SendDatagramError> {
        if !self.connected {
            return Err(SendDatagramError::Disabled);
        }
        let session_id = self
            .session_stream
            .expect("connected implies session_stream is Some");
        let quarter_id = u64::from(session_id) / 4;

        let mut buf = BytesMut::new();
        VarInt::from_u64(quarter_id)
            .expect("stream id / 4 always fits in VarInt")
            .encode(&mut buf);
        buf.extend_from_slice(payload);

        conn.datagrams().send(Bytes::from(buf), true)
    }

    /// Drain all queued feedback data received on non-session bidi streams.
    pub fn drain_feedback(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.feedback_queue)
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    /// Try to decode a peer SETTINGS frame from the buffered data for `sid`.
    fn try_decode_settings(&mut self, sid: StreamId) {
        let data = match self.stream_bufs.get(&sid) {
            Some(b) => b.as_slice().to_vec(),
            None => return,
        };

        let mut slice: &[u8] = &data;
        match Settings::decode(&mut slice) {
            Ok(settings) => {
                let wt_sessions = settings.supports_webtransport();
                tracing::info!(
                    stream_id = ?sid,
                    webtransport_sessions = wt_sessions,
                    "received peer HTTP/3 SETTINGS"
                );
                if wt_sessions == 0 {
                    tracing::warn!("peer SETTINGS do not advertise WebTransport support");
                }
                self.peer_control_stream = Some(sid);
                self.peer_settings_received = true;
                // Remove the stream buffer; we no longer need it.
                self.stream_bufs.remove(&sid);
            }
            Err(web_transport_proto::SettingsError::UnexpectedEnd) => {
                // Partial data — wait for more bytes.
            }
            Err(web_transport_proto::SettingsError::UnexpectedStreamType(_)) => {
                // Chrome opens QPACK encoder (0x02) and decoder (0x03) streams
                // alongside the HTTP/3 control stream (0x00). We don't need
                // their contents for M0; silently drop the buffer so we stop
                // trying to decode them as SETTINGS.
                tracing::trace!(stream_id = ?sid, "dropping non-control uni stream");
                self.stream_bufs.remove(&sid);
            }
            Err(e) => {
                tracing::warn!(stream_id = ?sid, error = ?e, "SETTINGS decode error");
                self.stream_bufs.remove(&sid);
            }
        }
    }

    /// Try to decode a WebTransport CONNECT request from the buffered data for `sid`.
    ///
    /// On success: sends a 200 response and marks the session as connected.
    fn try_decode_connect(&mut self, conn: &mut Connection, sid: StreamId) {
        if self.connected {
            // Already have a session — this bidi stream is not a CONNECT request.
            // Route its data to the feedback queue (used for ReceiverFeedback).
            if let Some(buf) = self.stream_bufs.remove(&sid) {
                let data = buf.as_slice().to_vec();
                if !data.is_empty() {
                    self.feedback_queue.push(data);
                }
            }
            self.pending_bidi_streams.retain(|&s| s != sid);
            return;
        }

        let data = match self.stream_bufs.get(&sid) {
            Some(b) => b.as_slice().to_vec(),
            None => return,
        };

        let mut slice: &[u8] = &data;
        match ConnectRequest::decode(&mut slice) {
            Ok(req) => {
                tracing::info!(
                    stream_id = ?sid,
                    url = %req.url,
                    "WebTransport CONNECT request received"
                );

                // Send 200 OK response.
                let response = ConnectResponse::OK;
                let mut resp_buf = BytesMut::new();
                if let Err(e) = response.encode(&mut resp_buf) {
                    tracing::warn!(stream_id = ?sid, error = ?e, "ConnectResponse encode failed");
                    return;
                }

                // ConnectResponse is tiny (~20 bytes) and written on a fresh
                // bidi stream, so a partial write would indicate flow-control
                // exhaustion that points at a deeper problem. Warn rather
                // than silently accept a short write.
                match conn.send_stream(sid).write(&resp_buf) {
                    Ok(n) if n == resp_buf.len() => {
                        tracing::debug!(stream_id = ?sid, bytes = n, "WebTransport 200 response sent");
                    }
                    Ok(n) => {
                        tracing::warn!(
                            stream_id = ?sid,
                            written = n,
                            expected = resp_buf.len(),
                            "short write on CONNECT response"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(stream_id = ?sid, error = ?e, "failed to write CONNECT response");
                        return;
                    }
                }

                self.session_stream = Some(sid);
                self.connected = true;
                // Remove from pending and buffer.
                self.pending_bidi_streams.retain(|&s| s != sid);
                self.stream_bufs.remove(&sid);

                tracing::info!(stream_id = ?sid, "WebTransport session established");
            }
            Err(web_transport_proto::ConnectError::UnexpectedEnd) => {
                // Partial data — wait for more bytes.
            }
            Err(e) => {
                tracing::warn!(stream_id = ?sid, error = ?e, "CONNECT decode error");
                self.stream_bufs.remove(&sid);
                self.pending_bidi_streams.retain(|&s| s != sid);
            }
        }
    }
}

impl Default for WebTransportServer {
    fn default() -> Self {
        Self::new()
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Errors that can occur when draining a recv stream.
#[derive(Debug)]
enum DrainError {
    Readable(quinn_proto::ReadableError),
    Read(quinn_proto::ReadError),
}

impl std::fmt::Display for DrainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DrainError::Readable(e) => write!(f, "readable error: {e:?}"),
            DrainError::Read(e) => write!(f, "read error: {e:?}"),
        }
    }
}

/// Drain all currently available bytes from a recv stream into a Vec.
///
/// Returns an empty Vec if there is no data yet (not an error).
fn drain_stream(conn: &mut Connection, sid: StreamId) -> Result<Vec<u8>, DrainError> {
    let mut out = Vec::new();
    let mut recv = conn.recv_stream(sid);
    let mut chunks = recv.read(true).map_err(DrainError::Readable)?;
    loop {
        match chunks.next(usize::MAX) {
            Ok(Some(chunk)) => {
                out.extend_from_slice(&chunk.bytes);
            }
            Ok(None) => break,                          // FIN reached
            Err(quinn_proto::ReadError::Blocked) => break, // no more data yet
            Err(e) => {
                let _ = chunks.finalize();
                return Err(DrainError::Read(e));
            }
        }
    }
    let _ = chunks.finalize();
    Ok(out)
}

// ─── unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use web_transport_proto::{Settings, VarInt};

    // ── Construction ─────────────────────────────────────────────────────────

    #[test]
    fn new_is_disconnected() {
        let wt = WebTransportServer::new();
        assert!(!wt.is_connected());
        assert!(!wt.settings_sent);
        assert!(!wt.peer_settings_received);
        assert!(wt.our_control_stream.is_none());
        assert!(wt.peer_control_stream.is_none());
        assert!(wt.session_stream.is_none());
    }

    #[test]
    fn default_is_same_as_new() {
        let a = WebTransportServer::new();
        let b = WebTransportServer::default();
        assert_eq!(a.is_connected(), b.is_connected());
    }

    // ── VarInt quarter-stream-id encoding ────────────────────────────────────

    /// Encode a u64 as a QUIC VarInt and return the bytes.
    fn encode_varint(v: u64) -> Vec<u8> {
        let mut buf = BytesMut::new();
        VarInt::from_u64(v).unwrap().encode(&mut buf);
        buf.to_vec()
    }

    /// Decode a VarInt from the front of a byte slice; return (value, remaining).
    fn decode_varint(data: &[u8]) -> (u64, &[u8]) {
        let mut slice = data;
        let v = VarInt::decode(&mut slice).unwrap();
        (v.into_inner(), slice)
    }

    #[test]
    fn varint_roundtrip_small() {
        for v in [0u64, 1, 62, 63, 64, 127, 255] {
            let encoded = encode_varint(v);
            let (decoded, rest) = decode_varint(&encoded);
            assert_eq!(decoded, v, "roundtrip failed for {v}");
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn varint_roundtrip_medium() {
        // Values in 2^14 - 1 range (2-byte VarInt).
        for v in [256u64, 1000, 16383] {
            let encoded = encode_varint(v);
            assert_eq!(encoded.len(), 2, "expected 2-byte encoding for {v}");
            let (decoded, rest) = decode_varint(&encoded);
            assert_eq!(decoded, v);
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn varint_roundtrip_large() {
        // 4-byte range.
        for v in [16384u64, 1_000_000, 1_073_741_823] {
            let encoded = encode_varint(v);
            assert_eq!(encoded.len(), 4, "expected 4-byte encoding for {v}");
            let (decoded, rest) = decode_varint(&encoded);
            assert_eq!(decoded, v);
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn quarter_stream_id_encoding() {
        // For a bidi stream id = 4 (0th server bidi stream in quinn-proto), quarter = 1.
        let stream_id_raw: u64 = 4;
        let quarter_id = stream_id_raw / 4;
        let encoded = encode_varint(quarter_id);
        let (decoded, _) = decode_varint(&encoded);
        assert_eq!(decoded, 1);
    }

    // ── SETTINGS frame byte-level golden test ─────────────────────────────────

    #[test]
    fn settings_encode_contains_required_fields() {
        let mut settings = Settings::default();
        settings.enable_webtransport(1);

        let mut buf = BytesMut::new();
        settings.encode(&mut buf);
        let wire = buf.to_vec();

        // The wire bytes must start with the CONTROL stream type (0x00).
        assert_eq!(
            wire[0], 0x00,
            "first byte should be CONTROL stream type 0x00"
        );

        // Must be decodable back.
        let mut slice: &[u8] = &wire;
        let decoded = Settings::decode(&mut slice).unwrap();
        assert!(
            decoded.supports_webtransport() >= 1,
            "decoded settings must report WebTransport support"
        );
    }

    #[test]
    fn settings_encode_roundtrip() {
        let mut settings = Settings::default();
        settings.enable_webtransport(1);

        let mut buf = BytesMut::new();
        settings.encode(&mut buf);

        let mut slice: &[u8] = &buf;
        let decoded = Settings::decode(&mut slice).unwrap();
        assert_eq!(decoded.supports_webtransport(), 1);
    }

    // ── partial-data robustness ───────────────────────────────────────────────

    #[test]
    fn settings_partial_decode_returns_unexpected_end() {
        let mut settings = Settings::default();
        settings.enable_webtransport(1);
        let mut buf = BytesMut::new();
        settings.encode(&mut buf);
        let full = buf.to_vec();

        // Try every prefix shorter than the full encoding.
        for prefix_len in 1..full.len() {
            let partial = &full[..prefix_len];
            let mut slice: &[u8] = partial;
            let result = Settings::decode(&mut slice);
            assert!(
                matches!(
                    result,
                    Err(web_transport_proto::SettingsError::UnexpectedEnd
                        | web_transport_proto::SettingsError::InvalidSize)
                ),
                "expected UnexpectedEnd or InvalidSize for prefix_len={prefix_len}, got {result:?}"
            );
        }
    }
}
