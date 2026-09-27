//! V4L2 stateless H.264 decode: access units in, exported dmabufs out.
//!
//! The `v4l2` backend's half of [`crate::decoder`]. Same public shape as the
//! VA-API half — [`H264Decoder`], [`HwFrame`], [`MappedFrame`] — because
//! `ghostframe-client-gpu`'s renderer drives one API and the backend is a
//! compile-time choice underneath it.
//!
//! Where VA-API gives ffmpeg a render node and gets `AVFrame`s back, here
//! `cros-codecs` parses the bitstream and drives the V4L2 Request API, while
//! *we* supply the output buffers ([`crate::v4l2_frame`]) so each one is
//! exported as a dmabuf exactly once.
//!
//! ## Two things a reader should know before trusting this
//!
//! **It needs a patched `cros-codecs` to pick the right device.** The
//! unpatched 0.0.6 scan takes the first `/dev/videoN` with an OUTPUT mplane
//! queue, which on RK3399 is the hantro *encoder*. We carry a 25-line patch
//! that honours `CROS_CODECS_V4L2_DEVICE`
//! (`tools/hw-probe/v4l2-expbuf-rs/cros-codecs-0.0.6.patch`). When that is
//! absent [`H264Decoder::with_device`] refuses with
//! [`crate::H264Error::V4l2Unavailable`] rather than opening an encoder and
//! failing per frame, so `probe.rs` reports no H.264 and the session runs on
//! the tile codecs — which is the documented fallback, not a failure.
//!
//! **`cros-codecs` can panic on a stalled decode.** Its
//! `V4l2Device::sync` gives a queued request ~250 ms and then
//! `panic!("there should not be a scenario where a queued frame is not
//! returned.")`. That is upstream's code on our render thread, and it is not
//! ours to fix without another carried patch; [`HwFrame`] construction logs at
//! TRACE before it can block, so the panic has a precursor in the log rather
//! than appearing from nowhere. `DecodedHandle::is_ready` would let us avoid
//! the blocking call entirely -- it is `todo!()` on this backend, so asking
//! panics too.

use std::sync::Arc;

use cros_codecs::bitstream_utils::NalIterator;
use cros_codecs::codec::h264::parser::Nalu as H264Nalu;
use cros_codecs::decoder::stateless::h264::H264;
use cros_codecs::decoder::stateless::{
    DecodeError, DynStatelessVideoDecoder, StatelessDecoder, StatelessVideoDecoder,
};
use cros_codecs::decoder::{BlockingMode, DecodedHandle, DecoderEvent};
use cros_codecs::Resolution;

use crate::v4l2_device;
use crate::v4l2_frame::{ExportTable, FramePool, PooledFrame};
use crate::{DmabufPlanes, H264Error, PlaneDesc, DRM_FORMAT_GR88, DRM_FORMAT_R8};

/// One decoded frame, holding its capture buffer out of the driver's free list.
///
/// The `DecodedHandle` is kept purely for that: dropping it is what lets
/// `cros-codecs` requeue the V4L2 buffer, so anything reading the pixels must
/// outlive it. That is the same contract the VA-API `HwFrame` has with its
/// `AVFrame` reference, arrived at from the opposite direction.
///
/// Deliberately **not** `Send`, unlike the VA-API sibling. `cros-codecs`'
/// handle is `Rc<RefCell<…>>` inside, so claiming `Send` here would be a lie
/// rather than the carefully-argued `unsafe impl` the other backend can make
/// about `AVFrame`. Nothing needs it: decode and blit both happen on the render
/// thread.
pub struct HwFrame {
    /// Order matters: `frame` is an `Arc` into what `handle` owns, so `handle`
    /// must not be dropped first. Rust drops fields in declaration order, so
    /// `frame` (the borrow) is listed first and released first.
    frame: Arc<PooledFrame>,
    _handle: DynDecodedHandleOfPooledFrame,
}

type DynDecodedHandleOfPooledFrame = Box<dyn DecodedHandle<Frame = PooledFrame>>;

impl HwFrame {
    fn from_handle(handle: DynDecodedHandleOfPooledFrame) -> Result<Self, H264Error> {
        // The precursor line for upstream's ~250 ms `sync` panic: a decode that
        // never completes is a driver or bitstream problem, and this is the last
        // thing logged before cros-codecs gives up the way it gives up.
        //
        // It would be better to log this only when the frame is *not* ready.
        // `DecodedHandle::is_ready` is `todo!()` on the V4L2 backend
        // (`backend/v4l2/decoder/stateless.rs:98`), so merely asking panics --
        // a fourth candidate patch, and the reason this is unconditional and at
        // TRACE rather than conditional and at DEBUG.
        tracing::trace!("v4l2 decode: syncing decoded frame");
        handle
            .sync()
            .map_err(|e| H264Error::V4l2(format!("sync: {e}")))?;
        let frame = handle.video_frame();
        if !frame.is_populated() {
            return Err(H264Error::V4l2(
                "decoded frame has no exported dmabuf; see the VIDIOC_EXPBUF error above".into(),
            ));
        }
        Ok(Self {
            frame,
            _handle: handle,
        })
    }

    pub fn width(&self) -> u32 {
        self.frame.display_size().0
    }

    pub fn height(&self) -> u32 {
        self.frame.display_size().1
    }

    /// Download this buffer to system memory as tightly packed NV12.
    ///
    /// The fallback when the dmabuf cannot be imported, and the exactness
    /// oracle's route to the pixels. Mirrors the VA-API sibling's shape exactly
    /// — `(luma, chroma)`, `width * height` and `width * height.div_ceil(2)`
    /// bytes, strides removed — so `oracle_tests` compares like with like.
    pub fn download_nv12(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        use cros_codecs::video_frame::{VideoFrame, UV_PLANE, Y_PLANE};

        let w = self.width() as usize;
        let h = self.height() as usize;
        let mapping = match self.frame.map() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "v4l2 decode: cannot map frame for download");
                return None;
            }
        };
        let planes = mapping.get();
        let pitches = self.frame.get_plane_pitch();
        let y_stride = pitches[Y_PLANE];
        let uv_stride = pitches[UV_PLANE];
        let chroma_h = h.div_ceil(2);
        if y_stride < w || uv_stride < w {
            tracing::warn!(
                y_stride,
                uv_stride,
                w,
                "v4l2 decode: stride narrower than width"
            );
            return None;
        }
        if planes[Y_PLANE].len() < y_stride * h || planes[UV_PLANE].len() < uv_stride * chroma_h {
            tracing::warn!("v4l2 decode: mapped plane shorter than its own stride * rows");
            return None;
        }
        let mut luma = Vec::with_capacity(w * h);
        for row in 0..h {
            luma.extend_from_slice(&planes[Y_PLANE][row * y_stride..][..w]);
        }
        let mut chroma = Vec::with_capacity(w * chroma_h);
        for row in 0..chroma_h {
            chroma.extend_from_slice(&planes[UV_PLANE][row * uv_stride..][..w]);
        }
        Some((luma, chroma))
    }

    /// Describe this buffer as an NV12 dmabuf.
    ///
    /// No mapping call is involved, unlike the VA-API path's
    /// `av_hwframe_map`: the fd was exported once when the buffer was first
    /// dequeued, and the layout comes from the driver's own
    /// `v4l2_pix_format_mplane`. The name is kept for API symmetry, and the
    /// returned [`MappedFrame`] keeps the same promise — `planes().fd` is valid
    /// exactly as long as it lives.
    pub fn map_dmabuf(&self) -> Result<MappedFrame, H264Error> {
        let (width, height) = self.frame.display_size();
        let fd = self
            .frame
            .dmabuf_fd()
            .ok_or_else(|| H264Error::V4l2("frame has no exported dmabuf".into()))?;
        let stride = self.frame.stride();
        let chroma_offset = self.frame.chroma_offset();
        let size = self.frame.total_size();

        let luma = PlaneDesc {
            offset: 0,
            pitch: stride,
        };
        let chroma = PlaneDesc {
            offset: chroma_offset,
            pitch: stride,
        };

        let planes = DmabufPlanes {
            fd,
            // rkvdec writes plain raster NV12 -- confirmed by its luma bytes
            // matching a software decode exactly at the reported stride -- so
            // there is no modifier to carry and no detiling step, unlike the
            // MediaTek MM21 path cros-codecs is written around.
            modifier: 0,
            size,
            width,
            height,
            luma,
            chroma,
            fourcc_luma: DRM_FORMAT_R8,
            fourcc_chroma: DRM_FORMAT_GR88,
        };

        // The same extent arithmetic the VA-API path applies to a DRM
        // descriptor, applied here to numbers the driver chose. A coded height
        // we did not pick and a stride we did not pick can overrun the
        // allocation exactly the same way.
        crate::descriptor::check_extent(luma, u64::from(height), size, "luma")?;
        crate::descriptor::check_extent(chroma, u64::from(planes.chroma_height()), size, "chroma")?;

        Ok(MappedFrame {
            _frame: self.frame.clone(),
            planes,
        })
    }
}

/// An NV12 dmabuf description, keeping the buffer it describes alive.
///
/// Holds an `Arc` on the pool frame, so the V4L2 buffer is not requeued and its
/// exported fd is not closed while this lives. `planes` is private with a
/// `&self` accessor for the reason the VA-API sibling spells out: `DmabufPlanes`
/// is `Copy`, and a `pub` field would let a caller copy the fd out and use it
/// after the owner drops — not "fd is closed", which fails loudly, but "fd
/// number was recycled", where a later `dup()` silently imports something else.
pub struct MappedFrame {
    _frame: Arc<PooledFrame>,
    planes: DmabufPlanes,
}

impl MappedFrame {
    pub fn planes(&self) -> &DmabufPlanes {
        &self.planes
    }
}

/// H.264 decode on a V4L2 stateless decoder.
///
/// Not `Send`, for the reason given on [`HwFrame`].
pub struct H264Decoder {
    decoder: DynStatelessVideoDecoder<PooledFrame>,
    pool: FramePool,
    /// Kept so exported fds outlive the frames that reference them, and so a
    /// [`Self::reset`] does not re-export buffers it already has.
    _exports: Arc<ExportTable>,
    /// Per-picture timestamp, and the decoder's key for its in-flight request
    /// map. Must be unique per submission: keying it off the *output* frame
    /// count collides — several pictures share timestamp 0 — and upstream
    /// unwraps the lookup, so a collision is a panic, not a wrong picture.
    next_timestamp: u64,
    /// Mirrors ffmpeg's terminal `AVERROR_EOF` state after
    /// [`Self::finish`]. `cros-codecs` has no equivalent — its `flush` leaves
    /// the decoder usable once a keyframe arrives — so the contract
    /// `decode_after_finish_without_reset_is_an_error` guarantees is enforced
    /// here rather than inherited.
    finished: bool,
}

impl H264Decoder {
    /// Open the machine's stateless H.264 decoder, whichever node it is on.
    pub fn new() -> Result<Self, H264Error> {
        let path = v4l2_device::find_h264_decoder().ok_or_else(|| {
            H264Error::V4l2Unavailable(
                "no /dev/video* node enumerates V4L2_PIX_FMT_H264_SLICE (S264)".into(),
            )
        })?;
        let node = path.to_string_lossy().into_owned();
        Self::with_device(&node)
    }

    /// Open a specific node, refusing if `cros-codecs` would not use it.
    ///
    /// The check is the point. `cros-codecs 0.0.6` chooses its own device and
    /// ignores what it is told, so "succeeded in constructing a decoder" does
    /// not mean "decoding on `node`". Refusing here turns a per-frame
    /// `Unrecoverable decoding error` on an encoder into one clear message at
    /// startup, and lets `probe.rs` report no H.264 so the session falls back
    /// to the tile codecs.
    pub fn with_device(node: &str) -> Result<Self, H264Error> {
        let path = std::path::Path::new(node);
        if !v4l2_device::enumerates_h264_slice(path) {
            return Err(H264Error::V4l2Unavailable(format!(
                "{node} does not enumerate V4L2_PIX_FMT_H264_SLICE on its OUTPUT queue"
            )));
        }
        // Predict what cros-codecs will actually open: the override if one is
        // set (the carried patch honours it), otherwise its own scan.
        //
        // Both halves matter, and the second was learned the hard way. A stale
        // override is worse than none: `/dev/videoN` numbering is **not stable
        // across boots** on this hardware -- rkvdec moved from video3 to video1
        // over one reboot, swapping places with the hantro decoder -- so an
        // override written down once silently starts naming a different device.
        // Checking only "does the scan agree, or is an override set at all"
        // accepted exactly that case and handed the stream to hantro, which
        // advertises no H.264 and failed with `driver does not support S264`
        // several layers down.
        let effective = v4l2_device::device_cros_codecs_will_open();
        if effective.as_deref() != Some(path) {
            let named = effective
                .as_deref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "nothing".into());
            return Err(H264Error::V4l2Unavailable(format!(
                "cros-codecs would open {named} rather than {node}; set {}={node} and build \
                 with tools/hw-probe/v4l2-expbuf-rs/cros-codecs-0.0.6.patch applied. Node \
                 numbering is not stable across boots -- prefer discovering it with \
                 `probe::default_device()` over writing one down.",
                v4l2_device::DEVICE_OVERRIDE_ENV,
            )));
        }

        let exports = Arc::new(ExportTable::default());
        // NonBlocking so `next_event` polls rather than parking the render
        // thread inside the decoder.
        let decoder = StatelessDecoder::<H264, _>::new_v4l2(BlockingMode::NonBlocking)
            .map_err(|e| H264Error::V4l2Unavailable(format!("cannot open decoder: {e:?}")))?
            .into_trait_object();
        tracing::info!(node, "v4l2 decode: opened stateless H.264 decoder");
        Ok(Self {
            decoder,
            pool: FramePool::new(exports.clone()),
            _exports: exports,
            next_timestamp: 1,
            finished: false,
        })
    }

    /// Feed one access unit; return every frame it completed.
    ///
    /// An empty result is normal: nothing comes out before the first keyframe.
    /// The access unit is split into NAL units here because `cros-codecs`
    /// consumes one NAL at a time, and all NALs of one picture share a
    /// timestamp — that is how the decoder knows they belong together.
    pub fn decode(&mut self, au: &[u8]) -> Result<Vec<HwFrame>, H264Error> {
        if self.finished {
            return Err(H264Error::V4l2(
                "decode() after finish() without an intervening reset()".into(),
            ));
        }
        let timestamp = self.next_timestamp;
        self.next_timestamp += 1;

        let mut out = Vec::new();
        for nalu in NalIterator::<H264Nalu>::new(au) {
            self.submit_nalu(timestamp, &nalu, &mut out)?;
        }
        self.drain_events(&mut out)?;
        Ok(out)
    }

    /// Submit one NAL, draining events whenever the decoder asks us to.
    ///
    /// `decode` reports how many bytes it took and may take fewer than offered,
    /// so the remainder is resubmitted rather than dropped: for H.264 a lost
    /// NAL means corruption until the next IDR, with nothing to tell the caller
    /// it happened.
    fn submit_nalu(
        &mut self,
        timestamp: u64,
        nalu: &[u8],
        out: &mut Vec<HwFrame>,
    ) -> Result<(), H264Error> {
        let mut rest = nalu;
        // Bounded so a decoder that accepts nothing and asks for events
        // forever cannot spin the render thread. 64 rounds is far past any real
        // NAL: one round per event-drain is the normal worst case, and the pool
        // holds single digits of frames.
        for _ in 0..64 {
            if rest.is_empty() {
                return Ok(());
            }
            let pool = &self.pool;
            match self.decoder.decode(timestamp, rest, &mut || pool.alloc()) {
                Ok(0) => {
                    // No progress and no error: draining is the only thing that
                    // can change the outcome.
                    self.drain_events(out)?;
                }
                Ok(n) => rest = &rest[n.min(rest.len())..],
                Err(DecodeError::CheckEvents) | Err(DecodeError::NotEnoughOutputBuffers(_)) => {
                    self.drain_events(out)?;
                }
                Err(e) => return Err(H264Error::V4l2(format!("decode: {e}"))),
            }
        }
        Err(H264Error::V4l2(
            "decoder made no progress on one NAL after 64 drain-and-retry rounds".into(),
        ))
    }

    /// Collect everything the decoder has ready.
    ///
    /// A `FormatChanged` event is where the output pool is (re)built: the frame
    /// count comes from `min_num_frames`, i.e. from the codec's DPB
    /// requirement, not from any ghostframe constant.
    fn drain_events(&mut self, out: &mut Vec<HwFrame>) -> Result<(), H264Error> {
        while let Some(event) = self.decoder.next_event() {
            match event {
                DecoderEvent::FrameReady(handle) => out.push(HwFrame::from_handle(handle)?),
                DecoderEvent::FormatChanged => {
                    let info = self
                        .decoder
                        .stream_info()
                        .ok_or_else(|| {
                            H264Error::V4l2("format changed with no stream info".into())
                        })?
                        .clone();
                    let Resolution { width, height } = info.display_resolution;
                    tracing::info!(
                        width,
                        height,
                        coded_width = info.coded_resolution.width,
                        coded_height = info.coded_resolution.height,
                        frames = info.min_num_frames,
                        "v4l2 decode: output format"
                    );
                    self.pool
                        .resize(info.display_resolution, info.min_num_frames);
                }
            }
        }
        Ok(())
    }

    /// Finish the stream and drain what the decoder still holds.
    ///
    /// **Terminal**, like the VA-API sibling: [`Self::decode`] afterwards is an
    /// error until [`Self::reset`]. Idempotent.
    pub fn finish(&mut self) -> Result<Vec<HwFrame>, H264Error> {
        let mut out = Vec::new();
        if !self.finished {
            self.decoder
                .flush()
                .map_err(|e| H264Error::V4l2(format!("flush: {e}")))?;
            self.finished = true;
        }
        self.drain_events(&mut out)?;
        Ok(out)
    }

    /// Discard buffered state and continue — ffmpeg's `avcodec_flush_buffers`.
    ///
    /// Needed on stream discontinuity: unrecovered loss, a resolution change,
    /// or a session reset. Also the way back from [`Self::finish`]'s terminal
    /// state. Frames the flush completes are dropped on purpose: `reset` means
    /// "forget where we were", and handing back pictures from before the
    /// discontinuity is the opposite of that.
    pub fn reset(&mut self) {
        if let Err(e) = self.decoder.flush() {
            tracing::warn!(error = %e, "v4l2 decode: flush during reset failed");
        }
        let mut discarded = Vec::new();
        if let Err(e) = self.drain_events(&mut discarded) {
            tracing::warn!(error = %e, "v4l2 decode: draining during reset failed");
        }
        if !discarded.is_empty() {
            tracing::debug!(
                frames = discarded.len(),
                "v4l2 decode: discarded frames completed by reset"
            );
        }
        self.finished = false;
    }
}
