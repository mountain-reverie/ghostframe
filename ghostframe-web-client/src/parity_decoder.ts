// Mirror of the server's TileParityEnvelope wire format.
//   [0x04][group_first_wire_seq u32 BE][k u8][parity_idx u8]
//   [group_first_payload_len u16 BE][parity_payload]

export const TILE_PARITY_ENVELOPE = 0x04;
const PARITY_HEADER_SIZE = 9;

export interface ParityHeader {
  groupFirstWireSeq: number;
  k: number;
  parityIdx: number;
  groupFirstPayloadLen: number;
  parityPayload: Uint8Array;
}

export function parseParityEnvelope(buf: Uint8Array): ParityHeader {
  if (buf.length < PARITY_HEADER_SIZE) {
    throw new Error(`parity envelope too short: ${buf.length}`);
  }
  if (buf[0] !== TILE_PARITY_ENVELOPE) {
    throw new Error(`expected TILE_PARITY discriminator 0x04, got 0x${buf[0].toString(16)}`);
  }
  const v = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  return {
    groupFirstWireSeq: v.getUint32(1, false),
    k: buf[5],
    parityIdx: buf[6],
    groupFirstPayloadLen: v.getUint16(7, false),
    parityPayload: buf.slice(PARITY_HEADER_SIZE),
  };
}

/**
 * Encode helper used only by tests — production server-side never calls
 * this (it builds parity in Rust). Exported with the `ForTest` suffix to
 * make the testing-only intent explicit.
 */
export function encodeParityEnvelopeForTest(h: ParityHeader): Uint8Array {
  const buf = new Uint8Array(PARITY_HEADER_SIZE + h.parityPayload.length);
  buf[0] = TILE_PARITY_ENVELOPE;
  const v = new DataView(buf.buffer);
  v.setUint32(1, h.groupFirstWireSeq, false);
  buf[5] = h.k;
  buf[6] = h.parityIdx;
  v.setUint16(7, h.groupFirstPayloadLen, false);
  buf.set(h.parityPayload, PARITY_HEADER_SIZE);
  return buf;
}

function xorInto(out: Uint8Array, src: Uint8Array): void {
  const pad = out.length - src.length;
  for (let i = 0; i < src.length; i++) out[pad + i] ^= src[i];
}

export class ParityDecoder {
  private window = new Map<number, Uint8Array>();
  private order: number[] = [];
  private pendingParities = new Map<number, ParityHeader>();
  private windowCapacity: number;

  constructor(windowCapacity: number) { this.windowCapacity = windowCapacity; }

  hasSource(wireSeq: number): boolean { return this.window.has(wireSeq); }

  /**
   * Add a source datagram. Returns a recovered source datagram bytes
   * if the addition completed a pending parity group; null otherwise.
   */
  recordSource(wireSeq: number, bytes: Uint8Array): Uint8Array | null {
    if (!this.window.has(wireSeq)) {
      this.window.set(wireSeq, bytes);
      this.order.push(wireSeq);
      while (this.window.size > this.windowCapacity) {
        const oldest = this.order.shift();
        if (oldest !== undefined) this.window.delete(oldest);
      }
    }
    // Probe pending parities that *might* now be recoverable.
    for (const [gfws, parity] of this.pendingParities) {
      const result = this.tryRecover(parity);
      if (result !== null) {
        this.pendingParities.delete(gfws);
        return result;
      }
    }
    return null;
  }

  receiveParity(parity: ParityHeader): Uint8Array | null {
    const result = this.tryRecover(parity);
    if (result === null) {
      // Buffer for later — a delayed source may complete the group.
      this.pendingParities.set(parity.groupFirstWireSeq, parity);
    }
    return result;
  }

  private tryRecover(parity: ParityHeader): Uint8Array | null {
    const missing: number[] = [];
    const received: Uint8Array[] = [];
    for (let i = 0; i < parity.k; i++) {
      const ws = parity.groupFirstWireSeq + i;
      const src = this.window.get(ws);
      if (src === undefined) missing.push(ws);
      else received.push(src);
    }
    if (missing.length !== 1) return null;
    // Recover: XOR(received sources) XOR parity_payload = missing source.
    const targetLen = Math.max(parity.parityPayload.length, ...received.map(s => s.length));
    const out = new Uint8Array(targetLen);
    xorInto(out, parity.parityPayload);
    for (const src of received) xorInto(out, src);
    return out;
  }
}
