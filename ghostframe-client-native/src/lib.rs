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
    /// [`Client::debug_map_frame`] failed: not connected, an unknown
    /// `buffer_id`, the render thread did not reply within its timeout, or
    /// the dmabuf mmap itself failed.
    #[error("debug_map_frame: {0}")]
    DebugMap(String),
    /// [`Client::cdf53_coverage`] failed: not connected, or the net thread
    /// did not reply within its timeout.
    #[error("cdf53_coverage: {0}")]
    Cdf53Coverage(String),
}

/// Configuration for a [`Client`]. Mirrors `gf_client_config` in the C API
/// design (`docs/superpowers/specs/2026-09-22-native-client-design.md`, ยง4).
pub struct Config {
    /// This client's own tsnet node name.
    pub hostname: String,
    pub authkey: String,
    pub state_dir: std::path::PathBuf,
    /// A *request*, not an assertion: whether the host wants H.264 decode
    /// if this machine can do it. [`Client::supports_h264`] is the answer,
    /// ANDing this with a VA-API probe run once in [`Client::new`].
    pub supports_h264: bool,
    pub indices_raw: bool,
    /// The export ring's buffer count. `0` means "use the render thread's
    /// default" (currently 3) -- see `render_thread::DEFAULT_EXPORT_BUFFER_COUNT`.
    /// A nonzero value is passed straight to `Renderer::new`, which rejects
    /// `0` itself (`GpuError::NoExportBuffers`) since a ring with no
    /// buffers could never publish a frame.
    pub n_export_buffers: u32,
    /// DRM format modifiers the consumer prefers, in priority order. Empty
    /// means "linear only" (`DRM_FORMAT_MOD_LINEAR`). Passed straight to
    /// `ExportRing::new`.
    pub preferred_modifiers: Vec<u64>,
    /// Allocate export buffers in CPU-mappable memory so
    /// [`Client::debug_map_frame`] can read pixels back.
    ///
    /// **Leave this `false` in production.** A consumer imports the dmabuf
    /// into its own GPU and never touches it with the CPU, and demanding
    /// host visibility can pin every export to a small BAR aperture -- on
    /// the development hardware that is 256 MiB, against 7.75 GiB of
    /// CPU-invisible VRAM. It also risks tests and production landing in
    /// different memory types, which is how a bug becomes unreproducible.
    ///
    /// `debug_map_frame` returns an error when this is `false`.
    pub debug_map_frames: bool,
}

/// A published frame's exported dmabuf, mapped and copied out as plain
/// bytes. Test and diagnostic use only -- see [`Client::debug_map_frame`].
pub struct DebugFrameBytes {
    pub bytes: Vec<u8>,
    pub stride: u64,
    pub offset: u64,
    pub width: u32,
    pub height: u32,
}

/// Snapshot of the net thread's CDF 5/3 tile-coverage state. Test and
/// diagnostic use only -- see [`Client::cdf53_coverage`]. Not part of the C
/// ABI: `ghostframe-client-capi` does not (yet) expose this.
pub struct Cdf53Coverage {
    pub summary: ghostframe_client_core::cdf53_coverage::Cdf53CoverageSummary,
    /// The most-stalled incomplete tiles, capped at
    /// `CDF53_INCOMPLETE_TILES_LIMIT`. Each entry is
    /// `(tile_x, tile_y, received_mask, present_mask, sweep_attempts)`.
    pub incomplete: Vec<(u8, u8, u16, u16, u8)>,
}

/// Cap on how many incomplete tiles [`Client::cdf53_coverage`] returns, so a
/// fully stalled production-scale screen (2000+ tiles) can't turn one
/// diagnostic call into an unbounded reply.
const CDF53_INCOMPLETE_TILES_LIMIT: usize = 20;

/// How long [`Client::cdf53_coverage`] waits for the net thread to reply
/// before giving up. Mirrors [`DEBUG_MAP_FRAME_TIMEOUT`]'s reasoning: a
/// wedged net thread should fail loudly, not hang the caller.
const CDF53_COVERAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long [`Client::debug_map_frame`] waits for the render thread to
/// reply before giving up. A wedged render thread (GPU hang, deadlock)
/// should fail the caller with a clear message rather than hang forever.
const DEBUG_MAP_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    bridge: Option<std::sync::Arc<GhostbridgeHandle>>,
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
    /// The capability actually advertised in HELLO: what the host asked for,
    /// AND what the machine can do. Computed once in `new`, because HELLO is
    /// built at connect and never revised.
    effective_h264: bool,
}

impl Client {
    /// Allocate the event queue. Opens no GPU device and no tailnet
    /// session -- see the struct doc.
    pub fn new(config: Config) -> Result<Self, ClientError> {
        let queue = Arc::new(EventQueue::new()?);
        let effective_h264 = config.supports_h264 && {
            let probed = ghostframe_client_h264::vaapi_h264_decode_available();
            if config.supports_h264 && !probed {
                tracing::info!(
                    "H.264 was requested but VA-API decode is unavailable here; \
                     advertising tile codecs only"
                );
            }
            probed
        };
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
            effective_h264,
        })
    }

    /// The H.264 capability this client actually advertises.
    ///
    /// `Config::supports_h264` is a *permission*, not an assertion: a host
    /// may ask for H.264 on a machine that cannot decode it, and gets a
    /// working session on the tile codecs rather than a failure.
    pub fn supports_h264(&self) -> bool {
        self.effective_h264
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
    /// forwarded to the render thread, which passes them to
    /// `Renderer::new`/`ExportRing::new` at first-frame construction time
    /// (renderer construction is lazy -- see `render_thread::handle_core_event`).
    /// Use an existing tsnet node instead of creating one in `connect`.
    ///
    /// A host application that is already on the tailnet -- because it runs
    /// its own tsnet node, or because it embeds several ghostframe clients --
    /// should not be forced to stand up a second one. Call this before
    /// `connect` and the client dials through the node you supply; its
    /// lifetime is then yours, not ours.
    ///
    /// This is also what the e2e harness needs: it already owns a tsnet node
    /// for its forwarders, and running a second `tsnet.Server` in the same
    /// process was observed not to converge a working peer datapath even
    /// though both nodes logged in to the control plane successfully.
    pub fn attach_bridge(&mut self, bridge: std::sync::Arc<GhostbridgeHandle>) {
        self.bridge = Some(bridge);
    }

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

        // `TS_CONTROL_URL` mirrors `ghostframe-xdaemon`'s own
        // `env::var("TS_CONTROL_URL").unwrap_or_default()` (see
        // `ghostframe-xdaemon/src/main.rs`): unset means "join the real
        // Tailscale network" (ghostbridge's `gbridge_new` treats an empty
        // string as "use tsnet's default `ControlURL`"), set means "join
        // this custom control plane instead" -- e.g. the e2e harness's
        // headscale. There is deliberately no `Config` field for this: a
        // production embedder always wants the real tailnet, and the one
        // consumer that doesn't (this crate's own e2e test) is exactly the
        // kind of test-only override an env var is for.
        let bridge = match self.bridge.clone() {
            // Supplied by the embedder via `attach_bridge`; already up.
            Some(existing) => existing,
            None => {
                let control_url = std::env::var("TS_CONTROL_URL").unwrap_or_default();
                let b = std::sync::Arc::new(GhostbridgeHandle::connect(&GhostbridgeConfig {
                    hostname: self.config.hostname.clone(),
                    authkey: self.config.authkey.clone(),
                    state_dir: self.config.state_dir.to_string_lossy().into_owned(),
                    control_url,
                })?);
                b.up()?;
                b
            }
        };

        let cert_hash = bootstrap::fetch_cert_hash(&bridge, host)?;

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
            supports_h264: self.effective_h264,
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
        let export_buffers = self.config.n_export_buffers;
        let preferred_modifiers = self.config.preferred_modifiers.clone();
        let host_visible = self.config.debug_map_frames;
        let render_handle = std::thread::Builder::new()
            .name("gf-render".into())
            .spawn(move || {
                render_thread::run(
                    ctx,
                    render_rx,
                    render_queue,
                    published,
                    export_buffers,
                    host_visible,
                    preferred_modifiers,
                )
            })
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

    /// Map a published frame's dmabuf and copy its bytes out, together with
    /// the plane stride and offset needed to index them.
    ///
    /// Test and diagnostic use only. Round-trips a request to the render
    /// thread, which owns the `Renderer`; `ExportedImage::map_read` is a CPU
    /// mmap with explicit DMA_BUF_IOCTL_SYNC rather than a second Vulkan
    /// import, because cross-device PRIME import returns stale bytes on this
    /// hardware and previously read as a decode bug.
    pub fn debug_map_frame(&self, frame: &PublishedFrame) -> Result<DebugFrameBytes, ClientError> {
        // Fail with the reason rather than with a bare mmap EPERM from deep
        // inside the render thread. Without host-visible export memory the
        // buffer simply cannot be mapped, and that errno points nowhere
        // near the missing config flag.
        if !self.config.debug_map_frames {
            return Err(ClientError::DebugMap(
                "Config::debug_map_frames is false, so export buffers were allocated in \
                 device-local memory the CPU cannot map; set it to true for diagnostics"
                    .into(),
            ));
        }
        let tx = self
            .render_tx
            .as_ref()
            .ok_or_else(|| ClientError::DebugMap("not connected".into()))?;
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(RenderMsg::DebugMapFrame(frame.buffer_id, reply_tx))
            .map_err(|_| ClientError::DebugMap("render thread is not running".into()))?;
        reply_rx
            .recv_timeout(DEBUG_MAP_FRAME_TIMEOUT)
            .map_err(|_| {
                ClientError::DebugMap(format!(
                    "render thread did not reply within {DEBUG_MAP_FRAME_TIMEOUT:?} \
                     (wedged render thread?)"
                ))
            })?
            .map_err(ClientError::DebugMap)
    }

    /// Snapshot the net thread's CDF 5/3 tile-coverage state.
    ///
    /// Diagnostic API only -- not part of the C ABI. `ClientCore` (and the
    /// `ClientNet` that owns it) lives entirely on the net thread, so this
    /// round-trips a request through the same wake-eventfd + channel pair
    /// `push_*` uses to send input, with an `mpsc::Sender` reply mirroring
    /// [`Client::debug_map_frame`]'s round-trip to the render thread. A
    /// bounded timeout means a wedged net thread fails loudly rather than
    /// hanging the caller.
    pub fn cdf53_coverage(&self) -> Result<Cdf53Coverage, ClientError> {
        let tx = self
            .net_cmd_tx
            .as_ref()
            .ok_or_else(|| ClientError::Cdf53Coverage("not connected".into()))?;
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(NetCommand::Cdf53Coverage(reply_tx))
            .map_err(|_| ClientError::Cdf53Coverage("net thread is not running".into()))?;
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
        reply_rx.recv_timeout(CDF53_COVERAGE_TIMEOUT).map_err(|_| {
            ClientError::Cdf53Coverage(format!(
                "net thread did not reply within {CDF53_COVERAGE_TIMEOUT:?} (wedged net thread?)"
            ))
        })
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
