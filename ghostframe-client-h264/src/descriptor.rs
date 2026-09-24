//! `AVDRMFrameDescriptor` -> a plain struct with no ffmpeg types in it.
//!
//! This is the crate boundary: `ghostframe-client-gpu` consumes
//! [`DmabufPlanes`] and never links ffmpeg.
//!
//! Drivers describe an NV12 dmabuf in two equivalent shapes -- one layer
//! with two planes, or two layers with one plane each -- and which one you
//! get is not something to depend on. Both are normalised here, and *only*
//! those two shapes: anything else (a different plane count, or a
//! different-but-plane-count-compatible format like P010) is rejected by
//! its DRM fourcc, not silently accepted because the numbers happened to
//! line up.

use crate::H264Error;
use ffmpeg_sys_next as ffi;

/// Pack four ASCII bytes into a DRM fourcc the way `<drm/drm_fourcc.h>`'s
/// `fourcc_code` macro does: little-endian, first character in the low byte.
const fn fourcc_code(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// `DRM_FORMAT_NV12`: one layer, luma and interleaved chroma as two planes
/// of the same composed format. What a single-object, single-layer VA-API
/// export uses.
pub const DRM_FORMAT_NV12: u32 = fourcc_code(b'N', b'V', b'1', b'2');

/// `DRM_FORMAT_R8`: one 8-bit sample per pixel. The luma layer's format in
/// the two-layer export shape. Verified against the Task 1 spike's
/// recorded `layer[0] format=0x20203852` (spec Sec 7.1).
pub const DRM_FORMAT_R8: u32 = fourcc_code(b'R', b'8', b' ', b' ');

/// `DRM_FORMAT_GR88`: two interleaved 8-bit samples per pixel (U then V).
/// The chroma layer's format in the two-layer export shape. Verified
/// against the Task 1 spike's recorded `layer[1] format=0x38385247`.
pub const DRM_FORMAT_GR88: u32 = fourcc_code(b'G', b'R', b'8', b'8');

/// One plane's position inside the dmabuf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneDesc {
    pub offset: u64,
    pub pitch: u64,
}

/// An NV12 dmabuf: one fd, luma plane, chroma plane.
///
/// **Why `fd: i32`, not `BorrowedFd<'a>`.** A lifetime parameter would make
/// the escape described below a compile error, and would let an importer
/// use `try_clone_to_owned()` instead of `libc::dup`. It was rejected
/// because this struct is also the plain literal-construction type
/// synthetic tests build by hand with no `AVFrame` (and no VA-API) behind
/// them at all, and a lifetime here would infect every
/// `ghostframe-client-gpu` signature that carries a `DmabufPlanes`.
/// [`crate::decoder::MappedFrame::planes`] borrowing `&self` is the
/// compromise instead: it makes the common misuse -- using the fd after
/// the mapping that owns it is dropped -- a borrow-checker error at the
/// call site that constructs the value, while leaving `DmabufPlanes` itself
/// lifetime-free everywhere else.
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
    /// Total size of the dmabuf object, straight from
    /// `AVDRMObjectDescriptor::size`. Authoritative -- an importer sizing
    /// its `VkDeviceMemory` allocation from `chroma.offset + chroma.pitch *
    /// height/2` instead would silently drop any padding the driver put
    /// past the last chroma row, and undersize the allocation.
    pub size: u64,
    /// DISPLAY dimensions (`AVFrame::width`/`height`), not the coded size.
    /// The alignment padding lives in `PlaneDesc::pitch`, which is why both
    /// are carried separately: a 640-wide frame here has pitch 768.
    pub width: u32,
    pub height: u32,
    pub luma: PlaneDesc,
    /// `width.div_ceil(2)` x `height.div_ceil(2)` samples, two bytes each (U
    /// and V interleaved). `div_ceil`, not `/ 2`: this client is deliberately
    /// tested at non-16-aligned resolutions, where truncation loses the last
    /// chroma column. See [`Self::chroma_width`] / [`Self::chroma_height`].
    pub chroma: PlaneDesc,
    /// `DRM_FORMAT_R8` in both accepted shapes -- the composed
    /// [`DRM_FORMAT_NV12`] layer decomposes to R8 luma + GR88 chroma by
    /// definition, so this is synthesized rather than read off the wire
    /// when the driver reports the single-layer shape.
    pub fourcc_luma: u32,
    /// `DRM_FORMAT_GR88` in both accepted shapes. See `fourcc_luma`.
    pub fourcc_chroma: u32,
}

impl DmabufPlanes {
    /// Build from a mapped DRM_PRIME descriptor.
    ///
    /// Not `unsafe`: every operation on `d` here is a bounds-checked read of
    /// a `&T` the caller already holds safely. The one unsafe step in this
    /// path -- dereferencing the raw `AVDRMFrameDescriptor*` ffmpeg hands
    /// back -- happens once, in [`crate::decoder::HwFrame::map_dmabuf`],
    /// with its own SAFETY comment; this function starts from a safe
    /// reference and has no more business claiming an unsafe contract than
    /// any other function that reads a struct.
    ///
    /// Accepts exactly the two shapes VA-API/DRM drivers use to describe
    /// NV12 -- one `DRM_FORMAT_NV12` layer with two planes, or a
    /// `DRM_FORMAT_R8` layer followed by a `DRM_FORMAT_GR88` layer, one
    /// plane each -- and validates both by fourcc, not by plane count
    /// alone: a P010 (10-bit) export also has two planes and would
    /// otherwise parse cleanly as NV12 and get rendered as garbage three
    /// tasks later. Every count read from `d` is compared directly rather
    /// than used to index or bound a loop, so a garbage
    /// `nb_layers`/`nb_planes` (negative, or larger than the fixed `[_; 4]`
    /// arrays hold) reports a `Descriptor` error instead of indexing out of
    /// bounds -- this runs per frame, on Task 9's render thread, where a
    /// panic takes the session down over a decode the driver merely
    /// described oddly.
    pub fn from_descriptor(
        d: &ffi::AVDRMFrameDescriptor,
        width: u32,
        height: u32,
    ) -> Result<Self, H264Error> {
        if d.nb_objects != 1 {
            return Err(H264Error::Descriptor(format!(
                "expected 1 dmabuf object, got {} -- the import path binds one fd",
                d.nb_objects
            )));
        }
        let size = d.objects[0].size as u64;

        let (luma, chroma, fourcc_luma, fourcc_chroma) = match d.nb_layers {
            1 => {
                let layer = &d.layers[0];
                if layer.format != DRM_FORMAT_NV12 {
                    return Err(H264Error::Descriptor(format!(
                        "single-layer export must be DRM_FORMAT_NV12 (0x{DRM_FORMAT_NV12:08x}), \
                         got 0x{:08x}",
                        layer.format
                    )));
                }
                if layer.nb_planes != 2 {
                    return Err(H264Error::Descriptor(format!(
                        "a DRM_FORMAT_NV12 layer must have 2 planes, got {}",
                        layer.nb_planes
                    )));
                }
                let luma = plane_desc(&layer.planes[0])?;
                let chroma = plane_desc(&layer.planes[1])?;
                (luma, chroma, DRM_FORMAT_R8, DRM_FORMAT_GR88)
            }
            2 => {
                let y = &d.layers[0];
                let uv = &d.layers[1];
                if y.format != DRM_FORMAT_R8 || uv.format != DRM_FORMAT_GR88 {
                    return Err(H264Error::Descriptor(format!(
                        "two-layer export must be DRM_FORMAT_R8 (0x{DRM_FORMAT_R8:08x}) then \
                         DRM_FORMAT_GR88 (0x{DRM_FORMAT_GR88:08x}), got 0x{:08x} then 0x{:08x}",
                        y.format, uv.format
                    )));
                }
                if y.nb_planes != 1 || uv.nb_planes != 1 {
                    return Err(H264Error::Descriptor(format!(
                        "each layer of a two-layer NV12 export must have exactly 1 plane, \
                         got {} and {}",
                        y.nb_planes, uv.nb_planes
                    )));
                }
                let luma = plane_desc(&y.planes[0])?;
                let chroma = plane_desc(&uv.planes[0])?;
                (luma, chroma, DRM_FORMAT_R8, DRM_FORMAT_GR88)
            }
            n => {
                return Err(H264Error::Descriptor(format!(
                    "expected 1 (composed DRM_FORMAT_NV12) or 2 (R8+GR88) layers, got {n}"
                )));
            }
        };

        // The single most valuable invariant available at this boundary:
        // every plane must actually fit inside the object the driver said
        // it allocated. Free to check here; otherwise diagnosed, if at all,
        // as an opaque GPU fault three tasks later.
        check_extent(luma, height as u64, size, "luma")?;
        check_extent(chroma, (height as u64).div_ceil(2), size, "chroma")?;

        Ok(DmabufPlanes {
            fd: d.objects[0].fd,
            modifier: d.objects[0].format_modifier,
            size,
            width,
            height,
            luma,
            chroma,
            fourcc_luma,
            fourcc_chroma,
        })
    }

    /// Chroma plane width in samples: `width.div_ceil(2)`. A method, not
    /// just a doc comment on `chroma`, so a caller writes `planes.width /
    /// 2` nowhere and gets the truncating version by copy-paste on a
    /// non-16-aligned resolution.
    pub fn chroma_width(&self) -> u32 {
        self.width.div_ceil(2)
    }

    /// Chroma plane height in samples: `height.div_ceil(2)`. See
    /// [`Self::chroma_width`].
    pub fn chroma_height(&self) -> u32 {
        self.height.div_ceil(2)
    }
}

/// Read one plane, checked against the two ways a DRM plane descriptor can
/// point somewhere this crate's single-fd import cannot follow.
fn plane_desc(p: &ffi::AVDRMPlaneDescriptor) -> Result<PlaneDesc, H264Error> {
    if p.object_index != 0 {
        return Err(H264Error::Descriptor(format!(
            "plane references object {}, but only object 0 was imported",
            p.object_index
        )));
    }
    // `offset`/`pitch` are `isize` in the ffmpeg binding; a negative value
    // cast straight to `u64` would wrap into an enormous positive one and
    // sail into Task 6's `vkBindImageMemory` as a huge, wrong offset.
    let offset = u64::try_from(p.offset)
        .map_err(|_| H264Error::Descriptor(format!("plane offset {} is negative", p.offset)))?;
    let pitch = u64::try_from(p.pitch)
        .map_err(|_| H264Error::Descriptor(format!("plane pitch {} is negative", p.pitch)))?;
    Ok(PlaneDesc { offset, pitch })
}

/// `offset + pitch * rows <= size`, computed with checked arithmetic so a
/// driver-reported overflow is a `Descriptor` error rather than a silent
/// wraparound that then passes the very check meant to catch it.
fn check_extent(p: PlaneDesc, rows: u64, size: u64, which: &str) -> Result<(), H264Error> {
    let bytes = p.pitch.checked_mul(rows).ok_or_else(|| {
        H264Error::Descriptor(format!(
            "{which} plane pitch {} * {rows} rows overflows",
            p.pitch
        ))
    })?;
    let end = p.offset.checked_add(bytes).ok_or_else(|| {
        H264Error::Descriptor(format!(
            "{which} plane offset {} + {bytes} bytes overflows",
            p.offset
        ))
    })?;
    if end > size {
        return Err(H264Error::Descriptor(format!(
            "{which} plane extends to byte {end}, past the object's {size}-byte allocation"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffmpeg_sys_next as ffi;

    /// Build a descriptor by hand in the shape VA-API uses for NV12: one
    /// object, one layer, two planes.
    fn one_object_two_planes() -> ffi::AVDRMFrameDescriptor {
        let mut d: ffi::AVDRMFrameDescriptor = unsafe { std::mem::zeroed() };
        d.nb_objects = 1;
        d.objects[0].fd = 7;
        d.objects[0].size = 1024 * 768 * 3 / 2;
        d.objects[0].format_modifier = 0;
        d.nb_layers = 1;
        d.layers[0].format = DRM_FORMAT_NV12;
        d.layers[0].nb_planes = 2;
        d.layers[0].planes[0].object_index = 0;
        d.layers[0].planes[0].offset = 0;
        d.layers[0].planes[0].pitch = 1024;
        d.layers[0].planes[1].object_index = 0;
        d.layers[0].planes[1].offset = 1024 * 768;
        d.layers[0].planes[1].pitch = 1024;
        d
    }

    /// The other shape drivers use: two layers of one plane each.
    fn two_layers_one_plane_each() -> ffi::AVDRMFrameDescriptor {
        let mut d: ffi::AVDRMFrameDescriptor = unsafe { std::mem::zeroed() };
        d.nb_objects = 1;
        d.objects[0].fd = 9;
        d.objects[0].size = 1024 * 768 * 3 / 2;
        d.objects[0].format_modifier = 0;
        d.nb_layers = 2;
        d.layers[0].format = DRM_FORMAT_R8;
        d.layers[0].nb_planes = 1;
        d.layers[0].planes[0].object_index = 0;
        d.layers[0].planes[0].offset = 0;
        d.layers[0].planes[0].pitch = 1024;
        d.layers[1].format = DRM_FORMAT_GR88;
        d.layers[1].nb_planes = 1;
        d.layers[1].planes[0].object_index = 0;
        d.layers[1].planes[0].offset = 1024 * 768;
        d.layers[1].planes[0].pitch = 1024;
        d
    }

    #[test]
    fn reads_the_single_layer_shape() {
        let d = one_object_two_planes();
        let planes = DmabufPlanes::from_descriptor(&d, 1024, 768).expect("parse");
        assert_eq!(planes.fd, 7);
        assert_eq!(planes.modifier, 0);
        assert_eq!(planes.size, 1024 * 768 * 3 / 2);
        assert_eq!(planes.luma.offset, 0);
        assert_eq!(planes.luma.pitch, 1024);
        assert_eq!(planes.chroma.offset, 1024 * 768);
        assert_eq!(planes.chroma.pitch, 1024);
        assert_eq!(planes.width, 1024);
        assert_eq!(planes.height, 768);
        assert_eq!(planes.fourcc_luma, DRM_FORMAT_R8);
        assert_eq!(planes.fourcc_chroma, DRM_FORMAT_GR88);
    }

    #[test]
    fn reads_the_two_layer_shape_identically() {
        let a = one_object_two_planes();
        let b = two_layers_one_plane_each();
        let pa = DmabufPlanes::from_descriptor(&a, 1024, 768).expect("parse a");
        let pb = DmabufPlanes::from_descriptor(&b, 1024, 768).expect("parse b");
        // Every field except `fd` must agree: the two fixtures describe the
        // same buffer layout via a different fd (7 vs. 9) on purpose, to
        // prove that choice is irrelevant to how the shape is read. Checking
        // `size` and the `fourcc_*` fields too, not just `luma`/`chroma`, is
        // what makes this assert load-bearing rather than near-vacuous --
        // both fixtures previously used identical offsets, so the original
        // field-by-field asserts alone would pass even if the two shapes
        // were parsed by coincidence rather than by the intended logic.
        assert_eq!(pa.luma, pb.luma);
        assert_eq!(pa.chroma, pb.chroma);
        assert_eq!(pa.size, pb.size);
        assert_eq!(pa.width, pb.width);
        assert_eq!(pa.height, pb.height);
        assert_eq!(pa.fourcc_luma, pb.fourcc_luma);
        assert_eq!(pa.fourcc_chroma, pb.fourcc_chroma);
    }

    /// Two objects means the planes live in separate dmabufs. The import
    /// path assumes one fd, so this must be an error rather than silently
    /// reading plane 1 from the wrong buffer.
    #[test]
    fn rejects_a_multi_object_descriptor() {
        let mut d = one_object_two_planes();
        d.nb_objects = 2;
        d.objects[1].fd = 8;
        match DmabufPlanes::from_descriptor(&d, 1024, 768) {
            Err(H264Error::Descriptor(msg)) => {
                assert!(
                    msg.contains("expected 1 dmabuf object"),
                    "wrong error message: {msg}"
                );
            }
            other => panic!("expected a Descriptor error naming the object count, got {other:?}"),
        }
    }

    /// A plane whose `object_index` points somewhere other than the one fd
    /// this crate imports. Nothing else currently exercises this branch.
    #[test]
    fn rejects_a_plane_referencing_a_different_object() {
        let mut d = one_object_two_planes();
        d.layers[0].planes[1].object_index = 1;
        let err = DmabufPlanes::from_descriptor(&d, 1024, 768);
        assert!(matches!(err, Err(H264Error::Descriptor(_))));
    }

    /// A P010 (10-bit) export also has 2 planes, and would parse cleanly as
    /// NV12 if only the plane count were checked -- Task 6 would then build
    /// `R8Unorm`/`Rg8Unorm` textures over 16-bit data and render garbage
    /// that looks like a decode bug. Rejecting by fourcc is what catches it
    /// here instead.
    #[test]
    fn rejects_a_non_nv12_fourcc() {
        let mut d = one_object_two_planes();
        d.layers[0].format = fourcc_code(b'R', b'1', b'6', b' '); // DRM_FORMAT_R16
        let err = DmabufPlanes::from_descriptor(&d, 1024, 768);
        assert!(matches!(err, Err(H264Error::Descriptor(_))));
    }

    /// I420 (planar YUV, Y/U/V each their own layer) is a real shape a
    /// decoder could in principle export; this crate only understands NV12,
    /// so three layers must be rejected outright rather than the first two
    /// silently mistaken for the NV12 pair.
    #[test]
    fn rejects_a_three_layer_descriptor() {
        let mut d: ffi::AVDRMFrameDescriptor = unsafe { std::mem::zeroed() };
        d.nb_objects = 1;
        d.objects[0].fd = 5;
        d.objects[0].size = 1024 * 768 * 3 / 2;
        d.nb_layers = 3;
        for l in 0..3 {
            d.layers[l].nb_planes = 1;
        }
        let err = DmabufPlanes::from_descriptor(&d, 1024, 768);
        assert!(matches!(err, Err(H264Error::Descriptor(_))));
    }

    #[test]
    fn rejects_zero_layers() {
        let mut d = one_object_two_planes();
        d.nb_layers = 0;
        let err = DmabufPlanes::from_descriptor(&d, 1024, 768);
        assert!(matches!(err, Err(H264Error::Descriptor(_))));
    }

    /// The panic this crate used to be able to hit: `nb_layers` is a
    /// driver-supplied count into a fixed `[_; 4]` array, and the old
    /// `for l in 0..d.nb_layers as usize { d.layers[l] ... }` loop indexed
    /// straight off it. A count larger than the array, or negative (which
    /// wraps to a huge `usize` on the `as` cast), must report a
    /// `Descriptor` error instead of taking the process down -- this runs
    /// per frame, on Task 9's render thread.
    #[test]
    fn out_of_range_layer_count_does_not_panic() {
        let mut d = one_object_two_planes();
        d.nb_layers = 37;
        let err = DmabufPlanes::from_descriptor(&d, 1024, 768);
        assert!(matches!(err, Err(H264Error::Descriptor(_))));
    }

    #[test]
    fn negative_layer_count_does_not_panic() {
        let mut d = one_object_two_planes();
        d.nb_layers = -1;
        let err = DmabufPlanes::from_descriptor(&d, 1024, 768);
        assert!(matches!(err, Err(H264Error::Descriptor(_))));
    }

    #[test]
    fn out_of_range_plane_count_does_not_panic() {
        let mut d = one_object_two_planes();
        d.layers[0].nb_planes = 99;
        let err = DmabufPlanes::from_descriptor(&d, 1024, 768);
        assert!(matches!(err, Err(H264Error::Descriptor(_))));
    }

    #[test]
    fn negative_plane_count_does_not_panic() {
        let mut d = one_object_two_planes();
        d.layers[0].nb_planes = -1;
        let err = DmabufPlanes::from_descriptor(&d, 1024, 768);
        assert!(matches!(err, Err(H264Error::Descriptor(_))));
    }

    /// A plane that claims to extend past the object's reported allocation.
    /// This is the check that turns a short `VkDeviceMemory` allocation
    /// from an opaque GPU fault into a decode-time `Descriptor` error.
    #[test]
    fn rejects_a_plane_that_overruns_the_object() {
        let mut d = one_object_two_planes();
        d.objects[0].size = 10; // far smaller than the planes claim
        let err = DmabufPlanes::from_descriptor(&d, 1024, 768);
        assert!(matches!(err, Err(H264Error::Descriptor(_))));
    }

    #[test]
    fn chroma_dimensions_round_up_on_odd_resolutions() {
        let d = one_object_two_planes();
        let planes = DmabufPlanes::from_descriptor(&d, 641, 481).expect("parse");
        assert_eq!(planes.chroma_width(), 321);
        assert_eq!(planes.chroma_height(), 241);
    }
}
