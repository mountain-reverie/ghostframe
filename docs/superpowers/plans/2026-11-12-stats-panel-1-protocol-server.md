# Stats Panel PR 1: Protocol and Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A browser or native client can send `STATS_SUBSCRIBE {on}` on the feedback stream and receive, on that stream's return half, a `STATS_SCHEMA` frame (answer to every `on`) and then a `STATS_SAMPLE` frame once a second, with the server's RTT, bandwidth estimate, rate target, sent bytes, frame mode and reason, H.264 backend, and loss counters. Nothing is sent before a subscribe or after an unsubscribe. No UI.

**Architecture:**
- `ghostframe-protocol` gains `stats.rs` (wire types, framing, `ServerFrameReader`) and `STATS_SUBSCRIBE_MSG_TYPE = 0x0B` in `feedback_stream.rs`.
- `ghostframe-lib` gains `transport/stats_table.rs` (the `pub(crate)` descriptor table, `StatsInputs`, the version string) and `transport/io_bridge/stats_service.rs` (per-session subscription state, the 1 Hz sampler, frame writes).
- `WebTransportServer` learns which bidi stream carried the feedback and writes whole frames on its return half.
- `ghostframe-client-net` gains a read hook for that return half, so the browserless harness can decode what the server sent.
- Version capture is a `build.rs` in `ghostframe-lib`. The harness gains a `StatsTap` and a new `browserless_stats` test target.

**Tech Stack:** Rust 2021, quinn-proto 0.11 (sans-IO), thiserror 2, proptest, tokio (paused clock in the harness). No new crate, no new dependency.

**Spec:** `docs/specs/stats-panel-design.md` sections 1.1, 1.2, 1.4 (server half), 3 (wire and server errors), 4 (protocol and server sampler tests), 6 item 1.

---

## Ground rules

- Worktree `/home/cedric/work/gf-stats-protocol`, branch `feat/stats-protocol-server` from `master` (Task 1). Every command below runs from that directory.
- Commit with explicit `git add <paths>`, never `git add -A`. Every commit message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Never push.
- GPU-touching gates (`just ci-local`) run only as `~/work/gf-gpu-run just ci-local`, never bare (it touches VA-API; AGENTS.md and the repo notes).
- Mutation checks: each test task ends with "break the code, watch it fail, restore". The restore is `git checkout -- <file>` run after the task's commit, so the task's passing state is what comes back.
- `clippy --workspace --all-targets -- -D warnings` is the real gate. `cargo test -p <crate>` hides test targets that fail to compile.
- Protocol crate has `clippy::wildcard_enum_match_arm` warn (and so does `ghostframe-lib`): never write `_ =>` over an enum in non-test code. Matching on `u8` is fine.
- `ghostframe-protocol` is built for `wasm32-unknown-unknown` by `.github/workflows/client-core.yml`: no `std::time`, no threads in `stats.rs`.

## File structure

**Created**

| File | Responsibility |
|---|---|
| `ghostframe-protocol/src/stats.rs` | Stats wire format: `MetricKind`, `MetricGroup`, `MetricDescriptor`, `StatsSchema`, `MetricValue`, `StatsSample`, `StatsSubscribe`, `StatsWireError`, `ServerFrameReader`, framing constants, tests and proptests |
| `ghostframe-lib/src/transport/stats_table.rs` | `pub(crate)` descriptor table (`Stat`), `StatsInputs` (plain data read from the bridge), `schema()`, `host_name()`, `SERVER_VERSION` |
| `ghostframe-lib/src/transport/io_bridge/stats_service.rs` | `StatsSession`, `StatsCounters`, `IoBridge` methods: subscribe handling, `stats_inputs`, `service_stats`, `stats_next_due`, frame write; its unit tests |
| `ghostframe-e2e/src/harness/stats_tap.rs` | `StatsPlan`, `StatsProbe`, `StatsTap`: subscribes through the harness client and decodes what comes back |
| `ghostframe-e2e/tests/browserless_stats.rs` | The no-UI e2e hook: browserless scenes that subscribe and assert live numbers |

**Modified**

| File | Change |
|---|---|
| `ghostframe-protocol/src/feedback_stream.rs` | `STATS_SUBSCRIBE_MSG_TYPE`, `STATS_SUBSCRIBE_SIZE`; array 7 to 8; registration test |
| `ghostframe-protocol/src/lib.rs` | `pub mod stats;` |
| `ghostframe-lib/build.rs` | `GHOSTFRAME_BUILD_VERSION` (crate version, commit, dirty) and its rerun triggers |
| `ghostframe-lib/src/tile/classifier.rs` | `last_reason` field and `last_decision_reason()` |
| `ghostframe-lib/src/tile/classifier_decide_tests.rs` | test for the reason accessor |
| `ghostframe-lib/src/transport/mod.rs` | `pub(crate) mod stats_table;` |
| `ghostframe-lib/src/transport/webtransport.rs` | `last_feedback_stream`, `stats_stream`, `stats_tail`, `note_stats_subscribe`, `write_stats_frame`, `StatsWrite` |
| `ghostframe-lib/src/transport/io_bridge.rs` | `mod stats_service`, three state fields, the `0x0B` dispatch arm, `next_wake_deadline` input |
| `ghostframe-lib/src/transport/io_bridge/session.rs` | `forget_session` clears the subscription |
| `ghostframe-client-net/src/lib.rs` | `ClientNet::read_feedback_return` |
| `ghostframe-client-net/tests/feedback.rs` | two real-quinn tests of the return half |
| `ghostframe-e2e/src/harness/mod.rs` | `pub mod stats_tap;` |
| `ghostframe-e2e/src/harness/browserless.rs` | `RunOptions::stats`, `ClientProbe::stats`, `BrowserlessResult::client_stats`, the tap call |
| `.github/workflows/e2e.yml` | `cargo test --test browserless_stats` by name |
| `ghostframe-lib/include/ghostframe.h` | regenerated (new `pub const` lines land here, see Task 18) |

No new workspace member, so the e2e test-server Dockerfile needs no change. No new `GHOSTFRAME_*` variable, so `ci/check-env-reads.sh` is unaffected (`build.rs` is not under `src/`).

## Wire format (the one place it is written down)

```text
client -> server, feedback stream:   [0x0B][on u8: 0|1]                      (2 bytes)

server -> client, same stream's return half, framed:
    [type u8][len u16 LE][payload]     len <= 16384
  type 0x01 STATS_SCHEMA payload:
    schema_id u16 | server_version (u16 len + utf8) | host_name (u16 len + utf8)
    | count u16 (<= 512) | count x descriptor
  descriptor:  id u16 | name (u8 len + utf8) | unit (u8 len + utf8)
               | kind u8 (0 gauge, 1 counter, 2 text) | group u8 (0..3, else Other=255)
               | label (u8 len + utf8)
  type 0x02 STATS_SAMPLE payload:
    schema_id u16 | timestamp_us u64 | count u16 | count x value
  value: gauge/counter -> f64 LE (8 bytes); text -> u8 len + utf8  (kind from the schema)
```

The sample carries `count` because spec section 3 makes "value count does not match the schema" a rejected sample; with no count on the wire that case could not be told from truncation.

---

### Task 1: Worktree and branch

**Model:** haiku

**Files:** none (creates `/home/cedric/work/gf-stats-protocol`)

- [ ] **Step 1: Invoke the skill.** Use superpowers:using-git-worktrees. The repo keeps its worktrees as siblings under `/home/cedric/work/gf-*`.

- [ ] **Step 2: Create the worktree from master**

```bash
cd /home/cedric/work/ghostframe
git fetch origin master
git worktree add -b feat/stats-protocol-server /home/cedric/work/gf-stats-protocol origin/master
```

Expected: `Preparing worktree (new branch 'feat/stats-protocol-server')`. If `origin/master` is unavailable offline, use `master`.

- [ ] **Step 3: Copy the prebuilt web client** (ghostbridge embeds `dist/` at compile time; a fresh worktree fails "timed out waiting for frame rendering" without it)

```bash
cp -r /home/cedric/work/ghostframe/ghostframe-web-client/dist /home/cedric/work/gf-stats-protocol/ghostframe-web-client/dist
```

- [ ] **Step 4: Verify the baseline builds**

```bash
cd /home/cedric/work/gf-stats-protocol
git status --short | head
cargo test -p ghostframe-protocol feedback_stream 2>&1 | tail -5
```

Expected: clean status (the `dist/` is gitignored), `test result: ok`.

No commit: nothing changed.

---

### Task 2: Register `STATS_SUBSCRIBE_MSG_TYPE`

**Model:** haiku

**Files:**
- Modify: `ghostframe-protocol/src/feedback_stream.rs` (constants after `DISPLAY_MODE_MSG_TYPE` line 38; array line 125; tests)

- [ ] **Step 1: Write the failing test.** Append inside `mod tests` in `ghostframe-protocol/src/feedback_stream.rs`:

```rust
    #[test]
    fn stats_subscribe_is_registered_and_clear_of_the_ack_envelope() {
        assert_eq!(STATS_SUBSCRIBE_MSG_TYPE, 0x0B);
        assert_eq!(STATS_SUBSCRIBE_SIZE, 2);
        assert!(
            FEEDBACK_STREAM_MSG_TYPES.contains(&STATS_SUBSCRIBE_MSG_TYPE),
            "a type missing from the table escapes the collision test"
        );
        // 0x0A is the frame-ack datagram envelope; keep it off the stream
        // namespace so a dump of either channel stays unambiguous.
        assert_ne!(STATS_SUBSCRIBE_MSG_TYPE, 0x0A);
    }
```

- [ ] **Step 2: Run it and see it fail**

```bash
cargo test -p ghostframe-protocol stats_subscribe_is_registered 2>&1 | tail -15
```

Expected: compile error `cannot find value 'STATS_SUBSCRIBE_MSG_TYPE' in this scope`.

- [ ] **Step 3: Implement.** After the `DISPLAY_MODE_MSG_TYPE` constant (line 38) add:

```rust
/// Client request to start or stop the server's connection-stats stream,
/// `[STATS_SUBSCRIBE_MSG_TYPE, on]` with `on` 1 or 0; see
/// [`crate::stats::StatsSubscribe`]. 0x0A is skipped: it is the frame-ack
/// datagram envelope.
pub const STATS_SUBSCRIBE_MSG_TYPE: u8 = 0x0B;
/// Encoded length of a stats subscribe message.
pub const STATS_SUBSCRIBE_SIZE: usize = 2;
```

Change the table (line 125) to:

```rust
pub const FEEDBACK_STREAM_MSG_TYPES: [u8; 8] = [
    FEEDBACK_MSG_TYPE,
    HELLO_MSG_TYPE,
    DECODE_ERROR_MSG_TYPE,
    INPUT_MSG_TYPE,
    DISPLAY_INFO_MSG_TYPE,
    DISPLAY_MODE_MSG_TYPE,
    RESET_REQUEST_MSG_TYPE,
    STATS_SUBSCRIBE_MSG_TYPE,
];
```

- [ ] **Step 4: Run and see it pass**

```bash
cargo test -p ghostframe-protocol feedback_stream 2>&1 | tail -8
```

Expected: all `feedback_stream::tests::*` pass, including `stream_message_types_are_distinct`.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-protocol/src/feedback_stream.rs
git commit -m "$(cat <<'EOF'
feat(protocol): register STATS_SUBSCRIBE (0x0B) on the feedback stream

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 6: Prove the test is load-bearing.** Remove `STATS_SUBSCRIBE_MSG_TYPE,` from the array and change `[u8; 8]` to `[u8; 7]`; run `cargo test -p ghostframe-protocol stats_subscribe_is_registered`; expect `assertion failed: FEEDBACK_STREAM_MSG_TYPES.contains(...)`. Then set the constant to `0x0A` with the array restored: expect `assert_eq!(STATS_SUBSCRIBE_MSG_TYPE, 0x0B)` to fail. Restore: `git checkout -- ghostframe-protocol/src/feedback_stream.rs`, rerun, green.

---

### Task 3: `stats.rs` types, errors, framing helpers and `StatsSubscribe`

**Model:** sonnet

**Files:**
- Create: `ghostframe-protocol/src/stats.rs`
- Modify: `ghostframe-protocol/src/lib.rs` (add `pub mod stats;` after `pub mod protocol;`)

All the types are defined here so the later tasks' tests compile; their `impl`s arrive in Tasks 4 to 6.

- [ ] **Step 1: Register the module.** In `ghostframe-protocol/src/lib.rs` add `pub mod stats;` between `pub mod protocol;` and `pub mod tile;`.

- [ ] **Step 2: Write the failing tests.** Create `ghostframe-protocol/src/stats.rs` containing only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_roundtrip_and_layout() {
        for on in [false, true] {
            let bytes = StatsSubscribe { on }.encode();
            assert_eq!(bytes, [0x0B, u8::from(on)]);
            assert_eq!(StatsSubscribe::decode(&bytes), Ok(StatsSubscribe { on }));
        }
    }

    #[test]
    fn subscribe_hostile_input() {
        assert_eq!(StatsSubscribe::decode(&[]), Err(StatsWireError::Truncated));
        assert_eq!(
            StatsSubscribe::decode(&[0x0B]),
            Err(StatsWireError::Truncated)
        );
        assert_eq!(
            StatsSubscribe::decode(&[0x0C, 1]),
            Err(StatsWireError::BadMessage),
            "a wrong type byte"
        );
        assert_eq!(
            StatsSubscribe::decode(&[0x0B, 2]),
            Err(StatsWireError::BadMessage),
            "`on` is 0 or 1"
        );
        // The stream is framed by message type: bytes past the message belong
        // to the next one and are not examined.
        assert_eq!(
            StatsSubscribe::decode(&[0x0B, 1, 0xFF, 0xFF]),
            Ok(StatsSubscribe { on: true })
        );
    }

    #[test]
    fn metric_group_unknown_byte_decodes_to_other() {
        for g in [
            MetricGroup::Connection,
            MetricGroup::Bandwidth,
            MetricGroup::Video,
            MetricGroup::Loss,
            MetricGroup::Other,
        ] {
            assert_eq!(MetricGroup::from_u8(g.as_u8()), g);
        }
        assert_eq!(MetricGroup::from_u8(4), MetricGroup::Other);
        assert_eq!(MetricGroup::from_u8(77), MetricGroup::Other);
        assert_eq!(MetricGroup::Other.as_u8(), 255);
    }

    #[test]
    fn metric_kind_unknown_byte_is_not_a_kind() {
        for k in [MetricKind::Gauge, MetricKind::Counter, MetricKind::Text] {
            assert_eq!(MetricKind::from_u8(k.as_u8()), Some(k));
        }
        assert_eq!(MetricKind::from_u8(3), None);
        assert_eq!(MetricKind::from_u8(255), None);
    }
}
```

- [ ] **Step 3: Run and see it fail**

```bash
cargo test -p ghostframe-protocol stats:: 2>&1 | tail -15
```

Expected: compile errors `cannot find type 'StatsSubscribe'` and friends.

- [ ] **Step 4: Implement.** Put this at the top of `ghostframe-protocol/src/stats.rs`, above the test module:

```rust
//! The connection-stats wire format: how a client asks the server for its
//! numbers, and how the server sends them.
//!
//! ## Channels
//!
//! *Client to server* the subscribe is a feedback-stream message
//! ([`StatsSubscribe`], type [`crate::feedback_stream::STATS_SUBSCRIBE_MSG_TYPE`]).
//!
//! *Server to client* the numbers travel on the return half of that same
//! bidirectional stream, which nothing used before. It has its own one-byte
//! type namespace ([`SERVER_MSG_STATS_SCHEMA`], [`SERVER_MSG_STATS_SAMPLE`])
//! and explicit framing, `[type u8][len u16 LE][payload]`, because a stream
//! read can split or join messages. [`ServerFrameReader`] reassembles.
//!
//! A frame's payload is at most [`SERVER_FRAME_MAX_LEN`]. Every string and
//! count is bounded and checked on decode; decoding never panics.
//!
//! ## Layout
//!
//! ```text
//! STATS_SCHEMA   schema_id u16 | server_version (u16 len + utf8)
//!                | host_name (u16 len + utf8) | count u16 (<= 512)
//!                | count x [ id u16 | name (u8 len + utf8) | unit (u8 len + utf8)
//!                            | kind u8 | group u8 | label (u8 len + utf8) ]
//! STATS_SAMPLE   schema_id u16 | timestamp_us u64 | count u16 | count x value
//!                value: gauge or counter -> f64 LE; text -> u8 len + utf8
//!                (the kind comes from the schema, not the wire)
//! ```
//!
//! No capability bit and no compatibility path: the web client is embedded in
//! the server binary, so both sides always ship together.

use thiserror::Error;

use crate::feedback_stream::{STATS_SUBSCRIBE_MSG_TYPE, STATS_SUBSCRIBE_SIZE};

/// Server-to-client frame type: the metric list.
pub const SERVER_MSG_STATS_SCHEMA: u8 = 0x01;
/// Server-to-client frame type: one value per descriptor.
pub const SERVER_MSG_STATS_SAMPLE: u8 = 0x02;
/// Largest payload of one server-to-client frame.
pub const SERVER_FRAME_MAX_LEN: usize = 16 * 1024;
/// Most descriptors one schema may carry.
pub(crate) const MAX_DESCRIPTORS: usize = 512;
/// Bytes in a frame header: type and length.
const FRAME_HEADER_LEN: usize = 3;
/// Longest string behind a one-byte length prefix.
const MAX_SHORT_STR: usize = 255;

/// Why a stats message did not decode, or could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum StatsWireError {
    /// The buffer ended before the message did.
    #[error("stats message truncated")]
    Truncated,
    /// A string, a count or a whole frame is over its limit; also bytes left
    /// over after a schema.
    #[error("stats message or field over its limit")]
    TooLong,
    /// A descriptor's kind byte is not a [`MetricKind`].
    #[error("unknown metric kind")]
    UnknownKind,
    /// A sample's value count differs from the schema's descriptor count (or
    /// bytes are left over after its values).
    #[error("sample value count does not match the schema")]
    CountMismatch,
    /// A sample names a different schema than the one it is decoded against.
    #[error("sample schema_id does not match the schema")]
    SchemaMismatch,
    /// A string is not UTF-8.
    #[error("string is not valid UTF-8")]
    BadUtf8,
    /// A subscribe message with the wrong type byte or an `on` other than 0 or 1.
    #[error("not a stats subscribe message")]
    BadMessage,
}

/// How a metric behaves over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// A value that goes up and down.
    Gauge = 0,
    /// A running total; the reader derives a rate.
    Counter = 1,
    /// A string.
    Text = 2,
}

impl MetricKind {
    /// `None` for a byte that is not a kind.
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Gauge),
            1 => Some(Self::Counter),
            2 => Some(Self::Text),
            _ => None,
        }
    }

    /// The wire byte.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// The panel section a metric belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricGroup {
    Connection = 0,
    Bandwidth = 1,
    Video = 2,
    Loss = 3,
    /// Any group this build does not know: a metric a newer server adds still
    /// shows up, in a trailing section.
    Other = 255,
}

impl MetricGroup {
    /// An unknown byte is [`MetricGroup::Other`], never an error.
    pub fn from_u8(b: u8) -> Self {
        match b {
            0 => Self::Connection,
            1 => Self::Bandwidth,
            2 => Self::Video,
            3 => Self::Loss,
            _ => Self::Other,
        }
    }

    /// The wire byte.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// One metric the server can report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricDescriptor {
    /// Stable within a schema; the position in a sample is the schema order,
    /// not this id.
    pub id: u16,
    /// OTel-style name, e.g. `ghostframe.bwe.estimate`.
    pub name: String,
    /// UCUM unit: `bit/s`, `ms`, `1`, `By`, `s`.
    pub unit: String,
    pub kind: MetricKind,
    pub group: MetricGroup,
    /// What the panel shows.
    pub label: String,
}

/// The metric list a session is about to send samples against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsSchema {
    /// Changes whenever a schema is (re)sent; a sample names the schema it
    /// was built against.
    pub schema_id: u16,
    pub server_version: String,
    pub host_name: String,
    pub metrics: Vec<MetricDescriptor>,
}

/// One value of one metric.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricValue {
    Num(f64),
    Text(String),
}

/// One second's values, in schema order.
#[derive(Debug, Clone, PartialEq)]
pub struct StatsSample {
    pub schema_id: u16,
    /// Server monotonic clock, microseconds.
    pub timestamp_us: u64,
    pub values: Vec<MetricValue>,
}

/// Start (`on`) or stop the stats stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsSubscribe {
    pub on: bool,
}

impl StatsSubscribe {
    /// `[STATS_SUBSCRIBE_MSG_TYPE, on]`.
    pub fn encode(&self) -> [u8; STATS_SUBSCRIBE_SIZE] {
        [STATS_SUBSCRIBE_MSG_TYPE, u8::from(self.on)]
    }

    /// Bytes past [`STATS_SUBSCRIBE_SIZE`] belong to the next message and are
    /// not examined.
    pub fn decode(data: &[u8]) -> Result<Self, StatsWireError> {
        if data.len() < STATS_SUBSCRIBE_SIZE {
            return Err(StatsWireError::Truncated);
        }
        if data[0] != STATS_SUBSCRIBE_MSG_TYPE {
            return Err(StatsWireError::BadMessage);
        }
        match data[1] {
            0 => Ok(Self { on: false }),
            1 => Ok(Self { on: true }),
            _ => Err(StatsWireError::BadMessage),
        }
    }
}

// ---- encoding helpers --------------------------------------------------

/// A string behind a one-byte length.
fn put_str8(out: &mut Vec<u8>, s: &str) -> Result<(), StatsWireError> {
    if s.len() > MAX_SHORT_STR {
        return Err(StatsWireError::TooLong);
    }
    out.push(s.len() as u8);
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// A string behind a two-byte length.
fn put_str16(out: &mut Vec<u8>, s: &str) -> Result<(), StatsWireError> {
    let len = u16::try_from(s.len()).map_err(|_| StatsWireError::TooLong)?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// `[type][len u16 LE][payload]`, or `TooLong` past [`SERVER_FRAME_MAX_LEN`].
fn frame(msg_type: u8, payload: &[u8]) -> Result<Vec<u8>, StatsWireError> {
    if payload.len() > SERVER_FRAME_MAX_LEN {
        return Err(StatsWireError::TooLong);
    }
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.push(msg_type);
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// A bounds-checked reader over a payload.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], StatsWireError> {
        let end = self.pos.checked_add(n).ok_or(StatsWireError::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(StatsWireError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, StatsWireError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, StatsWireError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u64(&mut self) -> Result<u64, StatsWireError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().expect("take(8) is 8 bytes")))
    }

    fn f64(&mut self) -> Result<f64, StatsWireError> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn str8(&mut self) -> Result<String, StatsWireError> {
        let n = usize::from(self.u8()?);
        let b = self.take(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| StatsWireError::BadUtf8)
    }

    fn str16(&mut self) -> Result<String, StatsWireError> {
        let n = usize::from(self.u16()?);
        let b = self.take(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| StatsWireError::BadUtf8)
    }

    fn finished(&self) -> bool {
        self.pos == self.buf.len()
    }
}
```

- [ ] **Step 5: Run and see it pass**

```bash
cargo test -p ghostframe-protocol stats:: 2>&1 | tail -10
cargo clippy -p ghostframe-protocol --all-targets -- -D warnings 2>&1 | tail -20
```

Expected: 4 tests pass. Run the tests only. The private helpers and `Cursor` are unused until Task 4, and clippy is first run in Task 5, Step 4. Do not add any `allow`.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-protocol/src/stats.rs ghostframe-protocol/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(protocol): stats wire types, errors and the subscribe message

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 7: Mutation check.** In `StatsSubscribe::decode` change `data[1]` arm `1 => Ok(Self { on: true })` to `1 => Ok(Self { on: false })`; `subscribe_roundtrip_and_layout` must fail. Change `MetricGroup::from_u8`'s `_ => Self::Other` to `_ => Self::Loss`; `metric_group_unknown_byte_decodes_to_other` must fail. Restore with `git checkout -- ghostframe-protocol/src/stats.rs` and rerun green.

---

### Task 4: Schema encode and decode

**Model:** sonnet

**Files:** Modify `ghostframe-protocol/src/stats.rs`

- [ ] **Step 1: Write the failing tests.** Append inside `mod tests`:

```rust
    fn descriptor(id: u16, name: &str, kind: MetricKind, group: MetricGroup) -> MetricDescriptor {
        MetricDescriptor {
            id,
            name: name.to_string(),
            unit: "bit/s".to_string(),
            kind,
            group,
            label: format!("label {name}"),
        }
    }

    fn sample_schema() -> StatsSchema {
        StatsSchema {
            schema_id: 7,
            server_version: "0.1.0 (aab2fa2d, dirty)".to_string(),
            host_name: "evangeline".to_string(),
            metrics: vec![
                descriptor(1, "ghostframe.bwe.estimate", MetricKind::Gauge, MetricGroup::Bandwidth),
                descriptor(2, "ghostframe.loss.verdicts", MetricKind::Counter, MetricGroup::Loss),
                descriptor(3, "ghostframe.frame.mode", MetricKind::Text, MetricGroup::Video),
            ],
        }
    }

    /// A schema with one metric and every string empty: its payload layout is
    /// fixed, so a test can patch a byte by offset.
    ///   0-1 schema_id, 2-3 version len, 4-5 host len, 6-7 count,
    ///   8-9 id, 10 name len, 11 unit len, 12 kind, 13 group, 14 label len.
    fn minimal_payload() -> Vec<u8> {
        let schema = StatsSchema {
            schema_id: 1,
            server_version: String::new(),
            host_name: String::new(),
            metrics: vec![MetricDescriptor {
                id: 9,
                name: String::new(),
                unit: String::new(),
                kind: MetricKind::Gauge,
                group: MetricGroup::Connection,
                label: String::new(),
            }],
        };
        let payload = schema.encode_frame()[FRAME_HEADER_LEN..].to_vec();
        assert_eq!(payload.len(), 15, "layout the offsets below rely on");
        payload
    }

    #[test]
    fn schema_frame_roundtrip_and_framing() {
        let schema = sample_schema();
        let f = schema.encode_frame();
        assert_eq!(f[0], SERVER_MSG_STATS_SCHEMA);
        let len = usize::from(u16::from_le_bytes([f[1], f[2]]));
        assert_eq!(len, f.len() - FRAME_HEADER_LEN, "length covers the payload only");
        assert_eq!(StatsSchema::decode_payload(&f[FRAME_HEADER_LEN..]), Ok(schema));
    }

    #[test]
    fn every_strict_prefix_of_a_schema_is_an_error() {
        let payload = sample_schema().encode_frame()[FRAME_HEADER_LEN..].to_vec();
        for cut in 0..payload.len() {
            assert!(
                StatsSchema::decode_payload(&payload[..cut]).is_err(),
                "a {cut}-byte prefix of a {}-byte schema decoded",
                payload.len()
            );
        }
    }

    #[test]
    fn schema_unknown_kind_is_an_error_and_unknown_group_is_other() {
        let mut p = minimal_payload();
        p[12] = 9;
        assert_eq!(StatsSchema::decode_payload(&p), Err(StatsWireError::UnknownKind));
        let mut p = minimal_payload();
        p[13] = 77;
        let s = StatsSchema::decode_payload(&p).expect("an unknown group still decodes");
        assert_eq!(s.metrics[0].group, MetricGroup::Other);
    }

    #[test]
    fn schema_bad_utf8_and_trailing_bytes_are_errors() {
        // One metric named "a": the name byte is at offset 11.
        let schema = StatsSchema {
            schema_id: 1,
            server_version: String::new(),
            host_name: String::new(),
            metrics: vec![descriptor(1, "a", MetricKind::Gauge, MetricGroup::Other)],
        };
        let mut p = schema.encode_frame()[FRAME_HEADER_LEN..].to_vec();
        p[11] = 0xFF;
        assert_eq!(StatsSchema::decode_payload(&p), Err(StatsWireError::BadUtf8));

        let mut p = minimal_payload();
        p.push(0);
        assert_eq!(StatsSchema::decode_payload(&p), Err(StatsWireError::TooLong));
    }

    #[test]
    fn schema_descriptor_count_is_bounded_before_it_is_read() {
        let mut p = minimal_payload();
        p[6..8].copy_from_slice(&((MAX_DESCRIPTORS + 1) as u16).to_le_bytes());
        assert_eq!(StatsSchema::decode_payload(&p), Err(StatsWireError::TooLong));
    }

    #[test]
    fn try_encode_refuses_what_decode_would_refuse() {
        let mut s = sample_schema();
        s.metrics[0].name = "n".repeat(256);
        assert_eq!(s.try_encode_frame(), Err(StatsWireError::TooLong));

        let mut s = sample_schema();
        s.metrics = (0..=MAX_DESCRIPTORS as u16)
            .map(|i| descriptor(i, "m", MetricKind::Gauge, MetricGroup::Other))
            .collect();
        assert_eq!(s.try_encode_frame(), Err(StatsWireError::TooLong), "513 descriptors");

        // Every string is within its own 255-byte prefix, so only the
        // 16 KiB frame limit can refuse this one. Built directly, not through
        // `descriptor()`, whose label would push a 255-byte name past 255.
        let mut s = sample_schema();
        s.metrics = (0..100u16)
            .map(|i| MetricDescriptor {
                id: i,
                name: "n".repeat(200),
                unit: "u".to_string(),
                kind: MetricKind::Gauge,
                group: MetricGroup::Other,
                label: "l".repeat(200),
            })
            .collect();
        assert_eq!(
            s.try_encode_frame(),
            Err(StatsWireError::TooLong),
            "100 descriptors of about 410 bytes (about 41 KB) are over the 16 KiB frame limit"
        );
        // Control: the same shape with 10 descriptors (about 4 KB) encodes,
        // so the refusal above is the size and not a string.
        s.metrics.truncate(10);
        assert!(s.try_encode_frame().is_ok());
    }
```

- [ ] **Step 2: Run and see it fail**

```bash
cargo test -p ghostframe-protocol stats:: 2>&1 | tail -15
```

Expected: `no method named 'encode_frame' found for struct 'StatsSchema'`.

- [ ] **Step 3: Implement.** Add above the test module in `stats.rs`:

```rust
impl StatsSchema {
    /// The whole frame, `[0x01][len u16 LE][payload]`, or why it cannot be
    /// built: a string over its prefix, more than `MAX_DESCRIPTORS`
    /// descriptors, or a payload over [`SERVER_FRAME_MAX_LEN`]. The server
    /// uses this one: a table that outgrows the limits is counted, not a
    /// panic.
    pub fn try_encode_frame(&self) -> Result<Vec<u8>, StatsWireError> {
        if self.metrics.len() > MAX_DESCRIPTORS {
            return Err(StatsWireError::TooLong);
        }
        let mut p = Vec::new();
        p.extend_from_slice(&self.schema_id.to_le_bytes());
        put_str16(&mut p, &self.server_version)?;
        put_str16(&mut p, &self.host_name)?;
        p.extend_from_slice(&(self.metrics.len() as u16).to_le_bytes());
        for m in &self.metrics {
            p.extend_from_slice(&m.id.to_le_bytes());
            put_str8(&mut p, &m.name)?;
            put_str8(&mut p, &m.unit)?;
            p.push(m.kind.as_u8());
            p.push(m.group.as_u8());
            put_str8(&mut p, &m.label)?;
        }
        frame(SERVER_MSG_STATS_SCHEMA, &p)
    }

    /// [`Self::try_encode_frame`] for a schema the caller knows is within
    /// the limits (a test, a static table).
    ///
    /// # Panics
    /// When the schema is over a limit.
    pub fn encode_frame(&self) -> Vec<u8> {
        self.try_encode_frame()
            .expect("stats schema is within the wire limits")
    }

    /// Decode a frame's payload (the bytes after the 3-byte header).
    pub fn decode_payload(payload: &[u8]) -> Result<Self, StatsWireError> {
        let mut c = Cursor::new(payload);
        let schema_id = c.u16()?;
        let server_version = c.str16()?;
        let host_name = c.str16()?;
        let count = usize::from(c.u16()?);
        if count > MAX_DESCRIPTORS {
            return Err(StatsWireError::TooLong);
        }
        let mut metrics = Vec::with_capacity(count);
        for _ in 0..count {
            let id = c.u16()?;
            let name = c.str8()?;
            let unit = c.str8()?;
            let kind = MetricKind::from_u8(c.u8()?).ok_or(StatsWireError::UnknownKind)?;
            let group = MetricGroup::from_u8(c.u8()?);
            let label = c.str8()?;
            metrics.push(MetricDescriptor {
                id,
                name,
                unit,
                kind,
                group,
                label,
            });
        }
        if !c.finished() {
            return Err(StatsWireError::TooLong);
        }
        Ok(Self {
            schema_id,
            server_version,
            host_name,
            metrics,
        })
    }
}
```

- [ ] **Step 4: Run and see it pass**

```bash
cargo test -p ghostframe-protocol stats:: 2>&1 | tail -15
```

Expected: 10 tests pass (the 4 from Task 3 and 6 new).

- [ ] **Step 5: Commit**

```bash
git add ghostframe-protocol/src/stats.rs
git commit -m "$(cat <<'EOF'
feat(protocol): STATS_SCHEMA encode and bounded decode

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 6: Mutation check.** (a) In `decode_payload` delete the `if count > MAX_DESCRIPTORS` block: `schema_descriptor_count_is_bounded_before_it_is_read` must fail (it would instead fail later with `Truncated`, not `TooLong`). (b) Replace `MetricKind::from_u8(c.u8()?).ok_or(StatsWireError::UnknownKind)?` with `MetricKind::from_u8(c.u8()?).unwrap_or(MetricKind::Gauge)`: `schema_unknown_kind_is_an_error_and_unknown_group_is_other` must fail. (c) Delete the `if !c.finished()` block: `schema_bad_utf8_and_trailing_bytes_are_errors` must fail. (d) In `frame()` delete the `payload.len() > SERVER_FRAME_MAX_LEN` check: `try_encode_refuses_what_decode_would_refuse` (its 100-descriptor case) must fail. Restore `git checkout -- ghostframe-protocol/src/stats.rs`; rerun green.

---

### Task 5: Sample encode and decode

**Model:** sonnet

**Files:** Modify `ghostframe-protocol/src/stats.rs`

- [ ] **Step 1: Write the failing tests.** Append inside `mod tests`:

```rust
    fn sample_for(schema: &StatsSchema) -> StatsSample {
        StatsSample {
            schema_id: schema.schema_id,
            timestamp_us: 123_456_789,
            values: vec![
                MetricValue::Num(42_100_000.0),
                MetricValue::Num(17.0),
                MetricValue::Text("h264".to_string()),
            ],
        }
    }

    #[test]
    fn sample_frame_roundtrip_and_framing() {
        let schema = sample_schema();
        let sample = sample_for(&schema);
        let f = sample.encode_frame();
        assert_eq!(f[0], SERVER_MSG_STATS_SAMPLE);
        let len = usize::from(u16::from_le_bytes([f[1], f[2]]));
        assert_eq!(len, f.len() - FRAME_HEADER_LEN);
        assert_eq!(
            StatsSample::decode_payload(&f[FRAME_HEADER_LEN..], &schema),
            Ok(sample)
        );
    }

    #[test]
    fn sample_with_the_wrong_schema_id_is_rejected() {
        let schema = sample_schema();
        let mut sample = sample_for(&schema);
        sample.schema_id = schema.schema_id.wrapping_add(1);
        let f = sample.encode_frame();
        assert_eq!(
            StatsSample::decode_payload(&f[FRAME_HEADER_LEN..], &schema),
            Err(StatsWireError::SchemaMismatch)
        );
    }

    #[test]
    fn sample_with_the_wrong_value_count_is_rejected() {
        let schema = sample_schema();
        let mut short = sample_for(&schema);
        short.values.pop();
        let f = short.encode_frame();
        assert_eq!(
            StatsSample::decode_payload(&f[FRAME_HEADER_LEN..], &schema),
            Err(StatsWireError::CountMismatch)
        );
        let mut long = sample_for(&schema);
        long.values.push(MetricValue::Num(1.0));
        let f = long.encode_frame();
        assert_eq!(
            StatsSample::decode_payload(&f[FRAME_HEADER_LEN..], &schema),
            Err(StatsWireError::CountMismatch)
        );
    }

    #[test]
    fn every_strict_prefix_of_a_sample_is_an_error() {
        let schema = sample_schema();
        let payload = sample_for(&schema).encode_frame()[FRAME_HEADER_LEN..].to_vec();
        for cut in 0..payload.len() {
            assert!(
                StatsSample::decode_payload(&payload[..cut], &schema).is_err(),
                "a {cut}-byte prefix of a {}-byte sample decoded",
                payload.len()
            );
        }
    }

    #[test]
    fn sample_trailing_bytes_and_bad_utf8_are_errors() {
        let schema = sample_schema();
        let mut p = sample_for(&schema).encode_frame()[FRAME_HEADER_LEN..].to_vec();
        p.push(0);
        assert_eq!(
            StatsSample::decode_payload(&p, &schema),
            Err(StatsWireError::CountMismatch)
        );
        // The text value "h264" is last: its bytes are the final four.
        let mut p = sample_for(&schema).encode_frame()[FRAME_HEADER_LEN..].to_vec();
        let n = p.len();
        p[n - 1] = 0xFF;
        assert_eq!(
            StatsSample::decode_payload(&p, &schema),
            Err(StatsWireError::BadUtf8)
        );
    }

    #[test]
    fn sample_text_over_255_bytes_cannot_be_encoded() {
        let s = StatsSample {
            schema_id: 1,
            timestamp_us: 0,
            values: vec![MetricValue::Text("x".repeat(256))],
        };
        assert_eq!(s.try_encode_frame(), Err(StatsWireError::TooLong));
    }

    #[test]
    fn sample_over_the_frame_limit_cannot_be_encoded() {
        // 70 values of 255 bytes are 70 x 256 + 12 = 17 932 bytes: every
        // string fits its prefix and the count is far under 512, so only the
        // 16 KiB frame limit refuses it.
        let s = StatsSample {
            schema_id: 1,
            timestamp_us: 0,
            values: (0..70).map(|_| MetricValue::Text("x".repeat(255))).collect(),
        };
        assert_eq!(s.try_encode_frame(), Err(StatsWireError::TooLong));
        let mut ok = s.clone();
        ok.values.truncate(10);
        assert!(ok.try_encode_frame().is_ok());
    }
```

- [ ] **Step 2: Run and see it fail**

```bash
cargo test -p ghostframe-protocol stats:: 2>&1 | tail -12
```

Expected: `no method named 'encode_frame' found for struct 'StatsSample'`.

- [ ] **Step 3: Implement.** Add above the test module:

```rust
impl StatsSample {
    /// The whole frame, `[0x02][len u16 LE][payload]`, or `TooLong` for a text
    /// value over 255 bytes or a payload over [`SERVER_FRAME_MAX_LEN`].
    pub fn try_encode_frame(&self) -> Result<Vec<u8>, StatsWireError> {
        if self.values.len() > MAX_DESCRIPTORS {
            return Err(StatsWireError::TooLong);
        }
        let mut p = Vec::new();
        p.extend_from_slice(&self.schema_id.to_le_bytes());
        p.extend_from_slice(&self.timestamp_us.to_le_bytes());
        p.extend_from_slice(&(self.values.len() as u16).to_le_bytes());
        for v in &self.values {
            match v {
                MetricValue::Num(x) => p.extend_from_slice(&x.to_bits().to_le_bytes()),
                MetricValue::Text(s) => put_str8(&mut p, s)?,
            }
        }
        frame(SERVER_MSG_STATS_SAMPLE, &p)
    }

    /// [`Self::try_encode_frame`] for a sample the caller knows is within the
    /// limits.
    ///
    /// # Panics
    /// When the sample is over a limit.
    pub fn encode_frame(&self) -> Vec<u8> {
        self.try_encode_frame()
            .expect("stats sample is within the wire limits")
    }

    /// Decode a frame's payload against `schema`, which says each value's
    /// kind. A different `schema_id`, a different value count, or bytes left
    /// over are errors: the panel marks its rows stale instead of showing
    /// misaligned numbers.
    pub fn decode_payload(payload: &[u8], schema: &StatsSchema) -> Result<Self, StatsWireError> {
        let mut c = Cursor::new(payload);
        let schema_id = c.u16()?;
        if schema_id != schema.schema_id {
            return Err(StatsWireError::SchemaMismatch);
        }
        let timestamp_us = c.u64()?;
        let count = usize::from(c.u16()?);
        if count != schema.metrics.len() {
            return Err(StatsWireError::CountMismatch);
        }
        let mut values = Vec::with_capacity(count);
        for m in &schema.metrics {
            values.push(match m.kind {
                MetricKind::Gauge | MetricKind::Counter => MetricValue::Num(c.f64()?),
                MetricKind::Text => MetricValue::Text(c.str8()?),
            });
        }
        if !c.finished() {
            return Err(StatsWireError::CountMismatch);
        }
        Ok(Self {
            schema_id,
            timestamp_us,
            values,
        })
    }
}
```

- [ ] **Step 4: Run and see it pass; clippy now has no dead code**

```bash
cargo test -p ghostframe-protocol stats:: 2>&1 | tail -8
cargo clippy -p ghostframe-protocol --all-targets -- -D warnings 2>&1 | tail -20
```

Expected: 17 tests pass; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-protocol/src/stats.rs
git commit -m "$(cat <<'EOF'
feat(protocol): STATS_SAMPLE encode and schema-checked decode

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 6: Mutation check.** (a) Delete the `schema_id != schema.schema_id` check: `sample_with_the_wrong_schema_id_is_rejected` fails. (b) Delete the `count != schema.metrics.len()` check: `sample_with_the_wrong_value_count_is_rejected` fails (the long sample would then decode with trailing bytes and still return `CountMismatch` via the `finished` arm, but the short one would return `Truncated`, so the test is red). (c) Swap the `Gauge | Counter` arm to read `c.str8()`: the roundtrip test fails. (d) In `frame()` delete the `payload.len() > SERVER_FRAME_MAX_LEN` check (if Task 4 (d) was restored, delete it again): `sample_over_the_frame_limit_cannot_be_encoded` must fail, as must Task 4's `try_encode_refuses_what_decode_would_refuse`. Restore with `git checkout -- ghostframe-protocol/src/stats.rs`; rerun green.

---

### Task 6: `ServerFrameReader`

**Model:** sonnet

**Files:** Modify `ghostframe-protocol/src/stats.rs`

- [ ] **Step 1: Write the failing tests.** Append inside `mod tests`:

```rust
    fn two_frames() -> (Vec<u8>, Vec<u8>) {
        let schema = sample_schema();
        (schema.encode_frame(), sample_for(&schema).encode_frame())
    }

    #[test]
    fn reader_returns_nothing_until_a_frame_is_whole() {
        let (a, _) = two_frames();
        let mut r = ServerFrameReader::default();
        assert!(r.next_frame().is_none(), "empty");
        for b in &a[..a.len() - 1] {
            r.push(&[*b]);
            assert!(r.next_frame().is_none(), "a frame must not appear early");
        }
        r.push(&a[a.len() - 1..]);
        let (ty, payload) = r.next_frame().expect("whole").expect("valid");
        assert_eq!(ty, SERVER_MSG_STATS_SCHEMA);
        assert_eq!(payload, a[FRAME_HEADER_LEN..]);
        assert!(r.next_frame().is_none());
    }

    #[test]
    fn reader_splits_frames_joined_in_one_read() {
        let (a, b) = two_frames();
        let mut r = ServerFrameReader::default();
        let mut joined = a.clone();
        joined.extend_from_slice(&b);
        r.push(&joined);
        assert_eq!(r.next_frame().expect("first").expect("ok").0, SERVER_MSG_STATS_SCHEMA);
        assert_eq!(r.next_frame().expect("second").expect("ok").0, SERVER_MSG_STATS_SAMPLE);
        assert!(r.next_frame().is_none());
        assert_eq!(r.buffered(), 0);
    }

    #[test]
    fn reader_passes_an_unknown_type_through_by_its_length() {
        let mut r = ServerFrameReader::default();
        r.push(&[0x7E, 2, 0, 0xAA, 0xBB]);
        let (_, b) = two_frames();
        r.push(&b);
        assert_eq!(r.next_frame().expect("unknown").expect("ok"), (0x7E, vec![0xAA, 0xBB]));
        assert_eq!(
            r.next_frame().expect("then the real one").expect("ok").0,
            SERVER_MSG_STATS_SAMPLE,
            "the unknown frame was skipped by its length, not by guessing"
        );
    }

    #[test]
    fn reader_reports_an_oversize_length_once_and_resets() {
        let mut r = ServerFrameReader::default();
        let too_long = (SERVER_FRAME_MAX_LEN as u16 + 1).to_le_bytes();
        r.push(&[SERVER_MSG_STATS_SAMPLE, too_long[0], too_long[1], 1, 2, 3]);
        assert_eq!(r.next_frame(), Some(Err(StatsWireError::TooLong)));
        assert_eq!(r.buffered(), 0, "the reader state is dropped");
        assert!(r.next_frame().is_none(), "reported once, not on every poll");
        // A length at the limit is fine.
        let at_limit = (SERVER_FRAME_MAX_LEN as u16).to_le_bytes();
        r.push(&[SERVER_MSG_STATS_SAMPLE, at_limit[0], at_limit[1]]);
        assert!(r.next_frame().is_none(), "waiting for the payload, not an error");
    }
```

- [ ] **Step 2: Run and see it fail**

```bash
cargo test -p ghostframe-protocol stats:: 2>&1 | tail -10
```

Expected: `cannot find type 'ServerFrameReader'`.

- [ ] **Step 3: Implement.** Add above the test module:

```rust
/// Reassembles `[type u8][len u16 LE][payload]` frames from stream reads.
///
/// Feed it whatever the stream returned with [`Self::push`] and drain frames
/// with [`Self::next_frame`]. A frame may arrive in pieces, and several may
/// arrive in one read. An unknown type is returned like any other: the caller
/// skips it, and its length keeps the stream in step.
#[derive(Debug, Default)]
pub struct ServerFrameReader {
    buf: Vec<u8>,
}

impl ServerFrameReader {
    /// Append bytes read from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes held that are not yet a whole frame.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// The next whole frame as `(type, payload)`, `None` when more bytes are
    /// needed.
    ///
    /// A length over [`SERVER_FRAME_MAX_LEN`] returns `Err(TooLong)` once and
    /// empties the buffer. Nothing closes: the stream cannot be resynchronised
    /// (a length is the only marker), so whatever follows is read as new
    /// frames; a server that honours the limit never gets here.
    pub fn next_frame(&mut self) -> Option<Result<(u8, Vec<u8>), StatsWireError>> {
        if self.buf.len() < FRAME_HEADER_LEN {
            return None;
        }
        let len = usize::from(u16::from_le_bytes([self.buf[1], self.buf[2]]));
        if len > SERVER_FRAME_MAX_LEN {
            self.buf.clear();
            return Some(Err(StatsWireError::TooLong));
        }
        let end = FRAME_HEADER_LEN + len;
        if self.buf.len() < end {
            return None;
        }
        let msg_type = self.buf[0];
        let payload = self.buf[FRAME_HEADER_LEN..end].to_vec();
        self.buf.drain(..end);
        Some(Ok((msg_type, payload)))
    }
}
```

- [ ] **Step 4: Run and see it pass**

```bash
cargo test -p ghostframe-protocol stats:: 2>&1 | tail -8
```

Expected: 21 tests pass.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-protocol/src/stats.rs
git commit -m "$(cat <<'EOF'
feat(protocol): ServerFrameReader reassembles stats frames across reads

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 6: Mutation check.** (a) Change `len > SERVER_FRAME_MAX_LEN` to `len >= SERVER_FRAME_MAX_LEN`: the at-limit half of `reader_reports_an_oversize_length_once_and_resets` fails. (b) Remove `self.buf.clear();` from the oversize branch: the `buffered() == 0` assertion fails. (c) Replace `self.buf.drain(..end)` with nothing: `reader_splits_frames_joined_in_one_read` fails (the same frame comes back twice). Restore with `git checkout -- ghostframe-protocol/src/stats.rs`; rerun green.

---

### Task 7: Property tests over arbitrary schemas, samples and hostile bytes

**Model:** sonnet

**Files:** Modify `ghostframe-protocol/src/stats.rs` (a second test module so the strategies stay separate)

- [ ] **Step 1: Write the tests.** Append after the existing `mod tests`:

```rust
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn arb_descriptor() -> impl Strategy<Value = MetricDescriptor> {
        (
            any::<u16>(),
            "\\PC{0,20}",
            "\\PC{0,8}",
            0u8..3,
            prop_oneof![Just(0u8), Just(1), Just(2), Just(3), Just(255)],
            "\\PC{0,20}",
        )
            .prop_map(|(id, name, unit, kind, group, label)| MetricDescriptor {
                id,
                name,
                unit,
                kind: MetricKind::from_u8(kind).expect("0..3 are kinds"),
                group: MetricGroup::from_u8(group),
                label,
            })
    }

    fn arb_schema() -> impl Strategy<Value = StatsSchema> {
        (
            any::<u16>(),
            "\\PC{0,20}",
            "\\PC{0,20}",
            prop::collection::vec(arb_descriptor(), 0..40),
        )
            .prop_map(|(schema_id, server_version, host_name, metrics)| StatsSchema {
                schema_id,
                server_version,
                host_name,
                metrics,
            })
    }

    /// A schema with a sample built against it. Gauges and counters use
    /// normal finite floats: NaN is not equal to itself, and the wire carries
    /// the bits anyway.
    fn arb_schema_and_sample() -> impl Strategy<Value = (StatsSchema, StatsSample)> {
        arb_schema().prop_flat_map(|schema| {
            let values: Vec<BoxedStrategy<MetricValue>> = schema
                .metrics
                .iter()
                .map(|m| match m.kind {
                    MetricKind::Text => "\\PC{0,20}".prop_map(MetricValue::Text).boxed(),
                    MetricKind::Gauge | MetricKind::Counter => {
                        prop::num::f64::NORMAL.prop_map(MetricValue::Num).boxed()
                    }
                })
                .collect();
            let id = schema.schema_id;
            (Just(schema), any::<u64>(), values).prop_map(move |(schema, timestamp_us, values)| {
                (
                    schema,
                    StatsSample {
                        schema_id: id,
                        timestamp_us,
                        values,
                    },
                )
            })
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]

        #[test]
        fn a_schema_survives_its_own_frame(schema in arb_schema()) {
            let f = schema.try_encode_frame().expect("strategy stays inside the limits");
            prop_assert_eq!(f[0], SERVER_MSG_STATS_SCHEMA);
            prop_assert_eq!(StatsSchema::decode_payload(&f[FRAME_HEADER_LEN..]), Ok(schema));
        }

        #[test]
        fn a_sample_survives_its_own_frame((schema, sample) in arb_schema_and_sample()) {
            let f = sample.try_encode_frame().expect("strategy stays inside the limits");
            prop_assert_eq!(
                StatsSample::decode_payload(&f[FRAME_HEADER_LEN..], &schema),
                Ok(sample)
            );
        }

        #[test]
        fn a_sample_against_a_changed_schema_never_decodes_silently(
            (schema, sample) in arb_schema_and_sample(),
            extra in arb_descriptor(),
        ) {
            let f = sample.try_encode_frame().expect("within limits");
            let mut longer = schema.clone();
            longer.metrics.push(extra);
            prop_assert_eq!(
                StatsSample::decode_payload(&f[FRAME_HEADER_LEN..], &longer),
                Err(StatsWireError::CountMismatch),
                "a sample one value short of its schema must be rejected as a count mismatch"
            );
        }

        #[test]
        fn a_sample_with_one_value_too_many_never_decodes_silently(
            (schema, sample) in arb_schema_and_sample(),
        ) {
            prop_assume!(!schema.metrics.is_empty());
            let f = sample.try_encode_frame().expect("within limits");
            let mut shorter = schema.clone();
            shorter.metrics.pop();
            prop_assert_eq!(
                StatsSample::decode_payload(&f[FRAME_HEADER_LEN..], &shorter),
                Err(StatsWireError::CountMismatch),
                "a sample one value over its schema must be rejected as a count mismatch"
            );
        }

        #[test]
        fn decoders_never_panic_on_arbitrary_bytes(
            bytes in prop::collection::vec(any::<u8>(), 0..400),
            (schema, _) in arb_schema_and_sample(),
        ) {
            let _ = StatsSchema::decode_payload(&bytes);
            let _ = StatsSample::decode_payload(&bytes, &schema);
            let _ = StatsSubscribe::decode(&bytes);
            let mut r = ServerFrameReader::default();
            r.push(&bytes);
            while r.next_frame().is_some() {}
        }

        #[test]
        fn the_reader_is_indifferent_to_how_the_stream_is_cut(
            (schema, sample) in arb_schema_and_sample(),
            cuts in prop::collection::vec(1usize..64, 1..40),
        ) {
            let schema_frame = schema.try_encode_frame().expect("within limits");
            let sample_frame = sample.try_encode_frame().expect("within limits");
            let mut stream = schema_frame.clone();
            stream.extend_from_slice(&sample_frame);
            stream.extend_from_slice(&sample_frame);

            let mut r = ServerFrameReader::default();
            let mut got: Vec<(u8, Vec<u8>)> = Vec::new();
            let mut pos = 0usize;
            let mut i = 0usize;
            while pos < stream.len() {
                let n = cuts[i % cuts.len()].min(stream.len() - pos);
                r.push(&stream[pos..pos + n]);
                pos += n;
                i += 1;
                while let Some(f) = r.next_frame() {
                    got.push(f.expect("no frame is over the limit"));
                }
            }
            prop_assert_eq!(got.len(), 3);
            prop_assert_eq!(&got[0].1[..], &schema_frame[FRAME_HEADER_LEN..]);
            prop_assert_eq!(&got[1].1[..], &sample_frame[FRAME_HEADER_LEN..]);
            prop_assert_eq!(&got[2].1[..], &sample_frame[FRAME_HEADER_LEN..]);
            prop_assert_eq!(r.buffered(), 0);
        }
    }
}
```

- [ ] **Step 2: Run them**

```bash
cargo test -p ghostframe-protocol stats:: 2>&1 | tail -12
cargo clippy -p ghostframe-protocol --all-targets -- -D warnings 2>&1 | tail -5
cargo build -p ghostframe-protocol --target wasm32-unknown-unknown 2>&1 | tail -3
```

Expected: 27 tests pass, clippy clean, wasm build ok (install the target with `rustup target add wasm32-unknown-unknown` if missing). If a proptest finds a counter-example, it is a defect in Tasks 3 to 6: fix the code, never the strategy. A `proptest-regressions/` file the run writes under `ghostframe-protocol/` must be committed.

- [ ] **Step 3: Commit**

```bash
git status --short
git add ghostframe-protocol/src/stats.rs
git commit -m "$(cat <<'EOF'
test(protocol): property tests for the stats wire format

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

(If `git status` shows a new `ghostframe-protocol/proptest-regressions/stats.txt`, add that exact path to the commit.)

- [ ] **Step 4: Mutation check.** In `StatsSample::decode_payload` delete the `count != schema.metrics.len()` check and the `if !c.finished()` check together: both `a_sample_against_a_changed_schema_never_decodes_silently` (it would now get `Truncated`, not `CountMismatch`) and `a_sample_with_one_value_too_many_never_decodes_silently` (it would now get `Ok`) must fail on their exact-error assertions. Restore and delete only the `count` check: the first must still fail (`Truncated`). Then in `ServerFrameReader::next_frame` change `FRAME_HEADER_LEN + len` to `FRAME_HEADER_LEN + len + 1`: `the_reader_is_indifferent_to_how_the_stream_is_cut` fails. Restore with `git checkout -- ghostframe-protocol/src/stats.rs`; rerun green.

---

### Task 8: Version capture: `build.rs` and `SERVER_VERSION`

**Model:** sonnet

**Decision, justified.** The spec (section 1.4) puts the `build.rs` in `ghostframe-server`. The schema is built in `ghostframe-lib`, which is also linked into the e2e harness and the benches. A server-side `build.rs` would need a runtime setter threaded into `IoBridge`, and the browserless harness would then have no real version string to test. So the capture lives in `ghostframe-lib/build.rs`, which already runs for every consumer of the schema. The git commit is the same for every crate in the workspace. The crate version is **not** one workspace version: there is no `[workspace.package]`, and each crate carries its own `version = "0.1.0"`. `ghostframe-lib`'s version stands in for `ghostframe-server`'s, and the two drift the first time only one is bumped. Keep them equal when bumping, or accept that the panel shows the library's.

**What the version match means (settled here, binding on PR 2).** The server's `dirty` flag is computed only when this build script reruns, and the client's when the wasm crate rebuilds. On a dirty tree where only client-core, client-wasm, web-client or server files changed, `just build-release` rebuilds the bundle (`dirty`) but not this script, so the two strings can legitimately disagree on `dirty` while the code is current. Therefore `versions_match` in `StatsView` compares **the crate version and the commit only**; `dirty` is parsed out, shown in the row, and **never** makes the match fail. PR 2 must implement it that way (a parse of `<pkg> (<commit>[, dirty])`, comparing the first two parts; an `unknown` commit on either side never counts as a mismatch). This is recorded under "Contract deviations" below.

**Files:**
- Modify: `ghostframe-lib/build.rs`
- Create: `ghostframe-lib/src/transport/stats_table.rs` (only the constant and its tests for now)
- Modify: `ghostframe-lib/src/transport/mod.rs`

- [ ] **Step 1: Write the failing tests.** Create `ghostframe-lib/src/transport/stats_table.rs`:

```rust
//! The server's stats metric table and the numbers behind it.
//!
//! Internal on purpose: nothing here is `pub`, so none of it reaches the
//! cbindgen header. It is the one list a later OpenTelemetry exporter will
//! read, so the panel and the exporter cannot drift apart.

/// `0.1.0 (aab2fa2d)`, `0.1.0 (aab2fa2d, dirty)`, or `0.1.0 (unknown)` outside a
/// git checkout. Captured by `build.rs`; see its doc for what triggers a rerun.
pub(crate) const SERVER_VERSION: &str = env!("GHOSTFRAME_BUILD_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_version_has_the_documented_shape() {
        let rest = SERVER_VERSION
            .strip_prefix(env!("CARGO_PKG_VERSION"))
            .unwrap_or_else(|| panic!("{SERVER_VERSION:?} must start with the crate version"));
        let inner = rest
            .strip_prefix(" (")
            .and_then(|r| r.strip_suffix(')'))
            .unwrap_or_else(|| panic!("{SERVER_VERSION:?} must end in ` (<commit>)`"));
        let commit = inner.strip_suffix(", dirty").unwrap_or(inner);
        assert!(
            commit == "unknown"
                || (commit.len() >= 8 && commit.chars().all(|c| c.is_ascii_hexdigit())),
            "commit part is `unknown` or 8+ hex digits: {SERVER_VERSION:?}"
        );
    }

    #[test]
    fn server_version_names_the_checked_out_commit() {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--short=8", "HEAD"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output();
        let Ok(out) = out else { return }; // no git on this machine
        if !out.status.success() {
            return; // not a checkout (a source tarball): `unknown` is right
        }
        let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(
            SERVER_VERSION.contains(&head),
            "build.rs captured a stale commit: version {SERVER_VERSION:?}, HEAD {head}"
        );
    }
}
```

In `ghostframe-lib/src/transport/mod.rs` add (after `pub mod scheduler;`): `pub(crate) mod stats_table;`

- [ ] **Step 2: Run and see it fail**

```bash
cargo test -p ghostframe-lib --lib stats_table 2>&1 | tail -12
```

Expected: `environment variable GHOSTFRAME_BUILD_VERSION not defined at compile time`.

- [ ] **Step 3: Implement.** Replace `ghostframe-lib/build.rs` with:

```rust
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `0.1.0 (aab2fa2d)`, `0.1.0 (aab2fa2d, dirty)` or, with no commit,
/// `0.1.0 (unknown)`.
fn format_version(pkg: &str, commit: Option<&str>, dirty: bool) -> String {
    match commit {
        Some(c) if dirty => format!("{pkg} ({c}, dirty)"),
        Some(c) => format!("{pkg} ({c})"),
        None => format!("{pkg} (unknown)"),
    }
}

/// Run git in `dir`; `None` when git is missing, fails, or prints nothing.
///
/// `--no-optional-locks` (and `GIT_OPTIONAL_LOCKS=0`) is load-bearing:
/// `git status` otherwise refreshes and rewrites `.git/index` whenever stat
/// data is stale (after every `git checkout -- <file>`, or any outside
/// `git status` from a shell or an IDE). The index is a `rerun-if-changed`
/// input below, so a script that writes the file it watches would make the
/// next cargo run rerun it and recompile `ghostframe-lib` and everything above
/// it for nothing.
fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Rerun when the checked-out commit or the tree's tracked state changes.
///
/// `git rev-parse --git-path` finds the real files in a worktree, where
/// `.git` is a file. HEAD alone is not enough: a commit moves the branch ref,
/// not HEAD, and may leave the index untouched. A path that does not exist
/// would make cargo rerun this script on every build, so only existing paths
/// are named.
fn emit_git_reruns(dir: &str) {
    let mut paths: Vec<String> = ["HEAD", "index", "packed-refs"]
        .iter()
        .filter_map(|p| git(dir, &["rev-parse", "--git-path", p]))
        .collect();
    if let Some(head_ref) = git(dir, &["symbolic-ref", "-q", "HEAD"]) {
        paths.extend(git(dir, &["rev-parse", "--git-path", &head_ref]));
    }
    for p in paths {
        let p = Path::new(&p);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            Path::new(dir).join(p)
        };
        if abs.exists() {
            println!("cargo:rerun-if-changed={}", abs.display());
        }
    }
}

fn emit_build_version(manifest_dir: &str) {
    let pkg = env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let commit = git(manifest_dir, &["rev-parse", "--short=8", "HEAD"]);
    // Tracked files only: this repository keeps untracked notes in its root,
    // and a flag that is always on says nothing.
    let dirty = commit.is_some()
        && git(manifest_dir, &["status", "--porcelain", "--untracked-files=no"]).is_some();
    println!(
        "cargo:rustc-env=GHOSTFRAME_BUILD_VERSION={}",
        format_version(&pkg, commit.as_deref(), dirty)
    );
    emit_git_reruns(manifest_dir);
    // The dirty flag also follows this crate's own sources (and the protocol
    // crate's, which the header generation below parses). A source edit in
    // another crate does not rerun this script: the flag can lag there until
    // the next commit, index or ghostframe-lib edit. That is accepted, and is
    // why the client compares crate version and commit only (see the Task 8
    // preamble); widening this list to every embedded crate would recompile
    // ghostframe-lib on every client edit.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=cbindgen.toml");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=../ghostframe-protocol/src");
}

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    emit_build_version(&manifest_dir);

    // The ghostbridge c-archive build and its link directives moved to
    // `ghostframe-tsnet`, which owns that FFI surface. Cargo propagates a
    // dependency's `cargo:rustc-link-*` output to whatever links it, so this
    // crate still gets the archive without repeating the recipe.

    // Generate a C header with cbindgen. Non-fatal while ghostframe-lib has
    // no `pub extern "C"` exports — it becomes fatal once M1 starts exporting
    // real symbols; at that point a parse error here should fail the build.
    let cbindgen_config = cbindgen::Config::from_root_or_default(&manifest_dir);
    let include_dir = PathBuf::from(&manifest_dir).join("include");
    std::fs::create_dir_all(&include_dir).expect("Failed to create include/ directory");
    let bindings = cbindgen::Builder::new()
        .with_config(cbindgen_config)
        .with_crate(manifest_dir.clone())
        .with_language(cbindgen::Language::C)
        .generate()
        .expect("cbindgen failed");
    bindings.write_to_file(include_dir.join("ghostframe.h"));
}
```

- [ ] **Step 4: Run and see it pass.** The first build re-runs cbindgen; `include/ghostframe.h` may already change because of Task 2's constant (Task 18 commits that).

```bash
cargo test -p ghostframe-lib --lib stats_table 2>&1 | tail -8
```

Expected: both tests pass. The `stats_table` module is `pub(crate)` with an unused const in a non-test build: if clippy later says `constant SERVER_VERSION is never used`, it is used from Task 10 on; do not add an allow.

- [ ] **Step 5: Commit** (leave `include/ghostframe.h` for Task 18; check `git status` and add only the files below)

```bash
git add ghostframe-lib/build.rs ghostframe-lib/src/transport/stats_table.rs ghostframe-lib/src/transport/mod.rs
git commit -m "$(cat <<'EOF'
feat(lib): capture the build version (crate, commit, dirty) in build.rs

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 6: Mutation check.** (a) In `format_version` return `format!("{pkg} (unknown)")` for every case: `server_version_names_the_checked_out_commit` fails. Restore with `git checkout -- ghostframe-lib/build.rs`.

(b) Prove the ref-path rerun. First `git commit --allow-empty -m tmp1`, then `cargo test -p ghostframe-lib --lib server_version_names`: it must be **green** (the branch ref moved, the script reran, the new HEAD is in the version). Now delete the `symbolic-ref` block in `emit_git_reruns`, `git commit --allow-empty -m tmp2`, and run the same command: it must be **red** (the ref file is no longer watched, so the string still names `tmp1`'s commit; if `packed-refs` or `index` happened to trigger a rerun anyway, the test stays green: record that and keep the block, it is still correct for the loose-ref case). Undo with `git reset --soft HEAD~2` (never `--hard`: the tree may hold the regenerated `include/ghostframe.h`), then `git checkout -- ghostframe-lib/build.rs`, and rerun the test green.

(c) Check the index is left alone. Make the index stat-stale (`touch ghostframe-lib/Cargo.toml`), record `stat -c %Y "$(git rev-parse --git-path index)"`, run `cargo build -p ghostframe-lib`, record it again: the two values must be equal. This is observational (a plain `git status` in the same state would rewrite the index, which is the thing the flags prevent); do not remove the flags to watch it fail.

---

### Task 9: The classifier remembers the last `mode.decision` reason

**Model:** sonnet

The sampler needs "last mode.decision reason" as text. Today the reason is only a tracing field (`ghostframe-lib/src/tile/classifier.rs:512`), computed and dropped.

**Files:**
- Modify: `ghostframe-lib/src/tile/classifier.rs` (field next to `last_emitted_mode` line 362, constructor line 407, `decide_frame_mode_at` line 507, accessor after `last_emitted_mode()` line 536)
- Modify: `ghostframe-lib/src/tile/classifier_decide_tests.rs`

- [ ] **Step 1: Write the failing test.** Append to `ghostframe-lib/src/tile/classifier_decide_tests.rs`:

```rust
#[test]
fn the_reason_of_the_last_mode_decision_is_kept_for_readers() {
    let mut c = Classifier::new(ClassifierConfig::default());
    assert_eq!(
        c.last_decision_reason(),
        "none",
        "before any decision there is nothing to report"
    );
    let mode = c.decide_frame_mode_at(0, &[], FrameMode::TileCodec);
    assert_eq!(c.last_emitted_mode(), Some(mode), "premise: a decision was recorded");
    let reason = c.last_decision_reason();
    assert_ne!(reason, "none", "the reason `decide_inner` returned is stored");
    assert!(!reason.is_empty());
}
```

- [ ] **Step 2: Run and see it fail**

```bash
cargo test -p ghostframe-lib --lib the_reason_of_the_last_mode_decision 2>&1 | tail -10
```

Expected: `no method named 'last_decision_reason'`.

- [ ] **Step 3: Implement.** In `classifier.rs`:

After the `last_emitted_mode: Option<FrameMode>,` field (line 362) add:

```rust
    /// The `reason` logged with the last `mode.decision` event, for readers
    /// that want it as text (the stats panel). `"none"` before the first.
    last_reason: &'static str,
```

In `Classifier::new` after `last_emitted_mode: None,` add `last_reason: "none",`.

In `decide_frame_mode_at`, after `self.last_emitted_mode = Some(next_mode);` add `self.last_reason = reason;`.

After `pub fn last_emitted_mode(&self)` add:

```rust
    /// The reason of the last `mode.decision` event (`"none"` before the
    /// first), as logged. Updated on the same transitions the event fires on.
    pub fn last_decision_reason(&self) -> &'static str {
        self.last_reason
    }
```

- [ ] **Step 4: Run and see it pass**

```bash
cargo test -p ghostframe-lib --lib classifier 2>&1 | tail -6
```

Expected: all classifier tests pass.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/tile/classifier.rs ghostframe-lib/src/tile/classifier_decide_tests.rs
git commit -m "$(cat <<'EOF'
feat(lib): classifier keeps the last mode.decision reason as text

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 6: Mutation check.** Delete the `self.last_reason = reason;` line: the test fails on `assert_ne!(reason, "none")`. Restore with `git checkout -- ghostframe-lib/src/tile/classifier.rs`; rerun green.

---

### Task 10: The descriptor table, `StatsInputs` and the schema

**Model:** sonnet

**Files:** Modify `ghostframe-lib/src/transport/stats_table.rs`

The table is an exhaustive enum: `Stat::ALL` is the order of a schema and a sample, `descriptor()` and `StatsInputs::value()` are exhaustive matches, so a new metric that forgets one of them does not compile. Ids are stable and never reused.

- [ ] **Step 1: Write the failing tests.** Append inside `mod tests` in `stats_table.rs`:

```rust
    use ghostframe_protocol::stats::{
        MetricGroup, MetricKind, MetricValue, StatsSchema, SERVER_FRAME_MAX_LEN,
        SERVER_MSG_STATS_SCHEMA,
    };

    #[test]
    fn ids_and_names_are_unique_and_ids_are_nonzero() {
        let mut ids = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();
        for s in Stat::ALL {
            let d = s.descriptor();
            assert_ne!(d.id, 0, "id 0 is reserved");
            assert!(ids.insert(d.id), "id {} used twice", d.id);
            assert!(names.insert(d.name.clone()), "name {} used twice", d.name);
            assert!(!d.label.is_empty() && !d.unit.is_empty());
        }
    }

    #[test]
    fn every_stat_is_in_all_exactly_once() {
        // `ALL` is written by hand; the exhaustive `id()` match is the compiler's
        // list. Their sizes must agree, or a new variant is silently unsampled.
        let n = Stat::ALL.len();
        let distinct: std::collections::HashSet<u16> = Stat::ALL.iter().map(|s| s.id()).collect();
        assert_eq!(distinct.len(), n);
        assert_eq!(n, 11, "the v1 table; update the spec table when this changes");
    }

    #[test]
    fn a_value_has_the_variant_its_descriptor_kind_says() {
        let inputs = StatsInputs::default();
        for s in Stat::ALL {
            let d = s.descriptor();
            match (d.kind, inputs.value(s)) {
                (MetricKind::Text, MetricValue::Text(_)) => {}
                (MetricKind::Gauge | MetricKind::Counter, MetricValue::Num(_)) => {}
                (kind, v) => panic!("{}: kind {kind:?} but value {v:?}", d.name),
            }
        }
    }

    #[test]
    fn values_follow_the_schema_order_and_carry_the_inputs() {
        let inputs = StatsInputs {
            rtt_ms: 18.5,
            uptime_s: 61.0,
            bwe_bps: 42_100_000.0,
            rate_target_bps: 33_000_000.0,
            sent_bytes: 9_000_000.0,
            frame_mode: "h264",
            mode_reason: "bandwidth_short",
            h264_backend: "libva-direct",
            loss_path_resends: 4.0,
            loss_verdicts: 5.0,
            nacks_received: 6.0,
        };
        let schema = schema(3);
        let values = inputs.values();
        assert_eq!(values.len(), schema.metrics.len());
        let by_name = |n: &str| {
            let i = schema.metrics.iter().position(|m| m.name == n).expect(n);
            values[i].clone()
        };
        assert_eq!(by_name("ghostframe.rtt.smoothed"), MetricValue::Num(18.5));
        assert_eq!(by_name("ghostframe.bwe.estimate"), MetricValue::Num(42_100_000.0));
        assert_eq!(by_name("ghostframe.frame.mode_reason"), MetricValue::Text("bandwidth_short".into()));
        assert_eq!(by_name("ghostframe.h264.backend"), MetricValue::Text("libva-direct".into()));
        assert_eq!(by_name("ghostframe.loss.nacks_received"), MetricValue::Num(6.0));
    }

    #[test]
    fn the_schema_fits_the_wire_and_round_trips() {
        let s = schema(9);
        assert_eq!(s.schema_id, 9);
        assert_eq!(s.server_version, SERVER_VERSION);
        assert!(!s.host_name.is_empty(), "falls back to `unknown`, never empty");
        let frame = s.try_encode_frame().expect("the table fits the 16 KiB frame");
        assert!(frame.len() - 3 <= SERVER_FRAME_MAX_LEN);
        assert_eq!(frame[0], SERVER_MSG_STATS_SCHEMA);
        let back = StatsSchema::decode_payload(&frame[3..]).expect("decodes");
        assert_eq!(back, s);
    }

    #[test]
    fn groups_cover_the_four_panel_sections() {
        let groups: std::collections::HashSet<u8> =
            Stat::ALL.iter().map(|s| s.descriptor().group.as_u8()).collect();
        for g in [MetricGroup::Connection, MetricGroup::Bandwidth, MetricGroup::Video, MetricGroup::Loss] {
            assert!(groups.contains(&g.as_u8()), "no metric in {g:?}");
        }
    }
```

- [ ] **Step 2: Run and see it fail**

```bash
cargo test -p ghostframe-lib --lib stats_table 2>&1 | tail -12
```

Expected: `cannot find type 'Stat'`.

- [ ] **Step 3: Implement.** Replace the non-test part of `stats_table.rs` (everything above `#[cfg(test)]`) with:

```rust
//! The server's stats metric table and the numbers behind it.
//!
//! Internal on purpose: nothing here is `pub`, so none of it reaches the
//! cbindgen header. It is the one list a later OpenTelemetry exporter will
//! read, so the panel and the exporter cannot drift apart.
//!
//! A metric is a [`Stat`]. [`Stat::ALL`] is the order of a schema and of
//! every sample against it; [`Stat::descriptor`] and [`StatsInputs::value`]
//! are exhaustive matches, so adding a variant that forgets either does not
//! compile. Ids are stable: never renumber, never reuse.

use ghostframe_protocol::stats::{
    MetricDescriptor, MetricGroup, MetricKind, MetricValue, StatsSchema,
};

/// `0.1.0 (aab2fa2d)`, `0.1.0 (aab2fa2d, dirty)`, or `0.1.0 (unknown)` outside a
/// git checkout. Captured by `build.rs`; see its doc for what triggers a rerun.
pub(crate) const SERVER_VERSION: &str = env!("GHOSTFRAME_BUILD_VERSION");

/// One server metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stat {
    RttSmoothed,
    SessionUptime,
    BweEstimate,
    RateTarget,
    SentBytes,
    FrameMode,
    ModeReason,
    H264Backend,
    LossPathResends,
    LossVerdicts,
    NacksReceived,
}

impl Stat {
    /// Schema and sample order.
    pub(crate) const ALL: [Stat; 11] = [
        Stat::RttSmoothed,
        Stat::SessionUptime,
        Stat::BweEstimate,
        Stat::RateTarget,
        Stat::SentBytes,
        Stat::FrameMode,
        Stat::ModeReason,
        Stat::H264Backend,
        Stat::LossPathResends,
        Stat::LossVerdicts,
        Stat::NacksReceived,
    ];

    /// The wire id. Stable.
    pub(crate) fn id(self) -> u16 {
        match self {
            Stat::RttSmoothed => 1,
            Stat::SessionUptime => 2,
            Stat::BweEstimate => 3,
            Stat::RateTarget => 4,
            Stat::SentBytes => 5,
            Stat::FrameMode => 6,
            Stat::ModeReason => 7,
            Stat::H264Backend => 8,
            Stat::LossPathResends => 9,
            Stat::LossVerdicts => 10,
            Stat::NacksReceived => 11,
        }
    }

    /// `(name, unit, kind, group, label)`.
    fn def(self) -> (&'static str, &'static str, MetricKind, MetricGroup, &'static str) {
        use MetricGroup::{Bandwidth, Connection, Loss, Video};
        use MetricKind::{Counter, Gauge, Text};
        match self {
            Stat::RttSmoothed => ("ghostframe.rtt.smoothed", "ms", Gauge, Connection, "Round-trip time"),
            Stat::SessionUptime => ("ghostframe.session.uptime", "s", Gauge, Connection, "Session uptime"),
            Stat::BweEstimate => ("ghostframe.bwe.estimate", "bit/s", Gauge, Bandwidth, "Bandwidth estimate"),
            Stat::RateTarget => ("ghostframe.rate.target", "bit/s", Gauge, Bandwidth, "Rate target"),
            Stat::SentBytes => ("ghostframe.sent.bytes", "By", Counter, Bandwidth, "Sent"),
            Stat::FrameMode => ("ghostframe.frame.mode", "1", Text, Video, "Frame mode"),
            Stat::ModeReason => ("ghostframe.frame.mode_reason", "1", Text, Video, "Mode reason"),
            Stat::H264Backend => ("ghostframe.h264.backend", "1", Text, Video, "H.264 encoder"),
            Stat::LossPathResends => ("ghostframe.loss.path_resends", "1", Counter, Loss, "Loss-path resends"),
            Stat::LossVerdicts => ("ghostframe.loss.verdicts", "1", Counter, Loss, "Loss verdicts"),
            Stat::NacksReceived => ("ghostframe.loss.nacks_received", "1", Counter, Loss, "NACKs received"),
        }
    }

    /// What goes in the schema for this metric.
    pub(crate) fn descriptor(self) -> MetricDescriptor {
        let (name, unit, kind, group, label) = self.def();
        MetricDescriptor {
            id: self.id(),
            name: name.to_string(),
            unit: unit.to_string(),
            kind,
            group,
            label: label.to_string(),
        }
    }
}

/// The numbers one sample is built from, read off the bridge in one place so
/// the table above and the bridge cannot disagree about a source.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct StatsInputs {
    /// Smoothed round-trip time, milliseconds.
    pub(crate) rtt_ms: f64,
    /// Seconds since the live session began.
    pub(crate) uptime_s: f64,
    /// The bandwidth estimate, bit/s.
    pub(crate) bwe_bps: f64,
    /// The rate the H.264 encoder was last told, bit/s (0 off H.264).
    pub(crate) rate_target_bps: f64,
    /// Cumulative bytes handed to quinn for this session's datagrams.
    pub(crate) sent_bytes: f64,
    /// `tile` or `h264`.
    pub(crate) frame_mode: &'static str,
    /// The last `mode.decision` reason, `none` before the first.
    pub(crate) mode_reason: &'static str,
    /// `libva-direct`, `openh264`, `pcm`, `none` or `unavailable`.
    pub(crate) h264_backend: &'static str,
    pub(crate) loss_path_resends: f64,
    pub(crate) loss_verdicts: f64,
    pub(crate) nacks_received: f64,
}

impl StatsInputs {
    /// The value of one metric.
    pub(crate) fn value(&self, stat: Stat) -> MetricValue {
        match stat {
            Stat::RttSmoothed => MetricValue::Num(self.rtt_ms),
            Stat::SessionUptime => MetricValue::Num(self.uptime_s),
            Stat::BweEstimate => MetricValue::Num(self.bwe_bps),
            Stat::RateTarget => MetricValue::Num(self.rate_target_bps),
            Stat::SentBytes => MetricValue::Num(self.sent_bytes),
            Stat::FrameMode => MetricValue::Text(self.frame_mode.to_string()),
            Stat::ModeReason => MetricValue::Text(self.mode_reason.to_string()),
            Stat::H264Backend => MetricValue::Text(self.h264_backend.to_string()),
            Stat::LossPathResends => MetricValue::Num(self.loss_path_resends),
            Stat::LossVerdicts => MetricValue::Num(self.loss_verdicts),
            Stat::NacksReceived => MetricValue::Num(self.nacks_received),
        }
    }

    /// Every value, in [`Stat::ALL`] order: a sample's `values`.
    pub(crate) fn values(&self) -> Vec<MetricValue> {
        Stat::ALL.iter().map(|s| self.value(*s)).collect()
    }
}

/// This machine's host name, `unknown` when the kernel will not say.
///
/// `ghostframe-server` has the same helper in `util.rs`, but the library
/// cannot depend on the server crate.
pub(crate) fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return "unknown".to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    if name.is_empty() {
        "unknown".to_string()
    } else {
        name
    }
}

/// The schema a session sends: the version, the host and the whole table.
pub(crate) fn schema(schema_id: u16) -> StatsSchema {
    StatsSchema {
        schema_id,
        server_version: SERVER_VERSION.to_string(),
        host_name: host_name(),
        metrics: Stat::ALL.iter().map(|s| s.descriptor()).collect(),
    }
}
```

- [ ] **Step 4: Run and see it pass.** At this point `Stat`, `StatsInputs`, `schema` are unused outside tests: expect `dead_code` warnings from a non-test `cargo check`, which Task 14 resolves by using them. Run tests only:

```bash
cargo test -p ghostframe-lib --lib stats_table 2>&1 | tail -12
```

Expected: 8 tests pass (2 from Task 8 and 6 new).

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/stats_table.rs
git commit -m "$(cat <<'EOF'
feat(lib): the stats descriptor table and the numbers a sample is built from

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 6: Mutation check.** (a) Change `Stat::LossVerdicts`'s id to 9 (a duplicate): `ids_and_names_are_unique_and_ids_are_nonzero` fails. (b) Change `Stat::FrameMode`'s kind to `Gauge`: `a_value_has_the_variant_its_descriptor_kind_says` fails. (c) In `value()` swap the `RateTarget` and `BweEstimate` fields: `values_follow_the_schema_order_and_carry_the_inputs` fails. Restore with `git checkout -- ghostframe-lib/src/transport/stats_table.rs`; rerun green.

---

### Task 11: `WebTransportServer` writes whole frames on the feedback stream's return half

**Model:** sonnet

**Facts this task rests on** (read, not assumed):
- The server routes every non-session bidi stream's bytes to `feedback_queue` (`webtransport.rs:529-614`), and the queue drops the stream id. One stream per client is the production shape (`ghostframe-web-client/src/main.ts:153`, `ghostframe-client-net/src/lib.rs:516`).
- quinn-proto 0.11 has no query for how many bytes a `send_stream(..).write` will take: `write` may accept a prefix. So "a frame that cannot be written whole is dropped" is done as: a frame is refused before its first byte when a previous partial write is still owed; a frame that is *started* is always finished (its tail is held and written first on the next call). The stream is never left with a torn frame in the middle.

**Files:** Modify `ghostframe-lib/src/transport/webtransport.rs`

- [ ] **Step 1: Write the failing unit test.** Append inside `mod tests` in `webtransport.rs`:

```rust
    #[test]
    fn a_stats_subscribe_pins_the_stream_that_carried_feedback() {
        let mut wt = WebTransportServer::new();
        assert!(
            !wt.note_stats_subscribe(),
            "no feedback stream has been seen: nowhere to answer"
        );
        let s = sid(0);
        wt.test_feed_feedback_stream(s, &wt_bidi_header(0));
        wt.test_feed_feedback_stream(s, &[0x0B, 1]);
        assert!(wt.note_stats_subscribe());
        assert_eq!(wt.stats_stream, Some(s));
        assert!(wt.stats_tail.is_empty());
    }
```

(`sid` and `wt_bidi_header` are the helpers already in that module.)

- [ ] **Step 2: Run and see it fail**

```bash
cargo test -p ghostframe-lib --lib a_stats_subscribe_pins 2>&1 | tail -10
```

Expected: `no method named 'note_stats_subscribe'`.

- [ ] **Step 3: Implement.**

Add three fields to `WebTransportServer` (after `wt_header_stripped`):

```rust
    /// The bidi stream whose bytes were last routed to the feedback queue.
    /// A client has one; a stats subscribe is answered on it.
    last_feedback_stream: Option<StreamId>,
    /// The stream stats frames are written on: the feedback stream as it was
    /// when the client last subscribed.
    stats_stream: Option<StreamId>,
    /// The unwritten end of a stats frame quinn accepted only a prefix of.
    /// Written first on the next call, so a frame is never left torn.
    stats_tail: Vec<u8>,
```

In `new()` add `last_feedback_stream: None, stats_stream: None, stats_tail: Vec::new(),`.

In `route_stream_to_feedback`, inside `if !data.is_empty() {` before the push, add `self.last_feedback_stream = Some(sid);` so it reads:

```rust
            if !data.is_empty() {
                self.last_feedback_stream = Some(sid);
                self.feedback_queue.push(data);
            }
```

Add the outcome type above `impl WebTransportServer` (next to `// ─── main struct`):

```rust
/// What [`WebTransportServer::write_stats_frame`] did with a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsWrite {
    /// The frame is on the stream, whole or with its tail held for the next
    /// call.
    Written,
    /// The stream had no room (flow control) or still owes a previous
    /// frame's tail: the frame was not started. Drop it and count it.
    Blocked,
    /// No stream is pinned: the client has not subscribed on one we know.
    NoStream,
    /// The stream is closed or the peer stopped it.
    StreamGone,
}
```

Add to `impl WebTransportServer` (after `drain_feedback`):

```rust
    /// A stats subscribe arrived: answer on the stream that carries feedback.
    /// `false` when no feedback stream has been seen (the subscribe itself
    /// came on one, so this does not happen in production). A different
    /// stream than last time drops any held tail.
    pub fn note_stats_subscribe(&mut self) -> bool {
        let Some(sid) = self.last_feedback_stream else {
            return false;
        };
        if self.stats_stream != Some(sid) {
            self.stats_stream = Some(sid);
            self.stats_tail.clear();
        }
        true
    }

    /// Bytes of a stats frame still owed to the stream (0 normally).
    pub fn stats_tail_len(&self) -> usize {
        self.stats_tail.len()
    }

    /// Write one whole stats frame on the pinned stream's return half.
    ///
    /// quinn-proto cannot say in advance how much a write will take, so this
    /// never starts a frame it might not finish *and drop*: if the stream
    /// still owes the tail of a previous frame, that is written first and, if
    /// it does not all fit, `frame` is refused untouched ([`StatsWrite::Blocked`]).
    /// A frame that is started is always completed, a prefix now and the rest
    /// at the head of the next call, so the reader never sees a torn frame.
    pub fn write_stats_frame(&mut self, conn: &mut Connection, frame: &[u8]) -> StatsWrite {
        let Some(sid) = self.stats_stream else {
            return StatsWrite::NoStream;
        };
        if !self.stats_tail.is_empty() {
            match conn.send_stream(sid).write(&self.stats_tail) {
                Ok(n) => {
                    self.stats_tail.drain(..n);
                }
                Err(quinn_proto::WriteError::Blocked) => {}
                Err(quinn_proto::WriteError::Stopped(_) | quinn_proto::WriteError::ClosedStream) => {
                    self.stats_tail.clear();
                    return StatsWrite::StreamGone;
                }
            }
            if !self.stats_tail.is_empty() {
                return StatsWrite::Blocked;
            }
        }
        match conn.send_stream(sid).write(frame) {
            Ok(n) if n == frame.len() => StatsWrite::Written,
            Ok(n) => {
                self.stats_tail = frame[n..].to_vec();
                StatsWrite::Written
            }
            Err(quinn_proto::WriteError::Blocked) => StatsWrite::Blocked,
            Err(quinn_proto::WriteError::Stopped(_) | quinn_proto::WriteError::ClosedStream) => {
                StatsWrite::StreamGone
            }
        }
    }
```

(`quinn_proto::WriteError` has exactly the variants `Blocked`, `Stopped(VarInt)`, `ClosedStream`; the match is exhaustive, so a quinn-proto upgrade that adds one fails the build here.)

- [ ] **Step 4: Run and see it pass**

```bash
cargo test -p ghostframe-lib --lib webtransport 2>&1 | tail -8
```

Expected: all `webtransport::tests::*` pass.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/webtransport.rs
git commit -m "$(cat <<'EOF'
feat(lib): WebTransportServer writes whole stats frames on the feedback stream

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 6: Mutation check.** Delete `self.last_feedback_stream = Some(sid);` from `route_stream_to_feedback`: the unit test fails (`note_stats_subscribe` returns false). Restore with `git checkout -- ghostframe-lib/src/transport/webtransport.rs`; rerun green. The write path itself is proven against a real quinn connection in Task 12.

---

### Task 12: `ClientNet::read_feedback_return` and the real-quinn tests of the return half

**Model:** sonnet

`ClientNet` opens the feedback stream and writes on it but never reads its return half. This adds an explicit, caller-driven read (no behaviour change for existing users: nothing reads it unless asked). PR 2's wasm path reads the stream in TypeScript; this is for the native client later and, now, for the browserless harness.

**Files:**
- Modify: `ghostframe-client-net/src/lib.rs` (new method after `send_input`, line 659)
- Modify: `ghostframe-client-net/tests/feedback.rs`

- [ ] **Step 1: Write the failing tests.** Append to `ghostframe-client-net/tests/feedback.rs`:

```rust
use ghostframe_lib::transport::webtransport::StatsWrite;
use ghostframe_protocol::stats::{
    MetricDescriptor, MetricGroup, MetricKind, MetricValue, ServerFrameReader, StatsSample,
    StatsSchema, SERVER_MSG_STATS_SAMPLE,
};

/// What the server writes on the feedback stream's return half reaches the
/// client's reader, on the real quinn pair (the wire path of the stats stream).
#[test]
fn a_stats_frame_written_by_the_server_reaches_the_client() {
    let (mut client, mut server, mut wt, mut now_us, base) = connected_session();
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);
    assert!(
        wt.note_stats_subscribe(),
        "the Hello arrived on the feedback stream, so it is known"
    );

    let schema = text_schema();
    let frame = StatsSample {
        schema_id: schema.schema_id,
        timestamp_us: 77,
        values: vec![MetricValue::Text("tile".into())],
    }
    .encode_frame();
    let conn = server.connections.values_mut().next().expect("connection");
    assert_eq!(wt.write_stats_frame(conn, &frame), StatsWrite::Written);
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);

    let got = client.read_feedback_return(now_us);
    assert_eq!(got, frame, "the bytes arrive unchanged, on the return half");
    assert!(
        client.read_feedback_return(now_us).is_empty(),
        "a read drains: the same bytes are not returned twice"
    );
}

fn text_schema() -> StatsSchema {
    StatsSchema {
        schema_id: 1,
        server_version: "t".into(),
        host_name: "h".into(),
        metrics: vec![MetricDescriptor {
            id: 1,
            name: "n".into(),
            unit: "1".into(),
            kind: MetricKind::Text,
            group: MetricGroup::Other,
            label: "l".into(),
        }],
    }
}

/// With the client not reading, the stream's flow-control window fills. The
/// server must then refuse whole frames, never leave one torn: what the
/// client eventually reads decodes cleanly, in order, and contains exactly the
/// frames the server reported as written.
#[test]
fn a_full_window_refuses_whole_frames_and_never_tears_one() {
    let (mut client, mut server, mut wt, mut now_us, base) = connected_session();
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);
    assert!(wt.note_stats_subscribe());
    let schema = text_schema();

    let make = |i: u64| {
        StatsSample {
            schema_id: schema.schema_id,
            timestamp_us: i,
            values: vec![MetricValue::Text("x".repeat(200))],
        }
        .encode_frame()
    };

    let mut written: Vec<u64> = Vec::new();
    let mut blocked = false;
    for i in 0..200_000u64 {
        let conn = server.connections.values_mut().next().expect("connection");
        match wt.write_stats_frame(conn, &make(i)) {
            StatsWrite::Written => written.push(i),
            StatsWrite::Blocked => {
                blocked = true;
                break;
            }
            other => panic!("frame {i}: {other:?}"),
        }
    }
    assert!(
        blocked,
        "premise: 200k x ~205 bytes overruns quinn's 1.25 MB default stream window"
    );
    assert!(
        wt.stats_tail_len() > 0 || !written.is_empty(),
        "premise: something was written before the block"
    );

    // The client drains; credit returns; the owed tail is flushed by the next
    // write, then one more frame goes after it.
    let mut reader = ServerFrameReader::default();
    let mut seen: Vec<u64> = Vec::new();
    let drain = |client: &mut ClientNet,
                 server: &mut _,
                 wt: &mut _,
                 now_us: &mut u64,
                 reader: &mut ServerFrameReader,
                 seen: &mut Vec<u64>| {
        for _ in 0..400 {
            pump_with_wt(client, server, wt, base, now_us, 8);
            reader.push(&client.read_feedback_return(*now_us));
            while let Some(f) = reader.next_frame() {
                let (ty, payload) = f.expect("no torn or oversize frame");
                assert_eq!(ty, SERVER_MSG_STATS_SAMPLE);
                let s = StatsSample::decode_payload(&payload, &text_schema())
                    .expect("every frame that arrives is whole");
                seen.push(s.timestamp_us);
            }
        }
    };
    drain(&mut client, &mut server, &mut wt, &mut now_us, &mut reader, &mut seen);

    let last = u64::MAX;
    let conn = server.connections.values_mut().next().expect("connection");
    assert_eq!(
        wt.write_stats_frame(conn, &make(last)),
        StatsWrite::Written,
        "credit is back: the tail flushes and the new frame follows"
    );
    written.push(last);
    drain(&mut client, &mut server, &mut wt, &mut now_us, &mut reader, &mut seen);

    assert_eq!(wt.stats_tail_len(), 0, "nothing is left owed");
    assert_eq!(reader.buffered(), 0, "no half frame is left in the reader");
    assert_eq!(
        seen, written,
        "exactly the frames reported Written, in order, none torn, none refused ones"
    );
}
```

Add the imports this needs at the top of the file next to `use common::{...}`: `use ghostframe_client_net::ClientNet;`. (`connected_session` and `pump_with_wt` are already imported.) The `drain` closure's `server` and `wt` parameters are typed by inference from the call; if the compiler cannot infer them, write the concrete types `&mut ghostframe_lib::transport::quic::QuicServer` and `&mut ghostframe_lib::transport::webtransport::WebTransportServer`.

- [ ] **Step 2: Run and see it fail**

```bash
cargo test -p ghostframe-client-net --test feedback a_stats_frame 2>&1 | tail -12
```

Expected: `no method named 'read_feedback_return' found for struct 'ClientNet'`.

- [ ] **Step 3: Implement.** In `ghostframe-client-net/src/lib.rs` after `send_input` (ends line 659) add:

```rust
    /// Bytes the server has written on the feedback stream's return half
    /// since the last call (empty before the stream exists or when nothing
    /// arrived). Nothing calls this unless the embedder asks: the stats
    /// stream (`ghostframe_protocol::stats`) is opt-in, and a client that
    /// never subscribes never has anything to read.
    ///
    /// Reading returns flow-control credit to the server, so the call also
    /// flushes the update.
    pub fn read_feedback_return(&mut self, now_us: u64) -> Vec<u8> {
        let Some(sid) = self.feedback_stream else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(conn) = self.endpoint.connection() {
            let mut recv = conn.recv_stream(sid);
            if let Ok(mut chunks) = recv.read(true) {
                // `Ok(None)` is the server finishing the stream and an `Err`
                // is a reset or no data yet: either way there is nothing more
                // to take now.
                while let Ok(Some(chunk)) = chunks.next(usize::MAX) {
                    out.extend_from_slice(&chunk.bytes);
                }
                let _ = chunks.finalize();
            }
        }
        let now = self.instant(now_us);
        self.endpoint.drive_outgoing(now);
        out
    }
```

- [ ] **Step 4: Run and see them pass**

```bash
cargo test -p ghostframe-client-net --test feedback 2>&1 | tail -12
```

Expected: the two new tests and the existing ones pass. If `a_full_window_refuses_whole_frames_and_never_tears_one` reports the premise failed (`blocked` false), the window is larger than assumed: read `ClientNet::connect`'s `TransportConfig` (line 261, default `stream_receive_window`), raise the iteration cap or the frame size, and keep the premise assertion: a test that never reaches the block proves nothing.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-net/src/lib.rs ghostframe-client-net/tests/feedback.rs
git commit -m "$(cat <<'EOF'
feat(client-net): read the feedback stream's return half; test the stats wire path

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 6: Mutation check.** (a) In `write_stats_frame` change `self.stats_tail = frame[n..].to_vec(); StatsWrite::Written` to drop the tail (`StatsWrite::Written` only): the tear test fails (`seen != written`, or a frame fails to decode). (b) Remove the early `return StatsWrite::Blocked;` after the tail flush so a frame is written behind an unfinished tail: the tear test fails. (c) Make `read_feedback_return` return `Vec::new()`: the first test fails. Restore with `git checkout -- ghostframe-lib/src/transport/webtransport.rs ghostframe-client-net/src/lib.rs`; rerun green.

---

### Task 13: Subscription state, the `0x0B` dispatch arm, and `forget_session`

**Model:** sonnet

**Files:**
- Create: `ghostframe-lib/src/transport/io_bridge/stats_service.rs` (state and subscribe handling; the sampler is Task 14)
- Modify: `ghostframe-lib/src/transport/io_bridge.rs` (module declaration after `mod session;` line 66; three fields; initialisers; the dispatch arm; the doc list at line 1870)
- Modify: `ghostframe-lib/src/transport/io_bridge/session.rs` (`forget_session`, line 402)

- [ ] **Step 1: Write the failing tests.** Create `ghostframe-lib/src/transport/io_bridge/stats_service.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A bridge with one live session, `h`, and the stats writes captured
    /// instead of sent (a unit test has no quinn connection).
    async fn bridge() -> (IoBridge, ConnectionHandle) {
        let (our_end, _peer) = tokio::net::UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("QuicServer::new");
        let mut b = IoBridge::new_with_stream_for_test(our_end, server);
        let h = ConnectionHandle(0);
        b.wt_sessions.insert(h, WebTransportServer::default());
        b.last_hello_from = Some(h);
        b.stats_capture_for_test = Some(Vec::new());
        (b, h)
    }

    #[tokio::test]
    async fn a_subscribe_message_turns_the_subscription_on_and_off() {
        let (mut b, h) = bridge().await;
        assert!(b.stats_sessions.is_empty(), "no state before a subscribe");
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        assert!(b.stats_sessions.get(&h).is_some_and(|s| s.subscribed));
        b.dispatch_feedback_bytes(h, &[0x0B, 0]);
        assert!(b.stats_sessions.get(&h).is_some_and(|s| !s.subscribed));
    }

    #[tokio::test]
    async fn a_subscribe_from_a_session_that_never_said_hello_is_ignored() {
        let (mut b, _h) = bridge().await;
        let stranger = ConnectionHandle(5);
        b.wt_sessions.insert(stranger, WebTransportServer::default());
        b.dispatch_feedback_bytes(stranger, &[0x0B, 1]);
        assert!(
            b.stats_sessions.is_empty(),
            "only the live session may switch the stream on: the existing HELLO gate"
        );
    }

    #[tokio::test]
    async fn a_malformed_subscribe_is_ignored_counted_and_keeps_the_stream_in_step() {
        let (mut b, h) = bridge().await;
        // `on` = 7 is malformed; the message is fixed-length, so the valid
        // subscribe behind it in the same read must still be applied.
        b.dispatch_feedback_bytes(h, &[0x0B, 7, 0x0B, 1]);
        assert!(b.stats_sessions.get(&h).is_some_and(|s| s.subscribed));
        assert_eq!(b.stats_counters.subscribes_malformed, 1);
        b.dispatch_feedback_bytes(h, &[0x0B, 9]);
        assert_eq!(b.stats_counters.subscribes_malformed, 2);
        assert!(
            b.stats_sessions.get(&h).is_some_and(|s| s.malformed_logged),
            "the log line is once per session; the count is not"
        );
    }

    #[tokio::test]
    async fn a_subscribe_split_across_reads_is_applied_when_whole() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B]);
        assert!(b.stats_sessions.is_empty(), "half a message does nothing");
        b.dispatch_feedback_bytes(h, &[1]);
        assert!(b.stats_sessions.get(&h).is_some_and(|s| s.subscribed));
    }

    #[tokio::test]
    async fn forgetting_a_session_clears_its_subscription() {
        // quinn reuses connection handles: a stale subscription would leak
        // into whichever connection gets this handle next.
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        assert!(b.stats_sessions.contains_key(&h));
        b.forget_session(h);
        assert!(
            !b.stats_sessions.contains_key(&h),
            "forget_session must clear the stats subscription"
        );
    }
}
```

- [ ] **Step 2: Declare the module so the test is built.** In `io_bridge.rs` after the `mod session; use session::{...};` block (the `use session::...` lines end near line 72) add:

```rust
#[path = "io_bridge/stats_service.rs"]
mod stats_service;
```

- [ ] **Step 3: Run and see it fail**

```bash
cargo test -p ghostframe-lib --lib stats_service 2>&1 | tail -15
```

Expected: errors such as `no field 'stats_sessions' on type 'IoBridge'`.

- [ ] **Step 4: Implement.**

(a) In `stats_service.rs`, above the test module:

```rust
//! The connection-stats service: per-session subscription state, the 1 Hz
//! sampler and the frame writes (spec `docs/specs/stats-panel-design.md`
//! section 1.2).
//!
//! It reads counters that already exist and adds no per-datagram or per-tile
//! work: a subscribed session costs one `StatsInputs` read a second. Nothing
//! is sent before a subscribe or after an unsubscribe.

use super::*;

/// The sampling period.
pub(super) const STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// One session's subscription. Keyed by connection handle in
/// `IoBridge::stats_sessions` and removed by `forget_session`: quinn reuses
/// handles, so a leftover entry would alias the next connection.
#[derive(Debug, Default)]
pub(super) struct StatsSession {
    /// The client asked for stats and has not asked to stop.
    pub(super) subscribed: bool,
    /// A schema is owed: set by every `on`, cleared when one is written.
    pub(super) schema_pending: bool,
    /// The id of the last schema written; samples are built against it.
    pub(super) schema_id: Option<u16>,
    /// When the next sample (or schema retry) is due; `None` means now.
    pub(super) next_sample_at: Option<std::time::Instant>,
    /// A malformed subscribe has been logged for this session.
    pub(super) malformed_logged: bool,
    /// A dropped frame has been logged (at INFO) since the last `on`: a stock
    /// deploy logs `ghostframe=info`, and a stream that silently fails (no
    /// stream pinned, permanently blocked, a table that no longer encodes)
    /// must be visible there, once, not only in DEBUG counters.
    pub(super) drop_logged: bool,
}

/// What the service has done, cumulative for the process.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct StatsCounters {
    pub(super) schemas_sent: u64,
    pub(super) samples_sent: u64,
    /// Frames refused because the stream had no room or owed a tail.
    pub(super) frames_blocked: u64,
    /// Frames with nowhere to go: no stream, a gone stream, or no connection.
    pub(super) frames_no_stream: u64,
    /// Frames that could not be built within the wire limits.
    pub(super) encode_failures: u64,
    pub(super) subscribes_malformed: u64,
}

impl IoBridge {
    /// Apply a well-formed `STATS_SUBSCRIBE` from the live session `from`.
    ///
    /// `on` owes a schema (even a repeated `on`: that is how a reopened panel
    /// resynchronises) and makes the first sample due at once. `off` stops
    /// everything and owes nothing.
    pub(super) fn apply_stats_subscribe(&mut self, from: ConnectionHandle, on: bool) {
        let pinned = self
            .wt_sessions
            .get_mut(&from)
            .is_some_and(|wt| wt.note_stats_subscribe());
        let st = self.stats_sessions.entry(from).or_default();
        st.subscribed = on;
        st.schema_pending = on;
        st.next_sample_at = None;
        st.drop_logged = false;
        tracing::debug!(target: LOG_TARGET, ?from, on, pinned, "stats subscription changed");
        if !on {
            let c = self.stats_counters;
            tracing::debug!(
                target: LOG_TARGET,
                ?from,
                schemas_sent = c.schemas_sent,
                samples_sent = c.samples_sent,
                frames_blocked = c.frames_blocked,
                frames_no_stream = c.frames_no_stream,
                encode_failures = c.encode_failures,
                "stats stream stopped"
            );
        }
    }

    /// A `STATS_SUBSCRIBE` that did not decode: counted always, logged once
    /// per session. The message is fixed-length, so the caller still skips it.
    pub(super) fn note_malformed_stats_subscribe(&mut self, from: ConnectionHandle) {
        self.stats_counters.subscribes_malformed += 1;
        let st = self.stats_sessions.entry(from).or_default();
        if !st.malformed_logged {
            st.malformed_logged = true;
            tracing::warn!(
                target: LOG_TARGET,
                ?from,
                "malformed stats subscribe; ignored (logged once per session)"
            );
        }
    }
}
```

(b) In `io_bridge.rs` add to the `IoBridge` struct, after `session_started_at: Option<std::time::Instant>,` (line 736):

```rust
    /// Stats subscriptions, by connection handle. Cleared per handle by
    /// `forget_session`; see `stats_service`.
    stats_sessions: HashMap<ConnectionHandle, stats_service::StatsSession>,
    /// What the stats service has done (cumulative).
    stats_counters: stats_service::StatsCounters,
    /// The last schema id handed out. Process-wide and monotonic (skipping 0),
    /// so a schema from a new connection never repeats one the client saw.
    stats_schema_seq: u16,
    /// Unit tests only: when `Some`, stats frames are recorded here and
    /// reported as written instead of going to a connection.
    #[cfg(test)]
    stats_capture_for_test: Option<Vec<(ConnectionHandle, Vec<u8>)>>,
```

and to the initialiser, after `session_started_at: None,` (line 1487):

```rust
            stats_sessions: HashMap::new(),
            stats_counters: stats_service::StatsCounters::default(),
            stats_schema_seq: 0,
            #[cfg(test)]
            stats_capture_for_test: None,
```

(c) The dispatch arm. In `dispatch_feedback_bytes`, extend the `use ghostframe_protocol::feedback_stream::{...}` (line 1888) to:

```rust
        use ghostframe_protocol::feedback_stream::{
            ResetRequest, RESET_REQUEST_MSG_TYPE, RESET_REQUEST_SIZE, STATS_SUBSCRIBE_MSG_TYPE,
            STATS_SUBSCRIBE_SIZE,
        };
        use ghostframe_protocol::stats::StatsSubscribe;
```

Add this arm before `crate::transport::input_inject::INPUT_MSG_TYPE => {`:

```rust
                STATS_SUBSCRIBE_MSG_TYPE => {
                    if buf.len() - offset < STATS_SUBSCRIBE_SIZE {
                        break;
                    }
                    match StatsSubscribe::decode(&buf[offset..]) {
                        Ok(msg) => self.apply_stats_subscribe(from, msg.on),
                        // Fixed length: the stream stays in step, so the
                        // message is skipped and the next one still parses.
                        Err(_) => self.note_malformed_stats_subscribe(from),
                    }
                    offset += STATS_SUBSCRIBE_SIZE;
                }
```

and add a doc line to the list above the function (after the `0x05 INPUT_MSG_TYPE` bullet, line 1870): `/// - \`0x0B\` STATS_SUBSCRIBE_MSG_TYPE (2 bytes) — start/stop the stats stream`.

(d) In `session.rs` `forget_session`, after `self.rechecker.forget(&handle);` add: `self.stats_sessions.remove(&handle);`.

- [ ] **Step 5: Run and see them pass.** Expect `dead_code` warnings for `service`-only items (`STATS_INTERVAL`, `schema_id`, `next_sample_at` reads) until Task 14; run tests only:

```bash
cargo test -p ghostframe-lib --lib stats_service 2>&1 | tail -12
```

Expected: 5 tests pass.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge/stats_service.rs ghostframe-lib/src/transport/io_bridge.rs ghostframe-lib/src/transport/io_bridge/session.rs
git commit -m "$(cat <<'EOF'
feat(lib): stats subscription state, the 0x0B dispatch arm, forget_session cleanup

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 7: Mutation check.** (a) Remove the `self.stats_sessions.remove(&handle);` line in `forget_session`: `forgetting_a_session_clears_its_subscription` fails. (b) In the dispatch arm replace `offset += STATS_SUBSCRIBE_SIZE;` with `break;` after the `Err` case only (i.e. stop parsing on a malformed one): the "keeps the stream in step" test fails. (c) Delete the whole `STATS_SUBSCRIBE_MSG_TYPE =>` arm: the first test fails (the byte falls to the `unknown` arm and the tail is discarded). Restore with `git checkout -- ghostframe-lib/src/transport/io_bridge.rs ghostframe-lib/src/transport/io_bridge/session.rs`; rerun green.

---

### Task 14: The 1 Hz sampler, the wake deadline and the post-turn hook

**Model:** sonnet

**Facts this task rests on:**
- The event loop sleeps to `next_wake_deadline()` (`io_bridge.rs:2117`); on a static screen with an idle client that is `None`, so without a stats entry in it a subscribed session would get a sample only when something else woke the loop.
- `post_turn` (`io_bridge.rs:2518`) runs `drain_app_events` (which dispatches the subscribe) and then a second `drain_outbound`; a frame written between the two leaves in the same turn.
- Time: `now_std()` is tokio's clock (virtual under the harness). `self.epoch` is its zero (`display_mode` uses `now_std().duration_since(self.epoch)`). `session_started_at` is a real-clock `std::time::Instant` (`session.rs:290`), so the uptime reads the real clock, as `on_connection_gone`'s `session_secs` does.
- Sources: RTT `reliable_emitter.rtt` (`has_sample()`, `srtt()`; `rtt_estimator.rs:97,140`), falling back to quinn's `live_path_rtt()` (`bwe_glue.rs:613`) before the first ack sample; estimate `bwe.snapshot().bitrate_bps` (`bwe/mod.rs:59,284`); rate target `h264.stats.rate_target_bps` (`h264_tx.rs:184`); sent bytes `WebTransportServer::datagram_bytes_enqueued()`; counters `reliable_emitter.stats.{loss_path_resends,nack_hit,nack_miss}` (`emitter.rs:142-152`) and `instr.losses_declared` (`transport_probe.rs:95`, the ledger's verdicts); the H.264 backend `h264.backend_kind` and `h264.availability` (`h264_tx.rs:387,421`).

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge/stats_service.rs`
- Modify: `ghostframe-lib/src/transport/io_bridge.rs` (`next_wake_deadline`, `post_turn`)

- [ ] **Step 1: Write the failing tests.** Append inside `mod tests` in `stats_service.rs`:

```rust
    use ghostframe_protocol::stats::{
        MetricValue, ServerFrameReader, StatsSample, StatsSchema, SERVER_MSG_STATS_SAMPLE,
        SERVER_MSG_STATS_SCHEMA,
    };

    /// Everything captured since the last call, decoded as `(type, payload)`.
    fn captured(b: &mut IoBridge) -> Vec<(u8, Vec<u8>)> {
        let raw = std::mem::take(b.stats_capture_for_test.as_mut().expect("capture is on"));
        let mut r = ServerFrameReader::default();
        for (_, f) in raw {
            r.push(&f);
        }
        let mut out = Vec::new();
        while let Some(f) = r.next_frame() {
            out.push(f.expect("the server only writes whole frames"));
        }
        out
    }

    fn decode_schema(f: &(u8, Vec<u8>)) -> StatsSchema {
        assert_eq!(f.0, SERVER_MSG_STATS_SCHEMA);
        StatsSchema::decode_payload(&f.1).expect("schema decodes")
    }

    fn decode_sample(f: &(u8, Vec<u8>), schema: &StatsSchema) -> StatsSample {
        assert_eq!(f.0, SERVER_MSG_STATS_SAMPLE);
        StatsSample::decode_payload(&f.1, schema).expect("sample decodes against its schema")
    }

    fn num(schema: &StatsSchema, s: &StatsSample, name: &str) -> f64 {
        let i = schema.metrics.iter().position(|m| m.name == name).expect(name);
        match &s.values[i] {
            MetricValue::Num(x) => *x,
            MetricValue::Text(t) => panic!("{name} is text {t:?}"),
        }
    }

    fn text(schema: &StatsSchema, s: &StatsSample, name: &str) -> String {
        let i = schema.metrics.iter().position(|m| m.name == name).expect(name);
        match &s.values[i] {
            MetricValue::Text(t) => t.clone(),
            MetricValue::Num(x) => panic!("{name} is number {x}"),
        }
    }

    #[tokio::test]
    async fn nothing_is_sent_before_a_subscribe() {
        let (mut b, _h) = bridge().await;
        let now = now_std();
        b.service_stats(now);
        b.service_stats(now + std::time::Duration::from_secs(5));
        assert!(captured(&mut b).is_empty());
        assert_eq!(b.stats_counters, StatsCounters::default());
    }

    #[tokio::test]
    async fn a_subscribe_is_answered_with_a_schema_and_a_first_sample_in_one_turn() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        b.service_stats(now_std());
        let frames = captured(&mut b);
        assert_eq!(frames.len(), 2, "schema then sample: {frames:?}");
        let schema = decode_schema(&frames[0]);
        assert_ne!(schema.schema_id, 0);
        assert_eq!(schema.metrics.len(), crate::transport::stats_table::Stat::ALL.len());
        assert_eq!(schema.server_version, crate::transport::stats_table::SERVER_VERSION);
        let sample = decode_sample(&frames[1], &schema);
        assert_eq!(sample.schema_id, schema.schema_id);
        assert_eq!(sample.values.len(), schema.metrics.len());
        assert_eq!(b.stats_counters.schemas_sent, 1);
        assert_eq!(b.stats_counters.samples_sent, 1);
    }

    #[tokio::test]
    async fn samples_follow_once_a_second_and_never_faster() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        let t0 = now_std();
        b.service_stats(t0);
        let _ = captured(&mut b);

        b.service_stats(t0 + std::time::Duration::from_millis(400));
        b.service_stats(t0 + std::time::Duration::from_millis(999));
        assert!(captured(&mut b).is_empty(), "not due yet");

        b.service_stats(t0 + std::time::Duration::from_millis(1000));
        let frames = captured(&mut b);
        assert_eq!(frames.len(), 1, "one sample, no schema: {frames:?}");
        assert_eq!(frames[0].0, SERVER_MSG_STATS_SAMPLE);

        // Called twice at the same instant: still one.
        b.service_stats(t0 + std::time::Duration::from_millis(1000));
        assert!(captured(&mut b).is_empty());
    }

    #[tokio::test]
    async fn an_unsubscribe_stops_everything() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        let t0 = now_std();
        b.service_stats(t0);
        let _ = captured(&mut b);
        b.dispatch_feedback_bytes(h, &[0x0B, 0]);
        for s in 1..=5 {
            b.service_stats(t0 + std::time::Duration::from_secs(s));
        }
        assert!(captured(&mut b).is_empty(), "off means off, and no schema either");
    }

    #[tokio::test]
    async fn a_repeated_subscribe_resends_the_schema_with_a_new_id() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        let t0 = now_std();
        b.service_stats(t0);
        let first = decode_schema(&captured(&mut b)[0]);

        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        b.service_stats(t0 + std::time::Duration::from_millis(10));
        let frames = captured(&mut b);
        assert_eq!(frames.len(), 2, "schema and a sample at once: {frames:?}");
        let second = decode_schema(&frames[0]);
        assert_ne!(second.schema_id, first.schema_id, "the client clears its rings on a new id");
        assert_eq!(decode_sample(&frames[1], &second).schema_id, second.schema_id);
    }

    #[tokio::test]
    async fn a_forgotten_handle_that_quinn_reuses_gets_no_stats() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        b.forget_session(h);
        // The same handle, a different connection, which never subscribed.
        b.wt_sessions.insert(h, WebTransportServer::default());
        b.last_hello_from = Some(h);
        b.service_stats(now_std() + std::time::Duration::from_secs(5));
        assert!(captured(&mut b).is_empty(), "the old subscription must not alias the new connection");
    }

    #[tokio::test]
    async fn a_sample_reads_the_live_counters() {
        let (mut b, h) = bridge().await;
        // Distinct non-zero values, so a swapped source cannot pass.
        b.reliable_emitter.stats.loss_path_resends = 5;
        b.reliable_emitter.stats.nack_hit = 2;
        b.reliable_emitter.stats.nack_miss = 3;
        b.instr.losses_declared = 9;
        b.reliable_emitter.rtt.on_sample(std::time::Duration::from_millis(40));
        b.session_started_at = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(5));
        b.frame_mode = crate::tile::FrameMode::TileCodec;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        b.service_stats(now_std());
        let frames = captured(&mut b);
        let schema = decode_schema(&frames[0]);
        let s = decode_sample(&frames[1], &schema);

        assert_eq!(num(&schema, &s, "ghostframe.loss.path_resends"), 5.0);
        assert_eq!(num(&schema, &s, "ghostframe.loss.nacks_received"), 5.0, "hit + miss");
        assert_eq!(num(&schema, &s, "ghostframe.loss.verdicts"), 9.0);
        let rtt = num(&schema, &s, "ghostframe.rtt.smoothed");
        assert!((rtt - 40.0).abs() < 0.5, "the first sample initialises srtt: {rtt}");
        assert!(num(&schema, &s, "ghostframe.session.uptime") >= 5.0);
        assert_eq!(
            num(&schema, &s, "ghostframe.bwe.estimate"),
            b.bwe.snapshot().bitrate_bps as f64,
            "the estimate is the bridge's own, not a copy that can drift"
        );
        assert!(num(&schema, &s, "ghostframe.bwe.estimate") > 0.0, "a zero estimate is a dead probe");
        assert_eq!(text(&schema, &s, "ghostframe.frame.mode"), "tile");
        assert_eq!(text(&schema, &s, "ghostframe.frame.mode_reason"), "none");
        assert_eq!(text(&schema, &s, "ghostframe.h264.backend"), "none");
    }

    #[tokio::test]
    async fn the_event_loop_wakes_for_the_next_sample() {
        let (mut b, h) = bridge().await;
        assert!(b.next_wake_deadline().is_none(), "premise: nothing else is pending");
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        let t0 = now_std();
        b.service_stats(t0);
        let due = b.next_wake_deadline().expect("a subscribed session needs a wake");
        // `>=` a whole interval, not `> t0`: without the `next_sample_at`
        // stamp the deadline falls back to "now", which is just after `t0`.
        assert!(
            due >= t0 + STATS_INTERVAL
                && due <= t0 + STATS_INTERVAL + std::time::Duration::from_millis(50),
            "{due:?}"
        );
        b.dispatch_feedback_bytes(h, &[0x0B, 0]);
        assert!(b.next_wake_deadline().is_none(), "an unsubscribed session needs none");
    }

    #[tokio::test]
    async fn a_dropped_frame_is_logged_once_per_subscription() {
        let (mut b, h) = bridge().await;
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        assert!(!b.stats_sessions[&h].drop_logged);
        assert!(b.note_stats_drop(h, "stream blocked"), "the first drop is the one that logs");
        assert!(!b.note_stats_drop(h, "stream blocked"), "later drops stay quiet");
        assert!(b.stats_sessions[&h].drop_logged);
        // A reopened panel is a new subscription and may log again.
        b.dispatch_feedback_bytes(h, &[0x0B, 1]);
        assert!(b.note_stats_drop(h, "stream blocked"));
    }
```

- [ ] **Step 2: Run and see it fail**

```bash
cargo test -p ghostframe-lib --lib stats_service 2>&1 | tail -15
```

Expected: `no method named 'service_stats' found for struct 'IoBridge'`.

- [ ] **Step 3: Implement.** In `stats_service.rs` add imports below `use super::*;`:

```rust
use crate::transport::stats_table::{self, StatsInputs};
use crate::transport::webtransport::StatsWrite;
use ghostframe_protocol::stats::StatsSample;
```

and extend `impl IoBridge` (a second `impl` block is fine) with:

```rust
impl IoBridge {
    /// The next schema id: process-wide, monotonic, never 0.
    fn next_stats_schema_id(&mut self) -> u16 {
        self.stats_schema_seq = self.stats_schema_seq.wrapping_add(1);
        if self.stats_schema_seq == 0 {
            self.stats_schema_seq = 1;
        }
        self.stats_schema_seq
    }

    /// Note that a frame could not be sent. The counters say how often; this
    /// puts one INFO line in a stock journal the first time per subscription
    /// (see `StatsSession::drop_logged`). `true` when it logged.
    fn note_stats_drop(&mut self, handle: ConnectionHandle, why: &str) -> bool {
        let c = self.stats_counters;
        let st = self.stats_sessions.entry(handle).or_default();
        if st.drop_logged {
            return false;
        }
        st.drop_logged = true;
        tracing::info!(
            target: LOG_TARGET,
            ?handle,
            why,
            frames_blocked = c.frames_blocked,
            frames_no_stream = c.frames_no_stream,
            encode_failures = c.encode_failures,
            "stats frame dropped (logged once per subscription; the counters keep running)"
        );
        true
    }

    /// Read the numbers one sample is built from. One place, so the table and
    /// the bridge cannot disagree about a source.
    fn stats_inputs(&self, handle: ConnectionHandle) -> StatsInputs {
        let rtt = if self.reliable_emitter.rtt.has_sample() {
            Some(self.reliable_emitter.rtt.srtt())
        } else {
            // Before the first acknowledgement sample: quinn's own path RTT.
            self.live_path_rtt()
        };
        let h264_backend = if self.h264.availability != h264_refs::H264Availability::Available {
            "unavailable"
        } else if self.h264.backend_kind.is_empty() {
            "none"
        } else {
            self.h264.backend_kind
        };
        StatsInputs {
            rtt_ms: rtt.map_or(0.0, |d| d.as_secs_f64() * 1e3),
            // Real clock on purpose: `session_started_at` is a real-clock
            // stamp (see `on_connection_gone`'s `session_secs`).
            uptime_s: self.session_started_at.map_or(0.0, |t| {
                std::time::Instant::now()
                    .saturating_duration_since(t)
                    .as_secs_f64()
            }),
            bwe_bps: self.bwe.snapshot().bitrate_bps as f64,
            rate_target_bps: f64::from(self.h264.stats.rate_target_bps),
            sent_bytes: self
                .wt_sessions
                .get(&handle)
                .map_or(0, |wt| wt.datagram_bytes_enqueued()) as f64,
            frame_mode: match self.frame_mode {
                crate::tile::FrameMode::H264 => "h264",
                crate::tile::FrameMode::TileCodec => "tile",
            },
            mode_reason: self.classifier.last_decision_reason(),
            h264_backend,
            loss_path_resends: self.reliable_emitter.stats.loss_path_resends as f64,
            loss_verdicts: self.instr.losses_declared as f64,
            nacks_received: (self.reliable_emitter.stats.nack_hit
                + self.reliable_emitter.stats.nack_miss) as f64,
        }
    }

    /// Write one frame, counting what happened. `true` when it is on the
    /// stream.
    fn write_stats_counted(&mut self, handle: ConnectionHandle, frame: &[u8]) -> bool {
        #[cfg(test)]
        if let Some(log) = self.stats_capture_for_test.as_mut() {
            log.push((handle, frame.to_vec()));
            return true;
        }
        let outcome = match (
            self.wt_sessions.get_mut(&handle),
            self.server.connections.get_mut(&handle),
        ) {
            (Some(wt), Some(conn)) => wt.write_stats_frame(conn, frame),
            _ => StatsWrite::NoStream,
        };
        match outcome {
            StatsWrite::Written => true,
            StatsWrite::Blocked => {
                self.stats_counters.frames_blocked += 1;
                self.note_stats_drop(handle, "stream blocked");
                false
            }
            StatsWrite::NoStream | StatsWrite::StreamGone => {
                self.stats_counters.frames_no_stream += 1;
                self.note_stats_drop(handle, "no stats stream");
                false
            }
        }
    }

    /// Send what is due to every subscribed session: the schema it is owed,
    /// then a sample, at most once per [`STATS_INTERVAL`]. A frame that cannot
    /// be written is dropped and counted; a schema that could not be written
    /// stays owed and is retried at the next interval (a sample is never sent
    /// against a schema the client has not received).
    ///
    /// Cheap on every loop turn (turns follow datagram batches, work is due
    /// at most once a second): when no subscribed session is due it returns
    /// after one pass over the (tiny) session map, with no allocation. The
    /// handle list below is built only when something is due.
    pub(super) fn service_stats(&mut self, now: std::time::Instant) {
        let any_due = self
            .stats_sessions
            .values()
            .any(|s| s.subscribed && !s.next_sample_at.is_some_and(|t| now < t));
        if !any_due {
            return;
        }
        let mut handles: Vec<ConnectionHandle> = self
            .stats_sessions
            .iter()
            .filter(|(_, s)| s.subscribed)
            .map(|(h, _)| *h)
            .collect();
        handles.sort_by_key(|h| h.0);
        for h in handles {
            let Some(st) = self.stats_sessions.get_mut(&h) else {
                continue;
            };
            if st.next_sample_at.is_some_and(|t| now < t) {
                continue;
            }
            st.next_sample_at = Some(now + STATS_INTERVAL);
            let schema_pending = st.schema_pending;
            let mut schema_id = st.schema_id;

            if schema_pending {
                let id = self.next_stats_schema_id();
                let Ok(frame) = stats_table::schema(id).try_encode_frame() else {
                    self.stats_counters.encode_failures += 1;
                    self.note_stats_drop(h, "schema does not encode");
                    continue;
                };
                if !self.write_stats_counted(h, &frame) {
                    continue;
                }
                self.stats_counters.schemas_sent += 1;
                schema_id = Some(id);
                if let Some(st) = self.stats_sessions.get_mut(&h) {
                    st.schema_pending = false;
                    st.schema_id = schema_id;
                }
            }
            let Some(schema_id) = schema_id else {
                continue;
            };

            let sample = StatsSample {
                schema_id,
                timestamp_us: now.saturating_duration_since(self.epoch).as_micros() as u64,
                values: self.stats_inputs(h).values(),
            };
            let Ok(frame) = sample.try_encode_frame() else {
                self.stats_counters.encode_failures += 1;
                self.note_stats_drop(h, "sample does not encode");
                continue;
            };
            if self.write_stats_counted(h, &frame) {
                self.stats_counters.samples_sent += 1;
            }
        }
    }

    /// When the service next has something to do, for the event loop's timer.
    pub(super) fn stats_next_due(&self) -> Option<std::time::Instant> {
        self.stats_sessions
            .values()
            .filter(|s| s.subscribed)
            .map(|s| s.next_sample_at.unwrap_or_else(now_std))
            .min()
    }
}
```

In `io_bridge.rs` `next_wake_deadline`, add `let stats = self.stats_next_due();` after `let deferred = ...;` and extend the array to:

```rust
        [
            quic, display, close, recheck, admission, pto, loss_time, deferred, stats,
        ]
```

In `post_turn`, after `self.drain_app_events(); self.server.drain_endpoint_events();` (and before the second `self.drain_outbound().await?;`) add:

```rust
        // The stats subscribe just dispatched (and the 1 Hz sample) leave in
        // the drain below, in this turn.
        self.service_stats(now_std());
```

- [ ] **Step 4: Run and see them pass; clippy is now clean for non-test code**

```bash
cargo test -p ghostframe-lib --lib stats_service 2>&1 | tail -15
cargo clippy -p ghostframe-lib --all-targets -- -D warnings 2>&1 | tail -20
```

Expected: 14 `stats_service` tests pass; clippy clean. If `a_sample_reads_the_live_counters` finds `ghostframe.rtt.smoothed` off, check that `on_sample` was the first sample (the estimator initialises `srtt` to it). If the wake test's premise (`next_wake_deadline().is_none()`) fails on a fresh bridge, another source is pending in this build: assert the stats deadline separately via `b.stats_next_due()` and keep the `is_some` check on `next_wake_deadline`.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge/stats_service.rs ghostframe-lib/src/transport/io_bridge.rs
git commit -m "$(cat <<'EOF'
feat(lib): per-session 1 Hz stats sampler, its wake deadline and post-turn hook

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 6: Mutation check.**
(a) In `service_stats` delete `st.next_sample_at = Some(now + STATS_INTERVAL);`: `samples_follow_once_a_second_and_never_faster` fails, and so does `the_event_loop_wakes_for_the_next_sample` (its `due >= t0 + STATS_INTERVAL` bound; the deadline would fall back to "now", just after `t0`). If only the 1 Hz test goes red, the wake test's bound has been loosened: tighten it.
(f) Invert the early-return (`if any_due { return; }`): the first-sample and 1 Hz tests fail. (g) Make `note_stats_drop` return `true` unconditionally: `a_dropped_frame_is_logged_once_per_subscription` fails.
(b) In `stats_inputs` change `self.instr.losses_declared` to `self.reliable_emitter.stats.loss_path_resends`: `a_sample_reads_the_live_counters` fails (5.0 vs 9.0).
(c) Remove `stats` from the array in `next_wake_deadline`: `the_event_loop_wakes_for_the_next_sample` fails.
(d) Remove the `service_stats(now_std())` call from `post_turn`: no unit test sees it (they call `service_stats` directly); the browserless scene in Task 16 does, so it is checked there.
(e) In `service_stats` filter `.filter(|(_, s)| s.subscribed)` removed: `an_unsubscribe_stops_everything` fails.
Restore with `git checkout -- ghostframe-lib/src/transport/io_bridge/stats_service.rs ghostframe-lib/src/transport/io_bridge.rs`; rerun green.

---

### Task 15: Harness `StatsTap`, `RunOptions::stats` and the result field

**Model:** sonnet

The browserless harness drives a real `ClientNet` against the real `IoBridge`. The tap sends the subscribe through the client's feedback stream (`send_input` writes arbitrary bytes there), reads the return half with `read_feedback_return`, and decodes with the protocol crate: the same decode PR 2's client will use.

**Files:**
- Create: `ghostframe-e2e/src/harness/stats_tap.rs`
- Modify: `ghostframe-e2e/src/harness/mod.rs` (add `pub mod stats_tap;` after `pub mod scroll_scene;`)
- Modify: `ghostframe-e2e/src/harness/browserless.rs` (`RunOptions`, `ClientProbe`, `BrowserlessResult`, `drive_session`, result construction)

- [ ] **Step 1: Write `stats_tap.rs`.**

```rust
//! A stats subscriber for the browserless harness: sends `STATS_SUBSCRIBE`
//! through the scene's real `ClientNet`, reads what the server writes on the
//! feedback stream's return half, and decodes it with the protocol crate.
//!
//! It exists so the server half of the stats panel is testable with no UI:
//! the numbers a scene reads here crossed the real QUIC stream, the real
//! framing and the real decoder.

use ghostframe_client_net::ClientNet;
use ghostframe_protocol::stats::{
    ServerFrameReader, StatsSample, StatsSchema, StatsSubscribe, SERVER_MSG_STATS_SAMPLE,
    SERVER_MSG_STATS_SCHEMA,
};
use std::time::Duration;

/// When the scene's client subscribes and unsubscribes, as offsets from the
/// scene's start. `None` everywhere is a listener that never subscribes: the
/// control for "nothing is sent before a subscribe".
#[derive(Debug, Clone, Copy, Default)]
pub struct StatsPlan {
    pub subscribe_at: Option<Duration>,
    /// Subscribe again (a repeated `on`).
    pub resubscribe_at: Option<Duration>,
    pub unsubscribe_at: Option<Duration>,
}

/// What the tap saw, in virtual microseconds since the scene started.
#[derive(Debug, Clone, Default)]
pub struct StatsProbe {
    pub subscribes_sent: u32,
    pub unsubscribes_sent: u32,
    /// `(arrival_us, schema)`.
    pub schemas: Vec<(u64, StatsSchema)>,
    /// `(arrival_us, sample)`, decoded against the schema current at arrival.
    pub samples: Vec<(u64, StatsSample)>,
    /// Samples that arrived before a schema or did not match it.
    pub rejected: u32,
    /// Frames that did not parse (oversize, bad schema payload).
    pub frame_errors: u32,
    /// Bytes read off the return half in total: non-zero proves the read path
    /// ran, which a "nothing arrived" assertion needs.
    pub bytes_read: u64,
}

pub struct StatsTap {
    plan: StatsPlan,
    reader: ServerFrameReader,
    current: Option<StatsSchema>,
    probe: StatsProbe,
    sent_on: bool,
    sent_again: bool,
    sent_off: bool,
}

fn due(at: Option<Duration>, vt_us: u64) -> bool {
    at.is_some_and(|d| vt_us >= d.as_micros() as u64)
}

impl StatsTap {
    pub fn new(plan: StatsPlan) -> Self {
        Self {
            plan,
            reader: ServerFrameReader::default(),
            current: None,
            probe: StatsProbe::default(),
            sent_on: false,
            sent_again: false,
            sent_off: false,
        }
    }

    /// Send what is due and decode what arrived. Call once per harness
    /// iteration with the current virtual time.
    pub fn step(&mut self, client: &mut ClientNet, vt_us: u64) {
        if !self.sent_on && due(self.plan.subscribe_at, vt_us) {
            self.sent_on = true;
            self.probe.subscribes_sent += 1;
            client.send_input(StatsSubscribe { on: true }.encode().to_vec(), vt_us);
        }
        if self.sent_on && !self.sent_again && due(self.plan.resubscribe_at, vt_us) {
            self.sent_again = true;
            self.probe.subscribes_sent += 1;
            client.send_input(StatsSubscribe { on: true }.encode().to_vec(), vt_us);
        }
        if self.sent_on && !self.sent_off && due(self.plan.unsubscribe_at, vt_us) {
            self.sent_off = true;
            self.probe.unsubscribes_sent += 1;
            client.send_input(StatsSubscribe { on: false }.encode().to_vec(), vt_us);
        }

        let bytes = client.read_feedback_return(vt_us);
        self.probe.bytes_read += bytes.len() as u64;
        self.reader.push(&bytes);
        while let Some(frame) = self.reader.next_frame() {
            let (msg_type, payload) = match frame {
                Ok(f) => f,
                Err(_) => {
                    self.probe.frame_errors += 1;
                    continue;
                }
            };
            match msg_type {
                SERVER_MSG_STATS_SCHEMA => match StatsSchema::decode_payload(&payload) {
                    Ok(schema) => {
                        self.current = Some(schema.clone());
                        self.probe.schemas.push((vt_us, schema));
                    }
                    Err(_) => self.probe.frame_errors += 1,
                },
                SERVER_MSG_STATS_SAMPLE => match self.current.as_ref() {
                    Some(schema) => match StatsSample::decode_payload(&payload, schema) {
                        Ok(sample) => self.probe.samples.push((vt_us, sample)),
                        Err(_) => self.probe.rejected += 1,
                    },
                    None => self.probe.rejected += 1,
                },
                // An unknown server frame type is skipped by its length.
                _ => {}
            }
        }
    }

    pub fn finish(self) -> StatsProbe {
        self.probe
    }
}
```

- [ ] **Step 2: Write the failing wiring.** The scene test in Task 16 is the failing test; here, first make the harness compile with the new fields. In `ghostframe-e2e/src/harness/mod.rs` add `pub mod stats_tap;`.

In `browserless.rs`:

(a) Add to `RunOptions` (after `ack_chatter`):

```rust
    /// Run a stats subscriber (see `stats_tap`): when it subscribes and
    /// unsubscribes. `None` runs none, and `BrowserlessResult::client_stats`
    /// is then empty.
    pub stats: Option<crate::harness::stats_tap::StatsPlan>,
```

(b) Add to `ClientProbe` (line 2085):

```rust
    /// What the stats tap saw; empty without `RunOptions::stats`.
    pub stats: crate::harness::stats_tap::StatsProbe,
```

(c) Add to `BrowserlessResult` (after `client_cdf53`, line 681):

```rust
    /// The stats stream as the client saw it: schemas, samples, and the
    /// counts of what did not decode. Empty without `RunOptions::stats`.
    pub client_stats: crate::harness::stats_tap::StatsProbe,
```

(d) In `drive_session`, before the `loop {` at line ~1676 add:

```rust
    let mut stats_tap = opts.stats.map(crate::harness::stats_tap::StatsTap::new);
```

and immediately before `let new_events = client.take_events();` (line ~1807) add:

```rust
        if let Some(tap) = stats_tap.as_mut() {
            tap.step(&mut client, now_us(base));
        }
```

(e) Where `ClientProbe` is built (line 2066) add `stats: stats_tap.map(|t| t.finish()).unwrap_or_default(),`.

(f) Where `BrowserlessResult` is built (line 1374 area) add `client_stats: client_probe.stats,` after `client_cdf53: client_probe.cdf53,`.

- [ ] **Step 3: Compile the whole workspace's test targets** (a struct field added to `BrowserlessResult` or `RunOptions` is a compile error at any literal that omits it, and `cargo test -p` would hide it)

```bash
cargo clippy -p ghostframe-e2e --all-targets -- -D warnings 2>&1 | tail -20
grep -rn "BrowserlessResult {" ghostframe-e2e ghostframe-bench --include=*.rs | grep -v "pub struct"
```

Expected: clippy clean; the grep shows only the one construction in `browserless.rs`. If another construction appears, add `client_stats: Default::default()` there. `RunOptions` literals all end in `..Default::default()` (checked at the nine sites in `ghostframe-e2e/tests`).

- [ ] **Step 4: Commit**

```bash
git add ghostframe-e2e/src/harness/stats_tap.rs ghostframe-e2e/src/harness/mod.rs ghostframe-e2e/src/harness/browserless.rs
git commit -m "$(cat <<'EOF'
test(e2e): a stats subscriber for the browserless harness

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 16: Browserless scenes that prove the server half end to end

**Model:** sonnet

**Files:** Create `ghostframe-e2e/tests/browserless_stats.rs`

Scene: 12 virtual seconds, 16x16 grid, a steady 8-tiles-per-tick region, 10 ms one-way delay. That carries acknowledgements back, so the RTT estimator and goog_cc have real samples. The positive control (test 1) is what makes the negative tests (2 and 3) non-vacuous: the same scene and the same tap do receive samples when subscribed.

- [ ] **Step 1: Write the tests.**

```rust
//! The server half of the stats panel, end to end and with no UI: a real
//! `ClientNet` subscribes on the feedback stream, the real `IoBridge` answers
//! on its return half, and the numbers are decoded with the protocol crate.
//!
//! Run: `cargo test --test browserless_stats -- --test-threads=1`

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, LinkOverrides, RunOptions,
    SceneLoad,
};
use ghostframe_e2e::harness::load_profile::{Churn, LoadProfile, PRODUCTION_CADENCE_US};
use ghostframe_e2e::harness::stats_tap::StatsPlan;
use ghostframe_e2e::netsim::NetProfile;
use ghostframe_protocol::stats::{MetricValue, StatsSample, StatsSchema};
use std::time::Duration;

const S: u64 = 1_000_000;

async fn run(plan: StatsPlan) -> BrowserlessResult {
    let scene = BrowserlessScene {
        seed: 7,
        tick_budget_floor_bytes: None,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Profile(LoadProfile {
            cadence_us: PRODUCTION_CADENCE_US,
            churn: Churn::Region { tiles_per_tick: 8 },
        }),
        cadence_us: PRODUCTION_CADENCE_US,
        net: NetProfile {
            delay_us: 10_000,
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(12),
        grid_cols: 16,
        grid_rows: 16,
    };
    run_browserless_with_options(
        scene,
        LinkOverrides::default(),
        RunOptions {
            stats: Some(plan),
            ..Default::default()
        },
    )
    .await
    .expect("scene ran")
}

fn index(schema: &StatsSchema, name: &str) -> usize {
    schema
        .metrics
        .iter()
        .position(|m| m.name == name)
        .unwrap_or_else(|| panic!("schema has no {name}"))
}

fn num(schema: &StatsSchema, s: &StatsSample, name: &str) -> f64 {
    match &s.values[index(schema, name)] {
        MetricValue::Num(x) => *x,
        MetricValue::Text(t) => panic!("{name} is text {t:?}"),
    }
}

fn subscribe_at_1s() -> StatsPlan {
    StatsPlan {
        subscribe_at: Some(Duration::from_secs(1)),
        ..Default::default()
    }
}

/// The positive control and the main gate: a subscribed session gets a schema,
/// then about one sample a second, whose numbers are live.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_subscribed_session_gets_a_schema_then_live_samples() {
    let r = run(subscribe_at_1s()).await;
    let st = &r.client_stats;
    println!(
        "STATS schemas={} samples={} rejected={} frame_errors={} bytes_read={}",
        st.schemas.len(),
        st.samples.len(),
        st.rejected,
        st.frame_errors,
        st.bytes_read
    );

    assert_eq!(st.subscribes_sent, 1);
    assert_eq!(st.schemas.len(), 1, "one subscribe, one schema");
    assert_eq!(st.frame_errors, 0);
    assert_eq!(st.rejected, 0, "every sample matched its schema");
    assert!(
        st.samples.len() >= 8,
        "11 s of subscription at 1 Hz, first sample with the schema: {}",
        st.samples.len()
    );

    let schema = &st.schemas[0].1;
    assert_ne!(schema.schema_id, 0);
    assert!(!schema.server_version.is_empty() && !schema.host_name.is_empty());

    // Timestamps: strictly increasing, about one second apart.
    for pair in st.samples.windows(2) {
        let gap = pair[1].1.timestamp_us - pair[0].1.timestamp_us;
        assert!(
            (900_000..=1_500_000).contains(&gap),
            "samples are ~1 s apart on the server's clock, got {gap} us"
        );
    }

    // The measurements are non-zero: a probe that never fires reads 0.
    let last = &st.samples.last().expect("samples").1;
    let bwe = num(schema, last, "ghostframe.bwe.estimate");
    assert!(bwe > 0.0, "bandwidth estimate: {bwe}");
    let rtt = num(schema, last, "ghostframe.rtt.smoothed");
    assert!(
        (20.0..1000.0).contains(&rtt),
        "RTT over a 2 x 10 ms link is at least 20 ms and well under a second: {rtt}"
    );
    let first_sent = num(schema, &st.samples[0].1, "ghostframe.sent.bytes");
    let last_sent = num(schema, last, "ghostframe.sent.bytes");
    assert!(
        last_sent > first_sent && last_sent > 0.0,
        "bytes handed to quinn grow while tiles are sent: {first_sent} -> {last_sent}"
    );
}

/// The negative control: the same scene and the same tap, never subscribing.
/// Vacuous without the positive test above, which proves the tap reads the
/// return half in this very scene.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn nothing_arrives_without_a_subscribe() {
    let r = run(StatsPlan::default()).await;
    let st = &r.client_stats;
    assert_eq!(st.subscribes_sent, 0, "premise: this client never subscribed");
    assert_eq!(st.bytes_read, 0, "not one byte on the return half");
    assert!(st.schemas.is_empty() && st.samples.is_empty());
}

/// After an unsubscribe the stream stops: no sample arrives later than the
/// message's own transit plus one in-flight frame.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_unsubscribe_stops_the_stream() {
    let r = run(StatsPlan {
        subscribe_at: Some(Duration::from_secs(1)),
        unsubscribe_at: Some(Duration::from_secs(6)),
        ..Default::default()
    })
    .await;
    let st = &r.client_stats;
    assert_eq!(st.unsubscribes_sent, 1);
    assert!(
        st.samples.len() >= 3,
        "premise: samples flowed before the unsubscribe: {}",
        st.samples.len()
    );
    let latest = st.samples.iter().map(|(t, _)| *t).max().expect("samples");
    assert!(
        latest < 6 * S + 300_000,
        "a sample arrived at {latest} us, long after the unsubscribe at 6 s"
    );
}

/// A repeated subscribe re-sends the schema under a new id, and samples then
/// follow the new one.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_repeated_subscribe_resends_the_schema_with_a_new_id() {
    let r = run(StatsPlan {
        subscribe_at: Some(Duration::from_secs(1)),
        resubscribe_at: Some(Duration::from_secs(5)),
        ..Default::default()
    })
    .await;
    let st = &r.client_stats;
    assert_eq!(st.subscribes_sent, 2);
    assert_eq!(st.schemas.len(), 2, "every on is answered with a schema");
    assert_ne!(st.schemas[0].1.schema_id, st.schemas[1].1.schema_id);
    assert_eq!(st.rejected, 0, "no sample straddled the change unmatched");
    let after = st
        .samples
        .iter()
        .filter(|(t, _)| *t > st.schemas[1].0)
        .all(|(_, s)| s.schema_id == st.schemas[1].1.schema_id);
    assert!(after, "samples after the second schema carry its id");
}
```

(`#[tokio::test(flavor = "current_thread", start_paused = true)]` matches how the other browserless targets run under the paused clock: check the attribute in `ghostframe-e2e/tests/browserless_bwe_rate_source.rs` and copy it exactly if it differs.)

- [ ] **Step 2: Run and see what fails first.** Before Task 14's `post_turn` hook and Task 15's tap are wired the first test fails with `schemas.len() == 0`. They are wired now, so run:

```bash
cargo test -p ghostframe-e2e --test browserless_stats -- --test-threads=1 --nocapture 2>&1 | tail -30
```

Expected: 4 tests pass. Capture the failing test name before re-running anything (the browserless suite can bail with `.expect("scene ran")`, a harness stall; see AGENTS.md "Known flakes"). Run it 3 times; all must pass.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-e2e/tests/browserless_stats.rs
git commit -m "$(cat <<'EOF'
test(e2e): browserless scenes for the stats stream, with no UI

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 4: Mutation checks (each separately, restore between).**
(a) Remove the `self.service_stats(now_std());` line from `post_turn` in `io_bridge.rs`: `a_subscribed_session_gets_a_schema_then_live_samples` fails (`schemas.len()` 0). This is the check Task 14 deferred.
(b) In `stats_inputs` change `rtt_ms: rtt.map_or(0.0, ...)` to `rtt_ms: 0.0`: the same test fails on the RTT band.
(c) In `service_stats` stop honouring the off: change `apply_stats_subscribe` to ignore `on == false` (`st.subscribed = true;`): `an_unsubscribe_stops_the_stream` fails.
(d) In `apply_stats_subscribe` set `st.schema_pending = on && st.schema_id.is_none();` (no re-send on a repeated on): `a_repeated_subscribe_resends_the_schema_with_a_new_id` fails.
(e) In `nothing_arrives_without_a_subscribe`'s premise: temporarily make `service_stats` ignore `subscribed` (send to every entry) and have the harness subscribe nothing: this test cannot see it (no entry exists), which is correct, and the unit test `nothing_is_sent_before_a_subscribe` is the one that guards the empty-table path. Instead mutate `StatsTap::step` to subscribe at 0 regardless of the plan: this test fails on `subscribes_sent == 0`, proving the control is wired to the plan.
Restore with `git checkout -- ghostframe-lib/src ghostframe-e2e/src`; rerun all four green.

---

### Task 17: Run the new target in CI by name

**Model:** haiku

CI runs integration targets by explicit name: a new `tests/*.rs` is invisible until a workflow lists it (and a similarly named job is not evidence).

**Files:** Modify `.github/workflows/e2e.yml`

- [ ] **Step 1: Add the line** to the **`browserless`** job (starts at line 31), not to `browserless-cc` (line 153, the roughly 25-minute congestion-control job, which holds the `browserless_model_controller` step at line 182). Anchor it after the last step of `browserless`, `cargo test --test c2_frame_off_loop -- --test-threads=1` (line 147; the `browserless_h264` step before it is line 146):

```yaml
      # Stats panel PR 1: the server half of the stats stream, end to end with
      # no UI (subscribe, schema, 1 Hz samples with live RTT and estimate,
      # unsubscribe, repeated subscribe).
      - run: cargo test --test browserless_stats -- --test-threads=1
```

- [ ] **Step 2: Verify it is by name, and that it landed inside `browserless:`**

```bash
grep -n "browserless_stats" .github/workflows/e2e.yml
grep -n "^  browserless:\|^  browserless-cc:\|^  harness-smoke:" .github/workflows/e2e.yml
sed -n 54,62p .github/workflows/e2e.yml
```

Expected: one `browserless_stats` match whose line number is greater than the `browserless:` line and less than the `browserless-cc:` line (the next job header), and not in `browserless-cc` or `harness-smoke`; the surrounding steps use the same `cargo test --test <name>` form (the `ghostframe-e2e` crate is the default for `--test` because the other browserless targets there are run the same way).

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/e2e.yml
git commit -m "$(cat <<'EOF'
ci(e2e): run the browserless_stats target by name

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 18: Regenerate the cbindgen header

**Model:** haiku

`ghostframe-lib/cbindgen.toml` parses `ghostframe-protocol` (`parse_deps = true`, `include = ["ghostframe-protocol"]`), so every `pub const` in the protocol crate lands in the checked-in public C header. The shared contract fixes these as `pub const`; they are wire-format constants like the existing `RESET_REQUEST_MSG_TYPE`. `just ci-local` diffs this file, so it must be regenerated and committed.

**Files:** Modify `ghostframe-lib/include/ghostframe.h`

- [ ] **Step 1: Regenerate and inspect the diff**

```bash
cargo build -p ghostframe-lib
git diff --stat ghostframe-lib/include/ghostframe.h
git diff ghostframe-lib/include/ghostframe.h
```

Expected additions only: `STATS_SUBSCRIBE_MSG_TYPE 11`, `STATS_SUBSCRIBE_SIZE 2`, `SERVER_MSG_STATS_SCHEMA 1`, `SERVER_MSG_STATS_SAMPLE 2`, `SERVER_FRAME_MAX_LEN` (16384, or the expression), each with its doc comment. The `pub(crate)` `MAX_DESCRIPTORS` and the private `FRAME_HEADER_LEN` and `MAX_SHORT_STR` must not appear. If a type (`MetricKind`, `StatsWireError`, ...) appears, cbindgen exported it: that is a defect, because a Rust enum with payloads or a `String` field has no C layout; investigate before committing (the types are not `#[repr]` and are unreferenced by any `extern "C"` function, so they should not appear).

- [ ] **Step 2: Check the client C header is unchanged** (its cbindgen does not parse dependencies)

```bash
cargo check -p ghostframe-client-capi
git diff --exit-code ghostframe-client-capi/include/ghostframe_client.h && echo unchanged
```

Expected: `unchanged`.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/include/ghostframe.h
git commit -m "$(cat <<'EOF'
chore(lib): regenerate ghostframe.h for the stats wire constants

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 19: Gates

**Model:** sonnet

**Files:** none (fixes only if a gate fails)

- [ ] **Step 1: Format**

```bash
cargo fmt --all
git status --short
```

If `cargo fmt` changed files, commit exactly those paths: `git add <the changed paths>` then `git commit -m "style: cargo fmt" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"`.

- [ ] **Step 2: Clippy on every target (the real gate; `cargo test -p` hides non-compiling targets)**

```bash
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -30
cargo clippy -p ghostframe-lib --all-targets --features cdf53-diag -- -D warnings 2>&1 | tail -10
```

Expected: clean. On an aarch64 machine remember the `unnecessary_cast` trap in AGENTS.md before blaming a toolchain.

- [ ] **Step 3: Targeted tests**

```bash
cargo test -p ghostframe-protocol 2>&1 | tail -5
cargo test -p ghostframe-lib --lib stats_ 2>&1 | tail -8
cargo test -p ghostframe-lib --lib webtransport 2>&1 | tail -8
cargo test -p ghostframe-lib --lib classifier 2>&1 | tail -8
cargo test -p ghostframe-client-net 2>&1 | tail -8
cargo test -p ghostframe-e2e --test browserless_stats -- --test-threads=1 2>&1 | tail -8
cargo test -p ghostframe-e2e --test loopback_h3 2>&1 | tail -5
cargo build -p ghostframe-protocol -p ghostframe-client-core -p ghostframe-client-net -p ghostframe-client-wasm --target wasm32-unknown-unknown 2>&1 | tail -3
```

Expected: all green. The wasm build is `client-core.yml`'s check; a `std::time` or thread use in `stats.rs` would break it. `ghostframe-client-wasm` does not use the stats module yet, but it links the protocol crate.

- [ ] **Step 4: The full local gate, under the GPU lock**

```bash
~/work/gf-gpu-run just ci-local 2>&1 | tail -40
```

Never run `just ci-local` bare: it touches VA-API and a live VCE contends with `gpu_test_lock`. It includes fmt, the env-read guard, the release-feature check, clippy, unit tests, web build, tsc, release build, the cbindgen header diff (`git diff --exit-code ghostframe-lib/include/ghostframe.h`) and go vet. Expected: `=== ci-local passed ===`. Capture a failing test's name before re-running it (flakes are listed in AGENTS.md; `encoder::h264::vaapi_direct` contends under load: re-run with `--test-threads=1`).

- [ ] **Step 5: The client-only gate** (client crates changed: `ghostframe-client-net`, `ghostframe-protocol`)

```bash
~/work/gf-gpu-run just ci-client 2>&1 | tail -30
```

`ci-client` runs `test-client`, which includes `ghostframe-client-gpu` and its GPU tests; parallel GPU load has reset amdgpu here, so it goes behind the lock like `ci-local`, never bare.

Expected: `=== ci-client passed ===`.

- [ ] **Step 6: Confirm the header is committed and the tree is clean**

```bash
git diff --exit-code ghostframe-lib/include/ghostframe.h && echo header-committed
git status --short
```

Expected: `header-committed`; no tracked changes (an untracked `proptest-regressions` file must have been added in Task 7).

- [ ] **Step 7: A live smoke check** (optional, owner-run: it needs the installed daemon). With a deployed build (`just build-release`, then `./target/release/ghostframe-server install --force` as the session's user), open the web client with the devtools console and run, in a page that exposes the stream (PR 2 adds the UI; until then use the browserless target as the proof). Do not install anything from this task.

No commit unless Step 1 formatted something.

---

### Task 20: Review

**Model:** opus

**Files:** none

- [ ] **Step 1: Request the review.** Use superpowers:requesting-code-review on the whole PR diff, `git diff master...feat/stats-protocol-server`, against `docs/specs/stats-panel-design.md` and this plan. Brief the reviewer with:
  - the four consumers of the wire format (`ghostframe-protocol`, `ghostframe-lib`, `ghostframe-client-core`, `ghostframe-client-wasm`): this PR touches the first two and adds a read hook to `ghostframe-client-net`; the other two are PR 2;
  - the spec checklist below;
  - the Self-review section's open questions.

- [ ] **Step 2: The reviewer checks, at minimum:**
  1. Every spec 1.1 message has its limits enforced on decode (counts, string lengths, frame length) and no decode path can panic (the proptest `decoders_never_panic_on_arbitrary_bytes` is real, and mutating a length check turns a test red).
  2. `STATS_SUBSCRIBE_MSG_TYPE` is in `FEEDBACK_STREAM_MSG_TYPES`; no catch-all arm routes the new type; the `0x0B` arm sits behind the live-session HELLO gate.
  3. Subscription state is cleared in `forget_session`, and the aliasing test (`a_forgotten_handle_that_quinn_reuses_gets_no_stats`) fails if the removal is deleted.
  4. The sampler adds no per-datagram or per-tile work; `service_stats` returns on `stats_sessions.is_empty()`; `next_wake_deadline` cannot spin (every due entry gets a future `next_sample_at`).
  5. A frame is never torn on the stream: the real-quinn test is load-bearing (mutate the tail handling and it fails), and its "window fills" premise is asserted.
  6. The version string: worktree-safe rerun triggers; the dirty flag's known lag is documented; `unknown` outside git.
  7. Every test in the plan was shown to fail when the code it guards is broken, and every measurement is shown non-zero (bwe, RTT, bytes in `browserless_stats`).
  8. `ghostframe.h` changes are only the documented constants; no type leaked into the C API.
  9. CI lists `browserless_stats` by name.

- [ ] **Step 3: Apply findings** with superpowers:receiving-code-review (verify each before changing code), re-run Task 19's gates, and only then hand the branch to the owner. Do not push or open the PR without the owner.

---

## Self-review

### Spec coverage

| Spec section | Covered by |
|---|---|
| 1.1 `STATS_SUBSCRIBE` (0x0B, `[type, on]`) | Task 2 (constant, registry), Task 3 (`StatsSubscribe`) |
| 1.1 server-to-client channel, framing `[type][len u16 LE][payload]`, 16 KiB, reassembly, unknown type skipped by length, oversize drops reader state | Task 3 (consts), Task 6 (`ServerFrameReader`), Task 12 (real-quinn wire path) |
| 1.1 `STATS_SCHEMA` contents, sent in answer to every `on` | Task 4 (encode/decode), Task 14 (`schema_pending`, tests 2 and 5), Task 16 (repeated subscribe) |
| 1.1 `STATS_SAMPLE`, once a second, schema order, counters as totals | Task 5, Task 14 (1 Hz test), Task 16 |
| 1.1 bounded lengths, checked on decode; single discriminator table, no catch-all | Tasks 3 to 7; Task 2; Task 13 (arm, no wildcard) |
| 1.2 descriptor table, `pub(crate)`, not in the header | Task 10, Task 18 (header diff) |
| 1.2 per-session sampler in `io_bridge`, 1 Hz, only while subscribed, no per-datagram work | Task 14 |
| 1.2 server metrics table (RTT, uptime, estimate, rate target, sent, mode, reason, backend, resends, verdicts, NACKs) | Task 10 (`Stat`), Task 14 (`stats_inputs`, with Task 9 for the reason) |
| 1.2 subscription state per session, cleared in `forget_session` | Task 13 (state, `forget_session`, aliasing test), Task 14 (reuse test) |
| 1.2 malformed `STATS_SUBSCRIBE` logged once per session and ignored | Task 13 |
| 1.4 version capture (crate version, commit, dirty, `unknown`, rerun triggers, no timestamp) | Task 8 (in `ghostframe-lib`, justified there) |
| 3 sample/schema mismatch, decode errors return errors and never panic | Tasks 4, 5, 7 |
| 3 server side: failed write never breaks the stream | Task 11, Task 12 (counted, whole frames only) |
| 4 protocol: round trips, proptest, truncation, count mismatch, unknown kind, collision tests | Tasks 2 to 7 |
| 4 server sampler: nothing before subscribe, stops after unsubscribe, repeated subscribe new `schema_id`, gone after `forget_session`, non-zero estimate and RTT on a running session | Task 14 (unit), Task 16 (running session) |
| 4 every test checked by breaking the code | the mutation step of every test task |
| 4 CI: every new integration test target added by name | Task 17 |
| 6 item 1: three messages, descriptor table, sampler, subscription state, version capture, e2e hook, no UI | Tasks 2 to 16 |

Not in this PR by design: client `ClientStats`, `ghostframe-web-ui`, `mountStatsPanel`, `main.ts`, the client version capture (PR 2); the Chromium panel e2e and the bundle-size guard (PR 3).

### Spec gaps found

1. **quinn-proto cannot say how much a stream write will take**, so "a sample that cannot be written whole is dropped" is realised as: a frame is refused before its first byte when the stream owes a previous tail, and a started frame is always finished (Task 11). The effect (no torn frame; whole frames dropped and counted) matches; the mechanism is not "check, then write".
2. **The dirty flag lags** for source edits outside `ghostframe-lib` (a build script reruns on its own package's files, the git state and `../ghostframe-protocol/src`, not on every crate). The spec says it reruns on `.git/HEAD` or `.git/index`; that is not enough in a worktree (`.git` is a file) or after a commit (the branch ref moves, HEAD does not), so the rerun list is derived with `git rev-parse --git-path`. Untracked files are ignored on purpose: this repository keeps untracked notes in its root, and a flag that is always on says nothing. The lag is accepted and made harmless by the match rule: the client compares crate version and commit only, and `dirty` never fails it (Task 8 preamble). Every git call in `build.rs` runs with `--no-optional-locks` so the script never rewrites the index it watches.
3. **`ghostframe.h` changes.** `ghostframe-lib/cbindgen.toml` parses the protocol crate, so the contract's `pub const`s become public C `#define`s (Task 18).
4. **Nothing reads the return half on the native client.** `ClientNet` never did; Task 12 adds an explicit read for the harness. The native client needs its own call site before it can use the stream (out of scope; the spec says exposing stats through the C API is).
5. **A `TooLong` frame length cannot be resynchronised**: the length is the only marker, so after the reader clears its buffer, later bytes are read as fresh frames. A server that honours the limit never sends one; a corrupt stream stays corrupt. Spec 1.1 says "closes nothing", which this follows.
6. **`off` is not answered with a schema**, a reading of spec 1.1 ("in answer to every `on`").
7. **The first sample is sent with the schema** (not one second after), so the panel fills at once; the spec only says "once a second while subscribed".
8. **Sample wire carries a value count**, which spec 1.1 does not list but spec 3 needs (a count mismatch cannot be told from truncation otherwise).

### Open questions for the owner

1. Accept the new `#define`s in `ghostframe.h` (Task 18), or move the stats consts behind a path cbindgen does not parse?
2. Is "tracked changes only" the right meaning of the `dirty` flag? (Settled for the match: it never fails it.)
3. Should schema ids restart per session (spec: "incremented on each schema the session sends") or stay process-wide monotonic as implemented here, which keeps a reconnecting client from seeing a repeated id?

### Contract deviations

All additions or clarifications; no name in the shared contract is changed.

- `StatsWireError` has one extra variant, `BadMessage` (a subscribe with the wrong type byte or an `on` that is not 0 or 1). The six contract variants exist as unit variants with the contract names.
- `StatsSchema::try_encode_frame` and `StatsSample::try_encode_frame` are added; the contract's `encode_frame` exists and panics only on a schema or sample over the wire limits. The server uses `try_encode_frame` and counts failures.
- `StatsSample` on the wire carries a `count u16` before the values (needed for the contract's "count mismatch is an error").
- `MetricKind::{from_u8, as_u8}` and `MetricGroup::{from_u8, as_u8}`, `ServerFrameReader::buffered()` and `Default`, and `PartialEq`/`Clone`/`Debug` derives on every stats type are added.
- Schema ids are process-wide monotonic on the bridge rather than restarting per session (still "incremented on each schema").
- `STATS_SUBSCRIBE_SIZE` is a `pub const` (the contract names it). `MAX_DESCRIPTORS` is `pub(crate)`: PR 2 does not need it (the decoder enforces it), and a `pub const` would become another `#define` in the cbindgen header. PR 2 that needs the limit must say so and make it `pub` then.
- **Version match semantics (binding on PR 2).** `StatsView::versions_match` compares the crate version and the commit only. `dirty` is displayed but never fails the match, because the server's flag is computed when `ghostframe-lib`'s build script reruns and the client's when the wasm crate rebuilds, so on a dirty tree they can disagree about `dirty` on current code. An `unknown` commit on either side is not a mismatch. See the Task 8 preamble.
- The server's reported crate version is `ghostframe-lib`'s `CARGO_PKG_VERSION`; there is no `[workspace.package]`, so it can drift from `ghostframe-server`'s.
- New public items outside the contract's list: `ghostframe_client_net::ClientNet::read_feedback_return(&mut self, now_us: u64) -> Vec<u8>`; `ghostframe_lib::transport::webtransport::{StatsWrite, WebTransportServer::note_stats_subscribe, write_stats_frame, stats_tail_len}`. PR 2 does not need them (the wasm path reads the stream in TypeScript) but must not rename them.
- The version `build.rs` is in `ghostframe-lib`, not `ghostframe-server` as spec 1.4 says (justified in Task 8: the schema is built in the library, which the harness and benches also link). PR 2's client-side `build.rs` in `ghostframe-client-wasm` is unaffected.

## Review notes

Review of this plan (13 issues) was applied in full. Points where the fix differs from the review's wording:

- **Task 17 anchor.** The review named `browserless_h264` (line 146) as the last step of `browserless`. The file's last step of that job is `c2_frame_off_loop` (line 147), so the plan anchors after that, with the `grep -n` check that the new line falls between `browserless:` and `browserless-cc:`. Same intent, exact line.
- **Task 14, optional cap on re-sent schemas.** Not applied. A repeated `on` is how a reopened panel resynchronises (spec 1.1, answered by a schema every time), and the Task 16 scene and Task 14 test pin that. The cost is bounded: the sender is the one authenticated session, the write is refused when the stream is blocked, and the frame is a single schema of a few hundred bytes. The review marked it optional. The required half (return before building the handle list) is applied, written as an `any_due` pass with no allocation (not `stats_next_due()`, whose "now" fallback is read after the caller's `now` and would wrongly report nothing due).
- **Task 14, drop visibility.** Applied as one INFO line per session per subscription on the first dropped frame (`drop_logged`, reset by each `on`), not as new fields on the `cumulative emit` line: that line belongs to the emitter's own cadence in another module, and a log line per first drop is enough to make a failing stream visible at `ghostframe=info`.
- **Task 8, version match.** Settled the cheaper way the review offered (match on crate version and commit, `dirty` shown but never failing), not by widening the rerun list, which would recompile `ghostframe-lib` on every client edit. Recorded under Contract deviations as binding on PR 2.
- **Task 8, mutation (c)** is observational (the index mtime before and after a build), since a regression there costs a spurious rebuild, not a wrong answer.
