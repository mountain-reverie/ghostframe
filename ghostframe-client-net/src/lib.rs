//! Sans-IO QUIC + WebTransport client session wrapping `ClientCore`.
//!
//! There is deliberately no dial API and no socket code in this crate: the
//! embedder supplies the byte pump. In production that pump is ghostbridge's
//! `dial_udp` through tsnet, so a client can never escape the tailnet; in
//! tests it is the netsim.

mod event;
pub mod tls;

pub use event::{ClientNetError, ClientNetEvent, UdpOut};

#[derive(Debug, Clone)]
pub struct ClientNetConfig {
    /// SNI name presented in the TLS handshake.
    pub server_name: String,
    /// The only certificate this client will accept, by SHA-256 of its DER.
    pub server_cert_sha256: [u8; 32],
    pub indices_raw_enabled: bool,
    pub supports_h264: bool,
}

pub struct ClientNet {
    config: ClientNetConfig,
    connected: bool,
}

impl ClientNet {
    pub fn new(config: ClientNetConfig, _now_us: u64) -> Result<Self, ClientNetError> {
        Ok(Self {
            config,
            connected: false,
        })
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn poll_transmit(&mut self) -> Option<UdpOut> {
        None
    }

    pub fn server_name(&self) -> &str {
        &self.config.server_name
    }
}
