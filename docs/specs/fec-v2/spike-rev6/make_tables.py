"""Turn results/*.tsv into the markdown tables used in RESULTS.md (written to results/tables.md)."""
import collections
R = "results/"
out = []
def table(title, head, rows):
    out.append(f"\n**{title}**\n")
    out.append("| " + " | ".join(head) + " |")
    out.append("|" + "---|" * len(head))
    for r in rows: out.append("| " + " | ".join(str(x) for x in r) + " |")

d = {}
for l in open(R + "B_native_speed.tsv"):
    p = l.rstrip("\n").split("\t")
    d[(p[0], p[1].split("(")[0], int(p[2]), int(p[3]), int(p[4]))] = float(p[5])
ks = [1, 2, 4, 10, 20, 40, 64, 128, 200]
f = lambda x: "-" if x is None else (f"{x:.2f}" if x < 10 else f"{x:.1f}" if x < 100 else f"{x:.0f}")
for c, label in [("rusty-ours", "rusty_erasure, our XOR-first matrix"), ("rusty-cauchy", "rusty_erasure, built-in Cauchy"), ("gf256-scalar", "in-house gf256 scalar oracle"), ("raptorq", "raptorq 2.0.1")]:
    table(f"Native encode, us (median of 200), {label}", ["r \\ k"] + [str(k) for k in ks],
          [[f"r={r}"] + [f(d.get((c, "encode", k, r, 0))) for k in ks] for r in [1, 2, 4, 8]])
for lost, name in [(1, "1 loss"), ("r", "r losses (first r sources lost, all r repairs present)")]:
    rows = []
    for c, op, label in [("rusty-ours", "decode_warm", "rusty ours, `recover`, coder cached"), ("rusty-ours", "decode_cold", "rusty ours, `recover`, matrix+coder built per block"),
                         ("rusty-ours", "decode_ee_cold", "rusty ours, e x e solve + rusty kernel (cold)"), ("rusty-cauchy", "decode_warm", "rusty built-in Cauchy, `recover`, cached"),
                         ("gf256-scalar", "decode", "gf256 scalar oracle"), ("raptorq", "decode", "raptorq")]:
        for r in ([2] if lost == 1 else [2, 8]):
            rows.append([label, f"r={r}"] + [f(d.get((c, op, k, r, 1 if lost == 1 else r))) for k in ks])
    if lost == 1:
        rows.append(["rusty ours, plain XOR of survivors (`raid::xor_gen`)", "r>=1"] + [f(d.get(("rusty-ours", "decode_xor_fastpath", k, 2, 1))) for k in ks])
    table(f"Native decode, us (median of 200), {name}", ["candidate", "r"] + [str(k) for k in ks], rows)

rows = []
for l in list(open(R + "D_wasm_summary.tsv"))[1:]:
    p = l.rstrip("\n").split("\t")
    rows.append(p)
lab = {("rusty-scalar", "0"): "rusty ours, recover (scalar)", ("rusty-simd128", "0"): "rusty ours, recover (simd128)",
       ("rusty-scalar", "1"): "rusty built-in Cauchy (scalar)", ("rusty-simd128", "1"): "rusty built-in Cauchy (simd128)",
       ("rusty-scalar", "2"): "rusty ours, e x e (scalar)", ("rusty-simd128", "2"): "rusty ours, e x e (simd128)",
       ("gf", "0"): "gf256 scalar", ("gf-simd128", "0"): "gf256 scalar code, +simd128 (autovec)", ("rq", "0"): "raptorq", ("rq-simd128", "0"): "raptorq, +simd128",
       ("rlc", "0"): "cu-fec window=k", ("rlc-simd128", "0"): "cu-fec window=k, +simd128", ("base", "0"): "baseline byte-loop XOR (reference)"}
order = list(lab)
by = collections.defaultdict(dict)
for p in rows: by[(p[0], p[1])][(int(p[2]), int(p[3]), int(p[4]))] = p
cfgs = [(10, 2, 1), (10, 2, 2), (10, 8, 8), (40, 2, 1), (40, 8, 1), (40, 8, 8), (200, 2, 1), (200, 8, 1), (200, 8, 8)]
trs = []
for key in order:
    if key not in by: continue
    r = [lab[key]]
    for c in cfgs:
        p = by[key].get(c)
        if p is None: r.append("-"); continue
        lo, hi = p[7].split("..") if ".." in p[7] else p[7].split("-")
        g = lambda v: f"{float(v):.1f}" if float(v) < 10 else f"{float(v):.0f}"
        r.append(f"{g(lo)} [{g(hi)}]")
    trs.append(r)
table("Wasm (Node 26 / V8) decode, us: best of 3 runs of the per-run median [slowest of the 3 run medians], 400 timed iterations per run after 50 warm-up",
      ["candidate"] + [f"k={k} r={r} lost={l}" for k, r, l in cfgs], trs)
bad = [p for p in rows if p[-1] != "ok"]
out.append(f"\nwasm correctness flag: {len(rows) - len(bad)} of {len(rows)} configs restored the exact bytes.")

rq = [l.rstrip("\n").split("\t") for l in open(R + "3_raptorq_overhead.tsv")][1:]
rows = collections.defaultdict(dict)
for s, k, n, t, fl, rate in rq: rows[(s, int(k))][int(n) - int(k)] = f"{fl}/{t}"
table("RaptorQ decode failures given exactly n received symbols (4000 trials each)", ["scenario", "k", "n=k", "n=k+1", "n=k+2"],
      [[s, k, v[0], v[1], v[2]] for (s, k), v in sorted(rows.items())])
open(R + "tables.md", "w").write("\n".join(out) + "\n")
