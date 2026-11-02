//! The export ring: N GPU-exported dmabuf buffers, filled from the private
//! [`Framebuffer`] with history-aware partial blits.
//!
//! ## Why "since this buffer was last written", not "since the last frame"
//!
//! The host holds an exported buffer until it has finished presenting it,
//! so by the time a buffer comes back to us for reuse it may be several
//! generations stale -- not just one frame behind. If `publish` copied only
//! the *current* frame's damage into whichever buffer happened to be free,
//! a buffer that missed two frames of updates would come back to the host
//! still showing pixels from three frames ago in every region that did not
//! change on this exact frame. That is a much harder bug to notice than a
//! visibly wrong frame: it looks almost right.
//!
//! [`crate::dirty::DirtyHistory`] is what makes the correct answer possible:
//! each [`ExportBuffer`] remembers the generation it was last filled at
//! (`filled_at_gen`), and `publish` asks for the union of every generation
//! sealed *after* that watermark, regardless of how many frames that spans.
//! A buffer that has never been filled (`filled_at_gen == None`) forces a
//! full-surface blit, since there is no watermark to diff from.

use crate::coalesce::{coalesce, Rect};
use crate::dirty::DirtyHistory;
use crate::export::{ExportedImage, PlaneLayout};
use crate::export_copy::{prefers_blit, ExportCopy};
#[cfg(feature = "vulkan")]
use crate::fence::ExportFence;
use crate::framebuffer::Framebuffer;
use crate::wgpu_ctx::WgpuContext;
use crate::GpuError;
use ghostframe_protocol::tile::TILE_SIZE;
use std::collections::HashMap;

/// How many sealed generations [`DirtyHistory`] keeps before the oldest is
/// evicted and `union_since` must fall back to a full blit. Generous
/// relative to the ring's buffer count: a buffer held by the host across
/// many publishes should still be recoverable with a partial blit rather
/// than repeatedly paying for full-surface copies. A `DirtyGrid` is a
/// handful of `u64` words, so keeping many of them costs little.
const HISTORY_CAPACITY: usize = 256;

/// One exported buffer plus the ring's bookkeeping for it.
/// Field order is drop order and it matters: `storage` holds an alias of
/// `texture`'s GL texture on GLES, so it goes first, and `texture` before
/// `exported`, whose fd outlives both.
pub struct ExportBuffer {
    /// A view the compute export copy writes through, and the texture it
    /// belongs to (which must outlive it). `None` where this export cannot be
    /// bound as a storage image.
    pub storage: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// The default view of `texture`, made once: the blit renders into it
    /// every publish.
    pub view: wgpu::TextureView,
    pub texture: wgpu::Texture,
    pub exported: ExportedImage,
    /// Generation this buffer's contents correspond to. `None` = never
    /// filled, which forces a full blit.
    pub filled_at_gen: Option<u64>,
    /// Held by the host until released.
    pub in_flight: bool,
}

/// Damage between a published frame and the one published just before it.
///
/// Only meaningful to a consumer that actually showed `frame_id`: if frames
/// were published but never presented, the damage of those is not in here,
/// and the consumer must treat everything as changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SincePrevious {
    /// `frame_id` of the previously published frame.
    pub frame_id: u32,
    /// Changed area in PIXEL coordinates, possibly empty.
    pub damage: Vec<Rect>,
}

/// What [`ExportRing::publish`] hands back.
///
/// Carries a snapshot of the published buffer's dmabuf handle alongside the
/// ring bookkeeping (`frame_id`/`buffer_id`/`damage`) so a consumer several
/// layers away (`ghostframe-client-capi`'s `gf_client_acquire_frame`) can
/// fill a `gf_frame` without a second round-trip back into the render
/// thread -- there is no other thread-safe way to reach the `Renderer`
/// that owns the underlying `ExportedImage`. `fd`/`planes`/`modifier` are
/// exactly [`ExportedImage::raw_fd`]/`planes`/`modifier` for
/// `buffer_id`, read at the moment of publish; the fd itself is owned by
/// the `ExportedImage` (i.e. by this crate), not by this struct or its
/// eventual caller. It stays open until [`ExportRing::release`] of this
/// frame's `frame_id` -- including across an [`ExportRing::resize`] -- or
/// until the ring is dropped, whichever comes first.
pub struct PublishedFrame {
    pub frame_id: u32,
    pub buffer_id: u32,
    /// Damage in PIXEL coordinates, ready for the host.
    ///
    /// Relative to this *buffer's* previous contents, which is what a
    /// consumer that copies the whole buffer needs. A consumer that keeps
    /// a long-lived surface and updates it from successive frames wants
    /// [`PublishedFrame::since_previous`] instead.
    pub damage: Vec<Rect>,
    /// What changed since the previously published frame (not the previous
    /// frame *this buffer* held), for a consumer whose on-screen content is
    /// that previous frame. `None` when that cannot be said: the first
    /// frame, the first after a resize, or damage history already evicted.
    pub since_previous: Option<SincePrevious>,
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    /// The dmabuf fd backing this buffer. Owned by the library's
    /// `ExportedImage`, not by this struct -- do not close it. Valid until
    /// this frame is released or the ring is dropped.
    pub fd: i32,
    pub planes: Vec<PlaneLayout>,
}

/// How a publish copied its damage into the export buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyPath {
    /// One render pass with a scissored triangle per damage rect.
    Blit,
    /// The compute kernel over exactly the damaged tiles.
    Kernel,
}

/// A publish decided but not yet carried out: the buffer to fill, what to
/// copy into it, and the history to commit once the copy has run. Made by
/// [`ExportRing::plan_publish`], recorded by [`ExportRing::encode_blit`], and
/// committed by [`ExportRing::finish_publish`] -- split so the copy can ride in
/// the frame's one submit instead of costing a second.
pub(crate) struct PublishPlan {
    buffer_id: u32,
    gen: u64,
    pixel_rects: Vec<Rect>,
    /// The same damage as `pixel_rects`, as the tiles it is made of.
    tiles: Vec<(u32, u32)>,
    since_previous: Option<SincePrevious>,
}

impl ExportBuffer {
    fn new(device: &wgpu::Device, exported: ExportedImage) -> Result<Self, GpuError> {
        let texture = exported.as_wgpu_texture(device)?;
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let storage = exported.storage_texture(device, &texture).map(|t| {
            let view = t.create_view(&wgpu::TextureViewDescriptor::default());
            (t, view)
        });
        Ok(ExportBuffer {
            exported,
            texture,
            view,
            storage,
            filled_at_gen: None,
            in_flight: false,
        })
    }
}

/// A ring of exported dmabuf buffers, filled with history-aware partial
/// blits from a shared private [`Framebuffer`].
pub struct ExportRing {
    buffers: Vec<ExportBuffer>,
    /// Vulkan: makes the sync_file a publish imports into its buffer's
    /// reservation. `None` when this device cannot (or stopped being able to),
    /// in which case a publish waits for the GPU.
    #[cfg(feature = "vulkan")]
    fence: Option<ExportFence>,
    /// Whether the next submit carries the fence's signal; set by
    /// [`ExportRing::arm_fence`], consumed by [`ExportRing::finish_publish`].
    #[cfg(feature = "vulkan")]
    fence_armed: bool,
    /// Wait for the GPU after every publish (`GHOSTFRAME_BLOCKING_PUBLISH`, or
    /// [`ExportRing::force_blocking_publish`]).
    blocking: bool,
    /// The compute export copy, when every buffer can be bound as a storage
    /// image and nothing asked for the render-pass blit. `None` means every
    /// publish takes the blit.
    copy: Option<ExportCopy>,
    /// Take the kernel even for damage [`prefers_blit`] would send to the
    /// blit. Benchmarks only.
    always_compute: bool,
    /// What the most recent publish that recorded a copy used.
    last_copy: Option<CopyPath>,
    history: DirtyHistory,
    next_frame_id: u32,
    /// `(frame_id, generation)` of the last successful publish. `None` at
    /// start and after a resize, which drops the history it indexes.
    last_publish: Option<(u32, u64)>,
    /// Maps an outstanding `frame_id` to the buffer it was published into,
    /// so `release` can find it without a linear scan tied to buffer state
    /// (and so an unknown `frame_id` is trivially "not found" rather than
    /// requiring buffer-shape assumptions to detect).
    in_flight: HashMap<u32, u32>,
    /// Buffers retired by [`ExportRing::resize`] while the host still held
    /// them, keyed by the `frame_id` they were published under.
    ///
    /// The host was handed that buffer's dmabuf fd and the C contract says it
    /// stays valid until the matching `release`. Closing it on resize would
    /// leave the host importing a fd number the kernel is free to hand to the
    /// next `open` -- fd confusion, triggerable by whoever controls the
    /// server's resolution. So a retired buffer lives here until `release`
    /// (or the ring's own drop) and only then closes.
    graveyard: HashMap<u32, ExportBuffer>,
    width: u32,
    height: u32,
    /// Kept so [`ExportRing::resize`] can reallocate with the same
    /// consumer preference the ring was constructed with.
    preferred_modifiers: Vec<u64>,
    host_visible: bool,
}

impl ExportRing {
    pub fn new(
        ctx: &WgpuContext,
        width: u32,
        height: u32,
        count: usize,
        preferred_modifiers: &[u64],
        // `host_visible`: allocate export buffers in CPU-mappable memory so
        // `ExportedImage::map_read` works. Diagnostic only -- production
        // passes false, because demanding host visibility can pin every
        // export to a small BAR aperture (256 MiB here, against 7.75 GiB of
        // CPU-invisible VRAM) for a readback a real consumer never performs.
        host_visible: bool,
    ) -> Result<Self, GpuError> {
        let mut buffers = Vec::with_capacity(count);
        for _ in 0..count {
            let exported =
                ExportedImage::new(ctx, width, height, preferred_modifiers, host_visible)?;
            buffers.push(ExportBuffer::new(&ctx.device, exported)?);
        }
        let config = crate::config::PublishConfig::from_env();
        let copy = Self::make_copy(ctx, &buffers, config.render_pass_export, (width, height));

        let (cols, rows) = tile_grid(width, height);

        #[cfg(feature = "vulkan")]
        let fence = match ExportFence::new(ctx) {
            Ok(fence) => Some(fence),
            Err(why) => {
                tracing::info!(
                    %why,
                    "export fences unavailable; every publish will wait for the GPU"
                );
                None
            }
        };

        Ok(ExportRing {
            buffers,
            #[cfg(feature = "vulkan")]
            fence,
            #[cfg(feature = "vulkan")]
            fence_armed: false,
            blocking: config.blocking,
            copy,
            always_compute: false,
            last_copy: None,
            history: DirtyHistory::new(cols, rows, HISTORY_CAPACITY),
            next_frame_id: 0,
            last_publish: None,
            in_flight: HashMap::new(),
            graveyard: HashMap::new(),
            width,
            height,
            preferred_modifiers: preferred_modifiers.to_vec(),
            host_visible,
        })
    }

    /// The compute copy for `buffers`, or `None` (with the reason logged) when
    /// the blit has to be used.
    fn make_copy(
        ctx: &WgpuContext,
        buffers: &[ExportBuffer],
        forced_off: bool,
        (width, height): (u32, u32),
    ) -> Option<ExportCopy> {
        if forced_off {
            tracing::info!("GHOSTFRAME_NO_COMPUTE_EXPORT set; exports use the render-pass blit");
            return None;
        }
        if buffers.is_empty() || buffers.iter().any(|b| b.storage.is_none()) {
            tracing::info!(
                "export buffers cannot be bound as storage images; \
                 exports use the render-pass blit"
            );
            return None;
        }
        let mut copy = ExportCopy::new(&ctx.device, buffers.len());
        if !Self::kernel_writes_the_export(ctx, &mut copy, &buffers[0], width, height) {
            tracing::warn!(
                "the compute export copy did not write the export buffer; \
                 exports use the render-pass blit"
            );
            return None;
        }
        Some(copy)
    }

    /// Run the kernel once on a known pixel and read the export back.
    ///
    /// Binding an EGLImage-backed texture as a storage image is something GLES
    /// 3.1 only promises for immutable textures, and `glEGLImageTargetTexture2DOES`
    /// makes a mutable one; Mesa accepts it, another driver may silently drop
    /// the stores. A check of the thing itself is simpler and more robust than
    /// guessing from the driver's name. Costs one tiny submit at creation; the
    /// buffer is then dirty garbage, which is fine because a buffer that has
    /// never been published is always filled whole first.
    fn kernel_writes_the_export(
        ctx: &WgpuContext,
        copy: &mut ExportCopy,
        buf: &ExportBuffer,
        width: u32,
        height: u32,
    ) -> bool {
        let Some((_, view)) = &buf.storage else {
            return false;
        };
        let mut fb = Framebuffer::new(&ctx.device, width.min(32), height.min(32));
        fb.debug_fill(&ctx.device, &ctx.queue, [0x11, 0x22, 0x33, 0x44]);
        let readback = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ghostframe-export-copy-selftest"),
            size: 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ghostframe-export-copy-selftest"),
            });
        copy.encode(
            &ctx.device,
            &ctx.queue,
            &mut encoder,
            &fb,
            (0, view),
            &[(0, 0)],
        );
        encoder.copy_texture_to_buffer(
            buf.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: None,
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        crate::submit::submit(&ctx.queue, encoder);
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        if ctx
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .is_err()
        {
            return false;
        }
        // The export is BGRA, the framebuffer pixel RGBA.
        let ok = slice
            .get_mapped_range()
            .is_ok_and(|bytes| bytes[..] == [0x33, 0x22, 0x11, 0x44]);
        // The cached groups hold the throwaway framebuffer's view.
        copy.drop_export_cache(copy.buffer_count());
        ok
    }

    /// Whether publishes copy damage with the compute kernel rather than the
    /// render-pass blit.
    pub fn uses_compute_copy(&self) -> bool {
        self.copy.is_some()
    }

    pub fn mark_dirty(&mut self, tx: u32, ty: u32) {
        self.history.current_mut().set(tx, ty);
    }

    pub fn mark_dirty_all(&mut self) {
        let (cols, rows) = tile_grid(self.width, self.height);
        let current = self.history.current_mut();
        for ty in 0..rows {
            for tx in 0..cols {
                current.set(tx, ty);
            }
        }
    }

    pub fn buffer(&self, buffer_id: u32) -> &ExportedImage {
        &self.buffers[buffer_id as usize].exported
    }

    /// Fill a free buffer from `fb` and hand it out, in a submit of its own.
    /// [`Renderer::flush_and_publish`](crate::renderer::Renderer::flush_and_publish)
    /// is the form a frame uses.
    ///
    /// Returns `None` when every buffer is held by the host. The library
    /// keeps decoding into its private framebuffer; nothing stalls and
    /// nothing is lost, because damage accumulates in the history and the
    /// next released buffer picks up everything since it was written.
    pub fn publish(&mut self, ctx: &WgpuContext, fb: &Framebuffer) -> Option<PublishedFrame> {
        let plan = self.plan_publish(fb)?;
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ghostframe-export-publish"),
            });
        let recorded = self.encode_blit(&ctx.device, &ctx.queue, &mut encoder, fb, &plan);
        if recorded {
            self.arm_fence(ctx);
            crate::submit::submit(&ctx.queue, encoder);
        }
        Some(self.finish_publish(ctx, plan, recorded))
    }

    /// First half of a publish: seal the generation and choose the buffer and
    /// the rectangles to copy. `None` when every buffer is held by the host.
    pub(crate) fn plan_publish(&mut self, fb: &Framebuffer) -> Option<PublishPlan> {
        // Seal the current generation *before* checking for a free buffer.
        // `DirtyHistory` owns sealed generations independently of any
        // buffer's state, so sealing here never loses damage even if we
        // return `None` right after: the seal stays in `self.history` and
        // whatever buffer is released next will pick it up via
        // `union_since` on its own (older) watermark. The alternative --
        // checking for a free buffer first and only sealing once one is
        // found -- would instead leave this round's dirty tiles sitting in
        // `current`, silently merged into whatever *next* publish happens
        // to seal. Not incorrect by itself (the tiles would still end up in
        // some sealed generation eventually), but it ties the seal point to
        // "a buffer happened to be free" instead of "publish was called",
        // which is a harder invariant to reason about under an in-flight
        // ring that regularly returns `None`.
        let gen = self.history.advance();

        // TODO(vulkan): a free buffer is one the host has released, but the
        // host's last read of it may still be queued (PresentIdleNotify is sent
        // when the X server's copy is queued, not finished), and nothing here
        // makes the re-blit wait for it. The fix would be the dmabuf's
        // DMA_BUF_IOCTL_EXPORT_SYNC_FILE(WRITE) imported as a wait semaphore on
        // the next submit. GLES gets this ordering from the kernel's implicit sync.
        let buffer_id = self.buffers.iter().position(|b| !b.in_flight)? as u32;

        let (cols, rows) = tile_grid(self.width, self.height);
        let dirty = self
            .history
            .union_since(self.buffers[buffer_id as usize].filled_at_gen);
        let rects = match &dirty {
            Some(dirty) => coalesce(dirty),
            None => vec![Rect {
                x: 0,
                y: 0,
                w: cols,
                h: rows,
            }],
        };
        // Only the kernel reads the tile list; the blit works from rects.
        let tiles = match (&self.copy, &dirty) {
            (None, _) => Vec::new(),
            (Some(_), Some(dirty)) => dirty.iter_set().collect(),
            (Some(_), None) => (0..rows)
                .flat_map(|ty| (0..cols).map(move |tx| (tx, ty)))
                .collect(),
        };
        let pixel_rects: Vec<Rect> = rects
            .into_iter()
            .map(|r| r.to_pixels(fb.width, fb.height))
            .collect();

        // Against the previous publish rather than this buffer's watermark:
        // every buffer is complete, so what differs from the previous frame
        // is exactly the union of generations sealed since it.
        let prev_publish = self.last_publish;
        let since_previous = prev_publish.and_then(|(frame_id, prev_gen)| {
            let dirty = self.history.union_since(Some(prev_gen))?;
            Some(SincePrevious {
                frame_id,
                damage: coalesce(&dirty)
                    .into_iter()
                    .map(|r| r.to_pixels(fb.width, fb.height))
                    .collect(),
            })
        });

        Some(PublishPlan {
            buffer_id,
            gen,
            pixel_rects,
            tiles,
            since_previous,
        })
    }

    /// Second half, recorded into `encoder`: copy the planned rectangles into
    /// the planned buffer. Returns whether anything was recorded.
    pub(crate) fn encode_blit(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        fb: &Framebuffer,
        plan: &PublishPlan,
    ) -> bool {
        let id = plan.buffer_id as usize;
        let buf = &self.buffers[id];
        let kernel =
            !(plan.tiles.is_empty() || (!self.always_compute && prefers_blit(&plan.tiles)));
        if let (true, Some(copy), Some((alias, view))) = (kernel, &mut self.copy, &buf.storage) {
            let recorded = copy.encode(device, queue, encoder, fb, (id, view), &plan.tiles);
            if recorded {
                // GLES: a barrier after the image stores, for whatever reads
                // the buffer next through its other wgpu texture. See
                // `ExportedImage::storage_texture`. Vulkan has one texture,
                // whose tracker does this itself.
                #[cfg(feature = "gles")]
                encoder.transition_resources(
                    std::iter::empty(),
                    std::iter::once(wgpu::TextureTransition {
                        texture: alias,
                        selector: None,
                        state: wgpu::TextureUses::COLOR_TARGET,
                    }),
                );
                #[cfg(feature = "vulkan")]
                let _ = alias;
                self.last_copy = Some(CopyPath::Kernel);
            }
            return recorded;
        }
        let recorded =
            fb.encode_blit_rects(device, encoder, &buf.texture, &buf.view, &plan.pixel_rects);
        if recorded {
            self.last_copy = Some(CopyPath::Blit);
        }
        recorded
    }

    /// Which path the most recent recorded copy took; `None` before the first.
    pub fn last_copy_path(&self) -> Option<CopyPath> {
        self.last_copy
    }

    /// Call immediately before the submit that carries a planned publish, and
    /// only when that submit is going to happen: on Vulkan it queues the
    /// fence's semaphore for the next submit, which would otherwise signal
    /// something unrelated. Nothing to do on GLES.
    #[cfg(feature = "vulkan")]
    pub(crate) fn arm_fence(&mut self, ctx: &WgpuContext) {
        let Some(fence) = &self.fence else { return };
        match fence.arm(ctx) {
            Ok(()) => self.fence_armed = true,
            Err(e) => self.disable_fence(&e),
        }
    }

    #[cfg(feature = "gles")]
    pub(crate) fn arm_fence(&mut self, _ctx: &WgpuContext) {}

    #[cfg(feature = "vulkan")]
    fn disable_fence(&mut self, why: &GpuError) {
        tracing::warn!(
            error = %why,
            "export fence failed; publishes will wait for the GPU from now on"
        );
        self.fence = None;
        self.fence_armed = false;
    }

    /// Third half, once the copy has been submitted (`submitted`: it was):
    /// make the buffer safe to read and commit the plan to the ring's
    /// bookkeeping.
    ///
    /// **GLES does not wait, and does not need a fence.** The export buffers
    /// are GBM bos imported into Mesa, and Mesa lists every bo a batch touches
    /// in the submit's handles, so panfrost attaches the job's fence to the
    /// shared dmabuf's reservation at submit. Anything that honours implicit
    /// sync (the X server through DRI3, a compositor, `DMA_BUF_IOCTL_SYNC`) then
    /// waits for the blit by itself. Measured on Mali-T860 with no fence of our
    /// own: straight after a 1080p publish the reservation held two unsignalled
    /// job fences in 5 of 8 frames, and `DMA_BUF_IOCTL_SYNC(START)` blocked
    /// 7.5 ms before the buffer went idle. `GHOSTFRAME_BLOCKING_PUBLISH=1`
    /// restores the wait.
    ///
    /// **Vulkan has to supply the fence.** Memory exported from `vkAllocateMemory`
    /// carries no implicit fence on RADV or ANV, so `fence.rs` signals an
    /// exportable semaphore from the submit and the sync_file goes into the
    /// dmabuf's reservation with `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`. Where that
    /// cannot be done the publish waits for the GPU.
    pub(crate) fn finish_publish(
        &mut self,
        ctx: &WgpuContext,
        plan: PublishPlan,
        submitted: bool,
    ) -> PublishedFrame {
        let PublishPlan {
            buffer_id,
            gen,
            pixel_rects,
            since_previous,
            ..
        } = plan;
        {
            let buf = &mut self.buffers[buffer_id as usize];
            buf.filled_at_gen = Some(gen);
            buf.in_flight = true;
        }

        // Nothing was submitted, so nothing new is being written: whatever
        // last wrote this buffer is already in its reservation.
        if submitted {
            if self.publish_is_synced(ctx, buffer_id) {
                // Fires the completion callbacks of finished submits (upload
                // slab recycling) without waiting for any.
                let _ = ctx.device.poll(wgpu::PollType::Poll);
            } else {
                self.wait_for_gpu(&ctx.device);
            }
        }
        #[cfg(feature = "vulkan")]
        {
            self.fence_armed = false;
        }

        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);
        self.in_flight.insert(frame_id, buffer_id);
        self.last_publish = Some((frame_id, gen));

        let exported = &self.buffers[buffer_id as usize].exported;
        PublishedFrame {
            frame_id,
            buffer_id,
            damage: pixel_rects,
            since_previous,
            width: exported.width,
            height: exported.height,
            modifier: exported.modifier,
            fd: exported.raw_fd(),
            planes: exported.planes.clone(),
        }
    }

    /// Whether the buffer just submitted is safe to read without this thread
    /// waiting for the GPU.
    fn publish_is_synced(&mut self, ctx: &WgpuContext, buffer_id: u32) -> bool {
        if self.blocking {
            return false;
        }
        #[cfg(feature = "vulkan")]
        {
            self.fence_armed && self.attach_fence(ctx, buffer_id)
        }
        #[cfg(feature = "gles")]
        {
            let _ = (ctx, buffer_id);
            true
        }
    }

    /// Export the fence of the submit just made and import it into the buffer's
    /// dmabuf. `false` means the buffer has no fence and the caller must wait;
    /// the fence is then switched off, since whatever failed will fail again.
    #[cfg(feature = "vulkan")]
    fn attach_fence(&mut self, ctx: &WgpuContext, buffer_id: u32) -> bool {
        use std::os::fd::AsRawFd;
        let Some(fence) = &self.fence else {
            return false;
        };
        // Timed at `debug` level, like the blocking wait below: this is what
        // replaced it, and `fence_us` against `poll_us` is the comparison.
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real driver call, not a virtual-clock path"
        )]
        let start = std::time::Instant::now();
        let attached = fence.take(ctx).and_then(|sync_file| match sync_file {
            // The driver reports the work already done: nothing to import.
            None => Ok(()),
            Some(sync_file) => {
                let dmabuf = self.buffers[buffer_id as usize].exported.raw_fd();
                // SAFETY: `dmabuf` is the fd the buffer's `ExportedImage` owns
                // and keeps open; `sync_file` is an open descriptor. The kernel
                // takes its own reference to the fence, so `sync_file` closing
                // at the end of this arm is fine.
                unsafe {
                    crate::dmabuf::dma_buf_import_sync_file(
                        dmabuf,
                        crate::dmabuf::DMA_BUF_SYNC_WRITE as u32,
                        sync_file.as_raw_fd(),
                    )
                }
            }
        });
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real driver call, not a virtual-clock path"
        )]
        let fence_us = start.elapsed().as_micros() as u64;
        match attached {
            Ok(()) => {
                tracing::debug!(fence_us, "publish: export fence attached");
                true
            }
            Err(e) => {
                self.disable_fence(&e);
                false
            }
        }
    }

    /// Block until the GPU has finished everything submitted so far.
    fn wait_for_gpu(&self, device: &wgpu::Device) {
        // Timed at `debug` level so M2's pacing measurement (design doc §5)
        // can isolate this stall from everything else `publish` does,
        // without adding an unconditional cost to the hot path -- off by
        // default, on with `RUST_LOG=ghostframe_client_gpu::ring=debug`.
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real GPU stall for M2 §5's pacing decision, not a virtual-clock path"
        )]
        let poll_start = std::time::Instant::now();
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll after export publish");
        #[allow(
            clippy::disallowed_methods,
            reason = "measuring a real GPU stall for M2 §5's pacing decision, not a virtual-clock path"
        )]
        let poll_us = poll_start.elapsed().as_micros() as u64;
        tracing::debug!(poll_us, "publish: device.poll stall");
    }

    /// Make every publish from here on wait for the GPU, as it does where no
    /// fence can be made. For tests of that path.
    #[doc(hidden)]
    pub fn force_blocking_publish(&mut self) {
        self.blocking = true;
    }

    /// Copy damage with the render-pass blit from here on, as where the export
    /// cannot be bound as a storage image. For A/B tests and benchmarks.
    #[doc(hidden)]
    pub fn force_render_pass_export(&mut self) {
        self.copy = None;
    }

    /// Use the compute copy for every publish, whatever the damage looks like.
    /// For benchmarks, which want each mode on its own.
    #[doc(hidden)]
    pub fn force_compute_export(&mut self) {
        self.always_compute = true;
    }

    /// Whether a publish returns without waiting for the GPU: not asked to
    /// block, and (on Vulkan) a fence is available.
    pub fn publish_is_nonblocking(&self) -> bool {
        #[cfg(feature = "vulkan")]
        let fenced = self.fence.is_some();
        #[cfg(feature = "gles")]
        let fenced = true;
        fenced && !self.blocking
    }

    /// Release the buffer held by `frame_id` back to the free pool. An
    /// unknown `frame_id` (already released, or never issued) is logged and
    /// ignored rather than panicking -- a host bug in release bookkeeping
    /// should not be able to crash the client.
    pub fn release(&mut self, frame_id: u32) {
        // A frame published before the last resize: its buffer is in the
        // graveyard, not in `buffers`. Dropping it closes its fd -- the
        // moment the host says it is done with it, and not before.
        if self.graveyard.remove(&frame_id).is_some() {
            return;
        }
        match self.in_flight.remove(&frame_id) {
            Some(buffer_id) => {
                if let Some(buf) = self.buffers.get_mut(buffer_id as usize) {
                    buf.in_flight = false;
                }
            }
            None => {
                tracing::warn!(frame_id, "release of unknown frame_id ignored");
            }
        }
    }

    /// Number of retired buffers still held by the host. Test and
    /// diagnostic use.
    pub fn retired_in_flight(&self) -> usize {
        self.graveyard.len()
    }

    /// Reallocate every buffer at the new size and drop all history. A
    /// no-op when the size is unchanged.
    ///
    /// Buffers the host still holds (`in_flight`) are moved to the graveyard
    /// rather than dropped, so their fds stay open until `release`.
    /// `next_frame_id` is deliberately NOT reset: frame ids stay unique for
    /// the life of the ring, so a late `release` of a pre-resize id can never
    /// be mistaken for a post-resize frame (and the C shim's per-frame_id
    /// damage list can never be overwritten under a held pointer).
    pub fn resize(&mut self, ctx: &WgpuContext, width: u32, height: u32) -> Result<(), GpuError> {
        // The server repeats FrameDimensions (10 identical datagrams per
        // session, for loss resilience). Reallocating on each would retire
        // the buffers the host is presenting and force full reblits.
        if width == self.width && height == self.height {
            return Ok(());
        }
        let count = self.buffers.len();

        let mut new_buffers = Vec::with_capacity(count);
        for _ in 0..count {
            let exported = ExportedImage::new(
                ctx,
                width,
                height,
                &self.preferred_modifiers,
                self.host_visible,
            )?;
            new_buffers.push(ExportBuffer::new(&ctx.device, exported)?);
        }

        // Everything fallible is done; from here the swap cannot fail.
        let old: Vec<ExportBuffer> = std::mem::replace(&mut self.buffers, new_buffers);
        let mut old: Vec<Option<ExportBuffer>> = old.into_iter().map(Some).collect();
        for (frame_id, buffer_id) in self.in_flight.drain() {
            if let Some(buf) = old.get_mut(buffer_id as usize).and_then(Option::take) {
                self.graveyard.insert(frame_id, buf);
            }
        }
        // The cached bind groups hold views of the buffers just replaced, and
        // of aliases that must not outlive them.
        if let Some(copy) = &mut self.copy {
            copy.drop_export_cache(self.buffers.len());
        }
        // Whatever is left was free; dropping it closes its fd, which no
        // one else holds.
        drop(old);

        self.width = width;
        self.height = height;
        self.last_publish = None;
        let (cols, rows) = tile_grid(width, height);
        self.history.reset(cols, rows);

        Ok(())
    }
}

fn tile_grid(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(TILE_SIZE), height.div_ceil(TILE_SIZE))
}
