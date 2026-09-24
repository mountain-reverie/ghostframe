# Native client M4a: single-client sessions — design

**Status:** approved for planning
**Date:** 2026-09-24
**Predecessors:** [M1](2026-09-22-native-client-design.md), [M2](2026-09-23-native-client-m2-design.md), [M3](2026-09-23-native-client-m3-design.md)
**Successor:** M4b (display capability negotiation and virtual EDID)

M3 finished the codec path: the native client decodes H.264 on VA-API hardware
and renders it zero-copy. M4a fixes something underneath it that M3 made
visible — the server has no notion of *which* client it is serving.

---

## 1. Scope

**In:** enforce exactly one attached client. A second client displaces the
first, which is told why. Plus two deferred VA-API items that gate nothing and
should not wait for a milestone of their own.

**Out:** display capability negotiation and EDID synthesis. That is M4b, and it
depends on this milestone's invariant so it never has to answer "whose screen?".

### 1.1 Why this is worth doing on its own

`io_bridge.rs` holds `wt_sessions: HashMap<ConnectionHandle, WebTransportServer>`
— several clients can attach at once — but the state derived from them is
singular. `adaptation_context.supports_h264` is assigned from whichever HELLO
arrived last (`io_bridge.rs:6275`), so two clients with different capabilities
already interfere: an H.264-capable client attaching after a tile-only one
flips the whole session into H.264 mode, and the tile-only client gets frames
it cannot decode.

That is a real defect today, independent of anything M4b wants. Making
single-client an invariant fixes it by construction rather than by adding
per-client fan-out, which is a much larger change and buys nothing anybody has
asked for.

---

## 2. Eviction fires on HELLO, not on session accept

A connection that never identifies itself must not displace a working session.
That covers a port scan, an abandoned handshake, a half-open connection, and a
client that dies mid-negotiation.

HELLO is also where capabilities arrive, so it is the first moment the server
knows a real client is present — and, for M4b, the message that will carry the
client's display modes. Putting the eviction decision there keeps both on the
same trigger.

**Consequence to accept:** between accept and HELLO, two sessions exist. That
window is bounded by the client's own handshake and nothing is served into it
— a session that has not said HELLO has not been sent frames — so the
singular-state defect §1.1 describes cannot occur during it.

---

## 3. The eviction message

A new server→client control datagram carrying a reason code.

**Why a datagram rather than a reliable stream.** Both clients already parse
inbound datagrams; neither has application-stream plumbing, and adding it would
be new machinery in the server and twice over in the clients. The failure mode
of a lost eviction datagram is that the client shows a generic disconnect
instead of a specific one — degraded, not broken, and not a hang.

**Mitigation, because it is lossy and this one matters.** The server sends the
message **three times**, then waits **one frame interval (~16 ms)** before
closing the session, so a single loss does not swallow it and the client has a
scheduling slot in which to process it.

Both numbers are guesses, not measurements, and should be labelled as such
where they are written. Three repeats survives two independent losses at the
~1% rates this project tests under; 16 ms is one frame at 60 Hz, chosen because
the client's event loop already turns over at that cadence. If either proves
insufficient the upgrade is a reliable stream, and the reason code carries over
unchanged.

**Not WebTransport's own close capsule.** `CLOSE_WEBTRANSPORT_SESSION` carries
an error code *and* a UTF-8 reason, which would be the natural home for this —
but this codebase's WebTransport implementation has no capsule framing
(`webtransport.rs:235` still carries a "Task 8 will use capsule framing here"
note). Implementing capsule framing to deliver one message is a bigger change
than the message, and it would be reached through the browser's
`WebTransport.closed` rather than the datagram path both clients already have.
Worth revisiting if capsule framing ever lands for another reason.

**Reason code, not free text.** The clients render their own wording. A code
keeps the wire small, keeps translation on the client side, and — the reason
that actually matters — lets tests assert on the cause rather than on a string
that will be reworded.

---

## 4. Client behaviour

**Web:** an overlay stating another session took over. **No reconnect button.**
Reloading is the reconnect, and a button invites two clients racing to evict
each other — a loop the user would experience as both windows flickering.

**Native:** log the reason and exit cleanly, **exit code 0**. Being displaced is
an expected outcome, not a failure; a non-zero code would make an ordinary
hand-off look like a crash to any supervisor or script wrapping the CLI.

**Neither client retries automatically.** Two clients with reconnect logic
evict each other indefinitely.

---

## 5. Testing

- **Server:** two sessions attach; assert the first is evicted and the second
  survives. Assert the evicted session receives the reason code.
- **Server, negative:** a connection that accepts but never sends HELLO evicts
  nobody. This is the guard §2 exists for, and it is the one a future
  refactor is most likely to break.
- **Native:** the clean-exit path — reason logged, exit code 0.
- **Web:** the overlay appears, asserted on the reason code arriving rather
  than on the rendered wording.

The negative test matters more than the positive one. "A second client evicts
the first" is the behaviour someone would notice immediately if it broke;
"an unidentified connection evicts nobody" is the one that would rot silently.

---

## 6. The two VA-API items

Neither relates to eviction. They are here because they are small, they were
deferred from M3 with reasons recorded, and they should not wait for a
milestone of their own.

**`AV_CODEC_FLAG_LOW_DELAY`.** Set it on the codec context before
`avcodec_open2` in `H264Decoder::with_device`. Without it, a stream whose SPS
carries a non-zero `max_num_reorder_frames` makes the decoder hold every frame
for one frame-time before emitting it — invisible to a test that counts frames,
and a full frame of added latency to someone watching. The server encodes with
`tune=zerolatency` and no B-frames, so nothing is given up by setting it.

**CI cannot see VA-API decode break.** M3's decoder and oracle tests gate on
`vainfo` reporting an H.264 VLD entrypoint, and CI runners have neither
`libva-utils` nor a VA-API device, so all of them skip. Nothing in CI fails if
hardware decode regresses.

**Decision: record it explicitly in the workflow; do not fake a device.** The
alternative — installing a software VA-API driver so the tests execute against
*something* — would have them verify a different implementation than the one
they exist to check, which is precisely the "we test a model of the shader, not
the shader" failure this whole native-client effort was started to escape. A
green CI run asserting a software decoder's behaviour is worse than an honest
skip, because it reads as coverage.

So: state in `e2e.yml`, where a reader of the workflow will find it, that the
VA-API oracles are developer-machine-only and that CI cannot catch a hardware
decode regression. Then confirm the parts that *can* run in CI actually do —
the descriptor tests are pure logic and the shader-validation walk needs no
GPU, and both should be demonstrably executing rather than assumed to.

---

## 7. Risks

1. **The eviction datagram is lost and the user sees a generic disconnect.**
   Mitigated by sending three times with a grace period; the fallback is a
   reliable stream.
2. **A reconnect loop** if either client ever gains automatic retry. Guarded by
   §4 and by the native client's exit code being 0, which makes "restart it"
   an explicit human act rather than a supervisor's default.
3. **The no-HELLO guard regresses in a refactor**, and an unidentified
   connection starts evicting real sessions. This is what §5's negative test
   is for.
4. **`AV_CODEC_FLAG_LOW_DELAY` changes decode behaviour on streams that do
   reorder.** The server does not produce them today, but a future encoder
   change could. The flag's effect is to refuse to buffer, so such a stream
   would decode out of order rather than late — worth a note where the flag is
   set.
