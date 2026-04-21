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
