// Present-time blit: samples the persistent framebuffer texture and writes
// it to the swapchain texture via a single full-screen triangle pair.

@group(0) @binding(0) var fb_tex: texture_2d<f32>;
@group(0) @binding(1) var fb_sampler: sampler;

struct VsOut {
  @builtin(position) pos : vec4<f32>,
  @location(0) uv : vec2<f32>,
};

// 6-vertex quad; positions computed from vertex_index so no vertex buffer needed.
@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
  // 0,1 -> (-1,-1); 1 -> (1,-1); 2 -> (-1, 1); 3 -> (1,-1); 4 -> (1,1); 5 -> (-1,1)
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
  return textureSample(fb_tex, fb_sampler, in.uv);
}
