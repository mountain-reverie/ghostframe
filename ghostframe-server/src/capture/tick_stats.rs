//! Per-tick cost counters for the capture path (damage-driven encoding PR 0).
//!
//! These are plain integers bumped at the explicit copy and allocation sites
//! of one tick, then read and reset by the capture loop. They are not a global
//! allocator hook: they count the sites the damage-driven plan removes (the
//! full-root `to_vec`, the cursor image, the damage tile set and list), so a
//! later PR that drops one of them shows up as a drop in the counter.

/// What one capture tick copied and allocated at the counted sites.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TickStats {
    /// Bytes memcpy'd or read into freshly allocated buffers by the capture
    /// path: exactly the root grab (every tick) and the cursor image.
    pub bytes_copied: u64,
    /// Heap allocations at the counted sites.
    pub allocs: u32,
}

impl TickStats {
    /// Record one allocation of `bytes` filled by a copy.
    pub fn alloc_copy(&mut self, bytes: usize) {
        self.bytes_copied += bytes as u64;
        self.allocs += 1;
    }

    /// Record one allocation whose bytes are not part of `bytes_copied`.
    pub fn alloc(&mut self) {
        self.allocs += 1;
    }

    /// Fold `other` into `self`.
    pub fn add(&mut self, other: TickStats) {
        self.bytes_copied += other.bytes_copied;
        self.allocs += other.allocs;
    }
}

/// Heap allocations a hash set made reaching `capacity`: its first allocation
/// holds 3 items and each growth roughly doubles that (hashbrown), one
/// allocation per step. 0 for a set that never allocated.
pub fn hashset_allocs(capacity: usize) -> u32 {
    let (mut steps, mut cap) = (0, 0usize);
    while cap < capacity {
        cap = if cap == 0 { 3 } else { cap * 2 + 1 };
        steps += 1;
    }
    steps
}

/// CPU time (user + system) consumed by the calling thread, in microseconds.
/// Only meaningful as a difference between two calls on the same thread.
pub fn thread_cpu_us() -> u64 {
    // SAFETY: `rusage` is plain C data for which all-zero is valid;
    // `getrusage` only writes through the pointer to that local, and
    // RUSAGE_THREAD is Linux-specific (this crate targets Linux).
    let (rc, ru) = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        (libc::getrusage(libc::RUSAGE_THREAD, &mut ru), ru)
    };
    if rc != 0 {
        return 0;
    }
    let tv = |t: libc::timeval| (t.tv_sec as u64) * 1_000_000 + t.tv_usec as u64;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}
