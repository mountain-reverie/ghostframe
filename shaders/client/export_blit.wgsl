// export_blit.wgsl — copy the framebuffer into an export buffer.
//
// A render pass rather than `copy_texture_to_texture`, for one reason: the
// export buffer is `bgra8unorm` while the framebuffer is `rgba8unorm`, and
// those are not copy-compatible in WebGPU. A draw converts, because writing a
// `vec4(r, g, b, a)` to a `bgra8unorm` target stores it in BGRA byte order --
// so the channel reordering is the API's job, not a swizzle here. Writing an
// explicit `.bgra` would double-swap.
//
// Why the export must be BGRA at all: X11/DRI3 infers a dmabuf's layout from
// depth and bpp using Mesa's fixed table (depth 24 / bpp 32 -> XRGB8888,
// i.e. bytes B,G,R,X). There is no depth that yields RGBA, so a buffer
// exported as ABGR8888 comes out with red and blue exchanged -- a blue desktop
// renders orange. See `window/x11.rs`'s module doc.
//
// Damage is applied with `set_scissor_rect` between draws rather than by
// building geometry per rect: one pipeline, one vertex buffer that does not
// exist, and the clip is free.

@group(0) @binding(0) var src: texture_2d<f32>;

struct VsOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) uv: vec2<f32>,
};

// A single triangle covering the viewport. Three vertices rather than a
// two-triangle quad: no seam along the diagonal, and one fewer vertex to
// invoke.
@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VsOut {
  var out: VsOut;
  // (-1,-1), (3,-1), (-1,3) in clip space; the excess is clipped away.
  let x = f32(i32(idx) / 2) * 4.0 - 1.0;
  let y = f32(i32(idx) & 1) * 4.0 - 1.0;
  out.pos = vec4<f32>(x, y, 0.0, 1.0);
  // Clip space is y-up, texture space y-down.
  out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
  return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
  // `textureLoad` with integer coordinates, not a sampler: this is a 1:1 copy
  // between identically-sized textures, so any filtering would be a way to
  // introduce error, not to avoid it.
  let dims = textureDimensions(src);
  let coord = vec2<i32>(in.uv * vec2<f32>(dims));
  return textureLoad(src, clamp(coord, vec2<i32>(0), vec2<i32>(dims) - vec2<i32>(1)), 0);
}
