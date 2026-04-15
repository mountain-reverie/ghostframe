use std::ffi::{c_char, c_int, CStr, CString};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
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
    /// Returns an owned socketpair fd already in O_NONBLOCK mode.
    pub fn listen_udp(&self, addr: &str) -> Result<UdpPacketConn, GhostbridgeError> {
        let c_addr = CString::new(addr)?;
        let mut fd: c_int = -1;
        let rc = unsafe { gbridge_listen_udp(self.sd, c_addr.as_ptr(), &mut fd) };
        if rc < 0 {
            return Err(GhostbridgeError::Ffi("gbridge_listen_udp", rc));
        }
        UdpPacketConn::from_raw_fd(fd)
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
        UdpPacketConn::from_raw_fd(fd)
    }

    pub fn get_ips(&self) -> Result<Vec<std::net::IpAddr>, GhostbridgeError> {
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
        unsafe { gbridge_close(self.sd) };
    }
}

/// Framed UDP packet read from the ghostbridge fd.
#[derive(Debug, Clone)]
pub struct UdpPacket {
    pub payload: Vec<u8>,
    pub addr: SocketAddr,
}

/// A framed UDP packet connection over a ghostbridge socketpair fd.
///
/// Owns the fd (via `UnixStream`) and is `!Sync` — all methods take `&mut self`.
/// The fd is in O_NONBLOCK mode; the sync methods below exist only for unit
/// tests. Production code consumes the raw fd via `into_raw_fd()` and wraps it
/// in `tokio::net::UnixStream` (see Task 5).
pub struct UdpPacketConn {
    stream: UnixStream,
}

impl UdpPacketConn {
    fn from_raw_fd(fd: c_int) -> Result<Self, GhostbridgeError> {
        // Safety: the fd is freshly returned from ghostbridge and nothing else owns it.
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        // ghostbridge already set O_NONBLOCK, but tell std explicitly so that
        // Read/Write honor it without EAGAIN being surfaced as an error in blocking calls.
        stream.set_nonblocking(false)?; // unit tests use blocking I/O
        Ok(Self { stream })
    }

    pub fn into_raw_fd(self) -> RawFd {
        self.stream.into_raw_fd()
    }

    /// **Blocking** framed read. Only used by unit tests; production path uses tokio.
    pub fn recv_from(&mut self) -> io::Result<UdpPacket> {
        let mut header = [0u8; 8];
        read_exact(&mut self.stream, &mut header)?;
        let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
        let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
        if total_len < 8 + payload_len + 3 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too short"));
        }
        let mut rest = vec![0u8; total_len - 8];
        read_exact(&mut self.stream, &mut rest)?;
        parse_frame_rest(&rest, payload_len)
    }

    /// **Blocking** framed write. Only used by unit tests.
    pub fn send_to(&mut self, payload: &[u8], addr: &SocketAddr) -> io::Result<()> {
        let frame = encode_frame(payload, addr);
        self.stream.write_all(&frame)
    }
}

fn read_exact(stream: &mut UnixStream, buf: &mut [u8]) -> io::Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let n = stream.read(&mut buf[off..])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
        }
        off += n;
    }
    Ok(())
}

/// Parse the post-header bytes of a frame into an `UdpPacket`.
/// Shared between the blocking path (above) and the tokio path (Task 5).
pub(crate) fn parse_frame_rest(rest: &[u8], payload_len: usize) -> io::Result<UdpPacket> {
    let payload = rest[..payload_len].to_vec();
    let port = u16::from_be_bytes(rest[payload_len..payload_len + 2].try_into().unwrap());
    let host_bytes = &rest[payload_len + 2..];
    let host_str = CStr::from_bytes_until_nul(host_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame host not null-terminated"))?
        .to_str()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame host not utf-8"))?;
    let addr: SocketAddr = format!("{}:{}", host_str, port)
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid socket address"))?;
    Ok(UdpPacket { payload, addr })
}

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
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    #[test]
    fn framing_round_trip() {
        let (mut a, mut b) = UnixStream::pair().unwrap();

        let payload = b"hello world";
        let _addr: std::net::SocketAddr = "100.64.0.2:4443".parse().unwrap();

        // Write
        let mut frame = Vec::new();
        frame.extend_from_slice(&((4 + 4 + payload.len() + 2 + 9 + 1) as u32).to_be_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&4443u16.to_be_bytes());
        frame.extend_from_slice(b"100.64.0.2");
        frame.push(0);
        a.write_all(&frame).unwrap();
        a.flush().unwrap();

        // Read
        let mut header = [0u8; 8];
        b.read_exact(&mut header).unwrap();
        let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
        let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
        let mut rest = vec![0u8; total_len - 8];
        b.read_exact(&mut rest).unwrap();

        assert_eq!(&rest[..payload_len], payload);
    }

    #[test]
    fn encode_then_parse() {
        let payload = b"test datagram";
        let addr: SocketAddr = "192.168.1.1:1234".parse().unwrap();

        let frame = encode_frame(payload, &addr);

        // Parse the header
        let total_len = u32::from_be_bytes(frame[0..4].try_into().unwrap()) as usize;
        let payload_len = u32::from_be_bytes(frame[4..8].try_into().unwrap()) as usize;
        assert_eq!(total_len, frame.len());

        let rest = &frame[8..];
        let pkt = parse_frame_rest(rest, payload_len).unwrap();
        assert_eq!(pkt.payload, payload);
        assert_eq!(pkt.addr, addr);
    }
}
