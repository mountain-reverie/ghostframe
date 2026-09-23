//! Native client library for `ghostframe-client-capi`.
//!
//! Owns the tsnet transport, a net thread and a render thread, and exposes
//! an idiomatic Rust [`Client`] API that `ghostframe-client-capi` wraps in
//! a C ABI. See `docs/superpowers/specs/2026-09-22-native-client-design.md`
//! for the full design.
//!
//! - [`bootstrap`]: fetches and pins the server's WebTransport cert hash
//!   over `/config.json`.
//! - [`event`]: an eventfd-backed queue the host adds to its own
//!   `poll`/`epoll` set to learn when the library has something to
//!   report.
//! - [`input`]: thin wrappers over `ghostframe_client_core::input` for
//!   encoding input events onto the wire.
//! - [`net_thread`] / [`render_thread`]: the two library threads. See
//!   their module docs for why the split is deliberate, not incidental.

pub mod bootstrap;
pub mod event;
pub mod input;
mod net_thread;
mod render_thread;

use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub use ghostframe_client_gpu::ring::PublishedFrame;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_client_net::{ClientNet, ClientNetConfig};
use ghostframe_tsnet::{GhostbridgeConfig, GhostbridgeHandle};

pub use event::ClientEvent;
use event::EventQueue;
use net_thread::{NetCommand, NetThreadArgs};
use render_thread::RenderMsg;

/// Errors surfaced by this crate's public API.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// A ghostbridge/tsnet FFI call failed (connect, dial, up, ...).
    #[error("tailnet: {0}")]
    Bridge(#[from] ghostframe_tsnet::GhostbridgeError),
    /// A local I/O failure: fd setup, read/write on the tsnet socketpair,
    /// eventfd/epoll/timerfd syscalls.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The `/config.json` cert-hash bootstrap request failed or was
    /// malformed.
    #[error("bootstrap: {0}")]
    Bootstrap(String),
    /// The QUIC/WebTransport transport layer failed.
    #[error("transport: {0}")]
    Net(#[from] ghostframe_client_net::ClientNetError),
    /// GPU/renderer construction or operation failed.
    #[error("gpu: {0}")]
    Gpu(#[from] ghostframe_client_gpu::GpuError),
    /// Something about `connect`'s arguments or the client's current state
    /// makes connecting impossible (already connected, unresolvable host).
    #[error("connect: {0}")]
    Connect(String),
}

/// Configuration for a [`Client`]. Mirrors `gf_client_config` in the C API
/// design (`docs/superpowers/specs/2026-09-22-native-client-design.md`, ยง4).
pub struct Config {
    /// This client's own tsnet node name.
    pub hostname: String,
    pub authkey: String,
    pub state_dir: std::path::PathBuf,
    /// False in M1 -- H.264 decode is M3 scope.
    pub supports_h264: bool,
    pub indices_raw: bool,
    /// Reserved for the export ring's buffer count. `Renderer::new`
    /// currently hardcodes its own count and does not yet accept this --
    /// see the note on [`Client::connect`].
    pub n_export_buffers: u32,
    /// Reserved for DRM modifier negotiation. `Renderer::new` currently
    /// always requests `DRM_FORMAT_MOD_LINEAR` via an empty modifier list
    /// and does not yet accept this -- see the note on [`Client::connect`].
    pub preferred_modifiers: Vec<u64>,
}

/// An embeddable ghostframe session: tailnet transport, QUIC/WebTransport,
/// GPU decode and export, driven by two background threads (see
/// [`net_thread`] and [`render_thread`]).
///
/// Nothing here opens a GPU device or a tailnet session until
/// [`Client::connect`] -- [`Client::new`] only allocates the eventfd-backed
/// [`EventQueue`], so constructing and dropping a `Client` is cheap and
/// side-effect-free even with no GPU or network available (an FFI
/// consumer's early-error path will do exactly this).
pub struct Client {
    config: Config,
    queue: Arc<EventQueue>,

    // `Some` only between a successful `connect` and `disconnect`/`Drop`.
    bridge: Option<GhostbridgeHandle>,
    net_thread: Option<std::thread::JoinHandle<()>>,
    render_thread: Option<std::thread::JoinHandle<()>>,
    net_cmd_tx: Option<mpsc::Sender<NetCommand>>,
    render_tx: Option<mpsc::Sender<RenderMsg>>,
    /// Shared with the net thread: writing `1` here wakes its `epoll_wait`
    /// for a queued `NetCommand` or a shutdown request. An `Arc` (rather
    /// than handing the net thread sole ownership) because both this
    /// struct and the net thread need the fd to stay open for as long as
    /// either might use it, and only a write -- never exclusive access --
    /// is needed from this side.
    wake_fd: Option<Arc<OwnedFd>>,
    /// The render thread's latest published frame, if the host hasn't
    /// acquired it yet. A `Mutex` rather than a channel because
    /// `acquire_frame` only ever wants the *latest* frame, not a queue of
    /// every one the render thread produced -- a channel would need its
    /// own draining logic to get the same "just the newest" behaviour.
    published: Arc<Mutex<Option<PublishedFrame>>>,
}

impl Client {
    /// Allocate the event queue. Opens no GPU device and no tailnet
    /// session -- see the struct doc.
    pub fn new(config: Config) -> Result<Self, ClientError> {
        let queue = Arc::new(EventQueue::new()?);
        Ok(Self {
            config,
            queue,
            bridge: None,
            net_thread: None,
            render_thread: None,
            net_cmd_tx: None,
            render_tx: None,
            wake_fd: None,
            published: Arc::new(Mutex::new(None)),
        })
    }

    /// Bring the tailnet up, fetch and pin the server's cert hash, open the
    /// QUIC/WebTransport session, and start the net and render threads.
    ///
    /// `host` may be an IP literal or a MagicDNS name. Resolution happens
    /// inside ghostbridge (`dial_udp`/`dial_tcp` take a string), and the
    /// `SocketAddr` quinn-proto works with never has to be the real one:
    /// `dial_udp` returns a *dialed* socketpair, and ghostbridge's
    /// `dialedPacketConn::WriteTo` ignores the destination argument
    /// entirely (`ghostbridge/main.go`), writing to the connected peer. The
    /// address is therefore a label quinn uses to identify its single peer,
    /// not a destination.
    ///
    /// What DOES matter is that the label is used consistently: every
    /// inbound datagram must be attributed to the same address `connect`
    /// announced, or quinn sees packets arriving from an unexpected source
    /// and treats it as path migration. So the net thread deliberately
    /// ignores the address ghostbridge reports per frame and substitutes
    /// this one.
    ///
    /// `Config::n_export_buffers` and `Config::preferred_modifiers` are
    /// accepted but not yet threaded through to
    /// `Renderer::new`/`ExportRing::new`, which currently hardcode 3
    /// buffers and an empty (LINEAR-only) modifier list respectively. That
    /// is a real gap against the design doc's `gf_client_config`, tracked
    /// here rather than papered over: wiring it through is a
    /// `client-gpu` change, not a `client-native` one.
    pub fn connect(&mut self, host: &str, port: u16) -> Result<(), ClientError> {
        if self.net_thread.is_some() {
            return Err(ClientError::Connect("already connected".into()));
        }

        // The peer label quinn-proto will use. If `host` is an IP literal we
        // keep it so logs and packet captures read naturally; otherwise we
        // use a stable RFC 5737 TEST-NET-1 documentation address, which can
        // never collide with a real route. Either way ghostbridge ignores it
        // on send and the net thread pins it on receive.
        let remote: SocketAddr = format!("{host}:{port}")
            .parse()
            .unwrap_or_else(|_| SocketAddr::from(([192, 0, 2, 1], port)));

        let bridge = GhostbridgeHandle::connect(&GhostbridgeConfig {
            hostname: self.config.hostname.clone(),
            authkey: self.config.authkey.clone(),
            state_dir: self.config.state_dir.to_string_lossy().into_owned(),
            control_url: String::new(),
        })?;
        bridge.up()?;

        let cert_hash = bootstrap::fetch_cert_hash(&bridge, host, port)?;

        let udp_fd = bridge.dial_udp(&format!("{host}:{port}"))?.into_raw_fd();
        // SAFETY: `into_raw_fd` just handed us sole ownership of a freshly
        // returned, open fd.
        let udp_fd = unsafe { OwnedFd::from_raw_fd(udp_fd) };

        // GPU device construction happens here, synchronously on the
        // caller's thread, rather than inside the render thread: a failure
        // (no Vulkan adapter, no dmabuf export support) should surface
        // from `connect` itself rather than as an asynchronous event the
        // caller might not be listening for yet.
        let ctx = WgpuContext::new()?;

        let base = Instant::now();
        let net_config = ClientNetConfig {
            server_name: host.to_string(),
            server_cert_sha256: cert_hash,
            indices_raw_enabled: self.config.indices_raw,
            supports_h264: self.config.supports_h264,
        };
        let mut client_net = ClientNet::new(net_config, 0)?;
        client_net.connect(remote, 0)?;

        // SAFETY: eventfd(2) with documented flags; the return value is
        // checked below.
        let wake_raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if wake_raw < 0 {
            return Err(ClientError::Io(std::io::Error::last_os_error()));
        }
        // SAFETY: `wake_raw` was just created and is owned by nothing else.
        let wake_fd = Arc::new(unsafe { OwnedFd::from_raw_fd(wake_raw) });

        let (net_cmd_tx, net_cmd_rx) = mpsc::channel();
        let (render_tx, render_rx) = mpsc::channel();

        let render_queue = Arc::clone(&self.queue);
        let published = Arc::clone(&self.published);
        let render_handle = std::thread::Builder::new()
            .name("gf-render".into())
            .spawn(move || render_thread::run(ctx, render_rx, render_queue, published))
            .map_err(ClientError::Io)?;

        let net_queue = Arc::clone(&self.queue);
        let net_render_tx = render_tx.clone();
        let net_wake_fd = Arc::clone(&wake_fd);
        let net_handle = std::thread::Builder::new()
            .name("gf-net".into())
            .spawn(move || {
                net_thread::run(NetThreadArgs {
                    client_net,
                    udp_fd,
                    wake_fd: net_wake_fd,
                    cmd_rx: net_cmd_rx,
                    render_tx: net_render_tx,
                    queue: net_queue,
                    base,
                    peer_addr: remote,
                })
            })
            .map_err(ClientError::Io)?;

        self.bridge = Some(bridge);
        self.net_thread = Some(net_handle);
        self.render_thread = Some(render_handle);
        self.net_cmd_tx = Some(net_cmd_tx);
        self.render_tx = Some(render_tx);
        self.wake_fd = Some(wake_fd);

        Ok(())
    }

    /// Tear the session down: stop both threads, close the tsnet session.
    /// Idempotent -- calling it (or dropping the `Client`) when not
    /// connected is a no-op.
    pub fn disconnect(&mut self) -> Result<(), ClientError> {
        self.shutdown_threads();
        self.bridge = None; // GhostbridgeHandle::drop runs gbridge_close
        Ok(())
    }

    fn shutdown_threads(&mut self) {
        if let Some(tx) = self.net_cmd_tx.take() {
            let _ = tx.send(NetCommand::Shutdown);
        }
        if let Some(wake) = &self.wake_fd {
            let one: u64 = 1;
            // SAFETY: `wake` is a valid, open eventfd for as long as this
            // `Arc` (shared with the net thread) is alive; `&one` is 8
            // valid bytes. Failure just means the counter was already
            // saturated, which is harmless -- the fd is already readable.
            unsafe {
                let _ = libc::write(
                    wake.as_raw_fd(),
                    &one as *const u64 as *const libc::c_void,
                    std::mem::size_of::<u64>(),
                );
            }
        }
        if let Some(tx) = self.render_tx.take() {
            let _ = tx.send(RenderMsg::Shutdown);
        }
        if let Some(h) = self.net_thread.take() {
            let _ = h.join();
        }
        if let Some(h) = self.render_thread.take() {
            let _ = h.join();
        }
        self.wake_fd = None;
    }

    /// The eventfd to add to the host's own `poll`/`epoll` set.
    pub fn event_fd(&self) -> RawFd {
        self.queue.as_raw_fd()
    }

    /// Pop the oldest queued event, if any. Never blocks.
    pub fn next_event(&self) -> Option<ClientEvent> {
        self.queue.pop()
    }

    /// The most recently published frame, if the render thread has
    /// produced one since the last call. Never blocks.
    pub fn acquire_frame(&mut self) -> Option<PublishedFrame> {
        self.published
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Return a previously acquired frame's buffer to the render thread's
    /// export ring so it can be reused.
    pub fn release_frame(&mut self, frame_id: u32) {
        if let Some(tx) = &self.render_tx {
            let _ = tx.send(RenderMsg::Release(frame_id));
        }
    }

    fn send_input(&mut self, bytes: Vec<u8>) {
        let Some(tx) = &self.net_cmd_tx else {
            tracing::warn!("push_* called before connect(); dropping input event");
            return;
        };
        if tx.send(NetCommand::SendInput(bytes)).is_err() {
            return;
        }
        if let Some(wake) = &self.wake_fd {
            let one: u64 = 1;
            // SAFETY: see `shutdown_threads`'s identical write.
            unsafe {
                let _ = libc::write(
                    wake.as_raw_fd(),
                    &one as *const u64 as *const libc::c_void,
                    std::mem::size_of::<u64>(),
                );
            }
        }
    }

    pub fn push_key(&mut self, keysym: u32, down: bool) {
        self.send_input(input::encode_key(keysym, down));
    }

    pub fn push_pointer_motion(&mut self, x: i16, y: i16) {
        self.send_input(input::encode_motion(x, y));
    }

    pub fn push_pointer_button(&mut self, x: i16, y: i16, button: u8, down: bool) {
        self.send_input(input::encode_button(x, y, button, down));
    }

    pub fn push_wheel(&mut self, dx: i16, dy: i16) {
        self.send_input(input::encode_wheel(dx, dy));
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // A `Client` dropped while its threads are still running would
        // leave them holding a `queue`/`published` Arc past the point any
        // consumer can observe -- harmless by itself, but the net thread
        // would keep driving a session nobody can interact with, and an
        // FFI consumer expects `gf_client_destroy` to actually stop
        // things. Always tear down cleanly, connected or not (this is a
        // no-op when `net_thread`/`render_thread` are already `None`).
        self.shutdown_threads();
    }
}
