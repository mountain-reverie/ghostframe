//! The net thread: an epoll loop over the framed tsnet UDP fd, a timerfd
//! armed from `ClientNet::poll_timeout`, and a wake eventfd for shutdown
//! and outbound input.
//!
//! This owns `ClientNet` and therefore every bit of ACK/NACK timing. It is
//! a separate thread from the renderer so a GPU stall on that side (the
//! `poll(wait_indefinitely)` inside `ExportRing::publish`) can never delay
//! a transport deadline -- see the module doc on `render_thread` for the
//! other half of that argument.
//!
//! There is no raw-socket path here: the only fd this thread ever touches
//! is the one `GhostbridgeHandle::dial_udp` handed back, and every packet
//! that crosses it is framed exactly as `ghostframe_tsnet::encode_frame` /
//! `parse_frame_rest` describe. Treating it as a plain UDP socket -- e.g.
//! skipping the 8-byte header -- silently delivers garbage; see the type
//! doc on `FrameReader` for how reads are kept correct across a
//! non-blocking fd that may hand back a frame in more than one piece.

use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ghostframe_client_net::{ClientNet, ClientNetEvent};
use ghostframe_tsnet::{parse_frame_rest, UdpPacket, MAX_FRAME_LEN};

use crate::event::{ClientEvent, EventQueue};
use crate::render_thread::RenderMsg;

/// Commands the owning `Client` sends to the net thread through the wake
/// eventfd + this channel pair.
pub(crate) enum NetCommand {
    /// An encoded input event (see `crate::input`), to be written onto the
    /// feedback stream via `ClientNet::send_input`.
    SendInput(Vec<u8>),
    /// Diagnostic: snapshot `ClientNet`'s CDF 5/3 coverage state and reply
    /// with it. Mirrors `RenderMsg::DebugMapFrame`'s round-trip shape -- see
    /// `Client::cdf53_coverage`.
    Cdf53Coverage(std::sync::mpsc::Sender<crate::Cdf53Coverage>),
    Shutdown,
}

/// Accumulates bytes read from the non-blocking tsnet fd across however
/// many `read(2)` calls it takes, and peels off complete frames.
///
/// A single frame can be up to `MAX_FRAME_LEN` (72 KiB); nothing guarantees
/// the AF_UNIX socketpair buffers that much before this thread's next
/// `epoll_wait` wakes it; and Reads on a stream never respect the writer's
/// message boundaries. So a naive "read exactly 8 bytes, then read exactly
/// `total_len - 8` more" would stall on the second read if only part of a
/// large frame has arrived. This type keeps whatever partial tail is left
/// over between calls instead of assuming a whole frame is always ready.
struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Drain everything currently available (non-blocking) off `fd`, then
    /// return every complete frame the accumulated buffer now contains.
    /// `Ok(vec![])` with nothing pulled off the fd (`WouldBlock` on the
    /// very first read) is the normal "nothing to do" case.
    fn poll(&mut self, fd: RawFd) -> std::io::Result<Vec<UdpPacket>> {
        let mut tmp = [0u8; 65536];
        loop {
            // SAFETY: `fd` is a valid, open, non-blocking fd for the
            // lifetime of this call; `tmp` is a valid buffer of the given
            // length.
            let n = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
            if n > 0 {
                self.buf.extend_from_slice(&tmp[..n as usize]);
                continue;
            }
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "tsnet udp fd closed",
                ));
            }
            let e = std::io::Error::last_os_error();
            match e.kind() {
                std::io::ErrorKind::WouldBlock => break,
                std::io::ErrorKind::Interrupted => continue,
                _ => return Err(e),
            }
        }

        let mut packets = Vec::new();
        loop {
            if self.buf.len() < 8 {
                break;
            }
            let total_len = u32::from_be_bytes(self.buf[0..4].try_into().unwrap()) as usize;
            let payload_len = u32::from_be_bytes(self.buf[4..8].try_into().unwrap()) as usize;
            if !(8..=MAX_FRAME_LEN).contains(&total_len) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("frame total_len {total_len} out of range"),
                ));
            }
            if self.buf.len() < total_len {
                break; // rest of the frame hasn't arrived yet
            }
            let rest = &self.buf[8..total_len];
            let pkt = parse_frame_rest(rest, payload_len)?;
            packets.push(pkt);
            self.buf.drain(..total_len);
        }
        Ok(packets)
    }
}

/// Write one framed packet to the tsnet fd, looping past `WouldBlock` with
/// a blocking `poll(2)` for `POLLOUT`. `dial_udp`'s destination is fixed at
/// dial time and ghostbridge ignores the address on every write (see
/// `dialedPacketConn::WriteTo` in `ghostbridge/main.go`), but `encode_frame`
/// still needs *a* `SocketAddr` to serialize -- passing the real
/// destination costs nothing and keeps this call site honest about what
/// it's sending to.
fn write_frame(fd: RawFd, payload: &[u8], destination: &SocketAddr) -> std::io::Result<()> {
    let frame = ghostframe_tsnet::encode_frame(payload, destination);
    let mut off = 0;
    while off < frame.len() {
        // SAFETY: `fd` is valid and open; the slice is valid for its length.
        let n = unsafe {
            libc::write(
                fd,
                frame[off..].as_ptr() as *const libc::c_void,
                frame.len() - off,
            )
        };
        if n > 0 {
            off += n as usize;
            continue;
        }
        let e = std::io::Error::last_os_error();
        match e.kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock => {
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: single-element array, valid for the call.
                unsafe {
                    libc::poll(&mut pfd, 1, -1);
                }
                continue;
            }
            _ => return Err(e),
        }
    }
    Ok(())
}

fn epoll_add(epfd: RawFd, fd: RawFd) -> std::io::Result<()> {
    let mut ev = libc::epoll_event {
        events: libc::EPOLLIN as u32,
        u64: fd as u64,
    };
    // SAFETY: epfd/fd are valid open fds; `ev` is a valid pointer for the
    // duration of the call.
    let rc = unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, &mut ev) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// (Re)arm `timer_fd` for `deadline_us` (microseconds since `base`), or
/// disarm it (all-zero `itimerspec`) when `None`. A deadline already in the
/// past is armed for the smallest representable non-zero delay rather than
/// zero, since an all-zero `it_value` is how `timerfd_settime` spells
/// "disarmed" -- passing an overdue deadline through unmodified would
/// silently cancel the timer instead of firing it immediately.
fn arm_timer(timer_fd: RawFd, deadline_us: Option<u64>, base: Instant) {
    let it_value = match deadline_us {
        None => libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        Some(deadline_us) => {
            let now = Instant::now().duration_since(base);
            let deadline = Duration::from_micros(deadline_us);
            let mut remaining = deadline.saturating_sub(now);
            if remaining.is_zero() {
                remaining = Duration::from_nanos(1);
            }
            libc::timespec {
                tv_sec: remaining.as_secs() as libc::time_t,
                tv_nsec: remaining.subsec_nanos() as i64,
            }
        }
    };
    let new_value = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value,
    };
    // SAFETY: `timer_fd` is a valid open timerfd; `new_value` is a valid
    // pointer, `NULL` for the unused old-value out-param is documented as
    // acceptable by `timerfd_settime(2)`.
    unsafe {
        libc::timerfd_settime(timer_fd, 0, &new_value, std::ptr::null_mut());
    }
}

fn drain_eventfd(fd: RawFd) {
    let mut buf: u64 = 0;
    // SAFETY: fd is a valid open eventfd; buf is 8 valid writable bytes.
    // Nonblocking EAGAIN (nothing to drain) is fine and ignored.
    unsafe {
        let _ = libc::read(fd, &mut buf as *mut u64 as *mut libc::c_void, 8);
    }
}

/// Core events that the host embedder needs to see, as `ClientEvent`s.
/// `None` for events that only concern the render thread.
///
/// Extracted from the thread loop so it can be tested directly: the loop
/// itself needs a live network client and a render thread to run at all.
///
/// Variants are enumerated explicitly rather than via a `_ =>` catch-all.
/// `ghostframe-client-core` does not itself enable
/// `clippy::wildcard_enum_match_arm` for consumers, but this repo lost
/// three months to a load-bearing wildcard arm once (see
/// `feedback_load_bearing_catch_all.md`); listing every variant here means
/// adding a new one to `Event` is a compile error at this site too, forcing
/// a deliberate decision about whether the host needs to see it.
pub(crate) fn host_event_for(core_ev: &ghostframe_client_core::Event) -> Option<ClientEvent> {
    use ghostframe_client_core::Event;
    match core_ev {
        Event::FrameDimensions { width, height } => Some(ClientEvent::Resized {
            width: *width,
            height: *height,
        }),
        // `ClientEvent::Disconnected` already exists and already carries a
        // reason string; eviction is a disconnect with a known cause, not a
        // new kind of host event.
        Event::Evicted { reason } => Some(ClientEvent::Disconnected {
            reason: format!("{reason:?}"),
        }),
        Event::TileReady { .. }
        | Event::NeedsH264 { .. }
        | Event::DecodeError { .. }
        | Event::TilePayload { .. }
        | Event::PaletteUpdated { .. } => None,
    }
}

/// Arguments bundled to keep `run`'s signature from growing every time a
/// new piece of shared state is needed.
pub(crate) struct NetThreadArgs {
    pub client_net: ClientNet,
    pub udp_fd: OwnedFd,
    pub wake_fd: Arc<OwnedFd>,
    pub cmd_rx: Receiver<NetCommand>,
    pub render_tx: std::sync::mpsc::Sender<RenderMsg>,
    pub queue: Arc<EventQueue>,
    pub base: Instant,
    /// The single peer address quinn-proto was told about in
    /// `ClientNet::connect`. Every inbound datagram is attributed to this,
    /// NOT to the address ghostbridge reports per frame: the socketpair is
    /// point-to-point and ghostbridge's dialedPacketConn ignores the
    /// destination on send, so the address is a label. Feeding quinn a
    /// different source than it was told to expect reads as path migration.
    pub(crate) peer_addr: std::net::SocketAddr,
}

pub(crate) fn run(args: NetThreadArgs) {
    let NetThreadArgs {
        mut client_net,
        udp_fd,
        wake_fd,
        cmd_rx,
        render_tx,
        queue,
        base,
        peer_addr,
    } = args;

    let udp_raw = udp_fd.as_raw_fd();
    let wake_raw = wake_fd.as_raw_fd();

    // SAFETY: epoll_create1 with no flags beyond CLOEXEC is a documented,
    // checked call.
    let epfd_raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epfd_raw < 0 {
        tracing::error!(error = %std::io::Error::last_os_error(), "epoll_create1 failed");
        return;
    }
    // SAFETY: epfd_raw was just created and is owned by nothing else.
    let epfd = unsafe { OwnedFd::from_raw_fd(epfd_raw) };

    // SAFETY: timerfd_create with a documented clock id and flag set.
    let timer_raw = unsafe {
        libc::timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
        )
    };
    if timer_raw < 0 {
        tracing::error!(error = %std::io::Error::last_os_error(), "timerfd_create failed");
        return;
    }
    // SAFETY: timer_raw was just created and is owned by nothing else.
    let timer_fd = unsafe { OwnedFd::from_raw_fd(timer_raw) };

    if epoll_add(epfd.as_raw_fd(), udp_raw).is_err()
        || epoll_add(epfd.as_raw_fd(), wake_raw).is_err()
        || epoll_add(epfd.as_raw_fd(), timer_fd.as_raw_fd()).is_err()
    {
        tracing::error!(error = %std::io::Error::last_os_error(), "epoll_ctl(ADD) failed");
        return;
    }

    arm_timer(timer_fd.as_raw_fd(), client_net.poll_timeout(), base);

    let mut reader = FrameReader::new();
    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 8];

    'outer: loop {
        // SAFETY: `events` is a valid buffer of the given capacity;
        // `epfd` is a valid open epoll fd. A wait timeout of -1 is
        // documented as "block until an event or a signal".
        let n = unsafe {
            libc::epoll_wait(
                epfd.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as i32,
                -1,
            )
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            tracing::error!(error = %e, "epoll_wait failed");
            break;
        }

        for ev in &events[..n as usize] {
            let fd = ev.u64 as RawFd;
            if fd == udp_raw {
                match reader.poll(udp_raw) {
                    Ok(packets) => {
                        for pkt in packets {
                            let now_us = Instant::now().duration_since(base).as_micros() as u64;
                            // Deliberately `peer_addr`, not `pkt.addr` -- see the field doc.
                            client_net.handle_udp(&pkt.payload, peer_addr, now_us);
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "tsnet udp fd read failed; net thread exiting");
                        break 'outer;
                    }
                }
            } else if fd == timer_fd.as_raw_fd() {
                let mut buf: u64 = 0;
                // SAFETY: valid open timerfd; buf is 8 valid writable bytes.
                unsafe {
                    let _ = libc::read(fd, &mut buf as *mut u64 as *mut libc::c_void, 8);
                }
                let now_us = Instant::now().duration_since(base).as_micros() as u64;
                client_net.on_timeout(now_us);
            } else if fd == wake_raw {
                drain_eventfd(wake_raw);
                loop {
                    match cmd_rx.try_recv() {
                        Ok(NetCommand::SendInput(bytes)) => {
                            let now_us = Instant::now().duration_since(base).as_micros() as u64;
                            client_net.send_input(bytes, now_us);
                        }
                        Ok(NetCommand::Cdf53Coverage(reply_tx)) => {
                            let summary = client_net.cdf53_coverage_summary();
                            let incomplete = client_net
                                .cdf53_incomplete_tiles(crate::CDF53_INCOMPLETE_TILES_LIMIT);
                            let _ = reply_tx.send(crate::Cdf53Coverage {
                                summary,
                                incomplete,
                            });
                        }
                        Ok(NetCommand::Shutdown) => break 'outer,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => break 'outer,
                    }
                }
            }
        }

        for ev in client_net.take_events() {
            match ev {
                ClientNetEvent::Connected => {
                    // QUIC handshake done, but the WebTransport session is
                    // not yet accepted -- nothing host-visible yet.
                }
                ClientNetEvent::SessionReady => {
                    queue.push(ClientEvent::Connected);
                }
                ClientNetEvent::ConnectionLost { reason } => {
                    queue.push(ClientEvent::Disconnected { reason });
                }
                ClientNetEvent::Core(core_ev) => {
                    if let Some(host_ev) = host_event_for(&core_ev) {
                        queue.push(host_ev);
                    }
                    if render_tx.send(RenderMsg::Core(core_ev)).is_err() {
                        // Render thread is gone; nothing more this thread
                        // can usefully do with decoded events, but ACK/NACK
                        // timing must keep running until told to stop.
                        tracing::warn!("render thread channel closed; dropping core event");
                    }
                }
            }
        }

        while let Some(out) = client_net.poll_transmit() {
            if let Err(e) = write_frame(udp_raw, &out.payload, &out.destination) {
                tracing::warn!(error = %e, "udp frame write failed");
            }
        }

        arm_timer(timer_fd.as_raw_fd(), client_net.poll_timeout(), base);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostframe_client_core::Event;
    use ghostframe_protocol::eviction::EvictionReason;

    #[test]
    fn eviction_becomes_a_disconnect_naming_the_cause() {
        let ev = host_event_for(&Event::Evicted {
            reason: EvictionReason::DisplacedByNewSession,
        });
        match ev {
            Some(ClientEvent::Disconnected { reason }) => assert!(
                reason.contains("DisplacedByNewSession"),
                "the reason must name the cause so an embedder can tell \
                 displacement from a network failure, got {reason:?}"
            ),
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    #[test]
    fn frame_dimensions_still_maps_to_resized() {
        // Regression guard on the extraction itself.
        assert_eq!(
            host_event_for(&Event::FrameDimensions {
                width: 800,
                height: 600
            }),
            Some(ClientEvent::Resized {
                width: 800,
                height: 600
            })
        );
    }

    #[test]
    fn unknown_eviction_reason_still_names_the_byte() {
        // Task 2's `EvictionReason::Unknown` carries the raw byte so an
        // operator debugging a version skew gets the number, not just
        // "unknown" -- confirm that byte actually reaches the embedder's
        // disconnect reason string.
        let ev = host_event_for(&Event::Evicted {
            reason: EvictionReason::Unknown(0x7A),
        });
        match ev {
            Some(ClientEvent::Disconnected { reason }) => assert!(
                reason.contains("122") || reason.contains("7A") || reason.contains("7a"),
                "expected the raw byte to be visible in the reason, got {reason:?}"
            ),
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }
}
