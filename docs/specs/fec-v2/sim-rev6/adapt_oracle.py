#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Exact tail oracle for FEC v2 R6-e (plan task E1; E4's r_for(k) is tested against it).

tail(n, r, p, b) = P(more than r of the n datagrams of one block are lost), where the n positions
are consecutive datagrams of one loss process:
  b <= 1.0  i.i.d. loss p (binomial tail, closed form)
  b  > 1.0  two-state Gilbert-Elliott, loss 1 in the bad state, 0 in the good, stationary start;
            P(bad -> good) = 1/b (mean bad run b), P(good -> bad) = p/(b(1-p)), so P(bad) = p.
The GE tail is an exact dynamic program over positions: state (bad?, number lost so far).

r_for(k, p, b, target, r_cap=32, small_k_cap=32) = smallest r >= 1 with tail(k + r, r) <= target,
under r <= max(1, ceil(k/4)) for k >= 4 (small_k_cap for k < 4) and r <= r_cap; if the target is not
reachable inside the cap, the cap.

Run: python3 -I adapt_oracle.py --selftest | --vectors   (--vectors rewrites adapt_oracle_vectors.csv)
"""
import functools
import math
import os
import random
import sys

P_LIST = (0.001, 0.005, 0.01, 0.02, 0.05, 0.1)
B_LIST = (1.0, 1.5, 3.0, 8.0)
N_LIST = (4, 10, 24, 64, 128, 223)
R_MAX = 32


def count_dist(n, p, b, start_bad=None):
    """Distribution of the number of losses among n consecutive datagrams (list, index = count)."""
    if p <= 0:
        return [1.0] + [0.0] * n
    if b <= 1.0:
        q = 1.0 - p
        d = [1.0]
        for _ in range(n):
            nd = [0.0] * (len(d) + 1)
            for j, x in enumerate(d):
                nd[j] += x * q
                nd[j + 1] += x * p
            d = nd
        return d
    p_bg = 1.0 / b
    p_gb = p * p_bg / (1.0 - p)
    assert p_gb <= 1.0
    pb0 = p if start_bad is None else (1.0 if start_bad else 0.0)
    # state after position i: g[j] = P(good now, j lost), s[j] = P(bad now, j lost)
    g = [1.0 - pb0] + [0.0] * n
    s = [0.0] * (n + 1)
    # position 0: the state at position 0 is the start state; a bad position loses its datagram
    s[1] = pb0
    g[0] = 1.0 - pb0
    for _ in range(1, n):
        ng = [0.0] * (n + 1)
        ns = [0.0] * (n + 1)
        for j in range(n + 1):
            gj, sj = g[j], s[j]
            if gj == 0.0 and sj == 0.0:
                continue
            # next position good
            ng[j] += gj * (1.0 - p_gb) + sj * p_bg
            # next position bad: one more loss
            if j + 1 <= n:
                ns[j + 1] += gj * p_gb + sj * (1.0 - p_bg)
        g, s = ng, ns
    return [g[j] + s[j] for j in range(n + 1)]


def tail_from_dist(d, r):
    return math.fsum(d[r + 1:]) if r + 1 < len(d) else 0.0


def tail(n, r, p, b):
    return tail_from_dist(count_dist(n, p, b), r)


def binom_tail(n, r, p):
    return math.fsum(math.comb(n, j) * p ** j * (1 - p) ** (n - j) for j in range(r + 1, n + 1))


@functools.lru_cache(maxsize=None)
def r_for(k, p, b, target=1e-3, r_cap=R_MAX, small_k_cap=R_MAX):
    cap = small_k_cap if k < 4 else max(1, math.ceil(k / 4))
    cap = min(cap, r_cap, 255 - k)
    for r in range(1, cap + 1):
        if tail(k + r, r, p, b) <= target:
            return r
    return cap


def vectors(path):
    with open(path, "w") as f:
        f.write("n,r,p,mean_burst,tail\n")
        for p in P_LIST:
            for b in B_LIST:
                for n in N_LIST:
                    d = count_dist(n, p, b)
                    for r in range(1, R_MAX + 1):
                        f.write(f"{n},{r},{p},{b},{tail_from_dist(d, r):.17g}\n")


def mc_tail(n, p, b, rs, trials, rng):
    """Monte Carlo counts: fraction of blocks with more than r losses, for each r in rs."""
    hits = {r: 0 for r in rs}
    p_bg = 1.0 / b if b > 1 else 1.0
    p_gb = p * p_bg / (1.0 - p) if b > 1 else p
    for _ in range(trials):
        bad = rng.random() < p
        c = 0
        for i in range(n):
            if b <= 1.0:
                lost = rng.random() < p
            else:
                if i:
                    bad = (rng.random() < p_gb) if not bad else (rng.random() >= p_bg)
                lost = bad
            c += lost
        for r in rs:
            if c > r:
                hits[r] += 1
    return {r: hits[r] / trials for r in rs}


def mc_check(dist_fn, trials=40000):
    """True if dist_fn agrees with Monte Carlo (5 sigma) on cases where the start state matters."""
    rng = random.Random(11)
    for (n, p, b, rs) in ((4, 0.1, 8.0, (0, 1, 2)), (10, 0.05, 3.0, (0, 1, 2)), (24, 0.1, 1.5, (1, 3, 5)),
                          (24, 0.02, 8.0, (0, 1, 2, 4))):
        mc = mc_tail(n, p, b, rs, trials, rng)
        d = dist_fn(n, p, b)
        for r in rs:
            t = tail_from_dist(d, r)
            se = math.sqrt(max(t * (1 - t), 1.0 / trials) / trials)
            if abs(mc[r] - t) > 5 * se + 1e-4:
                return False
    return True


def selftest():
    # 1. i.i.d. closed form against the binomial tail (the b <= 1 branch and b = 1 through DP-free path)
    for n in N_LIST:
        for p in P_LIST:
            for r in (1, 3, 10):
                a, c = tail(n, r, p, 1.0), binom_tail(n, r, p)
                assert abs(a - c) <= 1e-12 * max(c, 1e-30) + 1e-300, (n, r, p, a, c)
    # 2. GE: distribution sums to 1, mean count is n p (stationary start), 
    for n in (4, 64, 223):
        for p in P_LIST:
            for b in (1.5, 3.0, 8.0):
                d = count_dist(n, p, b)
                assert abs(sum(d) - 1) < 1e-12
                assert abs(sum(j * x for j, x in enumerate(d)) - n * p) < 1e-9 * max(1, n * p)
    # 3. DP against Monte Carlo; a DP that starts in the good state (non-stationary) must fail it
    assert mc_check(count_dist), "the DP disagrees with Monte Carlo"
    assert not mc_check(lambda n, p, b: count_dist(n, p, b, start_bad=False)), "a wrong DP passed the check"
    # 4. a burst cannot be repaired by fewer parities than its length: monotone properties
    for p in P_LIST:
        for b in B_LIST:
            for n in (10, 64):
                ts = [tail(n, r, p, b) for r in range(1, 20)]
                assert all(x >= y for x, y in zip(ts, ts[1:])), (p, b, n)
    for p in (0.01, 0.05):
        assert tail(64, 4, p, 8.0) > tail(64, 4, p, 1.0), "bursts must raise the tail"
    # 5. r_for: smallest r meeting the target (r-1 does not), and non-zero
    for (k, p, b) in ((10, 0.02, 1.0), (64, 0.05, 3.0), (128, 0.02, 8.0)):
        r = r_for(k, p, b, 1e-3)
        capped = r == min(max(1, math.ceil(k / 4)), R_MAX)
        assert r >= 1 and (capped or tail(k + r, r, p, b) <= 1e-3), (k, p, b, r)
        assert r == 1 or tail(k + r - 1, r - 1, p, b) > 1e-3, (k, p, b, r)
    print("adapt_oracle selftest ok")


if __name__ == "__main__":
    here = os.path.dirname(os.path.abspath(__file__))
    if "--selftest" in sys.argv:
        selftest()
    elif "--vectors" in sys.argv:
        vectors(os.path.join(here, "adapt_oracle_vectors.csv"))
        print("wrote adapt_oracle_vectors.csv")
    else:
        print(__doc__)
