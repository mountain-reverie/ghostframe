// Copy RGBA tiles that already sit in an upload slab into the framebuffer: the
// slab-sourced sibling of rgba_tiles.wgsl, same dispatch shape, same result.
//
// A slab is a 2048x512 texture the CPU wrote tiles into (see upload_pool.rs):
// slot i is the 32x32 block at ((i % 64) * 32, (i / 64) * 32). tile_work has
// one entry per tile to draw, naming its destination and its slot, so the
// render thread uploads only this small table -- the pixels never pass through
// the queue.
//
//   dispatchWorkgroups(num_tiles, 2, 2): wg.x = index of the tile in tile_work,
//   wg.y = which 16-row half, wg.z = which 16-column half. 16x8 invocations,
//   two rows each (128 invocations per workgroup is Mali-T860's limit).
//
// tile_work is bound with a dynamic offset: one buffer holds every slab's
// table for a flush, and each slab's dispatch picks its own.

struct TileWork {
  tile_x : u32,
  tile_y : u32,
  slot : u32,
};

@group(0) @binding(0) var<storage, read> tile_work: array<TileWork>;
@group(0) @binding(1) var slab: texture_2d<f32>;
@group(0) @binding(2) var framebuffer: texture_storage_2d<rgba8unorm, write>;

const TILES_PER_ROW : u32 = 64u;

@compute @workgroup_size(16, 8, 1)
fn main(
  @builtin(workgroup_id) wg: vec3<u32>,
  @builtin(local_invocation_id) lid: vec3<u32>,
) {
  let work = tile_work[wg.x];
  let fb_dims = textureDimensions(framebuffer);
  let x_in_tile = wg.z * 16u + lid.x;
  let dst_x = work.tile_x * 32u + x_in_tile;
  if (dst_x >= fb_dims.x) {
    return; // the right edge: this column is off the framebuffer for both rows
  }
  let src_x = (work.slot % TILES_PER_ROW) * 32u + x_in_tile;
  let src_y0 = (work.slot / TILES_PER_ROW) * 32u;
  for (var yy: u32 = 0u; yy < 2u; yy = yy + 1u) {
    let y_in_tile = wg.y * 16u + lid.y * 2u + yy;
    let dst_y = work.tile_y * 32u + y_in_tile;
    if (dst_y >= fb_dims.y) {
      continue;
    }
    let texel = textureLoad(slab, vec2<u32>(src_x, src_y0 + y_in_tile), 0);
    textureStore(framebuffer, vec2<u32>(dst_x, dst_y), texel);
  }
}
