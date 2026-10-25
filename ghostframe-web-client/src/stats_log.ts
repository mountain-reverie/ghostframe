// The periodic page-log summary (~every 2 s): per-rAF drain deltas,
// framebuffer size, CDF53 coverage from the wasm core, NACKs sent, and a
// four-pixel framebuffer probe.
//
// Everything printed here is live. Counters the wasm cutover orphaned
// (`rx{}`, `lastSeq`, the FEC recovery counters, per-tier receive bytes)
// used to print an "n/a(wasm-internal)" sentinel; they are gone rather than
// kept as lines that look like measurements. What remains and is fed:
//   - `Δdrain`   queue entries consumed per window (counted here)
//   - `cdf53-coverage` / `cdf53-incomplete`  from `core.cdf53Coverage()`
//   - `nack_sent`  NACK envelopes `drainTransmit` saw the core emit
//   - `cdf53fails`  from the recordProtocolEvent counters
//
// Identical snapshots are dropped entirely: no heartbeat, silence means
// nothing changed.

import type { WebGpuRenderer } from './webgpu/renderer.js';
import { protocol } from './protocol_constants.js';

/** The slice of `WasmClientCore` this logger reads. */
export interface StatsCore {
  cdf53Coverage(): unknown;
  cdf53IncompleteTiles(limit: number): unknown;
}

export interface StatsLoggerDeps {
  renderer: WebGpuRenderer;
  core: StatsCore;
  log: (msg: string) => void;
  getNackSent: () => number;
}

export interface QueueDepths {
  solid: number;
  palrle: number;
  cdf53: number;
  h264: number;
}

const STATS_INTERVAL_MS = 2000;

export function createStatsLogger({ renderer, core, log, getNackSent }: StatsLoggerDeps) {
  const drainTotals: QueueDepths = { solid: 0, palrle: 0, cdf53: 0, h264: 0 };
  let lastDrain: QueueDepths = { ...drainTotals };
  let lastStatsMs = 0;
  let lastLineKey = '';

  return {
    /** Call once per rAF, after the frame was encoded. `beforeDrain` is the
     * queue depths sampled BEFORE `encodeAndPresentFrame` consumed them. */
    onTick(beforeDrain: QueueDepths, rafTicks: number): void {
      drainTotals.solid += beforeDrain.solid;
      drainTotals.palrle += beforeDrain.palrle;
      drainTotals.cdf53 += beforeDrain.cdf53;
      drainTotals.h264 += beforeDrain.h264;

      const nowMs = performance.now();
      if (nowMs - lastStatsMs <= STATS_INTERVAL_MS) return;
      lastStatsMs = nowMs;

      const w = window as unknown as {
        __cdf53PrevalidateFails?: number;
        __cdf53LastFailCode?: number;
        __readPixel?: (x: number, y: number) => Promise<number[]>;
      };
      const drainDelta: QueueDepths = {
        solid: drainTotals.solid - lastDrain.solid,
        palrle: drainTotals.palrle - lastDrain.palrle,
        cdf53: drainTotals.cdf53 - lastDrain.cdf53,
        h264: drainTotals.h264 - lastDrain.h264,
      };
      lastDrain = { ...drainTotals };
      const fb = renderer.framebuffer;
      const cdf53Fails = w.__cdf53PrevalidateFails ?? 0;
      const cdf53Last = w.__cdf53LastFailCode ?? '-';
      const nackSent = getNackSent();

      const statsLine =
        `stats: Δdrain{s:${drainDelta.solid} p:${drainDelta.palrle} ` +
        `c:${drainDelta.cdf53} h:${drainDelta.h264}} ` +
        `fb:${fb.width}x${fb.height} ` +
        `cdf53fails:${cdf53Fails}(last=${cdf53Last}) ` +
        `raf:${rafTicks} submit:${renderer.submitCount}`;

      // `partial` is the field to watch: tiles still owed passes.
      // `gave_up` is narrower: tiles that exhausted MAX_TAIL_SWEEP_ATTEMPTS
      // on a known gap. A lost final pass has no gap evidence and appears
      // only in `partial`. `incomplete` then names them.
      let coverageLine: string;
      let incompleteLine: string | null = null;
      try {
        const cov = core.cdf53Coverage() as {
          line: string;
          partial: number;
          gaveUp?: number;
          gave_up?: number;
        };
        coverageLine = `cdf53-coverage: ${cov.line}`;
        const stuck = cov.gave_up ?? cov.gaveUp ?? 0;
        if (cov.partial > 0 || stuck > 0) {
          const worst = core.cdf53IncompleteTiles(8) as Array<{
            tile_x: number;
            tile_y: number;
            missing: number;
            sweep_attempts: number;
          }>;
          incompleteLine =
            'cdf53-incomplete: ' +
            worst
              .map(
                t =>
                  `(${t.tile_x},${t.tile_y}) missing=0b${t.missing
                    .toString(2)
                    .padStart(protocol().cdf53PassCount, '0')} sweeps=${t.sweep_attempts}`,
              )
              .join(' ');
        }
      } catch (e) {
        coverageLine = `cdf53-coverage: unavailable (${String(e)})`;
      }
      const nackLine = `nack_sent=${nackSent}`;

      // Dedup key: excludes `raf:` (always changes) and bookkeeping that
      // creeps; suppress only when nothing the reader cares about moved.
      const lineKey =
        `drain:${drainDelta.solid}/${drainDelta.palrle}/` +
        `${drainDelta.cdf53}/${drainDelta.h264}|` +
        `fb:${fb.width}x${fb.height}|fails:${cdf53Fails}|nack:${nackSent}`;
      const statsChanged = lineKey !== lastLineKey;
      if (!statsChanged) return;
      lastLineKey = lineKey;
      log(statsLine);
      log(coverageLine);
      if (incompleteLine) log(incompleteLine);
      log(nackLine);

      // Framebuffer probe: sample four known positions to settle "is the
      // texture written or is the blit broken". Only when the line changed
      // (identical output would be suppressed anyway; mapAsync isn't free).
      const readPixel = w.__readPixel;
      if (readPixel && fb.width > 0 && fb.height > 0) {
        const xs = [16, 960, 768, 1900];
        const ys = [16, 540, 992, 1060];
        Promise.all(
          xs.map((x, i) =>
            readPixel(Math.min(x, fb.width - 1), Math.min(ys[i], fb.height - 1)),
          ),
        )
          .then(samples => {
            const fmt = (px: number[]) => `[${px[0]},${px[1]},${px[2]},${px[3]}]`;
            log(
              `fb-pixels: tl(16,16)=${fmt(samples[0])} ` +
                `ctr(960,540)=${fmt(samples[1])} ` +
                `tile(768,992)=${fmt(samples[2])} ` +
                `br(1900,1060)=${fmt(samples[3])}`,
            );
          })
          .catch(err => log(`fb-pixels readback failed: ${err}`));
      }
    },
  };
}
