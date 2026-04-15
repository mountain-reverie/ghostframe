//! Rust FFI bindings to the ghostbridge Go `c-archive`.
//!
//! Ghostbridge exposes `tsnet.Server.ListenPacket` / `Dial` as C-callable
//! functions that return a Unix-domain socketpair fd. Each datagram crossing
//! the fd is wrapped in a small length-prefixed frame so a single fd can
//! carry UDP payload + source/destination address:
//!
//! ```text
//! [total_len u32 BE] [payload_len u32 BE] [payload] [port u16 BE] [host\0]
//! ```
//!
//! The returned fd is in `O_NONBLOCK` mode; production callers consume it via
//! [`UdpPacketConn::into_raw_fd`] and wrap it in `tokio::net::UnixStream` in
//! the I/O bridge (Task 5). The helpers [`parse_frame_rest`] and
//! [`encode_frame`] are shared between the sync and async paths so there's
//! exactly one implementation of the wire format.

use std::ffi::{c_char, c_int, CStr, CString};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::io::{FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;

extern "C" {
    fn gbridge_new(
        hostname: *const c_char,
        authkey: *const c_char,
        state_dir: *const c_char,
        control_url: *const c_char,
        sd_out: *mut c_int,
    ) -> c_int;
    fn gbridge_up(sd: c_int) -> c_int;
    fn gbridge_listen_udp(sd: c_int, addr: *const c_char, fd_out: *mut c_int) -> c_int;
    fn gbridge_dial_udp(sd: c_int, remote_addr: *const c_char, fd_out: *mut c_int) -> c_int;
    fn gbridge_close(sd: c_int) -> c_int;
    fn gbridge_getips(sd: c_int, buf: *mut c_char, buf_len: usize) -> c_int;
}

#[derive(Debug, thiserror::Error)]
pub enum GhostbridgeError {
    /// Raw return code from a ghostbridge C function. See `ghostbridge/main.go`
    /// for the full set of negative values.
    #[error("ghostbridge C call failed: {0} (rc={1})")]
    Ffi(&'static str, c_int),
    #[error("invalid C string: {0}")]
    Cstring(#[from] std::ffi::NulError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid frame: {0}")]
    Frame(&'static str),
}

pub struct GhostbridgeConfig {
    pub hostname: String,
    pub authkey: String,
    pub state_dir: String,
    pub control_url: String,
}

pub struct GhostbridgeHandle {
    sd: c_int,
}

impl GhostbridgeHandle {
    pub fn connect(config: &GhostbridgeConfig) -> Result<Self, GhostbridgeError> {
        let c_hostname = CString::new(config.hostname.clone())?;
        let c_authkey = CString::new(config.authkey.clone())?;
        let c_state_dir = CString::new(config.state_dir.clone())?;
        let c_control_url = CString::new(config.control_url.clone())?;

        let mut sd: c_int = -1;
        let rc = unsafe {
            gbridge_new(
                c_hostname.as_ptr(),
                c_authkey.as_ptr(),
                c_state_dir.as_ptr(),
                c_control_url.as_ptr(),
                &mut sd,
            )
        };
        if rc < 0 {
            return Err(GhostbridgeError::Ffi("gbridge_new", rc));
        }
        Ok(Self { sd })
    }

    pub fn up(&self) -> Result<(), GhostbridgeError> {
        let rc = unsafe { gbridge_up(self.sd) };
        if rc < 0 {
            return Err(GhostbridgeError::Ffi("gbridge_up", rc));
        }
        Ok(())
    }

    /// Listen for incoming UDP packets on `addr` (e.g. `":4443"`).
    /// Returns an owned non-blocking socketpair fd wrapped as [`UdpPacketConn`].
    pub fn listen_udp(&self, addr: &str) -> Result<UdpPacketConn, GhostbridgeError> {
        let c_addr = CString::new(addr)?;
        let mut fd: c_int = -1;
        let rc = unsafe { gbridge_listen_udp(self.sd, c_addr.as_ptr(), &mut fd) };
        if rc < 0 {
            return Err(GhostbridgeError::Ffi("gbridge_listen_udp", rc));
        }
        Ok(UdpPacketConn::from_raw_fd(fd))
    }

    /// Dial a remote UDP address over the tailnet. Used by the E2E test to
    /// proxy browser packets through the test's embedded tsnet node.
    pub fn dial_udp(&self, remote_addr: &str) -> Result<UdpPacketConn, GhostbridgeError> {
        let c_remote = CString::new(remote_addr)?;
        let mut fd: c_int = -1;
        let rc = unsafe { gbridge_dial_udp(self.sd, c_remote.as_ptr(), &mut fd) };
        if rc < 0 {
            return Err(GhostbridgeError::Ffi("gbridge_dial_udp", rc));
        }
        Ok(UdpPacketConn::from_raw_fd(fd))
    }

    pub fn get_ips(&self) -> Result<Vec<IpAddr>, GhostbridgeError> {
        let mut buf = [0u8; 1024];
        let rc = unsafe { gbridge_getips(self.sd, buf.as_mut_ptr() as *mut c_char, buf.len()) };
        if rc < 0 {
            return Err(GhostbridgeError::Ffi("gbridge_getips", rc));
        }
        let ip_str = CStr::from_bytes_until_nul(&buf)
            .map_err(|_| GhostbridgeError::Frame("gbridge_getips: missing NUL"))?
            .to_str()
            .map_err(|_| GhostbridgeError::Frame("gbridge_getips: non-utf8"))?;
        if ip_str.is_empty() {
            return Ok(Vec::new());
        }
        ip_str
            .split(',')
            .map(|s| s.parse().map_err(|_| GhostbridgeError::Frame("invalid IP")))
            .collect()
    }
}

impl Drop for GhostbridgeHandle {
    fn drop(&mut self) {
        let rc = unsafe { gbridge_close(self.sd) };
        if rc < 0 {
            tracing::warn!(sd = self.sd, rc, "gbridge_close returned non-zero");
        }
    }
}

/// Framed UDP packet read from the ghostbridge fd.
#[derive(Debug, Clone)]
pub struct UdpPacket {
    pub payload: Vec<u8>,
    pub addr: SocketAddr,
}

/// Owns a ghostbridge socketpair fd until it is handed off to the tokio I/O
/// bridge via [`UdpPacketConn::into_raw_fd`]. The fd is in `O_NONBLOCK` mode,
/// so reading/writing through the contained `UnixStream` directly would
/// surface `WouldBlock` — production code should always wrap the raw fd in
/// `tokio::net::UnixStream` before use.
pub struct UdpPacketConn {
    stream: UnixStream,
}

impl UdpPacketConn {
    fn from_raw_fd(fd: c_int) -> Self {
        // Safety: the fd is freshly returned from ghostbridge and nothing else
        // owns it. ghostbridge sets O_NONBLOCK before returning the fd.
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        Self { stream }
    }

    /// Consume the wrapper and return the raw non-blocking fd. The caller is
    /// responsible for the fd's lifetime from this point forward — typically
    /// by handing it to `tokio::net::UnixStream::from_std`.
    pub fn into_raw_fd(self) -> RawFd {
        self.stream.into_raw_fd()
    }
}

/// Parse the post-header bytes of a frame into an [`UdpPacket`].
/// Shared between tests here and the tokio I/O bridge (Task 5 will be the
/// first non-test caller).
///
/// `rest` is expected to contain `[payload (payload_len bytes)][port u16 BE][host\0]`.
#[allow(dead_code)] // Task 5 wires this into the tokio I/O bridge.
pub(crate) fn parse_frame_rest(rest: &[u8], payload_len: usize) -> io::Result<UdpPacket> {
    // Minimum rest length is payload + 2 (port) + 1 (NUL terminator).
    if rest.len() < payload_len.saturating_add(3) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame rest shorter than declared payload",
        ));
    }

    let payload = rest[..payload_len].to_vec();
    let port = u16::from_be_bytes(rest[payload_len..payload_len + 2].try_into().unwrap());
    let host_bytes = &rest[payload_len + 2..];
    let host_str = CStr::from_bytes_until_nul(host_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame host not null-terminated"))?
        .to_str()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame host not utf-8"))?;

    // IPv6 address literals contain colons and must be bracketed before
    // parsing as a SocketAddr. ghostbridge stores the bare IP string
    // produced by `net.SplitHostPort`, so this is the only place to add
    // the brackets.
    let addr: SocketAddr = if host_str.contains(':') {
        format!("[{}]:{}", host_str, port)
    } else {
        format!("{}:{}", host_str, port)
    }
    .parse()
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid socket address"))?;

    Ok(UdpPacket { payload, addr })
}

/// Encode a UDP payload + destination address as a single framed packet for
/// the ghostbridge socketpair. Shared between sync tests and the tokio bridge.
#[allow(dead_code)] // Task 5 wires this into the tokio I/O bridge.
pub(crate) fn encode_frame(payload: &[u8], addr: &SocketAddr) -> Vec<u8> {
    let host = addr.ip().to_string();
    let host_bytes = host.as_bytes();
    let total_len = (4 + 4 + payload.len() + 2 + host_bytes.len() + 1) as u32;

    let mut frame = Vec::with_capacity(total_len as usize);
    frame.extend_from_slice(&total_len.to_be_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&addr.port().to_be_bytes());
    frame.extend_from_slice(host_bytes);
    frame.push(0);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(payload: &[u8], addr_str: &str) {
        let addr: SocketAddr = addr_str.parse().unwrap();
        let frame = encode_frame(payload, &addr);

        let total_len = u32::from_be_bytes(frame[0..4].try_into().unwrap()) as usize;
        let payload_len = u32::from_be_bytes(frame[4..8].try_into().unwrap()) as usize;
        assert_eq!(total_len, frame.len(), "total_len must cover the frame");
        assert_eq!(payload_len, payload.len(), "payload_len must match input");

        let pkt = parse_frame_rest(&frame[8..], payload_len).unwrap();
        assert_eq!(pkt.payload, payload);
        assert_eq!(pkt.addr, addr);
    }

    #[test]
    fn roundtrip_ipv4() {
        roundtrip(b"hello world", "100.64.0.2:4443");
    }

    #[test]
    fn roundtrip_ipv6() {
        // Tailscale allocates IPv6 ULAs in fd7a::/48; this regression-guards
        // the colon-bracketing path in parse_frame_rest.
        roundtrip(b"ipv6 datagram", "[fd7a:115c:a1e0::2]:4443");
    }

    #[test]
    fn roundtrip_empty_payload() {
        roundtrip(b"", "127.0.0.1:1");
    }

    #[test]
    fn parse_frame_rest_rejects_short_rest() {
        // Claim payload_len = 10 but only provide 4 bytes.
        let rest = [1, 2, 3, 4];
        let err = parse_frame_rest(&rest, 10).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
