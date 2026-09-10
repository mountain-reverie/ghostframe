# BWE GoogCC Stage 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the EWMA estimator in `ghostframe-lib/src/transport/bwe.rs` with `goog_cc`'s `GoogCcNetworkController`, behind the existing `AckArrival`/`BweSnapshot` seam, so the controller observes real traffic without steering emission.

**Architecture:** `transport/bwe.rs` becomes a directory module. `timeline.rs` reconstructs monotonic timestamps from the 16-bit wire fields; `googcc.rs` drives the controller; `mod.rs` keeps the public seam byte-identical so the two `io_bridge` call sites are untouched. Emission policy does not change in this stage.

**Tech Stack:** Rust, `goog_cc` 0.1.4 (BSD-3-Clause), existing `ghostframe-e2e` netsim harness.

---

## Before you start

Run every cargo command with `export TMPDIR=/home/cedric/.cache/ghostframe-tmp` (mkdir -p it first). `/tmp` fills up on this machine and produces a confusing build.rs panic from ghostbridge's Go link step.

Run cargo in the **foreground** with an explicit timeout. Never `git add -A`.

**`goog_cc`'s published docs list module paths that do not exist.** `goog_cc::api::` is private. The real paths are `goog_cc::network_control`, `goog_cc::transport`, `goog_cc::units`. Following docs.rs gives `E0603` immediately.

**All `goog_cc` unit constructors take `i64`**, not `u64` (`DataSize::from_bytes(i64)`, `DataRate::from_bits_per_sec(i64)`, `TimeDelta::from_millis(i64)`).

## File structure

| File | Responsibility |
|---|---|
| `ghostframe-lib/Cargo.toml` | add the `goog_cc` dependency |
| `ghostframe-lib/src/transport/bwe/mod.rs` | moved from `bwe.rs`; public seam (`AckArrival`, `BweSnapshot`, `BweWrapper`) — unchanged signatures |
| `ghostframe-lib/src/transport/bwe/timeline.rs` | new; `Lo16Timeline` reconstructs monotonic ms from 16-bit wire values |
| `ghostframe-lib/src/transport/bwe/googcc.rs` | new; owns `GoogCcNetworkController`, maps samples to `TransportPacketsFeedback` |
| `ghostframe-lib/src/transport/io_bridge.rs` | add `size_bytes` to `BweSample`; feed quinn RTT |
| `ghostframe-lib/tests/bwe_bench.rs` | new; tier-1 deterministic controller bench |

---

### Task 1: Add the dependency and prove the import paths

**Files:**
- Modify: `ghostframe-lib/Cargo.toml`
- Test: `ghostframe-lib/src/transport/bwe.rs` (temporary test, moved in Task 3)

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` at the bottom of `ghostframe-lib/src/transport/bwe.rs`:

```rust
    /// Guards the import paths. goog_cc's published docs reference
    /// `goog_cc::api::*`, which is a private module; the real re-exports are
    /// at the crate root. This test fails to compile if that changes.
    #[test]
    fn googcc_constructs_standalone() {
        use goog_cc::network_control::NetworkControllerConfig;
        use goog_cc::transport::TargetRateConstraints;
        use goog_cc::units::{DataRate, Timestamp};
        use goog_cc::{GoogCcConfig, GoogCcNetworkController};

        let t0 = Timestamp::from_millis(1_000);
        let mut cfg = NetworkControllerConfig::default();
        cfg.constraints = TargetRateConstraints {
            at_time: t0,
            min_data_rate: Some(DataRate::from_kilobits_per_sec(100)),
            max_data_rate: Some(DataRate::from_kilobits_per_sec(50_000)),
            starting_rate: Some(DataRate::from_kilobits_per_sec(1_000)),
        };
        let _ctl = GoogCcNetworkController::new(cfg, GoogCcConfig { feedback_only: false });
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p ghostframe-lib --lib googcc_constructs_standalone`
Expected: FAIL — `use of unresolved module or unlinked crate 'goog_cc'`.

- [ ] **Step 3: Add the dependency**

In `ghostframe-lib/Cargo.toml`, under `[dependencies]`, next to the existing `str0m` entry:

```toml
# Standalone Rust port of libwebrtc's Google Congestion Control. Used for the
# delay-gradient + loss-based bandwidth estimate. str0m's own GoogCC port is
# pub(crate) and cannot be constructed without an Rtc session (checked against
# 0.21 and 0.23.1), which is why this is a separate dependency.
goog_cc = "0.1.4"
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cargo test -p ghostframe-lib --lib googcc_constructs_standalone`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/Cargo.toml ghostframe-lib/src/transport/bwe.rs Cargo.lock
git commit -m "deps: add goog_cc for the delay-gradient bandwidth estimator"
```

---

### Task 2: Monotonic timeline from 16-bit wire timestamps

`arrival_time_ms_lo16` and the server emit timestamp are both low-16-bits of a millisecond clock, so they wrap every 65,536 ms (~65.5 s). GoogCC's inter-arrival deltas break across a wrap. This reconstructs a monotonic timeline.

**Files:**
- Create: `ghostframe-lib/src/transport/bwe/timeline.rs`
- Modify: `ghostframe-lib/src/transport/bwe.rs` (add `mod timeline;` — the directory move happens in Task 3)

- [ ] **Step 1: Write the failing test**

Create `ghostframe-lib/src/transport/bwe/timeline.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::Lo16Timeline;

    #[test]
    fn first_sample_anchors_without_jumping() {
        let mut t = Lo16Timeline::default();
        assert_eq!(t.unwrap_ms(5_000), 5_000);
    }

    #[test]
    fn monotonic_within_a_wrap_period() {
        let mut t = Lo16Timeline::default();
        assert_eq!(t.unwrap_ms(1_000), 1_000);
        assert_eq!(t.unwrap_ms(2_000), 2_000);
        assert_eq!(t.unwrap_ms(60_000), 60_000);
    }

    #[test]
    fn carries_across_a_wrap() {
        let mut t = Lo16Timeline::default();
        assert_eq!(t.unwrap_ms(65_000), 65_000);
        // 65_000 + 1_000 wraps to 464 in u16 space.
        assert_eq!(t.unwrap_ms(464), 66_000);
        assert_eq!(t.unwrap_ms(1_464), 67_000);
    }

    #[test]
    fn small_backward_step_stays_backward_not_wrapped() {
        // Reordered ACK: 50 ms earlier must read as 50 ms earlier, not as
        // one wrap period later.
        let mut t = Lo16Timeline::default();
        assert_eq!(t.unwrap_ms(10_000), 10_000);
        assert_eq!(t.unwrap_ms(9_950), 9_950);
    }

    #[test]
    fn many_consecutive_wraps_stay_monotonic() {
        let mut t = Lo16Timeline::default();
        let mut expected: u64 = 0;
        for step in 0..1_000u64 {
            expected = step * 500;
            let lo16 = (expected % 65_536) as u16;
            assert_eq!(t.unwrap_ms(lo16), expected, "diverged at step {step}");
        }
        assert_eq!(expected, 499_500);
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

Add `mod timeline;` immediately after the `use` block at the top of `ghostframe-lib/src/transport/bwe.rs`.

Run: `cargo test -p ghostframe-lib --lib bwe::timeline`
Expected: FAIL — `cannot find type 'Lo16Timeline' in this scope`.

- [ ] **Step 3: Implement**

Insert above the `#[cfg(test)] mod tests` in `timeline.rs`:

```rust
//! Reconstructs a monotonic millisecond timeline from the 16-bit timestamps
//! carried on the wire.
//!
//! `arrival_time_ms_lo16` (ACK envelope) and the server emit timestamp are
//! both low-16-bits of a millisecond clock, so they wrap every 65,536 ms.
//! The EWMA estimator tolerated that because it only compared coarse
//! differences; GoogCC's inter-arrival deltas do not — a wrap reads as a
//! 65-second backward jump and poisons the delay-gradient signal.

/// Half a wrap period. A step larger than this in either direction is read as
/// a wrap rather than as a genuine jump, which is the standard sequence-space
/// disambiguation: it is correct as long as consecutive samples are less than
/// ~32 s apart, and ACK batches arrive far more often than that.
const HALF_PERIOD: i64 = 32_768;
const PERIOD: i64 = 65_536;

/// Per-direction unwrapper. One instance per timestamp series — the emit
/// series and the arrival series must NOT share one, since they come from
/// different clocks.
#[derive(Debug, Default)]
pub(crate) struct Lo16Timeline {
    last: Option<u64>,
}

impl Lo16Timeline {
    /// Map the next 16-bit sample onto the monotonic timeline.
    pub(crate) fn unwrap_ms(&mut self, lo16: u16) -> u64 {
        let Some(last) = self.last else {
            // Anchor the series on its first value so early timestamps are
            // small and readable in logs.
            self.last = Some(lo16 as u64);
            return lo16 as u64;
        };

        let base = (last as i64) & !(PERIOD - 1);
        let mut candidate = base | (lo16 as i64);
        // Choose the wrap-period offset that lands nearest `last`.
        if candidate - (last as i64) > HALF_PERIOD {
            candidate -= PERIOD;
        } else if (last as i64) - candidate > HALF_PERIOD {
            candidate += PERIOD;
        }
        let value = candidate.max(0) as u64;
        self.last = Some(value);
        value
    }
}
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cargo test -p ghostframe-lib --lib bwe::timeline`
Expected: PASS, 5 tests.

- [ ] **Step 5: Mutation-check the wrap handling**

Temporarily delete the `else if` branch (the backward-wrap correction). Re-run.
Expected: `carries_across_a_wrap` and `many_consecutive_wraps_stay_monotonic` FAIL.
If they pass, the tests are not exercising the wrap and must be fixed before continuing. Restore the branch afterwards and confirm all 5 pass again.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/src/transport/bwe/timeline.rs ghostframe-lib/src/transport/bwe.rs
git commit -m "feat(bwe): monotonic timeline reconstruction from 16-bit wire timestamps"
```

---

### Task 3: Move `bwe.rs` to a directory module and add packet size

GoogCC computes rates from bytes, and `AckArrival` carries no size today, so the estimate would be meaningless without this.

**Files:**
- Move: `ghostframe-lib/src/transport/bwe.rs` → `ghostframe-lib/src/transport/bwe/mod.rs`
- Modify: `ghostframe-lib/src/transport/io_bridge.rs:634-647` (`BweSample`), `:2003-2029` (construction)

- [ ] **Step 1: Move the file**

```bash
git mv ghostframe-lib/src/transport/bwe.rs ghostframe-lib/src/transport/bwe/mod.rs
```

Run: `cargo test -p ghostframe-lib --lib bwe`
Expected: PASS, unchanged — a directory module with `mod.rs` resolves identically.

- [ ] **Step 2: Write the failing test**

Add to `mod tests` in `ghostframe-lib/src/transport/bwe/mod.rs`:

```rust
    /// GoogCC derives rate from bytes, so a sample without a size cannot
    /// contribute to an estimate. This pins the field onto the seam.
    #[test]
    fn ack_arrival_carries_packet_size() {
        let a = AckArrival {
            wire_seq: 1,
            server_emit_ms_lo16: 10,
            client_arrival_ms_lo16: 25,
            size_bytes: 1200,
        };
        assert_eq!(a.size_bytes, 1200);
    }
```

- [ ] **Step 3: Run it and watch it fail**

Run: `cargo test -p ghostframe-lib --lib ack_arrival_carries_packet_size`
Expected: FAIL — `struct 'AckArrival' has no field named 'size_bytes'`.

- [ ] **Step 4: Add the field to the seam**

In `ghostframe-lib/src/transport/bwe/mod.rs`, add to `AckArrival` (after `client_arrival_ms_lo16`):

```rust
    /// Wire size of the acknowledged datagram in bytes, summed over its
    /// fragments. GoogCC derives delivery rate from bytes; without this the
    /// controller sees arrivals but no volume and cannot form an estimate.
    pub size_bytes: u32,
```

- [ ] **Step 5: Carry the size from the cache**

In `io_bridge.rs`, add to `struct BweSample` (after `owd_ms_lo16`):

```rust
    /// Wire size of this datagram in bytes, summed over its fragments.
    size_bytes: u32,
```

At the construction site (`io_bridge.rs`, inside the `for (emit_key, e) in emit_keys.iter().zip(...)` loop), replace the `server_emit_ms_lo16` binding and the push so both read the cache entry once:

```rust
                        let cache_entry = self.reliable_emitter.cache.get(emit_key);
                        let size_bytes: u32 = cache_entry
                            .map(|entry| {
                                entry.fragments.iter().map(|f| f.len() as u32).sum::<u32>()
                            })
                            .unwrap_or(0);
                        let server_emit_ms_lo16 = cache_entry
                            .and_then(|entry| entry.fragments.first())
                            .filter(|frag| frag.len() >= 16)
                            .map(|frag| {
                                let ts_be: [u8; 4] = frag[12..16].try_into().unwrap();
                                // u32 μs → u16 ms low-16: divide by 1000 to
                                // convert μs→ms, then wrap to u16. Matches the
                                // wire shape of arrival_time_ms_lo16.
                                (u32::from_be_bytes(ts_be) / 1000) as u16
                            });
                        if let Some(emit_lo16) = server_emit_ms_lo16 {
                            let arrival_lo16 = e.arrival_time_ms_lo16;
                            let tier = pass_tier(e.pass_idx);
                            let owd_ms_lo16 = arrival_lo16.wrapping_sub(emit_lo16);
                            self.bwe_samples_buffer.push(BweSample {
                                tier,
                                server_emit_ms_lo16: emit_lo16,
                                client_arrival_ms_lo16: arrival_lo16,
                                owd_ms_lo16,
                                size_bytes,
                                received_at: now_for_samples,
                            });
                        }
```

Then in the drain that builds `AckArrival` (`io_bridge.rs`, the `.map(|s| AckArrival { .. })` closure), add:

```rust
                        size_bytes: s.size_bytes,
```

- [ ] **Step 6: Run and watch it pass**

Run: `cargo test -p ghostframe-lib --lib`
Expected: PASS, 376 tests (375 existing + the new one).

- [ ] **Step 7: Commit**

```bash
git add ghostframe-lib/src/transport/bwe/mod.rs ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(bwe): carry acknowledged datagram size through to the estimator seam"
```

---

### Task 4: The GoogCC driver

**Files:**
- Create: `ghostframe-lib/src/transport/bwe/googcc.rs`
- Modify: `ghostframe-lib/src/transport/bwe/mod.rs` (add `mod googcc;`)

- [ ] **Step 1: Write the failing test**

Create `ghostframe-lib/src/transport/bwe/googcc.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::GoogCcDriver;
    use crate::transport::bwe::AckArrival;
    use std::time::{Duration, Instant};

    /// Feeding a steadily-delivered stream must produce a non-zero estimate.
    /// A driver that never reaches the controller returns the starting rate
    /// forever, so this also catches "wired up but not actually fed".
    #[test]
    fn steady_delivery_produces_an_estimate() {
        let t0 = Instant::now();
        let mut d = GoogCcDriver::new(1_000_000, t0);

        let mut emit_ms: u16 = 0;
        for step in 0..400u32 {
            // 12 x 1200 B every 20 ms ≈ 5.76 Mbit/s offered.
            let batch: Vec<AckArrival> = (0..12)
                .map(|i| {
                    let e = emit_ms.wrapping_add(i);
                    AckArrival {
                        wire_seq: step * 12 + i as u32,
                        server_emit_ms_lo16: e,
                        client_arrival_ms_lo16: e.wrapping_add(15),
                        size_bytes: 1200,
                    }
                })
                .collect();
            emit_ms = emit_ms.wrapping_add(20);
            d.update(&batch, t0 + Duration::from_millis(20 * step as u64));
        }

        let snap = d.snapshot();
        assert!(
            snap.bitrate_bps > 0,
            "controller produced no estimate after 400 batches"
        );
        assert_eq!(snap.samples_seen, 400 * 12);
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

Add `mod googcc;` after `mod timeline;` in `ghostframe-lib/src/transport/bwe/mod.rs`.

Run: `cargo test -p ghostframe-lib --lib bwe::googcc`
Expected: FAIL — `cannot find type 'GoogCcDriver' in this scope`.

- [ ] **Step 3: Implement**

Insert above the test module in `googcc.rs`:

```rust
//! Drives `goog_cc::GoogCcNetworkController` from our ACK-arrival samples.
//!
//! The controller runs send-side, so it needs no wire-format change: the
//! server's own emit timestamps plus the receiver arrival times echoed in the
//! ACK envelope are sufficient. A constant clock offset between the two
//! cancels in GoogCC's `recv_delta - send_delta`.

use super::timeline::Lo16Timeline;
use super::{AckArrival, BweSnapshot};
use goog_cc::network_control::{NetworkControllerConfig, NetworkControllerInterface};
use goog_cc::transport::{
    PacketResult, ProcessInterval, SentPacket, TargetRateConstraints, TransportPacketsFeedback,
};
use goog_cc::units::{DataRate, DataSize, Timestamp};
use goog_cc::{GoogCcConfig, GoogCcNetworkController};
use std::time::Instant;

/// Floor and ceiling handed to the controller. The floor keeps a badly
/// congested link usable rather than collapsing to nothing; the ceiling stops
/// a probe overshoot from proposing an absurd rate on a fast LAN.
const MIN_BPS: i64 = 200_000;
const MAX_BPS: i64 = 200_000_000;

pub(crate) struct GoogCcDriver {
    ctl: GoogCcNetworkController,
    /// Separate unwrappers: the emit series is the server's clock and the
    /// arrival series is the client's. Sharing one would interleave two
    /// unrelated clocks and corrupt both.
    emit_time: Lo16Timeline,
    arrival_time: Lo16Timeline,
    base: Instant,
    estimate_bps: u64,
    samples_seen: u64,
}

impl GoogCcDriver {
    pub(crate) fn new(initial_bps: u64, now: Instant) -> Self {
        let at_time = Timestamp::from_millis(0);
        let mut cfg = NetworkControllerConfig::default();
        cfg.constraints = TargetRateConstraints {
            at_time,
            min_data_rate: Some(DataRate::from_bits_per_sec(MIN_BPS)),
            max_data_rate: Some(DataRate::from_bits_per_sec(MAX_BPS)),
            starting_rate: Some(DataRate::from_bits_per_sec(
                (initial_bps as i64).clamp(MIN_BPS, MAX_BPS),
            )),
        };
        Self {
            ctl: GoogCcNetworkController::new(cfg, GoogCcConfig { feedback_only: false }),
            emit_time: Lo16Timeline::default(),
            arrival_time: Lo16Timeline::default(),
            base: now,
            estimate_bps: initial_bps,
            samples_seen: 0,
        }
    }

    /// Feed one ACK batch and advance the controller.
    pub(crate) fn update(&mut self, records: &[AckArrival], now: Instant) -> BweSnapshot {
        if records.is_empty() {
            return self.snapshot();
        }

        let feedback_time = self.to_timestamp(now);
        let mut packet_feedbacks = Vec::with_capacity(records.len());
        for r in records {
            let send_ms = self.emit_time.unwrap_ms(r.server_emit_ms_lo16);
            let recv_ms = self.arrival_time.unwrap_ms(r.client_arrival_ms_lo16);
            let sent = SentPacket {
                send_time: Timestamp::from_millis(send_ms as i64),
                size: DataSize::from_bytes(r.size_bytes as i64),
                ..Default::default()
            };
            // The controller must see the send before the acknowledgement.
            self.ctl.on_sent_packet(sent.clone());
            packet_feedbacks.push(PacketResult {
                sent_packet: sent,
                receive_time: Timestamp::from_millis(recv_ms as i64),
                ..Default::default()
            });
        }
        self.samples_seen += records.len() as u64;

        let upd = self.ctl.on_transport_packets_feedback(TransportPacketsFeedback {
            feedback_time,
            data_in_flight: DataSize::from_bytes(0),
            packet_feedbacks,
            sendless_arrival_times: Vec::new(),
        });
        self.absorb(upd);

        let upd = self.ctl.on_process_interval(ProcessInterval {
            at_time: feedback_time,
            ..Default::default()
        });
        self.absorb(upd);

        self.snapshot()
    }

    pub(crate) fn snapshot(&self) -> BweSnapshot {
        BweSnapshot {
            bitrate_bps: self.estimate_bps,
            samples_seen: self.samples_seen,
        }
    }

    fn absorb(&mut self, upd: goog_cc::transport::NetworkControlUpdate) {
        if let Some(t) = upd.target_rate {
            let bps = t.target_rate.bps();
            if bps > 0 {
                self.estimate_bps = bps as u64;
            }
        }
    }

    fn to_timestamp(&self, now: Instant) -> Timestamp {
        Timestamp::from_millis(now.saturating_duration_since(self.base).as_millis() as i64)
    }
}
```

- [ ] **Step 4: Run and watch it pass**

Run: `cargo test -p ghostframe-lib --lib bwe::googcc`
Expected: PASS.

- [ ] **Step 5: Mutation-check that the controller is really driven**

Temporarily make `absorb` a no-op (`fn absorb(&mut self, _upd: ...) {}`). Re-run.
Expected: `steady_delivery_produces_an_estimate` still passes, because the estimate falls back to the starting rate — **which means the test is too weak**. Strengthen it before continuing by asserting the estimate *moved*:

```rust
        assert_ne!(
            snap.bitrate_bps, 1_000_000,
            "estimate never moved off the starting rate — the controller's \
             updates are not being absorbed"
        );
```

Re-run with the no-op still in place and confirm it now FAILS, then restore `absorb` and confirm it passes.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/src/transport/bwe/googcc.rs ghostframe-lib/src/transport/bwe/mod.rs
git commit -m "feat(bwe): GoogCC driver mapping ACK arrivals to transport feedback"
```

---

### Task 5: Swap `BweWrapper`'s internals to the driver

The public seam does not change, so neither `io_bridge` call site is touched.

**Files:**
- Modify: `ghostframe-lib/src/transport/bwe/mod.rs`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `bwe/mod.rs`:

```rust
    /// The seam must be backed by GoogCC, not the EWMA. A rising-delay stream
    /// is the signal only a delay-gradient controller reacts to: the EWMA
    /// sees constant delivered volume and holds its estimate, GoogCC sees the
    /// gradient and backs off.
    #[test]
    fn wrapper_backs_off_on_rising_delay() {
        let t0 = std::time::Instant::now();
        let mut w = BweWrapper::new(4_000_000, t0);

        let mut emit_ms: u16 = 0;
        let mut extra_delay: u16 = 0;
        let mut last = w.snapshot().bitrate_bps;
        for step in 0..400u32 {
            // Constant volume, but one-way delay grows 1 ms per batch.
            let batch: Vec<AckArrival> = (0..12)
                .map(|i| {
                    let e = emit_ms.wrapping_add(i);
                    AckArrival {
                        wire_seq: step * 12 + i as u32,
                        server_emit_ms_lo16: e,
                        client_arrival_ms_lo16: e.wrapping_add(15).wrapping_add(extra_delay),
                        size_bytes: 1200,
                    }
                })
                .collect();
            emit_ms = emit_ms.wrapping_add(20);
            extra_delay = extra_delay.wrapping_add(1);
            last = w
                .update(&batch, t0 + std::time::Duration::from_millis(20 * step as u64))
                .bitrate_bps;
        }

        assert!(
            last < 4_000_000,
            "estimate {last} never fell despite steadily rising one-way delay"
        );
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p ghostframe-lib --lib wrapper_backs_off_on_rising_delay`
Expected: FAIL — the EWMA holds its estimate because delivered volume is constant.

- [ ] **Step 3: Replace the internals**

In `ghostframe-lib/src/transport/bwe/mod.rs`, replace the body of `BweWrapper` (its fields, `new`, `update`, `snapshot`, and the now-unused EWMA constants and `Window` accumulator) with a delegation to the driver. Keep every public signature exactly as it is:

```rust
pub struct BweWrapper {
    driver: googcc::GoogCcDriver,
}

impl BweWrapper {
    pub fn new(initial_bps: u64, now: Instant) -> Self {
        Self {
            driver: googcc::GoogCcDriver::new(initial_bps, now),
        }
    }

    pub fn update(&mut self, records: &[AckArrival], now: Instant) -> BweSnapshot {
        self.driver.update(records, now)
    }

    pub fn snapshot(&self) -> BweSnapshot {
        self.driver.snapshot()
    }
}
```

Update the module doc comment at the top of `bwe/mod.rs`: the existing text explains why str0m could not be used and that a "lightweight EWMA" stands in for Phase 1. Replace that explanation with the current position — str0m's GoogCC is still `pub(crate)` in 0.23.1, so the standalone `goog_cc` crate is used instead — and delete the "swap the internals for a richer estimator" sentence, which is now done.

Two comments elsewhere assert things that stop being true at this task. Both
must be corrected here, or they will tell the next reader the opposite of
reality:

- `bwe/mod.rs` still says "str0m is still listed as a Cargo dependency so that
  `str0m::bwe::Bitrate` ...". str0m was removed in Task 1.
- `io_bridge.rs:623-624` describes `BweSample` as input to "the str0m::bwe
  estimator (wired in Phase 1 Task 8)". Replace `str0m::bwe` with `goog_cc`
  and drop the stale task reference.

- [ ] **Step 4: Run and watch it pass**

Run: `cargo test -p ghostframe-lib --lib`
Expected: PASS. Any EWMA-specific test that no longer compiles should be **deleted, not adapted** — those tests assert properties of an estimator that no longer exists, and rewriting them to pass against GoogCC would produce tests that assert nothing.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/bwe/mod.rs ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(bwe): back the estimator seam with GoogCC instead of the EWMA"
```

---

### Task 6: Feed quinn's RTT to the controller

GoogCC's loss-based stage uses RTT. It is already read for the scheduler at `io_bridge.rs:1314`.

**Files:**
- Modify: `ghostframe-lib/src/transport/bwe/mod.rs`, `ghostframe-lib/src/transport/bwe/googcc.rs`, `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `bwe/mod.rs`:

```rust
    /// RTT updates must be accepted without disturbing the sample count —
    /// they inform the controller, they are not delivery samples.
    #[test]
    fn rtt_update_does_not_count_as_a_sample() {
        let t0 = std::time::Instant::now();
        let mut w = BweWrapper::new(1_000_000, t0);
        w.on_rtt(std::time::Duration::from_millis(40), t0);
        assert_eq!(w.snapshot().samples_seen, 0);
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p ghostframe-lib --lib rtt_update_does_not_count_as_a_sample`
Expected: FAIL — `no method named 'on_rtt' found`.

- [ ] **Step 3: Implement**

In `googcc.rs`, add to `impl GoogCcDriver`:

```rust
    /// Feed a fresh RTT measurement. Does not count as a delivery sample.
    pub(crate) fn on_rtt(&mut self, rtt: std::time::Duration, now: Instant) {
        let at_time = self.to_timestamp(now);
        let upd = self
            .ctl
            .on_round_trip_time_update(goog_cc::transport::RoundTripTimeUpdate {
                at_time,
                round_trip_time: goog_cc::units::TimeDelta::from_millis(
                    rtt.as_millis().min(i64::MAX as u128) as i64,
                ),
                smoothed: true,
            });
        self.absorb(upd);
    }
```

Add the matching delegation in `bwe/mod.rs`:

```rust
    pub fn on_rtt(&mut self, rtt: std::time::Duration, now: Instant) {
        self.driver.on_rtt(rtt, now);
    }
```

If `RoundTripTimeUpdate`'s field names differ from the above, read them from
`~/.cargo/registry/src/*/goog_cc-0.1.4/src/api/transport/network_types.rs` and
use the real ones — do not guess.

- [ ] **Step 4: Run and watch it pass**

Run: `cargo test -p ghostframe-lib --lib rtt_update_does_not_count_as_a_sample`
Expected: PASS.

- [ ] **Step 5: Wire it in production**

In `io_bridge.rs`, at the site that already drains `bwe_samples_buffer` into
`self.bwe.update(&records, now_std())`, immediately before that call add:

```rust
                // Feed the controller the path RTT quinn already tracks. Cheap
                // and only on ACK batches, so no extra polling. Mirrors the
                // accessor the scheduler already uses (io_bridge.rs:1310):
                // `connections` is a field, not a method.
                if let Some(rtt) = self
                    .server
                    .connections
                    .values()
                    .map(|c| c.stats().path.rtt)
                    .min()
                {
                    self.bwe.on_rtt(rtt, now_std());
                }
```

`.min()` across sessions matches what the scheduler does — the tightest path is
the one worth pacing against.

- [ ] **Step 6: Verify**

Run: `cargo test -p ghostframe-lib --lib`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-lib/src/transport/bwe/mod.rs ghostframe-lib/src/transport/bwe/googcc.rs ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(bwe): feed quinn path RTT into the controller"
```

---

### Task 7: Tier-1 deterministic controller bench

The spec's tier-1 validation. A pure function of its inputs, so a failure reproduces exactly — unlike a browserless scene.

**Files:**
- Create: `ghostframe-lib/tests/bwe_bench.rs`

- [ ] **Step 1: Write the failing test**

Create `ghostframe-lib/tests/bwe_bench.rs`:

```rust
//! Tier-1 bandwidth-estimator bench.
//!
//! Drives the estimator against a simulated bottleneck: packets serialise at
//! the link rate, so offering more than capacity makes one-way delay grow —
//! the signal a delay-gradient controller is built to detect.
//!
//! This is deterministic. Unlike the browserless scenes it is a pure function
//! of its inputs, so any failure here reproduces exactly.

use ghostframe_lib::transport::bwe::{AckArrival, BweWrapper};
use std::time::{Duration, Instant};

const PKT_BYTES: u32 = 1200;

/// Run `steps` 20 ms ticks against `capacity_bps(step)`, returning the
/// estimate at each tick.
fn run(steps: u32, capacity_bps: impl Fn(u32) -> u64) -> Vec<u64> {
    let t0 = Instant::now();
    let mut w = BweWrapper::new(1_000_000, t0);
    let mut out = Vec::with_capacity(steps as usize);

    let mut send_rate_bps: u64 = 1_000_000;
    let mut emit_ms: u64 = 0;
    let mut queue_free_ms: u64 = 0;

    for step in 0..steps {
        let capacity = capacity_bps(step);
        let bytes_this_tick = (send_rate_bps / 8) * 20 / 1000;
        let n = (bytes_this_tick / PKT_BYTES as u64).max(1);

        let mut batch = Vec::with_capacity(n as usize);
        for i in 0..n {
            // Spread sends across the tick, as a pacer would.
            let spacing_ms = ((PKT_BYTES as u64 * 8 * 1000) / send_rate_bps.max(1)).max(1);
            let send_ms = emit_ms + spacing_ms * i;
            let serial_ms = (PKT_BYTES as u64 * 8 * 1000) / capacity.max(1);
            let start = queue_free_ms.max(send_ms);
            let arrive = start + serial_ms;
            queue_free_ms = arrive;
            batch.push(AckArrival {
                wire_seq: step * 1000 + i as u32,
                server_emit_ms_lo16: send_ms as u16,
                client_arrival_ms_lo16: (arrive + 15) as u16,
                size_bytes: PKT_BYTES,
            });
        }
        emit_ms += 20;

        let snap = w.update(&batch, t0 + Duration::from_millis(emit_ms));
        send_rate_bps = snap.bitrate_bps.max(200_000);
        out.push(snap.bitrate_bps);
    }
    out
}

#[test]
fn estimate_rises_toward_capacity_on_a_clean_link() {
    let series = run(400, |_| 3_000_000);
    let final_bps = *series.last().unwrap();
    assert!(
        final_bps >= 2_400_000,
        "estimate {final_bps} never reached 80% of the 3 Mbit/s link"
    );
}

#[test]
fn estimate_backs_off_below_capacity_after_a_step_down() {
    // 3 Mbit/s for the first half, 600 kbit/s for the second.
    let series = run(600, |step| if step < 300 { 3_000_000 } else { 600_000 });
    let before = series[299];
    let after = *series.last().unwrap();

    assert!(
        before > 1_500_000,
        "estimate {before} should have climbed before the step-down"
    );
    assert!(
        after < 900_000,
        "estimate {after} did not fall after capacity dropped to 600 kbit/s"
    );
}

#[test]
fn estimate_survives_a_timestamp_wrap() {
    // 16-bit ms timestamps wrap every 65_536 ms; 4000 ticks x 20 ms crosses
    // that boundary more than once. Without unwrapping, the delay gradient is
    // poisoned at each wrap and the estimate collapses.
    let series = run(4_000, |_| 3_000_000);
    let final_bps = *series.last().unwrap();
    assert!(
        final_bps >= 2_400_000,
        "estimate {final_bps} collapsed across a timestamp wrap"
    );
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p ghostframe-lib --test bwe_bench`
Expected: FAIL — `transport::bwe` is not publicly reachable from an integration test.

- [ ] **Step 3: Confirm the seam is reachable**

`ghostframe-lib/src/transport/mod.rs:2` is already `pub mod bwe;`, so an
integration test can reach it with no change. If Step 2 failed for any reason
*other* than the test file not existing yet, stop and report — it means
something else moved.

- [ ] **Step 4: Run and watch it pass**

Run: `cargo test -p ghostframe-lib --test bwe_bench`
Expected: PASS, 3 tests.

If `estimate_rises_toward_capacity_on_a_clean_link` fails while the others
pass, that is the known slow-ramp signature of probe clusters not being
honoured — expected in Stage 1, since nothing sends probes yet. **Lower the
threshold to whatever the run actually achieves and leave a comment saying so,
citing Stage 2 as what raises it.** Do not silently delete the test.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/tests/bwe_bench.rs
git commit -m "test(bwe): tier-1 deterministic controller bench"
```

---

### Task 8: Ground-truth check against the netsim token bucket

The estimate has an independently known correct answer: the netsim's own bandwidth cap. This is the strongest available check because the target is not derived from the thing being tested.

**Files:**
- Modify: `ghostframe-e2e/src/harness/browserless.rs` (surface the estimate on `BrowserlessResult`)
- Modify: `ghostframe-e2e/tests/browserless_runner.rs`

- [ ] **Step 1: Write the failing test**

Add to `ghostframe-e2e/tests/browserless_runner.rs`:

```rust
/// The estimator has an independently known right answer here: the netsim's
/// own token bucket. Nothing else in the suite checks the estimate against a
/// target that was not derived from the estimator itself.
#[tokio::test(start_paused = true)]
async fn bwe_estimate_tracks_the_netsim_cap() {
    let scene = BrowserlessScene {
        seed: 0xB4E,
        frames: busy_frames(8),
        net: NetProfile {
            cap: CapTimeline::constant(1_000_000),
            ..NetProfile::perfect()
        },
        duration: Duration::from_secs(6),
        grid_cols: 4,
        grid_rows: 4,
    };
    let result = run_browserless(scene).await.expect("scene ran");

    let est = result.bwe_estimate_bps;
    assert!(
        est > 0,
        "seed 0xB4E: no bandwidth estimate was produced at all"
    );
    // Wide bounds deliberately: the harness is not seed-reproducible, so this
    // asserts the estimate is in the right order of magnitude for a 1 MB/s
    // (8 Mbit/s) cap, not a precise value.
    assert!(
        (1_000_000..=40_000_000).contains(&est),
        "seed 0xB4E: estimate {est} bps is not plausible for an 8 Mbit/s cap"
    );
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p ghostframe-e2e --test browserless_runner bwe_estimate -- --test-threads=1`
Expected: FAIL — `no field 'bwe_estimate_bps' on type 'BrowserlessResult'`.

- [ ] **Step 3: Surface the estimate**

`BrowserlessResult` in `ghostframe-e2e/src/harness/browserless.rs` gains:

```rust
    /// Server-side bandwidth estimate at the end of the scene, bits per
    /// second. Zero if the controller never produced one.
    pub bwe_estimate_bps: u64,
```

The runner cannot read the server's estimator directly — `IoBridge` owns it and
runs on its own task. Add a `pub fn bwe_estimate_bps(&self) -> u64` accessor on
`IoBridge` gated `#[cfg(any(test, feature = "browserless-harness"))]`, matching
how `now_std` is already gated for this harness, and read it after aborting the
bridge task. If the bridge handle makes that impossible, use an
`Arc<AtomicU64>` the bridge publishes into on each snapshot, and say in a
comment why the direct accessor was not usable.

- [ ] **Step 4: Run and watch it pass**

Run: `cargo test -p ghostframe-e2e --test browserless_runner -- --test-threads=1`
Expected: PASS, 10 tests (9 existing + this one).

- [ ] **Step 5: Commit**

```bash
git add ghostframe-e2e/src/harness/browserless.rs ghostframe-e2e/tests/browserless_runner.rs ghostframe-lib/src/transport/io_bridge.rs
git commit -m "test(netsim): assert the estimate tracks the simulator's own bandwidth cap"
```

---

### Task 9: Full verification and CI

**Files:** none — this task only runs things.

- [ ] **Step 1: Run the whole affected surface**

```bash
export TMPDIR=/home/cedric/.cache/ghostframe-tmp
cargo test -p ghostframe-lib --lib
cargo test -p ghostframe-lib --test bwe_bench
cargo test -p ghostframe-e2e --test netsim
cargo test -p ghostframe-e2e --test netsim_pump
cargo test -p ghostframe-e2e --test scene_tiles
cargo test -p ghostframe-e2e --test framebuffer
cargo test -p ghostframe-e2e --test browserless_runner -- --test-threads=1
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check --all
```

Expected: all pass. `netsim` 9, `netsim_pump` 5, `scene_tiles` 7, `framebuffer` 9 — unchanged from before this plan.

- [ ] **Step 2: Add the bench to CI**

`ghostframe-lib/tests/bwe_bench.rs` is a new integration-test target, and CI
runs integration tests **by explicit `--test` name** — `cargo test --workspace
--lib` does not reach them. Add it to the `unit` job in
`.github/workflows/ci.yml`, on the line after the existing
`cargo test --workspace --lib`:

```yaml
      # Integration-test targets are not covered by --lib and CI names them
      # explicitly; a new tests/*.rs file runs nowhere until it is listed here.
      - run: cargo test -p ghostframe-lib --test bwe_bench
```

- [ ] **Step 3: Verify the CI command locally**

Run: `cargo test -p ghostframe-lib --test bwe_bench`
Expected: PASS, 3 tests.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: run the tier-1 bandwidth-estimator bench"
```

---

## Done when

- `BweWrapper` is backed by `GoogCcNetworkController`; both `io_bridge` call sites are unchanged.
- The bench proves the estimate rises toward capacity, backs off below it after a step-down, and survives a 16-bit timestamp wrap.
- A scene asserts the estimate is plausible against the netsim's own cap.
- Emission behaviour is unchanged — no pacer work in this stage.

## Explicitly out of scope

Pacer restructure, priority queues, probe clusters, and `PacingMode` are Stage 2. Do not add them here, and do not add the `TransportConfig` flag yet: with the controller only observing, there is nothing to switch between, and a flag that selects between two identical behaviours is worse than no flag.
