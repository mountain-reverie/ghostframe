use ghostframe_client_core::Event as CoreEvent;
use std::net::SocketAddr;

/// A datagram to hand to the embedder's byte pump (tsnet in production, the
/// netsim in tests). This crate never owns a socket.
#[derive(Debug, Clone, PartialEq)]
pub struct UdpOut {
    pub payload: Vec<u8>,
    pub destination: SocketAddr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientNetEvent {
    /// QUIC handshake finished.
    Connected,
    /// WebTransport CONNECT accepted; datagrams may now flow.
    SessionReady,
    ConnectionLost {
        reason: String,
    },
    /// Anything the client core surfaced.
    Core(CoreEvent),
}

#[derive(Debug, thiserror::Error)]
pub enum ClientNetError {
    #[error("TLS configuration failed: {0}")]
    Tls(String),
    #[error("connect failed: {0}")]
    Connect(String),
}
