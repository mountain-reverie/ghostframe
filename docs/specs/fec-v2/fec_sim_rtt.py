#!/usr/bin/env python3
"""Reconstruction-first vs timer-driven repair, at 20 ms and 75 ms RTT.

Run: python3 -I fec_sim_rtt.py   (writes nothing; tee the output to fec_sim_rtt_out.txt)

Model (estimates, not measurements):
- The sender emits one bundle (= one QUIC packet = one FEC symbol) every SLOT_MS.
  Sources are grouped into RS(k, r) blocks; the r repair symbols follow the k
  sources. Ideal MDS: any k of k+r symbols decode the block.
- Original transmissions lose packets by a Gilbert-Elliott chain. Later
  transmissions (retransmits, extra repair symbols) are lost i.i.d. at the same
  mean rate: they go out at other times, so they see a fresh draw.
- One-way delay OWD = RTT/2, no jitter, no reordering. Feedback (ACK/NACK) is
  never lost (that case is what the last-resort timer exists for; see the doc).
- Deadline: a source must be usable at the receiver within OWD + BUDGET_MS of
  being sent; later arrivals count as residual loss ("late").

Policies
  timer_noFEC    no FEC; a sender timer retransmits each unACKed source at
                 send + RTT + 3 slots (quinn/RFC 9002 style), again each RTT.
  timer_FEC      the same timer, plus RS(k, r). The sender only learns of a FEC
                 recovery when the receiver's ACK arrives (decode time + OWD), so
                 the timer fires on anything not yet ACKed. This is the owner's
                 objection to stream retransmission.
  rto150_FEC     RS(k, r) plus today's app RTO: retransmit at
                 send + max(150 ms, 3 x RTT) if still unACKed.
  recon_early    as recon_first, but the first REPAIR_REQUEST goes out as soon as
                 the block is *known* unrecoverable (r+1 symbols seen missing),
                 instead of waiting for the window to close; a top-up request
                 follows at window close if still short.
  recon_first    RS(k, r). The receiver waits for the block's recovery window
                 (last symbol's expected arrival + ALLOW_MS). If it still has
                 fewer than k symbols, it sends REPAIR_REQUEST(block, m), where m
                 is the shortfall, and the sender answers with m *new* repair
                 symbols. Repeat while a round can still meet the deadline
                 (RTT-gated); otherwise give up (residual; H.264 then asks C5 for a
                 keyframe or reference invalidation).

Reported per policy:
  resid      fraction of sources not usable by the deadline
  lat_p50/p95/p99  ms from send to usable, over sources whose original was lost
  ovh        (repair + retransmitted symbols) / source symbols
  retx       retransmissions or extra repairs sent, per 1000 sources
  wasted     retransmissions that arrived after the source was already usable
             (via FEC or an earlier copy) or after its deadline, per 1000 sources
  avoided    lost sources made usable by reconstruction with no repair traffic,
             as a fraction of lost sources
"""
import random

SLOT_MS = 1.0          # ~9 Mbit/s of 1150-byte bundles
N_BLOCKS = 8000
ALLOW_MS = 3.0         # recovery-window allowance after the last expected symbol
BUDGETS = {"h264": 50.0, "tile": 120.0}


def ge_chain(n, rate, burst, rng):
    if burst <= 1.0:
        return [rng.random() < rate for _ in range(n)]
    p_bg = 1.0 / burst
    p_gb = rate * p_bg / (1.0 - rate)
    bad, out = False, []
    for _ in range(n):
        bad = (rng.random() < p_gb) if not bad else (rng.random() >= p_bg)
        out.append(bad)
    return out


def pct(v, q):
    if not v:
        return float("nan")
    v = sorted(v)
    return v[min(len(v) - 1, int(q * len(v)))]


def simulate(policy, k, r, rtt, rate, burst, budget, rng):
    owd = rtt / 2
    n = k + r if policy != "timer_noFEC" else k
    losses = ge_chain(N_BLOCKS * n, rate, burst, rng)
    resid = 0
    lat = []
    sent_extra = 0
    wasted = 0
    avoided = 0
    lost_total = 0
    t = 0.0
    for b in range(N_BLOCKS):
        send = [t + i * SLOT_MS for i in range(n)]
        lost = losses[b * n:(b + 1) * n]
        t += n * SLOT_MS
        arr = [None if lost[i] else send[i] + owd for i in range(n)]
        deadline = [send[i] + owd + budget for i in range(k)]
        # time each source becomes usable at the receiver
        usable = [arr[i] for i in range(k)]
        got = [a for a in arr if a is not None]
        decode_at = None
        if policy != "timer_noFEC" and len(got) >= k:
            decode_at = sorted(got)[k - 1]
        n_lost_src = sum(1 for i in range(k) if lost[i])
        lost_total += n_lost_src
        if decode_at is not None:
            for i in range(k):
                if usable[i] is None:
                    usable[i] = decode_at
            avoided += n_lost_src

        if policy in ("timer_noFEC", "timer_FEC", "rto150_FEC"):
            # sender learns "usable" via ACK at usable + owd
            for i in range(k):
                if not lost[i]:
                    continue
                if policy == "rto150_FEC":
                    fire = send[i] + max(150.0, 3 * rtt)
                    period = max(150.0, 3 * rtt)
                else:
                    fire = send[i] + rtt + 3 * SLOT_MS
                    period = rtt
                while True:
                    acked_at = None if usable[i] is None else usable[i] + owd
                    if acked_at is not None and acked_at <= fire:
                        break
                    if fire > deadline[i] + rtt:      # sender gives up eventually
                        break
                    sent_extra += 1
                    arrive = fire + owd
                    if rng.random() < rate:
                        fire += period
                        continue
                    if usable[i] is not None and usable[i] <= arrive:
                        wasted += 1                   # already rebuilt / delivered
                    elif arrive > deadline[i]:
                        wasted += 1                   # too late to matter
                        usable[i] = arrive
                    else:
                        usable[i] = arrive
                    fire += period
                    # stop after the copy that delivered it
                    if usable[i] is not None and usable[i] <= arrive:
                        break
        elif policy in ("recon_first", "recon_early"):
            have = len(got)
            if have < k:
                # receiver: block window closes after the last symbol's expected arrival
                window_close = send[n - 1] + owd + ALLOW_MS
                now = window_close
                if policy == "recon_early":
                    # Unrecoverability is certain as soon as r+1 symbols are known
                    # lost: a loss at index j is known when a later symbol arrives
                    # (no reordering here) plus the allowance. Ask then for the
                    # shortfall known so far; top up at window close.
                    lost_idx = [i for i in range(n) if lost[i]]
                    j = lost_idx[r]                       # the (r+1)-th loss
                    later = [arr[x] for x in range(j + 1, n) if arr[x] is not None]
                    if later:
                        t_know = min(later) + ALLOW_MS
                        lost_so_far = sum(1 for x in lost_idx if x <= j)
                        m1 = lost_so_far - r
                        land = t_know + rtt
                        if land <= min(deadline[i] for i in range(k) if usable[i] is None):
                            sent_extra += m1
                            got_back = sum(1 for _ in range(m1) if rng.random() >= rate)
                            have += got_back
                            now = max(window_close, land + ALLOW_MS)
                while have < k:
                    m = k - have
                    # RTT-gated: a round lands at now + rtt (request owd + response owd)
                    land = now + rtt
                    if land > min(deadline[i] for i in range(k) if usable[i] is None):
                        break                         # cannot beat the earliest deadline
                    sent_extra += m
                    for _ in range(m):
                        if rng.random() >= rate:
                            have += 1
                    now = land + ALLOW_MS
                if have >= k:
                    for i in range(k):
                        if usable[i] is None:
                            usable[i] = now - ALLOW_MS
        for i in range(k):
            if lost[i]:
                if usable[i] is None or usable[i] > deadline[i]:
                    resid += 1
                else:
                    lat.append(usable[i] - send[i])
    n_src = N_BLOCKS * k
    parity = 0 if policy == "timer_noFEC" else N_BLOCKS * r
    return {
        "resid": resid / n_src,
        "p50": pct(lat, 0.5), "p95": pct(lat, 0.95), "p99": pct(lat, 0.99),
        "ovh": (parity + sent_extra) / n_src,
        "retx": 1000 * sent_extra / n_src,
        "wasted": 1000 * wasted / n_src,
        "avoided": avoided / lost_total if lost_total else float("nan"),
    }


def main():
    rng = random.Random(0xFEC2)
    print(f"slot={SLOT_MS} ms, blocks={N_BLOCKS}, window allowance={ALLOW_MS} ms; latency over sources whose original was lost")
    header = f"{'rtt':>4s} {'model':8s} {'class':5s} {'policy':12s} {'k,r':6s} {'resid':>8s} {'p50':>6s} {'p95':>6s} {'p99':>6s} {'ovh':>6s} {'retx/1k':>8s} {'wasted/1k':>9s} {'avoided':>7s}"
    print(header)
    for rtt in (20.0, 75.0):
        for mname, rate, burst in (("iid2", 0.02, 1), ("ge2_b3", 0.02, 3), ("iid5", 0.05, 1)):
            for cls in ("tile", "h264"):
                budget = BUDGETS[cls]
                for policy, k, r in (
                    ("timer_noFEC", 10, 0),
                    ("timer_FEC", 10, 2),
                    ("rto150_FEC", 10, 2),
                    ("recon_first", 10, 2),
                    ("recon_first", 10, 3),
                    ("recon_first", 10, 4),
                    ("recon_early", 10, 2),
                    ("recon_early", 10, 3),
                ):
                    s = simulate(policy, k, r, rtt, rate, burst, budget, rng)
                    print(f"{rtt:4.0f} {mname:8s} {cls:5s} {policy:12s} {k},{r:<4d} {s['resid']:8.5f} {s['p50']:6.1f} {s['p95']:6.1f} {s['p99']:6.1f} {s['ovh']:6.3f} {s['retx']:8.1f} {s['wasted']:9.1f} {s['avoided']:7.3f}")
            print()


if __name__ == "__main__":
    main()
