// GPU-level CDF 5/3 debug hooks, installed only under `?debug`.
//
// These started life as M3.3b scaffolding ("removed once the wavelet math is
// verified" -- it was, 2026-05-29) but six e2e tests still drive them
// (e2e_cdf53_bypass_integrate, _integrate_correctness, _live_tile_state*,
// _tile_watcher, _inverse_gradient_tile), so they stay, off the production
// path: a normal session never constructs them, and the e2e harness opts in
// by appending `debug=1` to the page URL.
//
// Every hook pokes the renderer's CDF53 pipeline buffers directly; none is
// safe to call during a real session.

import type { WebGpuRenderer } from './webgpu/renderer.js';
import { protocol } from './protocol_constants.js';
import { prevalidateCdf53 } from '../pkg-web/ghostframe_client_wasm.js';

export function installCdf53Debug(
  renderer: WebGpuRenderer,
  url: URL,
  log: (msg: string) => void,
): void {
  // M3.3b diagnostic: a URL query param `cdf53watch=X,Y` activates the tile
  // watcher BEFORE the datagram loop starts, so the initial cdf53 emission
  // burst (most of the flow for a static gradient frame) is captured.
  // Without this, set-watcher-via-hook misses the burst that happens between
  // page load and the test's first evaluate.
  const watchParam = url.searchParams.get('cdf53watch');
  if (watchParam) {
    const [wx, wy] = watchParam.split(',').map(s => Number(s.trim()));
    if (Number.isFinite(wx) && Number.isFinite(wy)) {
      renderer.cdf53Pipeline.setTileWatcher(wx, wy);
      log(`Cdf53 tile watcher armed for (${wx},${wy})`);
    }
  }

  // M3.3b diagnostic: test-only hook to drive Cdf53 inverse with hand-supplied
  // coefficients (bypasses the integrate shader). Removed once the wavelet
  // math is verified.
  window.__cdf53TestInverse = async (
    coefficientsI16: number[],  // length 3072 (3 channels × 1024 i16 each)
    targetTileIdx: number = 0,  // optional, default tile 0 for backward compat
  ): Promise<number[]> => {
    const pipe = renderer.cdf53Pipeline;
    const device = renderer.device;

    // Split signed i16 coefficients into magnitudes + signs.
    const coefU32 = new Uint32Array(1536);
    const signU32 = new Uint32Array(96);
    for (let ch = 0; ch < 3; ch++) {
      for (let i = 0; i < 1024; i++) {
        const c = coefficientsI16[ch * 1024 + i];
        const mag = (c < 0 ? -c : c) & 0xFFFF;
        const wordIdx = ch * 512 + (i >> 1);
        if ((i & 1) === 0) {
          coefU32[wordIdx] = (coefU32[wordIdx] & 0xFFFF0000) | mag;
        } else {
          coefU32[wordIdx] = (coefU32[wordIdx] & 0x0000FFFF) | (mag << 16);
        }
        if (c < 0) {
          const signWordIdx = ch * 32 + (i >> 5);
          signU32[signWordIdx] |= 1 << (i & 31);
        }
      }
    }
    // Write into target tile region of each buffer.
    device.queue.writeBuffer(pipe.coefficientBuffer, targetTileIdx * 6144, coefU32);
    device.queue.writeBuffer(pipe.signBuffer, targetTileIdx * 384, signU32);
    device.queue.writeBuffer(pipe.tileGenBuffer, targetTileIdx * 4, new Uint32Array([1]));

    // Run inverse passes.
    const encoder = device.createCommandEncoder();
    pipe.encodeInverseAll(encoder);
    device.queue.submit([encoder.finish()]);

    // Read back the target tile's pixels via __readPixelRect.
    const ts = protocol().tileSize;
    const cols = Math.ceil(renderer.framebuffer.width / ts);
    const tileX = targetTileIdx % cols;
    const tileY = Math.floor(targetTileIdx / cols);
    if (!window.__readPixelRect) {
      throw new Error('__cdf53TestInverse needs the pixel readbacks: load the page with ?e2e as well as ?debug');
    }
    return await window.__readPixelRect(tileX * ts, tileY * ts, ts, ts);
  };

  // M3.3b diagnostic: test-only hook to drive the Cdf53 integrate shader
  // directly with hand-supplied RLE-encoded passes for tile 0, then read back
  // the resulting coefficientBuffer + signBuffer. Used to isolate whether the
  // integrate shader (or uploadBatch packing) is correct independently of the
  // wire path.
  window.__cdf53TestIntegrate = async (
    encodedPasses: number[][],  // 14 entries, each is the raw RLE-encoded payload as a number[]
  ): Promise<{ coefficients: number[]; signs: number[] }> => {
    const pipe = renderer.cdf53Pipeline;
    const device = renderer.device;

    // Reset per-tile state for tile 0.
    device.queue.writeBuffer(pipe.coefficientBuffer, 0, new Uint8Array(6144));  // 1536 u32 × 4 B
    device.queue.writeBuffer(pipe.signBuffer, 0, new Uint8Array(384));          // 96 u32 × 4 B
    device.queue.writeBuffer(pipe.tileGenBuffer, 0, new Uint32Array([0]));      // force gen-bump on first pass

    // Build the batch from the 14 encoded passes (all for tile 0, gen=1).
    // `prevalidateCdf53` (wasm) returns a flat result — no nested `entry` to
    // mutate, so tileX/tileY are filled in directly on construction.
    const entries: Array<{ tileX: number; tileY: number; gen: number; passIdx: number; bitPlanes: Uint8Array }> = [];
    for (let passIdx = 0; passIdx < encodedPasses.length; passIdx++) {
      const payload = new Uint8Array(encodedPasses[passIdx]);
      const r = prevalidateCdf53(payload, 1, passIdx);
      if (!r.ok) throw new Error('prevalidate failed for pass ' + passIdx + ' err=' + r.code);
      entries.push({
        tileX: 0,
        tileY: 0,
        gen: r.generation,
        passIdx: r.pass_idx,
        bitPlanes: r.bit_planes,
      });
    }

    // Upload + integrate.
    pipe.uploadBatch(entries);
    const encoder = device.createCommandEncoder();
    pipe.encodeIntegrate(encoder, entries.length);
    device.queue.submit([encoder.finish()]);

    // Read back coefficientBuffer[0..1536] and signBuffer[0..96] via staging.
    const coefStaging = device.createBuffer({
      size: 6144, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
    });
    const signStaging = device.createBuffer({
      size: 384, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
    });
    const copyEnc = device.createCommandEncoder();
    copyEnc.copyBufferToBuffer(pipe.coefficientBuffer, 0, coefStaging, 0, 6144);
    copyEnc.copyBufferToBuffer(pipe.signBuffer, 0, signStaging, 0, 384);
    device.queue.submit([copyEnc.finish()]);

    await coefStaging.mapAsync(GPUMapMode.READ);
    await signStaging.mapAsync(GPUMapMode.READ);
    const coefArr = Array.from(new Uint32Array(coefStaging.getMappedRange()));
    const signArr = Array.from(new Uint32Array(signStaging.getMappedRange()));
    coefStaging.unmap(); coefStaging.destroy();
    signStaging.unmap(); signStaging.destroy();
    return { coefficients: coefArr, signs: signArr };
  };

  // M3.3b diagnostic: dump per-tile GPU state (tileGen + coefficients + signs)
  // for a single tile index. Used by e2e_cdf53_live_tile_state to inspect the
  // live integrate path under real-world load.
  window.__cdf53DumpTileState = async (
    tileIdx: number,
  ): Promise<{ tileGen: number; coefficients: number[]; signs: number[] }> => {
    const pipe = renderer.cdf53Pipeline;
    const device = renderer.device;

    const coefStaging = device.createBuffer({
      size: 6144, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
    });
    const signStaging = device.createBuffer({
      size: 384, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
    });
    const genStaging = device.createBuffer({
      size: 4, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ,
    });
    const enc = device.createCommandEncoder();
    enc.copyBufferToBuffer(pipe.coefficientBuffer, tileIdx * 6144, coefStaging, 0, 6144);
    enc.copyBufferToBuffer(pipe.signBuffer, tileIdx * 384, signStaging, 0, 384);
    enc.copyBufferToBuffer(pipe.tileGenBuffer, tileIdx * 4, genStaging, 0, 4);
    device.queue.submit([enc.finish()]);

    await coefStaging.mapAsync(GPUMapMode.READ);
    await signStaging.mapAsync(GPUMapMode.READ);
    await genStaging.mapAsync(GPUMapMode.READ);
    const coefArr = Array.from(new Uint32Array(coefStaging.getMappedRange()));
    const signArr = Array.from(new Uint32Array(signStaging.getMappedRange()));
    const tileGen = new Uint32Array(genStaging.getMappedRange())[0];
    coefStaging.unmap(); coefStaging.destroy();
    signStaging.unmap(); signStaging.destroy();
    genStaging.unmap(); genStaging.destroy();
    return { tileGen, coefficients: coefArr, signs: signArr };
  };

  // M3.3b diagnostic: per-tile JS-side upload watcher. Captures the bytes
  // every uploadBatch hands off to the GPU for the watched (tileX, tileY),
  // along with the (gen, passIdx) the renderer attributed to that entry.
  // Used by `e2e_cdf53_tile_watcher` to verify the JS→GPU handoff is correct
  // independent of the integrate shader's behavior.
  window.__cdf53SetTileWatcher = (tileX: number, tileY: number) => {
    renderer.cdf53Pipeline.setTileWatcher(tileX, tileY);
  };

  // M3.3b queue-identity probe. Reports live state of renderer.cdf53Queue
  // and the pipeline's totals, so we can confirm whether pushes and
  // uploadBatch share the same array.
  window.__cdf53Probe = () => {
    return {
      cdf53QueueLengthNow: renderer.cdf53Queue.length,
      uploadBatchCallsLifetime: renderer.cdf53Pipeline.uploadBatchCalls,
      totalEntriesLifetime: renderer.cdf53Pipeline.totalEntries,
      isQueueAnArray: Array.isArray(renderer.cdf53Queue),
      pipelineCtorName: renderer.cdf53Pipeline.constructor.name,
      // Spot-check: prove the same Cdf53Pipeline instance is bound through
      // the renderer property accessor by storing a token and reading it back.
      sameInstanceCheck: (() => {
        (renderer.cdf53Pipeline as any).__token = 'probe-token-' + Date.now();
        return (renderer.cdf53Pipeline as any).__token;
      })(),
      // Read the watcher state directly through renderer.cdf53Pipeline so we
      // can tell if the watcher was reset somewhere after we set it.
      tileWatcherXNow: (renderer.cdf53Pipeline as any).tileWatcherX,
      tileWatcherYNow: (renderer.cdf53Pipeline as any).tileWatcherY,
      uploadBatchWithWatcherNull: (renderer.cdf53Pipeline as any).uploadBatchWithWatcherNull,
      uploadBatchWithWatcherSet: (renderer.cdf53Pipeline as any).uploadBatchWithWatcherSet,
      entriesWhileWatcherNull: (renderer.cdf53Pipeline as any).entriesWhileWatcherNull,
      entriesWhileWatcherSet: (renderer.cdf53Pipeline as any).entriesWhileWatcherSet,
      seenTilesAddCalls: (renderer.cdf53Pipeline as any).seenTilesAddCalls,
    };
  };
  window.__cdf53GetTileWatcher = () => {
    const pipe = renderer.cdf53Pipeline;
    return {
      captures: pipe.tileWatcherCaptures.map(c => ({
        batchSize: c.batchSize,
        entryIdx: c.entryIdx,
        tileX: c.tileX,
        tileY: c.tileY,
        gen: c.gen,
        passIdx: c.passIdx,
        bitPlanesOffset: c.bitPlanesOffset,
        bitPlanes: Array.from(c.bitPlanes),
      })),
      stats: {
        uploadBatchCalls: pipe.uploadBatchCalls,
        totalEntries: pipe.totalEntries,
        distinctTilesSeen: pipe.seenTiles.size,
        sampleTiles: Array.from(pipe.seenTiles).slice(0, 30),
      },
    };
  };
}
