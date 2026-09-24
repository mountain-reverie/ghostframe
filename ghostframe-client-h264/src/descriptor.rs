//! DRM descriptor parsing. Filled in by Task 4.
//!
//! For now this holds only the plain-data boundary types that `lib.rs`
//! re-exports: no ffmpeg types, so `ghostframe-client-gpu` can be tested
//! against synthetic planes with no decoder in the loop.

/// One plane's position inside the dmabuf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneDesc {
    pub offset: u64,
    pub pitch: u64,
}

/// An NV12 dmabuf: one fd, luma plane, chroma plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmabufPlanes {
    /// Borrowed from the mapped `AVFrame`, which owns it, and valid only while
    /// that `MappedFrame` is alive.
    ///
    /// **An importer must `dup()` this before any call that takes ownership.**
    /// `vkImportMemoryFdKHR` takes ownership: the fd is closed by
    /// `vkFreeMemory`, so handing this one over directly double-closes it
    /// against the `AVFrame`'s own unref. The design imports the same dmabuf
    /// twice (luma and chroma planes), which would be two closes of one fd.
    /// The future importer (Task 6's `import.rs`) must `dup()` this fd for
    /// exactly this reason.
    pub fd: i32,
    pub modifier: u64,
    /// DISPLAY dimensions (`AVFrame::width`/`height`), not the coded size.
    /// The alignment padding lives in `PlaneDesc::pitch`, which is why both
    /// are carried separately: a 640-wide frame here has pitch 768.
    pub width: u32,
    pub height: u32,
    pub luma: PlaneDesc,
    /// `width.div_ceil(2)` x `height.div_ceil(2)` samples, two bytes each (U
    /// and V interleaved). `div_ceil`, not `/ 2`: this client is deliberately
    /// tested at non-16-aligned resolutions, where truncation loses the last
    /// chroma column.
    pub chroma: PlaneDesc,
}
