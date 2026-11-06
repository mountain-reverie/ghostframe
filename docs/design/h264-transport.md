# H.264 transport

**Status:** describes the H.264 transport after Phase A (wire v2, ledger
integration, rate/GOP, stall rules) and the server side of Phase B
(acked-reference recovery on the libva-direct backend, section 10). Sections 3
and 4 describe the Phase A policy, which legacy sessions still run.

This is the note to read before touching H.264 code. It names code by file and
function or constant, not by line. When a constant here disagrees with the
code, the code is right and this note is stale.

## 1. Overview

H.264 is one of two frame modes. The other is the tile codec (Solid, PalRLE,
CDF 5/3; see `high-level-design.md`). The classifier (`tile/classifier.rs`,
`Classifier::decide_core`) picks the mode per frame. Reasons, in priority
order:

| Reason string | Meaning |
|---|---|
| `test_force` | `GHOSTFRAME_FORCE_FRAME_MODE` pins the mode; beats everything, `h264_unsupported` included |
| `h264_unsupported` | HELLO bit 0x02 clear (Firefox WebGPU): tile codec only, beats everything below |
| `h264_stalled`, `h264_stalled_holdoff` | transport stall (section 6): leave H.264 and stay out |
| `headroom_guard` | bandwidth estimate too low for the tile codec: force H.264 |
| `loss_override` | loss rate above 10 %: force H.264. **Off by default**; `GHOSTFRAME_LOSS_OVERRIDE=1` enables it, `GHOSTFRAME_LOSS_OVERRIDE_THRESHOLD` moves the line. One lost fragment kills a whole H.264 frame, so "H.264 rides through loss" no longer holds |
| `suspension` | WiFi suspension detected: force H.264 |
| `cost_comparison` | per-frame cost model with enter/exit hysteresis |
| `hysteresis_clamp` | the cost model wants to switch but the dwell has not elapsed: keep the previous mode |

The frame mode only switches the codec. Both modes share one transmission
ledger, one admission bucket and one bandwidth estimator.

Encoder backends (`encoder/h264_vaapi.rs`, `FullFrameEncoder`):

- `h264_vaapi` through ffmpeg, preferred. Opened rate-capped VBR: `bit_rate`
  equals `rc_max_rate`, VBV buffer is one second of the rate, `rc_mode=VBR`.
- `libx264` software fallback (`ultrafast`, `zerolatency`, same VBV, no
  `nal-hrd=cbr`).

VBR and not CBR because Mesa radeonsi CBR stuffs every frame up to
`bps / fps`: static P frames came out at about 8.3 kB against an 8.3 kB budget,
all filler, and the estimator then saw a saturated sender. With VBR the same
static P frame is about 126 bytes. The price is that VBR undershoots busy
content (section 9). Several comments and the `RateParams` doc in the encoder
and `h264_rate.rs` still say "CBR"; they predate that switch.

Neither backend has reference control. Recovery from loss is an IDR. The
encoder's own GOP is set to about 9 hours (`ENCODER_OWN_GOP`); the bridge asks
for every IDR itself.

## 2. Wire v2 (`ghostframe-protocol`)

### FrameHeader, 22 bytes (`protocol.rs`)

All big-endian.

| Offset | Field | Notes |
|---|---|---|
| 0..4 | `frame_seq` u32 | bit 31 always 0 (bit 31 set means a tile datagram) |
| 4..6 | `frag_idx` u16 | `frag_idx >= frag_total` marks a parity datagram |
| 6..8 | `frag_total` u16 | client refuses above `MAX_FRAME_FRAGMENTS` |
| 8..12 | `wire_seq` u32 | ledger identity; `NO_ACK` (0xFFFFFFFF) is untracked |
| 12..16 | `timestamp_us` u32 | |
| 16 | `flags` u8 | below |
| 17 | `slots` u8 | bits 0-1 `mark_lt_idx`, bits 2-3 `target_lt_idx` |
| 18..22 | `ref_frame_seq` u32 | frame a P frame predicts from; `REF_FRAME_NONE` for an IDR or unknown |

Flags:

| Bit | Name | Status today |
|---|---|---|
| 0x01 | `FRAME_FLAG_IDR` | in use |
| 0x02 | `FRAME_FLAG_NON_REF` | set by acked-reference sessions (a recovery P) |
| 0x04 | `FRAME_FLAG_MARK_LT` | set by acked-reference sessions (anchor candidates; `slots` bits 0-1) |
| 0x08 | `FRAME_FLAG_TARGET_IS_LT` | set by acked-reference sessions (`slots` bits 2-3 name the slot) |
| 0x10 | `FRAME_FLAG_PAD` | probe padding (section 5) |

The ffmpeg encoder predicts from the previous frame, so a P frame carries
`ref_frame_seq` = the previous H.264 frame the bridge sent (`h264_ref_info` in
`io_bridge/frame.rs`) and `slots` = 0. Parity datagrams carry no reference
information (flags hold only the IDR bit, `ref_frame_seq` is `REF_FRAME_NONE`);
read the reference from a source fragment, never from a parity.

### Fragmentation and parity

`fragment_frame_ref` cuts the access unit into fragments that fit
`max_fec_frame_fragment_payload(max_dg_size)` (room is left for the parity
header). Every source fragment is a ledger transmission with a real
`wire_seq`. H.264 XOR parity is always on (`ghostframe-protocol/src/fec.rs`:
group size 2 for an IDR, 5 for a P frame) and is billed `NO_ACK` as
`WireClass::Parity`: the client never acknowledges it, the ledger never judges
it. This is separate from tile parity (`GHOSTFRAME_TILE_FEC`, off by default).
The client rebuilds one missing fragment per group (`recover_group` in
`ghostframe-client-core/src/frame_assembly.rs`).

### Frame ack, datagram 0x0A (client to server)

`FrameAck`: `[0x0A][newest_seq u32 LE][decoded_mask u64 LE][refused_mask u64 LE]`,
21 bytes, cumulative over the last 64 frames: bit `i` is frame `newest_seq - i`.
`newest_seq` is the newest frame the client *handled*, decoded or refused, so a
run of refusals advances the window instead of falling off it. Both clients
send it (native and web, HELLO 0x04). It covers every handled frame, but is
only ever queued by `ClientCore::on_timeout`, right behind the 0x06 ack batch
in the same drain, so both share a QUIC packet; at most one per 4 ms, on the
client core's own clock. It has no deadline of its own: a frame ack that no
batch has carried after 100 ms goes out alone (a decoder that answers after
its own frame's batch rides the next frame's; only a quiet screen waits that
long). A frame ack sent at frame completion instead would be an extra
ack-eliciting packet that carries quinn's pending ACK frame to the server
early, which changes the server's bandwidth loop (79 of 96 capacity-step seeds
reached 0.8 x the new rate against 92 without). `FrameAckWindow` is in
`ghostframe-client-core/src/h264_gate.rs`. The server counts every one
(`dispatch_frame_ack_datagram`; `frame_acks_received` and
`frame_acks_malformed` in the `h264 client feedback` line, and in the
browserless `H264Report`) and, on an acked-reference session, feeds it to the
`RefManager` (section 10).

### Reset request, feedback stream 0x09 (client to server)

`[0x09][reason u8][frame_seq u32 LE]`, 6 bytes, on the reliable feedback
stream (`feedback_stream.rs`, `ResetRequest`).

| Reason | Value | Sent by |
|---|---|---|
| `DecoderFailure` | 1 | the decoder failed (`H264Outcome::DecodeError`: web WebCodecs error callback; native decode error), through the gate, paced 2.2 s |
| `Resolution` | 2 | defined; nothing sends it yet (the gate has the cause, an IDR answers it) |
| `NoDecoder` | 3 | no decoder could be opened (`H264Outcome::NoDecoder`), through the gate, paced |
| `GateReset` | 4 | the reference gate refused a P frame whose reference the client does not hold (section 7), paced 2.2 s |

**Phase A sessions act on `DecoderFailure` only** (`IoBridge::handle_reset_request`):
at most one per `RESET_REQUEST_MIN_INTERVAL` (2 s), then
`request_h264_idr(IdrCause::ClientReset)`. The other three reasons are counted
and logged at debug; the client recovers from the next loss IDR or scheduled
GOP IDR. **Acked-reference sessions act on `DecoderFailure`, `GateReset` and
`Resolution`**, with the same limit: a new plan epoch, which begins with an
IDR. **`NoDecoder` on any session** is not answered with an IDR, which cannot
help a client with nothing to decode it: H.264 is marked unavailable for the
rest of the session (`supports_h264` cleared, so the classifier falls back to
the tile codec) until the next HELLO.

### HELLO capability bits (`feedback_stream.rs`, `client_caps.rs`)

`[0x03][caps]`.

| Bit | Name | Meaning |
|---|---|---|
| 0x01 | `HELLO_CAP_INDICES_RAW` | PalRLE `indices_raw` payloads |
| 0x02 | `HELLO_CAP_SUPPORTS_H264` | client can decode and sample H.264. **This alone gates H.264.** |
| 0x04 | `HELLO_CAP_H264_ACKED_REF` | client sends v2 feedback (0x09 resets and 0x0A frame acks). Does not gate H.264 |
| 0xF8 | reserved | server ignores |

Both clients set 0x04 with 0x02: the native client whenever it supports H.264,
the web client always (`ghostframe-client-wasm` passes `h264_acked_ref: true`
and the browser runs the same gate through wasm). Chrome stays on H.264. The
server records bit 0x04 (`HELLO received` log line); a session with it, on a
backend that executes plans, recovers by acked references (section 10).
Every client reads the 22-byte header regardless.

## 3. Server transport

Code: `transport/io_bridge/h264_tx.rs` (tracker, hold, IDR policy, stall flag),
`io_bridge/frame.rs` (`send_h264_frame`, `finish_h264_arm`),
`transport/reliable_emitter/h264.rs` (probe cache, pad ring).

**Fragments in the ledger.** Each source fragment is submitted with
`ReliableTileEmitter::submit_h264_fragment`, which records a
`Content::H264Frag { frame_seq, frag_idx }` transmission, charges the admission
bucket and feeds the estimators. Every recorded transmission ends as exactly
one of: acknowledged live, judged lost, or forgotten. `tx_forgotten` counts
both fragments purged before departure (`forget_purged`) and fragments whose
content was gone when the probe timeout looked (`poll_pto` in
`io_bridge/bwe_glue.rs`). The `h264 stats` line gives `tx_recorded >=
tx_acked_live + tx_judged_lost + tx_forgotten`; the gap is still outstanding
(not logged). The identity is exact only within one session and without ledger
capacity evictions: the counters are cumulative, the ledger is wiped on a
session reset. Acknowledged H.264 fragments are BWE samples.

**Frame tracker.** `FrameTracker` is a ring of `TRACKER_RING` (256) frames with
one bit per fragment; it tells when a frame is fully acknowledged.

**Probe cache.** `H264Cache` keeps the fragments of frames not yet fully
acknowledged (at most `H264_CACHE_FRAMES`, 64 frames). The loss path asks it
whether a fragment is still current. Without an entry the fragment's content is
"gone" and would be declared lost on the spot, which under an ack stall would
hand goog_cc a false mass loss. Entries die on acknowledgement, on supersession
by an IDR, on trimming, or at session end.

**What is resent.** A loss verdict on an **IDR** fragment resends it
(`RetxCause::Loss`, counter `idr_fragments_resent`). The probe timeout resends
any fragment to draw an acknowledgement (`probe_fragments_resent`). A verdict
on a **P** fragment resends nothing; it starts the loss IDR below.

**P hold.** After an IDR of any cause except `GopPeriodic`, no P frame is
encoded until every fragment of that IDR is acknowledged
(`h264_frame_gate` returns `SkipHold`). The client evicts assemblies older than
`latest - 2`, so P frames behind an unfinished IDR would push it out. The hold
is a no-progress timeout: each acknowledged fragment restarts it, and after
`IDR_HOLD_MAX` (3 s) with none it ends and a `HoldTimeout` IDR is requested. A
scheduled GOP IDR holds nothing (the client already has a picture).

**Interim loss IDR.** A verdict on an unacknowledged P-frame fragment marks the
frame suspect, purges the queued fragments of that frame and every later one
(they chain from a picture the client lacks), drops their cache entries and
requests an IDR with cause `Loss`. At most one per `LOSS_IDR_MIN_INTERVAL`
(1 s); a request turned away is remembered (`deferred_loss_idr`) and made when
the interval passes, with P frames skipped meanwhile. Verdicts on frames an
IDR already supersedes or has pending ask for nothing. This path is deleted by
Phase B.

**IDR causes** (`IdrCause`): `session`, `loss`, `client_reset`, `resolution`,
`reentry` (tile to H.264; a still-pending loss request is credited to it),
`hold_timeout`, `rate_drop`, `rate_rise`, `gop_periodic`, `mtu_shrink`. Requests
coalesce: while one is pending the cause stays the first one's.

**MTU shrink.** Fragments are cut for the datagram size quinn reports when the
frame is made, and are stored (emission queue, loss-path cache) at that size.
quinn's MTU discovery can raise the size to about 1412 and its black-hole
detector can put it back to the 1200-byte floor (about 1160 of datagram), so a
stored fragment may no longer fit. Offering it fails `TooLarge`, and a refusal
re-queues the datagram at the head of the emission queue, so the drain stops at
it for good and the IDR is never delivered. The bridge's drain therefore checks
every datagram against the size quinn takes now and retires one that does not
fit: it is not offered, the ledger forgets it with no verdict, and its owner is
told. For an H.264 fragment that is an IDR cut for the new size under cause
`mtu_shrink`, which supersedes every older frame and replaces the hold on the
IDR that cannot complete (the cause is a transport event, like a session start,
not a decoder or loss event). On an acked-reference ("refs") session the
manager is told instead and no IDR is requested for a P frame: the P frame
becomes suspect, as if the ledger had judged it lost, and the manager recovers
from the anchor with frames cut at the new size. Only an incomplete in-flight
IDR is abandoned and sent again, cut at the new size, under `mtu_shrink` (the
re-send does not back the IDR timeout off: a retirement is not a slow link,
though it still counts in `idr_reissued`). A tile pass is dropped from the
retransmit cache and the next capture is made whole; probe padding and parity
are dropped. Counters: `oversize_h264`, `oversize_tile`, `oversize_pad`,
`oversize_untracked`; `send_too_large` counts quinn refusals and must stay 0.
On a refs session `mtu_retired` counts frames (the first oversize fragment of
each; its siblings are purged with it), `mtu_retired_idr` those of them that
were the IDR in flight, and the manager's `idr_mtu_shrink` the IDRs actually
sent again for it (counted when sent, not when abandoned).

**Frame skip.** The encoder is not run (the capture is dropped, the next frame
is made fully dirty) while an IDR hold or deferred loss IDR is in force, or
while the emission queue holds more H.264 bytes than
`max(one 33.3 ms frame period of the admission rate, two gate depths, one
datagram)` (`h264_backlog_exceeded`, `SkipBackpressure`).

**H.264 to tile.** On the mode switch `abandon_h264_inflight` makes the frames
sent so far no longer count as in flight, purges what is queued for them,
drops their cache entries, ends any hold, forgets padding and the probe
cadence, restarts the rate policy's estimate and clears the GOP clock.
Retransmissions already with quinn are left alone. The return to H.264 is an
`IdrCause::Reentry` IDR.

## 4. Rate and GOP (`io_bridge/h264_rate.rs`, `RatePolicy`)

The target is `clamp(0.8 x E, 300 kbit/s, 50 Mbit/s)` where `E` is
`bwe.snapshot().bitrate_bps` smoothed by a 250 ms EWMA. It is not the model
controller's probe-inclusive rate. The initial rate before any estimate is
4 Mbit/s. The 0.8 share (`RATE_SHARE`) is asserted at compile time to stay
below the BWE ceiling's `SATURATION`, or the ceiling would judge the sender
saturated and shrink the estimate it follows.

The encoder cannot change rate in place, so a new rate is a reopen, and a
reopen starts with an IDR. The policy is therefore a decision about when to pay
for an IDR:

- A candidate is adopted if it moved more than 10 % from the last one, or every
  1 s; evaluated at most every 250 ms. A running rate within 10 % of the
  candidate is left alone.
- A raw estimate more than 1.5 x the smoothed one is a spike and is held for
  1 s (2 s, and releasable, on acked-reference sessions: section 10), so the target can follow a probe that goog_cc pulls back faster than the
  EWMA rises.
- **Drop:** a candidate more than 30 % under the running rate reopens at once
  (`IdrCause::RateDrop`, with a P hold; the IDR is cheaper at the lower rate).
  It also starts a quiet period (3 s) with no padding.
- **Rise:** a candidate more than 30 % over reopens at once
  (`IdrCause::RateRise`; like every IDR except the scheduled GOP one it opens a
  P hold and is not padded) only if the raw estimate was over the line on
  2 consecutive evaluations, so a blip does not cause a rise followed by a drop.
- **Anything else** (shallower fall or rise past 10 %) waits for the next
  scheduled GOP IDR and takes the new rate there (counted as
  `rate_reopens_gop`).
- At most one reopen per 2 s (`MIN_REOPEN_INTERVAL`), urgent ones included.

**GOP.** Time-based and owned by the bridge: the frame is the scheduled IDR
when `GHOSTFRAME_H264_GOP_SECONDS` (default 3, clamped to 2..5; unparseable is
ignored with a warning) has passed since the last IDR **of any cause**. A loss,
reset or rate IDR restarts the clock. The encoder is opened at the real capture
rate (`CAPTURE_FPS`, default 30), because its rate control divides the rate by
fps to get a frame budget.

## 5. Probing

goog_cc's periodic probing is driven by its ALR (application-limited) detector.
The fork's detector (`AlrDetectorConfig::default()`: `alr_bandwidth_usage_percent`
80, `alr_start_budget_level_percent` 40, stop at -60) budgets 80 % of the
estimate, lets unused budget build up over a 500 ms window, and starts an
application-limited region when the budget is more than 40 % full. A stream
held at 0.8 x E refills that budget exactly as fast as it spends it, and every
IDR burst overdraws it, so it is not reliably seen as application-limited
(a stream that undershoots, as VBR does on busy content, can still trip it).
The design relies on goog_cc not probing for it after the start-up ladder; the
app probes below run regardless. goog_cc is also never given a `NetworkStateEstimate`
(`bwe/googcc.rs`, `GoogCcDriver::new`), so its network-state probes are off in
every mode. Capacity step-ups in H.264 mode are therefore found by probes of
ghostframe's own and by the IDR burst.

**App probes** (`h264_start_app_probe`). A probe cluster at 2 x E, 100 ms long
(at least 5 packets), every `APP_PROBE_INTERVAL` (5 s), where E here is the raw
`bwe.snapshot().bitrate_bps`, not the rate policy's EWMA. The chain continues
while that raw estimate is at least 0.7 x the previous probe's target rate: the
next probe follows after 400 ms, up to 4 in a chain. Cluster ids start at 1,000,000,000, a range goog_cc never issues;
goog_cc measures any cluster id it finds on acknowledged packets. A cluster
that padding could not fill (needs a window under 15 ms at the cap) is skipped
and counted (`app_probes_skipped`).

**Padding** (`h264_probe_pad`). The cluster is filled with copies of an
already acknowledged P-frame fragment (at least 600 bytes) under new
`wire_seq`s, flagged `FRAME_FLAG_PAD`. The client acknowledges them on receipt
(that is the measurement) and drops them without touching any assembly
(section 7). They are never resent. Limits: 1.2 x the cluster's `min_bytes`;
at most 384 KiB per window and 0.3 s of the estimate; stop when the pad ring is
full. A window goog_cc itself asked for (start-up ladder, repeated initial probes,
ALR) is claimed first, by `open_probe_window_for_burst`; the app probe starts
only if none was pending. Padding is not sent when:

- the emission queue holds anything beyond the frame just queued;
- within 3 s of a rate cut;
- an IDR is pending or due within 150 ms (the IDR is the burst);
- the IDR is a recovery IDR (every cause but `GopPeriodic`): its P hold waits
  for its fragments, and padding behind them would delay the hold.

The empty-queue and quiet-period guards apply only when a window is opened and
to an IDR's top-up; a P frame continuing an already open window is exempt (its
own earlier padding is in the queue).

A scheduled GOP IDR claims a pending window and is topped up with padding.
After padding, the BWE ceiling is held off for 1 s (`PAD_CEILING_HOLD`) so a
burst that spends the whole grant is not read as a saturated, silent path.

**What the ceiling reads as saturated.** A CBR stream at 0.8 x its estimate is
not rate-limited, and its ceiling readings say so only if the signal measures
the path. Two things made it measure the sender instead. A pass the model
window stopped forfeits its bank, and counting that as spend made 95 % of the
readings saturated whenever the acknowledgements were late (the window is the
acknowledgement clock, not the link): it counts now only with a queue standing
(quinn's round trip over 1.25 x its ten-second minimum and at least 5 ms over it), which puts the share
at about 0.4 and makes the result independent of when the client acknowledges.
And a window holding a stall in its acknowledgements (longer than the round
trip and than a quarter of the window, so that a path faster than the client's
5 ms flush and a frame interval is not mistaken for one: a frame the pacer had
not released, a run of skipped frames) read as a path
delivering a tenth of its rate; the sender, paced to that bound and spending
all of it, read its own rate back as the path's and sat there until the next
application probe, 0.13 to 0.2 x its pre-step estimate on a lossless link. A
saturated reading across such a stall is now set aside with no queue standing
(the last bound read from a window without a stall stays, goog_cc is not
moved). With a queue standing the low rate is the path's answer and is
published, as before. After a base round trip rise (a route change), and under
jitter that lifts quinn's round trip (WiFi), the queue test stays true for up
to ten seconds and both rules behave as they did before: the fix gives those
paths nothing. The cost: when the base round trip steps up with no queue yet,
the cut that the stalled window used to deliver waits for the first
acknowledgement (`p8e`: a short second dip after the restore, up to 200 ms on
two seeds of fourteen, one in fourteen before).

Where the stalls come from: an IDR is followed by its parity (K = 2, so 38
datagrams for a 77-fragment IDR, 54 kB), queued behind the fragments and
unacknowledged by design. Paced at the estimate that is a quarter second or
more with no acknowledgement at all, and the P frame queued behind it waits as
long. Seeds 0x6720 and 0x6724 showed 0.7 s of it (38 parity datagrams of one
IDR, none acked, the next frame's first fragment behind them) while the
client's acknowledgement path was healthy.

**PadRing** (`reliable_emitter/h264.rs`): up to `PAD_RING_CAP` (512) entries,
each forgotten after `PAD_ENTRY_TTL` (2 s). An entry exists only to carry the
probe tag to the acknowledgement and to count the pad's bytes as in flight.

**Ledger patience** (`transmission_ledger.rs`, `BURST_REORDER_FACTOR`). A
datagram of the H.264 stream (pad or fragment, never a tile) that departed
inside a marked probe burst is judged lost by the time rule only after 3 x the
usual threshold, because a burst queues behind itself. The packet-count rule
is unchanged. The burst window ends `PROBE_BURST_DRAIN` (300 ms) after the
probe window closes.

## 6. Stall rules and the headroom guard

`update_h264_stall` (h264_tx.rs) evaluates `h264_stalled` once per frame
decision and publishes it to the classifier. Frames before the last IDR (and before the frame at which the session last
left H.264, `ignore_before`), and frames a verdict doomed, are not counted
(`FrameTracker::unresolved` skips `frame_seq` below that floor).
Raised by any of:

- **(a) in-flight bound:** unresolved frames exceed
  `max(32, ceil(2 x srtt / 33.3 ms) + 8)` (`stall_inflight_bound`);
- **(b) no progress:** frames in flight and no H.264 fragment acknowledged for
  500 ms (`STALL_NO_PROGRESS`; any acknowledged fragment is progress, since a
  large IDR on a slow link drains a fragment at a time);
- **(c) reset storm:** 3 honoured `DecoderFailure` resets within 10 s raise
  the flag for 500 ms.

The classifier then returns `TileCodec` with `h264_stalled`. It beats every
other override, including the ones that force H.264. Re-entry is refused for a
holdoff (`h264_stalled_holdoff`). The stall time is refreshed on every stalled
evaluation, so the holdoff runs from when the stall clears: 2 s after the first stall, doubling with each
stall that ends a stretch of H.264 (2, 4, 8, 16, 30 s cap). H.264 that then
runs 10 s without a stall resets the holdoff to 2 s.

**headroom_guard** reads the bandwidth estimate (not the probe-inclusive rate):
engage below 2 Mbit/s held 500 ms, release above 3 Mbit/s held 1 s, at least 3
consecutive evaluations either way; between the thresholds it keeps its state.
An idle screen for 2 s releases an engaged guard whose estimate is back above
the enter line (tiny P frames cannot raise the estimate). The guard's timers
keep running through a stall and its holdoff. `GHOSTFRAME_HEADROOM_MIN_BPUS`
moves the enter threshold (bytes per microsecond; 0.25 is 2 Mbit/s), the exit
keeps the 3:2 ratio.

## 7. Client behaviour

`ghostframe-client-core/src/frame_assembly.rs` (shared by the native and wasm
clients):

- **Acknowledge on receipt.** A source fragment with a real `wire_seq` is
  acknowledged (0x06 batch) before anything else is decided, whatever then
  becomes of it. Malformed fragments are dropped first and not acknowledged.
- **Pad drop.** A `FRAME_FLAG_PAD` fragment is acknowledged and then dropped.
- **Stale drops.** A P fragment at or before the newest delivered frame is
  dropped. An IDR that completes at or before the newest delivered frame is
  dropped.
- **IDR assembly exemption.** Assemblies older than `latest - 2` are evicted,
  except the newest IDR assembly for `IDR_EXEMPT_US` (2 s), since the server
  resends IDR fragments and holds P frames, so an IDR can complete after newer
  `frame_seq`s were seen. Only one is exempt, so a hostile stream of IDR
  fragments cannot park memory.
- **Reference gate** (`ghostframe-client-core/src/h264_gate.rs`, pure; design
  7.1). Every completed access unit passes `H264Gate::on_frame`, which feeds it
  iff its `frame_seq` is newer than the last frame fed and it is an IDR or
  names a picture the client really holds: a long-term target
  (`FRAME_FLAG_TARGET_IS_LT`) must match the content of the named slot, a
  short-term target must be a good frame and still the newest reference. A
  refused frame never reaches a decoder; it is reported in the frame ack's
  refused mask. An IDR resets the gate (`good={idr}`, `lt[0]=idr`) and tells
  the decoder to reset (`Event::NeedsH264.reset_decoder`); the gate is the only
  thing that does. A Phase A stream sets no flag and names the previous frame
  sent, which is a short-term target on the newest reference, so the same rule
  gives the old behaviour: a P whose reference never arrived, and everything
  chained from it, is held back until an IDR, and `GateReset` (0x09) goes out
  at once, paced 2.2 s (the server's own limit is 2 s). A P frame naming no
  reference (`REF_FRAME_NONE`) is fed unless the gate is already waiting for an
  IDR. A stream that has shown a non-ref, mark or long-term-target flag is an
  acked-reference stream: its server repairs from an anchor on the refused bit,
  so the gate only asks for an IDR after 2.2 s of refusing with nothing fed.
- **Decoder results.** The consumer reports each fed frame once through
  `ClientCore::h264_result`: `Decoded` (same-call output with the frame's pts;
  native `decode_pts` / `HwFrame::pts`, web the `VideoDecoder` output callback
  for the chunk's timestamp), `NotOutput` (consumed with no picture, or in web
  overtaken by a later output), `DecodeError` or `NoDecoder`. A frame is usable
  as a reference from the moment it is fed (a WebCodecs decoder answers
  later). `NotOutput` is only a refused bit: a decoder with one frame of
  output delay answers every frame so and still holds them all, so nothing is
  evicted (the core counts the run, `h264_not_output_stats`, and the native
  renderer warns when older pictures come out instead). `DecodeError` and
  `NoDecoder` reset the gate and send `DecoderFailure` / `NoDecoder` (0x09).
  All reset requests share one 2.2 s pacing slot, and `NoDecoder` is sent once.
  After an error every result is refused until a frame fed with
  `reset_decoder` (the native renderer also stops decoding until then). The native
  render thread hands the results to the net thread (`NetCommand::H264Result`);
  the web client calls wasm `h264Result` and drains the transmit queue.
- **Web decoder.** `FullFrameDecoder` drops deltas after an async decode error
  until the next key chunk, answering each as `NotOutput`, and rebuilds the
  decoder at it. An IDR to a healthy decoder is just another key chunk.
- Malformed input, oversize access units (`MAX_H264_ACCESS_UNIT_BYTES`) and
  large `frag_total` are refused without panics.

## 8. Observability

The `h264 stats` line (target `ghostframe::h264`, INFO, cumulative, same
cadence as `cumulative emit`; `io_bridge/frame.rs`). Fields:

| Group | Fields |
|---|---|
| frames | `frames_sent`, `frames_idr`, `frames_acked`, `frames_suspect` |
| IDRs by cause | `idr_session`, `idr_loss`, `idr_client_reset`, `idr_resolution`, `idr_reentry`, `idr_hold_timeout`, `idr_gop_periodic`, `idr_rate_drop`, `idr_rate_rise`, `idr_mtu_shrink`, `send_too_large`, `oversize_h264`, `oversize_tile`, `oversize_pad`, `oversize_untracked`, `loss_idr_rate_limited`, `idr_requests_coalesced` |
| rate and probing | `rate_reopens_gop`, `rate_target_bps`, `app_probes`, `probe_padding_windows`, `probe_padding_bytes`, `frames_skipped_pad` |
| verdicts | `verdicts_idr_frag`, `verdicts_p_frag`, `verdicts_already_acked` |
| resends | `idr_fragments_resent`, `probe_fragments_resent` |
| hold and skips | `p_hold_ms`, `p_holds`, `hold_timeouts`, `frames_skipped_hold`, `frames_skipped_backpressure`, `queue_purged`, `queued_bytes`, `cache_fragments` |
| stall | `h264_stalled_ms`, `stalls_inflight`, `stalls_no_progress`, `stalls_reset` |
| ledger | `tx_recorded`, `tx_acked_live`, `tx_judged_lost`, `tx_forgotten`, `tx_acked_late`, `bwe_samples_seen` |

`H264Stats` also holds `app_probes_skipped`, `p_verdicts_in_probe`, `verdicts_superseded`,
`verdicts_unknown_frame`, `probe_padding_datagrams`, `idr_requests_by_cause`,
`rate_reopens_drop`, `rate_reopens_rise`, which the log line omits
(the tracing macro is at its field limit); the browserless harness reads them
through `H264Report`. A second line, `h264 client feedback` (target
`ghostframe_lib::transport::io_bridge`, not `ghostframe::h264`), carries
`frame_acks_received`, `frame_acks_malformed`, `reset_requests_received`,
`reset_requests_honoured`, `reset_requests_rate_limited`. Rate reopens and
stalls also log one event each (`H.264 rate: reopening the encoder`,
`h264 stalled: the classifier is told to leave H.264`). Reading it:
`frames_idr` against `frames_sent` is the IDR tax; `idr_loss` high means the
loss path is firing on P fragments; `tx_recorded` is at least the sum of
its three outcomes; the gap is the outstanding count (not logged), exact only
within one session and without ledger capacity evictions.

Environment variables (the classifier ones also accept their older
`GHOSTFRAME_TEST_*` names, e.g. `GHOSTFRAME_TEST_HEADROOM_MIN_BPUS`):

| Variable | Effect | Default |
|---|---|---|
| `GHOSTFRAME_H264_GOP_SECONDS` | scheduled IDR interval | 3, clamped 2..5 |
| `CAPTURE_FPS` | encoder frame rate | 30 |
| `GHOSTFRAME_LOSS_OVERRIDE` | enable `loss_override` (`1`, `true`, `yes`, `on`) | off |
| `GHOSTFRAME_LOSS_OVERRIDE_THRESHOLD` | loss rate that triggers it | 0.10 |
| `GHOSTFRAME_HEADROOM_MIN_BPUS` | headroom guard enter threshold | 0.25 (2 Mbit/s) |
| `GHOSTFRAME_FORCE_FRAME_MODE` | pin the frame mode (tests) | unset |
| `GHOSTFRAME_COST_H264_FRAME_US`, `GHOSTFRAME_COST_H264_FRAME_BYTES` | classifier cost model | compiled in |
| `GHOSTFRAME_RTO_PROBE=1` | one line per loss-path retransmission and verdict | off |

## 9. Known limitations

- **VBR undershoots busy content.** Measured on the moving-content encoder test
  at 625 kB per 5 s target, VBR produced 162 kB where CBR produced 627 kB. The
  stream is cheaper and blurrier than the estimate allows. Mesa-specific; the
  trade is the stuffing in section 1.
- **Shared loss path, spurious loss IDRs.** H.264 fragments are judged by the
  same loss path as tiles. At about 2 ms of path jitter it produces roughly
  30 to 40 spurious loss IDRs in a run (observed in the Phase A live checks,
  not asserted by a test). Each costs an IDR and a P hold.
- **Reordered P frames cost a reset.** A P frame that completes before its
  reference is refused by the client's reference gate, and the server does not
  yet act on `GateReset` or on the refused bit, so the picture waits for the
  next IDR.
- **Padding overhead,** roughly 5 to 8 % of the stream's bytes in the Phase A
  measurements.
- **IDR rate at low bitrate.** Near the 300 kbit/s floor an IDR is many times
  a P frame, and rate drops, loss IDRs and the 3 s GOP each add one.
- Frame acks (0x0A) are sent by both clients but the server only counts them;
  progress is inferred from 0x06 acknowledgements, which say a datagram
  arrived, not that a frame decoded. The browserless H.264 scenes run a
  perfect decoder with an 8 ms decode latency (`PERFECT_DECODER_LATENCY_US`), so
  the frame acks are always on there and ride the ack batches as they do in
  production.
- Resolution resets are defined but never sent (an IDR answers a resolution
  change).

## 10. Acked-reference sessions (Phase B, `io_bridge/h264_refs.rs`)

A session recovers from loss without an IDR when the client sent HELLO 0x04
(`h264_acked_ref`), acked references are not switched off
(`GHOSTFRAME_H264_BACKEND=ffmpeg` does), and the encode worker's backend
executes plans: libva-direct, when the machine has it. Anything else is the
Phase A policy of sections 3 and 4, unchanged. The decision is made on the
first planned frame: a worker whose backend answers `H264Reply::Unplanned` (no
libva-direct, or it refused the stream) puts the process back on Phase A, and
three failed encodes in a row do the same.

**Per frame** the loop owns the `RefManager` (`encoder/h264/refs.rs`) and
calls `plan_next(frame_seq, now)` with the plan epoch; `Plan::Hold` skips the
frame (the IDR of the epoch is still being delivered, section 5.4 of the
design), as does emission backpressure. The plan and the resident reference
set go to the worker (`FinishAction::H264Planned`), the worker's backend
encodes it from the GPU's NV12 buffer, and `on_h264_planned_done` commits the
access unit: a result of an older epoch is dropped, the wire header is written
from the plan (`FramePlan::wire_info`: IDR, non-ref, mark and target-is-LT
flags, the slot indices) with `ref_frame_seq` taken from the reference the
backend really put in `RefPicList0[0]` (`AuMeta.ref_seq`), the frame goes
through `send_h264_frame_described`, and `frame_sent` tells the manager what
was emitted (a mismatch kills the frame; three in a row restart with an IDR).
One frame is in flight, so a plan is always committed (or its epoch ended)
before the next is made.

**Feedback** goes to the manager: fragment acknowledgements (`frag_acked`), loss
verdicts on P fragments (`frame_suspect`: a hypothesis, which the client's
decoded bit overrides; the queued fragments of that frame are purged and the
next capture is made whole), the 0x0A frame ack (`frame_ack`), and every 0x09
reason but `NoDecoder` (`client_reset`, a new epoch that begins with an IDR;
the 2 s limit and the reset-storm stall rule apply). A frame the client
answered for stops counting as in flight for the stall flag. On these sessions
progress for the no-progress rule is a frame *newly* decoded, the IDR
draining, or a P fragment acknowledged while the client has not refused
anything since its last decode. A decoded *bit* is not progress: the mask is
cumulative, so a client that refuses everything after its IDR still shows one
for 64 frames; and a 0x0A blackout with the transport acks flowing is not a
stall. Frames sent for 500 ms since the last progress raise `h264_stalled`. The
clock starts with the first frame sent that nothing has decoded yet and is
cleared when the stream leaves H.264 (`abandon_h264_inflight`), so a stall that
sent the classifier to the tile codec, or an ordinary switch with an undecoded
last frame, does not hold the flag for the tile stretch.

A suspect frame's still-queued fragments are purged: its dependents are refused
by the client's gate anyway, a recovery P is cheap, and reordering is covered
(scene 4 recovers without an IDR). A frame the client reports decoded after
all overrides the suspicion until a recovery frame supersedes it.

Encoder faults (a failed encode, an unparsable access unit, an IDR flag that
disagrees with the plan) restart the stream with an IDR counted as an encoder
fault, whether the encode failed or three plans in a row did not match the
stream; three in a row with no *P-frame* anchor confirmed between them give
acked references up for the process (Phase A, logged at warn). A decoded IDR
does not count: it is candidate 0 of every epoch. Leaving H.264 while the
epoch's IDR is still being delivered purges it and marks it dead, so the return
sends it again at once.

**Not on these sessions**: the periodic GOP IDR, the loss-triggered IDR, the
IDR hold of section 3 (the manager's `Plan::Hold` replaces it; the loss path
still resends IDR fragments), the reopen on a rate change, and H.264 XOR parity
(the client tolerates a stream without it). The rate moves in place:
`RatePolicy` decisions that were reopens are `set_rate` calls, with no IDR. The
IDR is not the probe burst any more; padding and the application's probe
clusters (section 5) are the only probing.

**Rate control (B6).** The policy of section 4 is Phase A's; on these sessions
`RatePolicy::step_in_place` replaces its reopen economics, because a change of
rate is a re-send of the rate buffers on a P frame, not an IDR (T4, the RX 480:
the HRD restarts at 40 % fullness, so the first frame after a re-send is about
1.00 of the average, then a short dip of 0.56 and 0.87; the worst 1 s window at
4 Hz of noisy re-sends is 0.97 of the rate told).

- Candidate and target as in section 4 (`0.8 x` the smoothed estimate, a raw
  jump held), but a jump is held for 2 s (`SPIKE_HOLD_IN_PLACE`). A probe's
  result is the estimate's peak for a few hundred milliseconds, and the stream
  has to send at the rate it found for long enough for the acknowledgements to
  confirm it, or goog_cc pulls the estimate back before the stream used it. At
  1 s of hold the stream used the headroom on 28/16/18 of 96 timelines against
  52/58/55 at 2 s (the sustained metric below).
- The hold is **releasable**: it is let go when the raw estimate falls below
  the smoothed estimate from before the spike (the path is worse than it was),
  or below 0.85 of the spike while goog_cc reports a loss limit
  (`LossState`) or a standing queue (the queue estimate non-zero for
  `SPIKE_QUEUE_PERSIST`, 400 ms). A queue seen for less than that is not
  read as distress (a probe burst builds one by design: with the queue read
  at once the hold was let go on most step-ups and the stream used the
  headroom on 12 of 96). goog_cc's delay detector state is not
  exposed, so a delay-only overuse is seen as the estimate falling below the
  pre-spike level, not as itself. A hold that cannot be let go keeps a stream
  at three times its path for the rest of the 2 s after a drop that follows a
  rise (scene 6C, the drop 600 ms after the spike, three seeds: 144 kB over the
  link in the 3 s after it in all, against 98 kB, and 114 / 140 / 115 of 150
  frames shown against 129 / 146 / 138; the twin's latency does not tell them
  apart, 410-500 ms in both).
- An apply needs a 10 % fall or a 15 % rise from the running rate and is never
  closer than 250 ms to the last. Inside the band nothing is re-sent. There is
  no 2 s lock, so a drop shortly after a rise applies at once, and no confirmation
  count.
- A **cut** (the 3 s quiet period without padding, `last_cut_at`) is a fall of
  more than 30 % from the highest rate applied *outside a spike* in the last
  second, or from the rate in force before the spike. A spike is only
  recognised above 1.5 x the smoothed estimate, so its give-back is always a
  fall of over 30 %; counting it a cut silenced padding, the raise and the
  application's probes for 3 s after every probe.
- An expired spike is not re-armed by a later one (`observe` took the maximum
  with the previous spike's value without asking whether it was still held: a
  10 Mbit/s blip on a 6 Mbit/s path applied 8 Mbit/s on a 2 Mbit/s path
  for 2.1 s, 17 s after the blip, on a 3.5 Mbit/s reading). Phase A shares the line and is fixed with it.
- Every plan carries a `RateTarget`: `bps`, a P-frame cap of 3 average frames
  (`max_frame_bytes`, soft: the VCE overshoots it), the IDR budget `max(0.25 s of
  the rate, 32 kB)` (`idr_max_bytes`, an IDR is content-limited and lands within
  1.15 of it), and a VBV of `max(0.1 s of the rate, one cap)`, raised to the IDR
  budget on an IDR frame. The buffers (rate control, HRD, max frame size) are
  sent together; a change that only touches the cap is latched by a +-16 bps
  nudge of the rate. One writer: the planner's request, never the control
  channel. `rate_sends` / `rate_sends_10s` count every plan whose target differs
  from the last one sent, so the probe raise's re-sends (entering a window,
  leaving it) are counted with the policy's.
- **Probe raise (off by default).** A probe window is made of real frames where
  it can be. While a window is open (goog_cc's request or the application's
  own) each P frame is planned at `RATE_SHARE x` the window's rate (up to three
  frames of a 100 ms window at 30 fps, 2.1 on average), its fragments are
  tagged into the window, and the estimate ceiling is held off for it (for any
  window real frames claim, IDRs included, and for any tagged datagram: a
  legacy-stream IDR that claims a window holds the ceiling too, which is
  intended, its burst being the same burst). Padding stays, for the shortfall:
  on the first frame it is sized for the cluster less what the frames still to
  come are expected to carry (`RAISE_EXPECTED_YIELD`, half an average frame
  each), and the last frame tops up the whole shortfall; padding late in the
  window runs past its end and leaves at the pacer's rate, which measured worse
  than none. After the window the next plan is at the normal rate again: a
  re-send that costs a dip of about 0.5 of the average for two frames (T4 lever
  B), not "about nothing".

  The guards are checked for **every raised plan**, not only when a window
  opens: a cut (3 s), a late acknowledgement (the memory is two checkpoints at
  least 5 s apart, so it keeps the raise off for 5 to 10 s), a raised frame or
  a probe pad judged lost (`RAISE_LOSS_PAUSE`, 10 s), and no padding yet sent
  and judged (`RAISE_TRUST_AFTER`, 300 ms after the stream's first padding).
  The backlog guard is the plan's own (a backed-up queue skips the frame before
  the raise is considered).

  **Why it is off.** A window's burst on a buffer shallower than the burst
  drops the real frames queued behind the front of it (a padding-only window
  loses padding, which costs nothing). Before the guards a median 6 raised
  frames were lost per step-up run at a 60 ms buffer and 12 at 30 ms, against
  none without the raise, and sustained use of the headroom was 25/48 and 12/48
  against 35/48 and 26/48. With every guard above it is 31/48 and 31/48
  (against 35 and 26), and on the 600 ms population 55/28/44 of 96 against
  52/58/55 without it. The version without the "padding judged first" guard
  found the headroom on 70/67/71 of 96 at 600 ms (the raise's real gain) and
  paid for it at 60 ms (26/48 against 35/48) and 30 ms (22/48 against 26/48).
  It stays behind a field (`H264State::probe_raise_on`, set by the harness
  `InjectedH264::probe_raise`), measured by the `RefsRaise` rows and scene 9.
  `probe_raise_*` and `raised_frame_*` (below) account for it.
- **Measured** (browserless `p_b_seed_population`, `GF_POP_MODE`, 96 seeds, three
  ack timings, the P-B capacity step 2 -> 6 Mbit/s, `T_UP` = 11 s, a 600 ms link
  buffer, no collapse in any row). `reached` is the first estimate sample over
  `0.8 x 6` Mbit/s, which a transient spike gives. `sus_est` is the first time
  from which every estimate sample in the next second is over the line;
  `sus_use` also needs the access units captured in that second to total at
  least `0.8 x` the line (the stream uses what the estimate says). Hits within
  `T_UP`, per ack timing, and the step-up p50 / p90 on `sus_use` (ms):

  | | reached | sus_est | sus_use | step-up on sus_use |
  |---|---|---|---|---|
  | **shipped: in place, 2 s releasable hold, raise off** | 90/95/91 | 66/66/63 | **52/58/55** | 4100/16800, 4200/11100, 4300/6600 |
  | the same, hold never released | 90/95/91 | 66/67/64 | 52/58/56 | 4700/17000, 4700/20400, 4800/14000 |
  | the same, 1 s hold | 92/84/81 | 67/52/52 | 28/16/18 | 11100/17300, 11300/17900, 8300/22300 |
  | Phase A policy | 90/81/85 | 74/61/55 | 60/48/43 | 10400/20500, 10600/17300, 10800/18100 |
  | raise on, every guard | 95/92/93 | 74/63/64 | 55/28/44 | 5300/16900, 6600/20000, 6000/11800 |
  | raise on, no "padding judged" guard | 96/96/95 | 85/89/85 | 70/67/71 | 4500/11000, 5200/9900, 4500/5500 |

  Before this review's fixes the shipped row (2 s hold, raise on, the stale spike
  re-armed, the earlier model) read 96/95/95 reached, 78/71/72 `sus_est` and
  69/65/62 `sus_use`: "reached" was mostly a transient spike. Shallow buffers
  (48 timelines, as sent): `sus_use` 26 / 35 of 48 at 30 / 60 ms without the
  raise, 31 / 31 with every guard. Scene 6 (a capacity drop, with a drop shortly
  after a rise, 6B, and 600 ms after the probe's spike, 6C) bounds the bytes coded at the
  rate in force in any 1 s window at 1.15 x `RATE_SHARE x` the estimate, the
  backpressure skips after the drop at half those of a stream with no rate
  policy, and for 6C the bytes over the link, the frames shown and the latency.
- The model's raise: frames of a window's rate follow 1.00 / 0.36 / 0.83 of the
  average (mean yield 0.73, min 0.63, max 0.79 on the VCE, T4
  `h264_direct_probe_raise_lever_b_at_40pct`), the fall that ends the window
  dips to 0.5 for two frames, and the pessimistic gate runs at 0.86 of the
  profile (the VCE's minimum). The model's `idr_fill` (0.6) underestimates an
  IDR whose content is the limit (the VCE gave up to 1.15 x its budget).

**Tile codec.** Leaving H.264 is unchanged. Returning is `mode_reentry`: the
first frame is a recovery P from the newest confirmed anchor (an IDR only when
there is none), logged as `reentry_bytes` and `anchor_age_ms`. `frame_seq` is
shared with tile frames, so nothing here assumes +1.

**Observability.** The `h264 refs` line (target `ghostframe::h264`, the
`cumulative emit` cadence) carries the backend, plans, holds, the manager's
counters (recoveries, reroots, anchors confirmed, candidates marked, plan
mismatches, deadline suspects, `suspect_overridden`), the client's decoded and
refused counts, client resets, `reentry_bytes` and `anchor_age_ms`, the rate
(`rate_updates`, `rate_applies_up` / `_down`, `rate_sends`, `rate_sends_10s`)
and the probe raise (`probe_raise_windows` / `_frames` / `_bytes`,
`raised_frame_lost`, `raised_frames_acked`, `raised_delivery_us_max` / `_mean`).
`h264 stats` carries the plan's budget: `frame_cap_bytes`, `idr_cap_bytes`,
`frames_over_cap`, `max_cap_ratio_pct`.

**Client.** The web decoder reads profile, constraint flags and level from the
IDR's SPS (`codecStringFromSps`) and reconfigures on a key chunk when they
differ; libva-direct writes High at level 4.2 or above by frame size and rate.

**Tests.** `browserless_h264` `refs_scenes` run design T3 scenes 1 to 5, 7, 8
and 10 on this path with a PCM backend (`encoder/h264/harness.rs`) in place of
the encoder; `h264_direct_worker_path_*` (`io_bridge/tests.rs`, local, GPU) runs
the real worker and libva-direct through a loss.

### Known limits

- Probe padding stalls a reordering path on Phase A: with the rate policy
  running and 8 ms of reordering on a 20 ms link, the scripted CBR stream sent 39
  frames in 6 s against 164 on a clean path (the padding cluster fills quinn's
  FIFO ahead of the frames). The acked-reference loss and reordering scenes now
  run with the rate policy on (`refs_rated`): under 8 ms and 45 ms of
  reordering the stream makes at least 0.8 of the frames it makes without the
  policy (measured 0.93 to 0.98), with one IDR. The raise (off by default) keeps
  off for 5 to 10 s after a late acknowledgement for the same reason.
- Legacy sessions keep the Phase A encoder and policy (periodic IDR, loss IDR,
  parity, reopen on a rate change) until the ffmpeg path is removed.
- The step-up population reports the first sample over `0.8 x` the new capacity
  (`reached`) beside the sustained metric (`sus_est`, `sus_use`); the gates use
  `sus_use`. `reached` counts a probe's transient spike.
- The model's raise profile and post-window dip are one VCE's (the RX 480, T4);
  the P-frame cap is soft (`frames_over_cap` observes it).
- A raise in the window of a probe that fails (a real loss during it) costs the
  frames' picture quality, not padding bytes; `raised_frame_lost` counts them,
  and `pads_lost` (`h264 stats`) the probe pads the loss path judged lost, each
  of which pauses the raise.
