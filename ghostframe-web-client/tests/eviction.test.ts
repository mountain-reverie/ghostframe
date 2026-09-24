// Proves the wasm boundary surfaces the server's eviction notice
// (ghostframe_protocol::eviction, sentinel tile coords 0xFE/0xFE) as an
// `Evicted` event carrying a numeric reason code -- not a string, and not
// something main.ts has to parse out of raw datagram bytes itself. See
// ghostframe-client-wasm/src/boundary.rs's `WasmEvent::Evicted` and
// ghostframe-client-core/src/reassembly.rs's `is_eviction_sentinel` routing.
import { describe, it, expect } from 'vitest';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';
import { fragmentTile, buildEvictionDatagram } from './helpers/wasm.js';

const EVICTION_REASON_DISPLACED = 1; // EvictionReason::DisplacedByNewSession::to_byte()
const RAW_CODEC = 4; // ghostframe_protocol::protocol::Codec::Raw

function newCore(): InstanceType<typeof wasm.WasmClientCore> {
  return new wasm.WasmClientCore(false, false, false, 0n);
}

describe('eviction notice', () => {
  it('surfaces as an Evicted event carrying the reason code', () => {
    const core = newCore();
    const events = core.handleDatagram(buildEvictionDatagram(EVICTION_REASON_DISPLACED), 0n) as any[];
    const evicted = events.filter((e) => e.kind === 'Evicted');
    expect(evicted).toHaveLength(1);
    expect(evicted[0].reason).toBe(EVICTION_REASON_DISPLACED);
  });

  it('does not fire on an ordinary tile datagram', () => {
    // Sentinel coordinates are what route this. A busy screen must not
    // disconnect the client.
    const core = newCore();
    for (const f of fragmentTile(7, 3, 4, RAW_CODEC, 1, 0, new Uint8Array(8), 8)) {
      expect((core.handleDatagram(f, 0n) as any[]).filter((e) => e.kind === 'Evicted')).toHaveLength(0);
    }
  });
});
