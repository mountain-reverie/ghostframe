// cdf53_planes.wgsl -- stage A of the full-grid CDF 5/3 decoder: copy bit
// planes the host already decoded (events carry them as 384 bytes = 96 words
// per tile pass) into the plane store. One workgroup of 96
// invocations per plane, one word each; a plane owns its 96-word slot in the
// store, so nothing is shared and nothing is atomic.
//
// Frame buffer words used (see `Cdf53Grid` for the layout):
//   [4] plane count   [6] plane table offset (words)   [7] plane data offset
// Plane table row, 2 words: tile_index, pass_idx. Plane `i`'s data is the 96
// words at plane data offset + 96 * i, in the store's own channel order
// (B, G, R, 32 words each).
//
// A row outside the store (tile or pass out of range) writes nothing.

@group(0) @binding(0) var<storage, read> frame: array<u32>;
@group(0) @binding(1) var<storage, read_write> planes: array<u32>;

const H_PLANES: u32 = 4u;
const H_PLANE_ROWS_OFF: u32 = 6u;
const H_PLANE_DATA_OFF: u32 = 7u;

const TILE_WORDS: u32 = 1344u;
const PLANE_WORDS: u32 = 96u;

@compute @workgroup_size(96, 1, 1)
fn main(@builtin(workgroup_id) wg: vec3<u32>,
        @builtin(local_invocation_index) lid: u32) {
  let i = wg.x;
  if (i >= frame[H_PLANES]) { return; }
  let row = frame[H_PLANE_ROWS_OFF] + i * 2u;
  let tile = frame[row];
  let pass_idx = frame[row + 1u];
  // Compared before multiplying, so a wild index cannot wrap into range.
  if (tile >= arrayLength(&planes) / TILE_WORDS || pass_idx >= 14u) { return; }
  planes[tile * TILE_WORDS + pass_idx * PLANE_WORDS + lid] =
      frame[frame[H_PLANE_DATA_OFF] + i * PLANE_WORDS + lid];
}
