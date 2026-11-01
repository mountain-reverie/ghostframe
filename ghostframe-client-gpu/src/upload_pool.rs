//! A few persistent upload slabs, and the protocol that lets the decode thread
//! write tiles into them while the render thread only ever names them.
//!
//! ## Why
//!
//! A `TileReady` tile used to be a fresh 4 KiB `Vec` (allocate, zero, decode
//! into it), then copied into wgpu staging memory at flush, then copied again
//! by the driver. On a Pinebook Pro the CPU is what runs out, and those passes
//! and the allocator were at the top of both threads' profiles. Here the decode
//! thread writes the tile once, into memory the GPU samples directly, and the
//! render thread forwards `(slab, index)` and never touches pixels.
//!
//! ## Layout and sizing
//!
//! A slab is [`SLAB_WIDTH`] texels wide ([`TILES_PER_ROW`] = 64 tiles) and
//! `slab_rows * 32` texels tall. Slot `i` is the 32x32 texel block at
//! `((i % 64) * 32, (i / 64) * 32)`; a tile row is 128 contiguous bytes and
//! rows are one `stride` apart, which is why the decoder's sink is row based
//! (see `ghostframe_client_core::TileOut`).
//!
//! The geometry follows the remote resolution ([`slab_rows_for`],
//! [`slabs_needed`]), with `T` = tiles in a full screen:
//!
//! - **Slab size**: about `T / 4` slots, rounded up to whole slab rows and
//!   clamped to 256..=1024 slots, so a full repaint spans about four slabs.
//!   Finer slabs recycle sooner -- the old fixed 1024-slot slab was only
//!   reusable once all of it had drained, and ran dry in a connection's opening
//!   burst -- but every slab a flush uses costs a dispatch and a bind group, so
//!   not much finer.
//! - **Total**: `3 * T` slots (at least 4 slabs): one repaint being filled, one
//!   on the GPU, one spare. At 1080p (T = 2040) that is 512-slot, 2 MiB slabs,
//!   12 of them, 24 MiB. Capped at [`MAX_POOL_BYTES`].
//!
//! The pool starts at the 1080p plan, because the decode thread needs it
//! before the first `FrameDimensions` reaches the render thread, and **grows**
//! (slabs are appended, never freed or replaced) when a larger frame arrives,
//! see [`UploadPool::ensure_for`]. The slab geometry is fixed at creation.
//! When no slab is free the allocator says so and the decoder falls back to a
//! `Vec`; nothing here ever blocks.
//!
//! ## Who owns what
//!
//! - [`SlotAllocator`] lives on the decode thread (it takes `&mut self`, so one
//!   thread at a time). It is the only writer, and writes only slots it has not
//!   yet committed.
//! - [`UploadPool`] lives on the render thread with the GPU textures; the
//!   `Renderer` holds a clone. It never writes pixels.
//! - Both share an `Arc<PoolShared>` holding the CPU mappings and the per-slab
//!   atomics. **The mappings are owned by `Arc<SlabShared>`s and nothing else**,
//!   so they live until the last handle goes: dropping the renderer, the
//!   textures or the pool while the decode thread still holds a slot leaves the
//!   decode thread writing to valid memory nobody reads. The textures are *not*
//!   in `PoolShared`: dropping one off the render thread would run GL teardown
//!   on the wrong thread.
//! - Growth appends to the slab list under a lock the allocator takes only when
//!   it needs a new slab. Nothing is ever removed, so a slab with unconsumed
//!   slots or an open write window cannot be freed from under anyone.
//!
//! ## Life of a slab
//!
//! The allocator takes a slab, fills slots `0..` in order, and *retires* it
//! when full (a partially filled slab stays current: the GPU reads only slots
//! the render thread has been told about, and the decode thread writes only
//! beyond them, disjoint 128-byte rows). The render thread counts every slot
//! it was given, whether it was drawn, deduplicated or superseded, and after
//! the submit that could have read them completes it adds the count to the
//! slab's `consumed`. The allocator reuses a retired slab only when
//! `consumed == issued`, bumping the slab's `generation`; each [`SlotRef`]
//! carries the generation it was issued under, and the renderer
//! `debug_assert`s it still matches when the slot is drawn, so a reuse before
//! consumption is a loud failure in tests rather than a flicker in the field.
//!
//! Completion is `Queue::on_submitted_work_done`, not the publish poll:
//! `ExportRing` waits after each publish today, but a flush can submit without
//! publishing, and that wait is expected to go away.

use crate::upload_slab::{SlabMap, SlabSource};
use crate::wgpu_ctx::WgpuContext;
use crate::GpuError;
use ghostframe_client_core::{TileOut, TileSink, TILE_ROW_BYTES};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

/// Texels across a slab.
pub const SLAB_WIDTH: u32 = 2048;
/// Tiles across a slab.
pub const TILES_PER_ROW: u32 = SLAB_WIDTH / 32;
/// The frame the pool is sized for until a `FrameDimensions` says otherwise.
pub const DEFAULT_FRAME: (u32, u32) = (1920, 1080);
/// Pool capacity, in full screens of tiles. See the module doc.
const SCREENS_OF_SLOTS: u32 = 3;
/// A full repaint should span about this many slabs.
const SLABS_PER_SCREEN: u32 = 4;
/// Slab height bounds, in tile rows: 256..=1024 slots.
const MIN_SLAB_ROWS: u32 = 4;
const MAX_SLAB_ROWS: u32 = 16;
/// Fewest slabs a pool has.
const MIN_SLABS: usize = 4;
/// Most memory the pool will hold, whatever the resolution. A frame so large
/// that three screens do not fit runs partly on `Vec`s (the fallback).
pub const MAX_POOL_BYTES: usize = 128 << 20;

fn tiles_in(width: u32, height: u32) -> u32 {
    width.div_ceil(32) * height.div_ceil(32)
}

/// Tile rows per slab for a `width` x `height` frame.
pub fn slab_rows_for(width: u32, height: u32) -> u32 {
    let per_slab = tiles_in(width, height).div_ceil(SLABS_PER_SCREEN);
    per_slab
        .div_ceil(TILES_PER_ROW)
        .clamp(MIN_SLAB_ROWS, MAX_SLAB_ROWS)
}

fn slab_bytes(slab_rows: u32) -> usize {
    SLAB_WIDTH as usize * slab_rows as usize * 32 * 4
}

/// Slabs of `slab_rows` rows the pool should hold for a `width` x `height`
/// frame: three screens of slots, at least [`MIN_SLABS`], at most what
/// [`MAX_POOL_BYTES`] and the 8-bit slab index allow.
pub fn slabs_needed(width: u32, height: u32, slab_rows: u32) -> usize {
    let slots = (slab_rows * TILES_PER_ROW) as usize;
    let wanted = (tiles_in(width, height) as usize * SCREENS_OF_SLOTS as usize).div_ceil(slots);
    let cap = (MAX_POOL_BYTES / slab_bytes(slab_rows)).min(u8::MAX as usize);
    wanted.max(MIN_SLABS).min(cap)
}

/// A committed tile: where it is and which use of the slab it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotRef {
    slab: u8,
    index: u16,
    generation: u32,
}

impl SlotRef {
    pub fn slab(&self) -> usize {
        self.slab as usize
    }

    pub fn index(&self) -> u32 {
        self.index as u32
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }
}

struct SlabShared {
    map: SlabMap,
    /// Slots the render side has finished with since the allocator last took
    /// this slab. Written by the render thread (from a wgpu callback), read by
    /// the allocator.
    consumed: AtomicU32,
    /// Uses of this slab so far. Written by the allocator when it takes the
    /// slab, read by the renderer to check its slots are not stale.
    generation: AtomicU32,
    /// Slots committed in the slab's current use: a copy of the allocator's
    /// own count, there only so the render thread can report it.
    issued: AtomicU32,
}

struct PoolShared {
    /// Append-only; see the module doc.
    slabs: RwLock<Vec<Arc<SlabShared>>>,
    /// `slabs.len()`, readable without the lock.
    len: AtomicUsize,
    slots_per_slab: u32,
    /// Whether the allocator brackets its writes with `DMA_BUF_IOCTL_SYNC`.
    cpu_sync: bool,
    /// Times `acquire` found no slot.
    exhausted: AtomicU64,
    /// Time the allocator spent inside `DMA_BUF_IOCTL_SYNC`, and the calls.
    sync_ns: AtomicU64,
    /// Of `sync_ns`, the END calls.
    sync_end_ns: AtomicU64,
    sync_calls: AtomicU64,
}

impl PoolShared {
    fn slab(&self, i: usize) -> Option<Arc<SlabShared>> {
        self.slabs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(i)
            .cloned()
    }

    fn retire(&self, counts: &[u32]) {
        for (slab, &n) in counts.iter().enumerate() {
            if n == 0 {
                continue;
            }
            let s = self
                .slab(slab)
                .expect("retiring a slab that does not exist");
            let prev = s.consumed.fetch_add(n, Ordering::Release);
            debug_assert!(
                prev + n <= self.slots_per_slab,
                "slab {slab}: {} slots retired of at most {}",
                prev + n,
                self.slots_per_slab
            );
        }
    }
}

/// A snapshot of the pool for the render thread's periodic summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStats {
    /// Times a tile found no free slot and travelled as a `Vec`.
    pub exhausted: u64,
    /// Time the decode thread spent in the dma-buf sync ioctls, in total.
    pub sync_ns: u64,
    /// Of `sync_ns`, the time in END (window close) calls.
    pub sync_end_ns: u64,
    pub sync_calls: u64,
    /// Per slab: `(issued, consumed)` in its current use.
    pub slabs: Vec<(u32, u32)>,
}

/// What only the render thread touches: the textures, and where more slabs
/// come from.
struct GpuSide {
    textures: Mutex<Vec<wgpu::Texture>>,
    source: SlabSource,
    slab_rows: u32,
}

/// The GPU side of the pool: the textures, and the means to retire slots.
#[derive(Clone)]
pub struct UploadPool {
    shared: Arc<PoolShared>,
    gpu: Arc<GpuSide>,
}

impl UploadPool {
    /// The pool for [`DEFAULT_FRAME`]. Fails if the backend cannot (no
    /// writable linear image, not enough memory); the caller then runs
    /// without a pool.
    pub fn new(ctx: &WgpuContext) -> Result<(UploadPool, SlotAllocator), GpuError> {
        let (w, h) = DEFAULT_FRAME;
        let rows = slab_rows_for(w, h);
        Self::with_config(ctx, rows, slabs_needed(w, h, rows), None)
    }

    /// `count` slabs of 1024 slots, for tests that need to exhaust the pool
    /// quickly.
    pub fn with_slabs(
        ctx: &WgpuContext,
        count: usize,
    ) -> Result<(UploadPool, SlotAllocator), GpuError> {
        Self::with_config(ctx, MAX_SLAB_ROWS, count, None)
    }

    /// The general constructor: `count` slabs of `slab_rows` tile rows.
    /// `driver` replaces the detected kernel driver name when deciding whether
    /// writes need the dma-buf sync ioctls
    /// ([`crate::dmabuf::cpu_access_hooks_are_noops`]); tests inject it.
    pub fn with_config(
        ctx: &WgpuContext,
        slab_rows: u32,
        count: usize,
        driver: Option<&str>,
    ) -> Result<(UploadPool, SlotAllocator), GpuError> {
        assert!(
            (1..=u8::MAX as usize).contains(&count),
            "slab count must fit a SlotRef"
        );
        assert!(
            (1..=MAX_SLAB_ROWS).contains(&slab_rows),
            "slab rows out of range"
        );
        let limit = ctx.device.limits().max_texture_dimension_2d;
        if limit < SLAB_WIDTH.max(slab_rows * 32) {
            return Err(GpuError::Device(format!(
                "max_texture_dimension_2d is {limit}, an upload slab needs {SLAB_WIDTH}"
            )));
        }
        let gpu = Arc::new(GpuSide {
            textures: Mutex::new(Vec::new()),
            source: SlabSource::new()?,
            slab_rows,
        });
        let shared = Arc::new(PoolShared {
            slabs: RwLock::new(Vec::new()),
            len: AtomicUsize::new(0),
            slots_per_slab: slab_rows * TILES_PER_ROW,
            cpu_sync: gpu.source.needs_cpu_sync(driver),
            exhausted: AtomicU64::new(0),
            sync_ns: AtomicU64::new(0),
            sync_end_ns: AtomicU64::new(0),
            sync_calls: AtomicU64::new(0),
        });
        let pool = UploadPool { shared, gpu };
        pool.append(ctx, count)?;
        let allocator = SlotAllocator {
            shared: Arc::clone(&pool.shared),
            local: Vec::new(),
            current: None,
            cursor: 0,
            last_taken: 0,
            issued: Vec::new(),
            busy: Vec::new(),
            open: Vec::new(),
            sync_warned: false,
        };
        Ok((pool, allocator))
    }

    /// Append `count` slabs: allocate, put each in its sampled state, then
    /// publish them (textures first, so the render side never sees a slab
    /// without its texture).
    fn append(&self, ctx: &WgpuContext, count: usize) -> Result<(), GpuError> {
        let mut new_slabs = Vec::with_capacity(count);
        let mut new_textures = Vec::with_capacity(count);
        for _ in 0..count {
            let height = self.gpu.slab_rows * 32;
            let alloc = self.gpu.source.alloc(ctx, SLAB_WIDTH, height)?;
            // A slot is addressed as `index / 64` bands of 32 rows, so the
            // pitch must at least hold 64 tiles' rows; a driver that padded a
            // narrower pitch would shear every tile.
            if alloc.map.stride() < SLAB_WIDTH as usize * 4 {
                return Err(GpuError::Device(format!(
                    "upload slab pitch {} is narrower than {} bytes",
                    alloc.map.stride(),
                    SLAB_WIDTH * 4
                )));
            }
            new_textures.push(alloc.texture);
            new_slabs.push(Arc::new(SlabShared {
                map: alloc.map,
                consumed: AtomicU32::new(0),
                generation: AtomicU32::new(0),
                issued: AtomicU32::new(0),
            }));
        }
        // Put every new slab in its sampled state before anything can write to
        // it. Vulkan would otherwise do it at the first flush that draws from
        // the slab -- after the decode thread had written the first batch,
        // which a transition out of the image's initial layout is allowed to
        // discard. Harmless elsewhere.
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ghostframe-upload-slab-prime"),
            });
        encoder.transition_resources(
            std::iter::empty(),
            new_textures.iter().map(|texture| wgpu::TextureTransition {
                texture,
                selector: None,
                state: wgpu::TextureUses::RESOURCE,
            }),
        );
        ctx.queue.submit([encoder.finish()]);
        ctx.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| GpuError::Device(format!("priming upload slabs: {e}")))?;

        self.gpu
            .textures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(new_textures);
        let mut slabs = self.shared.slabs.write().unwrap_or_else(|e| e.into_inner());
        slabs.extend(new_slabs);
        self.shared.len.store(slabs.len(), Ordering::Release);
        Ok(())
    }

    /// Make sure the pool is big enough for a `width` x `height` frame,
    /// appending slabs if not. Returns how many were added. Never removes or
    /// replaces a slab, so slots in flight are undisturbed; the decode thread
    /// picks the new slabs up the next time it needs one. Render thread only.
    pub fn ensure_for(
        &self,
        ctx: &WgpuContext,
        width: u32,
        height: u32,
    ) -> Result<usize, GpuError> {
        let max = ghostframe_client_core::MAX_FRAME_DIMENSION;
        if !(1..=max).contains(&width) || !(1..=max).contains(&height) {
            return Ok(0);
        }
        let want = slabs_needed(width, height, self.gpu.slab_rows);
        let have = self.slab_count();
        if want <= have {
            return Ok(0);
        }
        self.append(ctx, want - have)?;
        tracing::info!(
            width,
            height,
            slabs = want,
            slots_per_slab = self.slots_per_slab(),
            "upload pool grown for the frame size"
        );
        Ok(want - have)
    }

    pub fn slab_count(&self) -> usize {
        self.shared.len.load(Ordering::Acquire)
    }

    /// Slots in each slab.
    pub fn slots_per_slab(&self) -> u32 {
        self.shared.slots_per_slab
    }

    pub(crate) fn texture(&self, slab: usize) -> wgpu::Texture {
        self.gpu.textures.lock().unwrap_or_else(|e| e.into_inner())[slab].clone()
    }

    /// Whether `slot` still belongs to the use of its slab it was issued
    /// under. False means the slab was recycled while the slot was pending.
    pub(crate) fn is_current(&self, slot: SlotRef) -> bool {
        self.shared
            .slab(slot.slab())
            .is_some_and(|s| s.generation.load(Ordering::Acquire) == slot.generation)
    }

    pub fn stats(&self) -> PoolStats {
        let s = &self.shared;
        PoolStats {
            exhausted: s.exhausted.load(Ordering::Relaxed),
            sync_ns: s.sync_ns.load(Ordering::Relaxed),
            sync_end_ns: s.sync_end_ns.load(Ordering::Relaxed),
            sync_calls: s.sync_calls.load(Ordering::Relaxed),
            slabs: s
                .slabs
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|s| {
                    (
                        s.issued.load(Ordering::Relaxed),
                        s.consumed.load(Ordering::Relaxed),
                    )
                })
                .collect(),
        }
    }

    /// Slots of `slab` the render side has finished with. For tests and
    /// diagnostics.
    pub fn consumed(&self, slab: usize) -> u32 {
        self.shared
            .slab(slab)
            .expect("no such slab")
            .consumed
            .load(Ordering::Acquire)
    }

    /// Retire slots the GPU never saw (a tile dropped before reaching a
    /// submit). `counts[slab]` slots of each slab.
    pub fn retire_now(&self, counts: &[u32]) {
        self.shared.retire(counts);
    }

    /// Retire slots once every submit made so far has completed on the GPU.
    /// Call it after the `queue.submit` that may read them. Independent of any
    /// other wait the caller does.
    pub fn retire_when_submitted_work_done(&self, queue: &wgpu::Queue, counts: Vec<u32>) {
        if counts.iter().all(|&n| n == 0) {
            return;
        }
        let shared = Arc::clone(&self.shared);
        queue.on_submitted_work_done(move || shared.retire(&counts));
    }
}

/// The decode thread's side: hands out slots and writes tiles into them.
///
/// A [`TileSink`], so `TileDecoder::decode_into` fills slots directly. Its
/// methods take `&mut self`: one thread owns the write side.
pub struct SlotAllocator {
    shared: Arc<PoolShared>,
    /// This thread's view of the append-only slab list, so the hot path takes
    /// no lock. Refreshed when a new slab is needed.
    local: Vec<Arc<SlabShared>>,
    /// The slab being filled and the next free slot in it.
    current: Option<usize>,
    cursor: u32,
    last_taken: usize,
    /// Slots committed in each slab's current use. Only this thread writes it.
    issued: Vec<u32>,
    /// Slab is held (filling, or retired and waiting to be consumed).
    busy: Vec<bool>,
    /// A CPU write window is open on the slab.
    open: Vec<bool>,
    sync_warned: bool,
}

impl SlotAllocator {
    /// Times `acquire` found no free slot (the decoder then used a `Vec`).
    pub fn exhausted(&self) -> u64 {
        self.shared.exhausted.load(Ordering::Relaxed)
    }

    /// Pick up slabs the pool has grown by since the last look.
    fn refresh(&mut self) {
        if self.local.len() == self.shared.len.load(Ordering::Acquire) {
            return;
        }
        let slabs = self.shared.slabs.read().unwrap_or_else(|e| e.into_inner());
        self.local.extend(slabs[self.local.len()..].iter().cloned());
        let n = self.local.len();
        self.issued.resize(n, 0);
        self.busy.resize(n, false);
        self.open.resize(n, false);
    }

    /// Run a dma-buf sync call, counting its time.
    fn timed_sync(
        shared: &PoolShared,
        warned: &mut bool,
        is_end: bool,
        call: impl FnOnce() -> Result<(), GpuError>,
    ) {
        let start = Instant::now();
        let result = call();
        let ns = Instant::now().saturating_duration_since(start).as_nanos() as u64;
        shared.sync_ns.fetch_add(ns, Ordering::Relaxed);
        if is_end {
            shared.sync_end_ns.fetch_add(ns, Ordering::Relaxed);
        }
        shared.sync_calls.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = result {
            Self::warn_sync(warned, &e);
        }
    }

    /// Close every CPU write window. Call before sending the render thread a
    /// message naming a slot written since the last call: the GPU may read the
    /// slot as soon as it hears of it.
    pub fn end_cpu_writes(&mut self) {
        for (slab, open) in self.open.iter_mut().enumerate() {
            if std::mem::take(open) && self.shared.cpu_sync {
                let (shared, map) = (&self.shared, &self.local[slab].map);
                Self::timed_sync(shared, &mut self.sync_warned, true, || map.end_cpu_write());
            }
        }
    }

    fn warn_sync(warned: &mut bool, e: &GpuError) {
        // Not fatal: coherent mappings need no bracket. Once is enough.
        if !std::mem::replace(warned, true) {
            tracing::warn!(error = %e, "dma-buf sync around slab writes failed");
        }
    }

    /// The next slab that is free, or whose every issued slot has been
    /// consumed; marks it busy under a new generation.
    fn take_slab(&mut self) -> Option<usize> {
        self.refresh();
        let n = self.local.len();
        for step in 1..=n {
            let s = (self.last_taken + step) % n;
            let slab = &self.local[s];
            if self.busy[s] && slab.consumed.load(Ordering::Acquire) != self.issued[s] {
                continue;
            }
            debug_assert!(
                !self.open[s] || self.issued[s] == 0,
                "recycling a slab whose write window is open"
            );
            self.busy[s] = true;
            self.issued[s] = 0;
            slab.consumed.store(0, Ordering::Release);
            slab.issued.store(0, Ordering::Relaxed);
            slab.generation.fetch_add(1, Ordering::AcqRel);
            self.last_taken = s;
            return Some(s);
        }
        None
    }
}

impl TileSink for SlotAllocator {
    type Placed = SlotRef;

    fn acquire(&mut self) -> Option<TileOut<'_>> {
        let slab = match self.current {
            Some(s) => s,
            None => match self.take_slab() {
                Some(s) => {
                    self.current = Some(s);
                    self.cursor = 0;
                    s
                }
                None => {
                    self.shared.exhausted.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            },
        };
        let shared = &self.local[slab];
        if !self.open[slab] {
            // START waits for the buffer's fences, so on a driver whose
            // exporter has hooks it can stall behind GPU jobs reading earlier
            // slots of this slab; see `cpu_access_hooks_are_noops` for why it
            // is skipped where that is safe, and kept everywhere else.
            if self.shared.cpu_sync {
                Self::timed_sync(&self.shared, &mut self.sync_warned, false, || {
                    shared.map.begin_cpu_write()
                });
            }
            self.open[slab] = true;
        }
        let stride = shared.map.stride();
        let index = self.cursor as usize;
        let offset = (index / TILES_PER_ROW as usize) * 32 * stride
            + (index % TILES_PER_ROW as usize) * TILE_ROW_BYTES;
        // The last byte of the slot's last row must be inside the mapping.
        assert!(
            offset + stride * 31 + TILE_ROW_BYTES <= shared.map.len(),
            "slot {index} runs past the slab"
        );
        // SAFETY: the 32 rows `offset + y * stride .. + 128` are inside the
        // mapping (asserted above), which the `Arc` in `self.local` keeps
        // alive for the borrow. This thread is the only writer, and the slot
        // at `cursor` has not been committed, so the render thread has not
        // been told of it and the GPU does not read it. `TileOut` forms only
        // 128-byte references to those row segments; the bytes between them,
        // which belong to neighbouring slots the GPU may be reading, are never
        // covered by a reference.
        let base = unsafe { NonNull::new_unchecked(shared.map.base().add(offset)) };
        Some(unsafe { TileOut::from_raw(base, stride) })
    }

    fn commit(&mut self, _frame_seq: u32, _tile_x: u8, _tile_y: u8) -> SlotRef {
        let slab = self.current.expect("commit without a preceding acquire");
        let index = self.cursor;
        self.cursor += 1;
        self.issued[slab] = self.cursor;
        self.local[slab]
            .issued
            .store(self.cursor, Ordering::Relaxed);
        if self.cursor == self.shared.slots_per_slab {
            // Full: retired. It is reusable once everything issued is consumed.
            self.current = None;
        }
        SlotRef {
            slab: slab as u8,
            index: index as u16,
            generation: self.local[slab].generation.load(Ordering::Acquire),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_1080p_pool_is_twelve_2_mib_slabs() {
        let rows = slab_rows_for(1920, 1080);
        assert_eq!(rows * TILES_PER_ROW, 512);
        assert_eq!(slabs_needed(1920, 1080, rows), 12);
        assert_eq!(slab_bytes(rows), 2 << 20);
        assert_eq!(slabs_needed(1920, 1080, rows) * slab_bytes(rows), 24 << 20);
    }

    #[test]
    fn sizing_follows_the_resolution_within_bounds() {
        for (w, h) in [
            (1, 1),
            (640, 480),
            (1280, 720),
            (1920, 1080),
            (3840, 2160),
            (8192, 8192),
        ] {
            let rows = slab_rows_for(w, h);
            assert!((MIN_SLAB_ROWS..=MAX_SLAB_ROWS).contains(&rows), "{w}x{h}");
            let n = slabs_needed(w, h, rows);
            assert!((MIN_SLABS..=u8::MAX as usize).contains(&n), "{w}x{h}");
            assert!(n * slab_bytes(rows) <= MAX_POOL_BYTES, "{w}x{h}");
            // Within the cap, the pool holds three screens of slots.
            if n * slab_bytes(rows) < MAX_POOL_BYTES {
                assert!(
                    n * (rows * TILES_PER_ROW) as usize >= 3 * tiles_in(w, h) as usize,
                    "{w}x{h}"
                );
            }
        }
        // Bigger frames never ask for fewer or smaller slabs.
        let small = slab_rows_for(640, 480);
        assert!(slab_rows_for(1920, 1080) >= small);
        assert!(slabs_needed(3840, 2160, 16) >= slabs_needed(1920, 1080, 16));
    }

    #[test]
    fn a_full_repaint_spans_few_slabs() {
        for (w, h) in [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)] {
            let rows = slab_rows_for(w, h);
            let spans = tiles_in(w, h).div_ceil(rows * TILES_PER_ROW);
            assert!(spans <= SLABS_PER_SCREEN + 4, "{w}x{h} spans {spans} slabs");
        }
    }

    #[test]
    fn geometry_fits_texture_limits_and_the_tile_grid() {
        for rows in MIN_SLAB_ROWS..=MAX_SLAB_ROWS {
            assert!(rows * 32 <= 512 && SLAB_WIDTH <= 2048);
            assert_eq!(SLAB_WIDTH % 32, 0);
        }
    }
}
