//! HTTP/3 SETTINGS + WebTransport CONNECT handshake, driven from the client
//! side.
//!
//! Mirrors the client half of what `ghostframe_lib::transport::webtransport::
//! WebTransportServer` does on the server side: open a uni control stream and
//! write SETTINGS (`enable_webtransport`), open a bidi stream and write the
//! CONNECT request, then read the response back off that same bidi stream.
//!
//! The response is an HTTP/3 HEADERS frame that can arrive split across
//! several QUIC STREAM frames (and therefore several `handle_udp` calls), so
//! [`WebTransportHandshake::on_readable`] accumulates bytes across calls and
//! only reports "not yet" (`ConnectError::UnexpectedEnd`) rather than
//! treating a short read as failure -- see its doc comment for why that
//! matters.

use bytes::BytesMut;
use quinn_proto::{Connection, Dir, StreamId};
use url::Url;
use web_transport_proto::{ConnectError, ConnectRequest, ConnectResponse, Settings};

use crate::event::ClientNetError;

/// Result of attempting to advance the handshake by reading the session
/// stream.
#[derive(Debug)]
pub(crate) enum ConnectOutcome {
    /// Not enough bytes yet to decode a full `ConnectResponse`; call again
    /// once more data has arrived.
    Pending,
    /// The server accepted the session (status 200).
    Accepted,
    /// The server rejected the session, or the response could not be
    /// decoded at all.
    Rejected(String),
}

/// Sans-IO WebTransport CONNECT handshake, client side.
///
/// Owns no QUIC state of its own beyond the stream ids it opened; every
/// method takes the `&mut Connection` to act on, mirroring
/// `WebTransportServer`'s shape.
pub(crate) struct WebTransportHandshake {
    /// Stream id of the bidi stream carrying the CONNECT request/response.
    /// `None` until `start` has run. Task 8 reads this back out (via
    /// `ClientNet`) to compute the datagram quarter-id prefix.
    session_stream: Option<StreamId>,
    /// Bytes received so far on `session_stream`, accumulated across
    /// possibly-partial reads until `ConnectResponse::decode` succeeds.
    response_buf: BytesMut,
}

impl WebTransportHandshake {
    pub(crate) fn new() -> Self {
        Self {
            session_stream: None,
            response_buf: BytesMut::new(),
        }
    }

    pub(crate) fn session_stream(&self) -> Option<StreamId> {
        self.session_stream
    }

    /// Open the uni SETTINGS stream and the bidi CONNECT stream. Call once,
    /// right after `quinn_proto::Event::Connected` fires.
    pub(crate) fn start(
        &mut self,
        conn: &mut Connection,
        server_name: &str,
    ) -> Result<(), ClientNetError> {
        // Uni stream: `Settings::encode` writes the stream-type varint
        // (0x00, `StreamUni::CONTROL`) itself before the SETTINGS frame, so
        // there is nothing extra to prepend here.
        let uni_sid = conn
            .streams()
            .open(Dir::Uni)
            .ok_or_else(|| ClientNetError::Handshake("stream limit reached (uni)".into()))?;
        let mut settings = Settings::default();
        settings.enable_webtransport(1);
        let mut settings_buf = BytesMut::new();
        settings.encode(&mut settings_buf);
        conn.send_stream(uni_sid)
            .write(&settings_buf)
            .map_err(|e| ClientNetError::Handshake(format!("write SETTINGS: {e}")))?;

        // Bidi stream: the WebTransport CONNECT request.
        let bidi_sid = conn
            .streams()
            .open(Dir::Bi)
            .ok_or_else(|| ClientNetError::Handshake("stream limit reached (bidi)".into()))?;
        let url = Url::parse(&format!("https://{server_name}/.well-known/webtransport"))
            .map_err(|e| ClientNetError::Handshake(format!("CONNECT url: {e}")))?;
        let connect = ConnectRequest::new(url);
        let mut connect_buf = BytesMut::new();
        connect
            .encode(&mut connect_buf)
            .map_err(|e| ClientNetError::Handshake(format!("encode CONNECT: {e}")))?;
        conn.send_stream(bidi_sid)
            .write(&connect_buf)
            .map_err(|e| ClientNetError::Handshake(format!("write CONNECT: {e}")))?;

        self.session_stream = Some(bidi_sid);
        Ok(())
    }

    /// Called when quinn-proto reports `StreamEvent::Readable` for `sid`.
    /// The caller must only invoke this once it has confirmed
    /// `sid == session_stream` (mirrors `ClientNet::drain_connection_events`,
    /// which does that check before calling in).
    ///
    /// Drains whatever bytes are currently available on the stream, appends
    /// them to the running buffer, and retries `ConnectResponse::decode`.
    /// `ConnectError::UnexpectedEnd` means the HEADERS frame is still
    /// incomplete -- treated as "not yet", not as an error. A QUIC STREAM
    /// frame boundary has nothing to do with an HTTP/3 frame boundary, so a
    /// single read handing us only part of the response is the common case,
    /// not an edge case: it happens whenever the underlying UDP path
    /// fragments or reorders delivery of the packets carrying this stream's
    /// data (exactly what the netsim used by later tasks does on purpose).
    /// Deciding "not yet" only on `UnexpectedEnd` -- and treating every
    /// other decode error as a real rejection -- means a response that
    /// trickles in one byte at a time still completes the handshake.
    pub(crate) fn on_readable(&mut self, conn: &mut Connection, sid: StreamId) -> ConnectOutcome {
        match drain_stream(conn, sid) {
            Ok(bytes) => self.response_buf.extend_from_slice(&bytes),
            Err(e) => return ConnectOutcome::Rejected(format!("stream read: {e}")),
        }
        self.try_decode_response()
    }

    /// Attempt to decode a `ConnectResponse` from whatever has accumulated so
    /// far. Split out from `on_readable` so the fragmentation behaviour is
    /// testable without fabricating a live `quinn_proto::Connection` — see
    /// `response_split_across_arbitrary_chunks_still_decodes`.
    pub(crate) fn try_decode_response(&mut self) -> ConnectOutcome {
        let mut slice: &[u8] = &self.response_buf;
        match ConnectResponse::decode(&mut slice) {
            Ok(resp) => {
                // Compare the numeric code directly rather than importing
                // `http::StatusCode` ourselves: `web-transport-proto`
                // already depends on `http` (and re-exports it), but this
                // crate has no other use for it, so pulling in the whole
                // crate just to spell `StatusCode::OK` isn't worth it.
                if resp.status.as_u16() == 200 {
                    ConnectOutcome::Accepted
                } else {
                    ConnectOutcome::Rejected(format!(
                        "CONNECT rejected: status {}",
                        resp.status.as_u16()
                    ))
                }
            }
            Err(ConnectError::UnexpectedEnd) => ConnectOutcome::Pending,
            Err(e) => ConnectOutcome::Rejected(format!("CONNECT response decode: {e}")),
        }
    }
}

/// Drain all currently available bytes from a recv stream into a `Vec`.
///
/// Returns an empty `Vec` if there is no data yet (not an error). Same
/// shape as the private `drain_stream` in
/// `ghostframe_lib::transport::webtransport` (that one isn't reachable from
/// here: it's a free function, not part of the crate's public surface).
fn drain_stream(conn: &mut Connection, sid: StreamId) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut recv = conn.recv_stream(sid);
    let mut chunks = recv
        .read(true)
        .map_err(|e| format!("readable error: {e:?}"))?;
    loop {
        match chunks.next(usize::MAX) {
            Ok(Some(chunk)) => out.extend_from_slice(&chunk.bytes),
            Ok(None) => break,                             // FIN reached
            Err(quinn_proto::ReadError::Blocked) => break, // no more data yet
            Err(e) => {
                let _ = chunks.finalize();
                return Err(format!("read error: {e:?}"));
            }
        }
    }
    let _ = chunks.finalize();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use web_transport_proto::ConnectResponse;

    /// The response must survive arriving in arbitrarily small pieces.
    ///
    /// A QUIC STREAM frame boundary has nothing to do with an HTTP/3 frame
    /// boundary, so a single read handing over only part of the response is
    /// the normal case once the netsim starts fragmenting and delaying
    /// delivery on purpose. Feeding the encoded bytes one at a time is the
    /// worst case: every prefix but the last must report `Pending`, and the
    /// final byte must flip it to `Accepted`.
    #[test]
    fn response_split_across_arbitrary_chunks_still_decodes() {
        let mut encoded = bytes::BytesMut::new();
        ConnectResponse::OK.encode(&mut encoded).expect("encode");
        assert!(
            encoded.len() > 1,
            "a one-byte response would not test anything"
        );

        let mut hs = WebTransportHandshake::new();
        for (i, byte) in encoded.iter().enumerate() {
            hs.response_buf.extend_from_slice(&[*byte]);
            let outcome = hs.try_decode_response();
            if i + 1 < encoded.len() {
                assert!(
                    matches!(outcome, ConnectOutcome::Pending),
                    "byte {} of {}: a partial response must be Pending, got {:?}",
                    i + 1,
                    encoded.len(),
                    outcome
                );
            } else {
                assert!(
                    matches!(outcome, ConnectOutcome::Accepted),
                    "the final byte must complete the handshake, got {outcome:?}"
                );
            }
        }
    }

    /// A well-formed non-200 response is a rejection, not a stall.
    #[test]
    fn non_200_response_is_rejected_not_pending() {
        let mut encoded = bytes::BytesMut::new();
        ConnectResponse::new(web_transport_proto::http::StatusCode::FORBIDDEN)
            .encode(&mut encoded)
            .expect("encode");

        let mut hs = WebTransportHandshake::new();
        hs.response_buf.extend_from_slice(&encoded);
        assert!(matches!(
            hs.try_decode_response(),
            ConnectOutcome::Rejected(_)
        ));
    }
}
