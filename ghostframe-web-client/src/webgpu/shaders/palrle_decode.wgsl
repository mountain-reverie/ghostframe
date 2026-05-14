// PalRLE compute decode: one workgroup per tile, 32×32 threads per workgroup.
// Each thread reads its 4-bit index, looks up palette_atlas, writes one pixel
// to the framebuffer's tile region. Detects in-shader errors (codes 5..7).

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

@compute @workgroup_size(32, 32, 1)
fn main(
  @builtin(workgroup_id) wg: vec3<u32>,
  @builtin(local_invocation_id) lid: vec3<u32>,
) {
  let work = tile_work[wg.x];
  let pixel_in_tile = lid.y * 32u + lid.x;  // 0..1023
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
    atomicStore(&errors[wg.x], 5u);  // ERR_INDEX_OOB
    return;
  }

  let bgra_packed = palette_atlas[work.palette_id * 16u + color_idx];
  // unpack4x8unorm: byte0 -> .x = B (our wire packs B in LSB).
  let bgra = unpack4x8unorm(bgra_packed);
  let rgba = bgra.zyxw; // BGRA -> RGBA swizzle

  textureStore(
    framebuffer,
    vec2<i32>(
      i32(work.tile_x * 32u + lid.x),
      i32(work.tile_y * 32u + lid.y),
    ),
    rgba,
  );
}
