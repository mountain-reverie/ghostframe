// PalRLE compute decode: FOUR workgroups per tile (in a 2x2 arrangement),
// 16×8 threads per workgroup = 128 invocations, each covering two pixels
// (two rows of one column).  Total 4×128×2 = 1024 pixels, the tile's 32×32.
//
// Why 128 invocations: a one-workgroup-per-tile design at 32×32 = 1024 would
// fail pipeline validation everywhere, so the tile was already split into 4
// workgroups to stay inside WebGPU's maxComputeInvocationsPerWorkgroup of
// 256.  128 is the next step down and is what the native client needs: GLES
// 3.1's minimum for that limit is 128, and Mali-T860 reports exactly 128, so
// at 256 `request_device` fails outright on Midgard — no pipeline, no pixels,
// and the failure names the device rather than this shader (GLES/V4L2 design
// §5).  128 is valid in every browser too, since WebGPU guarantees 256.
// Enforced by
// `shader_validation.rs::no_compute_shader_exceeds_the_portable_workgroup_limit`.
//
// The workgroup COUNT is unchanged, so palrle.rs and palrle.ts keep their
// dispatchWorkgroups(num_tiles, 2, 2).
//
// Dispatch shape from palrle.ts: dispatchWorkgroups(num_tiles, 2, 2)
//   wg.x = tile_idx
//   wg.y = sub_tile_y (0 or 1, selects upper / lower 16-pixel half)
//   wg.z = sub_tile_x (0 or 1, selects left / right 16-pixel half)

struct TileWork {
  tile_x      : u32,
  tile_y      : u32,
  palette_id  : u32,
  count       : u32,
  payload_off : u32,  // byte offset into indices_buf
  _pad0       : u32,
  _pad1       : u32,
  _pad2       : u32,
};

@group(0) @binding(0) var<storage, read> palette_atlas: array<u32, 4096>;
@group(0) @binding(1) var<storage, read> tile_work: array<TileWork>;
@group(0) @binding(2) var<storage, read> indices_buf: array<u32>;
@group(0) @binding(3) var framebuffer: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(4) var<storage, read_write> errors: array<atomic<u32>>;

@compute @workgroup_size(16, 8, 1)
fn main(
  @builtin(workgroup_id) wg: vec3<u32>,
  @builtin(local_invocation_id) lid: vec3<u32>,
) {
  let work = tile_work[wg.x];
  // Each workgroup covers a 16×16 sub-region of the 32×32 tile.
  // wg.y / wg.z pick which sub-region (0 or 1 each).
  let pixel_x_in_tile = wg.z * 16u + lid.x;  // 0..31
  let fb_dims = textureDimensions(framebuffer);
  let dst_x = work.tile_x * 32u + pixel_x_in_tile;

  // Two rows per invocation: lid.y runs 0..8 and covers the sub-region's 16
  // rows. Note `continue`, not `return`, for the per-pixel bail-outs below --
  // each pixel is independent and a `return` here would silently abandon this
  // invocation's second row.
  for (var yy: u32 = 0u; yy < 2u; yy = yy + 1u) {
    let pixel_y_in_tile = wg.y * 16u + lid.y * 2u + yy;  // 0..31
    let pixel_in_tile = pixel_y_in_tile * 32u + pixel_x_in_tile;  // 0..1023
    let nibble_idx = pixel_in_tile;

    // Locate the 4-bit nibble in indices_buf.
    let byte_idx = nibble_idx >> 1u;
    let word_idx = byte_idx >> 2u;
    let byte_in_word = byte_idx & 3u;
    let word = indices_buf[(work.payload_off >> 2u) + word_idx];
    let packed_byte = (word >> (byte_in_word * 8u)) & 0xFFu;
    // Low nibble = even pixel; high nibble = odd pixel.
    var color_idx: u32;
    if ((nibble_idx & 1u) == 0u) {
      color_idx = packed_byte & 0x0Fu;
    } else {
      color_idx = packed_byte >> 4u;
    }

    if (color_idx >= work.count) {
      // wg.x is tile_idx (one error slot per tile, shared across sub-workgroups).
      atomicStore(&errors[wg.x], 5u);  // ERR_INDEX_OOB
      continue;
    }

    let bgra_packed = palette_atlas[work.palette_id * 16u + color_idx];
    // unpack4x8unorm: byte0 -> .x = B (our wire packs B in LSB).
    let bgra = unpack4x8unorm(bgra_packed);
    let rgba = bgra.zyxw; // BGRA -> RGBA swizzle

    let dst_y = work.tile_y * 32u + pixel_y_in_tile;
    if (dst_x >= fb_dims.x || dst_y >= fb_dims.y) {
      continue;
    }
    textureStore(
      framebuffer,
      vec2<i32>(i32(dst_x), i32(dst_y)),
      rgba,
    );
  }
}
