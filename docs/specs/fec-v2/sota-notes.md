# FEC and loss-adaptive retransmission: state of the art (research notes, 2026-10-15)

Context: tiles and H.264 frames sent as QUIC DATAGRAMs (quinn-proto). About 10 small (~120 B) datagrams are coalesced per QUIC packet. Congestion control is a goog_cc-style delay BWE over Cubic. The client is browser wasm.

Legend: **[D]** = documented in the cited source (spec, source code, paper). **[I]** = my inference or synthesis, not stated by the source. Where I could not find something, I say so.

---

## 1a. Kyber, revised from its source code (2026-10-15)

The web-based notes in section 1 below are superseded where they conflict. Code reading, with file:line citations and commit hashes: `fec-design.md` §2.1.

Summary:
- Static RaptorQ FEC, 30 % with a minimum of 2 repair symbols per frame; each symbol is one full datagram.
- No loss estimator. RTT and lost packets come from quinn and are only displayed.
- The default mode is a reliable QUIC stream, so retransmission is quinn's generic loss recovery.
- Bitrate is set by hand.
- Keyframes come from intra-refresh, plus an IDR on subscribe or on fan-out lag.
- No Kyber code was copied.

## 1. "Kyber" (the QUIC remote desktop)

**Identification [D].** Kyber is the real-time video and controls streaming SDK from Jean-Baptiste Kempf (VideoLAN/VLC president, ex-CTO of the cloud-gaming company Shadow). The site is kyber.media, which now 301-redirects to kyber.tech. It is written in Rust and built on FFmpeg (server) and libVLC (player). It is "Open source. AGPL licensed", dual-licensed with a commercial option, and described as "open core".
- FRnOG 39 slides: https://media.frnog.org/FRnOG_39/FRnOG_39-6.pdf
- Akademy 2023 slides: https://conf.kde.org/event/5/contributions/147/attachments/76/92/Akademy2023_Kempf.pdf
- Site: https://kyber.tech/
- Kempf's site: https://jbkempf.com/
- SVTA abstract: https://university.svta.org/conference-proceedin/kyber-a-new-approach-for-real-time-video-and-controls-streaming-based-on-quic/
- Demuxed 2024 talk: https://www.youtube.com/watch?v=0RvosCplkCc
- FOSSASIA 2026 talk: https://www.youtube.com/watch?v=nqVzOMebNx8

There is no ambiguity about which product this is. "Kyber" otherwise means CRYSTALS-Kyber (ML-KEM), which is unrelated.

**Transport, as publicly documented [D] (FRnOG 39 slides, "Protocol" and "Unreliable Protocol + FEC" pages):**
- **Stack:** QUIC and WebTransport, one socket and one port (TCP+UDP), TLS at connection setup. The multiplexer is "written in Rust from scratch". Audio and video "can use Datagrams". Inputs are always reliable bidirectional streams. The web client runs the same Rust code in wasm, uses WebTransport datagrams, decodes with WebCodecs, and runs "FEC running in Wasm".
- **Four selectable modes:**
  - `Reliable`: each channel on one QUIC/WT stream.
  - `GOPstream`: one QUIC/WT stream per GOP.
  - `Unreliable`: video in datagrams; config and control on reliable streams.
  - `Unreliable_fec`: "Use of FEC (RaptorQ) to recover info without needing retransmissions when loss of packets".
- **FEC:** "RaptorQ (for now) but other schemes are possible". The slides say "RaptorQ source symbols of size configured to maximize the max Quic Datagram size", and "Hopefully, the RaptorQ encoded symbols are going on their own UDP datagrams". The diagram shows per-video-packet (per access unit) source blocks, each split into N datagrams plus repair datagrams. Repair symbols for an older block (P0D2) can be interleaved after a later block.
- **Channel model:** channels "similar to MoQ tracks". A channel is either datagram-mode or stream-mode. Packets are grouped so that SPS/PPS and frame data stay together.
- **Congestion / adaptation:** Gigazine paraphrases that Kyber "immediately send[s] feedback to the encoder to lower the bitrate" on jitter or fluctuation, citing Kempf interviews: https://gigazine.net/gsc_news/en/20250804-vlc-kyber/. The site says "Forward error correction" and "Bandwidth drops? Latency doesn't." No CC algorithm is named anywhere I found.
- **Latency:** the slides measure ~10-16 ms glass-to-glass on desktop (60-240 Hz) and ~16-33 ms on web. Interviews claim 8 ms at 240 Hz: https://streaminglearningcenter.com/codecs/an-interview-with-jean-baptiste-kempf-of-kyber.html
- **Loss-rate learning:** I found **no public source** saying that Kyber learns the loss rate and adapts retransmission or FEC. The public material describes only static mode selection (Reliable / GOPstream / Unreliable / Unreliable_fec) plus bitrate feedback. The owner's description may come from a talk recording (Demuxed 2024, Mile High Video 2025 https://www.youtube.com/watch?v=lWZt3ehI7us, FOSSASIA 2026) that I could not transcribe, or from unpublished code. **Treat "adaptive retransmission" as unverified.**
- **Source code:** no upstream repository URL is published (the site's "Get the SDK" link is a placeholder). A third-party project, https://github.com/lenylvt/syber ("Ultra-low latency remote desktop built on Kyber (QUIC + H.264)"), vendors Kyber crates named `kymux`, `kynet/kyproto` ("transport QUIC chiffré"), `kynput` and `kyutil/libkypc`. It exposes "Reliable / GOP Stream / Unreliable+FEC" as a server setting, which matches the slides. Its README says nothing about loss estimation.

**[I] Relevance to us.**
- Kyber's FEC unit is a whole encoded frame (one RaptorQ source block per video packet), with symbols sized to the maximum datagram. That is the opposite of our 120 B coalesced datagrams.
- Their slide's "hopefully ... on their own UDP datagrams" concedes the coalescing problem: if QUIC packs several symbols into one UDP packet, one network loss erases several symbols.

---

## 2. Moonlight / GameStream / Sunshine

All from source on master as of 2026-10:
- https://github.com/LizardByte/Sunshine/blob/master/src/stream.cpp
- https://github.com/LizardByte/Sunshine/blob/master/src/rtsp.cpp
- https://github.com/moonlight-stream/moonlight-common-c/blob/master/src/RtpVideoQueue.c
- https://github.com/moonlight-stream/moonlight-common-c/blob/master/src/VideoDepacketizer.c
- https://github.com/moonlight-stream/moonlight-common-c/blob/master/src/ControlStream.c

**Reed-Solomon library [D].** Both projects use `nanors` (submodule https://github.com/sleepybishop/nanors), a GF(2^8) RS library, via `#include <rs.h>` and `reed_solomon_new(data, parity)`. Because the field is GF(2^8), a block has at most 255 shards: Sunshine's comment derives `D = 255*100/(100+F)` for the maximum data shards per block.

**Per-frame FEC [D] (Sunshine `stream.cpp` ~L1611-1760, `fec::encode` ~L858).**
- `fec_percentage` config: **default 20**, range 0-100. The docs say "Percentage of error correcting packets per data packet in each video frame" (https://github.com/LizardByte/Sunshine/blob/master/docs/configuration.md).
- Parity per block: `parity = ceil(data * F / 100)`.
- Minimum parity: `minRequiredFecPackets` is negotiated from the client's RTSP field `x-nv-vqos[0].fec.minRequiredFecPackets` (default "0", rtsp.cpp L1081/1106). If parity falls below that minimum, the percentage is raised for that frame ("Increasing FEC percentage ... to meet parity shard minimum"). This matters for small frames, where 20% of 2 packets rounds down badly.
- Shard size = negotiated `packetSize` + RTP header, so every packet in a frame is the same size and the last one is zero-padded.
- A frame is split into at most `MAX_FEC_BLOCKS = 4` (the header has 2 bits for block count). If more are needed, **FEC is disabled for that frame**. The comment says this "should only happen for enormous frames (over 800 packets at 20%)". The FEC packet index is 10 bits.
- Bitrate accounting: if `fec_percentage <= 80`, the encoder bitrate is scaled by `(100-F)/100`, so video plus parity fits the user's bitrate (rtsp.cpp ~L1203-1212).
- Audio FEC is fixed RS(4 data + 2 parity) (`RTPA_DATA_SHARDS 4`, `RTPA_FEC_SHARDS 2`, moonlight-common-c `RtpAudioQueue.h`).

**No adaptivity [D].**
- Sunshine receives the client's `IDX_LOSS_STATS` (loss count, interval, last good frame) and **only logs it** (stream.cpp ~L1190-1204).
- Moonlight also sends a per-frame `SS_FRAME_FEC_STATUS` (data/parity totals, received data/parity, missing-before-highest, fecPercentage). I found no code in Sunshine's stream.cpp that changes `fec_percentage` from these reports. The percentage is static per session.

**No video retransmission [I, from code].**
- The video path is RTP/UDP with no NACK message in the control protocol. The control messages are loss stats, IDR request, reference-frame invalidation (RFI), FEC status, and others.
- If a frame is unrecoverable it is dropped: "Unrecoverable frame %d: lost FEC blocks". Recovery is then by RFI or IDR.

**RFI / IDR logic [D] (moonlight-common-c).**
- If RFI is supported, the client sends `connectionDetectedFrameLoss(start,end)`. Sunshine raises `invalidate_ref_frames_events` to the encoder, which re-references an older good frame. Otherwise it calls `LiRequestIdrFrame()`.
- **Speculative RFI** (RtpVideoQueue.c ~L210-243): if `missingPackets > totalPackets - neededPackets` before the frame completes, the client knows parity cannot save the frame and sends RFI immediately instead of waiting. This is disabled for 5 min (`SPECULATIVE_RFI_COOLDOWN_PERIOD_US 300000000`) after seeing out-of-sequence data, because reordering makes the prediction wrong.
- Connection-quality thresholds (ControlStream.c L125-127): frame-loss ≥30% in one interval, or ≥15% in two consecutive intervals, means "poor"; ≤5% means "okay". The loss report interval is 50 ms.

**[I] Takeaways.**
- In the most deployed open game-streaming stack, FEC is a static, per-frame MDS code at 20%, with RFI/IDR as the fallback and no retransmission.
- The minimum-parity knob, the "FEC off for huge frames" escape hatch, and speculative RFI on provably unrecoverable frames are all worth copying.

---

## 3. Other commercial and remote-desktop systems

**Parsec (BUD) [D].** UDP-based, DTLS 1.2 via OpenSSL, "basic reliability semantics like TCP", and "a highly tuned and custom congestion control algorithm". It has "no buffers of any kind on video", so it must detect congestion before it happens. Under loss the answer is to lower encoder bitrate. FEC is **not mentioned**. Source: https://parsec.app/blog/a-networking-protocol-built-for-the-lowest-latency-interactive-game-streaming-1fd5a03a6007/. No further public detail found.

**Steam Remote Play.** I found **no Valve documentation** on its FEC or retransmission. [I] It is often assumed to use Valve's GameNetworkingSockets, but I could not confirm that, so do not cite it.

**Chrome Remote Desktop [D].** Chromium's `remoting/protocol/` contains `webrtc_video_stream.*` and a WebRTC-based client video path (https://chromium.googlesource.com/chromium/src.git/+/main/remoting/protocol/webrtc_video_stream.h, https://codereview.chromium.org/1580823003). It therefore inherits WebRTC's NACK/FEC machinery (§4). I did not verify which protection modes CRD enables.

**Zoom.** No engineering publication on its FEC was found.
- Patent US9813193 [D] (assignee Zoom Video Communications; priority 2013): an FEC "filter table" at the server forwards "FEC packets with level equal or lower than its loss level to each receiver". It also limits retransmission requests per packet, and skips key-frame requests when more than 5 are received. https://patents.google.com/patent/US9813193
- [I] So Zoom does per-receiver, loss-level-graded FEC plus capped retransmission, at least as patented.

**Microsoft Teams [D] (via Tambur, NSDI'23).** "Teams employs an RS block code within each frame and varies the bandwidth overhead based on infrequent feedback from the receiver on packet losses". "Teams uses FEC only after a packet loss occurs, which is fairly standard in the industry." Source: https://www.usenix.org/conference/nsdi23/presentation/rudow

**PCoIP / HP Anyware [D].**
- Selective retransmission by data class: "Imaging data – may or may not be retransmitted depending on whether the imaging data is still current; Audio data – is not re-transmitted; USB packets – always re-transmitted" (https://anyware.hp.com/knowledge/what-is-intelligent-packet-re-transmission-in-pcoip-protocol).
- Teradici targets session loss below 0.1% (https://docs.teradici.com/knowledge/what-is-an-acceptable-level-of-packet-loss).
- No public FEC detail. A third-party blog claims FEC for audio (anecdotal).
- [I] "Retransmit only if not yet superseded" is exactly our tile situation.

**MS-RDPEUDP v1/v2 [D].** Specs: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeudp/ and the v2 PDF: https://winprotocoldocs-bhdugrdyduf5h2e4.b02.azurefd.net/MS-RDPEUDP2/%5bMS-RDPEUDP2%5d.pdf

RDPEUDP v1:
- §1.3.2.2: "An FEC Packet is added to the data stream after processing a block of m Source Packets". "The FEC Packets require no acknowledgments ... and they are not retransmitted". "The sender can either set the FEC block size to any value up to 255 or to not send any FEC Packets".
- §3.1.1.6 / §3.1.1.6.2: an FEC packet is a **linear combination over GF(2^8)** of the source packets (not plain XOR), sized to the largest source packet, with zero padding.
- §3.1.1.6.3: "Each FEC Packet can be used to recover only one Source Packet in the range covered by that FEC Packet". Not all combinations are invertible, and "not being able to decode a FEC Packet is not fatal".
- Header (§2.2.2.2 RDPUDP_FEC_PAYLOAD_HEADER): `snCoded`, `snSourceStart`, `uRange` (1 byte), `uFecIndex`.
- Two modes: RDP-UDP-R (reliable, retransmits) and RDP-UDP-L (lossy, `RDPUDP_FLAG_SYNLOSSY`, no retransmits).
- §3.1.1.4.1: loss is declared after **3 later datagrams** are received/acked.
- §3.1.6.1: retransmit timer = max(min RTO, 2×RTT), with min RTO 500 ms (v1) or 300 ms (v2 protocol version).

**RDPEUDP2 (UDP transport extension v3+) [D].**
- §3.1.1.1: "The RDP-UDP2 transport only supports reliable UDP mode ... This transport does not include an FEC layer." Microsoft **dropped FEC** in the newer transport.
- §3.1.1.2.3 loss detection: by reordering ("smaller by a set amount than the largest ... Received", the amount "fixed or dynamically variable") and by timeout (dynamically variable).
- Retransmissions get a new sequence number, while a channel sequence number in the payload identifies the data (§3.1.5.5). This is the same split as QUIC packet number vs stream offset.
- ACKs carry ACK vectors and per-packet receive-time deltas (`delayAckTimeAdditions`), and v2 is "URCP based rate control".

---

## 4. WebRTC

**Payload formats [D].**
- ULPFEC (RFC 5109): XOR with packet masks, carried in RED.
- FlexFEC (RFC 8627, https://www.rfc-editor.org/rfc/rfc8627): "non-interleaved and interleaved parity codes" (1-D row, 1-D column, 2-D XOR) that "offer a good protection against random and bursty packet losses, respectively". Repair goes in a separate RTP stream. §1.1.7 explicitly allows FEC combined with retransmission.

**Protection-factor logic [D].** Upstream main, `modules/video_coding/media_opt_util.{h,cc}`: https://webrtc.googlesource.com/src/+/main/modules/video_coding/media_opt_util.cc
- `kFecRateTable` (fec_rate_table.h, 6450 entries) is "Table for Protection factor (code rate) of delta frames, for the XOR FEC. Input is the packet loss and an effective rate (bits/frame)". The index is `rate_i*129 + loss_j`, with loss in 0..128 (Q8, so up to 50%).
- The effective rate is `resolnFac * bitsPerFrame`, where `resolnFac = 1/(pixels/(704*576))^0.3`.
- Key frames get a boosted factor (`boostKeyProt`).
- If the expected number of FEC packets per frame is below about 1, a cost correction `_corrFecCost` is set to 0.5 or 0, because rounding generates no FEC packets.

**Hybrid NACK/FEC by RTT [D] (`VCMNackFecMethod::ProtectionFactor`):**
- Low RTT, below `kLowRttNackMs = 20` ms: NACK only, and delta FEC `_protectionFactorD = 0`.
- RTT above `_highRttNackMs`: FEC only.
- In between: hybrid mode, which NACKs the residual after FEC. The RTT adjustment table is disabled ("TODO(mikhal): Disabling adjustment temporarily", `adjustRtt = 1.0`).
- **Current default:** `kNackFec` is constructed as `VCMNackFecMethod(field_trials, kLowRttNackMs, -1)`, so the high threshold is disabled and NACK is always allowed above 20 ms.
- `BitRateTooLowForFec`: FEC is off when bytes/frame < 400 / 700 / 1000 (by resolution tier), fewer than 3 temporal layers, and RTT < `kMaxRttTurnOffFec = 200` ms.
- `ComputeMaxFramesFec`: FEC may span `2 * base_layer_fps * rtt` frames, capped at `kUpperLimitFramesFec = 6`. The aim is complete frames within about one RTT.

**Loss input [D] (`fec_controller_default.cc`).**
- The reported fraction lost is passed through a **max filter** (`kMaxFilter`, "Use max window filter for now"). The window is 10 × 1 s bins (`kLossPrHistorySize = 10`, `kLossPrShortFilterWinMs = 1000`). FEC is therefore sized for the worst 1-second loss in the last 10 s.
- The FEC mask type is fixed to `kFecMaskRandom`. The comment reads: "`kFecMaskBursty` is more effective for consecutive losses ... As we currently do not have feedback data on the degree of correlated losses ... we keep default setting to `kFecMaskRandom`".

**FEC is charged against BWE [D].** `UpdateFecRates` measures the sent NACK+FEC rate over the total sent rate. It caps that at `kProtectionOverheadRateThreshold = 0.5` (field trial `WebRTC-ProtectionOverheadRateThreshold`) and returns `estimated_bitrate * (1 - overhead)` as the **encoder** target. The BWE estimate covers media plus protection, and the encoder gets the remainder.

**ULPFEC generator [D]** (https://webrtc.googlesource.com/src/+/main/modules/rtp_rtcp/source/ulpfec_generator.cc):
- `kMinMediaPackets = 4` (adapted down for low packets/frame).
- `kMaxExcessOverhead = 50` (Q8): it stops if actual overhead exceeds the requested rate by about 20%.
- `kHighProtectionThreshold = 80`.

**NACK timing [D]** (`nack_requester.cc`, upstream main): https://webrtc.googlesource.com/src/+/main/modules/video_coding/nack_requester.cc
- A missing seq is NACKed once the reordering-histogram-based wait (`WaitNumberOfPackets(0.5)`, the median reordering distance) has passed.
- It is re-NACKed every `rtt` (optional exponential backoff field trial: min retry 5 ms, max_rtt 160 ms, base 1.25).
- `kMaxNackRetries` is 100 on main (10 in older snapshots), `kMaxPacketAge = 10000`, `kMaxNackPackets = 1000`, and default RTT is 100 ms.
- **Packets already recovered by FEC/RTX are skipped** (`recovered_list_`).

**Sender-side RTT gating [D]** (`rtp_packet_history.cc` `VerifyRtt`, older mirror https://github.com/webrtc-sdk/webrtc/blob/master/modules/rtp_rtcp/source/rtp_packet_history.cc):
- A packet already retransmitted is not resent again if `now < last_send + rtt`.
- History holds packets for `max(3*RTT, 1000 ms)`, with capacity 9600.

**FEC-recovered packets and feedback [D]** (`call/call.cc`, `video/rtp_video_stream_receiver2.cc`, mirror https://github.com/webrtc-sdk/webrtc):
- `Call::OnRecoveredPacket` sets `set_recovered(true)` and delivers to the video receiver, but does **not** call `NotifyBweOfReceivedPacket`. Recovered packets are therefore **not reported as received in transport-wide feedback** and remain "lost" to the BWE.
- The receiver also skips `rtp_receive_statistics_->OnRtpPacket` for recovered packets, so RTCP RR loss is **pre-repair** loss.
- `UpdatePacketReceiveTimestamps` is skipped for recovered packets.
- This matches RFC 9265 Recommendation 1 (below).

**Transport-wide CC [D].**
- https://datatracker.ietf.org/doc/html/draft-holmer-rmcat-transport-wide-cc-extensions-01: a transport-wide seq number on every packet. Feedback lists per-packet arrival deltas, at 250 µs resolution.
- Standardised successor for RTP: RFC 8888 (https://www.rfc-editor.org/rfc/rfc8888).

**Loss-based BWE v2 [D]** (https://webrtc.googlesource.com/src/+/main/modules/congestion_controller/goog_cc/loss_based_bwe_v2.cc):
- Model: `p_loss = inherent_loss + (1 - inherent_loss) * max(0, sending_rate - loss_limited_bw) / sending_rate`. It separates a rate-independent "inherent" (random) loss from loss caused by sending above capacity.
- It holds candidate (bandwidth, inherent_loss) pairs and picks the one that maximises a temporally weighted log-likelihood over observations: `Σ w_t [lost·log p + received·log(1-p)]` plus a high-bandwidth bias.
- Defaults: `ObservationWindowSize = 15`, `ObservationDurationLowerBound = 250 ms`, `InitialInherentLossEstimate = 0.01`, `InherentLossLowerBound = 1e-3`, `NewtonIterations = 1`, `MaxIncreaseFactor = 1.3`.
- It does not increase the estimate while average loss > best inherent loss.
- [I] This is the closest production analogue to "learn the loss rate and decide whether it is congestion": the inherent-loss term is what FEC should cover, and the rate-dependent term is what the CC should cover.

---

## 5. QUIC plus FEC

**RFC 9265, "Forward Erasure Correction (FEC) Coding and Congestion Control in Transport" (NWCRG, 2022) [D].** https://www.rfc-editor.org/rfc/rfc9265.html
- Classifies FEC above, within, or below the transport.
- Recommendation 1 (§6.3): "From a congestion control point of view, a recovered packet must be considered as a lost packet".
- §6.2.2: disambiguating ACKed vs rebuilt packets helps the sender.
- §4: repair traffic must not cause congestion. When the repair ratio rises, the source rate should fall.
- Recommendation 3: if you hide loss from CC, compare against simply using a loss-tolerant CC.
- §3.2: FEC is most valuable for real-time traffic over unreliable or partially reliable transport.

**draft-michel-quic-fec-01 (Oct 2023, expired, Michel & Bonaventure) [D].** https://datatracker.ietf.org/doc/html/draft-michel-quic-fec
- Source symbols = protected frames (STREAM and DATAGRAM only). "A single source symbol MUST NOT contain the frames of several QUIC packets".
- Frames: SOURCE_SYMBOL (carries a Symbol ID), REPAIR, FEC_WINDOW, and SYMBOL_ACK (reports recovered SIDs so the sender does not retransmit them).
- Transport params: `enable_fec`, `decoder_fec_scheme`, `initial_coding_window`.
- The scheme is not fixed (RLC, RS, Tetrys cited).
- "Recovering symbols SHOULD NOT be used to infer any congestion state". "A QUIC endpoint MAY ignore every received repair symbol".

**QUIC-FEC (Michel, De Coninck, Bonaventure, IFIP Networking 2019) [D].** https://arxiv.org/abs/1904.11326
- A generic FEC frame with XOR, Reed-Solomon and convolutional RLC.
- Distinguishes received from recovered packets so CC still sees loss.
- Findings: FEC overhead **slows** downloads at low loss/delay and on long transfers. It helps under high loss + high delay and on small files, by avoiding RTOs. The authors conclude FEC should be used adaptively.

**FlEC (Michel, Cohen, Malak, De Coninck, Médard, Bonaventure; IEEE/ACM ToN 31(2):606-619, 2023; arXiv 2208.07741) [D].** https://arxiv.org/abs/2208.07741
- Implemented as a PQUIC plugin. Uses sliding-window random linear codes over GF(2^8) and is based on AC-RLNC.
- Two app-defined hooks: `ds()` (redundancy threshold) and `FECPattern()` (when to send repair).
- A loss monitor estimates the uniform loss rate and **Gilbert model parameters (GOOD→BAD, BAD→GOOD)**.
- Repair is sent when missing-DoF/added-DoF < r̂ + ds(). With ds = −l̂, repair goes "only when a packet is detected as lost and it has not been protected yet".
- For deadline messaging (video), FlEC "delays the sending of Repair Symbols to the last possible moment" that still meets the deadline.
- Repair is always within cwnd: "we never hide any signal to the congestion control".
- Results:
  - Video with a 250 ms deadline: plain QUIC failed 99% on-time delivery above 75 ms one-way delay, while FlEC "succeeded in every experiment".
  - At 5-70 ms one-way delay FlEC sent **no repair**, because ACKs arrived first.
  - "Protecting every message blindly" can double bytes sent.
  - FEC hurts with repair every RTT (AC-RLNC) on bulk, with small receive windows, and on 40-100 kB files.
- [I] The core lesson: at RTT ≪ deadline, ARQ is cheaper than FEC, and FEC pays off only as RTT approaches the deadline. This is the same structure as WebRTC's RTT gate.

**draft-swett-nwcrg-coding-for-quic.** An earlier NWCRG design (by Ian Swett) for coding in QUIC, superseded in spirit by RFC 9265. I did not fetch it, so no details are claimed.

**Coalescing [I].**
- Every QUIC-FEC design I read protects at **QUIC-packet or frame** granularity, and draft-michel forbids a source symbol spanning packets. None of them discusses many small application datagrams coalesced into one packet.
- With ~10 datagrams per UDP packet, the erasure unit the network sees is the packet. Per-datagram FEC across adjacent datagrams is defeated, because one packet loss is a 10-datagram burst.
- Options, all [I]:
  - (a) Make repair symbols cover data spread across **different** QUIC packets: interleave so that the k sources of a block land in k distinct packets.
  - (b) Size symbols to roughly a QUIC packet payload, so that one symbol equals one packet (Kyber's "symbol = max datagram" choice).
  - (c) Do FEC below quinn at the UDP-packet level, which needs a hook in the sans-IO poll_transmit path, and is the "below transport" category of RFC 9265.

**Related standards [D]:**
- RFC 6363 FECFRAME (block-code framework): https://www.rfc-editor.org/rfc/rfc6363
- RFC 8680 (FECFRAME extended to sliding-window codes): https://www.rfc-editor.org/rfc/rfc8680
- RFC 8681 (sliding-window RLC over GF(2) and GF(2^8)): "Compared to block FEC codes, these Sliding Window FEC Codes offer key advantages with real-time flows in terms of reduced FEC-related latency while often providing improved packet erasure recovery capabilities." https://www.rfc-editor.org/rfc/rfc8681
- RFC 9407 Tetrys (on-the-fly coding; "recover from erasures within an RTT-independent delay"): https://www.rfc-editor.org/rfc/rfc9407
- RFC 6330 RaptorQ: https://www.rfc-editor.org/rfc/rfc6330

---

## 6. Codes and implementations

**reed-solomon-simd (Rust, v3.1.0) [D].** https://github.com/AndersTrier/reed-solomon-simd
- Based on Leopard-RS, O(n log n). [I] Leopard uses GF(2^16), which is why shards are 16-bit words.
- "Shard size must be even". Multiples of 64 bytes are no longer required since 3.0.0.
- 1-32768 original × 1-32768 recovery shards. Up to 65535 total with power-of-two splits.
- SIMD: runtime selection of AVX2/SSSE3 (x86) and Neon (AArch64), "with fallback to plain Rust". **No wasm simd128 backend is listed**, so in wasm it uses the scalar path [I].
- Benchmarks (Ryzen 5 3600, 1 KiB shards, AVX2):
  - 32:32 encode 10.2 GiB/s, decode ~254 MiB/s.
  - 1024:1024 encode 4.8 GiB/s, decode 1.3 GiB/s.
  - The author says it is fastest except for "decoding with about 42 or fewer recovery shards".
  - One-time initialisation is under 10 ms.
- [I] Our blocks are small (k ≲ 64), so its O(n log n) advantage does not apply, and a GF(2^8) Cauchy/Vandermonde code is likely as fast or faster at small k.

**reed-solomon-erasure (Rust) [D].** https://github.com/rust-rse/reed-solomon-erasure
- A port of Klaus Post's Go / Backblaze RS. Offers `galois_8` (≤256 shards) and `galois_16`.
- Optional `simd-accel` C code is "tuned for Haswell+", so it is not usable in wasm.
- The README says "WASM builds are also available" (its wasm README link is now 404).

**raptorq (Rust, cberner, v2.0.1) [D].** https://github.com/cberner/raptorq
- RFC 6330. Reconstruction probability with K+h packets = 1 − 1/256^(h+1).
- Benchmarks (Ryzen 9 9950X3D, 1280 B symbols): encode ~16-30 Gbit/s at 10-1000 symbols; decode ~5.5-7 Gbit/s.
- No published wasm notes. [I] RaptorQ's precode/plan setup makes small-K blocks relatively costly. Kyber uses it anyway.

**C libraries [D]:**
- cm256: GF(256) Cauchy MDS, original < 256, "roughly 2x faster than Longhair". https://github.com/catid/cm256
- Leopard-RS: up to 65535 originals. https://github.com/catid/leopard
- wirehair: an O(N) fountain code, not MDS, "on average it takes about N + 0.02 packets to recover". https://github.com/catid/wirehair

**Wasm binary-size numbers:** I found **no published sizes** for any of these crates.

**Block vs streaming codes (latency) [D].**
- Martinian–Sundberg introduced low-delay burst-erasure streaming codes.
- Badr/Khisti et al. extend them to burst plus isolated erasures. "Streaming Codes for Channels with Burst and Isolated Erasures": https://arxiv.org/abs/1208.0072. MiDAS / layered constructions: https://arxiv.org/abs/1308.3827. Rate-optimal codes: https://arxiv.org/abs/1801.05919.
- Microsoft Research, Huang, Mehrotra and Li, "A hybrid FEC-ARQ protocol for low-delay lossless sequential data streaming", ICME 2009 (https://www.microsoft.com/en-us/research/?p=157762). It chooses per-packet between sending new data, resending originals, or sending check packets over undecoded packets, depending on conditions. It claims significantly lower total delay than ARQ or FEC alone.

**Tambur (Rudow, Yan, Kumar, Ananthanarayanan, Ellis, Rashmi; NSDI'23) [D].** https://www.usenix.org/conference/nsdi23/presentation/rudow, PDF https://www.usenix.org/system/files/nsdi23-rudow.pdf
- A streaming code with latency deadline τ = 3 frames (150 ms at 30 fps). Parity for frame i is sent with frames i..i+3.
- It handles "partial bursts", where only some packets of a frame are lost.
- A small neural network (one hidden layer, binary classifier) predicts the per-frame bandwidth overhead. The authors found ML beat heuristics.
- Results against Teams' per-frame RS ("Block-Within"): 26% fewer undecodable frames with 35% less redundancy bandwidth in simulation, and 26% fewer freezes and 29% less freeze duration in emulation.
- **Teams trace statistics** (~9700 calls with at least 2 loss events, which is 16% of traces):
  - Mean loss is 7% on these traces and 1.7% over all traces.
  - Frames using FEC per call: p25/50/75 = 13% / 48.8% / 70%.
  - Undecodable frames: p25/50/75 = 0.6% / 1.8% / 6.1%. The target is about 1%.
  - FEC overhead per call: 4.2% / 24% / 45%.
  - **38.1% of loss instances involve ≥2 consecutive packets.**
  - **38.4% span more than one frame.**
- A later critique (ReVo, arXiv 2604.27441, https://arxiv.org/pdf/2604.27441) says Tambur's long lookahead "tends to overpredict loss and yields consistently high overhead".

**TAROT (arXiv 2602.09880, Feb 2026) [D].** https://arxiv.org/abs/2602.09880
- Optimisation-driven per-segment choice of redundancy, block size and symbolization across RS, RaptorQ and XOR.
- Up to 43% less FEC overhead than static FEC, with +10 VMAF.
- Its context is DASH (LL and VoD), not interactive streaming.

---

## 7. Loss and burst estimation, and reporting

**Gilbert-Elliott [D, via FlEC].**
- Two-state Markov model with p = P(G→B) and r = P(B→G). In the simple Gilbert variant (loss probability 1 in B), mean burst = 1/r and stationary loss = p/(p+r).
- FlEC estimates p and r online.
- [I] For the simple Gilbert model, MLE from a 0/1 loss trace is p̂ = #(0→1)/#(0) and r̂ = #(1→0)/#(1).
- Measurement studies find 2-state GE inadequate on 802.11:
  - Cardoso & de Rezende find GE fails to capture the burst distribution and autocorrelation, and use a birth-death HMM: https://www.gta.ufrj.br/ftp/gta/TechReports/CaRe07.pdf
  - Happ et al. (QEST 2013, 116-node 802.11g) find phase-type distributions fit better than Gilbert as link quality rises: https://www.tkn.tu-berlin.de/bib/happ2013fault-impact/

**RTCP XR metrics [D]:**
- RFC 3611 burst/gap. A burst is the longest run starting and ending with a loss that contains no Gmin consecutive receptions. "A Gmin value of 16 is RECOMMENDED". The maximum loss rate inside a gap is 1/(Gmin+1). https://www.rfc-editor.org/rfc/rfc3611
- RFC 7003 burst/gap discard. https://www.rfc-editor.org/rfc/rfc7003
- RFC 5725 Post-repair Loss RLE ("packets that remain lost after all loss-repair methods are applied"). https://www.rfc-editor.org/rfc/rfc5725
- RFC 7509 post-repair loss count plus repaired loss count, "so that the RTP sender ... is able to evaluate the effectiveness of the repair methods". https://www.rfc-editor.org/rfc/rfc7509
- [I] Lesson: report pre-repair loss (for CC and the FEC sizer) and post-repair loss (for QoE and FEC efficacy) as separate counters.

**Congestion vs random loss [D]:**
- WebRTC LBv2 separates inherent loss from rate-dependent loss (§4).
- BBR (draft-ietf-ccwg-bbr) tolerates loss up to `BBR.LossThresh = 2%` per round before treating it as a congestion signal: https://datatracker.ietf.org/doc/draft-ietf-ccwg-bbr/
- Teradici sets a session loss target of 0.1%.
- [I] Practical discriminator: loss coinciding with rising one-way delay or queueing (our goog_cc delay gradient) is congestive. Loss at flat delay is random or link loss, and is the part FEC should be sized for.

**Deadline-aware / partial reliability [D]:**
- PR-SCTP (RFC 3758) "abandons" chunks per a lifetime policy: https://www.rfc-editor.org/rfc/rfc3758
- DTP (draft-shi-quic-dtp) is block-based "deliver-before-deadline" atop QUIC: "Data missing the deadline is useless since it will be overwrote by the new data". https://datatracker.ietf.org/doc/html/draft-shi-quic-dtp
- QUIC loss detection (RFC 9002 §6.1): packet threshold 3 (SHOULD NOT be less than 3) and time threshold 9/8 × max(srtt, latest_rtt). https://www.rfc-editor.org/rfc/rfc9002

**RTT-gated NACK [D]:**
- WebRTC: no FEC below 20 ms RTT; resend every RTT; sender suppresses a resend within one RTT of the last one.
- RDPEUDP: 3-dupack loss, RTO = max(300/500 ms, 2×RTT).
- [I] The rule implied by all of these: retransmit a tile or frame fragment only if `now + RTT/2 + ε < its usefulness deadline` (next frame or supersession), and the item has not already been superseded (PCoIP semantics).

---

## 8. Burst-length data for setting a model

- **Teams [D]** (Tambur, see §6): 38.1% of loss events are bursts of 2 or more packets, and 38.4% cross frame boundaries. Mean loss is 1.7% over all calls and 7% on calls where FEC engaged. Home broadband shows "loss rates ... often less than 1% for long periods, with infrequent periods of very bursty packet loss", and mobile shows "much longer packet loss bursts" around handovers (Tambur §3.2, which cites the FCC broadband report and other prior measurement studies; I did not check which ones).
- **Yajnik, Moon, Kurose, Towsley, INFOCOM 1999** ("Measurement and modelling of the temporal dependence in packet loss", DOI 10.1109/INFCOM.1999.749301) [D, abstract only]: 128 h of traces. A sliding-window average estimates loss rate better than exponential smoothing for the same memory. I did **not** retrieve its burst-length tables.
- **Burst ratio [D]** (US6931017, McGowan): mean burst length under random loss = 1/(1−L), and burst ratio = observed mean burst / that. A cheap, model-free burstiness metric: https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/6931017
- **802.11 [D, qualitative]:** GE underfits (see the papers above). I found **no authoritative number** for "mean Wi-Fi burst ≈ 1-3 packets", so treat that figure as folklore until a paper with tables is read.
- **[I] Suggested defaults for our system:**
  - Model loss per **UDP packet**, not per datagram.
  - Fit simple Gilbert p and r online from pre-repair ACK data, on a window of several seconds.
  - Use WebRTC-style max-filtering (10 × 1 s) for sizing.
  - Size FEC against the "inherent" component only.
  - Use MDS block size k with r = ceil(k·p_eff) + a minimum of 1, Sunshine-style.
  - Only use FEC when RTT/2 + decode slack would miss the deadline. Otherwise, use RTT-gated selective retransmit with supersession, as in WebRTC's 20 ms gate and FlEC's result.

---

## Gaps and unverified items

1. Whether Kyber adapts retransmission or FEC to measured loss: not in any public material found.
2. Steam Remote Play and Zoom internals: no primary sources.
3. Wasm binary sizes for RS/RaptorQ crates: none published.
4. Typical Wi-Fi burst-length tables: no primary source read. Yajnik 1999, Happ 2013 and Cardoso 2007 are the papers to read next.
5. WebRTC facts are from upstream `main` (googlesource) where checked. Some line numbers come from the older webrtc-sdk mirror, and differences are flagged (e.g., kMaxNackRetries 10 → 100).
