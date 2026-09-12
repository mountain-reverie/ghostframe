// Datagram-builder helper shared by the suites that drive WasmClientCore's
// tile reassembly directly (tile_assembly.test.ts, cdf53_globals.test.ts).
//
// This file used to also hold Clock/AckHarness/NackHarness/
// parseNackEnvelopeNested, adapting the wasm ACK/NACK batchers' poll-and-
// injected-time API to the shape the pre-cutover ack.test.ts/nack.test.ts
// suites were written against. Phase 4b deleted those two suites (they
// duplicated ghostframe-client-core's oracle_*.rs tests), so that adapter
// code went with them — fragmentTile is the only survivor.

const TILE_DATAGRAM_FLAG = 0x80000000;

function u32be(n: number): number[] {
  return [(n >>> 24) & 0xff, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff];
}
function u16be(n: number): number[] {
  return [(n >>> 8) & 0xff, n & 0xff];
}

/**
 * Builds tile-datagram fragments matching `ghostframe_protocol::protocol`'s
 * `fragment_tile` wire layout exactly:
 *   [DatagramHeader 16B: frame_seq|frag_idx|frag_total|wire_seq|timestamp_us]
 *   [TileHeader 8B: tile_x|tile_y|(codec<<1|lz4)|(generation<<4|pass)|payload_len]
 *   [payload chunk]
 * `wire_seq` is UNSTAMPED_WIRE_SEQ (0); `timestamp_us` is 0 — neither is
 * read by the reassembly path these tests exercise.
 *
 * Shared by `tests/tile_assembly.test.ts` and `tests/cdf53_globals.test.ts`
 * so both drive the wasm core with the exact same wire bytes rather than
 * each hand-rolling its own tile-datagram construction.
 */
export function fragmentTile(
  frameSeq: number,
  tileX: number,
  tileY: number,
  codec: number,
  generation: number,
  pass: number,
  payload: Uint8Array,
  maxFragmentPayload: number,
): Uint8Array[] {
  const chunks: Uint8Array[] = [];
  if (payload.length === 0) {
    chunks.push(new Uint8Array(0));
  } else {
    for (let i = 0; i < payload.length; i += maxFragmentPayload) {
      chunks.push(payload.slice(i, i + maxFragmentPayload));
    }
  }
  const fragTotal = chunks.length;
  return chunks.map((chunk, idx) => {
    const header = new Uint8Array([
      ...u32be(frameSeq | TILE_DATAGRAM_FLAG),
      ...u16be(idx),
      ...u16be(fragTotal),
      ...u32be(0), // wire_seq (UNSTAMPED_WIRE_SEQ)
      ...u32be(0), // timestamp_us
      tileX,
      tileY,
      (codec << 1) | 0, // lz4 = false
      ((generation & 0x0f) << 4) | (pass & 0x0f),
      ...u32be(payload.length),
    ]);
    const out = new Uint8Array(header.length + chunk.length);
    out.set(header, 0);
    out.set(chunk, header.length);
    return out;
  });
}
