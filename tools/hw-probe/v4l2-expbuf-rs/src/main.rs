//! Feasibility probe: can a dmabuf-exporting V4L2 `VideoFrame` be written
//! *downstream* of cros-codecs, with no fork beyond the device-path override?
//!
//! Lets the driver allocate (V4L2_MEMORY_MMAP, vb2 dma_contig -- what rkvdec
//! requires), then VIDIOC_EXPBUF each buffer to get a dmabuf fd. Reads the
//! decoded NV12 back *through the exported fd* so a byte-exact comparison
//! against a software golden validates both the fd and the plane layout.

use std::cell::RefCell;
use std::fmt;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use cros_codecs::bitstream_utils::NalIterator;
use cros_codecs::codec::h264::parser::Nalu as H264Nalu;
use cros_codecs::decoder::stateless::h264::H264;
use cros_codecs::decoder::stateless::StatelessDecoder;
use cros_codecs::decoder::stateless::StatelessVideoDecoder;
use cros_codecs::decoder::BlockingMode;
use cros_codecs::decoder::DecodedHandle;
use cros_codecs::decoder::DecoderEvent;
use cros_codecs::decoder::StreamInfo;
use cros_codecs::image_processing::nv12_to_i420;
use cros_codecs::utils::align_up;
use cros_codecs::video_frame::frame_pool::FramePool;
use cros_codecs::video_frame::{ReadMapping, VideoFrame, WriteMapping};
use cros_codecs::video_frame::{UV_PLANE, Y_PLANE};
use cros_codecs::{Fourcc, Resolution};

use cros_codecs::v4l2r::bindings::v4l2_plane;
use cros_codecs::v4l2r::device::Device;
use cros_codecs::v4l2r::ioctl::{expbuf, ExpbufFlags, V4l2Buffer};
use cros_codecs::v4l2r::memory::{MmapHandle, PlaneHandle};
use cros_codecs::v4l2r::Format;

use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};

// ---------------------------------------------------------------- the frame

/// A CPU view of the exported dmabuf, split into NV12 planes.
struct ExpbufMapping {
    addr: std::ptr::NonNull<std::ffi::c_void>,
    len: usize,
    plane_offsets: Vec<usize>,
    plane_sizes: Vec<usize>,
}

impl Drop for ExpbufMapping {
    fn drop(&mut self) {
        // SAFETY: `addr`/`len` come from the matching `mmap` in `map_helper`.
        unsafe { munmap(self.addr, self.len) }.expect("munmap failed");
    }
}

impl<'a> ReadMapping<'a> for ExpbufMapping {
    fn get(&self) -> Vec<&[u8]> {
        let base = self.addr.as_ptr() as *const u8;
        (0..self.plane_offsets.len())
            .map(|i| {
                // SAFETY: offsets and sizes are inside the mapping, checked in map_helper.
                unsafe { std::slice::from_raw_parts(base.add(self.plane_offsets[i]), self.plane_sizes[i]) }
            })
            .collect()
    }
}

impl<'a> WriteMapping<'a> for ExpbufMapping {
    fn get(&self) -> Vec<RefCell<&'a mut [u8]>> {
        unimplemented!("probe reads only")
    }
}

pub struct V4l2ExpbufVideoFrame {
    fourcc: Fourcc,
    resolution: Resolution,
    handle: MmapHandle,
    device: Option<Arc<Device>>,
    queue_format: Option<Format>,
    buffer: Option<V4l2Buffer>,
    /// The dmabuf of the V4L2 buffer index this frame was last dequeued with.
    ///
    /// Keyed by INDEX, not by frame object. With V4L2_MEMORY_MMAP the driver
    /// owns the buffer pool and hands an index out at dequeue time, so a frame
    /// object sees *different* indices over its life -- caching the fd of the
    /// first index it ever saw makes it read whatever picture now occupies that
    /// buffer. That reads as a frame-ordering glitch, not as corruption:
    /// every frame looks like a real frame, just the wrong one.
    dmabuf: Option<Arc<File>>,
}

/// index -> exported dmabuf, so EXPBUF runs once per buffer rather than once
/// per frame object. This is also the table a GPU importer must be keyed on.
fn export_table() -> &'static std::sync::Mutex<std::collections::HashMap<u32, Arc<File>>> {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u32, Arc<File>>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

impl fmt::Debug for V4l2ExpbufVideoFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("V4l2ExpbufVideoFrame")
            .field("fourcc", &self.fourcc)
            .field("resolution", &self.resolution)
            .field("queue_format", &self.queue_format)
            .field("buffer", &self.buffer)
            .field("dmabuf_fd", &self.dmabuf.as_ref().map(|f| f.as_raw_fd()))
            .finish()
    }
}

impl V4l2ExpbufVideoFrame {
    pub fn new(fourcc: Fourcc, resolution: Resolution) -> Self {
        Self {
            fourcc,
            resolution,
            handle: MmapHandle {},
            device: None,
            queue_format: None,
            buffer: None,
            dmabuf: None,
        }
    }

    /// Row pitch of the luma plane, as the driver reports it.
    fn stride(&self) -> usize {
        match self.queue_format.as_ref() {
            Some(f) => f.plane_fmt[0].bytesperline as usize,
            None => self.resolution.width as usize,
        }
    }

    /// The height the driver actually laid the luma plane out at. NOT derived
    /// from `sizeimage`: rkvdec reports sizeimage 614400 for 640x480 NV12,
    /// where the pixel data occupies only 460800 and the rest is scratch.
    fn coded_height(&self) -> usize {
        match self.queue_format.as_ref() {
            Some(f) => f.height as usize,
            None => self.resolution.height as usize,
        }
    }

    /// Byte offset of each NV12 plane inside the single exported buffer.
    fn plane_offsets(&self) -> Vec<usize> {
        // Mutation hook: the whole point of comparing against a software golden
        // is that a nearly-correct chroma offset must fail, not merely look
        // plausible. `CHROMA_SHIFT_ROWS=1` is the perturbation to check that.
        let shift: usize = std::env::var("CHROMA_SHIFT_ROWS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        vec![0, self.stride() * (self.coded_height() + shift)]
    }

    fn total_len(&self) -> usize {
        match self.buffer.as_ref() {
            Some(b) => b.as_v4l2_planes()[0].length as usize,
            None => self.stride() * self.coded_height() * 3 / 2,
        }
    }

    fn map_helper(&self) -> Result<ExpbufMapping, String> {
        let file = self.dmabuf.as_ref().ok_or("no exported dmabuf yet")?;
        let len = self.total_len();
        let offsets = self.plane_offsets();
        let sizes = self.get_plane_size();
        let end = offsets[UV_PLANE] + sizes[UV_PLANE];
        if end > len {
            return Err(format!("plane layout {end} exceeds buffer {len}"));
        }
        // SAFETY: mapping a dmabuf exported from a vb2 dma_contig buffer; `len`
        // is the length the driver reported for that plane.
        let addr = unsafe {
            mmap(
                None,
                std::num::NonZeroUsize::new(len).ok_or("zero-length buffer")?,
                ProtFlags::PROT_READ,
                MapFlags::MAP_SHARED,
                file,
                0,
            )
        }
        .map_err(|e| format!("mmap of dmabuf failed: {e}"))?;
        Ok(ExpbufMapping { addr, len, plane_offsets: offsets, plane_sizes: sizes })
    }

    pub fn dmabuf_fd(&self) -> Option<i32> {
        self.dmabuf.as_ref().map(|f| f.as_raw_fd())
    }
}

impl VideoFrame for V4l2ExpbufVideoFrame {
    type NativeHandle = MmapHandle;

    fn fourcc(&self) -> Fourcc {
        self.fourcc.clone()
    }

    fn resolution(&self) -> Resolution {
        self.resolution.clone()
    }

    /// **V4L2** planes, not logical ones. rkvdec reports `num_planes=1` for
    /// NV12 -- one buffer holding both planes -- and `queue_with_handles`
    /// rejects a QBUF whose handle count disagrees with the queue
    /// (`NumPlanesMismatch(2, 1)`). The trait conflates the two meanings,
    /// which is why upstream's `V4l2MmapVideoFrame` just `todo!()`s on any
    /// contiguous format. `get_plane_size`/`get_plane_pitch`/`map` below stay
    /// two-entry, because those *are* the logical planes.
    fn num_planes(&self) -> usize {
        1
    }

    fn get_plane_size(&self) -> Vec<usize> {
        let luma = self.stride() * self.coded_height();
        vec![luma, luma / 2]
    }

    fn get_plane_pitch(&self) -> Vec<usize> {
        vec![self.stride(), self.stride()]
    }

    fn map<'a>(&'a self) -> Result<Box<dyn ReadMapping<'a> + 'a>, String> {
        Ok(Box::new(self.map_helper()?))
    }

    fn map_mut<'a>(&'a mut self) -> Result<Box<dyn WriteMapping<'a> + 'a>, String> {
        Ok(Box::new(self.map_helper()?))
    }

    fn fill_v4l2_plane(&self, _index: usize, plane: &mut v4l2_plane) {
        self.handle.fill_v4l2_plane(plane)
    }

    fn process_dqbuf(&mut self, device: Arc<Device>, format: &Format, buf: &V4l2Buffer) {
        let index = buf.index();
        let mut table = export_table().lock().unwrap();
        let entry = match table.get(&index) {
            Some(f) => Some(f.clone()),
            None => match expbuf::<File>(
                device.as_ref(),
                buf.queue(),
                index as usize,
                0,
                ExpbufFlags::RDWR,
            ) {
                Ok(f) => {
                    log::info!("EXPBUF buffer {index} plane 0 -> dmabuf fd {}", f.as_raw_fd());
                    let f = Arc::new(f);
                    table.insert(index, f.clone());
                    Some(f)
                }
                Err(e) => {
                    log::error!("EXPBUF failed for buffer {index}: {e}");
                    None
                }
            },
        };
        drop(table);
        self.dmabuf = entry;
        self.device = Some(device);
        self.queue_format = Some(format.clone());
        self.buffer = Some(buf.clone());
    }
}

// ---------------------------------------------------------------- the probe

fn main() {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let input_path = args.next().expect("usage: expbuf-rs <in.h264> <out.i420>");
    let output_path = args.next().expect("usage: expbuf-rs <in.h264> <out.i420>");

    let input = std::fs::read(&input_path).expect("cannot read input");
    let mut output = File::create(&output_path).expect("cannot create output");

    let framepool = std::sync::Arc::new(std::sync::Mutex::new(FramePool::new(
        |stream_info: &StreamInfo| {
            log::info!(
                "pool alloc: display {:?} coded {:?}",
                stream_info.display_resolution,
                stream_info.coded_resolution
            );
            V4l2ExpbufVideoFrame::new(Fourcc::from(b"NV12"), stream_info.display_resolution)
        },
    )));

    let mut decoder = StatelessDecoder::<H264, _>::new_v4l2(BlockingMode::NonBlocking)
        .expect("cannot create decoder")
        .into_trait_object();

    let pool_for_alloc = framepool.clone();
    let mut frames = 0usize;

    let on_frame = |handle: &dyn DecodedHandle<Frame = cros_codecs::video_frame::frame_pool::PooledVideoFrame<V4l2ExpbufVideoFrame>>,
                    output: &mut File,
                    frames: &mut usize| {
        // `video_frame()` panics unless the request reached Done.
        handle.sync().expect("sync failed");
        let frame = handle.video_frame();
        let width = frame.resolution().width as usize;
        let height = frame.resolution().height as usize;
        let luma = width * height;
        let chroma = align_up(width, 2) / 2 * (align_up(height, 2) / 2);
        let mut buf = vec![0u8; luma + 2 * chroma];
        {
            let pitches = frame.get_plane_pitch();
            let mapping = frame.map().expect("cannot map decoded frame");
            let planes = mapping.get();
            let (dst_y, rest) = buf.split_at_mut(luma);
            let (dst_u, dst_v) = rest.split_at_mut(chroma);
            nv12_to_i420(
                planes[Y_PLANE],
                pitches[Y_PLANE],
                dst_y,
                width,
                planes[UV_PLANE],
                pitches[UV_PLANE],
                dst_u,
                align_up(width, 2) / 2,
                dst_v,
                align_up(width, 2) / 2,
                width,
                height,
            );
        }
        use std::io::Write;
        output.write_all(&buf).expect("cannot write output");
        *frames += 1;
    };

    // Timestamps key the decoder's in-flight request map, so they must be
    // unique per submission -- keying them off the *output* frame count
    // collides (several pictures share timestamp 0) and blows up in
    // `try_dequeue_capture_buffers`'s `requests.remove(&timestamp).unwrap()`.
    let mut au_index = 0u64;
    for nalu in NalIterator::<H264Nalu>::new(&input) {
        au_index += 1;
        let mut bitstream: &[u8] = &nalu;
        loop {
            match decoder.decode(au_index, bitstream, &mut || {
                pool_for_alloc.lock().unwrap().alloc()
            }) {
                Ok(n) if n == bitstream.len() => break,
                Ok(n) => bitstream = &bitstream[n..],
                Err(cros_codecs::decoder::stateless::DecodeError::CheckEvents)
                | Err(cros_codecs::decoder::stateless::DecodeError::NotEnoughOutputBuffers(_)) => {
                    while let Some(event) = decoder.next_event() {
                        match event {
                            DecoderEvent::FrameReady(h) => on_frame(&h, &mut output, &mut frames),
                            DecoderEvent::FormatChanged => {
                                let info = decoder.stream_info().expect("no stream info").clone();
                                log::info!("format changed: {info:?}");
                                framepool.lock().unwrap().resize(&info);
                            }
                        }
                    }
                }
                Err(e) => panic!("decode error: {e:?}"),
            }
        }
        while let Some(event) = decoder.next_event() {
            match event {
                DecoderEvent::FrameReady(h) => on_frame(&h, &mut output, &mut frames),
                DecoderEvent::FormatChanged => {
                    let info = decoder.stream_info().expect("no stream info").clone();
                    log::info!("format changed: {info:?}");
                    framepool.lock().unwrap().resize(&info);
                }
            }
        }
    }

    decoder.flush().expect("flush failed");
    while let Some(event) = decoder.next_event() {
        match event {
            DecoderEvent::FrameReady(h) => on_frame(&h, &mut output, &mut frames),
            DecoderEvent::FormatChanged => {}
        }
    }
    println!("decoded {frames} frames");
}
