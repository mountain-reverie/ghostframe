//! GStreamer H.264 decode: access units in, exported dmabufs out.
//!
//! The `gstreamer` backend's half of [`crate::decoder`]. Same public shape as
//! the VA-API half — [`H264Decoder`], [`HwFrame`], [`MappedFrame`] — because
//! `ghostframe-client-gpu`'s renderer drives one API and the backend is a
//! compile-time choice underneath it.
//!
//! `appsrc ! h264parse ! v4l2slh264dec ! appsink`, with the dmabuf caps
//! **required** so the decoder cannot quietly hand back system memory. On this
//! hardware the underlying decoder is rkvdec over the V4L2 Request API, but
//! nothing here says so: GStreamer picks the element and the device, and would
//! use a different one on different hardware.
//!
//! ## What this replaced, and why
//!
//! A `cros-codecs` backend that drove the V4L2 Request API directly. It worked
//! and was bit-exact, but it carried five patches against a crate whose upstream
//! has been dormant since March 2025, and it had to derive the NV12 plane layout
//! by hand from `v4l2_pix_format_mplane` — which is the part that produced
//! *plausible but wrong* output twice before it was right. GStreamer reports the
//! layout, so that arithmetic is no longer ours to get wrong.
//!
//! ## Three negotiation requirements, none obvious
//!
//! Each was a dead end first, and each failed somewhere other than its cause.
//! `tools/hw-probe/gst-dmabuf-rs` reproduces all three.
//!
//! 1. **Require the dmabuf *feature*, do not pin the format.** Adding
//!    `format=DMA_DRM` to the sink caps narrows the intersection and fails with
//!    a bare `not-negotiated (-4)`; the decoder fills in `format` and
//!    `drm-format` itself.
//! 2. **The sink must advertise `GstVideoMeta`** in `propose_allocation`, or
//!    `v4l2codecs` refuses outright: *"DMABuf caps negotiated without the
//!    mandatory support of VideoMeta"*. A default `appsink` does not, and the
//!    resulting error is reported by `h264parse`, two elements upstream.
//! 3. **`GstVideoMeta` carries the CODED size.** At 1080p it reports height
//!    1088 while the display height is 1080. Strides and offsets come from the
//!    meta; width and height come from the caps. Mixing those up writes eight
//!    extra rows of luma — the right shape, the wrong content.
//!
//! Requires **GStreamer >= 1.24.1**, which is where `v4l2codecs` gained DMA_DRM
//! caps. The `v1_24` feature on the gstreamer crates makes that a link-time
//! fact rather than a runtime surprise.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

use crate::{DmabufPlanes, H264Error, PlaneDesc, DRM_FORMAT_GR88, DRM_FORMAT_R8};

/// Samples the pipeline has produced and we have not handed out yet.
///
/// Filled by `appsink`'s `new_sample` callback on the streaming thread, drained
/// by [`H264Decoder::decode`] on the caller's. This exists so `decode` never
/// blocks: the pipeline is asynchronous, so a pull immediately after a push
/// usually finds nothing, and *waiting* for it would put the decoder's latency
/// on the render thread -- up to a full frame budget per access unit, at exactly
/// the times when there is nothing to show anyway (before the first keyframe,
/// or after loss).
///
/// Deliberately unbounded, which is safe for a reason worth stating: the
/// decoder's dmabuf pool is the real backpressure. Every queued sample pins one
/// of the ~11 pooled buffers, so a caller that stops consuming stalls the
/// decoder on buffer allocation rather than growing this queue without limit. It
/// is also why a caller must not hoard frames -- holding the whole pool
/// deadlocks the decoder, which is the first thing this backend got wrong.
type SampleQueue = Arc<Mutex<VecDeque<gst::Sample>>>;

/// Nominal frame duration for the timestamps handed to `appsrc`.
///
/// The decoder needs monotonically increasing PTS to order its output; the
/// actual value is irrelevant because nothing downstream schedules on it
/// (`appsink` runs with `sync=false`). 30fps is a round number, not a claim
/// about the stream.
const NOMINAL_FRAME_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(33);

/// The plane layout of one decoded frame, as GStreamer reports it.
#[derive(Debug, Clone, Copy)]
struct Layout {
    fd: i32,
    /// DISPLAY size, from the caps. **Not** the meta's, which is coded.
    width: u32,
    height: u32,
    /// Byte offset and row pitch per plane: `[0]` luma, `[1]` interleaved chroma.
    offsets: [u64; 2],
    strides: [u64; 2],
    size: u64,
    /// DRM modifier, reported by the decoder rather than assumed. `0` is
    /// `DRM_FORMAT_MOD_LINEAR`, which is what rkvdec produces and what
    /// `import_gles.rs` can import without a detiling step.
    modifier: u64,
}

/// One decoded frame, holding its dmabuf alive.
///
/// The `gst::Sample` is kept for exactly that: it owns the buffer, the buffer
/// owns the `GstDmaBufMemory`, and that owns the fd. Dropping it returns the
/// buffer to the decoder's pool, so anything reading the pixels must outlive it.
///
/// `Send`, unlike the cros-codecs backend's equivalent — GStreamer's mini-objects
/// are internally thread-safe, so this needs no `unsafe impl` and no argument.
pub struct HwFrame {
    sample: gst::Sample,
    layout: Layout,
}

impl HwFrame {
    pub fn width(&self) -> u32 {
        self.layout.width
    }

    pub fn height(&self) -> u32 {
        self.layout.height
    }

    /// Download this buffer to system memory as tightly packed NV12.
    ///
    /// The fallback when the dmabuf cannot be imported, and the exactness
    /// oracle's route to the pixels. Mirrors the VA-API sibling exactly —
    /// `(luma, chroma)`, `width * height` and `width * height.div_ceil(2)` bytes,
    /// strides removed — so the oracles compare like with like.
    pub fn download_nv12(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        let l = &self.layout;
        let w = l.width as usize;
        let h = l.height as usize;
        let chroma_h = h.div_ceil(2);
        let (y_stride, uv_stride) = (l.strides[0] as usize, l.strides[1] as usize);
        if y_stride < w || uv_stride < w {
            tracing::warn!(
                y_stride,
                uv_stride,
                w,
                "gst decode: stride narrower than width"
            );
            return None;
        }
        let end = l.offsets[1] as usize + uv_stride * chroma_h;
        if end > l.size as usize {
            tracing::warn!(
                end,
                size = l.size,
                "gst decode: plane layout exceeds the dmabuf"
            );
            return None;
        }

        // SAFETY: `l.fd` is a dmabuf owned by `self.sample` for the duration of
        // this call; mapped read-only for the length the memory reports, and
        // unmapped before returning on every path.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                l.size as usize,
                libc::PROT_READ,
                libc::MAP_SHARED,
                l.fd,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            tracing::warn!(error = %std::io::Error::last_os_error(), "gst decode: mmap failed");
            return None;
        }
        let mut luma = Vec::with_capacity(w * h);
        let mut chroma = Vec::with_capacity(w * chroma_h);
        for row in 0..h {
            // SAFETY: bounds checked against `l.size` above.
            let p = unsafe { (base as *const u8).add(l.offsets[0] as usize + row * y_stride) };
            luma.extend_from_slice(unsafe { std::slice::from_raw_parts(p, w) });
        }
        for row in 0..chroma_h {
            let p = unsafe { (base as *const u8).add(l.offsets[1] as usize + row * uv_stride) };
            chroma.extend_from_slice(unsafe { std::slice::from_raw_parts(p, w) });
        }
        // SAFETY: the pair returned by the `mmap` above.
        unsafe { libc::munmap(base, l.size as usize) };
        Some((luma, chroma))
    }

    /// Describe this buffer as an NV12 dmabuf.
    ///
    /// No mapping call is involved: GStreamer already reported the fd, the plane
    /// offsets, the strides and the modifier. The name is kept for symmetry with
    /// the VA-API path's `av_hwframe_map`, and the returned [`MappedFrame`] keeps
    /// the same promise — `planes().fd` is valid exactly as long as it lives.
    pub fn map_dmabuf(&self) -> Result<MappedFrame, H264Error> {
        let l = &self.layout;
        let luma = PlaneDesc {
            offset: l.offsets[0],
            pitch: l.strides[0],
        };
        let chroma = PlaneDesc {
            offset: l.offsets[1],
            pitch: l.strides[1],
        };
        let planes = DmabufPlanes {
            fd: l.fd,
            modifier: l.modifier,
            size: l.size,
            width: l.width,
            height: l.height,
            luma,
            chroma,
            fourcc_luma: DRM_FORMAT_R8,
            fourcc_chroma: DRM_FORMAT_GR88,
        };
        // The same extent arithmetic the VA-API path applies to a DRM
        // descriptor. GStreamer's numbers are trustworthy, but "trustworthy" and
        // "checked" are different things, and this runs once per frame on the
        // render thread.
        crate::descriptor::check_extent(luma, u64::from(l.height), l.size, "luma")?;
        crate::descriptor::check_extent(
            chroma,
            u64::from(planes.chroma_height()),
            l.size,
            "chroma",
        )?;
        Ok(MappedFrame {
            _sample: self.sample.clone(),
            planes,
        })
    }
}

/// An NV12 dmabuf description, keeping the buffer it describes alive.
///
/// `planes` is private with a `&self` accessor for the reason the VA-API sibling
/// spells out: `DmabufPlanes` is `Copy`, and a `pub` field would let a caller
/// copy the fd out and use it after the owner drops — not "fd is closed", which
/// fails loudly, but "fd number was recycled", where a later `dup()` silently
/// imports something else.
pub struct MappedFrame {
    _sample: gst::Sample,
    planes: DmabufPlanes,
}

impl MappedFrame {
    pub fn planes(&self) -> &DmabufPlanes {
        &self.planes
    }
}

/// H.264 decode through GStreamer.
pub struct H264Decoder {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    sink: gst_app::AppSink,
    /// Collected by the bus watch. Checked after every push, because a
    /// negotiation failure otherwise shows up only as "no frames came out".
    errors: Arc<Mutex<Vec<String>>>,
    samples: SampleQueue,
    next_pts: gst::ClockTime,
    /// Mirrors ffmpeg's terminal `AVERROR_EOF` state after [`Self::finish`], so
    /// the contract `decode_after_finish_without_reset_is_an_error` guarantees
    /// holds on both backends.
    finished: bool,
}

impl H264Decoder {
    /// Build the pipeline. GStreamer selects the decoder element and the device.
    pub fn new() -> Result<Self, H264Error> {
        Self::build()
    }

    /// Same, with a device named for diagnostics only.
    ///
    /// GStreamer chooses the V4L2 node itself, so this cannot steer it — and
    /// does not need to, which is the point: the elaborate device-selection
    /// machinery the previous backend required is simply absent here. The node is
    /// still checked, so a machine with no stateless H.264 decoder fails with one
    /// clear message at startup rather than an empty pipeline later.
    pub fn with_device(node: &str) -> Result<Self, H264Error> {
        if !crate::v4l2_device::enumerates_h264_slice(std::path::Path::new(node)) {
            return Err(H264Error::Gst(format!(
                "{node} does not enumerate V4L2_PIX_FMT_H264_SLICE on its OUTPUT queue"
            )));
        }
        tracing::debug!(
            node,
            "gst decode: node advertises S264 (GStreamer picks the device)"
        );
        Self::build()
    }

    fn build() -> Result<Self, H264Error> {
        gst::init().map_err(|e| H264Error::Gst(format!("gst_init: {e}")))?;

        let pipeline = gst::Pipeline::new();
        let src = gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::builder("video/x-h264")
                    .field("stream-format", "byte-stream")
                    .field("alignment", "au")
                    .build(),
            )
            .format(gst::Format::Time)
            .build();
        let parse = make("h264parse")?;
        let dec = make("v4l2slh264dec")?;

        let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let samples: SampleQueue = Arc::new(Mutex::new(VecDeque::new()));
        let queue_for_cb = samples.clone();
        let sink = gst_app::AppSink::builder()
            // Requirement 1: the feature only. See the module docs.
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .features(["memory:DMABuf"])
                    .build(),
            )
            // Requirement 2: without this, negotiation is refused outright.
            .callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .propose_allocation(|_sink, query| {
                        query.add_allocation_meta::<gst_video::VideoMeta>(None);
                        true
                    })
                    // Take delivery on the streaming thread so `decode` can be
                    // non-blocking. See `SampleQueue`.
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        queue_for_cb
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push_back(sample);
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            )
            // `sync=false`: frames are wanted as soon as they decode, not at
            // their presentation time. `drop=false` (the default) because losing
            // a frame silently is worse than back-pressure.
            .sync(false)
            .max_buffers(8)
            .build();

        pipeline
            .add_many([src.upcast_ref(), &parse, &dec, sink.upcast_ref()])
            .map_err(|e| H264Error::Gst(format!("add elements: {e}")))?;
        gst::Element::link_many([src.upcast_ref(), &parse, &dec, sink.upcast_ref()])
            .map_err(|e| H264Error::Gst(format!("link elements: {e}")))?;

        // Collect bus errors as they happen. This is not optional diagnostics:
        // a refused negotiation makes `try_pull_sample` return None forever with
        // no other signal, and every failure met while building this backend was
        // invisible until the bus was read.
        if let Some(bus) = pipeline.bus() {
            let sink_errors = errors.clone();
            bus.set_sync_handler(move |_, msg| {
                if let gst::MessageView::Error(e) = msg.view() {
                    let text = format!(
                        "{} ({})",
                        e.error(),
                        e.debug().unwrap_or_else(|| "no detail".into())
                    );
                    tracing::error!(error = %text, "gst decode: pipeline error");
                    sink_errors
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(text);
                }
                gst::BusSyncReply::Drop
            });
        }

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| H264Error::Gst(format!("set PLAYING: {e}")))?;
        tracing::info!(
            "gst decode: pipeline playing (appsrc ! h264parse ! v4l2slh264dec ! appsink)"
        );

        Ok(Self {
            pipeline,
            src,
            sink,
            errors,
            samples,
            next_pts: gst::ClockTime::ZERO,
            finished: false,
        })
    }

    /// Feed one access unit; return every frame it completed.
    ///
    /// An empty result is normal, not an error: nothing comes out before the
    /// decoder has an SPS and a keyframe.
    pub fn decode(&mut self, au: &[u8]) -> Result<Vec<HwFrame>, H264Error> {
        if self.finished {
            return Err(H264Error::Gst(
                "decode() after finish() without an intervening reset()".into(),
            ));
        }
        let mut buffer = gst::Buffer::from_slice(au.to_vec());
        {
            let b = buffer
                .get_mut()
                .expect("freshly allocated buffer is unique");
            b.set_pts(self.next_pts);
            b.set_duration(NOMINAL_FRAME_DURATION);
        }
        self.next_pts += NOMINAL_FRAME_DURATION;

        self.src
            .push_buffer(buffer)
            .map_err(|e| H264Error::Gst(format!("push_buffer: {e}")))?;
        self.take_error()?;
        self.drain_ready()
    }

    /// Everything the pipeline has ready right now, without waiting.
    ///
    /// Genuinely without waiting: the samples were collected by the `new_sample`
    /// callback, so this is a queue drain and not a pull. A frame therefore
    /// arrives on the `decode` call *after* the one that fed its access unit,
    /// which is what the "an empty result is normal" contract already allowed.
    fn drain_ready(&mut self) -> Result<Vec<HwFrame>, H264Error> {
        let taken: Vec<gst::Sample> = {
            let mut q = self.samples.lock().unwrap_or_else(|p| p.into_inner());
            q.drain(..).collect()
        };
        let mut out = Vec::with_capacity(taken.len());
        for sample in taken {
            out.push(self.frame_from_sample(sample)?);
        }
        self.take_error()?;
        Ok(out)
    }

    fn frame_from_sample(&self, sample: gst::Sample) -> Result<HwFrame, H264Error> {
        let layout = describe(&sample)?;
        Ok(HwFrame { sample, layout })
    }

    /// Promote a collected bus error into a `Result`.
    fn take_error(&self) -> Result<(), H264Error> {
        let mut errors = self.errors.lock().unwrap_or_else(|p| p.into_inner());
        if errors.is_empty() {
            return Ok(());
        }
        let joined = errors.join("; ");
        errors.clear();
        Err(H264Error::Gst(joined))
    }

    /// Signal end of stream and collect what the pipeline still holds.
    ///
    /// **Terminal**, like the VA-API sibling: [`Self::decode`] afterwards is an
    /// error until [`Self::reset`]. Idempotent — the end-of-stream is sent once
    /// and later calls only drain.
    ///
    /// **Call it until it returns empty.** One call does not necessarily return
    /// the whole tail, and that is not a shortcoming to be fixed by waiting
    /// longer: every returned frame pins one of the decoder's pooled dmabufs, so
    /// a caller holding a poolful stalls the decoder that would produce the
    /// rest. Returning "everything remaining" in one `Vec` is therefore
    /// impossible for any stream longer than the pool — about 11 frames here.
    ///
    /// The VA-API sibling has no such limit because ffmpeg grows its frame pool
    /// on demand; a V4L2 pool is fixed at negotiation. This is the one place the
    /// two backends' contracts genuinely differ, so it is stated rather than
    /// papered over, and `oracle_tests_gst` loops accordingly.
    pub fn finish(&mut self) -> Result<Vec<HwFrame>, H264Error> {
        if !self.finished {
            self.src
                .end_of_stream()
                .map_err(|e| H264Error::Gst(format!("end_of_stream: {e}")))?;
            self.finished = true;
        }
        // Waiting here, unlike `decode`: EOS means no more input is coming, so
        // waiting is the only way to collect what is still in flight. Bounded,
        // so a decoder that has died cannot hang the caller.
        //
        // **Do not accumulate the whole stream here.** Each returned frame pins
        // a pooled dmabuf, and holding more than the pool deadlocks the decoder
        // -- which is precisely how this backend failed first: every `decode`
        // returned nothing, all sixteen frames queued up for `finish`, and the
        // seventh exhausted the pool. The callback-driven `decode` above is what
        // keeps the tail here small; this loop stopping early is a symptom of
        // that having gone wrong, not of EOS.
        let deadline = std::time::Instant::now() + DRAIN_TIMEOUT;
        let mut out = Vec::new();
        loop {
            let taken: Vec<gst::Sample> = {
                let mut q = self.samples.lock().unwrap_or_else(|p| p.into_inner());
                q.drain(..).collect()
            };
            for sample in taken {
                out.push(self.frame_from_sample(sample)?);
            }
            if self.sink.is_eos() || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        if !self.sink.is_eos() {
            tracing::warn!(
                frames = out.len(),
                "gst decode: finish timed out before EOS; the pipeline may be stalled on its \
                 buffer pool because frames are being held"
            );
        }
        self.take_error()?;
        Ok(out)
    }

    /// Discard buffered state and continue — the analogue of ffmpeg's
    /// `avcodec_flush_buffers`.
    ///
    /// Needed on stream discontinuity: unrecovered loss, a resolution change, or
    /// a session reset. Also the way back from [`Self::finish`]'s terminal state.
    /// Frames the flush completes are dropped on purpose: `reset` means "forget
    /// where we were", and handing back pictures from before the discontinuity is
    /// the opposite of that.
    pub fn reset(&mut self) {
        // A flush pair rather than a state cycle: it clears the decoder's
        // buffered state and the appsink queue while leaving negotiation intact,
        // so the next keyframe starts decoding without renegotiating caps or
        // reallocating the dmabuf pool.
        let _ = self.pipeline.send_event(gst::event::FlushStart::new());
        let _ = self
            .pipeline
            .send_event(gst::event::FlushStop::builder(true).build());

        let discarded = {
            let mut q = self.samples.lock().unwrap_or_else(|p| p.into_inner());
            let n = q.len();
            q.clear();
            n
        };
        if discarded > 0 {
            tracing::debug!(frames = discarded, "gst decode: discarded frames on reset");
        }
        // The EOS that `finish` sent is part of what the flush clears, but
        // appsrc needs telling that more data is coming.
        if self.finished {
            if let Err(e) = self.pipeline.set_state(gst::State::Playing) {
                tracing::warn!(error = %e, "gst decode: could not return to PLAYING after reset");
            }
            self.finished = false;
        }
        self.errors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }
}

/// How long [`H264Decoder::finish`] waits for each remaining frame.
///
/// Generous: this runs once per session, and the alternative to waiting is
/// losing the tail of the stream. Not generous enough to hang a session if the
/// decoder has died, which is the case the bus error path handles.
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

impl Drop for H264Decoder {
    fn drop(&mut self) {
        // Without this the pipeline's threads and the decoder's buffer pool
        // outlive the struct, and the V4L2 device stays busy -- which the next
        // `H264Decoder::new()` in the same process discovers the hard way.
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn make(name: &str) -> Result<gst::Element, H264Error> {
    gst::ElementFactory::make(name).build().map_err(|e| {
        H264Error::Gst(format!(
            "cannot create `{name}`: {e}. gst-plugins-bad provides h264parse and \
             the v4l2codecs plugin"
        ))
    })
}

/// Pull the fd and the plane layout out of a decoded sample.
fn describe(sample: &gst::Sample) -> Result<Layout, H264Error> {
    let buffer = sample
        .buffer()
        .ok_or_else(|| H264Error::Gst("sample carries no buffer".into()))?;
    let caps = sample
        .caps()
        .ok_or_else(|| H264Error::Gst("sample carries no caps".into()))?;

    // DMA_DRM caps, which plain `VideoInfo` cannot parse. `VideoInfoDmaDrm`
    // carries the fourcc and modifier and exposes the ordinary `VideoInfo`
    // underneath for the display size and the default strides.
    let drm = gst_video::VideoInfoDmaDrm::from_caps(caps)
        .map_err(|e| H264Error::Gst(format!("caps are not DMA_DRM: {e}")))?;
    let info = drm
        .to_video_info()
        .map_err(|e| H264Error::Gst(format!("DMA_DRM caps carry no usable VideoInfo: {e}")))?;

    // Requirement 3: display size from the caps, never from the meta.
    let width = info.width();
    let height = info.height();
    let mut offsets = [info.offset()[0] as u64, info.offset()[1] as u64];
    let mut strides = [info.stride()[0] as u64, info.stride()[1] as u64];

    // The meta overrides the layout when the driver padded -- and only the
    // layout. Its `height` is the coded height (1088 at 1080p) and is
    // deliberately not read.
    if let Some(meta) = buffer.meta::<gst_video::VideoMeta>() {
        if meta.n_planes() != 2 {
            return Err(H264Error::Gst(format!(
                "VideoMeta reports {} planes, expected 2 for NV12",
                meta.n_planes()
            )));
        }
        offsets = [meta.offset()[0] as u64, meta.offset()[1] as u64];
        strides = [meta.stride()[0] as u64, meta.stride()[1] as u64];
    }

    if buffer.n_memory() != 1 {
        return Err(H264Error::Gst(format!(
            "buffer has {} memories, expected 1 -- a per-plane-fd layout is not supported",
            buffer.n_memory()
        )));
    }
    let mem = buffer.peek_memory(0);
    let dmabuf = mem
        .downcast_memory_ref::<gstreamer_allocators::DmaBufMemory>()
        .ok_or_else(|| {
            H264Error::Gst("decoded buffer is not dmabuf-backed despite memory:DMABuf caps".into())
        })?;

    Ok(Layout {
        fd: dmabuf.fd(),
        width,
        height,
        offsets,
        strides,
        size: mem.size() as u64,
        modifier: drm.modifier(),
    })
}
