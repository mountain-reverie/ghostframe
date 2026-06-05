export const FEEDBACK_MSG_TYPE = 0x01;
export const FEEDBACK_SIZE = 22;

export class LossTracker {
  private received = 0;
  private lost = 0;
  private recoveredFec = 0;
  private lastTimestampMs = 0;
  private suspensionDetected = false;

  /** Call for every received datagram (source or parity). */
  onDatagram(): void {
    const now = performance.now();
    if (this.lastTimestampMs > 0 && (now - this.lastTimestampMs) > 100) {
      this.suspensionDetected = true;
    }
    this.lastTimestampMs = now;
    this.received++;
  }

  /** Call when a stale assembly is evicted with missing fragments. */
  onStaleTile(expectedFragments: number, receivedFragments: number): void {
    const missing = expectedFragments - receivedFragments;
    if (missing > 0) {
      this.lost += missing;
    }
  }

  /** Call when a fragment is recovered via FEC. */
  onFecRecovery(): void {
    this.recoveredFec++;
  }

  /** Encode and reset counters. Returns a 22-byte feedback message. */
  encodeFeedback(): Uint8Array {
    const buf = new Uint8Array(FEEDBACK_SIZE);
    const view = new DataView(buf.buffer);

    const nowNs = BigInt(Math.round(performance.now() * 1_000_000));

    buf[0] = FEEDBACK_MSG_TYPE;
    view.setBigUint64(1, nowNs, false);
    view.setUint32(9, this.received, false);
    view.setUint32(13, this.lost, false);
    view.setUint32(17, this.recoveredFec, false);
    buf[21] = this.suspensionDetected ? 1 : 0;

    // Reset counters for next interval
    this.received = 0;
    this.lost = 0;
    this.recoveredFec = 0;
    this.suspensionDetected = false;

    return buf;
  }
}

// ---------------------------------------------------------------------------
// HELLO message (msg_type = 0x03) — capability advertisement
//
// Wire format (2 bytes):
//   [0]  msg_type = 0x03
//   [1]  capabilities : u8   bit 0 = supports indices_raw
//                            bits 1..7 reserved
// ---------------------------------------------------------------------------

export const HELLO_MSG_TYPE = 0x03;
export const HELLO_SIZE = 2;

export interface ClientCapabilities {
  indicesRawEnabled: boolean;
}

export function encodeHello(caps: ClientCapabilities): Uint8Array {
  let capsByte = 0;
  if (caps.indicesRawEnabled) capsByte |= 0x01;
  return new Uint8Array([HELLO_MSG_TYPE, capsByte]);
}

// ---------------------------------------------------------------------------
// DECODE_ERROR message (msg_type = 0x04) — per-tile decode failure
// ---------------------------------------------------------------------------

export const DECODE_ERROR_MSG_TYPE = 0x04;
export const DECODE_ERROR_SIZE = 5;

export const ERR_PAYLOAD_TOO_SHORT = 1;
export const ERR_COUNT_OUT_OF_RANGE = 2;
export const ERR_THIN_UNCACHED_PALETTE = 3;
export const ERR_BUNDLED_TRUNCATED = 4;
export const ERR_INDEX_OOB = 5;
export const ERR_RLE_OVERSHOOT = 6;
export const ERR_RLE_UNDERSHOOT = 7;

// M3.3b: Cdf53 prevalidation error codes.
export const ERR_CDF53_BAD_PASS = 8;       // pass_idx >= 14
export const ERR_CDF53_TRUNCATED = 9;      // length field exceeds payload remaining
export const ERR_CDF53_RLE_LENGTH = 10;    // decoded bit-plane != 128 bytes

export interface DecodeErrorMsg {
  codec: number;
  tileX: number;
  tileY: number;
  errorCode: number;
}

export function encodeDecodeError(m: DecodeErrorMsg): Uint8Array {
  return new Uint8Array([
    DECODE_ERROR_MSG_TYPE,
    m.codec & 0xFF,
    m.tileX & 0xFF,
    m.tileY & 0xFF,
    m.errorCode & 0xFF,
  ]);
}
