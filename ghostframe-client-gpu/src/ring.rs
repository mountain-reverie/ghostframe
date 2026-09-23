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
use crate::export::ExportedImage;
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
pub struct ExportBuffer {
    pub exported: ExportedImage,
    pub texture: wgpu::Texture,
    /// Generation this buffer's contents correspond to. `None` = never
    /// filled, which forces a full blit.
    pub filled_at_gen: Option<u64>,
    /// Held by the host until released.
    pub in_flight: bool,
}

/// What [`ExportRing::publish`] hands back.
pub struct PublishedFrame {
    pub frame_id: u32,
    pub buffer_id: u32,
    /// Damage in PIXEL coordinates, ready for the host.
    pub damage: Vec<Rect>,
}

/// A ring of exported dmabuf buffers, filled with history-aware partial
/// blits from a shared private [`Framebuffer`].
pub struct ExportRing {
    buffers: Vec<ExportBuffer>,
    history: DirtyHistory,
    next_frame_id: u32,
    /// Maps an outstanding `frame_id` to the buffer it was published into,
    /// so `release` can find it without a linear scan tied to buffer state
    /// (and so an unknown `frame_id` is trivially "not found" rather than
    /// requiring buffer-shape assumptions to detect).
    in_flight: HashMap<u32, u32>,
    width: u32,
    height: u32,
    /// Kept so [`ExportRing::resize`] can reallocate with the same
    /// consumer preference the ring was constructed with.
    preferred_modifiers: Vec<u64>,
}

impl ExportRing {
    pub fn new(
        ctx: &WgpuContext,
        width: u32,
        height: u32,
        count: usize,
        preferred_modifiers: &[u64],
    ) -> Result<Self, GpuError> {
        let mut buffers = Vec::with_capacity(count);
        for _ in 0..count {
            let exported = ExportedImage::new(ctx, width, height, preferred_modifiers)?;
            let texture = exported.as_wgpu_texture(&ctx.device)?;
            buffers.push(ExportBuffer {
                exported,
                texture,
                filled_at_gen: None,
                in_flight: false,
            });
        }

        let (cols, rows) = tile_grid(width, height);

        Ok(ExportRing {
            buffers,
            history: DirtyHistory::new(cols, rows, HISTORY_CAPACITY),
            next_frame_id: 0,
            in_flight: HashMap::new(),
            width,
            height,
            preferred_modifiers: preferred_modifiers.to_vec(),
        })
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

    /// Fill a free buffer from `fb` and hand it out.
    ///
    /// Returns `None` when every buffer is held by the host. The library
    /// keeps decoding into its private framebuffer; nothing stalls and
    /// nothing is lost, because damage accumulates in the history and the
    /// next released buffer picks up everything since it was written.
    pub fn publish(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fb: &Framebuffer,
    ) -> Option<PublishedFrame> {
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

        let buffer_id = self.buffers.iter().position(|b| !b.in_flight)? as u32;

        let rects = match self
            .history
            .union_since(self.buffers[buffer_id as usize].filled_at_gen)
        {
            Some(dirty) => coalesce(&dirty),
            None => {
                let (cols, rows) = tile_grid(self.width, self.height);
                vec![Rect {
                    x: 0,
                    y: 0,
                    w: cols,
                    h: rows,
                }]
            }
        };
        let pixel_rects: Vec<Rect> = rects
            .into_iter()
            .map(|r| r.to_pixels(fb.width, fb.height))
            .collect();

        fb.blit_rects(
            device,
            queue,
            &self.buffers[buffer_id as usize].texture,
            &pixel_rects,
        );

        let buf = &mut self.buffers[buffer_id as usize];
        buf.filled_at_gen = Some(gen);
        buf.in_flight = true;

        // v1 synchronisation simplification: block rather than exporting a
        // real fence. wgpu-hal 30 *can* signal an exportable semaphore via
        // `Queue::add_signal_semaphore`, so this is a deliberate deferral,
        // not a limitation -- not implemented here.
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll after export publish");

        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);
        self.in_flight.insert(frame_id, buffer_id);

        Some(PublishedFrame {
            frame_id,
            buffer_id,
            damage: pixel_rects,
        })
    }

    /// Release the buffer held by `frame_id` back to the free pool. An
    /// unknown `frame_id` (already released, or never issued) is logged and
    /// ignored rather than panicking -- a host bug in release bookkeeping
    /// should not be able to crash the client.
    pub fn release(&mut self, frame_id: u32) {
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

    /// Reallocate every buffer at the new size and drop all history.
    pub fn resize(&mut self, ctx: &WgpuContext, width: u32, height: u32) -> Result<(), GpuError> {
        let count = self.buffers.len();

        let mut new_buffers = Vec::with_capacity(count);
        for _ in 0..count {
            let exported = ExportedImage::new(ctx, width, height, &self.preferred_modifiers)?;
            let texture = exported.as_wgpu_texture(&ctx.device)?;
            new_buffers.push(ExportBuffer {
                exported,
                texture,
                filled_at_gen: None,
                in_flight: false,
            });
        }

        self.buffers = new_buffers;
        self.width = width;
        self.height = height;
        let (cols, rows) = tile_grid(width, height);
        self.history.reset(cols, rows);
        self.next_frame_id = 0;
        self.in_flight.clear();

        Ok(())
    }
}

fn tile_grid(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(TILE_SIZE), height.div_ceil(TILE_SIZE))
}
