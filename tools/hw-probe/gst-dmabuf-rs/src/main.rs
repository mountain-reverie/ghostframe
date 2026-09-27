//! Feasibility probe: does GStreamer's `v4l2slh264dec` hand out a dmabuf we can
//! describe well enough to import, and are the pixels bit-exact?
//!
//! The question that matters is not "does it decode" -- the design doc already
//! measured that with `gst-launch` -- but whether the *layout* comes out of
//! GStreamer precisely enough to build a `DmabufPlanes` from: one fd, luma and
//! chroma offsets, row pitches, total size. That is what `import_gles.rs` needs,
//! and it is what the cros-codecs path had to derive by hand from
//! `v4l2_pix_format_mplane` (and got wrong twice before it got right).
//!
//! Reads the decoded NV12 back **through the exported fd**, so a byte-exact
//! comparison against a software golden validates both the fd and the layout.
//!
//! Usage: `gst-probe <in.h264> <out.i420>`

use std::io::Write;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

/// One decoded frame's dmabuf, described.
#[derive(Debug, Clone, Copy)]
struct Layout {
    fd: i32,
    width: u32,
    height: u32,
    /// Byte offset and row pitch per plane: [0] luma, [1] interleaved chroma.
    offsets: [usize; 2],
    strides: [usize; 2],
    size: usize,
    /// Where the layout came from -- `GstVideoMeta` when the driver padded,
    /// otherwise computed from caps.
    source: &'static str,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let input_path = args.next().expect("usage: gst-probe <in.h264> <out.i420>");
    let output_path = args.next().expect("usage: gst-probe <in.h264> <out.i420>");
    let input = std::fs::read(&input_path).expect("cannot read input");
    let mut output = std::fs::File::create(&output_path).expect("cannot create output");

    gst::init().expect("gst init");

    // appsrc -> h264parse -> v4l2slh264dec -> appsink.
    //
    // `h264parse` is not optional: `v4l2slh264dec` needs the SPS/PPS and
    // alignment=au that the parser establishes, and feeding it a raw Annex-B
    // stream directly gets nothing out.
    let pipeline = gst::Pipeline::new();
    let src = gst_app::AppSrc::builder()
        .caps(&gst::Caps::builder("video/x-h264").field("stream-format", "byte-stream").build())
        .format(gst::Format::Time)
        .build();
    let parse = gst::ElementFactory::make("h264parse").build().expect("h264parse");
    let dec = gst::ElementFactory::make("v4l2slh264dec")
        .build()
        .expect("v4l2slh264dec -- is gst-plugins-bad's v4l2codecs installed?");
    // No caps restriction: on GStreamer 1.22 `v4l2slh264dec` does not advertise
    // `memory:DMABuf` at all, so naming it fails to LINK rather than negotiating.
    // dmabuf export is implicit here, decided by allocation negotiation.
    //
    // `max_buffers` generously above the 11 the pool was observed to cycle, in
    // case starving it is what makes the decoder fall back to copying.
    // Pin plain NV12 (no DMABuf feature -- 1.22's decoder cannot advertise it).
    // The decoder also offers tiled NV12_32L32 / NV12_4L4 / NV12_16L32S; if it
    // negotiates one of those and then converts for downstream, the conversion is
    // the system-memory copy we are trying to explain.
    let sink = gst_app::AppSink::builder()
        .caps(&gst::Caps::builder("video/x-raw").field("format", "NV12").build())
        .sync(false)
        .max_buffers(32)
        .build();

    pipeline
        .add_many([src.upcast_ref(), &parse, &dec, sink.upcast_ref()])
        .expect("add elements");
    gst::Element::link_many([src.upcast_ref(), &parse, &dec, sink.upcast_ref()]).expect("link");

    pipeline.set_state(gst::State::Playing).expect("play");

    // Push the whole stream, then EOS. The parser splits it into access units;
    // pushing one buffer keeps this probe honest about what the decoder does
    // rather than about how cleverly we can frame the input.
    src.push_buffer(gst::Buffer::from_slice(input)).expect("push");
    src.end_of_stream().expect("eos");

    let mut frames = 0usize;
    let mut first_layout: Option<Layout> = None;
    let mut seen_fds = std::collections::BTreeSet::new();
    loop {
        match sink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
            Some(sample) => {
                let layout = describe(&sample).expect("could not describe the decoded buffer");
                if first_layout.is_none() {
                    println!("layout: {layout:?}");
                    // How many distinct dmabufs does the pipeline cycle through?
                    // An importer wants to import once per fd and look up
                    // thereafter, exactly as the cros-codecs path had to.
                    first_layout = Some(layout);
                }
                seen_fds.insert(layout.fd);
                let (y, uv) = read_through_dmabuf(&layout);
                write_i420(&mut output, &y, &uv, layout.width, layout.height);
                frames += 1;
            }
            None => break,
        }
    }

    // Drain the bus. Without this a negotiation failure is completely silent:
    // `try_pull_sample` just returns None and the probe reports zero frames with
    // no reason, which cost real time to notice.
    if let Some(bus) = pipeline.bus() {
        while let Some(msg) = bus.pop() {
            match msg.view() {
                gst::MessageView::Error(e) => eprintln!(
                    "BUS ERROR from {:?}: {} ({:?})",
                    e.src().map(|s| s.path_string()),
                    e.error(),
                    e.debug()
                ),
                gst::MessageView::Warning(w) => eprintln!(
                    "BUS WARN from {:?}: {}",
                    w.src().map(|s| s.path_string()),
                    w.error()
                ),
                _ => {}
            }
        }
    }

    pipeline.set_state(gst::State::Null).expect("null");
    println!("decoded {frames} frames over {} distinct dmabuf fds: {seen_fds:?}", seen_fds.len());
}

/// Pull the fd and the plane layout out of a decoded sample.
///
/// Two things here are the whole point of the probe:
///
/// - **The dmabuf is found by asking the memory, not by reading caps.**
///   GStreamer 1.22 has no `memory:DMABuf` caps feature, so the caps say plain
///   `video/x-raw` even when the buffer is dmabuf-backed. Gating on caps would
///   conclude, wrongly, that there is nothing to import.
/// - **The layout comes from `GstVideoMeta`**, not from arithmetic on the
///   resolution. That is the same trap the cros-codecs path hit: NV12 chroma
///   does not start at `stride * display_height` when the coded height is
///   padded, and here GStreamer has already asked the driver.
fn describe(sample: &gst::Sample) -> Option<Layout> {
    let Some(buffer) = sample.buffer() else {
        eprintln!("sample carries no buffer");
        return None;
    };
    let Some(caps) = sample.caps() else {
        eprintln!("sample carries no caps");
        return None;
    };

    // The baseline layout for these caps. **Absence of `GstVideoMeta` is not a
    // failure** -- it is GStreamer's way of saying "standard packing for this
    // format and size", which is exactly what `VideoInfo` computes. Treating a
    // missing meta as an error rejected every 1080p buffer on this hardware,
    // where the decoder happens to produce unpadded output.
    let info = match gst_video::VideoInfo::from_caps(caps) {
        Ok(info) => info,
        Err(e) => {
            eprintln!("cannot read VideoInfo from caps: {e}");
            return None;
        }
    };
    let mut offsets = [info.offset()[0], info.offset()[1]];
    let mut strides = [info.stride()[0] as usize, info.stride()[1] as usize];
    let mut width = info.width();
    let mut height = info.height();
    let mut source = "caps/VideoInfo";

    // And when the meta IS there, it wins: it carries the driver's real padding.
    if let Some(meta) = buffer.meta::<gst_video::VideoMeta>() {
        if meta.n_planes() != 2 {
            eprintln!("VideoMeta reports {} planes, expected 2", meta.n_planes());
            return None;
        }
        offsets = [meta.offset()[0], meta.offset()[1]];
        strides = [meta.stride()[0] as usize, meta.stride()[1] as usize];
        width = meta.width();
        height = meta.height();
        source = "GstVideoMeta";
    }

    if buffer.n_memory() != 1 {
        eprintln!("buffer has {} memories, expected 1", buffer.n_memory());
        return None;
    }
    let mem = buffer.peek_memory(0);
    let Some(dmabuf) = mem.downcast_memory_ref::<gstreamer_allocators::DmaBufMemory>() else {
        eprintln!(
            "NOT dmabuf-backed: {} bytes, allocator {:?} -- this would be a CPU copy per frame",
            mem.size(),
            mem.allocator().map(|a| a.memory_type().to_string())
        );
        return None;
    };

    Some(Layout {
        fd: dmabuf.fd(),
        width,
        height,
        offsets,
        strides,
        size: mem.size(),
        source,
    })
}

/// Read the two NV12 planes back through the exported fd, tightly packed.
fn read_through_dmabuf(l: &Layout) -> (Vec<u8>, Vec<u8>) {
    let w = l.width as usize;
    let h = l.height as usize;
    let chroma_h = h.div_ceil(2);
    let need = l.offsets[1] + l.strides[1] * chroma_h;
    assert!(
        need <= l.size,
        "plane layout needs {need} bytes but the dmabuf is {}",
        l.size
    );

    // SAFETY: `l.fd` is a dmabuf owned by the sample, alive for this call;
    // read-only, sized to what the memory reports, unmapped below.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            l.size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            l.fd,
            0,
        )
    };
    assert_ne!(base, libc::MAP_FAILED, "mmap of the decoded dmabuf failed");

    let mut luma = Vec::with_capacity(w * h);
    let mut chroma = Vec::with_capacity(w * chroma_h);
    for row in 0..h {
        // SAFETY: bounds asserted above.
        let p = unsafe { (base as *const u8).add(l.offsets[0] + row * l.strides[0]) };
        luma.extend_from_slice(unsafe { std::slice::from_raw_parts(p, w) });
    }
    for row in 0..chroma_h {
        let p = unsafe { (base as *const u8).add(l.offsets[1] + row * l.strides[1]) };
        chroma.extend_from_slice(unsafe { std::slice::from_raw_parts(p, w) });
    }
    // SAFETY: the pair returned by the mmap above.
    unsafe { libc::munmap(base, l.size) };
    (luma, chroma)
}

/// NV12 -> I420, so the output is directly comparable to `ffmpeg -pix_fmt yuv420p`.
fn write_i420(out: &mut std::fs::File, luma: &[u8], chroma: &[u8], width: u32, height: u32) {
    let w = width as usize;
    let h = height as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    out.write_all(luma).expect("write luma");
    let mut u = Vec::with_capacity(cw * ch);
    let mut v = Vec::with_capacity(cw * ch);
    for row in 0..ch {
        let line = &chroma[row * w..row * w + cw * 2];
        for i in 0..cw {
            u.push(line[i * 2]);
            v.push(line[i * 2 + 1]);
        }
    }
    out.write_all(&u).expect("write u");
    out.write_all(&v).expect("write v");
}
