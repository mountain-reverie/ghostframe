import { describe, it, expect } from 'vitest';
import { ParityDecoder, parseParityEnvelope, encodeParityEnvelopeForTest } from '../src/parity_decoder.js';

const FEC_K = 10;

function fakeSource(wireSeq: number, payload: number): Uint8Array {
  // 16-byte DatagramHeader + 8-byte TileHeader + 1-byte payload
  const buf = new Uint8Array(25);
  const v = new DataView(buf.buffer);
  v.setUint32(0, 0x80000000 | wireSeq, false);  // frame_seq with TILE_DATAGRAM_FLAG
  v.setUint16(4, 0, false);  // frag_idx
  v.setUint16(6, 1, false);  // frag_total
  v.setUint32(8, wireSeq, false);  // wire_seq
  v.setUint32(12, 0, false);  // timestamp_us
  // tile header bytes 16..23 left as 0
  buf[24] = payload;
  return buf;
}

function xorBytes(...slices: Uint8Array[]): Uint8Array {
  const maxLen = slices.reduce((m, s) => Math.max(m, s.length), 0);
  const out = new Uint8Array(maxLen);
  for (const s of slices) {
    const pad = maxLen - s.length;
    for (let i = 0; i < s.length; i++) out[pad + i] ^= s[i];
  }
  return out;
}

describe('ParityDecoder', () => {
  it('recovers a single missing source from K-1 + parity', () => {
    const decoder = new ParityDecoder(FEC_K * 4);
    const sources = Array.from({ length: FEC_K }, (_, i) => fakeSource(i, i));
    const parity = xorBytes(...sources);
    // Feed K-1 sources (skip index 5)
    for (let i = 0; i < FEC_K; i++) {
      if (i !== 5) decoder.recordSource(i, sources[i]);
    }
    const envelope = encodeParityEnvelopeForTest({
      groupFirstWireSeq: 0,
      k: FEC_K,
      parityIdx: 0,
      groupFirstPayloadLen: sources[0].length,
      parityPayload: parity,
    });
    const recovered = decoder.receiveParity(parseParityEnvelope(envelope));
    expect(recovered).not.toBeNull();
    expect(recovered).toEqual(sources[5]);
  });

  it('returns null when multiple sources are missing', () => {
    const decoder = new ParityDecoder(FEC_K * 4);
    const sources = Array.from({ length: FEC_K }, (_, i) => fakeSource(i, i));
    const parity = xorBytes(...sources);
    // Feed K-2 sources
    for (let i = 0; i < FEC_K - 2; i++) decoder.recordSource(i, sources[i]);
    const envelope = encodeParityEnvelopeForTest({
      groupFirstWireSeq: 0,
      k: FEC_K,
      parityIdx: 0,
      groupFirstPayloadLen: sources[0].length,
      parityPayload: parity,
    });
    const recovered = decoder.receiveParity(parseParityEnvelope(envelope));
    expect(recovered).toBeNull();
  });

  it('returns null when no sources are missing', () => {
    const decoder = new ParityDecoder(FEC_K * 4);
    const sources = Array.from({ length: FEC_K }, (_, i) => fakeSource(i, i));
    const parity = xorBytes(...sources);
    for (let i = 0; i < FEC_K; i++) decoder.recordSource(i, sources[i]);
    const recovered = decoder.receiveParity(parseParityEnvelope(
      encodeParityEnvelopeForTest({
        groupFirstWireSeq: 0, k: FEC_K, parityIdx: 0,
        groupFirstPayloadLen: sources[0].length, parityPayload: parity,
      })
    ));
    expect(recovered).toBeNull();
  });

  it('evicts oldest sources when window full', () => {
    const decoder = new ParityDecoder(4);
    decoder.recordSource(0, new Uint8Array([1]));
    decoder.recordSource(1, new Uint8Array([2]));
    decoder.recordSource(2, new Uint8Array([3]));
    decoder.recordSource(3, new Uint8Array([4]));
    decoder.recordSource(4, new Uint8Array([5]));  // evicts wire_seq 0
    expect(decoder.hasSource(0)).toBe(false);
    expect(decoder.hasSource(4)).toBe(true);
  });

  it('replays buffered parity when missing source finally arrives', () => {
    const decoder = new ParityDecoder(FEC_K * 4);
    const sources = Array.from({ length: FEC_K }, (_, i) => fakeSource(i, i));
    const parity = xorBytes(...sources);
    // Feed K-2 sources, leaving indices K-2 and K-1 BOTH missing.
    for (let i = 0; i < FEC_K - 2; i++) decoder.recordSource(i, sources[i]);
    const parityEnvelope = parseParityEnvelope(encodeParityEnvelopeForTest({
      groupFirstWireSeq: 0, k: FEC_K, parityIdx: 0,
      groupFirstPayloadLen: sources[0].length, parityPayload: parity,
    }));
    // Parity arrives with 2 missing — buffer, return null.
    expect(decoder.receiveParity(parityEnvelope)).toBeNull();
    // Add source K-2 — now only K-1 is missing. The buffered parity replays
    // and recovers sources[K-1].
    const recovered = decoder.recordSource(FEC_K - 2, sources[FEC_K - 2]);
    expect(recovered).not.toBeNull();
    expect(recovered).toEqual(sources[FEC_K - 1]);
  });
});
