#!/usr/bin/env python3
"""FEC layout simulation for ghostframe (run with `python3 -I fec_sim.py`).

Question: at equal redundancy, which FEC *unit* and *code* recovers losses on a
QUIC-datagram path where quinn coalesces several small tile datagrams into one
QUIC packet (one packet loss = a run of consecutive wire_seqs), and how long does
a recovery take compared with a NACK/RTO retransmission?

Loss is applied per QUIC PACKET (Gilbert-Elliott, loss_bad=1, loss_good=0), the
way the browserless netsim and a real path drop it.

Schemes (all ~10-20% overhead unless stated):
  none            no FEC
  xor10_dg        today: XOR over K=10 CONTIGUOUS datagrams, P datagrams per packet
  xor10_dg_s12    C3's interleave: XOR over datagrams wire_seq g, g+12, g+24 ... (K=10)
  bundle_*        tile datagrams batched into MTU-sized bundles: 1 bundle = 1 QUIC
                  packet = 1 FEC symbol. Then:
    bundle_xor10      XOR over 10 contiguous bundles (10%)
    bundle_xor5       XOR over 5 contiguous bundles (20%)
    bundle_rs10_2     RS(k=10, r=2) block over bundles (20%), ideal MDS
    bundle_rs20_2     RS(k=20, r=2) (10%)
    bundle_rs20_4     RS(k=20, r=4) (20%)
    bundle_rlc_w20_r5 sliding-window random linear code: after every 5 sources one
                      repair = random GF(p) combination of the last 20 sources (20%)
    bundle_rlc_w30_r10  same, one repair per 10 sources over the last 30 (10%)

Metrics:
  residual   fraction of source symbols still missing after FEC (a source datagram
             in the unbundled schemes; a bundle in the bundled ones -- one lost
             bundle == the same ~P tile datagrams one lost packet carries)
  recovered  fraction of lost source symbols that FEC repaired
  delay_p50/p95  recovery delay in PACKET SLOTS from the lost symbol's own send
             slot to the slot whose arrival made it decodable. Multiply by the
             packet interval (e.g. 0.12 ms at 10k pkt/s, 1.2 ms at 1k pkt/s) and
             compare with a retransmission's >= 1 RTT + detection.

Estimates, not measurements: ideal decoders, no reordering, constant packet rate.
"""
import random
import statistics
import sys

P_DG_PER_PKT = 11  # measured ~10-12 tile datagrams per QUIC packet (C3 evidence A1)
N_PKTS = 60000


def ge_losses(n, rate, burst, rng):
    """Gilbert-Elliott packet losses with mean loss rate `rate` and mean burst `burst`."""
    if rate == 0:
        return [False] * n
    if burst <= 1.0:
        return [rng.random() < rate for _ in range(n)]
    p_bg = 1.0 / burst                      # bad -> good
    p_gb = rate * p_bg / (1.0 - rate)       # good -> bad, stationary pi_bad = rate
    bad = False
    out = []
    for _ in range(n):
        bad = (rng.random() < p_gb) if not bad else (rng.random() >= p_bg)
        out.append(bad)
    return out


def summarize(lost_src, recov_delay, n_src):
    n_lost = sum(lost_src)
    rec = len(recov_delay)
    resid = (n_lost - rec) / n_src
    d = sorted(recov_delay)
    p50 = d[len(d) // 2] if d else float("nan")
    p95 = d[int(len(d) * 0.95)] if d else float("nan")
    return resid, (rec / n_lost if n_lost else float("nan")), p50, p95


# --- unbundled datagram-level XOR (today and C3 interleave) -------------------
def sim_dg_xor(pkt_loss, k, stride, offset_groups=2):
    """Datagram i goes in packet i // P. Group = k datagrams with members
    base, base+stride, ...; parity sent `offset` datagrams after the group's last
    member (today: PARITY_INTERLEAVE_OFFSET = 20 = 2k datagrams after first).
    Parity datagrams also occupy packet slots; to keep it simple they ride in the
    packet of the datagram slot they are emitted at (adds ~10% datagrams)."""
    n_dg = len(pkt_loss) * P_DG_PER_PKT
    # Build the datagram stream with parities interleaved by emission slot.
    # Members of group (b, j): datagram index b*stride*k... simpler: block of
    # stride*k sources holds `stride` interleaved groups.
    lost_src = []
    recov = []
    # Emission positions: sources at slot s; we inflate slot positions by 1/k
    # for parity to approximate the extra datagrams.
    block = stride * k
    n_src = (n_dg * k) // (k + 1)
    n_src -= n_src % block
    src_slot = []
    slot = 0
    parity_slots = {}
    for i in range(n_src):
        src_slot.append(slot)
        slot += 1
        # after the last member of a block's groups: emit `stride` parities,
        # delayed by offset_groups*k sources (today's interleave offset)
        if (i + 1) % block == 0:
            b = i // block
            parity_slots[b] = slot  # placeholder, real slot assigned below
            for _ in range(stride):
                slot += 1
    # assign parity slot: after offset (approx: last member slot + offset_groups*k)
    lost = lambda s: pkt_loss[min(s // P_DG_PER_PKT, len(pkt_loss) - 1)]
    lost_src = [lost(s) for s in src_slot]
    for b in range(n_src // block):
        for j in range(stride):
            members = [b * block + j + m * stride for m in range(k)]
            last_slot = src_slot[members[-1]]
            pslot = last_slot + offset_groups * k  # emitted that many slots later
            if lost(pslot):
                continue
            missing = [m for m in members if lost_src[m]]
            if len(missing) == 1:
                recov.append(pslot - src_slot[missing[0]])
    # delay in datagram slots -> convert to packet slots
    recov = [d / P_DG_PER_PKT for d in recov]
    return summarize(lost_src, recov, n_src)


# --- bundled: 1 symbol = 1 packet ---------------------------------------------
def sim_bundle_block(pkt_loss, k, r, depth=1):
    """Systematic block code over k source packets + r repair packets (ideal MDS;
    r=1 is XOR). `depth` > 1 interleaves `depth` blocks symbol by symbol, so a
    burst of up to `depth` packets costs each block at most one symbol, at the
    price of a `depth`-times longer span (= recovery delay)."""
    lost_src, recov = [], []
    n_src = 0
    pos = 0
    n = len(pkt_loss)
    span = (k + r) * depth
    while pos + span <= n:
        for j in range(depth):
            slots = [pos + j + m * depth for m in range(k + r)]
            src = [pkt_loss[s] for s in slots[:k]]
            rep = [pkt_loss[s] for s in slots[k:]]
            got_rep = [i for i, l in enumerate(rep) if not l]
            miss = [i for i, l in enumerate(src) if l]
            lost_src.extend(src)
            n_src += k
            if miss and len(miss) <= len(got_rep):
                done_at = slots[k + got_rep[len(miss) - 1]]
                for m in miss:
                    recov.append(done_at - slots[m])
        pos += span
    return summarize(lost_src, recov, n_src)


PRIME = 2_147_483_647


def sim_bundle_rlc(pkt_loss, window, every, rng):
    """Sliding-window RLC (RFC 8681 style): after every `every` sources one repair
    symbol = random combination over GF(p) of the last `window` sources. Decoding
    by Gaussian elimination over the unknowns; a lost source counts recovered at
    the slot where it becomes solvable. Unknowns older than 2*window are abandoned
    (bounded decoder memory / deadline)."""
    lost_src, recov = [], []
    unknown = {}       # src idx -> slot it was sent
    eqs = []           # list of dict{src: coeff}, already reduced by known sources
    slot = 0
    src_idx = 0
    i = 0
    n = len(pkt_loss)
    while i < n:
        # one source
        l = pkt_loss[i]
        lost_src.append(l)
        if l:
            unknown[src_idx] = slot
        src_idx += 1
        slot += 1
        i += 1
        if src_idx % every == 0 and i < n:
            l_rep = pkt_loss[i]
            i += 1
            slot += 1
            if not l_rep:
                lo = max(0, src_idx - window)
                eq = {s: rng.randrange(1, PRIME) for s in unknown if s >= lo}
                if eq:
                    eqs.append(eq)
            # abandon too-old unknowns
            for s in [s for s in unknown if s < src_idx - 2 * window]:
                del unknown[s]
                eqs = [e for e in eqs if s not in e]
            # try to solve: Gaussian elimination on current eqs
            solved = solve(eqs, set(unknown))
            for s in solved:
                recov.append(slot - unknown[s])
                del unknown[s]
            if solved:
                eqs = [{v: c for v, c in e.items() if v not in solved} for e in eqs]
                eqs = [e for e in eqs if e]
    return summarize(lost_src, recov, len(lost_src))


def solve(eqs, unknowns):
    """Return the set of unknowns uniquely determined by eqs (GF(p) elimination)."""
    if not eqs:
        return set()
    vars_ = sorted({v for e in eqs for v in e})
    idx = {v: j for j, v in enumerate(vars_)}
    rows = []
    for e in eqs:
        row = [0] * len(vars_)
        for v, c in e.items():
            row[idx[v]] = c
        rows.append(row)
    # reduced row echelon form
    piv_cols = []
    r = 0
    for c in range(len(vars_)):
        pr = next((i for i in range(r, len(rows)) if rows[i][c]), None)
        if pr is None:
            continue
        rows[r], rows[pr] = rows[pr], rows[r]
        inv = pow(rows[r][c], PRIME - 2, PRIME)
        rows[r] = [(x * inv) % PRIME for x in rows[r]]
        for i in range(len(rows)):
            if i != r and rows[i][c]:
                f = rows[i][c]
                rows[i] = [(a - f * b) % PRIME for a, b in zip(rows[i], rows[r])]
        piv_cols.append(c)
        r += 1
        if r == len(rows):
            break
    solved = set()
    for i, c in enumerate(piv_cols):
        if all(rows[i][j] == 0 for j in range(len(vars_)) if j != c):
            solved.add(vars_[c])
    return solved


def main():
    rng = random.Random(0xFEC)
    models = [
        ("iid1", 0.01, 1), ("iid2", 0.02, 1), ("iid5", 0.05, 1),
        ("ge2_b3", 0.02, 3), ("ge5_b3", 0.05, 3), ("ge2_b8", 0.02, 8),
    ]
    schemes = [
        ("none", None),
        ("xor10_dg (today)", lambda pl: sim_dg_xor(pl, 10, 1)),
        ("xor10_dg_s12 (C3)", lambda pl: sim_dg_xor(pl, 10, 12)),
        ("bundle_xor10 (10%)", lambda pl: sim_bundle_block(pl, 10, 1)),
        ("bundle_xor5 (20%)", lambda pl: sim_bundle_block(pl, 5, 1)),
        ("bundle_rs20_2 (10%)", lambda pl: sim_bundle_block(pl, 20, 2)),
        ("bundle_rs10_2 (20%)", lambda pl: sim_bundle_block(pl, 10, 2)),
        ("bundle_rs20_4 (20%)", lambda pl: sim_bundle_block(pl, 20, 4)),
        ("bundle_rs10_2_d4 (20%)", lambda pl: sim_bundle_block(pl, 10, 2, 4)),
        ("bundle_rs10_2_d8 (20%)", lambda pl: sim_bundle_block(pl, 10, 2, 8)),
        ("bundle_rlc_w30_r10 (10%)", lambda pl: sim_bundle_rlc(pl, 30, 10, rng)),
        ("bundle_rlc_w20_r5 (20%)", lambda pl: sim_bundle_rlc(pl, 20, 5, rng)),
    ]
    print(f"packets={N_PKTS} datagrams/packet(unbundled)={P_DG_PER_PKT}; delay in packet slots")
    print(f"{'scheme':26s} {'model':7s} {'resid':>7s} {'recov':>6s} {'d_p50':>6s} {'d_p95':>6s}")
    for mname, rate, burst in models:
        pl = ge_losses(N_PKTS, rate, burst, rng)
        real = sum(pl) / len(pl)
        for sname, fn in schemes:
            if fn is None:
                print(f"{sname:26s} {mname:7s} {real:7.4f} {'-':>6s} {'-':>6s} {'-':>6s}")
                continue
            resid, rec, p50, p95 = fn(pl)
            print(f"{sname:26s} {mname:7s} {resid:7.4f} {rec:6.3f} {p50:6.1f} {p95:6.1f}")
        print()
        sys.stdout.flush()


if __name__ == "__main__":
    main()
