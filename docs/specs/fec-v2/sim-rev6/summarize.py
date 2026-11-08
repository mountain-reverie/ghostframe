#!/usr/bin/env python3
"""Condense the raw *_out.txt tables into the views FINDINGS.md quotes.

Run: python3 -I summarize.py > summary_out.txt     (after uniform_sim.py)
"""
import os

HERE = os.path.dirname(os.path.abspath(__file__))


def parse(fn, ncols_after_scheme):
    out = []
    for l in open(os.path.join(HERE, fn)):
        if l.startswith('#') or l.lstrip().startswith('rate'):
            continue
        rate, mix, loss = l.split()[:3]
        i = 19
        sch = l[i:i + 42].strip()
        nums = l[i + 43:].split()
        out.append((int(rate), mix, loss, sch, nums))
    return out


MAIN = {}
for rate, mix, loss, sch, n in parse("uniform_sim_out.txt", 0):
    MAIN[(rate, mix, loss, sch, float(n[0]))] = dict(
        ovh=float(n[1]), pre=float(n[2]), resid=float(n[3]),
        arq=float(n[4]) if n[4] != '-' else float('nan'),
        w50=n[5], w95=n[6], w99=n[7], f5=n[8], f20=n[9], f40=n[10])
FR = {}
for rate, mix, loss, sch, n in parse("uniform_sim_frames_out.txt", 0):
    FR[(rate, mix, loss, sch, float(n[0]))] = dict(
        n=int(n[2]), lat50=n[3], lat99=n[4], ex50=n[5], ex99=n[6],
        l33=float(n[7]), l66=float(n[8]), lost=float(n[9]))
ARQ = {}
for rate, mix, loss, sch, n in parse("uniform_sim_arq_out.txt", 0):
    ARQ[(rate, mix, loss, sch, float(n[0]), float(n[1]))] = (n[2], n[3], n[4])

RATES = (2, 10, 30, 100)


def row(rate, mix, loss, sch, rho):
    m = MAIN.get((rate, mix, loss, sch, rho))
    return m


def p(s):
    print(s)


def table_scheme(title, schemes, mixes, loss, rhos=(0.1, 0.2, 0.3), cols=("ovh", "resid", "arq", "w50", "w95", "w99")):
    p(f"\n### {title}  [loss {loss}]")
    hdr = f"{'scheme':<42} {'rho':>4}"
    for rate in RATES:
        hdr += " | " + f"{rate:>3}M " + " ".join(f"{c:>6}" for c in cols)
    for mix in mixes:
        p(f"-- mix {mix}")
        p(hdr)
        for sch in schemes:
            for rho in rhos:
                line = f"{sch:<42} {rho:>4.1f}"
                for rate in RATES:
                    m = row(rate, mix, loss, sch, rho)
                    if m is None:
                        line += " | " + " " * 5 + " ".join(f"{'?':>6}" for _ in cols)
                        continue
                    vals = []
                    for c in cols:
                        v = m[c]
                        vals.append(f"{v:>6.2f}" if isinstance(v, float) else f"{v:>6}")
                    line += " | " + " " * 5 + " ".join(vals)
                p(line)


def table_frames(title, schemes, mixes, loss, rhos=(0.1, 0.2, 0.3)):
    p(f"\n### {title}  [loss {loss}]  (H.264 frames: late33%/late66%/never%, lat50/lat99 abs excl OWD)")
    for mix in mixes:
        p(f"-- mix {mix}")
        hdr = f"{'scheme':<42} {'rho':>4}"
        for rate in RATES:
            hdr += f" | {rate:>3}M ovh%  l33  l66 lost lat50 lat99"
        p(hdr)
        for sch in schemes:
            for rho in rhos:
                line = f"{sch:<42} {rho:>4.1f}"
                for rate in RATES:
                    f = FR.get((rate, mix, loss, sch, rho))
                    m = MAIN.get((rate, mix, loss, sch, rho))
                    if f is None:
                        line += " | ?"
                        continue
                    line += f" |      {m['ovh']:>4.0f} {f['l33']:>4.1f} {f['l66']:>4.1f} {f['lost']:>4.1f} {f['lat50']:>5} {f['lat99']:>5}"
                p(line)


def main():
    p("# Condensed views of uniform_sim output (all numbers model estimates)")
    base = "none"
    p("\n### Baselines: 'none' (no FEC), pre-FEC loss = residual before ARQ")
    for mix in ("h264", "tiles", "mix"):
        for loss in ("iid1", "iid2", "ge2b3"):
            line = f"{mix:<5} {loss:<6}"
            for rate in RATES:
                m = row(rate, mix, loss, base, 0.0)
                f = FR.get((rate, mix, loss, base, 0.0))
                line += f" | {rate:>3}M pre {m['pre']:.2f}%"
                if f:
                    line += f" frames late33 {100*0+f['l33']:.1f}%"
            p(line)
    p("\nLoss-free frame completion latency (ms from first fragment departure; includes frame serialisation + standing queue):")
    for mix in ("h264", "mix"):
        for rate in RATES:
            f = FR.get((rate, mix, "loss0", base, 0.0))
            p(f"  {mix:<5} {rate:>3}M frames {f['n']:>6}  p50 {f['lat50']:>6}  p99 {f['lat99']:>6}")

    rsd = ["rs_drain k20 rmin1", "rs_drain k20 rmin2", "rs_drain k64 rmin1", "rs_drain k64 rmin2"]
    rst = ["rs_time T2 k20 rmin1", "rs_time T2 k64 rmin1", "rs_time T5 k20 rmin1", "rs_time T5 k64 rmin1",
           "rs_time T5 k64 rmin2", "rs_time T10 k20 rmin1", "rs_time T10 k64 rmin1"]
    il = ["rs_il D2 k20 rmin1", "rs_il D4 k20 rmin1"]
    kf = ["rs_time T5 kfit rmin1", "rs_time T10 kfit rmin1", "rs_time T5 kfit rmin2", "rs_time T10 kfit rmin2",
          "rs_time T25 k64 rmin1", "rs_time T33 k64 rmin1"]
    rq = ["rs-mds frame/burst cap64 rmin1", "rs-mds frame/burst cap200 rmin1", "rq frame/burst rmin2", "rq frame/burst cap256 rmin2", "rq frame/burst cap64 rmin2"]
    rlc = [f"rlc w{w} {f}" for w in (16, 32, 64) for f in ("flush", "noflush")]
    pc = ["perclass: H264 per-frame RS, tiles ARQ", "perclass: H264 per-frame RS + tile RS k20"]

    for loss in ("iid2", "iid1", "iid5", "ge2b3", "ge2b8"):
        table_scheme("Q1 RS closed at end of each drain", rsd, ("h264", "tiles"), loss)
    for loss in ("iid2", "ge2b3"):
        table_scheme("Q2 RS closed by time cap", rst, ("h264", "tiles"), loss)
        table_scheme("Q2c time cap with k fitted to the overhead (kfit) and long caps", kf, ("h264", "tiles"), loss)
        table_scheme("Q2b interleave", il, ("h264", "tiles"), loss)
        table_scheme("Q3 RaptorQ per frame / burst", rq, ("h264", "tiles"), loss)
        table_scheme("Q4 sliding-window RLC", rlc, ("h264", "tiles"), loss)
        table_scheme("Per-class (rev 5 style) designs", pc, ("h264", "tiles", "mix"), loss)
    for loss in ("iid2", "ge2b3", "iid5"):
        allh = rsd + rst + kf + il + rq + rlc + pc
        table_frames("H.264 frames, every scheme", ["none"] + allh, ("h264",), loss, rhos=(0.0, 0.1, 0.2, 0.3))

    p("\n### RLC idle flush: tile traffic tail (mix tiles), rho 0.2: resid%, arq%, w95, w99, share of lost bundles delivered no later than ARQ at OWD 20 ms")
    for loss in ("iid1", "iid2", "ge2b3"):
        for sch in ("rlc w16 flush", "rlc w16 noflush", "rlc w32 flush", "rlc w32 noflush", "rlc w64 flush", "rlc w64 noflush"):
            line = f"{loss:<6} {sch:<18}"
            for rate in RATES:
                m = row(rate, "tiles", loss, sch, 0.2)
                line += f" | {rate:>3}M ovh {m['ovh']:>4.1f} res {m['resid']:>5.2f} arq {m['arq']:>4.1f} w95 {m['w95']:>6} w99 {m['w99']:>7} f20 {m['f20']:>4}"
            p(line)
    p("\n### Tile ARQ comparison at OWD 20 ms: mean wait of a lost bundle beyond its own arrival (ms), FEC with ARQ for the unrecovered / min(FEC, ARQ) / pure ARQ (= 1.25 RTT + 5 = 55)  [tiles, rho 0.2]")
    for loss in ("iid2", "ge2b3"):
        for sch in ("rs_drain k64 rmin1", "rs_time T5 k64 rmin1", "rs_time T10 kfit rmin1", "rs-mds frame/burst cap64 rmin1", "rq frame/burst cap64 rmin2", "rlc w32 flush", "rlc w32 noflush"):
            line = f"{loss:<6} {sch:<34}"
            for rate in RATES:
                a = ARQ.get((rate, "tiles", loss, sch, 0.2, 20.0))
                line += f" | {rate:>3}M {a[0]:>7}/{a[1]:>6}/{a[2]:>5}" if a else " | ?"
            p(line)
    p("\n### Tile first paint (mix tiles, class c2 = bursts of > 60 bundles): recovered-bundle wait p50/p95/p99 (ms)")
    for l in open(os.path.join(HERE, "uniform_sim_classes_out.txt")):
        if l.startswith('#') or l.lstrip().startswith('rate'):
            continue
        parts = l.split()
        rate, loss = parts[0], parts[1]
        if loss != "iid2" or (rate not in ("2", "30")):
            continue
        sch = l[13:55].strip()
        rest = l[56:].split()
        rho = rest[0]
        if rho not in ("0.2",):
            continue
        p(f"{rate:>4}M {sch:<42} rho {rho}  c0 n={rest[1]} {rest[2]}/{rest[3]}/{rest[4]}  c1 n={rest[5]} {rest[6]}/{rest[7]}/{rest[8]}  c2 n={rest[9]} {rest[10]}/{rest[11]}/{rest[12]}")


if __name__ == "__main__":
    main()
