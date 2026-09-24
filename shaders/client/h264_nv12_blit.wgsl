// shaders/client/h264_nv12_blit.wgsl
//
// NV12 -> RGBA, full-range BT.601.
//
// This is the exact inverse of ghostframe-lib/src/capture/shaders/
// bgra_to_nv12.comp, which is the ONLY reason these constants are what they
// are. The two files must change together; nothing else links them.
//
// NOT BT.709, and NOT limited range. The encoder writes full-range BT.601
// and signals no VUI colour description at all. An earlier design document
// claimed BT.709 limited; it was wrong.
//
// Chroma upsampling is nearest-neighbour (integer /2), not bilinear. The
// forward shader samples chroma at the top-left pixel of each 2x2 block
// instead of averaging, so replication is what inverts it. Bilinear would
// look smoother and be further from the source.
//
// Mirrored on the CPU in ghostframe-client-gpu/src/nv12_reference.rs, which
// tests/gpu_nv12_blit.rs asserts this against exactly.

@group(0) @binding(0) var luma_tex: texture_2d<f32>;
@group(0) @binding(1) var chroma_tex: texture_2d<f32>;

struct VsOut {
  @builtin(position) pos: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
  // Full-surface quad in NDC. The viewport covers the whole framebuffer:
  // H.264 always replaces the entire frame, never a tile rect.
  let xs = array<f32, 6>(-1.0,  1.0, -1.0,  1.0,  1.0, -1.0);
  let ys = array<f32, 6>(-1.0, -1.0,  1.0, -1.0,  1.0,  1.0);
  var out: VsOut;
  out.pos = vec4<f32>(xs[vi], ys[vi], 0.0, 1.0);
  return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
  // textureLoad, not textureSample: integer coordinates with no sampler make
  // the nearest-neighbour chroma rule explicit and unfilterable.
  let p = vec2<i32>(i32(in.pos.x), i32(in.pos.y));
  let y = textureLoad(luma_tex, p, 0).r;
  let c = textureLoad(chroma_tex, p / 2, 0).rg;

  let u = c.r - 0.502;
  let v = c.g - 0.502;

  let r = y - 0.000927 * u + 1.401687 * v;
  let g = y - 0.343695 * u - 0.714169 * v;
  let b = y + 1.772160 * u + 0.000990 * v;

  return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), 1.0);
}
