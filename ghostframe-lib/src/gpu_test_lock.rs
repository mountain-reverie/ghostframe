//! Test-only: one lock for everything in this test binary that drives the GPU.
//!
//! The Vulkan capture tests (`GpuFrameProcessor`) and the VA-API encoder tests
//! used to run in parallel threads on the one card; a VCE fault during such a
//! run took the host down. Holding this lock around each processor call
//! (`new`, `process_frame*`, drop) and for the whole body of an encoder test
//! makes them mutually exclusive inside one binary. It is never held for a
//! processor's lifetime: tests move processors between threads and tasks, and
//! a lifetime guard deadlocked the multi-thread tokio tests.
//!
//! Reentrant per thread, because a test may hold a processor and create a
//! second one (a reference) or take the encoder lock under it.

use std::sync::{Condvar, Mutex};
use std::thread::ThreadId;

struct State {
    owner: Option<ThreadId>,
    depth: usize,
}

static STATE: Mutex<State> = Mutex::new(State {
    owner: None,
    depth: 0,
});
static FREED: Condvar = Condvar::new();

/// Held while this thread is allowed to use the GPU. May be dropped on a
/// different thread than the one that took it (a processor can be moved).
pub(crate) struct GpuTestGuard(());

pub(crate) fn lock() -> GpuTestGuard {
    let me = std::thread::current().id();
    let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
    // Poisoning is ignored: one failed GPU test must not fail the rest.
    while st.owner.is_some_and(|o| o != me) {
        st = FREED.wait(st).unwrap_or_else(|p| p.into_inner());
    }
    st.owner = Some(me);
    st.depth += 1;
    GpuTestGuard(())
}

impl Drop for GpuTestGuard {
    fn drop(&mut self) {
        let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
        st.depth -= 1;
        if st.depth == 0 {
            st.owner = None;
            FREED.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reentrant_on_one_thread_exclusive_across_threads() {
        let a = lock();
        let b = lock(); // would deadlock if not reentrant
        let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s2 = started.clone();
        let h = std::thread::spawn(move || {
            let _g = lock();
            s2.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !started.load(std::sync::atomic::Ordering::SeqCst),
            "another thread got the GPU while this one held it"
        );
        drop(b);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !started.load(std::sync::atomic::Ordering::SeqCst),
            "released after the inner guard only"
        );
        drop(a);
        h.join().unwrap();
        assert!(started.load(std::sync::atomic::Ordering::SeqCst));
    }
}
