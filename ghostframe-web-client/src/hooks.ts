// SPDX-License-Identifier: MIT OR Apache-2.0
// The page's e2e bookkeeping, called by the Rust client
// (ghostframe-client-wasm/src/app/hooks.rs) in a fixed order: per tick
// rafTick, events+datagramCounts (flush), events (timeout), beforePaint,
// afterPaint; per flush outside a tick events+datagramCounts. PR 4 ports this
// file to Rust and deletes it.
import { initDiagnostics, recordPaletteWrite, type DiagnosticsHandle } from './diagnostics.js';
import { recordProtocolEvent, codecDiscriminant, type Cdf53ErrorCodes } from './cdf53_globals.js';
import { createStatsLogger } from './stats_log.js';
import { health } from './health.js';
import { errorCodes, type DebugApi } from '../pkg-web/ghostframe_client_wasm.js';

type SlimTileData =
  | { codec: 'Solid'; bytes: Uint8Array; len: number }
  | { codec: 'PalRle'; palette_id: number; count: number; wire_flags: number; indices: Uint8Array; len: number }
  | { codec: 'Cdf53'; pass_idx: number; bit_planes: Uint8Array; present_passes?: number | null; len: number };

/** The record kinds Rust passes to `events`: the ones with bookkeeping. */
export type HookRecord =
  | {
      kind: 'TilePayload';
      frame_seq: number;
      tile_x: number;
      tile_y: number;
      generation: number;
      data: SlimTileData;
      error?: string;
    }
  | { kind: 'PaletteUpdated'; palette_id: number; colors: [number, number, number, number][]; error?: string }
  | { kind: 'FrameDimensions'; width: number; height: number; applied: boolean; reason?: string }
  | { kind: 'DecodeError'; codec: number; tile_x: number; tile_y: number; code: number };

export function createHooks(diagnostics: boolean) {
  let api: DebugApi | null = null;
  let diag: DiagnosticsHandle | null = null;
  let statsLog: ReturnType<typeof createStatsLogger> | null = null;
  let cdf53ErrorCodes: Cdf53ErrorCodes | null = null;
  let nackSent = 0;
  // The framebuffer size the server last announced (applied), for the recorders.
  let fbW = 0;
  let fbH = 0;

  // M3.5 bench: per-frame_seq earliest tile-receive timestamp. Keyed by
  // frame_seq; cleared once the frame's paint has been recorded.
  const firstRecvMs = new Map<number, number>();

  // A frame is "done" once the server has moved two sequences past it (the
  // core's stale-assembly threshold): every tile that will arrive has. It
  // then enters pendingFramePaintRaf and the next rAF tick records its
  // paint time into window.__ghostframe_framePaints, which the bench
  // harness (scene.rs read_client_diagnostics) turns into per-frame latency.
  // Fed from TilePayload events -- the per-datagram path that used to feed
  // it was deleted at the wasm cutover and this stayed empty until now.
  // Not a paint-latency measurement: "painted" is stamped on the first rAF
  // after the server is two sequences ahead, so the sample is at least two
  // frame intervals long (unbounded on a sparse scene). It proves the global
  // is live and gives a coarse upper bound; do not read it as photon latency.
  const MAX_PENDING_FRAME_STAMPS = 1024;
  let latestTileFrameSeq = 0;
  const pendingFramePaintRaf = new Set<number>();
  function noteFrameTileReceived(frameSeq: number, nowMs: number): number {
    // A tile for a frame already two sequences behind was (or will never be)
    // recorded; re-creating its entry would emit a duplicate paint record.
    if (frameSeq + 2 <= latestTileFrameSeq) return nowMs;
    if (!firstRecvMs.has(frameSeq)) {
      firstRecvMs.set(frameSeq, nowMs);
      // rAF (the only drain) stops in a hidden tab while datagrams keep
      // arriving; bound the map so a long background does not grow it, and
      // the scan below, without limit. Map iterates in insertion order.
      if (firstRecvMs.size > MAX_PENDING_FRAME_STAMPS) {
        const oldest = firstRecvMs.keys().next().value as number;
        firstRecvMs.delete(oldest);
        pendingFramePaintRaf.delete(oldest);
      }
    }
    if (frameSeq > latestTileFrameSeq) {
      latestTileFrameSeq = frameSeq;
      for (const seq of firstRecvMs.keys()) {
        if (seq + 2 <= latestTileFrameSeq) pendingFramePaintRaf.add(seq);
      }
    }
    return firstRecvMs.get(frameSeq)!;
  }

  function handleRecord(ev: HookRecord, atMs: number): void {
    // The five protocol-derived `window.__*` test globals
    // (docs/specs/wasm-cutover-main-ts-map.md Part 3) are re-derived from
    // every event here via `recordProtocolEvent`, in `cdf53_globals.ts` — the
    // same function a Node driver calls in
    // `tests/cdf53_globals.test.ts` against `pkg-node`, so the logic that
    // updates them is never duplicated.
    recordProtocolEvent(window as any, ev as any, cdf53ErrorCodes!, atMs);
    switch (ev.kind) {
      case 'TilePayload': {
        const d = ev.data;
        if (ev.error) {
          // The renderer refused it; the core has already acknowledged the tile.
          health.record(
            'renderer.apply',
            `tile (${ev.tile_x},${ev.tile_y}) seq ${ev.frame_seq}: ${ev.error}`,
          );
        }
        if (d.codec === 'Solid') {
          if (d.bytes[2] > 0x80) health.mark(ev.tile_x, ev.tile_y, ev.frame_seq, 'solid-red');
          if (diagnostics) {
            health.tally(`solid:${Array.from(d.bytes).map((b) => b.toString(16).padStart(2, '0')).join('')}`);
          }
        }

        const firstRecvMsClient = noteFrameTileReceived(ev.frame_seq, atMs);

        // Re-derive the `__ghostframeRecorded*` test globals from the event.
        // `palRleFlag` is the wire flags byte the server encoded, not the
        // palette id: `e2e_indices_raw_handshake` tests its bit 1.
        diag!.recordTile({
          seq: ev.frame_seq,
          tileX: ev.tile_x,
          tileY: ev.tile_y,
          codec: codecDiscriminant(d.codec),
          payloadLen: d.len,
          fbWidth: fbW,
          fbHeight: fbH,
          palRleFlag: d.codec === 'PalRle' ? d.wire_flags : undefined,
          firstRecvMsClient,
          lastPaintMsClient: atMs,
        });
        break;
      }

      case 'PaletteUpdated': {
        // Applied to the atlas inside wasm (before the tiles that use it, in
        // wire order). JS only keeps the e2e fingerprint log of the writes.
        if (ev.error) health.record('renderer.palette', `palette ${ev.palette_id}: ${ev.error}`);
        const bgra = new Uint8Array(ev.colors.length * 4);
        ev.colors.forEach((c, i) => bgra.set(c, i * 4));
        recordPaletteWrite(ev.palette_id, bgra);
        break;
      }

      case 'FrameDimensions': {
        if (!ev.applied) break;
        // `seq` is diagnostic-only (fifo-logged to
        // window.__ghostframeRecordedResizes, never asserted on); 0 is a placeholder.
        diag!.recordResize({ seq: 0, oldW: fbW, oldH: fbH, newW: ev.width, newH: ev.height, trigger: 'sentinel' });
        fbW = ev.width;
        fbH = ev.height;
        // The framebuffer texture was replaced: re-publish the raw handles
        // (`texture.width`/`.height` are read by the e2e suite).
        window.__ghostframeRenderer = api!.debugHandles();
        break;
      }

      case 'DecodeError':
        // Only the recordProtocolEvent call above.
        break;
    }
  }

  return {
    attach(a: DebugApi): void {
      api = a;
      // The three CDF53 `DecodeErrorCode` discriminants, read from the wasm
      // export rather than hardcoded 8/9/10 — a drifting discriminant should
      // break `recordProtocolEvent`'s classification loudly, not silently stop
      // matching. See docs/specs/wasm-cutover-main-ts-map.md Part 3.
      cdf53ErrorCodes = errorCodes() as unknown as Cdf53ErrorCodes;
      // Test/diagnostic hooks: swallowed-exception counts and renderer state.
      window.__ghostframeHealth = () => health.snapshot();
      window.__ghostframeRendererState = () => a.debugState();
      window.__ghostframeTile = (x: number, y: number) => a.debugTile(x, y);
      if (diagnostics) {
        // Time per painted tick, for the section 7 bench: [count, p50, p99, max] in
        // microseconds since the last call.
        window.__ghostframePaintUs = () => Array.from(a.paintUsStats());
        // Bench (?e2e only): main-thread CPU time of each rAF tick that presented a
        // frame, summarised by the client.
        window.__ghostframeTickUs = () => Array.from(a.tickUsStats());
        window.__ghostframeStatsSamples = 0;
        window.__ghostframeStatsFrames = 0;
      }
      // Wire all test/e2e diagnostic globals onto `window` via diagnostics.ts.
      // Pixel readbacks are for the e2e suite only (`?e2e` on the page URL).
      diag = initDiagnostics({ reader: a, readbacks: diagnostics });

      // Per-tile Cdf53 coverage, for e2e assertions.
      //
      // The interesting fields are `partial` (tiles still owed passes: the
      // stranding signal) and `gave_up` (tiles that exhausted
      // MAX_TAIL_SWEEP_ATTEMPTS on a known gap that was never repaired). A tile
      // whose final pass was lost has no gap evidence, so it never asks and shows
      // only in `partial`. A production-shaped scene that ends with either
      // non-zero has reproduced the stale-tile symptom; `incomplete` then names
      // the coordinates and the missing-pass mask.
      //
      // Exposed here rather than scraped from the log line so a test asserts on
      // structured state instead of parsing prose.
      window.__cdf53CoverageSummary = () => {
        const summary = a.cdf53Coverage();
        return {
          summary,
          incomplete: a.cdf53IncompleteTiles(16),
        };
      };

      statsLog = createStatsLogger({
        renderer: a,
        core: a,
        log: (m) => a.log(m),
        getNackSent: () => nackSent,
      });
    },

    events(records: HookRecord[], atMs: number): void {
      for (const r of records) {
        // One throwing event must not discard the rest of its batch.
        try {
          handleRecord(r, atMs);
        } catch (e) {
          health.record(`handleEvent:${r.kind}`, e);
          console.error('handleEvent failed; continuing', r.kind, e);
        }
      }
    },

    datagramCounts(tile: number, frame: number): void {
      // Assigned from the core rather than incremented on purpose: the core
      // classifies tile vs frame in `handle_datagram`'s own dispatch, and a
      // second copy of that discriminator in TypeScript is how the two drift.
      diag!.stats.tileDatagrams = tile;
      diag!.stats.frameDatagrams = frame;
    },

    rafTick(n: number): void {
      diag!.recordRafTick(n);
    },

    beforePaint(atMs: number): void {
      // M3.5 bench: emit recordFramePainted for all frames whose last tile was
      // received before this rAF tick. Uses the tick's entry time so all frames
      // pending in this tick share the same rafMsClient timestamp.
      if (pendingFramePaintRaf.size > 0) {
        for (const seq of pendingFramePaintRaf) {
          diag!.recordFramePainted({ seq, rafMsClient: atMs });
          // Cleanup per-frame bench state.
          firstRecvMs.delete(seq);
        }
        pendingFramePaintRaf.clear();
      }
    },

    afterPaint(info: { solid: number; palrle: number; cdf53: number; h264: number; rafTicks: number; nackSent: number }): void {
      nackSent = info.nackSent;
      // Drain accounting + the ~2 s stats line live in stats_log.ts.
      statsLog!.onTick(
        { solid: info.solid, palrle: info.palrle, cdf53: info.cdf53, h264: info.h264 },
        info.rafTicks,
      );
    },

    swallowed(site: string, message: string): void {
      health.record(site, message);
    },

    rendererError(tick: number, message: string): void {
      health.record('renderer', `tick ${tick}: ${message}`);
      health.tally(`gpu:${message.slice(0, 80)}`);
    },

    serverStream(samples: number, frames: number): void {
      if (diagnostics) {
        window.__ghostframeStatsSamples = samples;
        window.__ghostframeStatsFrames = frames;
      }
    },
  };
}
