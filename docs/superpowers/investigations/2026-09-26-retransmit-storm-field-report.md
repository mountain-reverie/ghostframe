# Retransmit storm in the field: the client never receives a full first frame

**Date:** 2026-09-26
**Reporter:** native-client work on `native-client-arm-gles-v4l2`
**Server:** `evangeline`, `ghostframe-xdaemon.service` (user unit), tile mode
**Client:** `ghostframe` native client, aarch64, GLES backend
**Related:** [`docs/specs/retransmit-storm-root-cause.md`](../../specs/retransmit-storm-root-cause.md)

**This is a server-side report. The client has been ruled out with
measurements — see §3, which exists so you do not re-derive it.**

---

## 1. Symptom

A freshly connected client renders roughly a quarter of the remote screen and
then stops filling in. On screen: part of one terminal window, the rest black —
no desktop background, no Enlightenment shelf. Whatever is still animating (a
blinking cursor) updates; nothing else ever arrives.

`retransmit-storm-root-cause.md` names this exactly: *"frames never fully
stabilise; per-tile stale content persists across many frames."*

## 2. What the server was doing

From `journalctl --user -u ghostframe-xdaemon.service`, three consecutive
`cumulative emit` lines ~7 s apart during a live session, on a daemon that had
been up about two hours:

| counter | t+0 | t+7s | t+13s |
| --- | --- | --- | --- |
| `cache_pending_entries` | 2697 | 4557 | **6417** |
| `rto_fired` | 281592 | 285432 | **289272** |
| `emitted_cdf53` | 309436 | 309436 | **309436** |
| `emitted_solid` | 136669 | 138169 | 139669 |
| `emitted_palrle` | 116872 | 117232 | 117592 |
| `nack_hit` / `nack_miss` | 264 / 26207 | ” | ” |

Read together:

- **`emitted_cdf53` is frozen.** Solid and palRLE keep flowing; the codec that
  carries the detailed content emits nothing at all. That is why the client
  receives a fragment: it gets the flat-colour tiles and none of the rest.
- **`cache_pending_entries` grows monotonically and never drains** — ~1860 per
  sample, i.e. the emitter is accumulating unacknowledged entries far faster
  than it retires them.
- **`rto_fired` climbs ~3840 per 7 s — about 550 retransmit timeouts per
  second.**
- **99% of NACKs miss** (264 hits against 26207 misses), and
  `emitter_ack_hits=236111` against `emitter_ack_misses=217345` — roughly half
  of all acknowledgements fail to translate back to the content they
  acknowledge. That is the mechanism `retransmit-storm-root-cause.md`
  describes: *"a late acknowledgement has nowhere to land."*
- `queued_critical_latency_mean_us=64159655` — **64 seconds** of queueing
  latency on the critical path.

## 3. The client is not the cause — and here is why you can skip re-checking it

The whole client path is instrumented (this branch added the counters). During
the same session:

| Stage | Measurement | Reading |
| --- | --- | --- |
| Tiles received | `tile_ready=1034`, `tile_ready_nonblack=1034`, `decode_error=0` | Everything that arrived was non-blank and decoded |
| Total volume | 1034 tiles = **0.51 screens** | Half a screen ever arrived |
| Framebuffer | `31744 → 61440 → 94208 → 586752 → 586752 → 586752` | **Monotonic; never loses content**, then plateaus |
| Exported dmabuf | `586752` — **identical to the framebuffer** | The buffer handed to the window holds exactly what was rendered |
| Present | 26–41 fps, `publish_starved=0`, releases 1:1, 0 watchdog fires | Never starved, never stalled |

The framebuffer plateaus at 28% of the screen because 28% is what arrived. It
never *drops* content, including across the ten `FrameDimensions` events the
server emits at session start, so nothing is being lost client-side either.

Reproduce these yourself on the client with:

```bash
GHOSTFRAME_CLIENT_DUMP_FRAME=/tmp/fb \
GHOSTFRAME_CLIENT_DUMP_EXPORT=/tmp/ex \
GHOSTFRAME_CLIENT_DUMP_EVERY=15 \
RUST_LOG=ghostframe=debug,info \
  ghostframe connect <host>
```

`non_black_px` on both dumps, plus the render thread's one-second summary
(`tile_ready`, `tile_ready_nonblack`, `decode_error`, `publishes`,
`publish_starved`, `releases`), is the whole picture.

## 4. Restarting the daemon clears it — and it comes back immediately

`systemctl --user restart ghostframe-xdaemon.service`, then reconnect:

- `emitted_cdf53` resumes.
- The client reaches **2073600 / 2073600 non-black pixels — the complete
  screen** — and holds it, across 46779 tiles (22.9 screens' worth).

**But the storm starts rebuilding at once.** Two `cumulative emit` lines a few
minutes after the restart:

```
frame_seq=3300  cdf53=33677  rto=22640  nack_hit=0 nack_miss=6764
                ack_hit=21977 ack_miss=33000  cache_pending=33001
frame_seq=3360  cdf53=34146  rto=23055  nack_hit=0 nack_miss=6764
                ack_hit=21977 ack_miss=33600  cache_pending=33601
```

- **`nack_hit=0` against `nack_miss=6764` — a 100% NACK miss rate from a cold
  start.**
- `cache_pending_entries` is already 33601 and climbing.
- `emitter_ack_misses` (33600) has overtaken `emitter_ack_hits` (21977).

So a restart is a workaround that buys a working session, not a fix. The defect
reproduces from cold within minutes, which should make it cheap to iterate on.

## 5. Where to start

`docs/specs/retransmit-storm-root-cause.md` already has the mechanism: since
"acknowledge transmissions, not content" (PR #90) an `AckEntry` carries a
`wire_seq`, and the server must translate it back to an `EmitKey` through a
ledger. When that translation fails the acknowledgement is dropped and the
entry stays pending forever.

The field counters say the translation is failing most of the time
(`nack_hit=0`, `emitter_ack_misses > emitter_ack_hits`), which is a much worse
rate than the 1788 retransmissions on a lossless link that document was written
against. Worth checking whether that is the same defect at a larger scale or a
regression on top of it — the later scheduler-repair and FEC work
(`docs/superpowers/plans/2026-09-19-*`) landed after that document was written.

Two specific questions the counters raise:

1. **Why does `emitted_cdf53` freeze completely while solid and palRLE keep
   flowing?** A storm alone would slow everything; one codec stopping dead
   suggests its entries are the ones stuck pending, not merely delayed.
2. **Why is `nack_hit` exactly 0 from a cold start?** Not "low" — zero. Every
   single NACK fails to find its entry. If NACK lookup and ACK lookup share the
   ledger, a 100% miss on one and ~60% on the other is a strong hint about
   where the key is going wrong.

## 6. Signals that would show it fixed

- `cache_pending_entries` **bounded** across a long session rather than
  monotonically climbing.
- `nack_hit` materially non-zero.
- `emitter_ack_hits` well above `emitter_ack_misses`.
- `emitted_cdf53` advancing continuously, never flat while other codecs move.
- Client-side: `non_black_px` reaching `total_px` on first paint and staying
  there, without a daemon restart.

## 7. Unrelated observations from the same logs

Recorded so they are not mistaken for part of this, or lost:

- **`ConnectionEvent for unknown connection handle=ConnectionHandle(13)`**,
  208 times in one burst, and 205 for handle 14. Stale connection state after a
  client goes away; the eviction path logs
  `evicting session handle=ConnectionHandle(13) reason=DisplacedByNewSession
  notices_sent=0` — note `notices_sent=0`, so the displaced client is never
  actually told.
- **`ghostframe-wm.service` is inactive** while `ghostframe-xdaemon.service`
  runs. Something is drawing the session, but not that unit. `ghostframe.target`
  is also inactive.
- **The server emits ten `FrameDimensions` events at session start**, each of
  which makes the client rebuild its export ring (three dmabuf allocations
  apiece, so ~30 allocations per connect). Harmless but wasteful, and it makes
  the first second of any session hard to read.
