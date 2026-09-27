//! The decoder's output buffers: driver-allocated, exported as dmabufs.
//!
//! `cros-codecs` lets the caller supply the frames its capture queue runs on,
//! through a `VideoFrame` implementation. This is ours. It exists because the
//! crate's only dmabuf-capable frame source is GBM, and **panfrost's GBM
//! cannot allocate NV12 at all** (`tools/hw-probe/gbmprobe.c`: every usage-flag
//! combination fails, so it is the driver and not ChromeOS's
//! `GBM_BO_USE_HW_VIDEO_DECODER` flag), with no `/dev/dma_heap` on this kernel
//! to allocate from instead.
//!
//! So the driver allocates: `V4L2_MEMORY_MMAP` over vb2 `dma_contig`, which is
//! what rkvdec requires anyway, and then `VIDIOC_EXPBUF` per buffer to get a
//! dmabuf fd out. Upstream's `V4l2MmapVideoFrame` is the nearest thing and is
//! not usable: it is CPU-mapped with no export path, and it opens with
//! `todo!("Contiguous formats are not currently supported for MMAP!")`, which
//! NV12 hits.
//!
//! Measured working end to end, byte-identical against a software decode at
//! 640x480 and 1920x1080:
//! `docs/superpowers/investigations/2026-09-26-h264-v4l2-expbuf-feasibility.md`.
//!
//! ## Two things here are load-bearing and neither is obvious
//!
//! **[`V4l2Frame::num_planes`] returns the V4L2 plane count, not the logical
//! one.** rkvdec reports `num_planes=1` for NV12 -- one buffer holding both
//! planes -- and `v4l2r` rejects a QBUF whose handle count disagrees with the
//! queue (`NumPlanesMismatch(2, 1)`). The trait conflates the two meanings of
//! "plane"; `get_plane_size`/`get_plane_pitch`/`map` stay two-entry here,
//! because those *are* the logical planes.
//!
//! **The dmabuf is keyed by V4L2 buffer index, not by frame object** -- see
//! [`ExportTable`]. This is the one that produces confident, wrong output.

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex, Weak};

use cros_codecs::v4l2r::bindings::v4l2_plane;
use cros_codecs::v4l2r::device::Device;
use cros_codecs::v4l2r::ioctl::{expbuf, ExpbufFlags, V4l2Buffer};
use cros_codecs::v4l2r::memory::{MmapHandle, PlaneHandle};
use cros_codecs::v4l2r::Format;
use cros_codecs::video_frame::{ReadMapping, VideoFrame, WriteMapping};
use cros_codecs::{Fourcc, Resolution};

/// `VIDIOC_EXPBUF` results, keyed by **V4L2 buffer index**.
///
/// With `V4L2_MEMORY_MMAP` the driver owns the buffer pool and assigns an index
/// at dequeue time, so one frame object sees several different indices over its
/// life. Exporting per frame object and caching that fd therefore makes the
/// frame read whichever picture now occupies the buffer it happened to see
/// first.
///
/// **The symptom is not corruption, which is why this is a table and not a
/// field.** Every frame comes out a real, correctly-decoded frame -- just the
/// wrong one. In the feasibility probe that was 40 of 60 frames in the wrong
/// order with no single frame looking wrong, which reads as a stutter and sends
/// you hunting in the renderer. Keyed by index it is byte-exact.
///
/// A GPU importer must be keyed the same way: import once per index, look up by
/// index thereafter.
#[derive(Default)]
pub(crate) struct ExportTable {
    by_index: Mutex<HashMap<u32, Arc<File>>>,
}

impl ExportTable {
    /// The dmabuf for `index`, exporting it on first sight.
    ///
    /// Returns `None` rather than erroring: this is called from
    /// `process_dqbuf`, which the `VideoFrame` trait gives no way to fail
    /// from. A missing fd surfaces at [`V4l2Frame::dmabuf_fd`] instead, where
    /// there is a `Result` to put it in.
    fn get_or_export(&self, device: &Device, buf: &V4l2Buffer) -> Option<Arc<File>> {
        let index = buf.index();
        let mut by_index = self.by_index.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = by_index.get(&index) {
            return Some(f.clone());
        }
        match expbuf::<File>(device, buf.queue(), index as usize, 0, ExpbufFlags::RDWR) {
            Ok(f) => {
                tracing::debug!(
                    index,
                    fd = f.as_raw_fd(),
                    "v4l2 decode: exported capture buffer as dmabuf"
                );
                let f = Arc::new(f);
                by_index.insert(index, f.clone());
                Some(f)
            }
            Err(e) => {
                tracing::error!(index, error = %e, "v4l2 decode: VIDIOC_EXPBUF failed");
                None
            }
        }
    }
}

/// A capture buffer the driver allocated, seen as a `cros-codecs` frame.
pub(crate) struct V4l2Frame {
    /// Always NV12 here. Carried rather than assumed because `fourcc()` is
    /// what the trait's own `num_planes`/subsampling defaults read.
    fourcc: Fourcc,
    /// DISPLAY resolution. The coded size lives in the queue format, and the
    /// two differ whenever the height is not 16-aligned -- 1080 display
    /// against 1088 coded on this hardware.
    resolution: Resolution,
    handle: MmapHandle,
    queue_format: Option<Format>,
    buffer: Option<V4l2Buffer>,
    dmabuf: Option<Arc<File>>,
    exports: Arc<ExportTable>,
}

impl fmt::Debug for V4l2Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("V4l2Frame")
            .field("fourcc", &self.fourcc)
            .field("resolution", &self.resolution)
            .field("queue_format", &self.queue_format)
            .field("buffer", &self.buffer)
            .field("dmabuf_fd", &self.dmabuf.as_ref().map(|f| f.as_raw_fd()))
            .finish()
    }
}

impl V4l2Frame {
    pub(crate) fn new(resolution: Resolution, exports: Arc<ExportTable>) -> Self {
        Self {
            fourcc: Fourcc::from(b"NV12"),
            resolution,
            handle: MmapHandle {},
            queue_format: None,
            buffer: None,
            dmabuf: None,
            exports,
        }
    }

    /// Luma row pitch, as the driver reports it.
    pub(crate) fn stride(&self) -> u64 {
        match self.queue_format.as_ref() {
            Some(f) => f.plane_fmt[0].bytesperline as u64,
            None => self.resolution.width as u64,
        }
    }

    /// The height the driver actually laid the luma plane out at.
    ///
    /// **Not** derived from `sizeimage`: rkvdec reports `sizeimage=614400` for
    /// a 640x480 NV12 whose pixels occupy 460800, the remainder being zero
    /// scratch, so dividing it out yields a height of 640 for a 480-row frame.
    /// **Not** the display height either, whenever that is not 16-aligned:
    /// 1080p reports display 1080 against coded 1088, and chroma starts at
    /// `bytesperline * 1088`.
    pub(crate) fn coded_height(&self) -> u64 {
        match self.queue_format.as_ref() {
            Some(f) => f.height as u64,
            None => self.resolution.height as u64,
        }
    }

    /// Byte offset of the interleaved chroma plane inside the one buffer.
    pub(crate) fn chroma_offset(&self) -> u64 {
        self.stride() * self.coded_height()
    }

    /// Size of the whole dmabuf object, from the driver.
    ///
    /// Authoritative for an importer sizing an allocation: it includes any
    /// padding past the last chroma row, which `chroma_offset + stride *
    /// chroma_rows` would drop.
    pub(crate) fn total_size(&self) -> u64 {
        match self.buffer.as_ref() {
            Some(b) => b.as_v4l2_planes()[0].length as u64,
            None => self.chroma_offset() * 3 / 2,
        }
    }

    pub(crate) fn display_size(&self) -> (u32, u32) {
        (self.resolution.width, self.resolution.height)
    }

    /// The exported dmabuf fd for the buffer this frame currently holds.
    ///
    /// Borrowed, not owned: it belongs to the [`ExportTable`], which outlives
    /// every frame. A caller handing it to anything that takes ownership of an
    /// fd must `dup()` first -- see [`crate::DmabufPlanes::fd`].
    pub(crate) fn dmabuf_fd(&self) -> Option<RawFd> {
        self.dmabuf.as_ref().map(|f| f.as_raw_fd())
    }

    /// Whether this frame has been through a dequeue and so knows its layout.
    pub(crate) fn is_populated(&self) -> bool {
        self.buffer.is_some() && self.dmabuf.is_some()
    }
}

/// A CPU view of the exported dmabuf, split into the two NV12 planes.
///
/// Only [`crate::decoder::HwFrame::download_nv12`] and the exactness oracle use
/// this; the render path imports the fd into the GPU and never maps anything.
struct DmabufMapping {
    base: *mut libc::c_void,
    len: usize,
    planes: Vec<(usize, usize)>,
}

impl Drop for DmabufMapping {
    fn drop(&mut self) {
        // SAFETY: `base`/`len` are the exact pair returned by the `mmap` in
        // `map_planes`, and this runs once.
        unsafe { libc::munmap(self.base, self.len) };
    }
}

impl<'a> ReadMapping<'a> for DmabufMapping {
    fn get(&self) -> Vec<&[u8]> {
        self.planes
            .iter()
            .map(|&(offset, len)| {
                // SAFETY: `map_planes` checked every (offset, len) lies inside
                // the mapping before constructing `self`.
                unsafe { std::slice::from_raw_parts((self.base as *const u8).add(offset), len) }
            })
            .collect()
    }
}

/// Never constructed: [`V4l2Frame::map_mut`] refuses instead of handing one
/// back, so the unwritable case is an error at the call site rather than a
/// panic somewhere inside a `get()` the caller has already committed to.
impl<'a> WriteMapping<'a> for DmabufMapping {
    fn get(&self) -> Vec<std::cell::RefCell<&'a mut [u8]>> {
        Vec::new()
    }
}

fn map_planes(frame: &V4l2Frame) -> Result<DmabufMapping, String> {
    let file = frame
        .dmabuf
        .as_ref()
        .ok_or("no dmabuf exported for this frame")?;
    let len = usize::try_from(frame.total_size()).map_err(|_| "buffer size overflows usize")?;
    if len == 0 {
        return Err("driver reported a zero-length buffer".into());
    }
    let sizes = frame.get_plane_size();
    let offsets = [
        0usize,
        usize::try_from(frame.chroma_offset()).unwrap_or(usize::MAX),
    ];
    for (i, &offset) in offsets.iter().enumerate() {
        let end = offset
            .checked_add(sizes[i])
            .ok_or("plane extent overflows")?;
        if end > len {
            return Err(format!(
                "plane {i} extends to byte {end}, past the buffer's {len} bytes"
            ));
        }
    }
    // SAFETY: mapping a dmabuf exported from a vb2 `dma_contig` buffer, for
    // `len` bytes the driver reported as that plane's length. Read-only, and
    // unmapped exactly once in `DmabufMapping::drop`.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(format!(
            "mmap of dmabuf failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(DmabufMapping {
        base,
        len,
        planes: vec![(offsets[0], sizes[0]), (offsets[1], sizes[1])],
    })
}

impl VideoFrame for V4l2Frame {
    type NativeHandle = MmapHandle;

    fn fourcc(&self) -> Fourcc {
        self.fourcc
    }

    fn resolution(&self) -> Resolution {
        self.resolution
    }

    /// **V4L2** planes, not logical ones -- see the module docs. One, because
    /// this backend targets NV12 on a driver that reports a single plane, which
    /// is what rkvdec does; a driver reporting two is rejected in
    /// `process_dqbuf` rather than silently mislaid here.
    fn num_planes(&self) -> usize {
        1
    }

    fn get_plane_size(&self) -> Vec<usize> {
        let luma = (self.stride() * self.coded_height()) as usize;
        vec![luma, luma / 2]
    }

    fn get_plane_pitch(&self) -> Vec<usize> {
        let stride = self.stride() as usize;
        vec![stride, stride]
    }

    fn map<'a>(&'a self) -> Result<Box<dyn ReadMapping<'a> + 'a>, String> {
        Ok(Box::new(map_planes(self)?))
    }

    fn map_mut<'a>(&'a mut self) -> Result<Box<dyn WriteMapping<'a> + 'a>, String> {
        Err("decoder output buffers are mapped read-only".into())
    }

    fn fill_v4l2_plane(&self, _index: usize, plane: &mut v4l2_plane) {
        // A no-op for MMAP: the buffer index is the whole identity.
        self.handle.fill_v4l2_plane(plane)
    }

    fn process_dqbuf(&mut self, device: Arc<Device>, format: &Format, buf: &V4l2Buffer) {
        if buf.num_planes() != 1 {
            tracing::error!(
                planes = buf.num_planes(),
                "v4l2 decode: driver reports a multi-plane capture buffer; \
                 this backend only handles single-plane NV12"
            );
            self.dmabuf = None;
            return;
        }
        self.dmabuf = self.exports.get_or_export(device.as_ref(), buf);
        self.queue_format = Some(format.clone());
        self.buffer = Some(buf.clone());
    }
}

/// A bounded supply of [`V4l2Frame`]s, recycled on drop.
///
/// Not `cros_codecs::video_frame::frame_pool::FramePool`, for a specific
/// reason: its `PooledVideoFrame` wrapper delegates every other defaulted
/// `VideoFrame` method but **not** `num_planes`, so the override this backend
/// depends on is silently discarded behind it and QBUF fails
/// `NumPlanesMismatch(2, 1)`. Owning the pool here delegates everything and
/// keeps the fix in code we control rather than in a carried patch.
///
/// The bound is what gives the decoder backpressure: [`Self::alloc`] returning
/// `None` is how `cros-codecs` learns to stop and let the caller drain, and it
/// is a cheaper signal than letting the capture queue run out of free buffers
/// underneath it. The count is **not** ghostframe's to choose -- see
/// [`Self::resize`].
pub(crate) struct FramePool {
    free: Arc<Mutex<Vec<V4l2Frame>>>,
    exports: Arc<ExportTable>,
}

impl FramePool {
    pub(crate) fn new(exports: Arc<ExportTable>) -> Self {
        Self {
            free: Arc::new(Mutex::new(Vec::new())),
            exports,
        }
    }

    /// Refill for a new stream format, discarding what was there.
    ///
    /// `count` comes from `cros-codecs`' `StreamInfo::min_num_frames`, i.e.
    /// from the driver by way of the codec's DPB requirement -- not from
    /// `Config::n_export_buffers` or any other ghostframe constant. The capture
    /// queue then asks the driver for `count + 2` ("+2 due to HCMP1_HHI_A.h264
    /// needing more"), and on this hardware 5 requested came back as 7.
    /// Allocating more frames here than the driver has buffers would only move
    /// the failure from a clean `None` to a queue error.
    pub(crate) fn resize(&mut self, resolution: Resolution, count: usize) {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        free.clear();
        for _ in 0..count {
            free.push(V4l2Frame::new(resolution, self.exports.clone()));
        }
    }

    /// One frame, or `None` when they are all still in flight.
    pub(crate) fn alloc(&self) -> Option<PooledFrame> {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        let inner = free.pop()?;
        Some(PooledFrame {
            inner: Some(inner),
            pool: Arc::downgrade(&self.free),
        })
    }
}

/// A frame on loan from a [`FramePool`], returned when dropped.
pub(crate) struct PooledFrame {
    /// `Option` only so `Drop` can move the frame back out. Never `None` while
    /// the value is alive, which is what lets the accessors below expect it.
    inner: Option<V4l2Frame>,
    pool: Weak<Mutex<Vec<V4l2Frame>>>,
}

impl PooledFrame {
    fn get(&self) -> &V4l2Frame {
        self.inner
            .as_ref()
            .expect("PooledFrame used after Drop moved its frame out")
    }

    fn get_mut(&mut self) -> &mut V4l2Frame {
        self.inner
            .as_mut()
            .expect("PooledFrame used after Drop moved its frame out")
    }

    pub(crate) fn stride(&self) -> u64 {
        self.get().stride()
    }

    pub(crate) fn chroma_offset(&self) -> u64 {
        self.get().chroma_offset()
    }

    pub(crate) fn total_size(&self) -> u64 {
        self.get().total_size()
    }

    pub(crate) fn display_size(&self) -> (u32, u32) {
        self.get().display_size()
    }

    pub(crate) fn dmabuf_fd(&self) -> Option<RawFd> {
        self.get().dmabuf_fd()
    }

    pub(crate) fn is_populated(&self) -> bool {
        self.get().is_populated()
    }
}

impl fmt::Debug for PooledFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.get(), f)
    }
}

impl Drop for PooledFrame {
    fn drop(&mut self) {
        if let (Some(frame), Some(pool)) = (self.inner.take(), self.pool.upgrade()) {
            pool.lock().unwrap_or_else(|e| e.into_inner()).push(frame);
        }
    }
}

/// Delegates **every** method, including the defaulted ones. That is the whole
/// reason this wrapper exists rather than `PooledVideoFrame`.
impl VideoFrame for PooledFrame {
    type NativeHandle = MmapHandle;

    fn fourcc(&self) -> Fourcc {
        self.get().fourcc()
    }

    fn resolution(&self) -> Resolution {
        self.get().resolution()
    }

    fn num_planes(&self) -> usize {
        self.get().num_planes()
    }

    fn get_plane_size(&self) -> Vec<usize> {
        self.get().get_plane_size()
    }

    fn get_plane_pitch(&self) -> Vec<usize> {
        self.get().get_plane_pitch()
    }

    fn map<'a>(&'a self) -> Result<Box<dyn ReadMapping<'a> + 'a>, String> {
        self.get().map()
    }

    fn map_mut<'a>(&'a mut self) -> Result<Box<dyn WriteMapping<'a> + 'a>, String> {
        self.get_mut().map_mut()
    }

    fn fill_v4l2_plane(&self, index: usize, plane: &mut v4l2_plane) {
        self.get().fill_v4l2_plane(index, plane)
    }

    fn process_dqbuf(&mut self, device: Arc<Device>, format: &Format, buf: &V4l2Buffer) {
        self.get_mut().process_dqbuf(device, format, buf)
    }
}
