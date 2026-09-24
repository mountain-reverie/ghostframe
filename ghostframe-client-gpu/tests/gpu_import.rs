//! Import a dmabuf we created ourselves, and read it back.
//!
//! Requires a GPU. Deliberately NOT named in any CI workflow.

use ghostframe_client_gpu::export::ExportedImage;
use ghostframe_client_gpu::import::import_nv12;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_client_h264::{DmabufPlanes, PlaneDesc};

#[test]
fn imports_a_linear_dmabuf_and_reads_the_bytes_back() {
    let Ok(ctx) = WgpuContext::new() else {
        eprintln!("no usable GPU; skipping");
        return;
    };

    // 64x64 RGBA export = 16384 bytes, host-visible so we can write a pattern.
    let src = ExportedImage::new(&ctx, 64, 64, &[], true).expect("export");
    let pitch = src.planes[0].stride;

    // Treat the RGBA export as an NV12 luma plane of width `pitch`: the
    // import path only cares about bytes, offsets and pitches.
    let planes = DmabufPlanes {
        fd: src.raw_fd(),
        modifier: src.modifier,
        // The whole exported object. `from_descriptor` fills this from
        // `objects[0].size`; here we know it because we allocated it.
        size: pitch * 64,
        fourcc_luma: ghostframe_client_h264::DRM_FORMAT_R8,
        fourcc_chroma: ghostframe_client_h264::DRM_FORMAT_GR88,
        width: 64,
        height: 64,
        luma: PlaneDesc {
            offset: src.planes[0].offset,
            pitch,
        },
        chroma: PlaneDesc {
            offset: src.planes[0].offset,
            pitch,
        },
    };

    let imported = match import_nv12(&ctx, &planes) {
        Ok(i) => i,
        Err(e) => {
            panic!(
                "import of a LINEAR dmabuf this process just exported failed: {e}. \
                 This is the import path itself failing, not the decoder."
            );
        }
    };
    assert_eq!(imported.luma.width(), 64);
    assert_eq!(imported.chroma.width(), 32);
    assert_eq!(imported.chroma.height(), 32);
}
