//! eventfd-backed event queue.
//!
//! The host application owns its own main loop (a Wayland or X11 event
//! loop, a game loop, whatever). It must be able to add **one file
//! descriptor** to its existing `poll`/`epoll` set and be woken when the
//! library has something to report -- the same idiom libinput, Wayland
//! and PipeWire use. Callbacks from library threads would force every
//! GTK/Qt/compositor embedder to marshal back to its main thread.
//!
//! # Thread-safety contract
//!
//! [`EventQueue::push`] may be called concurrently from any number of
//! threads (the net thread and the render thread both push). [`EventQueue::pop`]
//! is intended to be drained by a single thread (the host's main-loop
//! thread), though nothing here makes concurrent `pop` unsound -- it is
//! simply not the expected usage.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Mutex;

/// Events handed to the host. Mirrors the C `gf_event` in the capi crate.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    Connected,
    Disconnected {
        reason: String,
        /// True when the server deliberately ended this session
        /// (eviction). False for transport failure, idle timeout, or
        /// server shutdown. A host embedder (e.g. the CLI) exits cleanly
        /// only for the former: displacement is an ordinary hand-off, a
        /// dropped link is not, and a caller must be able to tell them
        /// apart from this field alone, not by parsing `reason`.
        expected: bool,
    },
    Resized {
        width: u32,
        height: u32,
    },
    FrameReady {
        frame_id: u32,
    },
    Error {
        message: String,
    },
}

/// A thread-safe queue whose eventfd the host can put in its own poll set.
///
/// ## Ordering: why there is no missed-wakeup window
///
/// The invariant we must maintain is: **whenever the queue is non-empty,
/// the eventfd is readable.** There are two operations that touch both
/// the queue and the fd counter, and each is ordered so the fd update
/// happens strictly after the queue mutation that requires it:
///
/// - `push`: the event is inserted into the `VecDeque` (under the mutex)
///   *before* we write to the eventfd. If a concurrent `pop` were to run
///   between the insert and the `write`, it would simply see a non-empty
///   queue and return the event -- the fd write becomes a harmless extra
///   wakeup (the read side tolerates spurious wakeups by design: a woken
///   poller that finds the queue empty just loops). What must never
///   happen is the reverse order (signal the fd, then insert the event),
///   because a poller woken by the fd could then find the queue *empty*
///   and go back to sleep having "consumed" a wakeup that carried no
///   data -- that is the missed-wakeup bug. Insert-then-signal cannot
///   produce that: by the time the fd is readable, the data is already
///   visible to anyone who acquires the mutex.
///
/// - `pop`: we hold the mutex for the whole operation. We pop the front
///   element, and only while still holding the mutex do we check whether
///   the queue is now empty; if so, we drain the eventfd back to zero.
///   Holding the mutex across both steps prevents a race where another
///   thread's `push` (which also takes the mutex to insert, see above)
///   observes a queue it thinks is non-empty while we are mid-drain: the
///   two critical sections are mutually exclusive, so "queue has N>0
///   items" and "the fd's readability" cannot be observed to disagree by
///   another thread taking the same mutex-protected path. Concretely: if
///   `pop` drains the fd, it does so having just observed the queue is
///   empty *under the lock*; any `push` that added something the `pop`
///   didn't see must have happened either fully before (so `pop` would
///   have dequeued it) or fully after (so it takes the lock after `pop`
///   released it, sees the queue empty, inserts, and re-signals the fd
///   itself). There is no interleaving that leaves the queue non-empty
///   with the fd cleared.
///
/// eventfd semantics used here: the write always adds `1` to the kernel
/// counter (coalescing multiple pending pushes into "readable", not into
/// a precise count -- we don't need a count, `VecDeque::is_empty` is the
/// source of truth). The read drains the *entire* counter back to 0 in
/// one syscall because `EFD_SEMAPHORE` is not set; that's what makes
/// "queue empty implies fd not readable" achievable with a single read.
pub struct EventQueue {
    queue: Mutex<VecDeque<ClientEvent>>,
    fd: OwnedFd,
}

impl EventQueue {
    /// Create a new queue with a fresh eventfd.
    ///
    /// The eventfd is created with `EFD_NONBLOCK | EFD_CLOEXEC` and
    /// **without** `EFD_SEMAPHORE`:
    ///
    /// - `EFD_CLOEXEC`: this library is linked into arbitrary host
    ///   applications. A leaked fd across `exec` is a real bug in that
    ///   setting -- e.g. a host that forks a helper process would
    ///   otherwise hand it a live handle into this queue's wakeup
    ///   channel for no reason.
    /// - `EFD_NONBLOCK`: the host polls this fd; reads and writes must
    ///   never block the calling thread.
    /// - No `EFD_SEMAPHORE`: with it, a single `read` decrements the
    ///   counter by one instead of draining it to zero, so an fd that
    ///   had N events queued stays readable after a `pop` has drained
    ///   the whole `VecDeque`. That desyncs the fd's readability from
    ///   `queue.is_empty()` and makes the host's main loop spin at 100%
    ///   CPU (readable fd, but `pop()` returns `None`).
    pub fn new() -> io::Result<Self> {
        // SAFETY: eventfd(2) with a valid initial value and only
        // documented flags; the return value is checked below.
        let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a valid, just-created, otherwise-unowned fd.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Self {
            queue: Mutex::new(VecDeque::new()),
            fd,
        })
    }

    /// Push an event onto the queue and signal the eventfd.
    ///
    /// Safe to call concurrently from multiple threads (e.g. the net
    /// thread and the render thread).
    pub fn push(&self, ev: ClientEvent) {
        {
            let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.push_back(ev);
        }
        // Signal *after* the event is visible in the queue -- see the
        // ordering argument on the struct doc above.
        let one: u64 = 1;
        // SAFETY: `self.fd` is a valid, open eventfd; `&one` points to
        // 8 valid bytes for the duration of the call. EAGAIN (the
        // counter is already at its near-max value) is not a
        // correctness issue for us: the fd is already readable, which
        // is the only property we depend on.
        unsafe {
            let _ = libc::write(
                self.fd.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                std::mem::size_of::<u64>(),
            );
        }
    }

    /// Pop the oldest event off the queue, if any.
    ///
    /// Intended to be called from a single thread (the host's main
    /// loop). See the struct doc for why this keeps the fd's
    /// readability in sync with `queue.is_empty()`.
    pub fn pop(&self) -> Option<ClientEvent> {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let ev = q.pop_front();
        if q.is_empty() {
            // Drain the counter back to 0 while still holding the lock,
            // so no concurrent push can be "lost" from the fd's point
            // of view -- see the ordering argument above.
            let mut buf: u64 = 0;
            // SAFETY: `self.fd` is a valid, open, non-blocking eventfd;
            // `&mut buf` points to 8 valid, writable bytes. A failure
            // (EAGAIN, meaning the counter was already 0) is expected
            // and harmless -- it just means there was nothing to drain.
            unsafe {
                let _ = libc::read(
                    self.fd.as_raw_fd(),
                    &mut buf as *mut u64 as *mut libc::c_void,
                    std::mem::size_of::<u64>(),
                );
            }
        }
        ev
    }

    /// The raw eventfd. The host adds this to its own poll/epoll set
    /// with `POLLIN`/`EPOLLIN`; it is readable exactly while the queue
    /// is non-empty.
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}
