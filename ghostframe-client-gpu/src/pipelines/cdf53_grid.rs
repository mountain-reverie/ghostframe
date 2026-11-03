//! Full-grid CDF 5/3 tile decoder: bit-planes in a per-tile plane store, one
//! workgroup per listed tile reconstructing from it.
//!
//! The decoder this replaced kept per-tile coefficient and sign buffers that
//! every arrived pass ORed bits into (atomics), cleared them from the host on a
//! generation change, and inverted through a work area in global memory over
//! four dispatches. This one keeps no coefficient state at all:
//!
//! - The **plane store** holds the bit-planes themselves, for every tile of the
//!   grid: 14 planes of 384 bytes (3 channels x 128) per tile,
//!   `cols * rows * 14 * 96` u32. A plane slot is written whole by exactly one
//!   plane or stream, so there is nothing to clear.
//! - **Stage A** (`cdf53_planes.wgsl`) fills the store: [`Cdf53Grid::push_plane`]
//!   queues a plane the host already decoded, and the stage is a word-for-word
//!   copy. (An earlier revision also decoded RLE streams on the GPU, one
//!   invocation per channel stream, for a "Phase 2" in which events carry the
//!   raw wire payload; nothing sends that yet, so the code was removed rather
//!   than compiled at start-up for nothing. It is in the history of this file,
//!   with its tests, under `cdf53_unpack.wgsl`.)
//! - **Stage B** (`cdf53_reconstruct.wgsl`), one workgroup per listed tile,
//!   builds the coefficients from the planes the tile's `plane_mask` names,
//!   inverts them in workgroup memory and stores the pixels.
//!
//! A generation change is therefore a host-side event only: the next frame
//! writes the new planes and lists the tile with a mask that names only them.
//! Stale planes stay in the store, unread.
//!
//! The host decides, per tile, the resolved prefix length `k` (passes 0..k are
//! known, a pass the tile's `present_passes` says was never sent counting as
//! known and all-zero) and the `plane_mask` of those among them that were
//! actually received; see [`crate::pipelines::cdf53_state::Cdf53GridState`],
//! which also decides *which* tiles to list under a time budget.
//!
//! # One upload per frame
//!
//! Everything the stages read for a frame is one buffer, written by one
//! `write_buffer_with` (each queue write costs ~0.16 ms of fixed CPU on GLES).
//! Layout, in u32 words, section offsets in the 16-word header:
//!
//! ```text
//! header (16): 0 cols, 1 fb width, 2 fb height, 3 tile count, 4 plane count,
//!              5 tiles off, 6 plane rows off, 7 plane data off, 15 = 0
//! tiles       2 words each: tile, plane_mask | k << 16
//! plane rows  2 words each: tile, pass
//! plane data  96 words per plane row, in the store's channel order
//! ```
//!
//! The slot table the reconstruct stage needs is constant and uploaded once,
//! at creation.
//!
//! # Host contract
//!
//! These are `debug_assert`ed, not checked in release builds, and the GPU
//! result is unspecified (a race on a plane slot, or one tile written twice)
//! if they are broken:
//!
//! - a tile is listed at most once per frame ([`Cdf53Grid::push_tile`]);
//! - a `(tile, pass)` plane is given at most once per frame;
//! - `k <= 14`;
//! - one [`Cdf53Grid::record`] per [`Cdf53Grid::begin_frame`], and one
//!   `record` per queue submit: the upload is a queue write, which runs ahead
//!   of every command of the next submit, so a second frame recorded into the
//!   same submit would overwrite the first's tables before either ran.
//! - at most 65535 listed tiles and planes per frame (one dispatch dimension each).

use std::collections::HashSet;

use crate::framebuffer::Framebuffer;

const PLANES_SRC: &str = include_str!("../../../shaders/client/cdf53_planes.wgsl");
const RECONSTRUCT_SRC: &str = include_str!("../../../shaders/client/cdf53_reconstruct.wgsl");

/// u32 words of plane store per tile: 14 planes x 96 words.
pub const TILE_WORDS: u64 = 14 * 96;
/// Bytes of one decoded plane: 3 channels x 128.
pub const PLANE_BYTES: usize = 384;
/// Smallest GPU buffer ever made, so the first few frames do not each regrow.
const MIN_BUFFER: u64 = 4096;
/// Words in the frame buffer's header.
const HEADER_WORDS: usize = 16;
/// Most workgroups one dispatch dimension takes.
const MAX_WORKGROUPS: u32 = 65535;

/// Reasons [`Cdf53Grid::resize`] can refuse a grid.
#[derive(Debug)]
pub struct GridTooLarge {
    pub bytes: u64,
    pub limit: u64,
}

impl std::fmt::Display for GridTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "plane store of {} bytes exceeds the device's {} byte binding limit",
            self.bytes, self.limit
        )
    }
}

impl std::error::Error for GridTooLarge {}

/// A buffer that only ever grows, to the next power of two over what was asked.
struct Grown {
    buffer: wgpu::Buffer,
    size: u64,
}

impl Grown {
    fn new(device: &wgpu::Device, label: &str) -> Self {
        Grown {
            buffer: Self::make(device, label, MIN_BUFFER),
            size: MIN_BUFFER,
        }
    }

    fn make(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Make room for `needed` bytes. True when the buffer was replaced, which
    /// invalidates every bind group that holds it.
    fn reserve(&mut self, device: &wgpu::Device, label: &str, needed: u64) -> bool {
        if needed <= self.size {
            return false;
        }
        self.size = needed.next_power_of_two();
        self.buffer = Self::make(device, label, self.size);
        true
    }
}

/// `phys_N` of `cdf53_reconstruct.wgsl`: the work-area slot of interleaved
/// position `y` after lifting level `n` (1 = the innermost, 8x8).
fn phys(n: u32, y: u32) -> u32 {
    if n == 0 {
        y
    } else if y.is_multiple_of(2) {
        phys(n - 1, y / 2)
    } else {
        (4 << (n - 1)) + y / 2
    }
}

/// Work-area slot (`row * 32 + column`) of each of a channel's 1024
/// coefficients, in layout order, as the reconstruct shader expects them: the
/// position `cdf53::forward` gives the coefficient, moved by the rule in the
/// shader's `place` comment.
fn coefficient_slots() -> Vec<u16> {
    // (first coefficient, y0, x0, height, width) of the ten subbands, in
    // layout order.
    const BANDS: [(u32, u32, u32, u32, u32); 10] = [
        (0, 0, 0, 4, 4),
        (16, 0, 4, 4, 4),
        (32, 4, 0, 4, 4),
        (48, 4, 4, 4, 4),
        (64, 0, 8, 8, 8),
        (128, 8, 0, 8, 8),
        (192, 8, 8, 8, 8),
        (256, 0, 16, 16, 16),
        (512, 16, 0, 16, 16),
        (768, 16, 16, 16, 16),
    ];
    let mut slots = vec![0u16; 1024];
    for (first, y0, x0, h, w) in BANDS {
        for j in 0..h * w {
            let (y, x) = (y0 + j / w, x0 + j % w);
            let mut row = y;
            let mut col = x;
            if y < 8 && (8..16).contains(&x) {
                row = phys(1, y);
            } else if y < 16 && x >= 16 {
                row = phys(2, y);
            }
            if (8..16).contains(&y) && x < 8 {
                col = phys(1, x);
            } else if y >= 16 && x < 16 {
                col = phys(2, x);
            }
            slots[(first + j) as usize] = (row * 32 + col) as u16;
        }
    }
    slots
}

pub struct Cdf53Grid {
    planes_copy: wgpu::ComputePipeline,
    reconstruct: wgpu::ComputePipeline,

    cols: u32,
    rows: u32,
    planes: Option<wgpu::Buffer>,
    frame: Grown,
    /// `coefficient_slots`, packed two u16 to a word; immutable, written once.
    slots: wgpu::Buffer,

    /// Bind groups hold the plane store and the frame buffer; both are
    /// replaced only by `resize` / a regrow, which drop these.
    /// Test override of the device's binding limit; see [`Self::set_limit_for_test`].
    limit_override: Option<u64>,
    planes_bind: Option<wgpu::BindGroup>,
    /// Cached against the framebuffer's epoch, like the other pipelines.
    reconstruct_bind: Option<(u64, wgpu::BindGroup)>,

    // Host side of the frame being built. Cleared, not freed, by `begin_frame`.
    tile_words: Vec<u32>,
    plane_rows: Vec<u32>,
    plane_data: Vec<u8>,
    /// Framebuffer size of the last `record`/`upload`, for the header.
    fb_size: (u32, u32),
    recorded: bool,

    // Host-contract bookkeeping, consulted only inside `debug_assert!`.
    seen_tiles: HashSet<u32>,
    seen_planes: HashSet<(u32, u8)>,
}

impl Cdf53Grid {
    pub fn new(device: &wgpu::Device) -> Self {
        Self::with_shader_sources(device, RECONSTRUCT_SRC)
    }

    /// As [`Self::new`] with other WGSL for the reconstruct stage, for A/B runs
    /// of shader variants. The entry point must be `main` and the bindings
    /// those of the shipped file.
    pub fn with_shader_sources(device: &wgpu::Device, reconstruct: &str) -> Self {
        let make = |label: &str, src: &str, zero_workgroup: bool| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(src.into()),
            });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: None,
                module: &module,
                entry_point: Some("main"),
                compilation_options: wgpu::PipelineCompilationOptions {
                    zero_initialize_workgroup_memory: zero_workgroup,
                    ..Default::default()
                },
                cache: None,
            })
        };
        let slot_table = coefficient_slots();
        let mut packed = Vec::with_capacity(2048);
        for pair in slot_table.chunks(2) {
            let word = pair[0] as u32 | (pair[1] as u32) << 16;
            packed.extend_from_slice(&word.to_le_bytes());
        }
        let slots = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-cdf53-grid-slots"),
            size: packed.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: true,
        });
        slots
            .slice(..)
            .get_mapped_range_mut()
            .expect("mapped")
            .copy_from_slice(&packed);
        slots.unmap();
        Cdf53Grid {
            slots,
            planes_copy: make("ghostframe-cdf53-grid-planes", PLANES_SRC, true),
            // The reconstruct shader declares 12 KB of workgroup memory and
            // writes all of it before reading any, so wgpu's zero-fill is pure
            // cost -- measured at ~80 us per tile on Mali-T860, four fifths of
            // the whole stage. The one variable that needs a zero start
            // (`any_magnitude`) is initialised by hand.
            reconstruct: make("ghostframe-cdf53-grid-reconstruct", reconstruct, false),
            limit_override: None,
            cols: 0,
            rows: 0,
            planes: None,
            frame: Grown::new(device, "ghostframe-cdf53-grid-frame"),
            planes_bind: None,
            reconstruct_bind: None,
            tile_words: Vec::new(),
            plane_rows: Vec::new(),
            plane_data: Vec::new(),
            fb_size: (0, 0),
            recorded: false,
            seen_tiles: HashSet::new(),
            seen_planes: HashSet::new(),
        }
    }

    /// Allocate the plane store for a `cols` x `rows` tile grid (~11 MB at
    /// 1080p). A no-op for the same shape. The store starts zeroed and every
    /// plane in it is stale until a plane writes it.
    ///
    /// A grid whose store exceeds the device's storage binding limit (128 MiB
    /// on the GLES devices this was measured on: about 8K) is refused with
    /// [`GridTooLarge`]. The grid is then unusable ([`Self::is_usable`] is
    /// false, [`Self::record`] records nothing) until a later `resize`
    /// succeeds, so a caller can fall back to the CPU decoder.
    pub fn resize(
        &mut self,
        device: &wgpu::Device,
        cols: u32,
        rows: u32,
    ) -> Result<(), GridTooLarge> {
        if self.planes.is_some() && self.cols == cols && self.rows == rows {
            return Ok(());
        }
        let bytes = cols as u64 * rows as u64 * TILE_WORDS * 4;
        let limits = device.limits();
        let limit = self.limit_override.unwrap_or_else(|| {
            limits
                .max_storage_buffer_binding_size
                .min(limits.max_buffer_size)
        });
        self.planes_bind = None;
        self.reconstruct_bind = None;
        if bytes > limit {
            self.planes = None;
            self.cols = 0;
            self.rows = 0;
            return Err(GridTooLarge { bytes, limit });
        }
        self.cols = cols;
        self.rows = rows;
        self.planes = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-cdf53-grid-planes"),
            size: bytes.max(4),
            // COPY_SRC/COPY_DST are for tests that seed and read the store back.
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        Ok(())
    }

    /// Pretend the device's binding limit is `bytes` (takes effect at the next
    /// [`Self::resize`]), to exercise the too-large path at a size a test can
    /// afford. Not for production.
    #[doc(hidden)]
    pub fn set_limit_for_test(&mut self, bytes: Option<u64>) {
        self.limit_override = bytes;
    }

    /// Whether a plane store exists, i.e. the last [`Self::resize`] succeeded.
    pub fn is_usable(&self) -> bool {
        self.planes.is_some()
    }

    /// Tile grid columns the store was sized for.
    pub fn cols(&self) -> u32 {
        self.cols
    }

    /// The plane store. Exposed for tests that seed it with sentinels or read
    /// it back; nothing outside the stages should write it.
    pub fn plane_store(&self) -> &wgpu::Buffer {
        self.planes.as_ref().expect("resize before use")
    }

    /// Start a frame: forget the previous one's tables, keep the capacity.
    pub fn begin_frame(&mut self) {
        self.tile_words.clear();
        self.plane_rows.clear();
        self.plane_data.clear();
        self.recorded = false;
        self.seen_tiles.clear();
        self.seen_planes.clear();
    }

    /// Queue one decoded plane (`pass_idx` 0..14 of `tile_index`, row-major in
    /// the grid): 384 bytes, the B, G, R channel planes of 128 bytes each, as
    /// `prevalidate_cdf53` makes them.
    pub fn push_plane(&mut self, tile_index: u32, pass_idx: u8, plane: &[u8]) {
        assert_eq!(plane.len(), PLANE_BYTES, "a plane is 3 x 128 bytes");
        debug_assert!(
            self.seen_planes.insert((tile_index, pass_idx)),
            "tile {tile_index} pass {pass_idx} given twice in one frame"
        );
        self.plane_rows
            .extend_from_slice(&[tile_index, pass_idx as u32]);
        self.plane_data.extend_from_slice(plane);
    }

    /// List `tile_index` for stage B. `plane_mask` bit p says plane p holds this
    /// generation's data; `k` is the resolved prefix length, 1..=14, counting
    /// planes that were never sent. A prefix whose coefficients are all zero is
    /// not drawn unless `k` is 14. Mask bits at or beyond `k` are ignored by the
    /// shader.
    pub fn push_tile(&mut self, tile_index: u32, plane_mask: u32, k: u32) {
        debug_assert!(k <= 14, "prefix length {k} > 14");
        debug_assert!(
            self.seen_tiles.insert(tile_index),
            "tile {tile_index} listed twice in one frame"
        );
        self.tile_words
            .extend_from_slice(&[tile_index, (plane_mask & 0xFFFF) | (k.min(0xFFFF) << 16)]);
    }

    /// Planes queued so far this frame.
    pub fn plane_count(&self) -> u32 {
        (self.plane_rows.len() / 2) as u32
    }

    /// Tiles queued so far this frame.
    pub fn tile_count(&self) -> u32 {
        (self.tile_words.len() / 2) as u32
    }

    /// Bytes [`Self::upload`] will write.
    pub fn upload_bytes(&self) -> usize {
        4 * (HEADER_WORDS + self.tile_words.len() + self.plane_rows.len()) + self.plane_data.len()
    }

    /// Copy the frame to the GPU in one queue write. Separate from the
    /// `record_*` calls so a benchmark can time the stages without the upload.
    /// `fb_size` is the framebuffer the tiles will be clipped to.
    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, fb_size: (u32, u32)) {
        self.fb_size = fb_size;
        let total = self.upload_bytes() as u64;
        if self
            .frame
            .reserve(device, "ghostframe-cdf53-grid-frame", total)
        {
            self.planes_bind = None;
            self.reconstruct_bind = None;
        }
        // Section offsets, in words.
        let tiles_off = HEADER_WORDS;
        let rows_off = tiles_off + self.tile_words.len();
        let data_off = rows_off + self.plane_rows.len();
        let mut header = [0u32; HEADER_WORDS];
        header[..8].copy_from_slice(&[
            self.cols,
            fb_size.0,
            fb_size.1,
            self.tile_count(),
            self.plane_count(),
            tiles_off as u32,
            rows_off as u32,
            data_off as u32,
        ]);
        // Word 15 stays zero; the reconstruct shader reads it to hide a loop
        // bound from the compiler (see the shader).
        let Some(mut view) = queue.write_buffer_with(
            &self.frame.buffer,
            0,
            wgpu::BufferSize::new(total).expect("the header is never empty"),
        ) else {
            // A validation failure; wgpu has reported it.
            return;
        };
        let mut at = 0usize;
        for words in [&header[..], &self.tile_words, &self.plane_rows] {
            for w in words {
                // Little-endian, as the shaders read them.
                view.slice(at..at + 4).copy_from_slice(&w.to_le_bytes());
                at += 4;
            }
        }
        view.slice(at..at + self.plane_data.len())
            .copy_from_slice(&self.plane_data);
    }

    /// Stage A: copy the frame's decoded planes into the plane store.
    pub fn record_planes(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder) {
        let n = self.plane_count();
        if n == 0 || self.planes.is_none() {
            return;
        }
        debug_assert!(n <= MAX_WORKGROUPS, "{n} planes in one frame");
        let planes = self.planes.as_ref().expect("checked");
        let frame = &self.frame.buffer;
        let layout = self.planes_copy.get_bind_group_layout(0);
        let bind = self.planes_bind.get_or_insert_with(|| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ghostframe-cdf53-grid-planes-bind"),
                layout: &layout,
                entries: &[entry(0, frame), entry(1, planes)],
            })
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("ghostframe-cdf53-grid-planes-pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.planes_copy);
        pass.set_bind_group(0, &*bind, &[]);
        pass.dispatch_workgroups(n, 1, 1);
    }

    /// Stage B: reconstruct the frame's listed tiles into `fb`. Tiles at the
    /// right or bottom edge are clipped to the framebuffer.
    pub fn record_reconstruct(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        fb: &Framebuffer,
    ) {
        let n = self.tile_count();
        if n == 0 || self.planes.is_none() {
            return;
        }
        debug_assert!(n <= MAX_WORKGROUPS, "{n} tiles in one frame");
        let planes = self.planes.as_ref().expect("checked");
        if self
            .reconstruct_bind
            .as_ref()
            .is_none_or(|(epoch, _)| *epoch != fb.epoch())
        {
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ghostframe-cdf53-grid-reconstruct-bind"),
                layout: &self.reconstruct.get_bind_group_layout(0),
                entries: &[
                    entry(0, planes),
                    entry(1, &self.frame.buffer),
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(fb.view()),
                    },
                    entry(3, &self.slots),
                ],
            });
            self.reconstruct_bind = Some((fb.epoch(), group));
        }
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("ghostframe-cdf53-grid-reconstruct-pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.reconstruct);
        pass.set_bind_group(0, &self.reconstruct_bind.as_ref().expect("just set").1, &[]);
        pass.dispatch_workgroups(n, 1, 1);
    }

    /// Release the framebuffer the cached bind group holds, after the
    /// framebuffer was replaced.
    pub fn drop_framebuffer_cache(&mut self) {
        self.reconstruct_bind = None;
    }

    /// Upload the frame and record the stages (planes, RLE, reconstruct) into
    /// `encoder`. Returns whether anything was recorded; false (and no upload)
    /// for an empty frame or an unusable grid. See the module's host contract:
    /// one `record` per `begin_frame` and per submit.
    pub fn record(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        fb: &Framebuffer,
    ) -> bool {
        debug_assert!(!self.recorded, "record() twice without begin_frame()");
        let work = self.tile_count() + self.plane_count();
        if work == 0 || self.planes.is_none() {
            return false;
        }
        self.recorded = true;
        self.upload(device, queue, (fb.width, fb.height));
        self.record_planes(device, encoder);
        self.record_reconstruct(device, encoder, fb);
        true
    }
}

/// A grid that passed [`probe`], with what the probe measured.
pub struct Probed {
    pub grid: Cdf53Grid,
    /// GPU microseconds per tile at a complete prefix (k = 14) on this device,
    /// from a 256-tile dispatch with the empty-submit overhead taken off.
    pub us_per_tile_k14: f64,
}

/// Build a grid, prove it works on this device, and measure what it costs.
///
/// Everything runs inside wgpu error scopes (validation, internal and
/// out-of-memory), so a pipeline that fails to build -- a shader the GL
/// backend cannot compile, say -- comes back as `Err` instead of reaching
/// wgpu's default uncaptured-error handler, which panics the render thread.
/// The caller then decodes on the CPU.
///
/// Blocks on the device; call it once, at start-up, on the render thread. The
/// grid it returns is the one that was tested: hand it to the renderer rather
/// than building another (the reconstruct shader is slow to compile on
/// panfrost).
pub fn probe(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Probed, String> {
    probe_with(device, queue, Cdf53Grid::new)
}

/// [`probe`] with the grid made by `make` (a test that must see it fail).
pub fn probe_with(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    make: impl FnOnce(&wgpu::Device) -> Cdf53Grid,
) -> Result<Probed, String> {
    in_error_scopes(device, || {
        let mut grid = make(device);
        self_test_with(device, queue, &mut grid)?;
        let us_per_tile_k14 = calibrate(device, queue, &mut grid)?;
        Ok(Probed {
            grid,
            us_per_tile_k14,
        })
    })
}

/// Run `f` with out-of-memory, internal and validation errors captured, and
/// report the first one as `Err`.
fn in_error_scopes<T>(
    device: &wgpu::Device,
    f: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let oom = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
    let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let result = f();
    // Last in, first out.
    let errors = [
        pollster::block_on(validation.pop()),
        pollster::block_on(internal.pop()),
        pollster::block_on(oom.pop()),
    ];
    if let Some(e) = errors.into_iter().flatten().next() {
        return Err(format!("GPU error: {e}"));
    }
    result
}

/// One tile as the wire carries it, decoded: the planes that exist, and the
/// `present_passes` bitmap.
struct Fixture {
    present: u16,
    planes: Vec<Option<Vec<u8>>>,
}

/// Two known pictures: a smooth ramp with an edge, and noise.
fn fixtures() -> Vec<Fixture> {
    use ghostframe_protocol::codec::cdf53 as codec;
    let tile = |kind: u32| -> Vec<u8> {
        let mut rng = 0x2545_F491u32;
        let mut out = Vec::with_capacity(4096);
        for y in 0..32u32 {
            for x in 0..32u32 {
                rng = rng.wrapping_mul(48271).wrapping_add(1);
                let n = (rng >> 24) as u8;
                let px = if kind == 0 {
                    let edge = if x > 15 { 90 } else { 0 };
                    [(x * 5 + edge) as u8, (y * 6 + x) as u8, (200 - x - y) as u8]
                } else {
                    [n, n.wrapping_mul(3), n.wrapping_add(77)]
                };
                out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
        }
        out
    };
    (0..2)
        .map(|kind| {
            let coeffs = codec::forward(&tile(kind));
            let (present, passes) = codec::encode_passes_sparse(&coeffs);
            let mut planes: Vec<Option<Vec<u8>>> = vec![None; 14];
            for (p, payload) in passes {
                let mut at = if p == 0 { 2 } else { 0 };
                let mut plane = Vec::with_capacity(PLANE_BYTES);
                for _ in 0..3 {
                    let len = u16::from_be_bytes([payload[at], payload[at + 1]]) as usize;
                    plane.extend(codec::rle_decode(&payload[at + 2..at + 2 + len]));
                    at += 2 + len;
                }
                planes[p as usize] = Some(plane);
            }
            Fixture { present, planes }
        })
        .collect()
}

/// Run `grid` once against the CPU reference on this device: two known tiles,
/// drawn complete (frame 1) and then one of them again at a partial prefix
/// (frame 2), read back and compared to `decode_planes` + `inverse`. The
/// reconstruct shader is sensitive to register allocation on panfrost -- a
/// build that tips over it silently writes nothing -- so the grid is not
/// trusted on a device until this passes. The partial frame is checked to
/// differ from the complete one, so a shader that draws nothing on it cannot
/// pass by leaving frame 1's picture in place. Errors name the frame.
pub fn self_test_with(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    grid: &mut Cdf53Grid,
) -> Result<(), String> {
    use ghostframe_protocol::codec::cdf53 as codec;
    let cases = fixtures();
    grid.resize(device, 2, 1).map_err(|e| e.to_string())?;
    let fb = Framebuffer::new(device, 64, 32);
    // (tile, prefix length): both complete, then the first at a partial prefix.
    let frames: [&[(usize, u32)]; 2] = [&[(0, 14), (1, 14)], &[(0, 11)]];
    let mut previous: [Option<Vec<u8>>; 2] = [None, None];
    for (n, frame) in frames.iter().enumerate() {
        grid.begin_frame();
        let mut want = Vec::new();
        for &(i, k) in *frame {
            let c = &cases[i];
            let mask = c.present as u32 & ((1u32 << k) - 1);
            for p in 0..k as usize {
                if let Some(plane) = &c.planes[p] {
                    grid.push_plane(i as u32, p as u8, plane);
                }
            }
            grid.push_tile(i as u32, mask, k);
            let zero = vec![0u8; PLANE_BYTES];
            let refs: Vec<&[u8]> = (0..k as usize)
                .map(|p| c.planes[p].as_deref().unwrap_or(&zero))
                .collect();
            let coeffs = codec::decode_planes(&refs);
            if k < 14 && coeffs.iter().all(|&c| c == 0) {
                // The grid would (rightly) not draw this.
                return Err(format!("fixture: tile {i} at prefix {k} is all zero"));
            }
            let bgr = codec::inverse(&coeffs);
            if previous[i].as_ref() == Some(&bgr) {
                return Err(format!(
                    "fixture: frame {} would leave tile {i} unchanged",
                    n + 1
                ));
            }
            previous[i] = Some(bgr.clone());
            want.push((i, bgr));
        }
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-cdf53-self-test"),
        });
        if !grid.record(device, queue, &mut enc, &fb) {
            return Err("nothing was recorded".into());
        }
        queue.submit(std::iter::once(enc.finish()));
        let got = fb.debug_read(device, queue);
        for (i, bgr) in want {
            for y in 0..32usize {
                for x in 0..32usize {
                    let o = (y * 64 + i * 32 + x) * 4;
                    let w = &bgr[(y * 32 + x) * 3..][..3];
                    if got[o..o + 4] != [w[2], w[1], w[0], 0xFF] {
                        return Err(format!(
                            "frame {} tile {i} pixel ({x}, {y}): got {:?}, want {:?}",
                            n + 1,
                            &got[o..o + 4],
                            [w[2], w[1], w[0], 0xFF]
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Tiles in the calibration dispatch: enough that the fixed cost of a submit
/// is a small part of it (at 64 tiles it inflated the per-tile figure by 40%
/// on a Mali-T860), few enough to take ~15 ms there.
const CALIBRATION_TILES: u32 = 256;

/// GPU microseconds per complete tile on this device: the wall time of
/// submit + wait for a [`CALIBRATION_TILES`]-tile frame of known content,
/// minus that of an empty submit, median of five after a warm-up. Wall time
/// includes the driver's own overhead, which is what a flush pays too.
fn calibrate(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    grid: &mut Cdf53Grid,
) -> Result<f64, String> {
    use std::time::Instant;
    let fixture = &fixtures()[0];
    grid.resize(device, 16, 16).map_err(|e| e.to_string())?;
    let fb = Framebuffer::new(device, 16 * 32, 16 * 32);
    let wait = || device.poll(wgpu::PollType::wait_indefinitely());
    let timed = |f: &mut dyn FnMut() -> Result<(), String>| -> Result<f64, String> {
        let t = Instant::now();
        f()?;
        wait().map_err(|e| e.to_string())?;
        Ok(Instant::now().duration_since(t).as_secs_f64() * 1e6)
    };
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    };
    let mut empty = || {
        let enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        queue.submit(std::iter::once(enc.finish()));
        Ok(())
    };
    let mut full = || {
        grid.begin_frame();
        for i in 0..CALIBRATION_TILES {
            for (p, plane) in fixture.planes.iter().enumerate() {
                if let Some(plane) = plane {
                    grid.push_plane(i, p as u8, plane);
                }
            }
            grid.push_tile(i, fixture.present as u32, 14);
        }
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-cdf53-calibration"),
        });
        if !grid.record(device, queue, &mut enc, &fb) {
            return Err("calibration recorded nothing".to_string());
        }
        queue.submit(std::iter::once(enc.finish()));
        Ok(())
    };
    timed(&mut full)?; // warm-up: first use compiles and binds
    let mut base = Vec::new();
    let mut work = Vec::new();
    for _ in 0..5 {
        base.push(timed(&mut empty)?);
        work.push(timed(&mut full)?);
    }
    let per_tile = (median(work) - median(base)) / CALIBRATION_TILES as f64;
    Ok(per_tile.clamp(0.5, 500.0))
}

fn entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reconstruct shader writes the 1024 coefficients of a channel into
    /// 1024 work-area slots and reads all 1024 back, never clearing the area:
    /// that it never reads a slot it did not write rests on the slot table
    /// being a permutation of 0..1024.
    #[test]
    fn coefficient_slots_are_a_permutation_of_the_work_area() {
        let slots = coefficient_slots();
        assert_eq!(slots.len(), 1024);
        let mut seen = [false; 1024];
        for &s in &slots {
            assert!((s as usize) < 1024, "slot {s} outside the 32x32 work area");
            assert!(!seen[s as usize], "slot {s} assigned twice");
            seen[s as usize] = true;
        }
        assert!(seen.iter().all(|&b| b), "a work-area slot is never written");
    }

    #[test]
    fn phys_is_a_bijection_per_level() {
        // Level n lifts lines of 4 << n samples (n = 1: the 8x8 level).
        for n in 1..=3u32 {
            let len = 4u32 << n;
            let mut seen = vec![false; len as usize];
            for y in 0..len {
                let p = phys(n, y) as usize;
                assert!(p < seen.len() && !seen[p], "level {n}: {y} -> {p}");
                seen[p] = true;
            }
        }
    }
}
