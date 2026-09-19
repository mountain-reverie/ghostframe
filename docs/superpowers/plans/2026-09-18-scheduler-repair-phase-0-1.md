# Scheduler-Owned Repair — Phases 0 and 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the browserless harness deterministic drop injection, then replace the scheduler's linear queue scans with a dense slot array plus a versioned slab — all behind the existing `Scheduler` public API, with no behavioural change.

**Architecture:** `Scheduler` already owns `generations: Vec<u8>` indexed `tile_y * cols + tile_x`. Phase 1 widens that existing dense array into `Vec<TileSlot>` carrying `current_gen`, `acked_mask` and one handle per pass, and moves `TileWork` into a versioned slab so the queues hold cheap handles instead of values. Phase 0 comes first because the case the whole redesign protects — a tile the receiver never knew was sent — cannot be produced with probabilistic loss.

**Tech Stack:** Rust, tokio (`start_paused` virtual clock), the in-repo browserless harness and its `NetSim`. No new dependencies: the slab is ~60 lines of `Vec<Option<_>>` plus a free list.

**Spec:** `docs/superpowers/specs/2026-09-18-scheduler-owned-repair-design.md`

---

## Context the engineer needs

**Read before starting:** `docs/specs/retransmit-storm-root-cause.md` (why this work exists) and `docs/specs/is-rto-still-needed.md` (what repair mechanisms already exist).

**Three facts that shape every task here:**

1. `Scheduler::mark_acked` currently scans both queues linearly. It costs nothing *today* only because `drain_refinement_pass_major` removes entries from the queue at emit time — measured 2016 calls scanning 0 items. Phase 3 will hold work until ACK, which deletes that property. The index must land first.
2. `NetSim::decide`'s random-number draw order is load-bearing and documented as such (`ghostframe-e2e/src/netsim/mod.rs:210-226`). Any new drop rule must run **after** `decide`, converting a `Deliver` into a `Drop`, so the rng stream stays bit-identical.
3. The browserless harness runs under `#[tokio::test(start_paused = true)]`. Where an `await` sits decides when virtual time advances. Do not move awaits in the scene loop.

**Running tests:**

```bash
cargo test -p ghostframe-lib --lib                      # 406 tests, ~1s
cargo test -p ghostframe-e2e --test browserless_runner  # 17 pass + 2 ignored, ~45s
```

Both must stay green after every task in this plan. Phase 1 changes no behaviour; if a browserless test changes its result, the refactor is wrong — do not adjust the test.

---

## File structure

| file | responsibility |
|---|---|
| `ghostframe-lib/src/transport/scheduler.rs` | unchanged public API, queue orchestration; gains two `#[path]` submodule declarations |
| `ghostframe-lib/src/transport/scheduler/slab.rs` *(new)* | `Handle`, `Slab<T>` — versioned handles, free list, ABA rejection |
| `ghostframe-lib/src/transport/scheduler/slots.rs` *(new)* | `TileSlot`, `SlotMap` — dense per-tile state indexed `tile_y * cols + tile_x` |
| `ghostframe-e2e/src/netsim/drop_plan.rs` *(new)* | `DropPlan` — deterministic "drop the nth datagram matching this tile-pass" |
| `ghostframe-e2e/src/harness/browserless.rs` | `BrowserlessScene::drops` field; apply `DropPlan` in `rule()` |
| `ghostframe-e2e/tests/browserless_runner.rs` | the Phase 0 acceptance test |

`transport/mod.rs:17` already says `pub mod scheduler;`. The two new files live in a
`scheduler/` directory beside it and are declared from `scheduler.rs` with
`#[path = "scheduler/slab.rs"]`, so **the existing file is not moved** — keeping this
phase's diff confined to one file plus two additions.

---

# Phase 0 — deterministic drop injection

> ## CORRECTION (2026-09-18, after Task 2 review): the wiring point was wrong
>
> Tasks 1 and 2 hooked `DropPlan` into `rule()` in the browserless harness,
> which operates on **UDP payloads — QUIC packets**. Application tile datagrams
> ride inside QUIC DATAGRAM frames, encrypted. Reading byte 0 for
> `TILE_DATAGRAM_FLAG` and bytes 16/17 for tile coordinates is meaningless at
> that layer.
>
> Measured on `a_single_solid_tile_arrives_on_a_perfect_link`, a scene that
> does deliver a real tile: 20 server-to-client packets, and
> `is_tile_datagram` fires **exactly once** — on packet #1, `byte0=0xc3`,
> len 1200, the QUIC Initial. Every packet that could carry the tile is
> short-header, so bit 7 of byte 0 is the unprotected form bit and is always
> `0`.
>
> Two consequences:
> - **It can never drop a tile.** Structurally inert for its stated purpose.
> - **It can drop the handshake.** Long-header packets have bit 7 set, so a
>   rule whose coordinates happen to match the Initial's bytes 16/17 would eat
>   it *and report `drops() == [1]`* — manufacturing the very "the drop fired,
>   so my premise is real" signal, by destroying the connection.
>
> This is the failure mode this plan exists to prevent, produced by the plan
> itself. It survived two review stages because `drop_plan.rs`'s unit tests
> feed it synthetic tile datagrams built by hand, and no scene set a non-empty
> plan — so nothing exercised it against what the harness actually hands it.
>
> **Corrected design.** The only server-side plaintext seam is
> `IoBridge::send_to_all_sessions` (`ghostframe-lib/src/transport/io_bridge.rs:1513`),
> which takes the application datagram pre-encryption and *already* hosts this
> exact kind of hook for `outbound_loss`. `ghostframe-e2e` already enables
> `test-loss-injection` and `browserless-harness`, so the seam is reachable.
>
> `LossInjector` cannot be reused: its `DropPredicate` is
> `fn(&[u8]) -> bool` — stateless by type, so it cannot express "the Nth
> occurrence". `DropPlan` therefore moves to
> `ghostframe-lib/src/transport/drop_plan.rs`, beside `loss_injection.rs`,
> behind the same feature gates, held as `Option<Arc<Mutex<DropPlan>>>` so its
> counters survive `bridge` being moved into the spawned task — the same
> shared-cell pattern the harness already uses for `bwe_cell` and
> `emitter_stats_cell`.
>
> The `DropPlan` **type** is unchanged and its seven tests port verbatim. What
> changes is where it lives and where it is consulted. Task 2's
> `rule()` parameter, the `dir == Direction::S2c` gate, both call-site edits
> and the `&mut BrowserlessScene` widening are all reverted.
>
> **Superseding tasks: 2R (relocate + rehook) and 3 (unchanged in intent).**
> The task text below is retained as the record of what was tried.


## Task 1: `DropPlan` type

> **Corrected after review (2026-09-18).** The four tests prescribed below were
> partly vacuous and were replaced during execution. Mutation testing found three
> surviving mutants: deleting the tile-flag check, honouring only
> `occurrences.first()`, and an off-by-one in `MIN_TILE_LEN` each left the suite
> green. The root cause was that both payloads in `ignores_non_tile_datagrams`
> were shorter than `MIN_TILE_LEN`, so they exited at the length guard before the
> flag guard was ever evaluated — the test asserted a property it never reached.
> The committed implementation also imports `is_tile_datagram` and
> `DATAGRAM_HEADER_SIZE` from `ghostframe-protocol` instead of re-declaring the
> wire constants (per the convention `netsim/pump.rs` states), drops `Clone` from
> `DropPlan` because the occurrence counters are live state, and adds `drops()`
> so a scene can assert its injected drop actually fired. **The committed file is
> the source of truth for this task, not the code below.**


A `DropPlan` names datagrams to drop by their tile-pass identity and occurrence count, so a test can say "drop the first transmission of tile (2,3) pass 0" and get exactly that, every run.

Tile datagrams carry a known layout: byte 0 has `TILE_DATAGRAM_FLAG` (0x80) set, `tile_x` is at byte 16, `tile_y` at byte 17, and `TileHeader.codec` at byte 18 as `(codec << 1) | lz4`. The pass index is not in a fixed byte across codecs, so `DropPlan` matches on tile coordinates only — which is sufficient for the cases this plan needs and avoids depending on per-codec layout.

**Files:**
- Create: `ghostframe-e2e/src/netsim/drop_plan.rs`
- Modify: `ghostframe-e2e/src/netsim/mod.rs` (add `pub mod drop_plan;` and re-export)

- [ ] **Step 1: Write the failing test**

Create `ghostframe-e2e/src/netsim/drop_plan.rs` with only the tests at first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal tile datagram: flag bit in byte 0, tile_x at 16,
    /// tile_y at 17. Everything else is zero — `DropPlan` reads nothing else.
    fn tile_datagram(tile_x: u8, tile_y: u8) -> Vec<u8> {
        let mut v = vec![0u8; 20];
        v[0] = 0x80;
        v[16] = tile_x;
        v[17] = tile_y;
        v
    }

    #[test]
    fn drops_only_the_named_occurrence() {
        let mut plan = DropPlan::new(vec![DropRule {
            tile_x: 2,
            tile_y: 3,
            occurrences: vec![0],
        }]);
        let dg = tile_datagram(2, 3);
        assert!(plan.should_drop(&dg), "first occurrence must drop");
        assert!(!plan.should_drop(&dg), "second occurrence must pass");
        assert!(!plan.should_drop(&dg), "third occurrence must pass");
    }

    #[test]
    fn leaves_other_tiles_alone() {
        let mut plan = DropPlan::new(vec![DropRule {
            tile_x: 2,
            tile_y: 3,
            occurrences: vec![0],
        }]);
        assert!(!plan.should_drop(&tile_datagram(0, 0)));
        assert!(!plan.should_drop(&tile_datagram(2, 4)));
        // The named tile is still on its first occurrence.
        assert!(plan.should_drop(&tile_datagram(2, 3)));
    }

    #[test]
    fn ignores_non_tile_datagrams() {
        let mut plan = DropPlan::new(vec![DropRule {
            tile_x: 0,
            tile_y: 0,
            occurrences: vec![0],
        }]);
        // ACK/NACK envelopes do not set the tile flag; byte 0 is a message
        // type. A plan must never swallow one.
        let ack = vec![0x06u8, 0, 0, 0, 0, 0];
        assert!(!plan.should_drop(&ack));
        // Too short to carry tile coordinates.
        assert!(!plan.should_drop(&[0x80u8, 0, 0]));
    }

    #[test]
    fn an_empty_plan_drops_nothing() {
        let mut plan = DropPlan::default();
        assert!(!plan.should_drop(&tile_datagram(1, 1)));
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p ghostframe-e2e --lib drop_plan`
Expected: FAIL — `cannot find type DropPlan in this scope`.

- [ ] **Step 3: Implement `DropPlan`**

Put this above the `mod tests` block in the same file:

```rust
//! Deterministic drop injection for the browserless harness.
//!
//! `NetProfile::loss` is probabilistic, which cannot produce the case this
//! exists for: a tile whose *only* transmission is lost, so the receiver
//! never learns it was sent and can never NACK it. Reaching that case by
//! raising `loss` does not work — at the rates where it becomes likely the
//! scene stops establishing at all (measured: bails at 0.60 and 0.90).
//!
//! Applied *after* `NetSim::decide` so the rng stream is untouched; see
//! that function's doc comment on draw ordering.

use ghostframe_protocol::protocol::TILE_DATAGRAM_FLAG;

/// Byte offsets of the tile coordinates inside a tile datagram.
const TILE_X_OFFSET: usize = 16;
const TILE_Y_OFFSET: usize = 17;
/// Shortest payload that can carry both coordinates.
const MIN_TILE_LEN: usize = TILE_Y_OFFSET + 1;

/// Drop the given occurrences of datagrams carrying this tile.
///
/// `occurrences` are zero-based counts of matching datagrams seen so far:
/// `vec![0]` drops the first and lets every later one through, which is the
/// "last write lost, then static" case.
#[derive(Debug, Clone)]
pub struct DropRule {
    pub tile_x: u8,
    pub tile_y: u8,
    pub occurrences: Vec<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct DropPlan {
    rules: Vec<DropRule>,
    /// Matches seen so far per rule, parallel to `rules`.
    seen: Vec<u32>,
}

impl DropPlan {
    pub fn new(rules: Vec<DropRule>) -> Self {
        let seen = vec![0; rules.len()];
        Self { rules, seen }
    }

    /// True if this datagram should be dropped. Advances the per-rule
    /// occurrence counter for whichever rule matched.
    pub fn should_drop(&mut self, payload: &[u8]) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        if payload.len() < MIN_TILE_LEN || (payload[0] & TILE_DATAGRAM_FLAG) == 0 {
            return false;
        }
        let tx = payload[TILE_X_OFFSET];
        let ty = payload[TILE_Y_OFFSET];
        for (i, rule) in self.rules.iter().enumerate() {
            if rule.tile_x == tx && rule.tile_y == ty {
                let n = self.seen[i];
                self.seen[i] = n.saturating_add(1);
                return rule.occurrences.contains(&n);
            }
        }
        false
    }
}
```

Then add to `ghostframe-e2e/src/netsim/mod.rs`, next to the existing `pub mod profile;` declarations:

```rust
pub mod drop_plan;
pub use drop_plan::{DropPlan, DropRule};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p ghostframe-e2e --lib drop_plan`
Expected: PASS, 4 tests.

- [ ] **Step 5: Verify the offsets against a real datagram**

The offsets above are asserted, not assumed. Confirm them against the encoder:

Run: `grep -rn 'TILE_X_OFFSET\|tile_x' ghostframe-lib/src/transport/protocol.rs ghostframe-protocol/src/protocol.rs | head`

Expected: a `TileHeader` layout placing `tile_x`/`tile_y` at 16/17, consistent with `first_fragment_fp` in `ghostframe-lib/src/transport/reliable_emitter/emitter.rs:495`, which reads the codec at byte 18. If the offsets differ, fix the constants and re-run Step 4 — do not proceed with guessed offsets.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-e2e/src/netsim/drop_plan.rs ghostframe-e2e/src/netsim/mod.rs
git commit -m "test(harness): deterministic drop injection by tile and occurrence

NetProfile::loss is probabilistic and cannot produce the case the repair
redesign exists for -- a tile whose only transmission is lost, so the
receiver never learns it was sent. Raising loss until that becomes likely
does not work: the scene stops establishing first (bails at 0.60, 0.90)."
```

## Task 2: Wire `DropPlan` into the harness

**Files:**
- Modify: `ghostframe-e2e/src/harness/browserless.rs` — `BrowserlessScene` (line ~120), `rule()` (line ~1086)

- [ ] **Step 1: Add the scene field**

In `BrowserlessScene`, after the `net: NetProfile,` field:

```rust
    /// Deterministic drops, applied on top of `net`'s probabilistic loss.
    /// Server-to-client only. Empty by default, so existing scenes are
    /// bit-identical.
    pub drops: crate::netsim::DropPlan,
```

Every existing `BrowserlessScene { .. }` literal in `ghostframe-e2e/tests/browserless_runner.rs` must gain `drops: Default::default(),`. There are 18 of them; the compiler will name each one.

- [ ] **Step 2: Apply it after `decide`, never before**

Change `rule()`'s signature and its `Verdict::Deliver` arm only:

```rust
fn rule(
    sim: &mut NetSim,
    dir: Direction,
    payload: Vec<u8>,
    now_us_at_send: u64,
    bytes_dropped: &mut u64,
    drops: &mut crate::netsim::DropPlan,
) -> Vec<InFlight> {
```

```rust
        Verdict::Deliver { at_us } => {
            // Applied *after* `decide` so the rng draw order above is
            // untouched -- see `NetSim::decide`'s doc comment. A plan can
            // only turn a delivery into a drop, never the reverse.
            if matches!(dir, Direction::S2c) && drops.should_drop(&payload) {
                *bytes_dropped += payload.len() as u64;
                return Vec::new();
            }
            vec![at(at_us, payload)]
        }
```

Thread `drops` from the scene through both `rule(...)` call sites
(`browserless.rs:690` and `browserless.rs:882` — they currently pass different
argument counts, so read both). The direction enum is declared at
`browserless.rs:1034` with variants `C2s`/`S2c`; `matches!` avoids depending on
whether it derives `PartialEq`.

- [ ] **Step 3: Verify no existing scene changed**

Run: `cargo test -p ghostframe-e2e --test browserless_runner`
Expected: `17 passed; 0 failed; 2 ignored` — identical to before. An empty `DropPlan` returns early before touching the payload, so every existing scene must be unaffected. If any result moved, the plan is being applied before `decide` or in the wrong direction.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-e2e/src/harness/browserless.rs ghostframe-e2e/tests/browserless_runner.rs
git commit -m "test(harness): apply DropPlan to server-to-client datagrams

Applied after NetSim::decide so the documented rng draw order is
untouched: a plan can turn a delivery into a drop, never the reverse."
```

## Task 3: The acceptance test — and the experiment that resolves an open question

This test produces the exact case the redesign protects: a Solid tile, one datagram, sent once, deterministically dropped, then nothing further happens. No assembly exists on the client, no coverage entry exists, so **no NACK is possible**. Whatever renders that tile is a sender-side repair.

The spec records as carried uncertainty #1 that the scheduler's 2×RTT `InFlight` retry has never been observed firing end-to-end. **This test resolves it.** Record the outcome either way — it is the evidence Phase 3 depends on.

**Files:**
- Modify: `ghostframe-e2e/tests/browserless_runner.rs`

- [ ] **Step 1: Extend the existing import**

`ghostframe-e2e/tests/browserless_runner.rs:19` already reads
`use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, NetProfile};`. Widen it:

```rust
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, DropPlan, DropRule, NetProfile};
```

- [ ] **Step 2: Write the test**

```rust
/// The case no receiver-driven mechanism can cover: a tile emitted exactly
/// once, whose only datagram is dropped. The client builds no assembly and
/// no coverage entry, so it cannot NACK — it does not know the tile exists.
///
/// Only a sender-side repair can render this tile. That makes this test the
/// direct evidence for whether `drain_priority_queue`'s 2xRTT `InFlight`
/// retry actually fires end-to-end, which had never been observed when the
/// repair redesign was specified.
#[tokio::test(start_paused = true)]
async fn a_solid_tile_whose_only_datagram_is_dropped_is_still_repaired() {
    let scene = BrowserlessScene {
        seed: 0x0D30_0001,
        load: SceneLoad::Script(vec![FrameScript {
            tiles: vec![
                ((0, 0), TileSpec::Solid { bgra: [10, 20, 30, 255] }),
                ((1, 1), TileSpec::Solid { bgra: [40, 50, 60, 255] }),
            ],
        }]),
        cadence_us: DEFAULT_CADENCE_US,
        // Lossless apart from the one deliberate drop, so anything missing
        // is attributable to that drop alone.
        net: NetProfile::perfect(),
        drops: DropPlan::new(vec![DropRule {
            tile_x: 1,
            tile_y: 1,
            occurrences: vec![0],
        }]),
        duration: Duration::from_secs(5),
        grid_cols: 4,
        grid_rows: 4,
    };
    let result = run_browserless(scene).await.expect("scene ran");

    // Premise check: the injected drop must actually have fired. Without
    // this, the test passes when the drop silently never matched -- the
    // failure mode this whole plan exists to avoid, and the one that made an
    // entire earlier version of this feature inert.
    //
    // Use `drops_fired`, NOT `bytes_dropped`. The plan is consulted in
    // `IoBridge::send_to_all_sessions`, upstream of the netsim, so a
    // plan-dropped datagram never reaches the simulated link and is never
    // counted there. Measured on exactly this scene shape:
    // `drops_fired=[1] bytes_dropped=0`.
    assert_eq!(
        result.drops_fired,
        vec![1],
        "the injected drop never fired, so this test never created the case \
         it claims to test -- check the DropRule's coordinates against what \
         the scene actually emits"
    );

    // Control: the undropped tile proves the scene worked at all.
    assert!(
        result.framebuffer.tile_rgba(0, 0).is_some(),
        "control tile (0,0) never arrived -- the scene itself is broken, \
         so this test proves nothing about repair"
    );

    assert!(
        result.framebuffer.tile_rgba(1, 1).is_some(),
        "tile (1,1) had its only datagram dropped and was never repaired. \
         The client cannot NACK it: with nothing received it has no \
         assembly and no coverage entry, so it does not know the tile \
         exists. Only a sender-side repair can recover this."
    );
}
```

- [ ] **Step 3: Run it and record what happens**

Run: `cargo test -p ghostframe-e2e --test browserless_runner a_solid_tile_whose_only_datagram_is_dropped -- --nocapture`

This is an experiment, not a known-failing test. Both outcomes are informative:

- **PASS** — a sender-side repair fired. Confirm *which* by re-running with
  `GHOSTFRAME_RTO_PROBE=1 GHOSTFRAME_NO_RTO=1` (both env vars exist on this branch): if it still passes with the RTO timer disabled, the scheduler's 2×RTT retry is the mechanism and carried uncertainty #1 is resolved in the design's favour.
- **FAIL** — neither mechanism repairs this case. That contradicts the design's assumption and **must be reported before Phase 1 continues**; the spec's Phase 3 deletes the emitter timer on the strength of that path existing.

- [ ] **Step 4: Record the finding in the spec**

Append the measured outcome to the "Open uncertainties" section of `docs/superpowers/specs/2026-09-18-scheduler-owned-repair-design.md`, replacing uncertainty #1's text with what was observed, including which mechanism fired and whether it survived `GHOSTFRAME_NO_RTO=1`.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-e2e/tests/browserless_runner.rs docs/superpowers/specs/2026-09-18-scheduler-owned-repair-design.md
git commit -m "test(browserless): a tile the receiver cannot know about is repaired

Deterministically drops the only datagram of a Solid tile. With nothing
received the client has no assembly and no coverage entry, so no NACK is
possible -- only a sender-side repair can recover it. Resolves the
design's carried uncertainty about whether the scheduler's 2xRTT retry
fires end-to-end."
```

---

# Phase 1 — the index structure

## Task 4: Versioned slab

The ledger will outlive slab entries once it keeps tombstones (Phase 2), so a stale `wire_seq` could resolve to a recycled slot and acknowledge the wrong work. Versioned handles make that impossible rather than unlikely.

**Files:**
- Create: `ghostframe-lib/src/transport/scheduler/slab.rs`
- Modify: `ghostframe-lib/src/transport/scheduler.rs` → moved in Task 5; for now add `#[path = "scheduler/slab.rs"] mod slab;` at the top of `scheduler.rs`

- [ ] **Step 1: Write the failing tests**

Create `ghostframe-lib/src/transport/scheduler/slab.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_then_get_returns_the_value() {
        let mut slab: Slab<u32> = Slab::new();
        let h = slab.insert(42);
        assert_eq!(slab.get(h), Some(&42));
    }

    #[test]
    fn remove_frees_the_entry() {
        let mut slab: Slab<u32> = Slab::new();
        let h = slab.insert(42);
        assert_eq!(slab.remove(h), Some(42));
        assert_eq!(slab.get(h), None, "a removed handle must not resolve");
        assert_eq!(slab.remove(h), None, "double remove must be a no-op");
    }

    /// The ABA hazard: a stale handle must never resolve to whatever now
    /// occupies its index. This is the property the transmission ledger
    /// depends on once it keeps tombstones past entry lifetime.
    #[test]
    fn a_stale_handle_never_resolves_to_the_slots_new_occupant() {
        let mut slab: Slab<u32> = Slab::new();
        let old = slab.insert(1);
        slab.remove(old);
        let new = slab.insert(2);
        assert_eq!(new.index, old.index, "test is vacuous unless the slot is reused");
        assert_ne!(new.version, old.version, "reuse must bump the version");
        assert_eq!(slab.get(old), None, "stale handle resolved to the new occupant");
        assert_eq!(slab.get(new), Some(&2));
    }

    #[test]
    fn len_counts_live_entries_only() {
        let mut slab: Slab<u32> = Slab::new();
        assert_eq!(slab.len(), 0);
        let a = slab.insert(1);
        let _b = slab.insert(2);
        assert_eq!(slab.len(), 2);
        slab.remove(a);
        assert_eq!(slab.len(), 1);
    }

    #[test]
    fn clear_drops_everything_and_invalidates_handles() {
        let mut slab: Slab<u32> = Slab::new();
        let h = slab.insert(7);
        slab.clear();
        assert_eq!(slab.len(), 0);
        assert_eq!(slab.get(h), None);
    }
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p ghostframe-lib --lib slab::tests`
Expected: FAIL — `cannot find type Slab in this scope`.

- [ ] **Step 3: Implement the slab**

Above `mod tests` in the same file:

```rust
//! A versioned slab: stable handles into a `Vec`, with reuse detection.
//!
//! Handles must be versioned because the transmission ledger keeps
//! `wire_seq -> Handle` tombstones that outlive the entries they name. An
//! unversioned index would let a late acknowledgement for a long-gone
//! transmission resolve to whatever work now occupies that slot, and
//! silently acknowledge the wrong tile.

/// A stable reference to a slab entry. Copy, 8 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle {
    pub index: u32,
    pub version: u32,
}

#[derive(Debug)]
struct Entry<T> {
    /// Even = vacant, odd = occupied. Incrementing on both insert and
    /// remove means a handle taken before a remove can never match after.
    version: u32,
    value: Option<T>,
}

#[derive(Debug)]
pub struct Slab<T> {
    entries: Vec<Entry<T>>,
    free: Vec<u32>,
    live: usize,
}

impl<T> Default for Slab<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Slab<T> {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            free: Vec::new(),
            live: 0,
        }
    }

    pub fn insert(&mut self, value: T) -> Handle {
        self.live += 1;
        if let Some(index) = self.free.pop() {
            let e = &mut self.entries[index as usize];
            e.version = e.version.wrapping_add(1);
            e.value = Some(value);
            return Handle {
                index,
                version: e.version,
            };
        }
        let index = self.entries.len() as u32;
        self.entries.push(Entry {
            version: 1,
            value: Some(value),
        });
        Handle { index, version: 1 }
    }

    fn entry(&self, h: Handle) -> Option<&Entry<T>> {
        let e = self.entries.get(h.index as usize)?;
        (e.version == h.version).then_some(e)
    }

    pub fn get(&self, h: Handle) -> Option<&T> {
        self.entry(h)?.value.as_ref()
    }

    pub fn get_mut(&mut self, h: Handle) -> Option<&mut T> {
        let e = self.entries.get_mut(h.index as usize)?;
        if e.version != h.version {
            return None;
        }
        e.value.as_mut()
    }

    pub fn remove(&mut self, h: Handle) -> Option<T> {
        let e = self.entries.get_mut(h.index as usize)?;
        if e.version != h.version {
            return None;
        }
        let value = e.value.take()?;
        e.version = e.version.wrapping_add(1);
        self.free.push(h.index);
        self.live -= 1;
        Some(value)
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn clear(&mut self) {
        for (i, e) in self.entries.iter_mut().enumerate() {
            if e.value.take().is_some() {
                e.version = e.version.wrapping_add(1);
                self.free.push(i as u32);
            }
        }
        self.live = 0;
    }
}
```

Add at the top of `ghostframe-lib/src/transport/scheduler.rs`, below the existing `use` block:

```rust
#[path = "scheduler/slab.rs"]
pub mod slab;
pub use slab::{Handle, Slab};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p ghostframe-lib --lib slab::tests`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/scheduler/slab.rs ghostframe-lib/src/transport/scheduler.rs
git commit -m "feat(scheduler): versioned slab with ABA rejection

Handles are versioned because the transmission ledger will keep
wire_seq -> Handle tombstones that outlive their entries. An unversioned
index would let a late ACK resolve to whatever now occupies the slot and
acknowledge the wrong tile."
```

## Task 5: Prove the one-handle-per-(tile, pass) invariant before relying on it

`TileSlot` will hold exactly one handle per `(tile, pass)`. That is only sound if two live work items for the same tile-pass never coexist. Measure it rather than assume it — this is cheap, and assuming it would corrupt delivery state under a case nobody tested.

**Files:**
- Modify: `ghostframe-lib/src/transport/scheduler.rs` (`enqueue_at` ~line 208, `enqueue_refinement_work_at` ~line 231)

- [ ] **Step 1: Add a temporary duplicate detector**

In both `enqueue_at` and `enqueue_refinement_work_at`, immediately after the two `debug_assert!` bounds checks:

```rust
        #[cfg(debug_assertions)]
        {
            let dup = self
                .priority_queue
                .iter()
                .chain(self.refinement_queue.iter())
                .any(|w| {
                    w.tile_x == work.tile_x
                        && w.tile_y == work.tile_y
                        && w.pass_idx == work.pass_idx
                        && matches!(w.state, WorkState::Pending | WorkState::InFlight)
                });
            assert!(
                !dup,
                "two live work items for tile ({},{}) pass {} -- TileSlot \
                 assumes one handle per (tile, pass)",
                work.tile_x, work.tile_y, work.pass_idx
            );
        }
```

- [ ] **Step 2: Run everything**

```bash
cargo test -p ghostframe-lib --lib
cargo test -p ghostframe-e2e --test browserless_runner
```

Expected: all green, no assertion fires.

**If the assertion fires:** the invariant does not hold. Stop and report which caller produced the duplicate. `TileSlot.passes` would then need a small `SmallVec<[Handle; 2]>` per pass instead of `Option<Handle>`, and Tasks 6–8 must be adjusted before proceeding. Do not silently widen it without reporting.

- [ ] **Step 3: Remove the detector, keep the finding**

Delete both inserted blocks. Record the result in the commit message — the invariant is now evidence, not assumption.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/transport/scheduler.rs
git commit -m "test(scheduler): verify one live work item per (tile, pass)

Ran a temporary debug assertion across the lib suite and the browserless
suite: no caller ever enqueues two live work items for the same tile and
pass. TileSlot can therefore hold Option<Handle> per pass rather than a
collection. Removed the assertion; recording the evidence here."
```

## Task 6: `TileSlot` — fold the ACK bitmap into the existing dense array

`Scheduler` already has `generations: Vec<u8>` indexed `tile_y * cols + tile_x`. This widens it into `Vec<TileSlot>` and deletes the `cdf53_passes_acked` HashMap along with the full-map scan `bump_generation` does per bump.

**Files:**
- Create: `ghostframe-lib/src/transport/scheduler/slots.rs`
- Modify: `ghostframe-lib/src/transport/scheduler.rs`

- [ ] **Step 1: Write the failing tests**

Create `ghostframe-lib/src/transport/scheduler/slots.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_is_row_major() {
        let map = SlotMap::new(4, 3);
        assert_eq!(map.index(0, 0), Some(0));
        assert_eq!(map.index(3, 0), Some(3));
        assert_eq!(map.index(0, 1), Some(4));
        assert_eq!(map.index(3, 2), Some(11));
        assert_eq!(map.index(4, 0), None, "x out of range");
        assert_eq!(map.index(0, 3), None, "y out of range");
    }

    #[test]
    fn acking_sets_only_that_pass() {
        let mut map = SlotMap::new(2, 2);
        map.record_ack(1, 1, 0, 3);
        assert_eq!(map.acked_mask(1, 1, 0), 0b1000);
        map.record_ack(1, 1, 0, 0);
        assert_eq!(map.acked_mask(1, 1, 0), 0b1001);
    }

    #[test]
    fn an_ack_for_a_stale_generation_is_ignored() {
        let mut map = SlotMap::new(2, 2);
        map.record_ack(0, 0, 0, 1);
        map.bump_generation(0, 0);
        assert_eq!(map.acked_mask(0, 0, 1), 0, "bump clears the mask");
        map.record_ack(0, 0, 0, 2);
        assert_eq!(
            map.acked_mask(0, 0, 1),
            0,
            "an ack naming the old generation must not touch the new one"
        );
    }

    #[test]
    fn bump_advances_the_generation_and_wraps_at_four_bits() {
        let mut map = SlotMap::new(1, 1);
        assert_eq!(map.generation(0, 0), 0);
        for expected in 1..=15u8 {
            assert_eq!(map.bump_generation(0, 0), expected);
        }
        assert_eq!(map.bump_generation(0, 0), 0, "4-bit generation wraps");
    }

    #[test]
    fn fully_acked_needs_every_pass_below_max() {
        let mut map = SlotMap::new(1, 1);
        for p in 0..13u8 {
            map.record_ack(0, 0, 0, p);
        }
        assert!(!map.fully_acked(0, 0, 0, 14));
        map.record_ack(0, 0, 0, 13);
        assert!(map.fully_acked(0, 0, 0, 14));
    }

    #[test]
    fn unacked_mask_is_the_complement_below_max() {
        let mut map = SlotMap::new(1, 1);
        assert_eq!(map.unacked_mask(0, 0, 0, 14), 0x3FFF);
        map.record_ack(0, 0, 0, 0);
        assert_eq!(map.unacked_mask(0, 0, 0, 14), 0x3FFE);
    }

    #[test]
    fn handles_are_stored_and_retrieved_per_pass() {
        let mut map = SlotMap::new(1, 1);
        let h = crate::transport::scheduler::Handle { index: 7, version: 1 };
        map.set_handle(0, 0, 2, Some(h));
        assert_eq!(map.handle(0, 0, 2), Some(h));
        assert_eq!(map.handle(0, 0, 3), None, "other passes are unaffected");
        map.set_handle(0, 0, 2, None);
        assert_eq!(map.handle(0, 0, 2), None);
    }
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p ghostframe-lib --lib slots::tests`
Expected: FAIL — `cannot find type SlotMap in this scope`.

- [ ] **Step 3: Implement `SlotMap`**

Above `mod tests`:

```rust
//! Dense per-tile delivery state, indexed `tile_y * cols + tile_x`.
//!
//! Replaces `Scheduler::generations: Vec<u8>` and the
//! `cdf53_passes_acked: HashMap<(u8, u8, u8), u16>` it sat beside. Folding
//! the ACK bitmap into the slot removes the full-map scan `bump_generation`
//! did on every bump, and makes an ACK for a stale generation a comparison
//! rather than a lookup.
//!
//! At most one generation per tile is live — `bump_generation` supersedes
//! the rest — so a single `current_gen` plus one mask is sufficient.

use crate::transport::scheduler::Handle;

/// Cdf53 emits 14 progressive passes; single-pass codecs use index 0.
pub const PASS_SLOTS: usize = 14;

/// Generations are 4 bits on the wire.
const GENERATION_MASK: u8 = 0x0F;

#[derive(Debug, Clone)]
pub struct TileSlot {
    pub current_gen: u8,
    pub acked_mask: u16,
    pub passes: [Option<Handle>; PASS_SLOTS],
}

impl Default for TileSlot {
    fn default() -> Self {
        Self {
            current_gen: 0,
            acked_mask: 0,
            passes: [None; PASS_SLOTS],
        }
    }
}

#[derive(Debug)]
pub struct SlotMap {
    cols: u32,
    rows: u32,
    slots: Vec<TileSlot>,
}

impl SlotMap {
    pub fn new(cols: u32, rows: u32) -> Self {
        Self {
            cols,
            rows,
            slots: vec![TileSlot::default(); (cols as usize) * (rows as usize)],
        }
    }

    pub fn resize(&mut self, cols: u32, rows: u32) {
        self.cols = cols;
        self.rows = rows;
        self.slots = vec![TileSlot::default(); (cols as usize) * (rows as usize)];
    }

    pub fn index(&self, tile_x: u8, tile_y: u8) -> Option<usize> {
        if (tile_x as u32) >= self.cols || (tile_y as u32) >= self.rows {
            return None;
        }
        Some((tile_y as usize) * (self.cols as usize) + (tile_x as usize))
    }

    fn slot(&self, tile_x: u8, tile_y: u8) -> Option<&TileSlot> {
        self.slots.get(self.index(tile_x, tile_y)?)
    }

    fn slot_mut(&mut self, tile_x: u8, tile_y: u8) -> Option<&mut TileSlot> {
        let i = self.index(tile_x, tile_y)?;
        self.slots.get_mut(i)
    }

    pub fn generation(&self, tile_x: u8, tile_y: u8) -> u8 {
        self.slot(tile_x, tile_y).map_or(0, |s| s.current_gen)
    }

    /// Advance the generation and clear all delivery state for the tile.
    /// Returns the new generation.
    pub fn bump_generation(&mut self, tile_x: u8, tile_y: u8) -> u8 {
        match self.slot_mut(tile_x, tile_y) {
            Some(s) => {
                s.current_gen = (s.current_gen.wrapping_add(1)) & GENERATION_MASK;
                s.acked_mask = 0;
                s.current_gen
            }
            None => 0,
        }
    }

    /// Record an acknowledgement. An ack naming a generation other than the
    /// tile's current one is silently ignored: it describes content that has
    /// already been superseded.
    pub fn record_ack(&mut self, tile_x: u8, tile_y: u8, generation: u8, pass_idx: u8) {
        debug_assert!(pass_idx < 16, "pass_idx {pass_idx} out of bitmap range");
        if let Some(s) = self.slot_mut(tile_x, tile_y) {
            if s.current_gen == generation {
                s.acked_mask |= 1u16 << (pass_idx & 0x0F);
            }
        }
    }

    /// True if this acknowledgement is new (not already recorded). Callers
    /// use it to avoid counting duplicates toward the delivery window.
    pub fn is_new_ack(&self, tile_x: u8, tile_y: u8, generation: u8, pass_idx: u8) -> bool {
        match self.slot(tile_x, tile_y) {
            Some(s) if s.current_gen == generation => {
                (s.acked_mask & (1u16 << (pass_idx & 0x0F))) == 0
            }
            _ => false,
        }
    }

    pub fn acked_mask(&self, tile_x: u8, tile_y: u8, generation: u8) -> u16 {
        match self.slot(tile_x, tile_y) {
            Some(s) if s.current_gen == generation => s.acked_mask,
            _ => 0,
        }
    }

    pub fn acked_count(&self, tile_x: u8, tile_y: u8, generation: u8) -> u8 {
        self.acked_mask(tile_x, tile_y, generation).count_ones() as u8
    }

    fn full_mask(max_passes: u8) -> u16 {
        if max_passes >= 16 {
            0xFFFF
        } else {
            (1u16 << max_passes) - 1
        }
    }

    pub fn fully_acked(&self, tile_x: u8, tile_y: u8, generation: u8, max_passes: u8) -> bool {
        let needed = Self::full_mask(max_passes);
        (self.acked_mask(tile_x, tile_y, generation) & needed) == needed
    }

    pub fn unacked_mask(&self, tile_x: u8, tile_y: u8, generation: u8, max_passes: u8) -> u16 {
        Self::full_mask(max_passes) & !self.acked_mask(tile_x, tile_y, generation)
    }

    pub fn handle(&self, tile_x: u8, tile_y: u8, pass_idx: u8) -> Option<Handle> {
        self.slot(tile_x, tile_y)?
            .passes
            .get(pass_idx as usize)
            .copied()
            .flatten()
    }

    pub fn set_handle(&mut self, tile_x: u8, tile_y: u8, pass_idx: u8, h: Option<Handle>) {
        if let Some(s) = self.slot_mut(tile_x, tile_y) {
            if let Some(cell) = s.passes.get_mut(pass_idx as usize) {
                *cell = h;
            }
        }
    }

    /// Drop all queued handles but keep generations and ACK state, matching
    /// `Scheduler::clear`'s documented contract: a late ACK on a stale
    /// (tile, gen) must remain a safe no-op.
    pub fn clear_handles(&mut self) {
        for s in self.slots.iter_mut() {
            s.passes = [None; PASS_SLOTS];
        }
    }
}
```

Declare it in `scheduler.rs` next to the slab declaration:

```rust
#[path = "scheduler/slots.rs"]
pub mod slots;
pub use slots::{SlotMap, TileSlot, PASS_SLOTS};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p ghostframe-lib --lib slots::tests`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/scheduler/slots.rs ghostframe-lib/src/transport/scheduler.rs
git commit -m "feat(scheduler): dense per-tile slots carrying generation and ack mask

Widens the existing generations: Vec<u8> into Vec<TileSlot>. Folding the
ack bitmap into the slot removes the full cdf53_passes_acked scan that
bump_generation ran on every bump, and makes a stale-generation ack a
comparison instead of a map lookup."
```

## Task 7: Migrate `Scheduler` onto `SlotMap`

Replaces `generations` and `cdf53_passes_acked` with a single `SlotMap`. The public API does not change; every existing test must pass untouched.

**Files:**
- Modify: `ghostframe-lib/src/transport/scheduler.rs`

- [ ] **Step 1: Swap the fields**

In the `Scheduler` struct, delete `generations: Vec<u8>` and `cdf53_passes_acked: HashMap<(u8, u8, u8), u16>`, and add:

```rust
    slots: SlotMap,
```

In `new`: replace both initialisers with `slots: SlotMap::new(cols, rows),`.
In `resize`: replace the `generations`/`cdf53_passes_acked` lines with `self.slots.resize(cols, rows);`.
In `clear`: add `self.slots.clear_handles();` and keep the existing comment about late ACKs being safe no-ops.

- [ ] **Step 2: Rewrite the accessors as delegations**

```rust
    pub fn generation_for(&self, tile_x: u8, tile_y: u8) -> u8 {
        self.slots.generation(tile_x, tile_y)
    }

    pub fn record_cdf53_ack(&mut self, tile_x: u8, tile_y: u8, generation: u8, pass_idx: u8) {
        // Only newly-acked passes count toward the AIMD delivery window;
        // duplicates reflect no new wire delivery and would distort the
        // bandwidth-budget heuristic.
        if self.slots.is_new_ack(tile_x, tile_y, generation, pass_idx) {
            self.delivery_window_acked = self.delivery_window_acked.saturating_add(1);
        }
        self.slots.record_ack(tile_x, tile_y, generation, pass_idx);
    }

    pub fn tile_fully_acked(&self, tile_x: u8, tile_y: u8, generation: u8, max_passes: u8) -> bool {
        self.slots.fully_acked(tile_x, tile_y, generation, max_passes)
    }

    pub fn cdf53_passes_acked_count(&self, tile_x: u8, tile_y: u8, generation: u8) -> u8 {
        self.slots.acked_count(tile_x, tile_y, generation)
    }

    pub fn cdf53_unacked_pass_mask(
        &self,
        tile_x: u8,
        tile_y: u8,
        generation: u8,
        max_passes: u8,
    ) -> u16 {
        self.slots.unacked_mask(tile_x, tile_y, generation, max_passes)
    }

    pub fn cdf53_unacked_tiles_for_gen(&self, candidates: &[((u8, u8), u8, u8)]) -> Vec<(u8, u8)> {
        candidates
            .iter()
            .filter_map(|&((tx, ty), gen, max_passes)| {
                (self.slots.acked_count(tx, ty, gen) < max_passes).then_some((tx, ty))
            })
            .collect()
    }

    #[cfg(test)]
    pub fn cdf53_passes_acked_for_test(&self, tile_x: u8, tile_y: u8, generation: u8) -> u8 {
        self.slots.acked_count(tile_x, tile_y, generation)
    }
```

- [ ] **Step 3: Rewrite `bump_generation`**

The full-map `retain` disappears — clearing the mask is part of `SlotMap::bump_generation`:

```rust
    pub fn bump_generation(&mut self, tile_x: u8, tile_y: u8) -> u8 {
        // Any queued work for this tile is now stale; mark Superseded so the
        // next tick drops it. Acked entries don't matter (already done).
        self.supersede_pending_for_tile(tile_x, tile_y);
        // Per-tile ACK state is cleared by the bump itself; there is no
        // longer a map to scan.
        self.slots.bump_generation(tile_x, tile_y)
    }
```

- [ ] **Step 4: Run every test**

```bash
cargo test -p ghostframe-lib --lib
cargo test -p ghostframe-e2e --test browserless_runner
```

Expected: `406 passed` and `17 passed; 0 failed; 2 ignored` — unchanged.

Two behavioural details to check if anything fails:
- `bump_generation` previously wrapped via `generations[i] = (g + 1) & 0x0F`. `SlotMap::bump_generation` does the same; confirm against `scheduler.rs`'s original if a generation-related test fails.
- The old `cdf53_passes_acked` retained rows for *other* generations until a bump. Nothing reads a non-current generation's mask (`tile_fully_acked`, `cdf53_unacked_pass_mask` and `cdf53_unacked_tiles_for_gen` are all called with the tile's current generation), which is why one mask suffices. If a test disagrees, that assumption is wrong — report it rather than adding a second mask.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/scheduler.rs
git commit -m "refactor(scheduler): replace generations + cdf53_passes_acked with SlotMap

No behavioural change: 406 lib tests and browserless 17/17 unchanged.
Deletes the full-HashMap retain that bump_generation ran per bump."
```

## Task 8: The bounded-scan test for `mark_acked`

Written before the optimisation so it fails for the right reason. It must be a direct unit test: measuring this through a scene would be vacuous, because today `drain_refinement_pass_major` empties the queue at emit and `mark_acked` scans nothing.

**Files:**
- Modify: `ghostframe-lib/src/transport/scheduler.rs`

- [ ] **Step 1: Write the failing test**

Add a counter next to the other `Scheduler` fields:

```rust
    /// Work items examined by `mark_acked`. Test-only observability: the
    /// index exists so this stays flat as the queue grows, and a linear
    /// scan returning here is a silent O(n^2) regression.
    #[cfg(test)]
    pub(crate) mark_acked_comparisons: std::cell::Cell<u64>,
```

Initialise it in `new`. A `#[cfg(test)]` field needs the attribute repeated on its
initialiser, or non-test builds fail to compile with a missing-field error:

```rust
        Self {
            // ... existing fields
            #[cfg(test)]
            mark_acked_comparisons: std::cell::Cell::new(0),
        }
```

`resize` and `clear` leave it alone.

Add the test to `scheduler.rs`'s `mod tests`:

```rust
    /// `mark_acked` must not scan the queue. It is cheap today only because
    /// refinement work is removed from the queue at emit time; the repair
    /// redesign holds work until it is acknowledged, at which point a linear
    /// scan becomes O(n^2) over the whole session.
    #[test]
    fn mark_acked_cost_does_not_grow_with_queue_depth() {
        fn comparisons_with_queue_depth(depth: u8) -> u64 {
            let mut s = Scheduler::new(16, 16);
            let now = Instant::now();
            for i in 0..depth {
                let mut w = TileWork::raw_for_test(i % 16, i / 16, 0, vec![0u8; 8]);
                w.pass_idx = 0;
                s.enqueue_at(w, now);
            }
            // Acknowledge the tile enqueued first, i.e. the worst case for a
            // scan that walks from the front.
            s.mark_acked_comparisons.set(0);
            s.mark_acked(0, 0, 0, 0);
            s.mark_acked_comparisons.get()
        }

        let shallow = comparisons_with_queue_depth(4);
        let deep = comparisons_with_queue_depth(200);
        assert!(
            deep <= shallow + 2,
            "mark_acked examined {deep} items at depth 200 versus {shallow} at \
             depth 4 -- it is scanning the queue, which is O(n^2) once work is \
             held until acknowledged"
        );
    }
```

Instrument the existing loop so the counter is real:

```rust
    pub fn mark_acked(&mut self, tile_x: u8, tile_y: u8, generation: u8, pass_idx: u8) {
        for work in self
            .priority_queue
            .iter_mut()
            .chain(self.refinement_queue.iter_mut())
        {
            #[cfg(test)]
            self.mark_acked_comparisons
                .set(self.mark_acked_comparisons.get() + 1);
            // ... existing body unchanged
```

Note: the `#[cfg(test)]` line borrows `self` inside a loop that holds `&mut self.priority_queue`. Hoist the counter out of the borrow by taking a reference before the loop:

```rust
    pub fn mark_acked(&mut self, tile_x: u8, tile_y: u8, generation: u8, pass_idx: u8) {
        #[cfg(test)]
        let counter = &self.mark_acked_comparisons;
        for work in self
            .priority_queue
            .iter_mut()
            .chain(self.refinement_queue.iter_mut())
        {
            #[cfg(test)]
            counter.set(counter.get() + 1);
```

`Cell` is used precisely so this works behind a shared reference while the queues are mutably borrowed.

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p ghostframe-lib --lib mark_acked_cost_does_not_grow`
Expected: FAIL — roughly `examined 200 items at depth 200 versus 4 at depth 4`.

- [ ] **Step 3: Commit the failing test**

```bash
git add ghostframe-lib/src/transport/scheduler.rs
git commit -m "test(scheduler): pin mark_acked to constant cost

Fails today at 200 comparisons against 4. Written as a unit test because
measuring it through a scene would be vacuous: refinement work is removed
from the queue at emit, so mark_acked currently scans nothing in practice."
```

## Task 9: Make `mark_acked` O(1) via the slot index

**Files:**
- Modify: `ghostframe-lib/src/transport/scheduler.rs`

- [ ] **Step 1: Store handles as work is enqueued**

Move the queues to hold handles and let the slab own the `TileWork` values. The slot index then records *where* each tile-pass's work lives, so every lookup below becomes a direct resolve instead of a scan.

In the `Scheduler` struct replace:

```rust
    priority_queue: VecDeque<TileWork>,
    refinement_queue: VecDeque<TileWork>,
```

with:

```rust
    work: Slab<TileWork>,
    priority_order: VecDeque<Handle>,
    /// Refinement order bucketed by `pass_idx`, so pass-major drain is a walk
    /// over buckets rather than a repeated `min()` over the whole queue.
    refinement_order: [VecDeque<Handle>; PASS_SLOTS],
```

`enqueue_at` becomes:

```rust
    pub fn enqueue_at(&mut self, mut work: TileWork, now: Instant) {
        debug_assert!((work.tile_x as u32) < self.cols, "tile_x out of bounds");
        debug_assert!((work.tile_y as u32) < self.rows, "tile_y out of bounds");
        work.queued_at = now;
        work.last_sent_at = None;
        work.state = WorkState::Pending;
        let (tx, ty, pass) = (work.tile_x, work.tile_y, work.pass_idx);
        let h = self.work.insert(work);
        self.slots.set_handle(tx, ty, pass, Some(h));
        self.priority_order.push_back(h);
    }
```

`enqueue_refinement_work_at` is identical except the last line:

```rust
        self.refinement_order[(pass as usize).min(PASS_SLOTS - 1)].push_back(h);
```

- [ ] **Step 2: Rewrite `mark_acked` as a point lookup**

```rust
    pub fn mark_acked(&mut self, tile_x: u8, tile_y: u8, generation: u8, pass_idx: u8) {
        let Some(h) = self.slots.handle(tile_x, tile_y, pass_idx) else {
            return;
        };
        let Some(work) = self.work.get_mut(h) else {
            return;
        };
        // Stale-generation, stale-pass, or already-resolved entries are
        // skipped silently: late ACKs after bump_generation, and duplicate
        // per-fragment ACKs, are both normal.
        if work.generation == generation && work.state == WorkState::InFlight {
            work.state = WorkState::Acked;
        }
    }
```

The `mark_acked_comparisons` counter now increments at most once; move the `#[cfg(test)]` increment to just before the `slots.handle` lookup and delete the loop instrumentation.

- [ ] **Step 3: Update the remaining queue consumers**

Each of these now walks handles and resolves through the slab. Signatures and return types are unchanged.

```rust
    pub fn queue_len(&self) -> usize {
        self.priority_order.len()
    }

    pub fn refinement_queue_len(&self) -> usize {
        self.refinement_order.iter().map(|q| q.len()).sum()
    }

    pub fn refinement_deficit_tiles(&self) -> u32 {
        let mut seen: std::collections::HashSet<(u8, u8)> = std::collections::HashSet::new();
        for h in self.refinement_order.iter().flatten() {
            if let Some(w) = self.work.get(*h) {
                if w.state != WorkState::Acked {
                    seen.insert((w.tile_x, w.tile_y));
                }
            }
        }
        seen.len() as u32
    }

    pub fn refinement_queue_holds_tile(&self, tile_x: u8, tile_y: u8) -> bool {
        (0..PASS_SLOTS as u8).any(|p| {
            self.slots
                .handle(tile_x, tile_y, p)
                .and_then(|h| self.work.get(h))
                .is_some_and(|w| {
                    w.tile_x == tile_x
                        && w.tile_y == tile_y
                        && !matches!(w.state, WorkState::Acked | WorkState::Superseded)
                })
        })
    }

    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn peek_for_test(&self) -> Vec<TileWork> {
        self.priority_order
            .iter()
            .filter_map(|h| self.work.get(*h).cloned())
            .collect()
    }

    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn refinement_peek_for_test(&self) -> Vec<TileWork> {
        self.refinement_order
            .iter()
            .flatten()
            .filter_map(|h| self.work.get(*h).cloned())
            .collect()
    }
```

`supersede_pending_for_tile` becomes O(passes):

```rust
    pub fn supersede_pending_for_tile(&mut self, tile_x: u8, tile_y: u8) {
        for p in 0..PASS_SLOTS as u8 {
            if let Some(h) = self.slots.handle(tile_x, tile_y, p) {
                if let Some(w) = self.work.get_mut(h) {
                    if matches!(w.state, WorkState::Pending | WorkState::InFlight) {
                        w.state = WorkState::Superseded;
                    }
                }
            }
        }
    }
```

`clear` and `resize` must also empty the slab and the order queues:

```rust
        self.work.clear();
        self.priority_order.clear();
        for q in self.refinement_order.iter_mut() {
            q.clear();
        }
```

- [ ] **Step 4: Run the bounded-scan test**

Run: `cargo test -p ghostframe-lib --lib mark_acked_cost_does_not_grow`
Expected: PASS.

- [ ] **Step 5: Run everything**

```bash
cargo test -p ghostframe-lib --lib
cargo test -p ghostframe-e2e --test browserless_runner
```

Expected: `406 passed` (plus the new tests from Tasks 4, 6, 8) and browserless `17 passed; 0 failed; 2 ignored`.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/src/transport/scheduler.rs
git commit -m "perf(scheduler): O(1) mark_acked and O(passes) supersede via slot index

Queues hold slab handles; the slot index maps (tile, pass) to its handle.
mark_acked drops from a full queue scan to one lookup, and
supersede_pending_for_tile from a queue scan to 14 slot reads."
```

## Task 10: Pass-major drain without the repeated `min()`

**Files:**
- Modify: `ghostframe-lib/src/transport/scheduler.rs` (`drain_priority_queue` ~line 630, `drain_refinement_pass_major` ~line 666)

- [ ] **Step 1: Rewrite both drains over handles**

```rust
    fn drain_priority_queue(
        work: &mut Slab<TileWork>,
        order: &mut VecDeque<Handle>,
        budget: usize,
        rtt: Duration,
        now: Instant,
        out: &mut Vec<TileWork>,
    ) {
        let retry_after = 2 * rtt;
        // Drop terminal-state entries first.
        order.retain(|h| match work.get(*h) {
            Some(w) => !matches!(w.state, WorkState::Superseded | WorkState::Acked),
            None => false,
        });

        let mut spent = 0usize;
        for h in order.iter() {
            let Some(w) = work.get(*h) else { continue };
            let eligible = match w.state {
                WorkState::Pending => true,
                WorkState::InFlight => w
                    .last_sent_at
                    .map(|t| now.duration_since(t) >= retry_after)
                    .unwrap_or(true),
                _ => false,
            };
            if !eligible {
                continue;
            }
            let cost = w.payload.len();
            if spent + cost > budget {
                break;
            }
            spent += cost;
            let w = work.get_mut(*h).expect("handle resolved above");
            w.state = WorkState::InFlight;
            w.last_sent_at = Some(now);
            out.push(w.clone());
        }
    }
```

```rust
    /// Pass-major: every tile's pass 0 before any tile's pass 1. With one
    /// bucket per pass that is a walk over buckets in order, replacing the
    /// repeated `queue.iter().map(pass_idx).min()` scan.
    fn drain_refinement_pass_major(
        work: &mut Slab<TileWork>,
        order: &mut [VecDeque<Handle>; PASS_SLOTS],
        budget: usize,
        now: Instant,
        out: &mut Vec<TileWork>,
    ) {
        for q in order.iter_mut() {
            q.retain(|h| match work.get(*h) {
                Some(w) => !matches!(w.state, WorkState::Superseded | WorkState::Acked),
                None => false,
            });
        }

        let mut spent = 0usize;
        for q in order.iter_mut() {
            while let Some(h) = q.front().copied() {
                let Some(w) = work.get(h) else {
                    q.pop_front();
                    continue;
                };
                let cost = w.payload.len();
                if spent + cost > budget {
                    return;
                }
                spent += cost;
                q.pop_front();
                let w = work.get_mut(h).expect("handle resolved above");
                w.last_sent_at = Some(now);
                w.state = WorkState::InFlight;
                out.push(w.clone());
            }
        }
    }
```

**Behaviour to preserve exactly:** refinement work is still *removed* from its bucket at emit (`q.pop_front()`), matching today's `queue.remove(idx)`. Holding it until acknowledged is Phase 3's change, not this one. Priority work is still *retained* until Acked or Superseded. Keeping those two different is what makes this phase behaviour-preserving.

Note the slab entry is not freed on refinement emit here, to avoid changing when handles are invalidated mid-phase; `mark_acked` and the retain above both tolerate a handle whose bucket entry is gone. Freeing is Phase 3's concern.

- [ ] **Step 2: Update `tick_at`'s calls**

```rust
        Self::drain_priority_queue(
            &mut self.work,
            &mut self.priority_order,
            priority_budget,
            rtt,
            now,
            &mut emitted,
        );
        Self::drain_refinement_pass_major(
            &mut self.work,
            &mut self.refinement_order,
            refinement_budget,
            now,
            &mut emitted,
        );
```

The empty-queue repurposing above it must consult the new fields:

```rust
        let refinement_empty = self.refinement_order.iter().all(|q| q.is_empty());
        if refinement_empty {
            priority_budget = budget_bytes;
            refinement_budget = 0;
        } else if self.priority_order.is_empty() {
            refinement_budget = budget_bytes;
            priority_budget = 0;
        }
```

- [ ] **Step 3: Run everything**

```bash
cargo test -p ghostframe-lib --lib
cargo test -p ghostframe-e2e --test browserless_runner
```

Expected: unchanged results. Pay particular attention to `"InFlight work should retry after 2×RTT"` (`scheduler.rs` ~line 988) and the pass-major ordering tests — those pin the two behaviours this task rewrites.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/transport/scheduler.rs
git commit -m "perf(scheduler): bucket refinement order by pass, drop the min() scan

Pass-major drain walks one bucket per pass instead of recomputing
queue.iter().map(pass_idx).min() at every level. Behaviour preserved:
refinement work is still removed at emit, priority work still retained
until acked or superseded."
```

## Task 11: `pending_refinement_snapshot` and final sweep

**Files:**
- Modify: `ghostframe-lib/src/transport/scheduler.rs`

- [ ] **Step 1: Port the last queue walker**

```rust
    pub fn pending_refinement_snapshot(
        &self,
        coverage_snapshot: &[crate::transport::fragment_coverage::FragmentCoverage],
    ) -> Vec<(u8, u8, u8, u8)> {
        use std::collections::HashMap;
        let mut counts: HashMap<(u8, u8, u8), u8> = HashMap::new();
        // Source 1: Pending/InFlight entries still in the refinement buckets.
        for h in self.refinement_order.iter().flatten() {
            let Some(w) = self.work.get(*h) else { continue };
            if matches!(w.state, WorkState::Pending | WorkState::InFlight) {
                let c = counts.entry((w.tile_x, w.tile_y, w.generation)).or_insert(0);
                *c = c.saturating_add(1);
            }
        }
        // Source 2: outstanding Cdf53 coverage entries, which live on
        // IoBridge.fragment_coverage rather than on Scheduler.
        for entry in coverage_snapshot {
            if matches!(entry.codec, crate::transport::protocol::Codec::Cdf53) {
                let c = counts
                    .entry((entry.tile_x, entry.tile_y, entry.generation))
                    .or_insert(0);
                *c = c.saturating_add(1);
            }
        }
        counts
            .into_iter()
            .map(|((tx, ty, g), n)| (tx, ty, g, n))
            .collect()
    }
```

`queue_states_for_test` and `bump_generation_collecting` need the same treatment — walk `priority_order`/`refinement_order`, resolve through `self.work`. Their signatures and semantics do not change.

- [ ] **Step 2: Confirm nothing outside the scheduler changed**

Run: `git diff --stat master -- ghostframe-lib/src ghostframe-e2e/src`

Expected: changes confined to `transport/scheduler.rs`, the two new `transport/scheduler/*.rs` files, and the Phase 0 harness files. **If `io_bridge.rs` appears in that list, the public API changed and this phase has failed its premise** — reconcile before continuing.

- [ ] **Step 3: Full verification**

```bash
cargo test -p ghostframe-lib --lib
cargo test -p ghostframe-e2e --test browserless_runner
cargo clippy -p ghostframe-lib --all-targets -- -D warnings
```

Expected: all green; browserless still `17 passed; 0 failed; 2 ignored`.

- [ ] **Step 4: Re-measure the scan volume**

Confirm the index did what it was built for, using the instrumentation already on this branch:

```bash
GHOSTFRAME_RTO_PROBE=1 cargo test -p ghostframe-e2e --test browserless_runner \
  a_lossless_link_with_a_real_rtt_does_not_retransmit -- --ignored --nocapture 2>&1 \
  | grep -c SCHEDPROBE || true
```

The scene's behaviour must be unchanged from before Phase 1 — the retransmit count should still read 1788, because nothing about repair policy has changed yet. A different number means this phase was not behaviour-preserving.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/scheduler.rs
git commit -m "refactor(scheduler): port remaining queue walkers to handles

Completes Phase 1. Public API unchanged, io_bridge untouched, 406 lib
tests and browserless 17/17 green, and the storm reproduction still
reads 1788 retransmits -- repair policy is Phase 3's change, not this
phase's."
```

---

## Done when

- [ ] Browserless has deterministic drop injection, and an existing-scene run is bit-identical with an empty plan.
- [ ] The "receiver cannot know" case has a test, and the spec records which mechanism repairs it — resolving carried uncertainty #1.
- [ ] `mark_acked` is O(1), pinned by a test that fails against a linear scan.
- [ ] `supersede_pending_for_tile` is O(passes); the per-bump `cdf53_passes_acked` full scan is gone.
- [ ] Pass-major drain no longer recomputes `min()` per level.
- [ ] `io_bridge.rs` is untouched; 406 lib tests and browserless 17/17 unchanged; the storm reproduction still reads 1788.

## Not in this plan

Phase 2 (ledger tombstones, versioned-handle resolution, loss deadline on `ack_p99` — the phase that actually fixes the production bug), Phase 3 (ownership move, deleting the emitter cache and RTO wheel, `AckLatencyTracker`, the gated safety sweep, `in_flight` send-order queue), and Phase 4 (retiring `fragment_coverage`). Each gets its own plan.

`in_flight: VecDeque<Handle>` is deliberately **not** added here: nothing reads it until Phase 3's safety sweep exists.
