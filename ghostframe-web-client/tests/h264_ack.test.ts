// SPDX-License-Identifier: MIT OR Apache-2.0
// The web client acknowledges H.264 fragments exactly as the native client
// does: both are ClientCore, reached here through the wasm boundary. The
// server records every bundle in its transmission ledger under its bundle_seq
// and judges it lost unless that bundle_seq comes back in an ACK v4 batch
// (0x08), so a browser that decoded H.264 and did not acknowledge it would
// read as a lossy path.
import { describe, expect, it } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import { framePlain } from './helpers/wasm.js';

const NO_ACK = 0xffffffff;

function u32be(n: number): number[] {
  return [(n >>> 24) & 0xff, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff];
}

/** One H.264 frame datagram: the 22-byte v2 header and `payload`. */
function frameDatagram(
  frameSeq: number,
  fragIdx: number,
  fragTotal: number,
  wireSeq: number,
  idr: boolean,
  payload: number[],
): Uint8Array {
  return Uint8Array.from([
    ...u32be(frameSeq),
    (fragIdx >>> 8) & 0xff, fragIdx & 0xff,
    (fragTotal >>> 8) & 0xff, fragTotal & 0xff,
    ...u32be(wireSeq),
    ...u32be(0), // timestamp_us
    idr ? 0x01 : 0x00, // flags
    0x00, // slots
    ...u32be(idr ? NO_ACK : 0), // ref_frame_seq
    ...payload,
  ]);
}

/** The bundle_seqs named by every ACK v4 batch the core has queued. */
function ackedBundleSeqs(core: any, nowUs: bigint): number[] {
  core.onTimeout(nowUs);
  const seqs: number[] = [];
  for (let out = core.pollTransmit(nowUs); out !== undefined; out = core.pollTransmit(nowUs)) {
    if (out.kind !== 'Datagram' || out.bytes[0] !== 0x08) continue;
    const n = out.bytes[1];
    for (let i = 0; i < n; i++) {
      const o = 2 + i * 6;
      seqs.push((out.bytes[o] | (out.bytes[o + 1] << 8) | (out.bytes[o + 2] << 16) | (out.bytes[o + 3] << 24)) >>> 0);
    }
  }
  return seqs.sort((a, b) => a - b);
}

describe('H.264 fragments through the wasm core', () => {
  it('are acknowledged by the bundle that carried them, NO_ACK or not', () => {
    const core = new wasm.WasmClientCore(true, true, true, 0n);
    while (core.pollTransmit(0n) !== undefined) { /* drain the HELLO */ }
    const events: any[] = [];
    events.push(...(core.handleDatagram(framePlain(41, [frameDatagram(7, 0, 2, 41, true, [1, 2, 3])]), 1_000n) as any[]));
    events.push(...(core.handleDatagram(framePlain(42, [frameDatagram(7, 1, 2, 42, true, [4, 5])]), 2_000n) as any[]));
    // A fragment of another frame whose record id the server does not track:
    // its bundle is acknowledged all the same (the record id is not consulted).
    core.handleDatagram(framePlain(43, [frameDatagram(8, 0, 1, NO_ACK, false, [9])]), 3_000n);
    // The frame was reassembled, so the datagrams were taken, not dropped.
    expect(events.some((e) => e.kind === 'NeedsH264')).toBe(true);
    expect(ackedBundleSeqs(core, 20_000n)).toEqual([41, 42, 43]);
  });

  it('a fragment older than the window is still acknowledged', () => {
    const core = new wasm.WasmClientCore(true, true, true, 0n);
    while (core.pollTransmit(0n) !== undefined) { /* drain the HELLO */ }
    core.handleDatagram(framePlain(1, [frameDatagram(50, 0, 1, 1, false, [1])]), 1_000n);
    const stale = core.handleDatagram(framePlain(2, [frameDatagram(10, 0, 1, 2, false, [1])]), 2_000n) as any[];
    expect(stale).toEqual([]);
    expect(ackedBundleSeqs(core, 20_000n)).toEqual([1, 2]);
  });
});
