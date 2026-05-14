// Solid codec — instanced quad with per-instance (tile_x, tile_y, color_packed).
// Each instance covers a 32×32 tile in framebuffer (rgba8unorm) coordinates.

struct VsOut {
  @builtin(position) pos : vec4<f32>,
  @location(0) color : vec4<f32>,
};

struct InstanceData {
  @location(1) tile_x : u32,
  @location(2) tile_y : u32,
  @location(3) color_packed : u32, // BGRA in u32 LSB-first
};

struct CanvasSize {
  width: u32,
  height: u32,
};
@group(0) @binding(0) var<uniform> canvas: CanvasSize;

@vertex
fn vs_main(
  @builtin(vertex_index) vi: u32,
  inst: InstanceData,
) -> VsOut {
  // Unit quad in [0,1]×[0,1].
  let xs = array<f32, 6>(0.0, 1.0, 0.0, 1.0, 1.0, 0.0);
  let ys = array<f32, 6>(0.0, 0.0, 1.0, 0.0, 1.0, 1.0);

  let cx = f32(inst.tile_x * 32u) + xs[vi] * 32.0;
  let cy = f32(inst.tile_y * 32u) + ys[vi] * 32.0;
  // Map pixel coords to NDC: x in [-1,1], y in [1,-1].
  let ndc_x = (cx / f32(canvas.width)) * 2.0 - 1.0;
  let ndc_y = 1.0 - (cy / f32(canvas.height)) * 2.0;

  // unpack BGRA → vec4 .x=B .y=G .z=R .w=A; swizzle to RGBA.
  let bgra = unpack4x8unorm(inst.color_packed);

  var out: VsOut;
  out.pos = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
  out.color = bgra.zyxw;
  return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
  return in.color;
}
