// Datagram-builder helpers shared by the suites that drive WasmClientCore's
// tile reassembly directly (tile_assembly.test.ts, cdf53_globals.test.ts,
// eviction.test.ts).
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

const EVICTION_SENTINEL_X = 0xfe;
const EVICTION_SENTINEL_Y = 0xfe;
const SKIP_CODEC = 0; // ghostframe_protocol::protocol::Codec::Skip

/**
 * Builds a single eviction-notice datagram matching
 * `ghostframe_protocol::eviction::build_eviction_datagram`'s wire layout:
 * a tile datagram at the eviction sentinel coordinates (0xFE, 0xFE),
 * `Codec::Skip`, and a 1-byte reason payload.
 *
 * There is no wasm-side builder to call here, and none is added: unlike
 * the sentinel-parsing logic (which moved server->client into
 * `ghostframe-client-core`/`ghostframe-protocol` at the wasm cutover),
 * `build_eviction_datagram` is server-only (`ghostframe_protocol::eviction`,
 * called from `ghostframe-lib`'s `io_bridge.rs`) and reuses `fragment_tile`
 * with fixed inputs -- the same wire builder `fragmentTile` above already
 * mirrors byte-for-byte. So this is a thin wrapper around `fragmentTile`,
 * exactly as unfragmented/untied to any wasm-bindgen export or Cargo
 * feature gate as `fragmentTile` itself is.
 */
/**
 * Build an eviction datagram the way the server would.
 *
 * **What this does NOT prove.** These bytes are encoded here in TypeScript,
 * using this file's own copies of the sentinel coordinates and codec id --
 * `ghostframe_protocol::eviction::build_eviction_datagram` is never called.
 * So a test built on this verifies the *decoder* (wasm core routing) against
 * a TS-encoded datagram; it cannot catch the Rust encoder drifting away from
 * these constants. That drift would ship green here and fail only in
 * production.
 *
 * This repo has been bitten by exactly that before: a harness that encoded
 * its own tiles hid a real bug in the product's encoder. The mitigation is
 * not to trust this file alone -- the Rust round-trip is covered by
 * `ghostframe-protocol/src/eviction.rs`'s own tests, and the full
 * server-to-client path by `ghostframe-e2e/tests/eviction.rs`. If you change
 * a sentinel or the codec on the Rust side, those are what will tell you;
 * this helper must then be updated by hand.
 */
export function buildEvictionDatagram(reason: number): Uint8Array {
  const frags = fragmentTile(
    0,
    EVICTION_SENTINEL_X,
    EVICTION_SENTINEL_Y,
    SKIP_CODEC,
    /* generation */ 0,
    /* pass */ 0,
    new Uint8Array([reason]),
    1,
  );
  if (frags.length !== 1) {
    throw new Error(`expected exactly one eviction datagram, got ${frags.length}`);
  }
  return frags[0];
}
