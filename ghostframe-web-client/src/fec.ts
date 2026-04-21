export const PARITY_HEADER_SIZE = 3;

export interface ParityInfo {
  groupStart: number;
  groupLen: number;
  xorData: Uint8Array;
}

/**
 * Decode the 3-byte parity header from a parity datagram payload.
 * Layout: group_start (u16 BE) | group_len (u8) | xor_data...
 * Returns null if the payload is too short to contain the header.
 */
export function decodeParityPayload(payload: Uint8Array): ParityInfo | null {
  if (payload.length < PARITY_HEADER_SIZE) {
    return null;
  }
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
  const groupStart = view.getUint16(0, false); // big-endian
  const groupLen = view.getUint8(2);
  const xorData = payload.slice(PARITY_HEADER_SIZE);
  return { groupStart, groupLen, xorData };
}

/**
 * XOR all received payloads together with xorData, producing the recovered fragment.
 * The result length matches xorData.length; shorter buffers are zero-padded implicitly.
 */
export function recoverFragment(receivedPayloads: Uint8Array[], xorData: Uint8Array): Uint8Array {
  return xorBuffers([...receivedPayloads, xorData], xorData.length);
}

function xorBuffers(buffers: Uint8Array[], maxLen: number): Uint8Array {
  const result = new Uint8Array(maxLen);
  for (const buf of buffers) {
    for (let i = 0; i < buf.length; i++) {
      result[i] ^= buf[i];
    }
  }
  return result;
}

export class ParityRecovery {
  /** Parity info keyed by group_start index. */
  private parities = new Map<number, ParityInfo>();

  addParity(payload: Uint8Array): void {
    const info = decodeParityPayload(payload);
    if (info) {
      this.parities.set(info.groupStart, info);
    }
  }

  /**
   * Try to recover a missing fragment.
   * @param missingIdx - the frag_idx of the missing source fragment
   * @param fragments - the assembly's fragment array (null for missing)
   * @param k - the parity group size (default 4)
   * @returns the recovered payload, or null if recovery isn't possible
   */
  tryRecover(
    missingIdx: number,
    fragments: (Uint8Array | null)[],
    k: number = 4,
  ): Uint8Array | null {
    // Find which group this fragment belongs to
    const groupStart = Math.floor(missingIdx / k) * k;
    const parity = this.parities.get(groupStart);
    if (!parity) return null;

    // Check that exactly one fragment in the group is missing
    const groupEnd = Math.min(groupStart + parity.groupLen, fragments.length);
    const received: Uint8Array[] = [];
    let missingCount = 0;

    for (let i = groupStart; i < groupEnd; i++) {
      if (fragments[i] === null) {
        missingCount++;
        if (missingCount > 1) return null; // can't recover 2+ losses
      } else {
        received.push(fragments[i]!);
      }
    }

    if (missingCount !== 1) return null;

    return recoverFragment(received, parity.xorData);
  }

  clear(): void {
    this.parities.clear();
  }
}
