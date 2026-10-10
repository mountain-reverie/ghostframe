# Ghostframe Protocol Comparative Review

**Date:** 2026-10-07 · **Question:** how does the ghostframe wire protocol fare against the state of the art (post-quantum transport security, QUIC-era media transport) and against mainstream remote desktops (VNC, Microsoft RDP, SPICE, PCoIP, Citrix, Parsec, Moonlight/Sunshine, RustDesk, WebRTC-based)?

Method: one pass built an implementation-accurate fact sheet from the code and specs (code wins where docs diverged); two research passes surveyed the 2025–2026 landscape from primary sources (RFCs, vendor protocol docs, engineering blogs, source repos). Companion to the [full project review](2026-10-07-full-project-review.md), which covers implementation-level findings; this report is about protocol *design*.

---

## Verdict in three sentences

Ghostframe is a **PCoIP/RDP-mixed-mode architecture** (per-tile classification, integer-wavelet build-to-lossless, H.264 for motion) running on a **game-streaming-era transport** (UDP-family datagrams, delay-based congestion estimation, parity FEC) delivered through the **2026 browser story** (WebTransport — Baseline across all three engines since Safari 26.4). Every element has shipped precedent and the overall shape matches where the industry's text-dominant branch has converged; the specific combination — datagram-level per-tile ACK with progressive wavelet refinement over QUIC — has no production peer, with RDP's RFX_PROGRESSIVE-over-RDPEUDP2 the nearest shipping relative. The three genuinely dated elements are the crypto provider (ring cannot offer post-quantum key exchange at all), the blind nested congestion-control stack (app BWE on top of quinn's Cubic), and the fixed XOR k=10 FEC (burst-blind, mis-tuned for short-RTT tailnet paths).

---

## 1. What ghostframe's protocol actually is (implementation-accurate)

- **Stack:** Tailscale WireGuard (userspace tsnet, zero open ports) → QUIC (quinn-proto 0.11, sans-IO, rustls 0.23 + ring, TLS 1.3) → WebTransport → ghostframe. All video/ACK/NACK/parity on unreliable datagrams; one reliable bidi stream for control (HELLO, input, display negotiation, decode-error feedback).
- **Certs:** ephemeral self-signed, 13-day validity, pinned via `serverCertificateHashes` (SHA-256 of DER); hash delivered over the tailnet-served `/config.json`. No CA, no auth code in ghostframe — identity is Tailscale's.
- **Codecs:** 32×32 tiles, per-tile classifier (change frequency × magnitude) choosing Skip / Solid (4 B) / PalRLE (≤16 colors, shared palette table) / CDF 5/3 (3-level integer wavelet, 14 bit-plane passes, SPIHT-style midpoint partial decode, lossless at K=14) / full-frame H.264 (VA-API, GOP 11). Frame-level mode decision by a bench-tuned cost model with hard overrides (headroom < 0.25 B/µs or smoothed loss > 10% → H.264). Idle tiles escalate from lossy H.264 snapshots to CDF53 refinement → **PixelPerfect** (byte-exact, oracle-asserted).
- **Reliability:** ACK rev3 acknowledges *transmissions* (6-byte entries: wire_seq + arrival-time lo16), batched 64+8-overlap, 5 ms flush. Receiver-driven NACKs are ground truth (89-for-89 under 10% injected loss); RTO (p95×2, 150 ms–2 s first deadline, ×2 backoff to 5 s) is the documented last resort. Generation bump cancels all queued/cached passes for a changed tile — supersession, not replay.
- **Congestion:** goog_cc (libwebrtc port) fed by the protocol's own ACK timing (delay gradient + ledger-derived loss + real bytes-in-flight), ALR + repeated initial probing; combined by min with a quinn-path-stats AIMD budget; pacer-rate consumed at 2.5× target.
- **FEC:** XOR single parity, K=10 interleaved for tiles (always on), K=2 keyframe / K=5 P-frame for H.264. Recovered losses still reported to the estimator.
- **Measured:** static scene 0.04 Mbps; mixed scene 2.9 Mbps at 1080p30; capture→last-send 28–31 ms; policy locks to full-frame H.264 at ≤10 Mbps links; refinement needs roughly ≥5 Mbps and low loss to converge.
- **Deliberately absent:** audio, clipboard, file transfer, multi-monitor, auto-reconnect, multi-user, non-tailnet operation.

---

## 2. Head-to-head

| | Transport | Codec strategy | Loss handling | Congestion control | Text fidelity | Status 2026 |
|---|---|---|---|---|---|---|
| **Ghostframe** | QUIC datagrams / WebTransport | per-tile classified + wavelet refinement + full-frame H.264 mode | NACK-first ARQ + RTO backstop + XOR FEC | app-level goog_cc ⊓ quinn Cubic | lossless by construction (PixelPerfect) | pre-production |
| **VNC/RFB** | TCP, pull model | per-rect intra-only (Tight/ZRLE) | TCP | TCP (TigerVNC adds app-level Vegas) | lossless configs exist | stagnant; KasmVNC evolving into a video pipeline |
| **RDP (AVD/W365)** | TCP + RDPEUDP2 (reliable-only; FEC dropped) + Multipath (GA 2025-26) | classify → ClearCodec text / RFX_PROGRESSIVE / AVC444(v2) | ARQ with ACK vectors + receiver timestamps | delay-aware (URCP-class) | AVC444 full-chroma; lossless text codec | the benchmark incumbent |
| **SPICE** | TCP | draw commands + rate-based video detection | TCP | none | lossless default | effectively EOL (removed from RHEL 9) |
| **PCoIP / HP Anyware** | UDP | text/image classifier, wavelet build-to-lossless, H.264 offload under motion | proprietary ARQ | proprietary adaptive | numerically lossless — its brand | niche (VFX/medical) |
| **Citrix HDX** | TCP/UDP (EDT) | selective: lossless text overlay + H.264/HEVC/AV1 transient regions | ARQ | adaptive | build-to-lossless | mainstream |
| **Moonlight/Sunshine** | ENet control + RTP-ish UDP video | full-frame H.264/HEVC/AV1 always | **RS-FEC 20% + reference-frame invalidation; never retransmits video** | none (user bitrate slider) | brute force (4:4:4 experimental) | healthy OSS |
| **Parsec** | BUD/UDP + DTLS | full-frame video | proactive CC, no published FEC | custom "saturate to the edge" | 4:4:4 paid tier | best-in-class latency claims (LAN ~7 ms added) |
| **WebRTC class (CRD, xCloud, Selkies)** | ICE/DTLS-SRTP | full-frame VP9/AV1 (SCC tools) | NACK/RTX low-RTT, FEC stalled at flexfec-03 | goog_cc / transport-cc | 4:2:0 text blur is the chronic complaint | mature; jitter buffer is the hidden 50–125 ms |
| **RustDesk** | TCP / KCP-UDP / WebRTC-SCTP (2026) | full-frame VP9/AV1, i444 option, no lossless | reliable-stream ARQ only | TestDelay probes → halve bitrate | 4:4:4 option | fast-moving OSS |
| **MoQ (Cloudflare relay, 2025)** | QUIC **streams** per subgroup | n/a (transport) | RESET_STREAM cancellation | QUIC-native | n/a | pre-RFC, production relay network exists |

### The most interesting mirror: RDP's RFX_PROGRESSIVE

Microsoft ships, today, in AVD mixed mode, a codec that is architecturally ghostframe's CDF53 path: **LeGall 5/3 integer DWT, 64×64 tiles, bit-plane-truncation quantization, first-pass + upgrade-pass tiles, refine-to-lossless on static content**, with the LL band delta-coded against the previous frame. Ghostframe independently converged on the same wavelet, same progressive-refinement model, same build-to-lossless endgame — with three deltas worth knowing:

1. **Entropy coding.** RFX uses RLGR1 on the first pass and SRL + raw-bit refinement on upgrades; ghostframe sends RLE'd bit-planes (plus an unwired LZ4 flag). RFX's refinement coding is tighter per byte; ghostframe's is simpler and GPU-friendlier. Worth a bench if CDF53 bytes-per-lossless ever becomes the constraint (current numbers: 174 B solid → 4.4 KB photo per tile).
2. **LL delta-coding.** RFX delta-codes the LL band across generations; ghostframe re-sends the full coefficient set each generation bump. For slowly-mutating tiles (blinking cursor in a text editor) a generation-over-generation coefficient delta is a real byte saver — the per-tile generation machinery to anchor it already exists.
3. **Acknowledgment granularity.** RDP acks at frame granularity (`RDPGFX_FRAME_ACKNOWLEDGE` + queueDepth); ghostframe acks per datagram-transmission. Ghostframe's is strictly finer — this is a genuine advance over the nearest shipping relative, and the recorded production evidence (degradation caught in 600 ms, Karn-filter sample loss eliminated) supports it.

### Where the industry split lands

There is no single consensus — two stable equilibria, split by content class:

- **Game streaming (motion-dominant): video-codec-everything won.** All of Moonlight, Parsec, Steam, GFN, xCloud push every pixel through one hardware encoder, never retransmit video, and repair via FEC + reference-frame invalidation. Text is brute-forced with 4:4:4 or bitrate.
- **Desktop remoting (text-dominant): classification persists everywhere it matters.** Microsoft's own docs put ~80% of session graphics as text and keep classify-then-route as the default software path; Citrix keeps a lossless text overlay even inside full-screen-codec mode; PCoIP classifies per region. The chronic failure that keeps classification alive is exactly the one ghostframe designed against: 4:2:0 video destroys text chroma, and desktop content is mostly static.
- **The hedge:** AV1 screen-content tools (palette mode + intra-block-copy in the *base* spec) effectively move the classifier inside the codec — Meet measured 25%+ screen-share savings, Jitsi picked AV1 as the best screen-share codec. But hardware AV1 encoders don't document SCC tool support as of 2026, so the collapse hasn't happened.

Ghostframe sits squarely and correctly in the second camp for its stated workload. Its differentiator over RDP/Citrix is the *endgame*: provable byte-exactness (PixelPerfect with an oracle) rather than "visually lossless," plus a finer-grained transport. Its differentiator over PCoIP is being open, GPU-compute-based, and browser-reachable.

### Transport: who else uses QUIC datagrams?

Nobody ships production interactive desktop video on raw QUIC datagrams today. RDP went multipath-UDP (and has **no** QUIC transport shipped or announced despite rumors); RustDesk evaluated QUIC and dropped it for KCP, then added WebRTC/SCTP; the MoQ world — the one production QUIC media deployment, via Cloudflare's 330-city relay network — argues for prioritized/resettable **streams** over datagrams ("Never Use Datagrams," Curley 2024), *except* for small self-contained loss-tolerant units with their own recovery. A 32×32 tile pass with per-transmission ACK and NACK recovery is precisely that carve-out, so ghostframe's choice is defensible — but see the MoQ watch item in §4.6: subgroup streams with RESET_STREAM cancellation now deliver most of what the custom datagram layer hand-builds (supersession = reset the stream; priority = transport-level) and will have RFC status within a year.

Also notable: **RDPEUDP2 dropped FEC entirely** in favor of better ARQ + receiver-timestamped delay signals; WebRTC practice prefers RTX over FEC at low RTT; RustDesk is ARQ-only. At tailnet RTTs (5–40 ms direct paths) the industry weight is behind retransmission — which ghostframe's NACK-first design already embodies. The FEC layer is the add-on to re-examine, not the ARQ (§4.3).

---

## 3. Where ghostframe is ahead of or at the state of the art

1. **Transmission-identity ACKs.** Acknowledging the *transmission* (wire_seq) rather than the content is what RDPEUDP2's ACK-vector+timestamp design gestures at and what content-keyed schemes (including ghostframe's own rev1/rev2) provably get wrong. The documented production win — goog_cc was under-seeing loss 4× before the ledger — is the right kind of evidence.
2. **Receiver-driven NACK as primary, timer as last resort.** The RTO analysis (1,788 spurious timer retransmits on a lossless link vs NACKs going 89-for-89) matches where RDP, RustDesk, and WebRTC all ended up. Keeping the RTO only for the "single-datagram tile lost entirely, then static" case is defensible and documented.
3. **Supersession-first scheduling.** Generation bump cancelling queued/cached/in-flight passes, small send buffers, and the one-datagram budget floor ("waiting must happen where cancellation can reach it") is the same insight MoQ encodes as RESET_STREAM and game streamers encode as "never retransmit video" — arrived at independently with the stronger property that *static* content still converges to lossless.
4. **Provable losslessness.** No mainstream competitor asserts byte-exactness with a CPU-vs-GPU oracle; PCoIP markets "numerically lossless," RDP/Citrix do build-to-lossless without the proof obligation. For the stated niche (operator's own desktop, text-heavy) this is the differentiating feature.
5. **The security envelope.** Zero open ports, no auth code (delegated to tailnet SSO/ACLs), ephemeral in-memory keys, 13-day pinned certs. The cert model is the textbook-correct `serverCertificateHashes` instantiation, and WebPKI itself is converging on the same freshness-over-revocation logic (CA/B ballot SC-081: 200-day certs now, 47-day by 2029). Delivering the pin over the authenticated tailnet converts pinning's classic weakness (distribution) into its strongest link.
6. **WebTransport timing.** Baseline across Chrome/Firefox/Safari since Safari 26.4 (March 2026). The bet has matured. (Mind the Firefox ≥125 floor — earlier versions mishandled `serverCertificateHashes`.)
7. **Honest adaptation envelope.** The M3.6/M3.7 mode policy (cost model + hard overrides, lock to H.264 under 10 Mbps or 10% loss) with parametric-sweep validation is more empirical grounding than most open-source competitors have (Moonlight has a bitrate slider; RustDesk halves bitrate on probe delay).

---

## 4. Where it's behind — actionable improvements

Ordered by (impact on the protocol's goals) × (cheapness), since nothing is in production yet.

### 4.1 Post-quantum key exchange: currently impossible with the ring provider — swap it (small, do soon)

The direct answer to the Kyber question: CRYSTALS-Kyber was finalized as **ML-KEM (FIPS 203, Aug 2024)**; the hybrid TLS group **X25519MLKEM768** is now a published standard (RFC 10024), default-on in Chrome since 131 and Firefox since 132, and carries >⅔ of human TLS traffic at Cloudflare. **Ghostframe negotiates none of it:** the rustls `ring` provider implements no PQ primitives and none are planned, so the handshake silently falls back to classical X25519 even against browsers that offer the hybrid. The fix is a provider swap, not a redesign: rustls ≥ 0.23.22 ships X25519MLKEM768 under the **aws-lc-rs** provider (`prefer-post-quantum` feature); quinn-proto takes an injected `CryptoProvider`, so no QUIC-layer change. Cost: ~1.2 KB larger ClientHello — irrelevant against video-rate traffic.

Honest threat framing: harvest-now-decrypt-later is the only PQ threat on this timeline, and screen pixels are among the least HNDL-sensitive payloads imaginable — but **keystrokes and (future) clipboard on the feedback stream are plausible long-lived secrets**, and the swap is nearly free. The Tailscale underlay is classical too (WireGuard's PQ hedge is a PSK Tailscale doesn't expose; their docs say PQ "eventually") — treat it as defense-in-depth that picks up PQ when Tailscale ships it. Signatures need nothing: the browser mandates ECDSA P-256 ≤14-day certs for pinning, and the out-of-band hash model sidesteps the entire ML-DSA/certificate migration.

### 4.2 Nested congestion control: goog_cc on top of quinn's Cubic is the textbook warned-against configuration (medium, highest protocol-correctness value)

Every datagram still traverses quinn's **Cubic** (quinn's BBR is an experimental v1 port). The RTP-over-QUIC literature (draft-ietf-avtcore-rtp-over-quic; Engelbart & Ott, EPIQ '21) names the failure modes, all applicable here: (a) Cubic fills the bottleneck queue until drop, inflating exactly the one-way-delay gradient the app BWE reads — the app backs off first and its estimate converges to "whatever queue Cubic maintains"; (b) at cwnd exhaustion quinn queues/drops datagrams invisibly, so submit timestamps diverge from wire departure, corrupting the delay signal; (c) padding probes capped by cwnd measure Cubic, not the path. The existing min(BWE, quinn-AIMD) combiner and the 0.8×send-buffer clamp already mitigate steady state; probing, loss episodes, and slow-start-after-idle are where the loops fight.

Fixes in ascending effort, all sanctioned: (1) **expose quinn CC state to the BWE** — stamp emit time at actual wire departure (pacer release, not submit), discard cwnd-clipped probe samples, treat cwnd exhaustion as a hard signal; (2) **install a custom controller** via the public `TransportConfig::congestion_controller_factory` trait — a high-floor/near-no-op controller makes the app BWE the single authority (RFC-legal; effectively what production game-streaming stacks over UDP do); (3) longer-term, read **ECN counts** from quinn's ACK data and adopt an L4S-capable controller (SCReAM v2 is the only specified one) — L4S is really deploying now (Comcast low-latency DOCSIS in production, Apple client-side since iOS 17, GeForce NOW shipping it), and it degrades gracefully to the delay-based path elsewhere.

### 4.3 FEC: fixed XOR K=10 is the weakest element — make it adaptive or drop it to near-zero on clean paths (medium)

Single parity recovers exactly one loss per group: at 1% random loss ~10% of lost packets are unrecoverable anyway; at 5% it's ~37%; and **burst loss — the dominant Wi-Fi/last-mile pattern on a tailnet path — defeats adjacent-group parity outright** (the stated motivation for FlexFEC's interleaved mode). Meanwhile the 10% overhead is paid permanently, including on the clean short-RTT paths where the protocol's own NACK recovers in ~1 frame-time and the industry verdict (RDPEUDP2 dropping FEC, WebRTC RTX-over-FEC at low RTT, RustDesk ARQ-only) says retransmission wins. Upgrades, all precedented: (a) **adapt the FEC rate from the BWE's measured loss + RTT** (libwebrtc's protection-factor table; QUIRL's latency-budget gate) — zero overhead on clean paths, ramp on lossy/long ones; (b) for burst tolerance at the same math, **interleaved/column XOR** (FlexFEC-style) or small-group **Reed-Solomon** via SIMD (K=10, M=2 recovers any double loss; cost trivial at these rates — Sunshine ships 20% RS per frame); (c) skip RaptorQ — wrong tool at 10-datagram groups, plus lingering IPR nuisance. Supporting evidence specific to this exact architecture: Eghbal & Lu 2025 (*Future Internet* 17:297) measured QUIC + partial ordering + FEC-with-ARQ-backstop cutting p99 screen-update blocking 230 ms → 0.4 ms at 0.5% loss / 100 ms RTT.

### 4.4 H.264 mode: three ideas from the game-streaming camp (small-medium)

- **GOP 11 (~6 IDR/s) is costly.** The original spec's instinct (infinite GOP + intra-refresh) matches what every game streamer ships. Better still, Moonlight/Sunshine's **reference-frame invalidation / LTR**: on loss, tell the encoder to reference the last known-good frame rather than sending an IDR. The NACK machinery already knows exactly which frame failed; VA-API exposes reference-picture control. This converts keyframe bursts (the moment most likely to overflow a constrained link) into ordinary P-frames.
- **4:2:0 chroma on text.** Ghostframe's H.264 mode has the same text-blur failure as everyone else's; the idle-escalation path heals it *after the fact*, which is the right architecture — but during sustained motion on a constrained link (the lock-to-H264 regime), text is blurry with no relief. RDP's answer (AVC444: pack the discarded chroma into a second 4:2:0 frame through the same hardware encoder) is clever and patent-documented; a cheaper ghostframe-native variant is **chroma catch-up via the existing tile path** — PalRLE/CDF53 refinement over text regions while H.264 carries motion, which is literally Citrix's "lossless text overlay in full-screen-codec mode."
- **AV1 watch item.** AV1 SCC (palette + intra-block-copy, base spec) is the one plausible future where a single codec collapses the classifier; Citrix already ships AV1, Steam defaults to it, browsers decode it. Blocked today by hardware encoders not implementing SCC tools. The tile architecture loses nothing by waiting; just keep the codec enum open (it is — u8 with explicit discriminants).

### 4.5 Operational gaps against the mainstream (known, but now ranked)

For the stated single-operator niche, the absence list is mostly principled, but three items sit on the critical path to "usable daily driver," in order: **(1) audio** — every competitor has it; the spec's Opus plan is sound; note WebTransport datagrams + 20 ms Opus frames with in-payload FEC is exactly the "datagram-appropriate" case even MoQ concedes; **(2) clipboard** — the highest-value/lowest-cost feature in the category, and it raises the HNDL stakes of §4.1; **(3) reconnect** — the deliberate no-auto-reconnect stance is defensible against eviction races, but a single "session resume within N seconds with cache intact" path would remove the most user-visible roughness (page reload = full first-paint burst). Multi-monitor and file transfer are legitimately later.

### 4.6 Transport watch items (zero code today)

- **MoQ:** draft-ietf-moq-transport is at -22 heading to IESG; Cloudflare runs a production relay network. When it reaches RFC + quinn-ecosystem tooling, re-evaluate whether **refinement passes** (not the critical path) move to subgroup streams: RESET_STREAM gives supersession for free, transport priorities replace the two-tier scheduler split, and relays become a path to multi-viewer if that ever stops being a non-goal. The critical-tier datagram path can stay — it is the legitimate datagram niche.
- **No UDP fallback exists.** WebTransport's HTTP/2 fallback is implemented by zero browsers; UDP-hostile networks are simply unsupported. On a tailnet this is mostly moot (WireGuard is UDP anyway — if UDP is blocked, Tailscale itself degrades to DERP-relayed TCP… which still carries the QUIC datagrams, at a latency cost). Document this chain explicitly; it's a support-ticket generator, not a design flaw.
- **Cert rotation:** the 13-day cert currently guarantees a hard edge at expiry. Adopt the libp2p pattern before production: pre-generate the successor, advertise **both hashes** (the API takes an array), backdate notBefore ~1 h for clock skew, re-fetch the hash per session and never cache it across days.

---

## 5. What to validate before production (protocol-level test additions)

1. **Burst-loss model.** All loss-injection testing is i.i.d.-probability based (`GHOSTFRAME_*_LOSS_PROBABILITY`). Add a Gilbert-Elliott (burst) model to the injection harness — it is the model under which XOR K=10 fails and under which the §4.3 decision should be made with data.
2. **Nested-CC fight reproduction.** A shaped-link scene that forces slow-start-after-idle and probe-during-Cubic-growth, asserting BWE convergence time — this is the regime §4.2 predicts oscillation in; measure before and after exposing wire-departure timestamps.
3. **Competitive latency baseline.** The bench measures ghostframe against itself. One afternoon with TigerVNC, KasmVNC, Sunshine/Moonlight, and RustDesk on the same host/scene/link matrix (photon-to-photon via the existing capture oracle) would turn this report's qualitative table into marketing-grade numbers — and catch regressions against the field, not just against yesterday.
4. **Text-under-motion fidelity.** A scrolling-text-at-constrained-bandwidth scene scoring OCR-ability or per-glyph SSIM during H.264-locked mode would quantify §4.4's chroma gap and prove (or kill) the chroma catch-up idea.

---

## 6. Priority summary

| # | Action | Size | Why now |
|---|---|---|---|
| 1 | Swap rustls provider ring → aws-lc-rs, `prefer-post-quantum` (X25519MLKEM768) | S | Only blocker to PQ KEX; browsers already offer it; protects future input/clipboard traffic against HNDL |
| 2 | Dual-hash cert rotation (libp2p pattern) + per-session hash fetch | S | Removes a guaranteed 13-day outage edge before it ships |
| 3 | Adaptive FEC rate off measured loss/RTT; interleaved or RS parity for bursts | M | 10% permanent overhead buys ~nothing on clean paths and fails on bursty ones |
| 4 | Expose quinn CC state to the BWE (wire-departure stamps, cwnd-clipped probe discard); then consider custom controller via the public trait | M | Textbook nested-CC hazard; fixes the signal the whole adaptation stack depends on |
| 5 | H.264: drop GOP-11 for intra-refresh or NACK-driven reference invalidation | M | Converts keyframe bursts to P-frames exactly when the link is tightest |
| 6 | Burst-loss injection model + nested-CC and text-under-motion bench scenes | M | The three §4 decisions above should be closed with data, and the harness is 90% there |
| 7 | Audio (Opus over datagrams, per spec), then clipboard, then session-resume | L | The three features between "demo" and "daily driver"; everything else can wait |
| 8 | Watch: MoQ for refinement-tier streams; AV1 SCC hardware encoders; Tailscale PQ PSK; CDF53 LL delta-coding and RFX-style refinement entropy coding if bytes-to-lossless becomes the constraint | — | Re-check ~every 6 months; none are blockers today |

---

## Addendum (2026-10-08): Kyber — the VideoLAN streaming protocol

The original review read "Kyber" as the post-quantum cipher (§4.1). There is also **Kyber the protocol**: Jean-Baptiste Kempf's (VideoLAN/VLC) ultra-low-latency streaming kit, announced mid-2025 — QUIC-based, FFmpeg server + VLC client, full-frame H.264/HEVC/VP9/AV1, bidirectional input streaming, **RaptorQ FEC instead of retransmission**, claiming 8 ms glass-to-glass on controlled links (20–25 ms figures in the SVTA talk), targeting cloud gaming / drones / robotics / remote desktop. AGPL + commercial dual license, not browser-reachable (native VLC-derived client; no WebTransport story published).

Placement in the §2 landscape: Kyber belongs to the **game-streaming equilibrium** (video-codec-everything, never retransmit, repair by FEC), executed on QUIC. It is the first serious interactive-video-on-QUIC shipping effort and validates the transport bet from the native-client side, as WebTransport-Baseline does from the browser side. It does not change the §2 conclusion for the text-dominant desktop workload: full-frame 4:2:0 video with latency-over-quality priorities has no lossless story and no text-fidelity answer — Kyber optimizes exactly the axis ghostframe trades away, and vice versa.

**Why RaptorQ is right for Kyber and still wrong for ghostframe** (expands §4.3): Kyber's repair unit is a *video frame* — generated atomically by the encoder, naturally a large FEC block (tens to hundreds of packets; more for keyframes). At K in the hundreds, RaptorQ's ~2-symbol reception overhead is ~1%, decode is near-linear, and ratelessness lets the sender mint extra repair symbols on the fly without re-planning an (n,k) code — and the no-feedback repair path suits drone/cellular links where an RTT is unaffordable or feedback is unreliable. Ghostframe's repair unit is a ~10-datagram tile-pass group that *accumulates* across the pacing budget rather than arriving as a burst: at K=10 RaptorQ's overhead floor is a 20% penalty for a probabilistic guarantee where Reed-Solomon is deterministic MDS at the same overhead, its precode setup cost is amortized over nothing, and enlarging blocks to fix that means buffering delay — paying latency to justify the code. Add the feedback asymmetry (ghostframe's NACK path is ground truth at 5–40 ms tailnet RTTs; Kyber deliberately has no retransmission) and supersession (much of what FEC would protect is voided by the next generation), and the §4.3 recommendation stands: adaptive-rate interleaved XOR or small-group RS. The one place the Kyber pattern maps onto ghostframe is the **full-frame H.264 path**, where keyframes genuinely are large blocks (50–100+ datagrams at 1080p): upgrade that path's K=2 XOR to per-frame RS (Sunshine ships exactly this at 20%); RaptorQ becomes arguable there only if single frames ever exceed RS's practical shard limits (≈255/block).

**Bottom line:** nothing in the comparison says the architecture is wrong — the industry's text-dominant branch (RDP mixed mode, PCoIP, Citrix) validates per-tile classification + wavelet build-to-lossless, the game-streaming branch validates the transport instincts (supersession, never-block-on-stale, receiver-driven repair), and ghostframe's transmission-identity ACK layer is genuinely ahead of its nearest shipping relative. The gaps are a crypto-provider swap, two tuning-layer modernizations (FEC, CC coupling) that the industry has published answers for, and the feature trio every mainstream competitor treats as table stakes.
