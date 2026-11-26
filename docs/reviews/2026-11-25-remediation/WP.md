# WP — wire protocol, client-core robustness, fuzzing

Re-verified at `e85fb005` (worktree `ghostframe-review-2026-11-25`). Every
file:line below was read at that commit. Code blocks are **sketches**: the
signatures they reuse were checked against the tree, and the new names are
proposals.

Cross-cutting rules for every WP PR (AGENTS.md):

- A wire change touches all four consumers together: `ghostframe-protocol`,
  `ghostframe-lib`, `ghostframe-client-core`, `ghostframe-client-wasm`. The
  wasm one is the easy one to miss.
- Gate with `cargo clippy --workspace --all-targets -- -D warnings`, not with
  a per-crate test summary, because a test target that fails to compile drops
  out of the summary silently. Run `gf-gpu-run just ci-local` before pushing.
- A daemon that will be installed or put in an e2e image is built with
  `just build-release`, which runs build-web first, because the web bundle is
  embedded. Run `just containers-build` before any e2e run that follows a
  lib/server change.
- Owner rule: there are no production deployments and no wire compatibility
  to keep ("GPU-first, no compat needed"), so a wire change is a lockstep
  change, not a negotiated one.
- New `pub const` items in `ghostframe-lib` land in `ghostframe.h`. Moving
  constants *out* of the lib (WP-6) changes the header, so regenerate it and
  commit the result.

---

## WP-0 (was: 0x04 tile-parity envelope collision): STALE

**Status at e85fb005: STALE.** Commit `5cd35e7f` ("refactor: delete the tile
XOR FEC (superseded by FEC v2 rev 6)") removed the parity envelope, and
`e8441d62` made FEC v2 rev 6 framing unconditional. Client→server 0x04 now
only classifies as `InboundKind::AckBatchV1` (a superseded ACK revision,
counted and dropped): `ghostframe-protocol/src/protocol.rs:880`
(`0x02..=0x04 | 0x06 => InboundKind::AckBatchV1`), routed to
`dispatch_superseded_ack_datagram` at
`ghostframe-lib/src/transport/io_bridge/ack_dispatch.rs:784`. On the
feedback *stream*, 0x04 is `DECODE_ERROR_MSG_TYPE`. It lives on a different
channel, which `decode_error_batcher.rs:24-33` already documents.

**Does the new FEC framing create an analogous first-byte ambiguity? No.** I
checked it:

- Every server→client datagram goes through `fec::frame::parse`
  (`ghostframe-client-core/src/fec_rx/blocks.rs:317`). Byte 0 bits 7-6 are the
  frame kind (`FEC_KIND_MASK = 0xc0`, `fec/frame.rs:10-16`). Kind 00 is
  rejected (`frame.rs:118-119`), and SOURCE and REPAIR also check the scheme
  bits and the block shape.
- Tile and frame datagrams are no longer datagrams on the wire. They are
  **records** inside a SOURCE or PLAIN frame, each behind a `u16` length
  prefix (`push_record`, `frame.rs` end). A tile record's first byte
  (`0x80..=0xFF`, from `frame_seq | TILE_DATAGRAM_FLAG`) is never at offset 0
  of a UDP payload, so it cannot be confused with `FEC_FRAME_REPAIR = 0x80`.
- Inside a record the only discriminator left is bit 31 of `frame_seq`
  (`is_tile_datagram`, `protocol.rs:409-415`, used at
  `reassembly.rs:206` and `fec_rx/blocks.rs:474`). That is WP-1.
- Residual oddity, harmless and noted for WP-7: the server answers a 4-byte
  `"ping"` datagram with `"pong"` framed as a lone PLAIN bundle
  (`io_bridge.rs:3001-3012`, `ack_dispatch.rs:26-32`). It does this before the
  HELLO gate, because `recv_datagram` runs ahead of
  `dispatch_inbound_datagram`. No client sends `"ping"`, and the `"pong"`
  record is under 20 bytes, so `reassembly.rs:197` ignores it. Server inbound
  classifies `'p' = 0x70` as `InboundKind::FrameFragment`. The path is dead,
  and WP-7 deletes it.

No work item.

---

## WP-1 — `frame_seq` is not a closed 31-bit space; ordering is non-wrapping in places (Low-Med)

### Status at e85fb005: CONFIRMED

- Server counter: `IoBridge::frame_seq: u32` (`ghostframe-lib/src/transport/io_bridge.rs:577`),
  initialised to 0 once at construction (`io_bridge.rs:1518`) and advanced
  with `self.frame_seq.wrapping_add(1)` at
  `io_bridge/frame.rs:349` (test-only CPU path) and `frame.rs:777`
  (`begin_gpu_frame`, production). Nothing resets it per session and nothing
  masks bit 31. The counter is process-wide.
- `fragment_frame_ref` (`ghostframe-protocol/src/protocol.rs:572-613`) writes
  `frame_seq` verbatim into `FrameHeader` (`:594`). Its header comment
  (`:420`) promises "bit 31 always 0", and no code enforces that.
- Receiver discrimination: `is_tile_datagram` (`protocol.rs:409`) reads bit
  31. A frame record with `frame_seq >= 2^31` is routed to the tile path
  (`reassembly.rs:206`), where it is decoded as a garbage tile and rejected.
  From that point every H.264 frame is lost until the counter wraps 2^31
  frames later.
- Non-wrapping comparisons on the client:
  `ghostframe-client-core/src/frame_assembly.rs:80` (`fh.frame_seq <= last`),
  `:86` (`fh.frame_seq < latest.saturating_sub(2)`), `:91` (`>`), `:143-148`
  (`newest_idr_assembly` uses `.max()`), `:157-165` (`retain(seq >= threshold)`),
  `:181` (`fh.frame_seq > newest`). The anchor field
  `latest_full_frame_seq: u32` starts at `0` (`client-core/src/lib.rs:228,288`),
  which only works because the comparisons do not wrap.
- Wrapping comparisons already exist, written four times over:
  `h264_gate::seq_after` (`h264_gate.rs:81-85`), `FrameAckWindow::record`'s
  `SEQ_MASK` (`h264_gate.rs:394-430`), `cdf53_coverage::frame_seq_is_newer` and
  `forward` (`cdf53_coverage.rs:109-116, 152-154`), and `FrameAck::bit`
  (`protocol.rs:807-810`).
- Non-wrapping comparisons on the server: `FrameTracker::unresolved`
  (`io_bridge/h264_tx.rs:318`, `frame_seq >= floor`);
  `H264Cache::drop_before` / `drop_from` (`reliable_emitter/h264.rs:122-160`,
  `BTreeMap::split_off(&frame_seq)`); `supersede_h264_before`
  (`reliable_emitter/emitter.rs:738-740`, `|f| f < frame_seq`).
  `TRACKER_RING = 256` (`encoder/h264/tracker.rs:11`) divides 2^31, so the
  ring slot stays continuous across a 31-bit wrap.

Reachability: one tick per captured frame. At a steady 60 fps the counter
reaches bit 31 after about 414 days of daemon uptime (2^31 / 60 / 86400).
Severity stays Low-Med: the failure is silent, total loss of H.264 until a
daemon restart, but it is far away in time.

### Root cause

`frame_seq` is defined as 31 bits only in comments. Its producer is a raw
`u32`, and every consumer picked its own ordering: some 31-bit serial, some
plain `u32`.

### Design

**Chosen:** a closed 31-bit serial space with one helper module in
`ghostframe-protocol`, used by every producer and every comparison.

```rust
// sketch — ghostframe-protocol/src/seq.rs (new), re-exported as protocol::seq
pub const FRAME_SEQ_MASK: u32 = 0x7FFF_FFFF;
/// Next sequence number in the 31-bit space (wraps 0x7FFF_FFFF -> 0).
pub const fn next(seq: u32) -> u32 { seq.wrapping_add(1) & FRAME_SEQ_MASK }
/// Forward distance b -> a, modulo 2^31.
pub const fn forward(a: u32, b: u32) -> u32 { (a & FRAME_SEQ_MASK).wrapping_sub(b & FRAME_SEQ_MASK) & FRAME_SEQ_MASK }
/// RFC 1982: `a` strictly newer than `b` (half-range 2^30).
pub const fn is_newer(a: u32, b: u32) -> bool { let f = forward(a, b); f != 0 && f < (1 << 30) }
/// Serial ordering for max/min/sort (Equal only when equal mod 2^31).
pub fn cmp(a: u32, b: u32) -> core::cmp::Ordering { ... }
```

- Server: `self.frame_seq = seq::next(self.frame_seq)` at both sites.
  `fragment_frame_ref` adds `debug_assert_eq!(frame_seq & TILE_DATAGRAM_FLAG, 0)`
  and writes `frame_seq & FRAME_SEQ_MASK`. In release, the header can then
  never classify as a tile, whatever the caller does.
- Client: every comparison in `frame_assembly.rs` goes through `seq::*`, and
  `latest_full_frame_seq` becomes `Option<u32>`. This is required, not
  cosmetic. The process-wide counter means a client that connects to a
  long-running server sees a first `frame_seq` anywhere in the space.
  Measured serially against an initial 0, a value above 2^30 reads as *older*,
  and the first 2^30 frames would all be dropped. `h264_gate::seq_after`,
  `cdf53_coverage::frame_seq_is_newer` / `forward`, `FrameAckWindow`'s
  `SEQ_MASK` and `FrameAck::bit` become thin calls into `seq`, or are deleted
  and their call sites redirected.
- Server ordering sites listed above become serial. `drop_before` / `drop_from`
  switch from `split_off` to `retain(|&s, _| !seq::is_newer(frame_seq, s))`
  and its mirror. `trim_to` bounds the map, so O(n) is fine. `unresolved`
  becomes `!seq::is_newer(floor, f.frame_seq)`. `purge_queued_h264` takes
  `|f| seq::is_newer(frame_seq, f)`.
- Tile labels keep their path: `scheduler.frame_label(.., seq) | TILE_DATAGRAM_FLAG`
  (`io_bridge/emit.rs:769`) and the client masks with `& !TILE_DATAGRAM_FLAG`
  (`reassembly.rs:253`). Once the producer is 31-bit, the label is too.

**Rejected:**
1. *Reset `frame_seq` per session and declare a wrap unreachable.* That moves
   the cliff without removing it. Tile labels and the scheduler's slot labels
   are derived from it across session boundaries, and the change lands in
   `io_bridge.rs` lifecycle code that other groups own.
2. *Make `frame_seq` a `u64` on the wire.* Every header grows by 4 bytes, and
   the tile/frame discriminator still needs a bit.
3. *A `FrameSeq` newtype across the whole tree.* It is the strongest option,
   but it touches hundreds of signatures, many in `io_bridge`, while other
   groups are working there. Revisit once the helper module is in.

### Files touched

`ghostframe-protocol/src/{lib.rs, seq.rs (new), protocol.rs}`;
`ghostframe-client-core/src/{lib.rs, frame_assembly.rs, h264_gate.rs, cdf53_coverage.rs}`;
`ghostframe-lib/src/transport/io_bridge/{frame.rs, h264_tx.rs}`;
`ghostframe-lib/src/transport/reliable_emitter/{h264.rs, emitter.rs}`.
The wasm client calls `ClientCore` and has no ordering of its own. Check
with `rg 'frame_seq\s*[<>]' ghostframe-client-wasm/src`, which is empty
today.

### Implementation steps

1. Add `ghostframe-protocol/src/seq.rs` with the sketch above and
   `pub mod seq;` in `lib.rs`. Unit tests go in the same file (see Tests).
2. In `protocol.rs::fragment_frame_ref`, add the debug_assert and the mask.
   Update the `FrameHeader` comment at `:420` to point at `seq::FRAME_SEQ_MASK`.
3. Replace `FrameAck::bit`'s inline mask with `seq::forward(self.newest_seq, seq)`.
4. In `client-core`, change `latest_full_frame_seq` to `Option<u32>` with
   initial `None`. Check `on_session_reset` or the equivalent: if it clears
   the field today, set it to `None`.
5. Rewrite `frame_assembly.rs`:
   - `:80` becomes `!seq::is_newer(fh.frame_seq, last)`.
   - `:86` becomes `latest.is_some_and(|l| seq::is_newer(l, fh.frame_seq) && seq::forward(l, fh.frame_seq) > 2)`.
   - `:91` becomes `latest.is_none_or(|l| seq::is_newer(fh.frame_seq, l))`.
   - `newest_idr_assembly` becomes `.max_by(|a, b| seq::cmp(*a, *b))`.
   - `evict_stale_frame_assemblies` keeps `seq` when `!seq::is_newer(l, s) || seq::forward(l, s) <= 2`.
   - `:181` becomes `seq::is_newer(fh.frame_seq, newest)`.
6. Make `h264_gate::seq_after` a `#[inline] pub use`-style wrapper over
   `seq::is_newer`, or delete it and fix its 7 call sites (`rg seq_after`). Do
   the same for `cdf53_coverage::{frame_seq_is_newer, forward}` and
   `FRAME_SEQ_MASK`. Keep `LABEL_WINDOW` / `LABEL_EXPIRY` as they are: they
   already assume this arithmetic.
7. Server: change `frame.rs:349` and `:777` to `seq::next`. Make the three
   server ordering sites serial (above). Then run
   `rg -n 'frame_seq\s*[<>]=?|split_off\(&frame_seq|\.max\(\)' ghostframe-lib/src/transport ghostframe-lib/src/encoder/h264`
   and convert, or comment as "same frame, equality only", every remaining hit.
8. Add a `#[cfg(test)] pub(super) fn set_frame_seq_for_test(&mut self, v: u32)`
   on `IoBridge`, for the wrap test.

### Tests

- `ghostframe-protocol/src/seq.rs` unit tests:
  - Proptest `is_newer(a, b)` against an `i64` reference model over random
    `(base, delta)` with `|delta| < 2^30`, with both operands masked.
  - Antisymmetry: `is_newer(a, b) && is_newer(b, a)` is never true.
  - `next(0x7FFF_FFFF) == 0`, and `next(x) & TILE_DATAGRAM_FLAG == 0` for every
    `x` (proptest).
- `protocol.rs` test `fragment_frame_ref_never_sets_tile_flag`: for
  `frame_seq` values in `{0x7FFF_FFFF, 0x8000_0000, u32::MAX}`, call it with
  `debug_assertions` off through a helper that bypasses the assert, or test
  the masked value, and assert `!is_tile_datagram(dg)` on every output.
- `client-core/tests/frame_assembly_wrap.rs` (new; CI's client-core job runs
  every target of the crate): feed single-fragment P frames with seq
  `0x7FFF_FFFD, 0x7FFF_FFFE, 0x7FFF_FFFF, 0, 1, 2` through `ClientCore::handle_datagram`,
  wrapped in PLAIN frames the way `fec_framing.rs` helpers already do. Assert
  six `Event::NeedsH264` in order. Second case: the first frame of a session
  is `0x5000_0000` (above 2^30). Assert it and its successor are delivered.
  This is the `Option` anchor case.
- `ghostframe-lib` unit test in `io_bridge/h264_tx.rs` or `reliable_emitter/h264.rs`:
  insert frames `0x7FFF_FFFE..=1` (wrapping), call `drop_before(0)`, and assert
  only frames before 0 serially (`0x7FFF_FFFE`, `0x7FFF_FFFF`) were dropped.
- Property to assert in the client test: delivered count == fed count, which
  is monotone and terminal. Not a magic number.

**Mutation checks:**
- Revert step 5 at `:86` to `fh.frame_seq < latest.saturating_sub(2)`. The
  wrap test fails, because frame `0` is dropped as stale.
- Revert step 4 by keeping a `0` anchor with serial compare. The
  `0x5000_0000` case fails.
- Revert `seq::next` to a bare `wrapping_add`. The `next` proptest fails.
- Revert `drop_before` to `split_off`. The server unit test fails.

### Gates

Clippy workspace, `just test-unit`, `cargo test -p ghostframe-protocol -p ghostframe-client-core`,
and the browserless H.264 suite (`cargo test --test browserless_h264`) as a
regression check. No e2e is needed: the wire bytes do not change for
`frame_seq < 2^31`. `just build-release` if the daemon is deployed for a live
check.

### Risks / owner decisions

- Serial comparison inverts at 2^30 frames apart. That is the same assumption
  the H.264 gate already makes, and `LABEL_EXPIRY` documents it for tiles.
- `io_bridge/frame.rs` is contended (see the PR table). Its two edits are
  one line each.

**Size: M.**

---

## WP-2 — CPU PalRle path never checks `index < count` (Low)

### Status at e85fb005: CONFIRMED, wider than reported

- `prevalidate_pal_rle` (`ghostframe-client-core/src/pal_rle_decode.rs:43-115`)
  validates the length, the count range and the cached palette, then expands
  RLE (`expand_rle_to_indices`, `:121-145`). Nothing compares an index with
  `count`, for RLE or for indices-raw (`:55-71`).
- RLE overshoot and undershoot both come back as `DecodeErrorCode::PayloadTooShort`
  (`:99-100`, `ok_or(PayloadTooShort)`), so the CPU path never reports
  `RleOvershoot` (6) or `RleUndershoot` (7) either.
- This is the **production native-client path**, not only a test helper. The
  native decode thread decodes PalRle on the CPU with `TileDecoder`
  (`ghostframe-client-native/src/decode_thread.rs:362`). The decoder paints
  through `self.palettes[palette_id]` (`tile_decoder.rs:340-346, 577-590`),
  and its palette slots past `count` "keep what they held"
  (`tile_decoder.rs:426-432`). An out-of-range index therefore paints a stale
  colour and is never reported. `decode_pal_rle_tile` (`:150-188`, used by the
  e2e framebuffer oracle) does the same.
- The browser path is covered: the WGSL shader rejects `index >= count` with
  code 5 (`ghostframe-render/src/tile_renderer.rs:620-624`, test
  `ghostframe-client-gpu/tests/gpu_tileready.rs:240-275`).
- The protocol crate's `decode_pal_rle` rejects with `IndexOutOfRange`
  (`ghostframe-protocol/src/codec/pal_rle.rs:465-470`). It does not handle the
  indices-raw flag (bit 1), however.

### Root cause

The validation contract was split between the CPU prevalidation and the GPU
shader, and the CPU half was ported without the shader's check.

### Design

**Chosen:** make `prevalidate_pal_rle` the single validator every client path
goes through. It already is: `reassembly.rs:380`. It now rejects:

- any expanded index `>= count`, as `IndexOob`, for Bundled, Thin and IndicesRaw;
- an RLE run past 1024 pixels, as `RleOvershoot`;
- fewer than 1024 pixels, as `RleUndershoot`.

A rejected **bundled** payload's upsert is **not** applied. That is
consistent with protocol `decode_pal_rle`, which returns no palette on error.
A later thin tile against that palette then reports `ThinUncachedPalette`
(3), and the server recovers from that (`io_bridge.rs:2310`).

The GPU shader check stays, as defence in depth, for consumers that build
`TileData` themselves (the GPU tests). Since prevalidation now catches the
case first, code 5 is reported on both clients.

Keep the two implementations independent. The client module header
(`pal_rle_decode.rs:5-9`) deliberately keeps the client port independent of
protocol's decoder, as an oracle. Unify their *accept sets* instead, with a
differential test.

**Rejected:**
1. *Route the client through `ghostframe_protocol::codec::pal_rle::decode_pal_rle`.*
   That loses the independent oracle, and the protocol decoder has no
   indices-raw.
2. *Check in `TileDecoder::write` and clamp.* That paints something
   plausible and still reports nothing, which is the bug.

### Files touched

`ghostframe-client-core/src/pal_rle_decode.rs`; tests in
`ghostframe-client-core/tests/` (`oracle_prevalidate.rs` exists, so extend it);
doc comment of `ghostframe-e2e/tests/e2e.rs:1182-1210` (`e2e_palrle_oob_index`):
the code-5 report now originates in CPU prevalidation, not the shader. The
test's assertion, `error_code=5` in the server log, still holds.

### Implementation steps

1. Change `expand_rle_to_indices` to return `Result<Vec<u8>, DecodeErrorCode>`.
   A run that crosses 1024 gives `Err(RleOvershoot)`, and `pixel_idx != 1024` at
   the end gives `Err(RleUndershoot)`. It also takes `count: u8` and returns
   `Err(IndexOob)` on the first `idx >= count`.
2. Indices-raw branch (`:55-71`): after copying, scan both nibbles of the 512
   bytes against `shadow.count(palette_id)` and return `Err(IndexOob)`.
3. Bundled branch: pass the bundled `count`. Thin: pass `shadow.count(palette_id)`.
4. Leave `decode_pal_rle_tile` as is. It inherits the checks.
5. Fix the e2e doc comment.

### Tests

- `oracle_prevalidate.rs`, new cases:
  - Bundled, `count=3`, one RLE byte `0x5F` (index 5, run 16) followed by
    valid runs to 1024: `Err(IndexOob)`.
  - Thin with the shadow holding count 2, index 2: `Err(IndexOob)`.
  - Indices-raw with one high nibble `0xF` against count 4: `Err(IndexOob)`.
  - 1025 pixels: `Err(RleOvershoot)`. 1023 pixels: `Err(RleUndershoot)`.
  - Boundary case: index `count-1` passes.
- `ClientCore`-level test (extend `client-core/tests/hostile_input.rs`): a
  PLAIN-framed PalRle tile with an out-of-range index. Assert an
  `Event::DecodeError { code: IndexOob, .. }`, no `TilePayload` for that tile,
  and a `PollOutput::Stream([0x04, 2, x, y, 5])` from `poll_transmit`.
- Differential proptest in `oracle_prevalidate.rs`. Over arbitrary payloads
  with `flags ∈ {0x00, 0x01}` and an arbitrary cached palette count,
  `prevalidate_pal_rle(..).is_ok() == decode_pal_rle(..).is_ok()`, and on
  error the classes match: `IndexOutOfRange → IndexOob`,
  `PixelCountMismatch → RleOvershoot | RleUndershoot`,
  `PaletteCountOutOfRange → CountOutOfRange`,
  `UncachedPalette → ThinUncachedPalette`,
  `Truncated → PayloadTooShort | BundledTruncated`.
  The same assertion goes into the WP-5 `decode_tile_payload` fuzz target.
  Prove the generator is non-vacuous: assert that at least 10% of cases hit
  each class.

**Mutation check:** delete the `idx >= count` comparison. The index cases
and the differential proptest fail. Revert the overshoot and undershoot codes
to `PayloadTooShort`. The two RLE cases fail.

### Gates

`cargo test -p ghostframe-client-core`, clippy workspace. No wire change.
`e2e_palrle_oob_index` needs no rerun (same observable), but run it once
locally after `just containers-build` to confirm the log line still appears.

### Risks / owner decisions

None of substance. The server encoder never produces an out-of-range index.
An OOB index means a server bug or hostile input, so dropping the upsert
costs at most one palette resend.

**Size: S.**

---

## WP-3 — Native WebTransport CONNECT response: unbounded buffer, no deadline, re-fires after accept (Low)

### Status at e85fb005: CONFIRMED, plus a related latent defect

- `WebTransportHandshake::response_buf: BytesMut` (`ghostframe-client-net/src/handshake.rs:49-50`)
  grows with every readable event (`:121-127`). `drain_stream`
  (`:166-185`) reads `usize::MAX` per chunk. Reading returns flow-control
  credit to quinn, so a server that streams headers without ever completing
  the frame grows the buffer without bound.
- No handshake deadline. `ClientNet` uses `TransportConfig::default()`
  (`lib.rs:350-352`). quinn's idle timeout is reset by every packet, so a
  peer that trickles bytes holds the client in `Pending` indefinitely.
  `net_thread.rs` only arms `ClientNet::poll_timeout()` (`:381, :493`).
- Latent: after `Accepted`, the client keeps calling `on_readable` for every
  `Readable` on the session stream (`lib.rs:472-484`). `try_decode_response`
  re-decodes from the start of the unconsumed buffer (`handshake.rs:133-156`),
  so any later byte or FIN on the CONNECT stream emits `SessionReady` again,
  and with it another `ClientEvent::Connected` (`net_thread.rs:467-468`). It
  also keeps appending to the buffer. Today's server writes nothing after the
  200 (`ghostframe-lib/src/transport/webtransport.rs:850-880`), so this is
  latent.
- The browser client uses the browser's WebTransport and does not go through
  this code.

### Design

**Chosen:** bound the handshake in the sans-IO layer (`ClientNet`), so the
browserless and client-net tests can drive it with virtual time.

```rust
// sketch — ghostframe-client-net/src/handshake.rs
pub(crate) const MAX_CONNECT_RESPONSE_BYTES: usize = 16 * 1024;
pub(crate) const CONNECT_TIMEOUT_US: u64 = 10_000_000;
enum State { Idle, Waiting { deadline_us: u64 }, Done }
// on_readable: in Done, drain and discard (credit returned, nothing kept).
// Pending with response_buf.len() > MAX_CONNECT_RESPONSE_BYTES -> Rejected("CONNECT response exceeds 16 KiB").
```

`ClientNet::poll_timeout` includes the handshake deadline while `Waiting`.
`on_timeout` past it emits `ConnectionLost { reason: "WebTransport CONNECT timed out" }`
and closes the connection (`conn.close(now, VarInt(0), b"connect timeout")`).

After `Accepted` or `Rejected` the state is `Done`, and `on_readable` never
reports an outcome again.

**Owner decision (optional step 5):** under the WebTransport draft, a FIN on
the CONNECT stream after acceptance means the session closed. Mapping that to
`ConnectionLost` is the correct behaviour, but no current server sends it.
Recommend doing it.

**Rejected:**
1. *Set quinn's `max_idle_timeout` low.* It does not bound a trickling peer,
   and it would shorten the live session's idle timeout too.
2. *Put the deadline in `net_thread.rs`.* That makes it untestable without a
   real event loop, and the capi embedding would need it duplicated.

### Files touched

`ghostframe-client-net/src/{handshake.rs, lib.rs}`; tests in
`ghostframe-client-net/tests/handshake.rs` (exists; e2e.yml runs
`cargo test -p ghostframe-client-net`, all targets).

### Implementation steps

1. Add the state enum and constants to `WebTransportHandshake`. `start(conn, server_name, now_us)`
   sets `Waiting { deadline_us: now_us + CONNECT_TIMEOUT_US }`. The caller at
   `lib.rs:466` passes `now_us`, which `drain_connection_events` already has.
2. `on_readable`: in `Done`, drain and drop the bytes and return `Pending`
   (rename it to an `Ignored` variant if clearer). Otherwise append. Then, if
   the buffer exceeds the cap, return `Rejected` and move to `Done`. Then try
   to decode, and on `Accepted` or `Rejected` move to `Done`.
3. Add `handshake.deadline_us() -> Option<u64>` and fold it into
   `ClientNet::poll_timeout`. In `ClientNet::on_timeout`, if it is due and
   `!session_ready`, push `ConnectionLost`, close the connection and set `Done`.
4. Add a unit test hook: keep `try_decode_response` testable without a
   connection, as today.
5. (Owner OK) A FIN in `Done` after `Accepted` becomes `ConnectionLost("session stream closed")`.

### Tests

- `handshake.rs` unit tests:
  - `response_over_cap_is_rejected`: feed 16 KiB + 1 of a never-completing
    HEADERS prefix in 1-byte steps. Assert `Pending` until the cap, then
    `Rejected`. Property: the buffer length is never above the cap plus the
    last chunk.
  - `accept_is_reported_once`: feed a full 200 response, then 100 more bytes.
    Assert exactly one `Accepted` over all calls.
- `client-net/tests/handshake.rs`: a stub server that completes QUIC but
  never answers CONNECT (the existing test helpers build a `QuicServer`).
  Advance virtual `now_us` by `CONNECT_TIMEOUT_US + 1` through
  `poll_timeout` / `on_timeout`. Assert one `ClientNetEvent::ConnectionLost`
  whose reason contains "timed out". Prove the timer path ran: assert that
  `poll_timeout()` returned the handshake deadline beforehand.

**Mutation check:**
- Remove the cap check. `response_over_cap_is_rejected` fails.
- Remove `Done` gating. `accept_is_reported_once` fails with two `Accepted`.
- Drop the deadline from `poll_timeout`. The integration test fails: no
  `ConnectionLost`, because quinn's 30 s idle timeout is longer than the
  advanced time.

### Gates

`cargo test -p ghostframe-client-net`, `just test-client`,
`just ci-client`. No wire change, no server change.

**Size: S.**

---

## WP-4 — `RefContinuity` keeps unbounded SPS/PPS tables and is unused (Low)

### Status at e85fb005: CONFIRMED

- `RefContinuity` (`ghostframe-client-h264/src/bitstream.rs:442-~540`) stores
  `sps: Vec<Sps>` and `pps: Vec<(u32, u32)>`. It de-duplicates by id with a
  `retain` followed by a `push` (`:479-489`), so each table is bounded only
  by the number of distinct ids. The ids are unchecked `ue(v)` values:
  `parse_sps` (`:269`, `let id = r.ue()?`) and `parse_pps_ids` (`:386-389`).
  The spec limits them to 0..=31 and 0..=255. A hostile stream can grow both
  tables without limit, and each NAL costs O(n) through `retain`.
- **Unused in production.** `rg 'RefContinuity|Continuity::'` outside
  `bitstream.rs` returns nothing (the one hit,
  `ghostframe-lib/src/capture/dmabuf.rs:1045`, is an unrelated comment). Only
  its own tests (`bitstream.rs:712-840`) use it. The production H.264 loss
  handling is the acked-reference gate (`client-core/src/h264_gate.rs`).
- Still used: `parse_sps`, `Sps` and `low_latency_sps`, by
  `decoder_gst.rs:466` and `oracle_tests_gst.rs:365`.

### Design

**Chosen:** delete `RefContinuity`, `Continuity`, `RefContinuity::slice`,
`read_slice`, and their tests. Rewrite module doc point 1
(`bitstream.rs:6-14`) to say loss detection is the acked-reference gate's job
(`h264_gate`). Keep `parse_sps` / `Sps` / `low_latency_sps`.

Separately, harden `parse_sps` to reject `id > 31` (`None`), since
`low_latency_sps` parses untrusted SPS on the GStreamer path.

**Rejected:** bounding the tables to 32/256 entries. It keeps dead code alive
and keeps the O(n²) worst case.

### Files touched

`ghostframe-client-h264/src/bitstream.rs` only.

### Implementation steps

1. Delete the items above. Run `cargo clippy -p ghostframe-client-h264 --all-targets`
   and `--no-default-features --features gstreamer-backend,test-support`.
   Remove any helper the deletion leaves unused (`dead_code` will flag it).
2. Add `if id > 31 { return None; }` in `parse_sps`.
3. Update the module doc.

### Tests

- Existing `low_latency_sps` tests stay green: `just test-client-gstreamer`
  and `cargo test -p ghostframe-client-h264 --lib`.
- New unit test `parse_sps_rejects_out_of_range_id`: hand-build an SPS with
  `seq_parameter_set_id = 32` (exp-Golomb), assert `parse_sps(..) == None` and
  `low_latency_sps(au) == None`.

**Mutation check:** remove the `id > 31` guard. The new test fails.

### Gates

`cargo test -p ghostframe-client-h264 --lib`, `just test-client-gstreamer`,
clippy workspace.

**Size: S.**

---

## WP-5 — Fuzzing is thin and never runs (Medium)

### Status at e85fb005: CONFIRMED

- Only `ghostframe-client-net/fuzz/` exists, with two targets:
  - `decode_tile_payload`: Solid, CDF 5/3 prevalidate and validate, PalRle
    prevalidate and decode. It is pure and useful.
  - `handle_udp`: its own header admits quinn discards everything at header
    parse, `cov: 674`.
- Nothing references fuzzing in `Justfile`, `ci/` or `.github/workflows/`
  (`rg -i 'fuzz|miri'` is empty). Last touched `e8441d62`.
- No stateful `ClientCore::handle_datagram` target. Every server→client byte
  now goes through FEC v2 framing (`fec_rx`), which is the largest untested
  parser.
- Server inbound parsers, with where they live today:

| Message | Entry point (called from) | Lives in |
| --- | --- | --- |
| datagram classifier | `classify_inbound` (`ack_dispatch.rs:780`) | protocol |
| TILE_NACK 0x05 | `TileNackEnvelope::decode` | protocol |
| FRAME_ACK 0x0A | `FrameAck::decode` | protocol |
| ACK v4 0x08 | `fec::feedback::BundleAckBatch::decode` (`ack_dispatch.rs:121`) | protocol |
| LOSS_REPORT 0x07 | `fec::feedback::LossReport::decode` (`repair_status.rs:102`) | protocol |
| REPAIR_REQUEST 0x0B | `fec::feedback::RepairRequest::decode` (`repair_status.rs:146`) | protocol |
| repair-status stream | `fec::repair_status::StatusReader::push` (`repair_status.rs:184`) | protocol |
| feedback-stream **framing loop** | `IoBridge::dispatch_feedback_bytes` (`io_bridge.rs:1990-2210`) | **lib, inside IoBridge** |
| FEEDBACK 0x01 | `ReceiverFeedback::decode` | protocol |
| HELLO 0x03 | `client_caps::HelloMsg::decode` | **lib** |
| DECODE_ERROR 0x04 | `decode_error::DecodeErrorMsg::decode` | **lib** |
| INPUT 0x05 | `input_inject::{required_len, decode_input_msg}` | **lib** |
| DISPLAY_INFO 0x07 / MODE 0x08 | `display::{DisplayInfoMsg, DisplayModeMsg}::decode` | **lib** |
| RESET_REQUEST 0x09 | `feedback_stream::ResetRequest::decode` | protocol |
| STATS_SUBSCRIBE 0x0B | `stats::StatsSubscribe::decode` | protocol |
| CONNECT request | `web_transport_proto::ConnectRequest::decode` (`webtransport.rs:818`) | third party |

  The parsers in **bold** cannot be fuzzed without linking `ghostframe-lib`.
  That crate is AGPL, and it pulls in Vulkan and VA-API plus ghostbridge's Go
  build, which is impractical for a fuzz crate. Moving them is WP-6.

### Design

**Chosen:**

1. **One fuzz crate at the repo root, `fuzz/`**, outside the workspace
   (`[workspace]` table, `cargo-fuzz = true`), MIT OR Apache-2.0. It depends
   on `ghostframe-protocol`, `ghostframe-client-core` and
   `ghostframe-client-net`, never on `ghostframe-lib`: the parsers it needs
   move to protocol in WP-6, and `ci/check-licenses.py` rule 2 would force the
   fuzz crate AGPL otherwise. `ghostframe-client-net/fuzz/` moves into it, so
   there is one corpus layout and one recipe.
2. **Targets:**

| Target | Drives | Asserts beyond "no panic" |
| --- | --- | --- |
| `decode_tile_payload` (moved) | as today, plus the WP-2 differential vs `codec::pal_rle::decode_pal_rle` | accept sets agree (WP-2 mapping) |
| `fec_frame` | `fec::frame::parse`, `records`, `source_from_symbol` | `parse(encode(parse(x)?)) == parse(x)` for SOURCE/REPAIR/PLAIN headers (round trip) |
| `client_core_stateful` | `ClientCore::new`, then a `Vec<Op>` from `arbitrary`: `Datagram(Vec<u8>)`, `StructuredDatagram(Spec)`, `ServerStream(Vec<u8>)`, `Advance(u32 µs)`, `Timeout`, `PollTransmit`, `H264Result{..}`, `RequestReset`, `SetDisplayMode` | `poll_timeout()` is `Some` and `>=` the previous `now_us` after `on_timeout`; `assembly_stats()` / `block_stats()` held-entry counts stay under their documented caps; every `PollOutput::Datagram` classifies to a known `InboundKind` (via `classify_inbound`), and every stream output starts with a byte in `FEEDBACK_STREAM_MSG_TYPES` |
| `server_inbound_datagram` | `classify_inbound`, then the matching protocol decoder | decoders only accept their own discriminator; `decode(encode(decode(x)?)) == decode(x)` where an encoder exists |
| `feedback_stream` | WP-6's `FeedbackStreamReader`, in arbitrary chunkings of the same byte string | the message sequence is chunking-independent; retained tail `< 64` bytes after `NeedMore` (today a `debug_assert` at `io_bridge.rs:2196-2201`) |
| `repair_status_stream` | `StatusReader::push` in arbitrary chunkings | same chunking-independence; an error is sticky |
| `stats_wire` | `stats::*::decode`, `decode_payload` with a decoded schema | no panic; schema-bound payload length checks |
| `handle_udp` (moved) | as today | keep, cheap |

   `StructuredDatagram(Spec)` is what gets the stateful target past the FEC
   framing. It builds a valid PLAIN or SOURCE frame (`PlainFrame::encode`,
   `SourceFrame::encode`, `push_record`) whose records come from the real
   encoders with fuzzer-chosen fields: `fragment_tile`, `fragment_frame_ref`,
   `build_frame_dimensions_datagram`, the eviction notice, and a REPAIR from
   `fec::codec`. Raw `Datagram` keeps the malformed coverage.
3. **Corpus seeding from the existing encoders:** a `[[bin]] gen_corpus` in
   the fuzz crate. It writes `fuzz/corpus/<target>/seed-*` from the protocol
   and client-core encoders:
   - server→client: the encoders above;
   - client→server: `encode_hello`, `encode_display_info` / `_mode`, the input
     encoders, `DecodeErrorBatcher::report`, `TileNackEnvelope::encode`,
     `FrameAck::encode`, `BundleAckBatch`, `LossReport` and `RepairRequest`
     encoders, `StatusRecord::encode`;
   - payloads: `encode_pal_rle_payload*` and the CDF 5/3 encoder.

   The corpus is not checked in, in keeping with the owner's rule against
   large blobs. The nightly runs `gen_corpus` and then restores the cached
   corpus on top.
4. **CI:**
   - **Nightly job** in `.github/workflows/nightly.yml`, alongside `e2e-sweep`
     and wired into the same `report-failure` issue.
     `cargo +${FUZZ_NIGHTLY} fuzz run <t> -- -max_total_time=300 -rss_limit_mb=2048`
     per target, as a matrix. `FUZZ_NIGHTLY` is a dated nightly pinned in the
     workflow env: `rust-toolchain.toml` pins 1.96.1, and an explicit `+toolchain`
     overrides that. The corpus is cached with `actions/cache` keyed by target,
     and `fuzz/artifacts/` is uploaded on failure.
   - **PR-time compile guard:** `cargo check --manifest-path fuzz/Cargo.toml --bins`
     on the pinned stable in `client-core.yml`, so targets cannot rot. Verify
     `libfuzzer-sys` checks on stable (its build script compiles libFuzzer
     C++, and ubuntu runners have a C++ compiler). If it does not, use a
     `--features` gate.
   - `just fuzz <target> [secs]` and `just fuzz-corpus` recipes.
5. **Miri nightly**, a separate job in the same workflow:
   `PROPTEST_CASES=8 cargo +${FUZZ_NIGHTLY} miri test -p ghostframe-protocol -p ghostframe-client-core --lib --tests`.
   - The value is real: `client-core/src/tile_out.rs:68-91` has
     `unsafe fn from_raw` and raw row pointers, and protocol's FEC depends on
     `rusty_erasure`, about 110 `unsafe` sites.
   - Tests Miri cannot run (SIMD intrinsics inside `rusty_erasure`, very long
     proptests) get `#[cfg_attr(miri, ignore)]`. The job prints the ignored
     count, and the PR lists each ignore with its reason.

**Rejected:**
1. *Fuzz through `ghostframe-lib`* (a lib-side fuzz crate calling
   `IoBridge::dispatch_feedback_bytes`). It is AGPL and needs ffmpeg dev
   libraries, Vulkan and the Go toolchain, so the fuzz build gets the
   workspace's weight. `IoBridge` is also not constructible without a backend.
2. *Per-crate fuzz directories* (protocol/fuzz, client-core/fuzz). That means
   three lockfiles, three corpora and three recipes for one wire.
3. *Fuzz on every PR.* Short fuzz runs find little and cost runner minutes.
   The PR-time compile guard plus the nightly is the right split.
4. *OSS-Fuzz.* It is not appropriate for a private, undeployed project.

### Files touched

- New: `fuzz/{Cargo.toml, fuzz_targets/*.rs, src/bin/gen_corpus.rs}` and `fuzz/.gitignore`
  (`corpus/`, `artifacts/`, `target/`).
- Delete `ghostframe-client-net/fuzz/`.
- `.github/workflows/{nightly.yml, client-core.yml}`, `Justfile`,
  `DEVELOPERS.md` (a "Fuzzing" section), and `#[cfg_attr(miri, ignore)]`
  sprinkles in protocol and client-core tests.
- Possibly a `pub fn` test-hook accessor in client-core if the invariants need
  a cap constant that is private today. Prefer making the cap constant `pub`.

### Implementation steps

1. Land WP-6 first. The server parsers and `FeedbackStreamReader` must be in
   protocol.
2. `git mv ghostframe-client-net/fuzz fuzz`. Fix the relative `path =` deps
   and the crate name (`ghostframe-fuzz`). Add `arbitrary = { version = "1", features = ["derive"] }`.
3. Write the targets in the table, one file each. In `client_core_stateful`,
   cap the op count (say 256) and the datagram size (2048) so exec/s stays
   above about 1k.
4. Write `gen_corpus`. Each seed must round-trip through its decoder
   (`assert!` in the generator), so a seed that stopped parsing after a wire
   change fails loudly.
5. Add the Justfile recipes, the nightly and Miri jobs, and the PR-time check.
6. Run each target locally for 10 minutes. Record exec/s and `cov:` in the PR
   description. Fix or file what it finds. A crash found here is a finding,
   not a reason to weaken the target.

### Tests

The targets are the tests. Each target's non-vacuity has to be proved, since
a fuzzer that never reaches the code reports green:

- **Coverage proof:** `cargo fuzz coverage <t>` plus `llvm-cov report`, run
  once for the PR description. `client_core_stateful` must show line coverage
  in `fec_rx/blocks.rs`, `reassembly.rs::handle_source_tile`,
  `frame_assembly.rs` and `h264_gate.rs`.
- **Mutation checks**, each run for 60 s, with the expected crash found:
  - Remove the length check from `DisplayInfoMsg::decode` (after its move to
    protocol). `server_inbound_datagram` or `feedback_stream` panics on index.
  - In `fec::frame::parse`, change `need` for REPAIR to 8. `fec_frame` panics
    at `dg[8]`.
  - Comment out the `frag_total > MAX_FRAME_FRAGMENTS` guard
    (`frame_assembly.rs:104`). `client_core_stateful`, with an RSS limit,
    reports OOM or slow-unit.
  - Revert one WP-2 check. `decode_tile_payload`'s differential assertion
    fires.
- **Miri mutation:** in `tile_out.rs`'s unit test, index one stride past the
  buffer. Miri reports UB.

### Gates

`cargo check --manifest-path fuzz/Cargo.toml --bins` (stable), the nightly
job green on its first run (`workflow_dispatch`), `just ci-local`
(`check-licenses.py` must accept `fuzz/`; it is outside the workspace, but
its files need `SPDX-License-Identifier: MIT OR Apache-2.0`).

### Risks / owner decisions

- **Owner:** approve nightly-toolchain usage in CI. It is the first nightly in
  the repo, confined to the fuzz and Miri jobs.
- Miri may be impractically slow on `rusty_erasure` tests. Fallback: run Miri
  on client-core only, plus protocol modules other than FEC, and record why.
- Runner minutes: about 8 targets × 5 min plus Miri, roughly 1 h of nightly
  compute.

**Size: L** (M if `FeedbackStreamReader` is counted in WP-6).

---

## WP-6 — Wire codecs duplicated between server and client (Medium)

### Status at e85fb005: CONFIRMED, broader than reported

| Message | Server (lib) | Client | Shared in protocol today |
| --- | --- | --- | --- |
| DECODE_ERROR 0x04 | `transport/decode_error.rs:22` `DECODE_ERROR_SIZE = 5`, `DecodeErrorMsg::{decode, encode}` `:33-63`, codes `ERR_*` `:24-30` (1..=7 only) | `client-core/src/decode_error_batcher.rs:36` `DECODE_ERROR_SIZE = 5`, hand-built `vec![..]` `:85-90`; codes as `DecodeErrorCode` (`event.rs:14-25`, 1..=10) | only `DECODE_ERROR_MSG_TYPE` |
| HELLO 0x03 | `transport/client_caps.rs:40-80` `HelloMsg`, `ClientCapabilities` | `client-core/src/loss_tracker.rs:140-152` `encode_hello` | type, size, cap bits |
| DISPLAY_INFO 0x07 / MODE 0x08 | `transport/display.rs:22-96` (sizes 11/5, BE) | `loss_tracker.rs:164-197` `encode_display_info` / `_mode` (sizes as literals 11/5) | only the msg types |
| INPUT 0x05 | `transport/input_inject.rs:17-37` sub-kinds plus `required_len`, `:80-130` `decode_input_msg` | `client-core/src/input.rs:16-80` encoders, sub-kinds as literals | only the msg type |
| FEEDBACK 0x01, RESET 0x09, STATS_SUB 0x0B, all datagram envelopes | shared | shared | yes (good) |

- `client-core/tests/oracle_feedback.rs` pins HELLO bytes as literals
  (`:8-25`). Those are byte-exact oracles standing in for a shared codec.
- `DecodeErrorCode` u8→enum mapping is hand-written **twice**:
  `ghostframe-render/src/tile_renderer.rs:217-230` (`decode_error_code(u32)`)
  and `ghostframe-client-wasm/src/app/client.rs:44-58`
  (`decode_error_code_from_u8`). The lib keeps a third, partial copy as
  `ERR_*` constants covering 1..=7, with no CDF 5/3 codes 8..=10. The old
  "client-gpu renderer" mapping now lives in `ghostframe-render`.

### Root cause

Message codecs were written next to their first consumer. The protocol crate
then gained the type bytes (commit `1840505f`, "one definition of the
feedback-stream types and wire constants") but not the layouts.

### Design

**Chosen:** move every client→server feedback-stream message layout into
`ghostframe-protocol::feedback_stream`, with an `encode` and a `decode` side
by side. The lib and client-core keep only re-exports and their own logic
(rate limiting, injection, clamping). Move `DecodeErrorCode` to protocol with
`TryFrom<u8>`, re-exported from client-core so `ghostframe_client_core::DecodeErrorCode`
paths keep compiling.

Also extract the feedback-stream framing loop as a pure
`FeedbackStreamReader`, which WP-5 needs and which removes the largest parser
from `IoBridge`.

```rust
// sketch — ghostframe-protocol/src/feedback_stream.rs (additions)
#[repr(u8)] pub enum DecodeErrorCode { PayloadTooShort = 1, /* ... */ Cdf53RleLength = 10 }
impl TryFrom<u8> for DecodeErrorCode { type Error = ProtocolError; /* UndefinedValue */ }
pub const DECODE_ERROR_SIZE: usize = 5;
pub struct DecodeErrorMsg { pub codec: u8, pub tile_x: u8, pub tile_y: u8, pub error_code: u8 }
impl DecodeErrorMsg { pub fn code(&self) -> Option<DecodeErrorCode>; pub fn encode(&self, buf: &mut Vec<u8>); pub fn decode(d: &[u8]) -> Result<Self, ProtocolError>; }
pub struct ClientCapabilities { pub indices_raw_enabled: bool, pub supports_h264: bool, pub h264_acked_ref: bool }
pub struct HelloMsg { pub caps: ClientCapabilities }               // moved verbatim
pub const DISPLAY_INFO_SIZE: usize = 11; pub const DISPLAY_MODE_SIZE: usize = 5;
pub struct DisplayInfoMsg { .. } pub struct DisplayModeMsg { .. }  // moved verbatim
pub enum InputMsg { PointerMove{x:i16,y:i16}, PointerButton{..}, Wheel{..}, KeyDown{keysym:u32}, KeyUp{keysym:u32} }
impl InputMsg { pub fn required_len(sub_kind: u8) -> Option<usize>; pub fn decode(d: &[u8]) -> Result<(Self, usize), ProtocolError>; pub fn encode(&self, buf: &mut Vec<u8>); }

pub enum FeedbackMsg { Feedback(ReceiverFeedback), Hello(HelloMsg), DecodeError(DecodeErrorMsg),
    ResetRequest(Option<ResetRequest>), DisplayInfo(DisplayInfoMsg), DisplayMode(DisplayModeMsg),
    StatsSubscribe(Result<StatsSubscribe, StatsWireError>), Input(InputMsg) }
pub enum Next { Msg(FeedbackMsg), NeedMore, Garbage { msg_type: u8 } }
#[derive(Default)] pub struct FeedbackStreamReader { buf: Vec<u8>, off: usize }
impl FeedbackStreamReader {
    pub fn push(&mut self, bytes: &[u8]);
    pub fn peek_type(&self) -> Option<u8>;   // for the per-message HELLO gate
    pub fn next(&mut self) -> Next;          // one message at a time
    pub fn discard(&mut self);               // garbage / non-live: drop the tail
    pub fn pending_len(&self) -> usize;
}
```

`IoBridge::dispatch_feedback_bytes` keeps its *policy*: the per-message HELLO
gate (`io_bridge.rs:2025-2042`), which runs before each `next()` through
`peek_type()`, and the per-message handlers. The framing moves into the
reader. `feedback_recv_bufs: HashMap<ConnectionHandle, Vec<u8>>`
(`io_bridge.rs:942`) becomes `HashMap<ConnectionHandle, FeedbackStreamReader>`.
Behaviour is identical: fixed-length messages that fail to decode are still
skipped (ResetRequest with an undefined reason, StatsSubscribe errors), and
unknown types and unknown INPUT sub-kinds discard the tail.

**Licence (owner decision):** the moved code goes from AGPL files
(`ghostframe-lib`) into MIT OR Apache-2.0 files (`ghostframe-protocol`).
Every SPDX header changes with it. The owner holds the copyright, so this is
a relicensing decision for the owner, not a technical one. It is consistent
with the licensing split, since protocol is shared by design. Get an explicit
yes in the PR.

**Rejected:**
1. *Keep the lib codecs and add a cross-crate round-trip test.* It catches
   drift but keeps two implementations, and the server parsers stay
   unfuzzable without the lib.
2. *Move `dispatch_feedback_bytes` whole into protocol.* The handlers need
   `IoBridge` state. Only the framing is protocol.

### Files touched

- `ghostframe-protocol/src/{feedback_stream.rs, protocol.rs (ProtocolError variants)}`.
- `ghostframe-lib/src/transport/{client_caps.rs, decode_error.rs, display.rs, input_inject.rs}`:
  codecs become `pub use ghostframe_protocol::feedback_stream::{..}`. Keep
  `DisplayError` / the controller trait and `apply_input` / `InputInjector`
  where they are.
- `ghostframe-lib/src/transport/io_bridge.rs` (`dispatch_feedback_bytes`,
  `feedback_recv_bufs`, `handle_decode_error`'s `ERR_THIN_UNCACHED_PALETTE`
  becomes `DecodeErrorCode::ThinUncachedPalette`) — **contended**.
- `ghostframe-lib/include/ghostframe.h`: regenerate; `ERR_*` /
  `DECODE_ERROR_SIZE` / `DISPLAY_*_SIZE` leave the lib's public consts.
- `ghostframe-client-core/src/{event.rs, decode_error_batcher.rs, loss_tracker.rs, input.rs, lib.rs}`.
- `ghostframe-render/src/tile_renderer.rs:217-230`.
- `ghostframe-client-wasm/src/app/client.rs:44-58`.
- Tests: `client-core/tests/oracle_feedback.rs`,
  `client-core/tests/oracle_decode_error_batcher.rs`, and lib tests that
  import `crate::transport::decode_error::*` (e.g. `io_bridge/tests.rs:2849`).

### Implementation steps

1. Protocol: move `DecodeErrorCode` (`client-core/src/event.rs:12-25`) to
   `feedback_stream.rs` with `TryFrom<u8>`. Re-export it from `client-core/src/event.rs`
   (`pub use ghostframe_protocol::feedback_stream::DecodeErrorCode;`), so the
   `lib.rs:37` re-export stays valid.
2. Protocol: move `HelloMsg` / `ClientCapabilities`, `DecodeErrorMsg` plus
   size, `DisplayInfoMsg` / `DisplayModeMsg` plus sizes, and `InputMsg` plus
   `required_len` plus decode, then add `InputMsg::encode`. Move their unit
   tests with them (`client_caps.rs` tests, `decode_error.rs:65-104`,
   `display.rs` tests, `input_inject.rs:150+` incl.
   `required_len_matches_decoder_consumed_length`).
3. Lib: replace the moved bodies with `pub use` re-exports. Make
   `input_inject::required_len` a re-export of `InputMsg::required_len`.
4. Client-core:
   - `encode_hello` → `HelloMsg { caps }.encode`;
   - `encode_display_info` / `_mode` → the structs' `encode`;
   - `DecodeErrorBatcher::report` → `DecodeErrorMsg { .. }.encode`, deleting
     the local `DECODE_ERROR_SIZE`;
   - `input.rs` encoders → `InputMsg::encode` into a fixed array (keep the
     `[u8; N]` return types; the callers in native and wasm use them).
5. Render and wasm: replace both hand mappings with
   `u8::try_from(code).ok().and_then(|c| DecodeErrorCode::try_from(c).ok())`
   in `tile_renderer.rs` (shader code is `u32`) and `DecodeErrorCode::try_from(v).ok()`
   in wasm.
6. Protocol: add `FeedbackStreamReader`. Port the loop from
   `io_bridge.rs:2019-2189` into `next()` 1:1, including the
   "need at least 2 bytes for INPUT" rule and the unknown-sub-kind discard.
7. Lib: rewrite `dispatch_feedback_bytes` to
   `push; loop { gate on peek_type; match next() { Msg(m) => handle, NeedMore => break, Garbage => { warn; discard; break } } }`.
   Keep every tracing line and counter.
8. Regenerate `ghostframe.h` (cbindgen) and commit it.

### Tests

- Protocol, in `feedback_stream.rs`: for every message, a proptest
  `decode(encode(m)) == m`. Plus "every size constant equals the encoded
  length", which replaces the literal `11`/`5` in client-core.
- `DecodeErrorCode::try_from(v)`: `Ok` for exactly 1..=10, and
  `try_from(c as u8) == Ok(c)` for every variant.
  Enumerate the variants with a `const ALL: [DecodeErrorCode; 10]` checked by
  an exhaustive `match`, so a new variant breaks compilation of the test.
  This follows the "load-bearing catch-all" memory: test against the
  constant, never a literal.
- `FeedbackStreamReader` proptest: a random `Vec<FeedbackMsg>`, encoded and
  concatenated, then split at random points and pushed chunk by chunk. The
  same sequence must come out, and `pending_len() < 64` after every
  `NeedMore`.
- Lib regression: the existing `io_bridge` feedback tests (HELLO gating, split
  PointerButton, unknown type discard) pass unchanged. Run
  `cargo test -p ghostframe-lib --lib io_bridge::`.
- Delete `oracle_feedback.rs`'s literal HELLO assertions, or keep a single one
  as a wire-pin (`[0x03, 0x01]`) next to a comment that the round trip lives
  in protocol.

**Mutation check:**
- Swap two fields in `DisplayInfoMsg::encode`. The protocol round trip
  passes, because both sides moved together, but the wire-pin test (keep one
  literal-bytes test per message in protocol, taken from today's lib tests)
  fails.
- Make the reader retain on unknown types instead of discarding. The lib's
  `unknown type` test and the reader proptest fail.

### Gates

This is a wire-code change across the four consumers, although the bytes do
not change. Run clippy workspace, `just ci-local` (cbindgen diff,
`check-licenses.py` for the SPDX headers of moved code), `cargo build -p ghostframe-client-wasm --target wasm32-unknown-unknown`,
`just build-release`, then `just containers-build` and the input and display
e2e (`e2e_input*`, `e2e_display*`, `e2e_palrle_oob_index`).

### Risks / owner decisions

- Relicensing of the moved code (above).
- `io_bridge.rs` contention: step 7 rewrites about 190 lines of
  `dispatch_feedback_bytes`. Coordinate with whichever group owns
  `io_bridge.rs` lifecycle and HELLO-gating work. That is the one hunk that
  will conflict.

**Size: M.**

---

## WP-7 — Protocol-crate consistency (Low, design)

### Status at e85fb005: CONFIRMED, partly

- **No wire version or epoch.** HELLO is `[0x03][caps]`
  (`feedback_stream.rs:28-30, 59-72`), with reserved bits `0xF8`. The CONNECT
  URL carries nothing: the native client uses `/.well-known/webtransport`
  (`client-net/src/handshake.rs:92`), the browser uses `https://{host}/`
  (`client-wasm/src/app/bootstrap.rs:61`), and the server does not look at
  the path (`webtransport.rs:818-830`, logs it only). AGENTS.md documents the
  failure mode of a mismatch: "the symptom is not a clear error — it is a NACK
  storm". A stale cached wasm bundle after a soft reload is exactly such a
  mismatch.
- **Error types are mixed.** `ProtocolError`, `fec::frame::FrameError`,
  `fec::codec::FecError`, `fec::feedback::FeedbackError`,
  `fec::repair_status::StatusError`, `stats::StatsWireError`,
  `codec::solid::SolidDecodeError` and `codec::pal_rle::PalRleDecodeError`
  sit alongside `Option`-returning decoders (`ReceiverFeedback::decode`,
  `feedback.rs:34`; `ResetRequest::decode`, `feedback_stream.rs:118`; and the
  lib's HELLO / DECODE_ERROR / DISPLAY / INPUT). `AckDecodeError` from the
  older report no longer exists: ACK v4 errors are `FeedbackError`.
- **Endianness is mixed.** Tile and frame headers and all feedback-stream
  messages are BE (`protocol.rs:84-90, 418-427`; `display.rs:5`; `input.rs:4`).
  TILE_NACK, FRAME_ACK and all FEC v2 frames and feedback are LE
  (`protocol.rs:648-651`, `fec/frame.rs:5`).
- **`as u16` truncation.** `fragment_tile` `frag_total = chunks.len() as u16`
  (`protocol.rs:257`, `frag_idx: idx as u16` `:265`); `fragment_frame_ref`
  likewise (`:590, :598`); the server tracker
  `on_frame_sent(seq, is_idr, datagrams.len() as u16)` (`io_bridge/frame.rs:1745`).
  A tile is at most one datagram (`MAX_TILE_PAYLOAD`), so the tile case cannot
  happen. The frame case wraps above 65 535 fragments, about 72 MB at 1.1 kB
  fragments. The client refuses `> MAX_FRAME_FRAGMENTS = 16 384`
  (`client-core/src/lib.rs:90`, `frame_assembly.rs:61,104`), but the server
  never checks against that.

### Decisions

**Fix now:**

1. **Protocol epoch in the CONNECT URL, not in HELLO.**
   `ghostframe_protocol::WIRE_EPOCH: u16` (start at 1, bump on any wire
   change; the PR template and AGENTS.md "four consumers" note say so). Both
   clients connect to `…/?gf_epoch=<N>`: the native one through
   `handshake.rs:92`, the browser through `bootstrap.rs:61`. The browser
   WebTransport API cannot set headers, but it can set the URL. The server's
   `connect_response` (`webtransport.rs`) answers **400** with a body-less
   refusal and a rate-limited warn naming both epochs, using the same pattern
   as the Origin 403. The native client already surfaces
   `"CONNECT rejected: status 400"` (`handshake.rs:145-149`), and the wasm
   client should map that to "server and page versions differ: reload the
   page".
   - Rejected, HELLO epoch byte: HELLO arrives after QUIC, WT and stream
     setup, so the server can only drop the session, not explain itself. And
     a size change of HELLO makes an old client's stream misparse, which is
     the silent failure being fixed.
   - Rejected, ALPN: browsers do not expose it.
2. **Checked fragment counts.** Move `MAX_FRAME_FRAGMENTS` and
   `MAX_H264_ACCESS_UNIT_BYTES` (`client-core/src/lib.rs:87-93`) into
   `ghostframe-protocol::protocol`, re-exported by client-core. Then:
   - `fragment_frame_ref` returns `Result<Vec<Vec<u8>>, ProtocolError>`, with
     a new `ProtocolError::TooManyFragments { needed: usize, max: u16 }`
     whenever `chunks > MAX_FRAME_FRAGMENTS`, and uses `u16::try_from` for
     `frag_idx`.
   - `fragment_tile` returns `Err` if `payload.len() > MAX_TILE_PAYLOAD`. That
     is the real invariant: one datagram.
   - The server's `send_h264_frame` (`io_bridge/frame.rs:1716-1745`)
     handles the `Err` by counting `h264 stats` `frames_unfragmentable`,
     warning once per second, and handing the frame to the manager as
     `frame_oversize` (the same path `h264_refs_oversize`,
     `io_bridge/h264_refs.rs:998`, uses: a P frame turns suspect and recovers
     from its anchor, an IDR is re-planned).
   - `on_frame_sent(.., u16::try_from(len).expect(..))` is then provably in
     range.
3. **Result everywhere, one umbrella type.** Every protocol `decode` returns
   `Result`, with no more `Option`. Convert `ReceiverFeedback::decode`,
   `ResetRequest::decode` and the WP-6 moved decoders to
   `Result<_, ProtocolError>`, adding the variants `WrongType { expected: u8, got: u8 }`,
   `UnknownSubKind(u8)` and `UndefinedValue { field: &'static str, value: u8 }`.
   Keep the layer errors (`FrameError`, `FeedbackError`, `StatusError`,
   `StatsWireError`, codec errors): their structured fields feed counters.
   Add `pub enum WireError` in `protocol/src/lib.rs` with
   `#[error(transparent)] #[from]` for each, for callers that want one type
   (the fuzz harness, logging).
   - Rejected, one flat `DecodeError` enum: every caller would have to match
     variants of unrelated layers, and FEC's error-driven counters would lose
     their exhaustive matches.
4. **Delete the ping/pong datagram path**, which is dead and served before the
   HELLO gate: `PING_PAYLOAD` / `PONG_PAYLOAD` (`protocol.rs:18-19`),
   `handle_datagram_payload` (`ack_dispatch.rs:26-32`), `send_pong_framed`,
   the inline branch at `io_bridge.rs:3001-3012`, and the test at
   `io_bridge/tests.rs:779-781`. This is an owner check, since some external
   tool might use it. `rg` finds no client.

**Document only:**

5. **Endianness.** Add a table to `ghostframe-protocol/src/lib.rs` docs: which
   message family is BE and which is LE, and the rule "new messages are LE
   (FEC v2 convention); existing BE layouts stay". Changing it would be a
   lockstep wire break for no gain.

### Files touched

- `ghostframe-protocol/src/{lib.rs, protocol.rs, feedback.rs, feedback_stream.rs}`.
- `ghostframe-lib/src/transport/{webtransport.rs, io_bridge/frame.rs, io_bridge/ack_dispatch.rs, io_bridge.rs}`
  (the last two only for the ping deletion; **contended**).
- `ghostframe-client-net/src/handshake.rs`; `ghostframe-client-wasm/src/app/bootstrap.rs`
  and its connect-error surfacing; `ghostframe-client-core/src/{lib.rs, frame_assembly.rs}`
  (constant move); every caller of `fragment_frame*` / `fragment_tile` in
  tests (`rg -l 'fragment_(frame|tile)'`, about 40 files, mechanical
  `.unwrap()` / `.expect()` in tests).

### Implementation steps

1. Epoch:
   - add `pub const WIRE_EPOCH: u16 = 1;` and
     `pub fn epoch_query() -> String { format!("gf_epoch={WIRE_EPOCH}") }`;
   - client-net: append the query to the CONNECT URL;
   - wasm: `format!("https://{host}/?{}", epoch_query())`;
   - server `connect_response`: parse `req.url.query_pairs()` for `gf_epoch`.
     A missing or different value gets 400 plus a rate-limited warn
     (`refusal_log_due()` already exists), and the connection never becomes
     live.
2. Fragments: move the constants, change the signatures, then fix the callers.
   The production callers are `io_bridge/frame.rs:1730` and `emit.rs`'s tile
   path. Add the `frames_unfragmentable` counter to `h264 stats` (and list it
   in `docs/design/h264-transport.md` §8, since AGENTS.md says every field is
   listed there).
3. Errors: add the variants, convert the `Option` decoders, and update their
   callers (`io_bridge.rs` feedback loop, which WP-6 has already narrowed to
   the reader; client-core's `handle_server_stream`). Add `WireError`.
4. Ping deletion (owner OK first).
5. Endianness doc table.

### Tests

- **Epoch:**
  - `webtransport.rs` unit test: `connect_response` for URLs with no query,
    `gf_epoch=0`, `gf_epoch=WIRE_EPOCH` and `gf_epoch=x` gives 400, 400, 200
    and 400.
  - `client-net/tests/handshake.rs`: a real loopback CONNECT succeeds, which
    proves the native client sends the right epoch.
  - e2e: any Chromium e2e passing after `just build-release` and
    `just containers-build` proves the browser sends it.
  - Mutation: bump `WIRE_EPOCH` in the server only (temporarily edit the
    constant on one side via a test-only override). The client-net loopback
    test fails with `status 400`.
- **Fragments:** a protocol unit test where a payload of
  `(MAX_FRAME_FRAGMENTS as usize + 1) * frag` bytes gives `Err(TooManyFragments { .. })`,
  and exactly `MAX_FRAME_FRAGMENTS * frag` gives `Ok` with
  `frag_total == MAX_FRAME_FRAGMENTS`. A lib unit test of `send_h264_frame`
  with an oversized `H264Out` asserts `frames_unfragmentable == 1`, nothing
  enqueued, and the manager told. Use the existing `h264_refs` test
  scaffolding (`raised_for_test`-style hooks). Mutation: drop the check, and
  the protocol test sees `frag_total` wrap or the `Ok` instead of `Err`.
- **Errors:** compile-level only, plus the existing decode tests updated from
  `assert!(x.is_none())` to `assert!(matches!(x, Err(ProtocolError::..)))`
  with the precise variant. That makes them stronger than today's `None`.

### Gates

This changes the wire (the CONNECT URL), across all four consumers:
- clippy workspace;
- `just ci-local` (cbindgen diff if `ghostframe-lib` gains or loses pub consts);
- `cargo build -p ghostframe-client-wasm --target wasm32-unknown-unknown`;
- **`just build-release`**: the embedded bundle must carry the new URL, or
  every browser session gets a 400. Afterwards confirm with
  `strings target/release/ghostframe-server | grep -c gf_epoch`;
- `just containers-build`, then `just test-e2e`. Native client:
  `just ci-client`.

### Risks / owner decisions

- **Owner:** approve the epoch-in-URL scheme and the bump rule.
- **Owner:** approve deleting ping/pong.
- After the epoch lands, any stale cached browser bundle is refused, which is
  the intended effect. The wasm error text must say "reload".

**Size: M** (epoch S, fragments S, errors M, mostly mechanical).

---

## Suggested PRs for WP

| # | Branch | Findings | Owns (files) | Depends on | Notes |
| --- | --- | --- | --- | --- | --- |
| 1 | `wp/palrle-index-oob` | WP-2 | `client-core/src/pal_rle_decode.rs`, `client-core/tests/{oracle_prevalidate,hostile_input}.rs`, e2e doc comment | — | Independent. Land first: smallest, and gives WP-5 its differential. |
| 2 | `wp/client-net-connect-bounds` | WP-3 | `client-net/src/{handshake.rs, lib.rs}`, `client-net/tests/handshake.rs` | — | Independent. PR 7 later edits `handshake.rs:92` (one line). |
| 3 | `wp/h264-drop-refcontinuity` | WP-4 | `client-h264/src/bitstream.rs` | — | Independent, trivial. |
| 4 | `wp/seq31` | WP-1 | `protocol/src/{seq.rs, protocol.rs (fragment_frame_ref, FrameAck::bit)}`, `client-core/src/{lib.rs, frame_assembly.rs, h264_gate.rs, cdf53_coverage.rs}`, `lib/.../reliable_emitter/{h264.rs, emitter.rs}`, `lib/.../io_bridge/{frame.rs:349,777 only, h264_tx.rs}` | — | **Contention:** `io_bridge/frame.rs` (two one-line edits) and `h264_tx.rs`. Rebase onto whichever io_bridge PR is in flight. Do not batch-merge it with other auto-merges without checking master compiles (rebase-merge semantic-conflict memory). |
| 5 | `wp/feedback-codecs-to-protocol` | WP-6 (codecs + `DecodeErrorCode` + `FeedbackStreamReader`) | `protocol/src/feedback_stream.rs`, lib `transport/{client_caps,decode_error,display,input_inject}.rs`, `io_bridge.rs::dispatch_feedback_bytes` + `feedback_recv_bufs` + `handle_decode_error`, client-core `{event,decode_error_batcher,loss_tracker,input}.rs`, `render/src/tile_renderer.rs:217-230`, `client-wasm/src/app/client.rs:44-58`, `ghostframe.h` | owner OK on relicensing | **Heaviest contention:** about 190 lines of `io_bridge.rs`. Split if needed: 5a codecs + TryFrom (io_bridge imports only), 5b reader + `dispatch_feedback_bytes` rewrite (coordinate a window with the io_bridge owner group). |
| 6 | `wp/fuzz-nightly` | WP-5 | `fuzz/**` (new), delete `client-net/fuzz/`, `.github/workflows/{nightly,client-core}.yml`, `Justfile`, `DEVELOPERS.md`, `cfg_attr(miri, ignore)` in protocol/client-core tests | 1, 5 (5b for `feedback_stream` target) | Owner OK on a nightly toolchain in CI. Workflows may be co-owned by a CI group; nightly.yml gains jobs, existing job untouched. |
| 7 | `wp/protocol-consistency` | WP-7 | `protocol/src/{lib,protocol,feedback,feedback_stream}.rs`, `lib/.../webtransport.rs`, `lib/.../io_bridge/frame.rs (send_h264_frame)`, ping deletion in `io_bridge.rs` + `ack_dispatch.rs`, `client-net/src/handshake.rs`, `client-wasm/src/app/bootstrap.rs`, about 40 test call sites of `fragment_*` | 2, 4, 5 | Wire change, all four consumers, `just build-release` + `containers-build` + e2e. Could split: 7a epoch (S, wire), 7b fragments + Result/WireError (M, mechanical), 7c ping deletion + endianness doc (S). The fuzz targets (6) absorb the `Result` change if 7b lands after 6. Either order works; 6 before 7b is preferred so the fuzzers guard the conversion. |

**Order:** run 1, 2, 3 and 4 in parallel, then 5 (5a, then 5b), then 6, then 7a, 7b, 7c.

**Interaction with other groups:** the `io_bridge.rs` / `io_bridge/*.rs`
hunks in 4, 5b and 7 are the merge-risk surface. Each PR keeps them minimal
and mechanical, so a conflict means a rebase, not a redesign. Two shared
worktrees must not share a target dir for the A/B checks (memory "two
worktrees, one target dir").
