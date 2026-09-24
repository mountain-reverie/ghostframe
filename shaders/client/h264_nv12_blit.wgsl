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
// DO NOT "simplify" 1.401687 / -0.343695 / -0.714169 / 1.772160 to the
// textbook full-range BT.601 inverse (1.402 / -0.344136 / -0.714136 /
// 1.772). They are not the same matrix by accident of rounding -- these are
// the exact inverse of the forward shader above, which the textbook
// constants are not. The two differ by at most 0.1587 of a 255-step across
// the whole YUV cube (exhaustively measured, at cb=0/cr=0 on the R channel),
// so a reader who "corrects" this to the textbook values will not see a
// visible difference and will have quietly made the shader stop being the
// honest inverse of what the encoder did.
//
// Chroma upsampling is nearest-neighbour (integer /2), not bilinear. The
// forward shader samples chroma at the top-left pixel of each 2x2 block
// instead of averaging, so replication is what inverts it. Bilinear would
// look smoother and be further from the source.
//
// Mirrored on the CPU in ghostframe-client-gpu/src/nv12_reference.rs, which
// tests/gpu_nv12_blit.rs asserts this against exactly (Task 8; a 1-LSB
// tolerance applies only to the small FMA-reachable sample set recorded in
// the design doc's §9.1, not to this reference in general).

const CHROMA_CENTRE: f32 = 0.502;

@group(0) @binding(0) var luma_tex: texture_2d<f32>;
@group(0) @binding(1) var chroma_tex: texture_2d<f32>;

struct VsOut {
  @builtin(position) pos: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
  // Full-surface quad in NDC. The viewport covers the whole framebuffer:
  // H.264 always replaces the entire frame, never a tile rect.
  //
  // This winding is counter-clockwise in NDC, which is CLOCKWISE in
  // framebuffer space once wgpu's Y flip is applied. A pipeline that sets
  // `cull_mode: Some(Front)` (wgpu's default front face is CCW) would cull
  // every triangle here and render nothing. Safe today only because no
  // pipeline in this crate sets a cull mode -- every one uses
  // `..Default::default()`, which is `None`.
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
  //
  // `in.pos` ties this shader to BOTH the viewport size and the decoded
  // frame's texture size being the same. If the framebuffer is ever larger
  // than the decoded surface, these loads go out of range; WGSL's texture
  // robustness rules require an out-of-bounds textureLoad to return zeros
  // rather than fault, so the failure mode is a silent black band at the
  // edge, not an error.
  let p = vec2<i32>(i32(in.pos.x), i32(in.pos.y));
  let y = textureLoad(luma_tex, p, 0).r;
  let c = textureLoad(chroma_tex, p / 2, 0).rg;

  let u = c.r - CHROMA_CENTRE;
  let v = c.g - CHROMA_CENTRE;

  let r = y - 0.000927 * u + 1.401687 * v;
  let g = y - 0.343695 * u - 0.714169 * v;
  let b = y + 1.772160 * u + 0.000990 * v;

  return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), 1.0);
}
