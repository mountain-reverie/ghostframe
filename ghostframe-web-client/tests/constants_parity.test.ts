// Asserts every wasm-exported protocol constant equals the TypeScript
// constant it replaces, while both still exist. This is the cheapest
// equivalence check in the whole cutover: it runs in milliseconds and would
// catch a discriminant or message-type that drifted during the port.
//
// Deleted at step 4 together with the TS constants it reads.
import { describe, it, expect } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import {
  ACK_BATCH_MSG_TYPE, ACK_ENTRY_SIZE, ACK_OVERLAP_COUNT, MAX_ACK_ENTRIES,
} from '../src/ack';
import { TILE_NACK_ENVELOPE, NACK_BATCH_FLUSH_MS, NACK_BATCH_MAX } from '../src/nack.js';
import { TILE_PARITY_ENVELOPE } from '../src/parity_decoder.js';
import {
  HELLO_MSG_TYPE, HELLO_SIZE, DECODE_ERROR_MSG_TYPE, DECODE_ERROR_SIZE,
  ERR_PAYLOAD_TOO_SHORT, ERR_COUNT_OUT_OF_RANGE, ERR_THIN_UNCACHED_PALETTE,
  ERR_BUNDLED_TRUNCATED, ERR_INDEX_OOB, ERR_RLE_OVERSHOOT, ERR_RLE_UNDERSHOOT,
  ERR_CDF53_BAD_PASS, ERR_CDF53_TRUNCATED, ERR_CDF53_RLE_LENGTH,
} from '../src/feedback.js';

describe('protocol constants match between TS and wasm', () => {
  it('ack', () => {
    expect(Number(wasm.ackBatchMsgType())).toBe(ACK_BATCH_MSG_TYPE);
    expect(Number(wasm.ackEntrySize())).toBe(ACK_ENTRY_SIZE);
    expect(Number(wasm.ackOverlapCount())).toBe(ACK_OVERLAP_COUNT);
    expect(Number(wasm.maxAckEntries())).toBe(MAX_ACK_ENTRIES);
  });

  it('nack', () => {
    expect(Number(wasm.tileNackEnvelope())).toBe(TILE_NACK_ENVELOPE);
    expect(Number(wasm.nackBatchFlushMs())).toBe(NACK_BATCH_FLUSH_MS);
    expect(Number(wasm.nackBatchMax())).toBe(NACK_BATCH_MAX);
  });

  it('parity', () => {
    expect(Number(wasm.tileParityEnvelope())).toBe(TILE_PARITY_ENVELOPE);
  });

  it('feedback message types', () => {
    expect(Number(wasm.helloMsgType())).toBe(HELLO_MSG_TYPE);
    expect(Number(wasm.helloSize())).toBe(HELLO_SIZE);
    expect(Number(wasm.decodeErrorMsgType())).toBe(DECODE_ERROR_MSG_TYPE);
    expect(Number(wasm.decodeErrorSize())).toBe(DECODE_ERROR_SIZE);
  });

  it('every decode-error discriminant', () => {
    expect(wasm.errorCodes()).toEqual({
      ERR_PAYLOAD_TOO_SHORT, ERR_COUNT_OUT_OF_RANGE, ERR_THIN_UNCACHED_PALETTE,
      ERR_BUNDLED_TRUNCATED, ERR_INDEX_OOB, ERR_RLE_OVERSHOOT, ERR_RLE_UNDERSHOOT,
      ERR_CDF53_BAD_PASS, ERR_CDF53_TRUNCATED, ERR_CDF53_RLE_LENGTH,
    });
  });
});
