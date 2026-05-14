// H.264 textured-quad blit: samples a GPUExternalTexture (from
// importExternalTexture on a WebCodecs VideoFrame) and writes its
// content into the current render-pass viewport.

@group(0) @binding(0) var ext_tex: texture_external;
@group(0) @binding(1) var ext_sampler: sampler;

struct VsOut {
  @builtin(position) pos : vec4<f32>,
  @location(0) uv : vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
  // Full-quad in NDC. The viewport is set per-draw to map this to a tile rect.
  let xs = array<f32, 6>(-1.0,  1.0, -1.0,  1.0,  1.0, -1.0);
  let ys = array<f32, 6>(-1.0, -1.0,  1.0, -1.0,  1.0,  1.0);
  let us = array<f32, 6>( 0.0,  1.0,  0.0,  1.0,  1.0,  0.0);
  let vs = array<f32, 6>( 1.0,  1.0,  0.0,  1.0,  0.0,  0.0);
  var out: VsOut;
  out.pos = vec4<f32>(xs[vi], ys[vi], 0.0, 1.0);
  out.uv = vec2<f32>(us[vi], vs[vi]);
  return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
  return textureSampleBaseClampToEdge(ext_tex, ext_sampler, in.uv);
}
