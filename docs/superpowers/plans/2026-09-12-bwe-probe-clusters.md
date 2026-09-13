# BWE Probe Clusters Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Consume goog_cc's `probe_cluster_configs` so the estimator can discover headroom, instead of discarding them.

**Architecture:** One active probe window at a time. Inside it, the per-tick budget rises to the probe's target rate and emitted passes are tagged with the cluster id. The tag travels emit → cache → ACK — the path Stage 2.1 built for `queued_at` — and is finally attached to `SentPacket.pacing_info`, which is the only thing that makes a packet a probe as far as the estimator is concerned.

**Tech Stack:** Rust, `goog_cc` 0.1.4.

Design: `docs/superpowers/specs/2026-09-12-bwe-probe-clusters-design.md`. Read it first — particularly "No padding — and the consequence", which is the part most likely to be implemented as an accidental bug.

---

## The shapes this builds on

All three already exist after Stage 2.1/2.2:

```rust
pub struct CacheEntry {
    pub fragments: SmallVec<[Bytes; 2]>,
    pub wire_seqs: SmallVec<[u32; 2]>,
    pub queued_at: Instant,
    pub first_sent_at: Instant,
    pub last_sent_at: Instant,
    pub attempts: u8,
    pub rto_deadline: Instant,
}

struct BweSample {
    tier: PassTier,
    server_emit_us: u64,
    queued_since_epoch_us: u64,
    client_arrival_ms_lo16: u16,
    owd_ms_lo16: u16,
    size_bytes: u32,
    received_at: std::time::Instant,
}

pub struct AckArrival {
    pub wire_seq: u32,
    pub server_emit_us: u64,
    pub client_arrival_ms_lo16: u16,
    pub size_bytes: u32,
}
```

`queued_at` shows the pattern end to end. Follow it; do not invent a second mechanism.

## The failure this plan is designed against

A cluster that is requested, partially filled, and then discarded by the
estimator **fails silently**. `probe_bitrate_estimator.rs:137` drops any
cluster missing `min_probes` or `min_bytes`, and nothing reports it.

So "the config was read" and "the probe worked" are different claims, and only
the second matters. Every verification step below distinguishes them.

---

## Task 1: Surface `probe_cluster_configs` from the driver

**Files:**
- Modify: `ghostframe-lib/src/transport/bwe/googcc.rs`
- Modify: `ghostframe-lib/src/transport/bwe/mod.rs`

- [ ] **Step 1: Capture the configs in `absorb`**

`absorb` (`googcc.rs:~218`) reads `upd.target_rate` and, since Stage 2.2,
`upd.pacer_config`. Add `upd.probe_cluster_configs`.

It is a `Vec<ProbeClusterConfig>`; each carries `id`, `target_data_rate`,
`target_duration`, `min_probe_delta`, `target_probe_count`, `at_time`.

Store the **most recent** config. The design specifies one active probe at a
time: a new config replaces any pending one, because overlapping clusters
interleave their packets and corrupt both measurements.

- [ ] **Step 2: Define the request type**

Do not leak `goog_cc` types past the `bwe` module boundary — the existing
wrapper deliberately keeps them internal. Add a plain struct:

```rust
/// A probe goog_cc has asked for. Rates and durations are converted out of
/// goog_cc's units here so nothing downstream needs the crate's types.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeRequest {
    pub id: i32,
    pub target_rate_bps: u64,
    pub duration: Duration,
    /// From the config's `target_probe_count`. The estimator discards a
    /// cluster with fewer packets than this.
    pub min_probes: i64,
    /// `target_rate x duration`. goog_cc offers no helper for this — see
    /// `PacedPacketInfo::new`, which takes it as a plain argument — so the
    /// derivation is ours and is asserted in the bench.
    pub min_bytes: i64,
}
```

- [ ] **Step 3: Expose it**

Add a `take_probe_request()` on `BweWrapper` that returns and clears the
pending request, so a config is consumed once rather than re-triggering every
tick.

- [ ] **Step 4: Verify and commit**

```bash
cd /home/cedric/work/ghostframe
cargo build -p ghostframe-lib 2>&1 | tail -3
cargo clippy -p ghostframe-lib --all-targets 2>&1 | tail -3
cargo fmt -p ghostframe-lib
```

```bash
git add ghostframe-lib/src/transport/bwe/
git commit -m "bwe: surface goog_cc's probe cluster requests

absorb captured target_rate and pacer_config and dropped
probe_cluster_configs. ProbeRequest converts the units at the module
boundary so nothing downstream needs goog_cc's types."
```

## Task 2: The probe window

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Add the state**

```rust
/// The probe currently being filled, if any. One at a time: a new request
/// replaces this rather than queueing, because two clusters' packets
/// interleaving would corrupt both measurements.
struct ActiveProbe {
    id: i32,
    target_rate_bps: u64,
    ends_at: Instant,
    min_probes: i64,
    min_bytes: i64,
    /// Cumulative tagged bytes — goog_cc's `probe_cluster_bytes_sent`.
    bytes_sent: i64,
    /// Cumulative tagged packets, for the min-probes check at close.
    packets_sent: i64,
}
```

Plus two counters on `IoBridge`: `probes_completed` and `probes_abandoned`.

- [ ] **Step 2: Open a window when a request arrives**

Poll `take_probe_request()` where the BWE drain already runs in `run()`, and
set `ActiveProbe` with `ends_at = now + duration`.

- [ ] **Step 3: Close it, and account for the outcome**

When `now >= ends_at`, clear the probe and increment **exactly one** counter:

- `probes_completed` if `packets_sent >= min_probes && bytes_sent >= min_bytes`
- `probes_abandoned` otherwise

**This accounting is the point of the task.** The design's no-padding
decision means a probe can only fill if the queues held enough work, so an
idle link legitimately abandons probes. Without the counter, "probing works
and the link was idle" and "probing is broken" are indistinguishable — and
the second would never be noticed.

Log an abandoned probe at `debug`, with the shortfall, so it is diagnosable
without a debugger.

- [ ] **Step 4: Override the budget inside the window**

At both budget sites — `dispatch_dirty_tiles_via_scheduler` and
`apply_injected_frame`, the two Stage 2.2 touched — the tick budget becomes
the probe's target rate instead of `min(aimd, googcc)` while a probe is
active. Sending *above* the current estimate is what a probe is for.

**`clamp_to_quinn_capacity` still runs last and still wins.** A probe is not
worth dropping tiles for, and that clamp exists because `scheduler.tick` is
destructive.

**Do not generate padding.** If the queues are short, the probe under-fills
and is abandoned — that is the designed behaviour, not a gap to fill.

- [ ] **Step 5: Verify and commit**

```bash
cargo test -p ghostframe-lib 2>&1 | grep 'test result' | tail -4
cargo clippy -p ghostframe-lib --all-targets 2>&1 | tail -3
```

## Task 3: The tagging path

**Files:**
- Modify: `ghostframe-lib/src/transport/reliable_emitter/{cache.rs,emitter.rs}`
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`
- Modify: `ghostframe-lib/src/transport/bwe/mod.rs`

Mirror `queued_at` exactly. Four hops:

- [ ] **Step 1: `CacheEntry` carries the probe tag**

```rust
/// Probe cluster this pass was tagged for, if any. `None` for ordinary
/// traffic, which is the overwhelming majority.
pub probe: Option<ProbeTag>,
```

where `ProbeTag` is `{ id: i32, min_probes: i64, min_bytes: i64, bytes_sent_before: i64 }`.

`bytes_sent_before` is the cluster's cumulative byte count **at the moment
this packet was tagged** — that is goog_cc's `probe_cluster_bytes_sent`
semantics, not the final total. Getting this wrong is easy and silent.

- [ ] **Step 2: Stamp it at emit**

`submit_one` already takes `queued_at`; add the optional tag the same way and
update `submit_batch` and the ~20 mechanical test call sites (passing `None`
is fine wherever the distinction does not matter).

Advance the active probe's `bytes_sent` / `packets_sent` as each pass is
tagged.

- [ ] **Step 3: Carry it to the sample and the arrival**

`BweSample` gains `probe: Option<ProbeTag>`, read off the cache entry in the
ACK path. The `run()` drain copies it onto `AckArrival`.

- [ ] **Step 4: Build `PacedPacketInfo` in the driver**

`googcc.rs:~181` currently builds `SentPacket { ..Default::default() }`,
which leaves `probe_cluster_id` at `NOT_APROBE`. Set `pacing_info` from the
tag when present:

```rust
let pacing_info = match r.probe {
    Some(p) => {
        let mut info = PacedPacketInfo::new(p.id, p.min_probes, p.min_bytes);
        info.probe_cluster_bytes_sent = p.bytes_sent_before;
        info
    }
    None => PacedPacketInfo::default(), // probe_cluster_id = NOT_APROBE
};
```

**Untagged packets must keep `NOT_APROBE`.** `probe_bitrate_estimator.rs:88`
asserts on it, and mis-tagging ordinary traffic would feed the probe
estimator garbage.

- [ ] **Step 5: Verify and commit**

```bash
cargo test -p ghostframe-lib 2>&1 | grep 'test result' | tail -4
```

## Task 4: Bench coverage — the gate

**Files:**
- Modify: `ghostframe-lib/tests/bwe_bench.rs`

Deterministic and a pure function of its inputs, unlike the browserless
harness. This is where probing is actually verified.

- [ ] **Step 1: A cluster is emitted *and consumed***

Drive the bench until `ProbeController` requests a cluster, fill it, and
assert **the estimate moves**.

Asserting the config was read, or that packets were tagged, is not enough —
a cluster can be fully tagged and still discarded. The estimate moving is the
only evidence the estimator accepted it.

- [ ] **Step 2: An under-filled cluster is discarded and counted**

Same setup, but supply less than `min_bytes` of work. Assert
`probes_abandoned` incremented and `probes_completed` did not.

**Without this test, Step 1 could pass while every real probe silently
fails** — because a cluster that over-fills trivially satisfies both, and
nothing else distinguishes the two paths.

- [ ] **Step 3: `min_bytes` derivation is right**

Assert the derived `min_bytes` equals `target_rate x duration` for a known
config. The derivation is ours, not goog_cc's, and a silent error here makes
every cluster either trivially pass or never pass.

- [ ] **Step 4: Untagged traffic is unaffected**

Assert packets outside any window carry `NOT_APROBE` and the estimate still
tracks normally.

- [ ] **Step 5: Commit**

## Task 5: Regression guards and the measurement

- [ ] **Step 1: Full suites**

```bash
cd /home/cedric/work/ghostframe
cargo test -p ghostframe-lib 2>&1 | grep 'test result' | tail -4
cargo test -p ghostframe-e2e --test browserless_runner -- --test-threads=1 2>&1 | grep 'test result'
cargo clippy -p ghostframe-lib -p ghostframe-e2e --all-targets 2>&1 | tail -3
```

`cdf53_converges_to_lossless_under_10pct_loss` catches a probe that stalls
emission; `every_cdf53_pass_eventually_lands` catches one that starves
refinement. Both must stay green.

Run the browserless suite **several times**. It is not seed-reproducible, and
this repo has documented contention flakes — report the rate rather than a
single run.

- [ ] **Step 2: The guard metric**

Re-run the baseline per `docs/specs/bwe-tier-latency-baseline.md`.

**`last_sent_at → ACK` must not rise.** A probe deliberately overshoots the
estimate, making it the most plausible way to *cause* the wire queueing the
pacer exists to prevent. Reference after Stage 2.2 and the harness fix:
critical 26.4 ms, refinement 38.4 ms, ratio 0.688.

- [ ] **Step 3: Report probe outcomes**

Record `probes_completed` and `probes_abandoned` from a scene run. A run with
zero of both means the window never opened — report that rather than
treating it as a pass.

- [ ] **Step 4: Append to the baseline doc and commit**

---

## Done criteria

- [ ] `cargo test`, clippy, `fmt` clean.
- [ ] Bench proves a cluster is **consumed** (estimate moves), not merely read.
- [ ] Bench proves an **under-filled** cluster is discarded and counted.
- [ ] Untagged packets carry `NOT_APROBE`.
- [ ] Browserless guards green across repeated runs.
- [ ] `last_sent_at → ACK` did not rise.
- [ ] Probe outcome counters reported from a real scene.

## What this plan does not do

- Generate padding. Deliberate — see the design.
- Support concurrent clusters.
- Read `pad_window`, still unused since Stage 2.2.
- Touch tier ordering. Stage 2.3 is retired: the harness fidelity fix showed
  production already prioritises correctly.
