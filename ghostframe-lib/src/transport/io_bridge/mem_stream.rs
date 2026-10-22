//! An in-memory byte pipe standing in for the ghostbridge socketpair in the
//! browserless harness.
//!
//! A kernel socket is read through tokio's reactor, so *when* the bridge
//! learns that bytes have arrived is decided by epoll and by when the runtime
//! happens to park, not by anything the scene controls. This pipe is
//! waker-based and synchronous: bytes pushed by the harness are visible to the
//! very next poll of the bridge, and what the bridge wrote is visible to the
//! harness the moment its turn returns. Nothing here touches a thread, a
//! reactor or a clock.
//!
//! Compiled only under `cfg(test)` / `browserless-harness`.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Default)]
struct Pipe {
    bytes: VecDeque<u8>,
    /// Woken when bytes arrive. Only the reading end ever parks here.
    reader: Option<Waker>,
    closed: bool,
}

/// One end of the pipe. Reads see what the other end wrote; `push` and
/// `take_all` are the synchronous, harness-side view of the same two buffers.
pub struct MemStream {
    incoming: Arc<Mutex<Pipe>>,
    outgoing: Arc<Mutex<Pipe>>,
}

/// A connected pair: what one end writes, the other reads.
pub fn mem_stream_pair() -> (MemStream, MemStream) {
    let a = Arc::new(Mutex::new(Pipe::default()));
    let b = Arc::new(Mutex::new(Pipe::default()));
    (
        MemStream {
            incoming: a.clone(),
            outgoing: b.clone(),
        },
        MemStream {
            incoming: b,
            outgoing: a,
        },
    )
}

impl MemStream {
    /// Append bytes for the other end to read, waking it if it is parked.
    /// Callers write whole frames, so a reader never observes half of one.
    pub fn push(&self, bytes: &[u8]) {
        let mut p = self.outgoing.lock().expect("mem pipe poisoned");
        p.bytes.extend(bytes);
        if let Some(w) = p.reader.take() {
            w.wake();
        }
    }

    /// Remove and return everything the other end has written so far.
    pub fn take_all(&self) -> Vec<u8> {
        let mut p = self.incoming.lock().expect("mem pipe poisoned");
        p.bytes.drain(..).collect()
    }
}

impl Drop for MemStream {
    fn drop(&mut self) {
        let mut p = self.outgoing.lock().expect("mem pipe poisoned");
        p.closed = true;
        if let Some(w) = p.reader.take() {
            w.wake();
        }
    }
}

impl AsyncRead for MemStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut p = self.incoming.lock().expect("mem pipe poisoned");
        if p.bytes.is_empty() {
            if p.closed {
                return Poll::Ready(Ok(()));
            }
            p.reader = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = buf.remaining().min(p.bytes.len());
        let (front, back) = p.bytes.as_slices();
        let from_front = n.min(front.len());
        buf.put_slice(&front[..from_front]);
        buf.put_slice(&back[..n - from_front]);
        p.bytes.drain(..n);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for MemStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.push(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
