pub mod capture;
pub mod encoder;
pub mod ffi;
pub mod server;
pub mod tile;
pub mod transport;

pub use server::{FrameSubmission, GhostframeServer};
pub use transport::ghostbridge::{GhostbridgeConfig, GhostbridgeError};
pub use transport::io_bridge::IoBridge;

/// Framing primitives re-exported for the E2E test harness.
///
/// These encode/decode the length-prefixed frame format used on the
/// ghostbridge socketpair fd so the integration test can forward UDP
/// packets between a loopback socket and the tsnet `UdpPacketConn`
/// without duplicating the wire-format implementation.
pub mod framing {
    pub use crate::transport::ghostbridge::{encode_frame, parse_frame_rest, UdpPacket};
}
