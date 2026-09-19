# Scheduler-Owned Repair — Phase 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop a late acknowledgement from stranding its cache entry forever, so the retransmission storm ends — without moving repair ownership.

**Architecture:** `TransmissionLedger::expire` currently deletes the `wire_seq → EmitKey` translation when it declares a transmission lost. Nothing re-establishes it, so an acknowledgement arriving afterwards resolves to nothing, `on_ack` never runs, and the emitter's cache entry becomes immortal. This phase keeps a bounded **tombstone** of that translation past expiry, so a late acknowledgement still releases its entry. The loss is still reported — what changes is that the entry is no longer stranded.

**Tech Stack:** Rust. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-18-scheduler-owned-repair-design.md`

---

## Why this phase alone fixes the production bug

Measured on the lossless 70 ms reproduction (`a_lossless_link_with_a_real_rtt_does_not_retransmit`, currently `#[ignore]`d):

| | |
|---|---|
| retransmissions on a link that drops nothing | **1788** |
| transmissions expired by the ledger | 1458 |
| of those, **acknowledged after expiry** | **1458 (100%)** |
| loss horizon in force | 236 ms |
| Cdf53 emit → ACK latency | p50 145 ms, **p90 251 ms** |

Every expired transmission was acknowledged afterwards. None was lost. The horizon simply sits below the acknowledgement distribution's upper decile, and losing that race is currently permanent.

## Correction to the spec's phasing

The spec lists "the loss deadline sized on `ack_p99`" in Phase 2 but puts `AckLatencyTracker` in Phase 3 — circular, since the deadline needs the measurement. **Resolved: deadline sizing moves to Phase 3 with the tracker.** Tombstones alone end the stranding, which is the production bug. Resizing the deadline only reduces how often the race is lost in the first place, and is an optimisation on top.

Versioned handles also move to Phase 3. They matter when the ledger points at slab entries, which is the ownership move; in this phase it still names content via `EmitKey`, and an `EmitKey` cannot be recycled the way a slab slot can.

---

## File structure

| file | responsibility |
|---|---|
| `ghostframe-lib/src/transport/transmission_ledger.rs` | tombstones, `Resolution`, the `acked_after_declared_lost` counter |
| `ghostframe-lib/src/transport/io_bridge.rs` | consume `Resolution`; release on a late acknowledgement without double-counting it in the estimator |
| `ghostframe-e2e/tests/browserless_runner.rs` | promote the storm reproduction to a gate |

---

## Task 1: Tombstones in the ledger

**Files:** `ghostframe-lib/src/transport/transmission_ledger.rs`

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn a_late_acknowledgement_still_resolves_after_expiry() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        let key = EmitKey::new(7, 1, 2, 0);
        l.record(99, t0, Transmission { emit_us: 10, wire_bytes: 500, key });

        let lost = l.expire(t0 + Duration::from_millis(300), Duration::from_millis(100));
        assert_eq!(lost.len(), 1, "the transmission is declared lost");

        match l.resolve(99) {
            Some(Resolution::Late(tx)) => assert_eq!(tx.key, key),
            other => panic!("a late ack must still resolve to its content, got {other:?}"),
        }
        assert_eq!(l.stats().acked_after_declared_lost, 1);
    }

    #[test]
    fn a_live_acknowledgement_resolves_as_live() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        let key = EmitKey::new(7, 1, 2, 0);
        l.record(99, t0, Transmission { emit_us: 10, wire_bytes: 500, key });
        match l.resolve(99) {
            Some(Resolution::Live(tx)) => assert_eq!(tx.key, key),
            other => panic!("expected Live, got {other:?}"),
        }
        assert_eq!(l.stats().acked_after_declared_lost, 0);
    }

    #[test]
    fn a_tombstone_resolves_only_once() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        l.record(99, t0, Transmission { emit_us: 10, wire_bytes: 500, key: EmitKey::new(7, 1, 2, 0) });
        let _ = l.expire(t0 + Duration::from_millis(300), Duration::from_millis(100));
        assert!(matches!(l.resolve(99), Some(Resolution::Late(_))));
        assert!(l.resolve(99).is_none(), "a duplicate ack must not resolve twice");
        assert_eq!(l.stats().acked_after_declared_lost, 1, "and must not double-count");
    }

    #[test]
    fn tombstones_are_bounded() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(8);
        for ws in 0..40u32 {
            l.record(ws, t0, Transmission { emit_us: 0, wire_bytes: 1, key: EmitKey::new(ws, 0, 0, 0) });
            let _ = l.expire(t0 + Duration::from_millis(300), Duration::from_millis(100));
        }
        assert!(
            l.tombstone_len() <= 8,
            "tombstones must be bounded by capacity; got {}",
            l.tombstone_len()
        );
        // The most recent expiry must still be resolvable -- eviction drops
        // the oldest, which is the one least likely to still be in flight.
        assert!(matches!(l.resolve(39), Some(Resolution::Late(_))));
    }

    #[test]
    fn an_unknown_wire_seq_still_resolves_to_nothing() {
        let mut l = TransmissionLedger::new(8);
        assert!(l.resolve(12345).is_none());
        assert_eq!(l.stats().unknown_acks, 1);
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p ghostframe-lib --lib transmission_ledger`
Expected: FAIL — `cannot find type Resolution`.

- [ ] **Step 3: Implement**

Add the resolution type:

```rust
/// What an acknowledgement turned out to refer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Outstanding when the acknowledgement arrived — the normal case.
    Live(Transmission),
    /// Already expired and reported to the estimator as lost, but
    /// acknowledged after all.
    ///
    /// The content still has to be released, or its cache entry is stranded
    /// forever: nothing else ever clears it, and it retransmits at the
    /// backoff ceiling for the rest of the session. The timing, however, must
    /// **not** be fed to the estimator — that transmission has already been
    /// accounted for as a loss, and counting it again would report the same
    /// bytes twice.
    Late(Transmission),
}
```

Add to `LedgerStats`:

```rust
    /// Acknowledgements that arrived after their transmission had been
    /// declared lost. A healthy link reads near zero; on the lossless 70 ms
    /// reproduction this read 1458 per 8-second scene, every one of which
    /// stranded a cache entry.
    pub acked_after_declared_lost: u64,
```

Add to `TransmissionLedger`:

```rust
    /// `wire_seq -> Transmission` for expired records, retained so a late
    /// acknowledgement can still release its content. Bounded by the same
    /// capacity as `records`; oldest evicted first.
    tombstones: HashMap<u32, Transmission>,
    tombstone_order: VecDeque<u32>,
```

Both initialised empty in `new`. Then:

```rust
    pub fn tombstone_len(&self) -> usize {
        self.tombstones.len()
    }

    pub fn resolve(&mut self, wire_seq: u32) -> Option<Resolution> {
        if let Some((_, tx)) = self.records.remove(&wire_seq) {
            return Some(Resolution::Live(tx));
        }
        if let Some(tx) = self.tombstones.remove(&wire_seq) {
            self.stats.acked_after_declared_lost += 1;
            return Some(Resolution::Late(tx));
        }
        self.stats.unknown_acks += 1;
        None
    }
```

In `expire`, where the record is currently removed and pushed to `lost`, also tombstone it:

```rust
                    self.order.pop_front();
                    if let Some((_, tx)) = self.records.remove(&front) {
                        self.tombstones.insert(front, tx.clone());
                        self.tombstone_order.push_back(front);
                        while self.tombstones.len() > self.capacity {
                            if let Some(oldest) = self.tombstone_order.pop_front() {
                                self.tombstones.remove(&oldest);
                            } else {
                                break;
                            }
                        }
                        lost.push(tx);
                    }
```

`Transmission` must derive `Clone` for this; add it if absent.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p ghostframe-lib --lib transmission_ledger`
Expected: PASS.

- [ ] **Step 5: Verify by mutation**

Delete the `self.tombstones.insert(...)` line and re-run. `a_late_acknowledgement_still_resolves_after_expiry` must fail. Restore it. Report both results — a tombstone test that passes without tombstones is worthless.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/src/transport/transmission_ledger.rs
git commit -m "feat(ledger): keep the translation alive past expiry

Expiry exists to report a loss, not to forget which pass a wire_seq
belonged to. Dropping the mapping means a late acknowledgement resolves
to nothing, on_ack never runs, and the emitter cache entry is immortal.
Bounded tombstones keep it resolvable; Resolution::Late marks it so the
caller can release the content without re-reporting timing the
estimator already counted as a loss."
```

## Task 2: Release on a late acknowledgement

> **AMENDED after Task 1.** Changing `resolve`'s return type breaks **three**
> pre-existing tests in `transmission_ledger.rs`, not just the `io_bridge.rs`
> caller. Task 1 was correctly forbidden from touching them; Task 2 owns them.
>
> Two are mechanical — they compare `resolve(...)` against a bare
> `Transmission`, which no longer type-checks:
>
> - `a_resolved_transmission_is_classified_once_and_not_expired` (~line 212)
> - `the_cap_evicts_oldest_first_and_counts` (~line 425)
>
> Wrap the expectation: `assert_eq!(l.resolve(7), Some(Resolution::Live(tx(7))))`.
> Change nothing else about them.
>
> The third is not mechanical. `an_acknowledgement_after_expiry_is_counted_not_retracted`
> asserts `l.resolve(7) == None` with `unknown_acks == 1` and the comment
> *"a late acknowledgement is visible, not silent"*. **It encodes the
> pre-tombstone behaviour as intended** — its author knew late acknowledgements
> happen and chose to make them countable without releasing the content.
>
> Its two real intents survive this phase and must be preserved:
> the late acknowledgement is still **visible** (now as
> `acked_after_declared_lost`, a more precise name than `unknown_acks`), and
> the loss is still **not retracted** to the estimator (`Resolution::Late`
> produces no `BweSample` — that is Step 1 of this task). What changes is that
> the content is now released instead of stranded. Rewrite it to say so:
>
> ```rust
>     /// A late acknowledgement is visible and does not retract the loss --
>     /// but it does release the content.
>     ///
>     /// Previously this asserted `resolve` returned `None`, which made the
>     /// acknowledgement countable while leaving its cache entry unreachable
>     /// forever. Visibility and non-retraction were the point and are kept;
>     /// the stranding was not, and is gone.
>     #[test]
>     fn an_acknowledgement_after_expiry_is_counted_and_releases_content() {
>         let t0 = Instant::now();
>         let mut l = TransmissionLedger::new(64);
>         l.record(7, t0, tx(7));
>         assert_eq!(
>             l.expire(t0 + Duration::from_millis(500), Duration::from_millis(100))
>                 .len(),
>             1
>         );
>         assert_eq!(
>             l.resolve(7),
>             Some(Resolution::Late(tx(7))),
>             "the content must still be releasable, or its cache entry is stranded"
>         );
>         assert_eq!(
>             l.stats().acked_after_declared_lost,
>             1,
>             "a late acknowledgement is visible, not silent"
>         );
>         assert_eq!(
>             l.stats().unknown_acks,
>             0,
>             "it is no longer an unknown acknowledgement -- we know exactly what it was"
>         );
>     }
> ```
>
> Task 1 could not run the real crate's tests at all, since it does not
> compile until this task lands. It verified its logic in a scratch copy
> instead. **Re-run the real suite here and report the true numbers** — the
> scratch result does not count as verification of what is in the tree.


**Files:** `ghostframe-lib/src/transport/io_bridge.rs` — the ACK resolution block (search `transmission_ledger.resolve`)

- [ ] **Step 1: Consume the new type**

The `filter_map` currently maps `resolve(...)` to `(tx.key, tx, arrival)`. It must now distinguish the two cases, because a `Late` resolution releases content but must not produce a `BweSample`. Carry the kind through:

```rust
                let resolved: Vec<(
                    crate::transport::reliable_emitter::EmitKey,
                    crate::transport::transmission_ledger::Transmission,
                    u16,
                    bool, // late: already reported lost, so no timing sample
                )> = batch
                    .entries
                    .iter()
                    .filter_map(|e| {
                        use crate::transport::transmission_ledger::Resolution;
                        match self.transmission_ledger.resolve(e.wire_seq) {
                            Some(Resolution::Live(tx)) => {
                                Some((tx.key, tx, e.arrival_time_ms_lo16, false))
                            }
                            Some(Resolution::Late(tx)) => {
                                Some((tx.key, tx, e.arrival_time_ms_lo16, true))
                            }
                            None => None,
                        }
                    })
                    .collect();
```

Update the destructuring at every use site. In the `BweSample` loop, skip late ones:

```rust
                for (emit_key, tx, arrival_lo16, late) in resolved.iter() {
                    if *late {
                        // Already counted as a loss when it expired. Feeding
                        // its timing now would report the same bytes twice.
                        continue;
                    }
```

`emit_keys` — which drives `on_ack`, and therefore the release — must include **both** kinds. That release is the whole point of the phase.

- [ ] **Step 2: Verify both suites are unchanged**

```bash
cargo test -p ghostframe-lib --lib
cargo test -p ghostframe-e2e --test browserless_runner
```

Expected: 427+ lib (Task 1 adds 5), browserless 18 passed, 0 failed, 2 ignored.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "fix(transport): release content on a late acknowledgement

A transmission acknowledged after expiry still has to clear its cache
entry -- nothing else ever does, so it retransmits at the backoff
ceiling until the session ends. Its timing is deliberately not fed to
the estimator, which already counted those bytes as lost."
```

## Task 3: Promote the storm reproduction to a gate

**Files:** `ghostframe-e2e/tests/browserless_runner.rs`

- [ ] **Step 1: Measure before changing the test**

```bash
cargo test -p ghostframe-e2e --test browserless_runner \
  a_lossless_link_with_a_real_rtt_does_not_retransmit -- --ignored --nocapture
```

Record the retransmit count. Before this phase it was **1788**. Report what you actually see.

- [ ] **Step 2: Remove the `#[ignore]`**

Delete the `#[ignore = "reproduces an open bug: 1776 spurious retransmissions on a lossless link"]` attribute, and rewrite the doc comment to describe what the test now guards rather than what it used to reproduce. Keep the existing `retransmit_attempts_total < 20` threshold — it was chosen as "a handful could be scheduling jitter; a storm cannot".

- [ ] **Step 3: Run it as a normal test**

Run: `cargo test -p ghostframe-e2e --test browserless_runner`
Expected: **19 passed, 0 failed, 1 ignored** — one more passing, one fewer ignored.

If the retransmit count is still high, **stop and report the number**. Do not raise the threshold. A number between 20 and 1788 means the tombstone helps but something else also strands entries, and that is a finding worth having.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-e2e/tests/browserless_runner.rs
git commit -m "test(browserless): the lossless-link storm is now a gate

Was #[ignore]d as an open bug at 1788 retransmissions on a link that
drops nothing. With late acknowledgements releasing their entries it
holds under 20."
```

## Task 4: Pin the invariant that was violated

**Files:** `ghostframe-lib/src/transport/transmission_ledger.rs`

The defect class was an entry reaching **no** terminal state. Pin it directly.

- [ ] **Step 1: Write the property test**

```rust
    /// Every recorded transmission must end in exactly one terminal state:
    /// acknowledged while live, acknowledged after expiry, or expired and
    /// never acknowledged. The bug this phase fixes was a fourth outcome --
    /// expired, then acknowledged, and silently dropped on the floor, leaving
    /// its cache entry unreachable forever.
    #[test]
    fn every_transmission_reaches_exactly_one_terminal_state() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(256);
        let n = 100u32;
        for ws in 0..n {
            l.record(ws, t0, Transmission { emit_us: ws, wire_bytes: 100, key: EmitKey::new(ws, 0, 0, 0) });
        }
        // Acknowledge a third while live.
        let mut live_acks = 0;
        for ws in (0..n).step_by(3) {
            if matches!(l.resolve(ws), Some(Resolution::Live(_))) {
                live_acks += 1;
            }
        }
        // Expire everything still outstanding.
        let expired = l.expire(t0 + Duration::from_millis(500), Duration::from_millis(100)).len() as u32;
        // Acknowledge everything again; the survivors resolve as Late.
        let mut late_acks = 0;
        for ws in 0..n {
            if matches!(l.resolve(ws), Some(Resolution::Late(_))) {
                late_acks += 1;
            }
        }
        assert_eq!(live_acks + expired, n, "every transmission was classified once");
        assert_eq!(
            late_acks, expired,
            "every expired transmission stayed resolvable, so none was stranded"
        );
        assert!(l.is_empty() && l.tombstone_len() == 0, "nothing left outstanding");
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test -p ghostframe-lib --lib every_transmission_reaches`
Expected: PASS.

- [ ] **Step 3: Verify it bites**

Delete the tombstone insert again; `late_acks` becomes 0 and the test must fail. Restore and report.

- [ ] **Step 4: Full verification and commit**

```bash
cargo test -p ghostframe-lib --lib
cargo test -p ghostframe-e2e --test browserless_runner
cargo fmt --all -- --check
cargo clippy -p ghostframe-lib -p ghostframe-e2e --all-targets -- -D warnings
```

```bash
git add ghostframe-lib/src/transport/transmission_ledger.rs
git commit -m "test(ledger): every transmission reaches exactly one terminal state

The defect was a fourth outcome -- expired, then acknowledged, then
dropped -- which left the cache entry unreachable forever."
```

---

## Done when

- [ ] A late acknowledgement resolves and releases its cache entry, verified by mutation.
- [ ] Tombstones are bounded, and the most recent expiry stays resolvable.
- [ ] A late acknowledgement produces no `BweSample` — its bytes were already counted as lost.
- [ ] The storm reproduction passes as a normal test: **under 20** retransmits on a lossless link, from 1788.
- [ ] `acked_after_declared_lost` is exposed, so the race's frequency is observable rather than inferred.
- [ ] Lib suite green, browserless **19 passed, 0 failed, 1 ignored**, fmt and clippy clean.

## Not in this plan

Phase 3 (ownership move: scheduler holds work until terminal, delete the emitter cache and RTO wheel, `AckLatencyTracker`, the gated safety sweep, versioned handles once the ledger names slab entries, and the loss deadline sized on `ack_p99`) and Phase 4 (retiring `fragment_coverage`).

The `order.retain(|w| *w != wire_seq)` on a duplicate `record` is O(n) and untouched here — `wire_seq` is allocated monotonically so it should be unreachable, but it is worth a look in Phase 3.
