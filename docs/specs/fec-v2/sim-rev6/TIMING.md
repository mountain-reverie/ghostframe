# Emission timing, read from the code (origin/master)

Purpose: what "end of a drain" means on the wire, so the revision-6 block-closing rules can be
simulated against real cadence. Paths are relative to the repository root.
`[C]` = read from code, `[A]` = assumption (the code does not settle it).

## 1. The loop and where a drain happens

* One task runs `IoBridge::run` (`ghostframe-lib/src/transport/io_bridge.rs:2286`, a biased
  `tokio::select!`). It wakes for: the earliest QUIC timeout, an inbound packet (client ACKs,
  feedback), a frame from capture, a frame-worker result, and `next_wake_deadline`
  (`io_bridge.rs:2107`). `next_wake_deadline` includes `admission_wake`, the token-refill
  timer. **[C]**
* After every wake, `post_turn` (`io_bridge.rs:2507`) calls `refill_after_drain`
  (`io_bridge.rs:2544` -> `io_bridge/pacing.rs:429`): `pump_admission` (one *pass*), then
  `drain_outbound` (quinn writes its FIFO to the socket), then another pass, until a pass
  admits nothing (cap `MAX_REFILL_ROUNDS = 256`, `pacing.rs:83`). **[C]**
* A frame commit calls `pump_admission` directly (H.264: `io_bridge/frame.rs:1776`, after all
  of the frame's fragments were queued by `submit_h264_fragment`, `frame.rs:1749`). **[C]**
* There is no fixed-rate send timer. The only timers are quinn's, the token wake, PTO/loss
  timers and the DisplayMode debounce. **[C]**

A "drain" for bundling purposes is one call of `drain_reliable_emitter`
(`io_bridge/emit.rs:28`), i.e. one admission pass. That is the unit the simulator closes RS
blocks on.

## 2. What bounds the datagrams per pass

Inside `pump_admission` (`pacing.rs:268`):

1. **Token bucket.** `admission.refill(now, rate, mtu)` (`pacing.rs:275`;
   `ghostframe-lib/src/transport/admission.rs:176`): `tokens += rate * dt`, capped at
   `burst_cap = max(2 MTU, rate * 33.333 ms)` (`admission.rs:49,98`). `rate` is the
   estimate over the payload fraction (`admission_rate`, `pacing.rs:220`), i.e. about the
   link rate when the estimate is right. A datagram goes only if `try_take(wire_len)` can
   afford all of it (`admission.rs:213`, `admit_one` `admission.rs:275`); otherwise the pass
   stops with `Stop::Tokens` and the datagram is put back at the head (`emit.rs:75-77`).
   Repair (parity) datagrams are charged exactly like sources (`admission.rs:273`).
   Control datagrams are charged afterwards and may put the bucket into debt (`charge`,
   `admission.rs:232`).
2. **Depth gate `D_target`.** A datagram is refused while quinn's FIFO already holds
   `D_target = max(MTU, rate * 10 ms)` (`admission.rs:40,91`; `emit.rs:66`, `Stop::Depth`).
   2 Mbit/s -> 2.5 kB (about 2 datagrams); 10 Mbit/s -> 12.5 kB (10); 30 Mbit/s -> 37.5 kB
   (32); 100 Mbit/s -> 125 kB (106). A pass can therefore admit about
   `ceil(D_target / MTU)` datagrams before the FIFO is "full".
3. **Pass loop.** Up to 64 passes per `pump_admission` (`pacing.rs:79`), each pulling at most
   `min(tokens - parity reserve, D_target - depth)` bytes (`pacing.rs:311-312`), and not
   starting a pull with fewer than `MIN_PULL_BYTES = 300` tokens (`pacing.rs:303`).
4. **When tokens run out.** The next wake is
   `max(time_until(need), token_wake_floor)` (`pacing.rs:405-413`) with
   `token_wake_floor = max(MTU / rate, 1 ms)` (`pacing.rs:146-153`); `need` is one MTU when a
   built datagram is waiting. So with a backlog, **passes are at least 1 ms apart and at
   least one datagram's worth of tokens apart.**

Consequences, per link rate (backlogged sender, tokens about zero after each pass):

| link | MTU slot | datagrams earned by the 1 ms floor | `D_target` cap | typical pass |
|---|---|---|---|---|
| 2 Mbit/s | 4.7 ms | 0.2 (wake is one slot) | 3 | **1** |
| 10 Mbit/s | 0.94 ms | 1.06 | 10 | **~1** |
| 30 Mbit/s | 0.31 ms | 3.2 | 32 | **~3** |
| 100 Mbit/s | 0.094 ms | 10.6 | 106 | **~10** |

After an idle period the bucket is full (33 ms of rate: 7 / 35 / 106 / 352 datagrams at 2 / 10
/ 30 / 100 Mbit/s), and the first passes are `D_target`-sized, back to back. A H.264 P frame
(0.8 x estimate / 30 fps = 0.8 x 33 ms of rate) fits inside a full bucket, so frames leave
as a run of full passes followed by 1 ms-granularity ones. **[C]** for the mechanism, **[A]**
for the exact interleaving with ACK-triggered wakes: every inbound ACK batch also runs a turn
and a pass, which can only make passes smaller than modelled. The simulator is therefore an
optimistic (large-k) estimate for "close at the end of each drain".

## 3. What quinn does afterwards

* The congestion controller is the model controller: `W = max(4 MTU, R_w * (min_rtt +
  max_ack_delay) * G)`, `G = gain = 2.0` (`ghostframe-lib/src/transport/model_controller.rs:1-30`,
  `ghostframe-lib/src/config.rs:345`). No slow start, no loss reaction. **[C]**
* quinn-proto 0.11.14 (`Cargo.lock`) still runs its own pacer (`src/connection/pacing.rs`):
  capacity `W * 2 ms / srtt` clamped to `[10, 256] MTU` (`pacing.rs:129-151`), refill
  `1.25 * W / srtt` (`pacing.rs:92`). With `W = 2 R rtt` that is a refill rate of about
  `2.5 x R` and a burst of at least 10 datagrams. **So quinn does pace, but faster than the
  admission rate: it does not smooth what admission releases, it only caps a burst at about
  2.5 x the rate.** In the simulator the datagrams of a pass leave at 2.5 x C and then
  serialise through a bottleneck at C. **[C]** for the formula, **[A]** that `R_w` equals
  the link rate.
* The pass is a unit on the wire too: `drain_outbound` writes the FIFO right after, so one
  pass is a back-to-back train of QUIC packets.

## 4. Frame sizes and cadence

* `CAPTURE_FPS` default 30 (`ghostframe-server/src/config.rs:34`). H.264 frames are committed
  once per captured frame; tile dispatch is also per captured frame, so damage bursts are
  quantised to 33 ms. The simulator uses Poisson burst arrivals instead **[A]**.
* The encoder rate is `0.8 x` the bandwidth estimate, floor 300 kbit/s, cap 50 Mbit/s
  (`io_bridge/h264_rate.rs:25-31,241`). Per `RateTarget::steady`
  (`ghostframe-lib/src/encoder/h264/backend.rs:92`): P-frame cap = 3 average frames
  (`FRAME_CAP_FRAMES = 3`, line 67), IDR budget = `max(0.25 s of rate, 32 kB)` (lines 71-73).
  Average P frame = `0.8 R / 30`: 2 Mbit/s -> 6.7 kB (5.6 bundles); 10 -> 33 kB (28);
  30 -> 100 kB (85); 100 -> 333 kB (282). An IDR is about 7.5 average frames at the budget.
* Whole frames are queued at commit and leave over about 27 ms (0.8 of a frame interval) at
  the admission rate, so the sender is busy most of every frame interval and "idle" gaps are
  short (about 7 ms) in H.264 mode and long (the whole inter-damage gap) in tile mode.
* Parity is billed to the same bucket (`admission.rs:273`, `PARITY_RESERVE_GROUP`,
  `admission.rs:54`). Today it reserves 1/(K+1) of the grant for K = 10 XOR parity; revision 6
  would change that reserve to the chosen overhead.

## 5. What is assumed, not read

* The simulator's link rate equals the admission rate. In the real system the estimate lags
  and overshoots; a standing queue of 15-30 ms appears in the model from the 33 ms bucket
  alone. Absolute latencies therefore carry a common-mode offset, and the report uses
  *excess* delay (relative to the bundle's own would-have-been arrival) and frame lateness
  *relative to the loss-free completion*.
* One bundle = one QUIC packet = one loss event. Partial bundles at the end of a pass are
  treated as full MTU bundles (no padding cost counted). Real tile datagrams are smaller
  than the MTU, so the number of bundles per pass is, if anything, lower than modelled.
* Loss is applied per bundle in packet-number order (not time-correlated), i.i.d. or
  Gilbert-Elliott with loss_bad = 1.
* The ACK clock, reordering, jitter and the congestion estimate's reaction to repair traffic
  are not simulated. Repair bytes are charged to the bucket, and the encoder rate is fixed
  at 0.7 x C (not 0.8) so that even 30 % overhead fits below C.
