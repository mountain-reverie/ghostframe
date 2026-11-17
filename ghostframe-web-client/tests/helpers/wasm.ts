// SPDX-License-Identifier: MIT OR Apache-2.0
// Datagram-builder helpers shared by the suites that drive WasmClientCore's
// tile reassembly directly (cdf53_globals.test.ts).
//
// This file used to also hold Clock/AckHarness/NackHarness/
// parseNackEnvelopeNested, adapting the wasm ACK/NACK batchers' poll-and-
// injected-time API to the shape the pre-cutover ack.test.ts/nack.test.ts
// suites were written against. Phase 4b deleted those two suites (they
// duplicated ghostframe-client-core's oracle_*.rs tests), so that adapter
// code went with them — fragmentTile is the only survivor.

import * as wasm from '../../pkg-node/ghostframe_client_wasm.js';

const TILE_DATAGRAM_FLAG = 0x80000000;

/**
 * One rev 6 PLAIN frame carrying `records` as bundle `bundleSeq`. Every
 * datagram a `WasmClientCore` accepts is a frame, so a suite that feeds it a
 * bare record wraps it with this (built by the Rust `frame_plain`, through
 * `framePlainForTest`, so the framing is never re-implemented here).
 */
export function framePlain(bundleSeq: number, records: Uint8Array[]): Uint8Array {
  const total = records.reduce((n, r) => n + r.length, 0);
  const concat = new Uint8Array(total);
  let off = 0;
  for (const r of records) {
    concat.set(r, off);
    off += r.length;
  }
  return wasm.framePlainForTest(bundleSeq, concat, Uint32Array.from(records.map((r) => r.length)));
}

let nextBundleSeq = 0;

/** [`framePlain`] with one record and a fresh `bundle_seq` per call. */
export function frameOne(record: Uint8Array): Uint8Array {
  return framePlain(nextBundleSeq++, [record]);
}

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
 * Shared by `tests/cdf53_globals.test.ts`
 * so it drives the wasm core with the exact same wire bytes rather than
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
