#!/usr/bin/env python3
"""Reads adapt_sim_out.txt, prints the comparisons FINDINGS.md section 8 quotes (python3 -I adapt_summary.py).
A difference is called real only when it exceeds the larger of the two cells' seed ranges (max - min)."""
import collections
import os
import re
import sys

here = os.path.dirname(os.path.abspath(__file__))
PAT = re.compile(r"^(\w+)\s+\(([^)]*)\)\s+(\d+)M (\w+)\s+(\S+)\s+late33\s+([\d.]+)\(\s*([\d.]+)-\s*([\d.]+)\) late66\s+([\d.]+)\(\s*([\d.]+)-\s*([\d.]+)\) "
                 r"resid\s+([\d.]+)\(\s*[\d.]+-\s*[\d.]+\) tw95\s+(\S+)\(\s*\S+-\s*\S+\) ovh\s+([\d.]+)\(\s*([\d.]+)-\s*([\d.]+)\)")
D = {}
for line in open(os.path.join(here, "adapt_sim_out.txt")):
    m = PAT.match(line)
    if not m:
        continue
    part, key, rate, mix, loss = m.group(1), tuple(x.strip().strip("'") for x in m.group(2).split(",")), int(m.group(3)), m.group(4), m.group(5)
    v = [float(m.group(i)) for i in (6, 7, 8, 9, 12, 14, 15, 16)]
    tw = float(m.group(13)) if m.group(13) != "nan" else float("nan")
    D[(part, key, rate, mix, loss)] = dict(l33=v[0], l33lo=v[1], l33hi=v[2], l66=v[3], res=v[4], tw=tw, ovh=v[5], olo=v[6], ohi=v[7])


def rng(c):
    return c["l33hi"] - c["l33lo"]


def verdict(a, b):
    """a vs b late33: 'a<b', 'a>b' only when the gap exceeds both ranges, else '~'."""
    d = a["l33"] - b["l33"]
    s = max(rng(a), rng(b))
    return "~" if abs(d) <= s else ("a<b" if d < 0 else "a>b")


def part_cap():
    print("== (i) T=20 against T=25/28/33 (late33), per k_max; counts over rate x mix x loss cells")
    for rates, name in (((2, 10, 30), "<= 30 Mbit/s"), ((100, 200), "100-200 Mbit/s")):
        for km in ("64", "128", "223"):
            c = collections.Counter()
            worse = []
            for rate in rates:
                for mix in ("h264", "mix"):
                    for loss in ("ge2b3", "ge2b8"):
                        a = D.get(("cap", ("20", km), rate, mix, loss))
                        for T in ("25", "28", "33"):
                            b = D.get(("cap", (T, km), rate, mix, loss))
                            if a and b:
                                v = verdict(a, b)
                                c[(T, v)] += 1
                                if v == "a>b":
                                    worse.append((rate, mix, loss, T, round(a["l33"] - b["l33"], 2)))
            print(f"  {name} k{km}: " + ", ".join(f"T{T}: better {c[(T,'a<b')]} / same {c[(T,'~')]} / worse {c[(T,'a>b')]}" for T in ("25", "28", "33")))
            for w in worse:
                print("     T20 worse:", w)
    print("== (iii) 2 and 10 Mbit/s, k64: T20 vs T28: late33 and overhead")
    for rate in (2, 10):
        for mix in ("h264", "mix"):
            for loss in ("ge2b3", "ge2b8"):
                a, b = D.get(("cap", ("20", "64"), rate, mix, loss)), D.get(("cap", ("28", "64"), rate, mix, loss))
                if a and b:
                    print(f"  {rate}M {mix} {loss}: T20 late33 {a['l33']:.2f} ({a['l33lo']:.2f}-{a['l33hi']:.2f}) ovh {a['ovh']:.1f} | "
                          f"T28 late33 {b['l33']:.2f} ({b['l33lo']:.2f}-{b['l33hi']:.2f}) ovh {b['ovh']:.1f}  [{verdict(a,b)}]")


def part_best():
    print("== (ii) per cell: best config (min late33 mean), the indistinguishable set, cheapest member (lowest T, then k_max)")
    for rate in (2, 10, 30, 100, 200):
        for mix in ("h264", "mix"):
            for loss in ("ge2b3", "ge2b8"):
                cs = {k[1]: c for k, c in D.items() if k[0] == "cap" and k[2] == rate and k[3] == mix and k[4] == loss}
                if not cs:
                    continue
                kb = min(cs, key=lambda k: cs[k]["l33"])
                best = cs[kb]
                same = [k for k, c in cs.items() if verdict(c, best) == "~" or c is best]
                cheap = min(same, key=lambda k: (float(k[0]), int(k[1])))
                print(f"  {rate:>3}M {mix:<4} {loss}: best T{kb[0]}/k{kb[1]} late33 {best['l33']:.2f} ({best['l33lo']:.2f}-{best['l33hi']:.2f}) ovh {best['ovh']:.1f}; "
                      f"{len(same)}/{len(cs)} indistinguishable; cheapest T{cheap[0]}/k{cheap[1]} late33 {cs[cheap]['l33']:.2f} ovh {cs[cheap]['ovh']:.1f}")


def part_ladder():
    print("== ladder cells: notch late33 mean (range), ovh; first notch indistinguishable from the best")
    for rate in (10, 30, 100):
        for p in ("0.005", "0.02", "0.05"):
            for b in ("1.0", "3.0", "8.0"):
                loss = f"p{p}b{b}"
                cs = {k[1][0]: c for k, c in D.items() if k[0] == "lad" and k[2] == rate and k[4] == loss}
                if not cs:
                    continue
                best = min(cs.values(), key=lambda c: c["l33"])
                first = next(n for n in ("n0", "n1", "n2a", "n2") if verdict(cs[n], best) == "~" or cs[n] is best)
                print(f"  {rate:>3}M p{p} b{b}: " + " ".join(f"{n} {cs[n]['l33']:5.2f}({rng(cs[n]):4.2f}) o{cs[n]['ovh']:4.1f}" for n in ("n0", "n1", "n2a", "n2")) + f"  -> {first}")


def part_split():
    print("== (iv) split gate: late33 mean (range) ovh; T20 single block vs a_il at rho 0.1/0.2/0.3")
    for rate in (100, 200):
        for mix in ("h264", "mix"):
            for loss in ("ge2b3", "ge2b8"):
                row = []
                for tag, key in (("T20k128", ("T20", "128")), ("T20k223", ("T20", "223")), ("a_il.1", ("a_il", "0.1")), ("a_il.2", ("a_il", "0.2")), ("a_il.3", ("a_il", "0.3"))):
                    c = D.get(("split", key, rate, mix, loss))
                    if c:
                        row.append(f"{tag} {c['l33']:5.2f}({rng(c):4.2f}) o{c['ovh']:4.1f}")
                print(f"  {rate}M {mix:<4} {loss}: " + " | ".join(row))


def part_low():
    print("== low rate (10 seeds): T20 vs T28, small-k cap 32 (plan) / 3 / 1 : late33 mean(range) ovh mean(range)")
    for rate in (2, 10):
        for mix in ("h264", "mix"):
            for loss in ("iid2", "ge2b3", "ge2b8"):
                row = []
                for T in ("20", "28"):
                    for cap in ("32", "3", "1"):
                        c = D.get(("low", (T, cap), rate, mix, loss))
                        if c:
                            row.append(f"T{T}c{cap} {c['l33']:4.1f}({rng(c):3.1f}) o{c['ovh']:5.1f}({c['olo']:.0f}-{c['ohi']:.0f})")
                print(f"  {rate}M {mix:<4} {loss:<5}: " + " | ".join(row))


if __name__ == "__main__":
    part_cap(); part_best(); part_ladder(); part_split(); part_low()
