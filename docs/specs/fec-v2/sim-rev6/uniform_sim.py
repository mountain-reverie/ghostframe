#!/usr/bin/env python3
"""Revision-6 FEC design simulation: uniform erasure coding over ALL server->client
bundles (H.264 fragments and lossless tiles alike).

Run: python3 -I uniform_sim.py [--quick] [--jobs N]
Writes (next to this file): uniform_sim_out.txt (full table), uniform_sim_frames_out.txt
(H.264 frame table), uniform_sim_arq_out.txt (ARQ comparison), uniform_sim_classes_out.txt
(per tile-burst-size recovery latency).

Everything here is a model: estimates, not measurements. See FINDINGS.md and TIMING.md.

Sender model (read from code, TIMING.md):
  * admission token bucket at the link rate C, burst cap 33.3 ms of rate (admission.rs);
  * a "pass" (one drain_reliable_emitter call) emits at most ceil(D_target/MTU) bundles,
    D_target = max(MTU, 10 ms of rate) (admission.rs depth_target); passes repeat
    as quinn's FIFO empties (refill_after_drain) or on the token wake (>= 1 ms, or
    the time to afford one MTU, whichever is longer; pacing.rs token_wake_floor);
  * bundles of one pass leave quinn at the pacer's burst rate, 2.5 x C (window = 2 x R x RTT,
    pacer refills at 1.25 x window / RTT; quinn pacing.rs), then serialise through a
    FIFO bottleneck of rate C;
  * repair bundles are charged to the bucket when emitted (debt allowed), like parity today;
  * H.264 frames are queued whole at commit (30 fps), tile bursts whole at damage time.

Receiver model: ideal MDS decode for RS (k of n), RaptorQ with the reception-overhead
failure probabilities of the task (1e-2 at k, 1e-4 at k+1, 1e-6 at k+2), sliding-window
RLC with real GF(2^8) random coefficients (so the 1/256 dependence is in the model).
Loss is per bundle (one bundle = one QUIC packet), i.i.d. or Gilbert-Elliott, indexed
by packet number. No reordering, no jitter, feedback not simulated.

Time unit: ms. "wait" (w) = how much later than its own would-have-been arrival a lost bundle
becomes usable (the FEC-attributable delay; excludes OWD and bottleneck queueing). Absolute
figures are not reported for bundles: with the token rate equal to the link rate the 33 ms
bucket leaves a standing 15-30 ms bottleneck queue in the model, a common-mode offset
(FINDINGS.md, assumptions).
"""
import math
import random
import sys
import zlib
from bisect import bisect_left
from collections import deque
from heapq import heappush, heappop
from multiprocessing import Pool
import os

PKT = 1180                  # bundle bytes (one QUIC datagram frame, one FEC symbol)
BURST = 2.5                 # quinn pacer burst rate / C
BUCKET_S = 33.333e-3        # admission.rs BURST_WINDOW
DTARGET_S = 10e-3           # admission.rs QUEUE_TARGET
FPS = 30
FRAME_MS = 1000.0 / FPS
RQ_FAIL = {0: 1e-2, 1: 1e-4, 2: 1e-6}   # task statement (fec-v2-design.md 2.5 has 1/256^(h+1))
OWDS = (5.0, 20.0, 40.0)
OWD_MAIN = 20.0

# ------------------------------------------------------------------ GF(256)
_EXP = [0] * 512
_LOG = [0] * 256
_x = 1
for _i in range(255):
    _EXP[_i] = _x
    _LOG[_x] = _i
    _x <<= 1
    if _x & 0x100:
        _x ^= 0x11D
for _i in range(255, 512):
    _EXP[_i] = _EXP[_i - 255]


def gmul(a, b):
    if a == 0 or b == 0:
        return 0
    return _EXP[_LOG[a] + _LOG[b]]


def ginv(a):
    return _EXP[255 - _LOG[a]]


# ------------------------------------------------------------------ loss
def ge_chain(n, rate, burst, rng):
    if rate <= 0:
        return [False] * n
    if burst <= 1.0:
        return [rng.random() < rate for _ in range(n)]
    p_bg = 1.0 / burst
    p_gb = rate * p_bg / (1.0 - rate)
    bad, out = False, []
    for _ in range(n):
        bad = (rng.random() < p_gb) if not bad else (rng.random() >= p_bg)
        out.append(bad)
    return out


LOSSES = [("iid0.5", 0.005, 1.0), ("iid1", 0.01, 1.0), ("iid2", 0.02, 1.0), ("iid5", 0.05, 1.0),
          ("ge2b2", 0.02, 2.0), ("ge2b3", 0.02, 3.0), ("ge2b8", 0.02, 8.0)]


# ------------------------------------------------------------------ traffic
def tile_burst_size(rng):
    u = rng.random()
    if u < 0.55:
        return rng.randint(1, 6)
    if u < 0.85:
        return rng.randint(7, 60)
    if u < 0.97:
        return rng.randint(61, 400)
    return rng.randint(1000, 2800)      # ~full-screen 1080p first paint (1.2-3.3 MB)


def burst_class(n):
    return 0 if n <= 6 else (1 if n <= 60 else 2)


def gen_demand(mix, C, n_src, rng):
    """List of (t_ms, n_bundles, kind, unit_id, extra) sorted by t.
    kind 'f' = H.264 frame (extra = is_idr), 'b' = tile burst (extra = burst class)."""
    R = C * 125.0                                  # bytes per ms
    h_share, t_share = {"h264": (0.70, 0.0), "tiles": (0.0, 0.30), "mix": (0.45, 0.20)}[mix]
    out = []
    total = 0
    if h_share:
        avg_p = h_share * R * FRAME_MS / PKT       # mean P-frame size in bundles
        # lognormal sizes (sigma 0.8 = many tiny frames and a few cap-sized), then rescaled
        raw = []
        nfr = int(n_src * (h_share / (h_share + t_share)) / max(avg_p, 1.0)) + 50
        for i in range(nfr):
            raw.append(rng.lognormvariate(0.0, 0.8))
        m = sum(raw) / len(raw)
        t = 0.0
        for i, v in enumerate(raw):
            idr = (i > 0 and i % 150 == 0)
            size = v / m * avg_p
            n = int(round(8 * avg_p)) if idr else int(round(min(size, 3 * avg_p)))
            n = max(1, n)
            out.append((t, n, 'f', i, 1 if idr else 0))
            total += n
            t += FRAME_MS
    if t_share:
        # Poisson burst arrivals, offered load t_share * C
        mean_n = 0.55 * 3.5 + 0.30 * 33.5 + 0.12 * 230.5 + 0.03 * 1900
        gap_mean = mean_n * PKT / (t_share * R)    # ms between bursts
        nb = int(n_src * (t_share / (h_share + t_share)) / mean_n) + 20
        t = 0.0
        for i in range(nb):
            t += rng.expovariate(1.0 / gap_mean)
            n = tile_burst_size(rng)
            out.append((t, n, 'b', i, burst_class(n)))
    out.sort(key=lambda d: d[0])
    return out


# ------------------------------------------------------------------ sender
class Timeline:
    pass


def build_timeline(demand, C, spec, rho, rng):
    """Run the sender for one scheme. Returns a Timeline with per-packet departure and
    link-arrival times (excluding OWD) and the code structure needed by the decoder."""
    R = C * 125.0
    pkt_ms = PKT * 8.0 / (C * 1000.0)
    cap = max(2 * PKT, R * BUCKET_S * 1000.0)
    gmax = max(1, math.ceil(max(PKT, R * DTARGET_S * 1000.0) / PKT))
    fam = spec["fam"]
    kmax = spec.get("kmax", 64)
    rmin = spec.get("rmin", 1)
    if spec.get("kfit") and rho > 0:
        # largest k at which r = rmin is not above the target overhead: close the block as soon
        # as it is that big (rho 0.1/0.2/0.3 with rmin 1 -> k 10/5/3; with rmin 2 -> 20/10/6)
        kmax = max(1, int(rmin / rho + 1e-9))

    tl = Timeline()
    dep = []           # packet departure time
    pkind = []         # 0 source, 1 repair
    pref = []          # source id (kind 0) / block id or RLC cnt (kind 1)
    s_unit = []        # per source: ('f'|'b', unit id)
    s_kind = []        # 'f' or 'b'
    s_cls = []         # burst class for tiles, -1 for frames
    s_pkt = []         # source id -> packet index
    s_commit = []
    blocks = []        # dict(srcs=[sid], reps=[pidx], code='rs'|'rq')
    src_order = []     # source ids in departure order (RLC windows)
    unit_src = {}      # (kind, unit) -> list of sids

    queue = deque()    # entries ('s', sid) / ('r', blockid)
    state = {"tokens": cap, "last": 0.0, "fifo_free": 0.0, "pass_t": None,
             "credit": 0.0, "since_rep": 0}
    timers = []
    open_blk = {"b": None}
    staged = []        # interleave staging: list of (blockid)
    n_blk_id = [0]

    def new_block(srcs, code):
        blocks.append({"srcs": list(srcs), "reps": [], "code": code, "k": len(srcs)})
        return len(blocks) - 1

    def nrep(k):
        return max(rmin, math.ceil(rho * k - 1e-9)) if rho > 0 else 0

    def emit(t, items):
        """items: list of ('s', sid) / ('r', blockid) / ('x', cnt) (RLC repair).
        Departs them back to back at the pacer burst rate."""
        start = max(t, state["fifo_free"])
        step = pkt_ms / BURST
        for i, (k, v) in enumerate(items):
            pi = len(dep)
            dep.append(start + i * step)
            if k == 's':
                pkind.append(0)
                pref.append(v)
                s_pkt[v] = pi
                src_order.append(v)
            elif k == 'r':
                pkind.append(1)
                pref.append(v)
                blocks[v]["reps"].append(pi)
            else:   # RLC repair covering the last w sources departed so far
                pkind.append(1)
                pref.append(len(src_order))
        state["fifo_free"] = start + len(items) * step

    def charge_debt(n):
        state["tokens"] = max(-cap, state["tokens"] - n * PKT)

    def make_blocks_from(srcs_list, code="rs"):
        """balanced split of a source list into blocks of <= kmax; returns block ids."""
        m = len(srcs_list)
        if m == 0:
            return []
        nb = math.ceil(m / kmax)
        base, extra = divmod(m, nb)
        ids, pos = [], 0
        for j in range(nb):
            sz = base + (1 if j < extra else 0)
            ids.append(new_block(srcs_list[pos:pos + sz], code))
            pos += sz
        return ids

    def enqueue_unit(kind, unit, n, cls, t):
        sids = []
        for _ in range(n):
            sid = len(s_unit)
            s_unit.append(unit)
            s_kind.append(kind)
            s_cls.append(cls)
            s_pkt.append(-1)
            s_commit.append(t)
            sids.append(sid)
        unit_src[(kind, unit)] = sids
        if fam == "split":
            # split_sim.py: spec["plan"](k, rho) -> [(source positions, r, code)] per H.264 frame;
            # tile bursts get plain RS blocks of <= kmax. Repairs queue after the sources of the
            # unit, round-robin across blocks (so a loss burst on the tail hits different blocks).
            if kind == 'f':
                for s in sids:
                    queue.append(('s', s))
                bids, rs = [], []
                for idxs, r, code in spec["plan"](len(sids), rho):
                    bids.append(new_block([sids[i] for i in idxs], code))
                    rs.append(r)
                for j in range(max(rs, default=0)):
                    for bid, r in zip(bids, rs):
                        if j < r:
                            queue.append(('r', bid))
            else:
                m = len(sids)
                nb = math.ceil(m / kmax)
                base, extra = divmod(m, nb)
                pos = 0
                for j in range(nb):
                    sz = base + (1 if j < extra else 0)
                    part = sids[pos:pos + sz]
                    pos += sz
                    bid = new_block(part, "rs")
                    for s in part:
                        queue.append(('s', s))
                    for _ in range(nrep(sz)):
                        queue.append(('r', bid))
            return
        embed = (fam == "rq") or (fam == "perclass" and kind == 'f')
        if embed:
            code = "rq" if (fam == "rq" and not spec.get("mds")) else "rs"
            cap_k = spec.get("kcap", 10 ** 9) if fam == "rq" else kmax
            m = len(sids)
            nb = math.ceil(m / cap_k)
            base, extra = divmod(m, nb)
            pos = 0
            for j in range(nb):
                sz = base + (1 if j < extra else 0)
                part = sids[pos:pos + sz]
                pos += sz
                bid = new_block(part, code)
                for s in part:
                    queue.append(('s', s))
                for _ in range(nrep(sz)):
                    queue.append(('r', bid))
        else:
            for s in sids:
                queue.append(('s', s))

    def close_time_block(bid, t):
        n = nrep(blocks[bid]["k"])
        emit(t, [('r', bid)] * n)
        charge_debt(n)

    def do_pass(t):
        # refill
        dt = t - state["last"]
        state["tokens"] = min(cap, state["tokens"] + R * dt)
        state["last"] = t
        aff = int(state["tokens"] // PKT) if state["tokens"] > 0 else 0
        take = min(aff, gmax, len(queue))
        if take == 0:
            if queue:
                need = max(1.0, (PKT - state["tokens"]) / R)
                state["pass_t"] = max(state["fifo_free"], t + need)
            else:
                state["pass_t"] = None
            return
        items = [queue.popleft() for _ in range(take)]
        state["tokens"] -= take * PKT
        popped_src = [v for k, v in items if k == 's']
        out = items
        if fam == "rs_drain":
            ids = make_blocks_from(popped_src)
            out = list(items)
            for bid in ids:
                n = nrep(blocks[bid]["k"])
                out += [('r', bid)] * n
                charge_debt(n)
        elif fam == "rs_il":
            ids = make_blocks_from(popped_src)
            for bid in ids:
                staged.append(bid)
            D = spec["D"]
            if len(staged) >= D or not queue:
                grp, staged[:] = list(staged), []
                seqs = []
                for bid in grp:
                    n = nrep(blocks[bid]["k"])
                    charge_debt(n)
                    seqs.append([('s', s) for s in blocks[bid]["srcs"]] + [('r', bid)] * n)
                out = []
                for j in range(max((len(s) for s in seqs), default=0)):
                    for s in seqs:
                        if j < len(s):
                            out.append(s[j])
            else:
                out = []
        elif fam == "perclass" and spec.get("tile_fec"):
            tsrc = [s for s in popped_src if s_kind[s] == 'b']
            out = list(items)
            if tsrc:
                ids = make_blocks_from(tsrc)
                for bid in ids:
                    n = nrep(blocks[bid]["k"])
                    out += [('r', bid)] * n
                    charge_debt(n)
        elif fam == "rs_time":
            T = spec["T"]
            if T == "formula":
                # fec-v2-rev6.md section 4 rule 2: min(max(5 ms, 6 bundle slots), frame interval)
                T = min(max(5.0, 6.0 * pkt_ms), FRAME_MS)
            out = list(items)
            closed_now = []
            for s in popped_src:
                b = open_blk["b"]
                if b is None:
                    bid = new_block([], "rs")
                    blocks[bid]["open_t"] = t
                    open_blk["b"] = bid
                    heappush(timers, (t + T, bid))
                    b = bid
                blocks[b]["srcs"].append(s)
                blocks[b]["k"] += 1
                if blocks[b]["k"] >= kmax:
                    closed_now.append(b)
                    open_blk["b"] = None
            for bid in closed_now:
                n = nrep(blocks[bid]["k"])
                out += [('r', bid)] * n
                charge_debt(n)
                blocks[bid]["closed"] = True
            # rule 3, Empty: the content queue is empty at the end of the pass (the code's
            # Stop::Empty, as opposed to Tokens/Depth with content still queued). The open block
            # closes and its repairs go out in this drain, charged like sources.
            # spec["empty_kmin"]: do not close on Empty below this k (wait for k_max or T);
            # spec["empty_rmin"]: minimum repairs for an Empty-closed block.
            if spec.get("empty") and not queue and open_blk["b"] is not None:
                bid = open_blk["b"]
                if blocks[bid]["k"] >= spec.get("empty_kmin", 1):
                    n = max(spec.get("empty_rmin", rmin), nrep(blocks[bid]["k"]))
                    out += [('r', bid)] * n
                    charge_debt(n)
                    blocks[bid]["closed"] = True
                    blocks[bid]["by"] = "empty"
                    open_blk["b"] = None
        elif fam == "rlc":
            out = []
            for k, v in items:
                out.append((k, v))
                if k == 's':
                    state["credit"] += rho
                    state["since_rep"] += 1
                    if state["credit"] >= 1.0 - 1e-9:
                        state["credit"] -= 1.0
                        state["since_rep"] = 0
                        out.append(('x', 0))
                        charge_debt(1)
        emit_items = out
        if fam == "rlc":
            # 'x' repairs need the source count at their position: emit() computes it
            # from src_order, so items are emitted in order, one by one.
            start = max(t, state["fifo_free"])
            step = pkt_ms / BURST
            cnt = 0
            for k, v in emit_items:
                pi = len(dep)
                dep.append(start + cnt * step)
                cnt += 1
                if k == 's':
                    pkind.append(0)
                    pref.append(v)
                    s_pkt[v] = pi
                    src_order.append(v)
                else:
                    pkind.append(1)
                    pref.append(len(src_order))
            state["fifo_free"] = start + cnt * step
            if spec.get("flush") and not queue and state["since_rep"] > 0:
                # sender idle: emit the pending repair now (credit goes negative)
                pi = len(dep)
                dep.append(state["fifo_free"])
                pkind.append(1)
                pref.append(len(src_order))
                state["fifo_free"] += step
                # the flush may overdraw the repair credit by at most one repair; anything
                # beyond that is extra overhead (visible in the ovh column), not a debt that
                # would starve the next burst of its regular repairs
                state["credit"] = max(state["credit"] - 1.0, -1.0)
                state["since_rep"] = 0
                charge_debt(1)
        else:
            if emit_items:
                emit(t, emit_items)
        if queue:
            if state["tokens"] >= PKT:
                state["pass_t"] = state["fifo_free"]
            else:
                need = max(1.0, (PKT - state["tokens"]) / R)
                state["pass_t"] = max(state["fifo_free"], t + need)
        else:
            state["pass_t"] = None

    di = 0
    nd = len(demand)
    while True:
        cands = []
        if di < nd:
            cands.append((demand[di][0], 0))
        if state["pass_t"] is not None:
            cands.append((state["pass_t"], 2))
        if timers:
            cands.append((timers[0][0], 1))
        if not cands:
            # interleave staging / open time block leftovers cannot exist here: staging is
            # flushed when the queue empties, time blocks have timers.
            break
        t, which = min(cands)
        if which == 0:
            _, n, kind, unit, extra = demand[di]
            di += 1
            enqueue_unit(kind, unit, n, extra if kind == 'b' else -1, t)
            if state["pass_t"] is None:
                state["pass_t"] = max(t, state["fifo_free"])
        elif which == 1:
            tt, bid = heappop(timers)
            if open_blk["b"] == bid and not blocks[bid].get("closed"):
                open_blk["b"] = None
                blocks[bid]["closed"] = True
                close_time_block(bid, tt)
        else:
            do_pass(t)

    # link: FIFO bottleneck at C
    arr = []
    free = 0.0
    for d in dep:
        free = max(d, free) + pkt_ms
        arr.append(free)
    tl.dep, tl.arr, tl.pkind, tl.pref = dep, arr, pkind, pref
    tl.s_unit, tl.s_kind, tl.s_cls, tl.s_pkt, tl.blocks = s_unit, s_kind, s_cls, s_pkt, blocks
    tl.src_order, tl.unit_src = src_order, unit_src
    tl.fam, tl.spec = fam, spec
    tl.n_src = len(s_unit)
    tl.n_rep = len(dep) - sum(1 for x in s_pkt if x >= 0)
    tl.pkt_ms = pkt_ms
    tl.s_commit = s_commit
    return tl


# ------------------------------------------------------------------ receiver
def decode(tl, lost, rng):
    """Return avail[sid] (ms, link time excluding OWD) or None."""
    arr = tl.arr
    ns = len(tl.s_pkt)
    avail = [None] * ns
    for sid in range(ns):
        p = tl.s_pkt[sid]
        if p >= 0 and not lost[p]:
            avail[sid] = arr[p]
    fam = tl.fam
    if fam == "none":
        return avail
    if fam == "rlc":
        return decode_rlc(tl, lost, avail, rng)
    for b in tl.blocks:
        srcs = b["srcs"]
        k = len(srcs)
        if k == 0:
            continue
        miss = [s for s in srcs if avail[s] is None]
        if not miss:
            continue
        got = [p for p in (tl.s_pkt[s] for s in srcs) if p >= 0 and not lost[p]]
        got += [p for p in b["reps"] if not lost[p]]
        if len(got) < k:
            continue
        got.sort()
        if b["code"] == "rs":
            dec = arr[got[k - 1]]
        else:
            dec = None
            for m in range(k, len(got) + 1):
                e = m - k
                f = RQ_FAIL.get(e, 0.0)
                if f == 0.0 or rng.random() >= f:
                    dec = arr[got[m - 1]]
                    break
            if dec is None:
                continue
        for s in miss:
            avail[s] = dec
    return avail


def decode_rlc(tl, lost, avail, rng):
    w = tl.spec["w"]
    order = tl.src_order
    pos_of = {}
    for i, s in enumerate(order):
        pos_of[s] = i
    arr = tl.arr
    piv = {}                 # pivot var -> row dict (var -> coef), pivot coef 1
    unk = []                 # sorted order-index of lost, unsolved sources
    # walk packets in departure order
    dep_n = len(tl.dep)
    sidx = 0
    for p in range(dep_n):
        if tl.pkind[p] == 0:
            if lost[p]:
                unk.append(sidx)
            sidx += 1
            continue
        if lost[p]:
            continue
        cnt = tl.pref[p]
        lo = bisect_left(unk, cnt - w)
        hi = bisect_left(unk, cnt)
        if lo == hi:
            continue
        eq = {}
        for v in unk[lo:hi]:
            eq[v] = rng.randrange(1, 256)
        # reduce by existing pivots
        for c in [c for c in eq if c in piv]:
            a = eq.get(c, 0)
            if a:
                for col, val in piv[c].items():
                    nv = eq.get(col, 0) ^ gmul(a, val)
                    if nv:
                        eq[col] = nv
                    else:
                        eq.pop(col, None)
        if not eq:
            continue
        pc = min(eq)
        inv = ginv(eq[pc])
        eq = {col: gmul(val, inv) for col, val in eq.items()}
        for c, row in piv.items():
            b = row.get(pc, 0)
            if b:
                for col, val in eq.items():
                    nv = row.get(col, 0) ^ gmul(b, val)
                    if nv:
                        row[col] = nv
                    else:
                        row.pop(col, None)
        piv[pc] = eq
        solved = [c for c, row in piv.items() if len(row) == 1]
        for c in solved:
            del piv[c]
            i = bisect_left(unk, c)
            if i < len(unk) and unk[i] == c:
                unk.pop(i)
            avail[order[c]] = arr[p]
    return avail


# ------------------------------------------------------------------ metrics
def pct(v, q):
    if not v:
        return float("nan")
    return v[min(len(v) - 1, int(q * len(v)))]


def evaluate(tl, avail, lost, owd_list=OWDS):
    ns = tl.n_src
    arr, dep = tl.arr, tl.dep
    lost_src = 0
    unrec = 0
    abss = []
    waits, added, per_cls = [], [], {0: [], 1: [], 2: []}
    faster = {o: 0 for o in owd_list}
    arq_mean_acc = {o: 0.0 for o in owd_list}
    fec_mean_acc = {o: 0.0 for o in owd_list}
    best_acc = {o: 0.0 for o in owd_list}
    for s in range(ns):
        p = tl.s_pkt[s]
        if p < 0 or not lost[p]:
            continue
        lost_src += 1
        a = avail[s]
        if a is None:
            unrec += 1
            for o in owd_list:
                arq_w = 1.25 * (2 * o) + 5.0
                arq_mean_acc[o] += arq_w
                fec_mean_acc[o] += arq_w
                best_acc[o] += arq_w
            continue
        w = a - arr[p]                 # excess over the arrival the bundle would have had
        waits.append(w)
        abss.append(a - dep[p])
        if tl.s_kind[s] == 'b':
            per_cls[tl.s_cls[s]].append(w)
        for o in owd_list:
            arq_w = 1.25 * (2 * o) + 5.0       # RTT + RTT/4 + 5 ms, beyond the original arrival time
            if w <= arq_w:
                faster[o] += 1
            arq_mean_acc[o] += arq_w
            fec_mean_acc[o] += w
            best_acc[o] += min(w, arq_w)
    waits.sort()
    abss.sort()
    r = {
        "a50": pct(abss, 0.50), "a95": pct(abss, 0.95), "a99": pct(abss, 0.99),
        "n_src": ns, "n_lost": lost_src, "unrec": unrec,
        "pre_loss": lost_src / ns if ns else float("nan"),
        "resid": unrec / ns if ns else float("nan"),
        "arq_frac": unrec / lost_src if lost_src else float("nan"),
        "ovh": tl.n_rep / ns if ns else float("nan"),
        "w50": pct(waits, 0.50), "w95": pct(waits, 0.95), "w99": pct(waits, 0.99),
        "faster": {o: (faster[o] / lost_src if lost_src else float("nan")) for o in owd_list},
        "mean_wait": {o: (fec_mean_acc[o] / lost_src if lost_src else float("nan")) for o in owd_list},
        "mean_best": {o: (best_acc[o] / lost_src if lost_src else float("nan")) for o in owd_list},
        "mean_wait_arq": {o: (arq_mean_acc[o] / lost_src if lost_src else float("nan")) for o in owd_list},
    }
    r["cls"] = {c: (len(v), pct(sorted(v), 0.5), pct(sorted(v), 0.95), pct(sorted(v), 0.99))
                for c, v in per_cls.items()}
    # H.264 frames
    fr = []
    for (kind, unit), sids in tl.unit_src.items():
        if kind != 'f':
            continue
        first_dep = min(dep[tl.s_pkt[s]] for s in sids)
        full = max(arr[tl.s_pkt[s]] for s in sids)
        tail = 0.0
        bad = False
        for s in sids:
            a = avail[s]
            if a is None:
                bad = True
                break
            tail = max(tail, a)
        fr.append((first_dep, full, None if bad else tail))
    if fr:
        lat = sorted(t - f for f, _, t in fr if t is not None)
        extra = []
        lost_f = late33 = late66 = 0
        for f, full, t in fr:
            if t is None:
                lost_f += 1
                late33 += 1
                late66 += 1
            else:
                e = t - full
                extra.append(e)
                if e > FRAME_MS:
                    late33 += 1
                if e > 2 * FRAME_MS:
                    late66 += 1
        extra.sort()
        r["fr"] = {"n": len(fr), "lat50": pct(lat, 0.5), "lat99": pct(lat, 0.99),
                   "ex50": pct(extra, 0.5), "ex99": pct(extra, 0.99), "ex_max": extra[-1] if extra else float("nan"),
                   "late33": late33 / len(fr), "late66": late66 / len(fr), "lost": lost_f / len(fr)}
    else:
        r["fr"] = None
    return r


# ------------------------------------------------------------------ grid
def schemes(quick):
    S = []
    S.append(("none", {"fam": "none"}, [0.0]))
    kms = (20, 64)
    for km in kms:
        for rm in (1, 2):
            S.append((f"rs_drain k{km} rmin{rm}", {"fam": "rs_drain", "kmax": km, "rmin": rm}, "ovh"))
    for T in (2, 5, 10):
        for km in kms:
            S.append((f"rs_time T{T} k{km} rmin1", {"fam": "rs_time", "T": T, "kmax": km, "rmin": 1}, "ovh"))
    S.append((f"rs_time T5 k64 rmin2", {"fam": "rs_time", "T": 5, "kmax": 64, "rmin": 2}, "ovh"))
    for T in (5, 10):
        for rm in (1, 2):
            S.append((f"rs_time T{T} kfit rmin{rm}", {"fam": "rs_time", "T": T, "kfit": True, "rmin": rm}, "ovh"))
    for D in (2, 4):
        S.append((f"rs_il D{D} k20 rmin1", {"fam": "rs_il", "D": D, "kmax": 20, "rmin": 1}, "ovh"))
    S.append(("rq frame/burst rmin2", {"fam": "rq", "rmin": 2, "kcap": 10 ** 9}, "ovh"))
    S.append(("rq frame/burst cap256 rmin2", {"fam": "rq", "rmin": 2, "kcap": 256}, "ovh"))
    S.append(("rq frame/burst cap64 rmin2", {"fam": "rq", "rmin": 2, "kcap": 64}, "ovh"))
    for T in (25, 33):
        S.append((f"rs_time T{T} k64 rmin1", {"fam": "rs_time", "T": T, "kmax": 64, "rmin": 1}, "ovh"))
    S.append(("rs-mds frame/burst cap64 rmin1", {"fam": "rq", "mds": True, "rmin": 1, "kcap": 64}, "ovh"))
    S.append(("rs-mds frame/burst cap200 rmin1", {"fam": "rq", "mds": True, "rmin": 1, "kcap": 200}, "ovh"))
    for w in (16, 32, 64):
        for fl in (True, False):
            S.append((f"rlc w{w} {'flush' if fl else 'noflush'}", {"fam": "rlc", "w": w, "flush": fl}, "ovh"))
    S.append(("perclass: H264 per-frame RS, tiles ARQ", {"fam": "perclass", "kmax": 64, "rmin": 1, "tile_fec": False}, "ovh"))
    S.append(("perclass: H264 per-frame RS + tile RS k20", {"fam": "perclass", "kmax": 20, "rmin": 1, "tile_fec": True}, "ovh"))
    return S


def job(args):
    (rate, mix, name, spec, rho, n_src) = args
    seed = zlib.crc32(f"{rate}|{mix}".encode())
    rng = random.Random(seed)
    demand = gen_demand(mix, rate, n_src, rng)
    tl = build_timeline(demand, rate, spec, rho, random.Random(seed + 1))
    res = []
    n = len(tl.dep)
    conds = LOSSES + ([("loss0", 0.0, 1.0)] if name == "none" else [])
    for (lname, lr, lb) in conds:
        lrng = random.Random(zlib.crc32(f"{rate}|{mix}|{lname}".encode()))
        lost = ge_chain(n, lr, lb, lrng)
        drng = random.Random(zlib.crc32(f"d|{rate}|{mix}|{name}|{rho}|{lname}".encode()))
        avail = decode(tl, lost, drng)
        res.append((lname, evaluate(tl, avail, lost)))
    return (rate, mix, name, rho, res, tl.n_src, len(tl.dep))


def fmt(x, nd=1):
    if x is None or (isinstance(x, float) and math.isnan(x)):
        return "-"
    return f"{x:.{nd}f}"


def main():
    quick = "--quick" in sys.argv
    jobs = os.cpu_count() or 4
    if "--jobs" in sys.argv:
        jobs = int(sys.argv[sys.argv.index("--jobs") + 1])
    n_src = 12000 if quick else 60000
    rates = (2, 10, 30, 100)
    mixes = ("h264", "tiles", "mix")
    work = []
    for (name, spec, ov) in schemes(quick):
        rhos = [0.0] if ov == [0.0] else (0.1, 0.2, 0.3)
        for rho in rhos:
            for rate in rates:
                for mix in mixes:
                    work.append((rate, mix, name, spec, rho, n_src))
    print(f"{len(work)} timelines x {len(LOSSES)} loss conditions, {n_src} source bundles each, {jobs} workers",
          file=sys.stderr)
    here = os.path.dirname(os.path.abspath(__file__))
    with Pool(jobs) as pool:
        results = pool.map(job, work, chunksize=1)

    main_f = open(os.path.join(here, "uniform_sim_out.txt"), "w")
    fr_f = open(os.path.join(here, "uniform_sim_frames_out.txt"), "w")
    arq_f = open(os.path.join(here, "uniform_sim_arq_out.txt"), "w")
    cls_f = open(os.path.join(here, "uniform_sim_classes_out.txt"), "w")
    hdr = (f"# source bundles per config ~{n_src}; w = ms a lost bundle becomes usable AFTER its own would-have-been arrival (FEC delay; excludes OWD and queueing). ovh = repair bundles / source bundles actually sent.\n"
           f"# resid = unrecovered source bundles / source bundles; arq% = unrecovered / lost; "
           f"fast5/20/40 = share of lost bundles FEC delivered no later than an ARQ retransmission would (OWD 5/20/40 ms).\n")
    main_f.write(hdr)
    main_f.write(f"{'rate':>4} {'mix':<5} {'loss':<7} {'scheme':<42} {'rho':>4} {'ovh%':>6} {'pre%':>6} {'resid%':>8} "
                 f"{'arq%':>6} {'w50':>7} {'w95':>7} {'w99':>8} {'fast5':>6} {'fast20':>6} {'fast40':>6}\n")
    fr_f.write("# H.264 frames (mix h264 and mix): lat = first fragment departure -> all fragments usable (ms, excl OWD)\n"
               "# extra = that minus the loss-free completion; late33/66 = extra > 1 / 2 frame intervals, or never complete\n")
    fr_f.write(f"{'rate':>4} {'mix':<5} {'loss':<7} {'scheme':<42} {'rho':>4} {'ovh%':>6} {'frames':>6} {'lat50':>7} "
               f"{'lat99':>8} {'ex50':>6} {'ex99':>8} {'late33%':>8} {'late66%':>8} {'lost%':>7}\n")
    arq_f.write("# mean delivery wait (ms beyond OWD) of a lost bundle when unrecovered ones fall back to ARQ "
                "(excess wait = 1.25 RTT + 5 ms)\n# vs the pure ARQ baseline; one line per OWD\n")
    arq_f.write(f"{'rate':>4} {'mix':<5} {'loss':<7} {'scheme':<42} {'rho':>4} {'owd':>4} {'mean_fec+arq':>13} {'mean_min(fec,arq)':>18} {'mean_arq':>9}\n")
    cls_f.write("# tile mix: wait p50/p95/p99 (ms beyond OWD) of recovered lost bundles, by burst size class "
                "(c0 1-6 bundles, c1 7-60, c2 >60)\n")
    cls_f.write(f"{'rate':>4} {'loss':<7} {'scheme':<42} {'rho':>4} " + " ".join(
        f"{'c'+str(c)+' n':>6} {'p50':>7} {'p95':>7} {'p99':>8}" for c in range(3)) + "\n")

    for (rate, mix, name, rho, res, ns, npk) in results:
        for lname, r in res:
            main_f.write(f"{rate:>4} {mix:<5} {lname:<7} {name:<42} {rho:>4.1f} {100*r['ovh']:>6.1f} {100*r['pre_loss']:>6.2f} "
                         f"{100*r['resid']:>8.4f} {100*r['arq_frac'] if r['arq_frac']==r['arq_frac'] else float('nan'):>6.1f} "
                         f"{fmt(r['w50']):>7} {fmt(r['w95']):>7} {fmt(r['w99']):>8} "
                         f"{fmt(100*r['faster'][5.0],0):>6} {fmt(100*r['faster'][20.0],0):>6} {fmt(100*r['faster'][40.0],0):>6}\n")
            f = r["fr"]
            if f:
                fr_f.write(f"{rate:>4} {mix:<5} {lname:<7} {name:<42} {rho:>4.1f} {100*r['ovh']:>6.1f} {f['n']:>6} "
                           f"{fmt(f['lat50']):>7} {fmt(f['lat99']):>8} {fmt(f['ex50']):>6} {fmt(f['ex99']):>8} "
                           f"{100*f['late33']:>8.2f} {100*f['late66']:>8.2f} {100*f['lost']:>7.2f}\n")
            if mix == "tiles" or mix == "mix":
                for o in OWDS:
                    if mix == "tiles":
                        arq_f.write(f"{rate:>4} {mix:<5} {lname:<7} {name:<42} {rho:>4.1f} {o:>4g} "
                                    f"{fmt(r['mean_wait'][o]):>13} {fmt(r['mean_best'][o]):>18} {fmt(r['mean_wait_arq'][o]):>9}\n")
            if mix == "tiles":
                cs = r["cls"]
                cls_f.write(f"{rate:>4} {lname:<7} {name:<42} {rho:>4.1f} " + " ".join(
                    f"{cs[c][0]:>6} {fmt(cs[c][1]):>7} {fmt(cs[c][2]):>7} {fmt(cs[c][3]):>8}" for c in range(3)) + "\n")
    for f in (main_f, fr_f, arq_f, cls_f):
        f.close()


if __name__ == "__main__":
    main()
