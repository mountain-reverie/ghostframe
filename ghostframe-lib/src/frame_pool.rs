//! Pooled frame buffers: [`FramePool`] and [`FrameBuf`].
//!
//! A capture producer grabs into a slot taken from a fixed-size pool and
//! sends the slot inside a `FrameSubmission`. When the frame ends anywhere
//! (the worker, a dropped frame, a failed submit, a closed channel) the
//! `FrameBuf` is dropped and its slot returns to the pool. The number of
//! slots is the pipeline depth, so a producer that finds the pool empty is
//! being back-pressured by the consumer, which is the point.
//!
//! The storage behind a slot is the producer's business ([`FrameStorage`]):
//! the X11 backend uses MIT-SHM segments so the X server writes the pixels
//! straight into the slot, with no copy in this process.

use std::any::Any;
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

/// Memory a frame lives in. Implemented for `Vec<u8>`; a producer may supply
/// its own (a shared-memory segment, say).
pub trait FrameStorage: Send + 'static {
    /// The whole buffer.
    fn bytes(&self) -> &[u8];
    /// The whole buffer, writable.
    fn bytes_mut(&mut self) -> &mut [u8];
    /// For a producer to recognise its own storage when a slot comes back.
    fn as_any(&self) -> &dyn Any;
    /// By value, for [`PoolSlot::take_storage`] callers that downcast.
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

impl FrameStorage for Vec<u8> {
    fn bytes(&self) -> &[u8] {
        self
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        self
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

struct PoolInner {
    /// One permit per slot that is not out with a frame or a producer.
    permits: Arc<Semaphore>,
    /// Storage waiting for reuse. May hold fewer entries than there are free
    /// permits (a slot not yet allocated) but never more.
    free: Mutex<Vec<Box<dyn FrameStorage>>>,
}

impl PoolInner {
    fn give_back(&self, storage: Box<dyn FrameStorage>) {
        self.free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(storage);
    }
}

/// A fixed number of reusable frame slots.
#[derive(Clone)]
pub struct FramePool {
    inner: Arc<PoolInner>,
    slots: usize,
}

impl FramePool {
    /// A pool of `slots` slots, none allocated yet.
    pub fn new(slots: usize) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                permits: Arc::new(Semaphore::new(slots)),
                free: Mutex::new(Vec::new()),
            }),
            slots,
        }
    }

    /// Number of slots, in use or not.
    pub fn slots(&self) -> usize {
        self.slots
    }

    /// Slots currently free (not held by a producer or a frame).
    pub fn free_slots(&self) -> usize {
        self.inner.permits.available_permits()
    }

    /// Take a slot if one is free. Its storage is whatever the last user
    /// left, or `None` if this slot was never allocated.
    pub fn try_acquire(&self) -> Option<PoolSlot> {
        match self.inner.permits.clone().try_acquire_owned() {
            Ok(permit) => Some(self.slot(permit)),
            Err(TryAcquireError::NoPermits) | Err(TryAcquireError::Closed) => None,
        }
    }

    /// Wait for a slot.
    pub async fn acquire(&self) -> PoolSlot {
        // The semaphore is never closed, so this cannot fail.
        let permit = self
            .inner
            .permits
            .clone()
            .acquire_owned()
            .await
            .expect("frame pool semaphore is never closed");
        self.slot(permit)
    }

    /// Resolve once at least one slot is free, without taking it. For a
    /// single-consumer producer that then calls [`try_acquire`] (nothing else
    /// takes slots, and slots only come back), so the wait can sit in an
    /// async loop while the grab itself stays synchronous.
    ///
    /// [`try_acquire`]: FramePool::try_acquire
    pub async fn wait_free(&self) {
        // Wait on the semaphore alone: no slot is taken, so no storage is
        // popped and pushed back.
        drop(
            self.inner
                .permits
                .acquire()
                .await
                .expect("frame pool semaphore is never closed"),
        );
    }

    /// Remove and return every free storage for which `remove` is true. A
    /// producer uses this to release storage that no longer fits (after a
    /// resize) without waiting for each slot to be popped. Storage held by
    /// frames in flight is not touched; it comes back later.
    pub fn take_free_where(
        &self,
        mut remove: impl FnMut(&dyn FrameStorage) -> bool,
    ) -> Vec<Box<dyn FrameStorage>> {
        let mut free = self.inner.free.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        let mut i = 0;
        while i < free.len() {
            if remove(free[i].as_ref()) {
                out.push(free.swap_remove(i));
            } else {
                i += 1;
            }
        }
        out
    }

    fn slot(&self, permit: OwnedSemaphorePermit) -> PoolSlot {
        let storage = self
            .inner
            .free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop();
        PoolSlot {
            permit: Some(permit),
            storage,
            pool: self.inner.clone(),
        }
    }
}

/// A slot taken from a [`FramePool`], not yet a frame. Dropping it returns
/// the slot.
pub struct PoolSlot {
    permit: Option<OwnedSemaphorePermit>,
    storage: Option<Box<dyn FrameStorage>>,
    pool: Arc<PoolInner>,
}

impl PoolSlot {
    /// The slot's storage, if it has any.
    pub fn storage(&self) -> Option<&dyn FrameStorage> {
        self.storage.as_deref()
    }

    /// Remove the storage (to discard or resize it). The slot stays held.
    pub fn take_storage(&mut self) -> Option<Box<dyn FrameStorage>> {
        self.storage.take()
    }

    /// Install storage the producer allocated.
    pub fn set_storage(&mut self, storage: Box<dyn FrameStorage>) {
        self.storage = Some(storage);
    }

    /// Writable view of the storage, if any.
    pub fn storage_mut(&mut self) -> Option<&mut (dyn FrameStorage + 'static)> {
        self.storage.as_deref_mut()
    }

    /// Turn the slot into a frame buffer. Returns the slot back if it has no
    /// storage yet.
    pub fn into_buf(mut self) -> Result<FrameBuf, PoolSlot> {
        let Some(storage) = self.storage.take() else {
            return Err(self);
        };
        let permit = self.permit.take();
        Ok(FrameBuf {
            repr: Repr::Pooled {
                storage: Some(storage),
                _permit: permit,
                pool: self.pool.clone(),
            },
        })
    }
}

impl Drop for PoolSlot {
    fn drop(&mut self) {
        if let Some(s) = self.storage.take() {
            self.pool.give_back(s);
        }
        // The permit drops after this body, once the storage is back.
    }
}

enum Repr {
    Owned(Vec<u8>),
    Pooled {
        storage: Option<Box<dyn FrameStorage>>,
        /// Dropped after `Drop::drop` has returned the storage, so a waiter
        /// woken by the permit always finds it.
        _permit: Option<OwnedSemaphorePermit>,
        pool: Arc<PoolInner>,
    },
}

/// The pixels of one frame: either owned outright or a pooled slot that
/// returns to its pool when dropped. Derefs to the bytes.
pub struct FrameBuf {
    repr: Repr,
}

impl FrameBuf {
    /// An empty buffer (a GPU frame carries its pixels in a dma-buf).
    pub fn empty() -> Self {
        Self {
            repr: Repr::Owned(Vec::new()),
        }
    }

    /// Whether this buffer returns to a pool on drop.
    pub fn is_pooled(&self) -> bool {
        matches!(self.repr, Repr::Pooled { .. })
    }

    /// The bytes, writable.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        match &mut self.repr {
            Repr::Owned(v) => v,
            Repr::Pooled { storage, .. } => storage.as_mut().expect("live").bytes_mut(),
        }
    }
}

impl Default for FrameBuf {
    fn default() -> Self {
        Self::empty()
    }
}

impl From<Vec<u8>> for FrameBuf {
    fn from(v: Vec<u8>) -> Self {
        Self {
            repr: Repr::Owned(v),
        }
    }
}

impl std::ops::Deref for FrameBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match &self.repr {
            Repr::Owned(v) => v,
            Repr::Pooled { storage, .. } => storage.as_ref().expect("live").bytes(),
        }
    }
}

impl AsRef<[u8]> for FrameBuf {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl std::fmt::Debug for FrameBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "FrameBuf({} bytes, pooled={})",
            self.len(),
            self.is_pooled()
        )
    }
}

impl Drop for FrameBuf {
    fn drop(&mut self) {
        if let Repr::Pooled { storage, pool, .. } = &mut self.repr {
            if let Some(s) = storage.take() {
                pool.give_back(s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn buf(pool: &FramePool, fill: u8) -> FrameBuf {
        let mut slot = pool.try_acquire().expect("free slot");
        if slot.storage().is_none() {
            slot.set_storage(Box::new(vec![0u8; 16]));
        }
        let mut b = slot.into_buf().ok().expect("storage");
        b.bytes_mut().fill(fill);
        b
    }

    #[test]
    fn drop_returns_the_slot_and_its_memory() {
        let pool = FramePool::new(2);
        let a = buf(&pool, 1);
        let ptr = a.as_ptr();
        assert_eq!(pool.free_slots(), 1);
        drop(a);
        assert_eq!(pool.free_slots(), 2);
        // The same allocation comes back: nothing was allocated again.
        let slot = pool.try_acquire().unwrap();
        assert_eq!(slot.storage().unwrap().bytes().as_ptr(), ptr);
    }

    #[test]
    fn exhaustion_is_visible_and_recovers() {
        let pool = FramePool::new(4);
        let held: Vec<_> = (0..4).map(|i| buf(&pool, i)).collect();
        assert!(pool.try_acquire().is_none(), "a fifth slot appeared");
        drop(held);
        assert_eq!(pool.free_slots(), 4);
        assert!(pool.try_acquire().is_some());
    }

    #[test]
    fn a_slot_dropped_before_it_became_a_frame_returns() {
        let pool = FramePool::new(1);
        let mut slot = pool.try_acquire().unwrap();
        slot.set_storage(Box::new(vec![0u8; 4]));
        drop(slot);
        assert_eq!(pool.free_slots(), 1);
        assert!(pool.try_acquire().unwrap().storage().is_some());
    }

    #[test]
    fn frames_dropped_on_another_thread_return() {
        let pool = FramePool::new(2);
        let a = buf(&pool, 1);
        let b = buf(&pool, 2);
        std::thread::spawn(move || drop((a, b))).join().unwrap();
        assert_eq!(pool.free_slots(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_free_blocks_until_a_frame_is_dropped() {
        let pool = FramePool::new(1);
        let a = buf(&pool, 1);
        let waiter = {
            let p = pool.clone();
            tokio::spawn(async move { p.wait_free().await })
        };
        // Nothing frees the slot: the waiter must still be pending.
        let timed = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::task::yield_now().await;
            while !waiter.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(timed.is_err(), "wait_free returned with every slot held");
        drop(a);
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("slot was returned but the waiter never woke")
            .unwrap();
        assert_eq!(pool.free_slots(), 1, "wait_free must not keep the slot");
    }

    #[test]
    fn take_free_where_removes_only_matching_free_storage() {
        let pool = FramePool::new(3);
        let mut held = Vec::new();
        for n in [4usize, 8, 8] {
            let mut slot = pool.try_acquire().unwrap();
            if slot.storage().is_none() {
                slot.set_storage(Box::new(vec![0u8; n]));
            }
            held.push(slot.into_buf().ok().unwrap());
        }
        let in_flight = held.pop().unwrap();
        drop(held);
        let gone = pool.take_free_where(|s| s.bytes().len() == 8);
        assert_eq!(gone.len(), 1, "the in-flight 8-byte one is not free");
        assert_eq!(pool.free_slots(), 2, "permits are untouched");
        drop(in_flight);
        assert_eq!(pool.take_free_where(|s| s.bytes().len() == 8).len(), 1);
    }

    #[test]
    fn owned_buffers_do_not_touch_a_pool() {
        let b = FrameBuf::from(vec![1, 2, 3]);
        assert!(!b.is_pooled());
        assert_eq!(&*b, &[1, 2, 3]);
        assert!(FrameBuf::default().is_empty());
    }
}
