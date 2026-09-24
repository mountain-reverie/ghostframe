//! `AVDRMFrameDescriptor` -> a plain struct with no ffmpeg types in it.
//!
//! This is the crate boundary: `ghostframe-client-gpu` consumes
//! [`DmabufPlanes`] and never links ffmpeg.
//!
//! Drivers describe an NV12 dmabuf in two equivalent shapes -- one layer
//! with two planes, or two layers with one plane each -- and which one you
//! get is not something to depend on. Both are normalised here.

use crate::H264Error;
use ffmpeg_sys_next as ffi;

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

impl DmabufPlanes {
    /// # Safety
    /// `d` must be a fully initialized descriptor that outlives the call.
    pub unsafe fn from_descriptor(
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

        // Flatten however the driver split the planes across layers.
        let mut flat: Vec<PlaneDesc> = Vec::new();
        for l in 0..d.nb_layers as usize {
            let layer = &d.layers[l];
            for p in 0..layer.nb_planes as usize {
                if layer.planes[p].object_index != 0 {
                    return Err(H264Error::Descriptor(format!(
                        "plane references object {}, but only object 0 was imported",
                        layer.planes[p].object_index
                    )));
                }
                flat.push(PlaneDesc {
                    offset: layer.planes[p].offset as u64,
                    pitch: layer.planes[p].pitch as u64,
                });
            }
        }

        if flat.len() != 2 {
            return Err(H264Error::Descriptor(format!(
                "expected 2 planes for NV12, got {}",
                flat.len()
            )));
        }

        Ok(DmabufPlanes {
            fd: d.objects[0].fd,
            modifier: d.objects[0].format_modifier,
            width,
            height,
            luma: flat[0],
            chroma: flat[1],
        })
    }
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
        d.layers[0].nb_planes = 1;
        d.layers[0].planes[0].object_index = 0;
        d.layers[0].planes[0].offset = 0;
        d.layers[0].planes[0].pitch = 1024;
        d.layers[1].nb_planes = 1;
        d.layers[1].planes[0].object_index = 0;
        d.layers[1].planes[0].offset = 1024 * 768;
        d.layers[1].planes[0].pitch = 1024;
        d
    }

    #[test]
    fn reads_the_single_layer_shape() {
        let d = one_object_two_planes();
        // SAFETY: `d` is a fully initialized descriptor living on this stack
        // frame for the duration of the call.
        let planes = unsafe { DmabufPlanes::from_descriptor(&d, 1024, 768) }.expect("parse");
        assert_eq!(planes.fd, 7);
        assert_eq!(planes.modifier, 0);
        assert_eq!(planes.luma.offset, 0);
        assert_eq!(planes.luma.pitch, 1024);
        assert_eq!(planes.chroma.offset, 1024 * 768);
        assert_eq!(planes.chroma.pitch, 1024);
        assert_eq!(planes.width, 1024);
        assert_eq!(planes.height, 768);
    }

    #[test]
    fn reads_the_two_layer_shape_identically() {
        let a = one_object_two_planes();
        let b = two_layers_one_plane_each();
        // SAFETY: both descriptors are fully initialized and live here.
        let pa = unsafe { DmabufPlanes::from_descriptor(&a, 1024, 768) }.expect("parse a");
        let pb = unsafe { DmabufPlanes::from_descriptor(&b, 1024, 768) }.expect("parse b");
        assert_eq!(pa.luma, pb.luma);
        assert_eq!(pa.chroma, pb.chroma);
    }

    /// Two objects means the planes live in separate dmabufs. The import
    /// path assumes one fd, so this must be an error rather than silently
    /// reading plane 1 from the wrong buffer.
    #[test]
    fn rejects_a_multi_object_descriptor() {
        let mut d = one_object_two_planes();
        d.nb_objects = 2;
        d.objects[1].fd = 8;
        // SAFETY: `d` is fully initialized.
        let err = unsafe { DmabufPlanes::from_descriptor(&d, 1024, 768) };
        assert!(
            err.is_err(),
            "a 2-object descriptor must not parse as one fd"
        );
    }
}
