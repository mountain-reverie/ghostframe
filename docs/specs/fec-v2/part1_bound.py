#!/usr/bin/env python3
"""Upper bound on the share of retransmissions FEC could avoid, from WS-C3's
browserless measurements (data_c3_rows.txt, copied from gf-c3/target/tmp).

net_lost_dg = netsim-dropped QUIC packets x datagrams per offered packet
(the only loss the NETWORK caused). A perfect FEC removes at most one
retransmission per network-lost datagram, so FEC_avoidable <= net_lost_dg / retx.
Run: python3 -I part1_bound.py
"""
import collections, re
rows = [dict(re.findall(r'(\w+)=(\S+)', l)) for l in open('data_c3_rows.txt') if l.startswith('C3ROW')]
g = collections.defaultdict(list)
for r in rows:
    g[(r['K'], r['model'])].append(r)
def med(rs, f):
    v = sorted(float(r[f]) for r in rs); return v[len(v) // 2]
print(f"{'cfg':10s} {'model':8s} {'pkt_drop':>8s} {'net_lost_dg':>11s} {'retx':>6s} {'rto%':>5s} {'nack_hit%':>9s} {'rto/net_lost':>12s} {'FEC_max%':>8s} {'unrcv/net_lost':>14s}")
for (k, m), rs in sorted(g.items()):
    if m == 'clean' or k not in ('0/S1/W40', '10/S1/W40'):
        continue
    if m.startswith('ge5_b8') or m.startswith('ge10'):
        # C3 flags dg/pkt as unreliable in the b8 rows with 30-110 dg/pkt
        pass
    dgpp = med(rs, 'total_src') / max(med(rs, 'offered'), 1)
    net = med(rs, 'dropped') * dgpp
    retx = med(rs, 'retx'); rto = med(rs, 'rto'); nh = med(rs, 'nack_hit')
    print(f"{k:10s} {m:8s} {med(rs,'dropped'):8.0f} {net:11.0f} {retx:6.0f} {100*rto/retx:5.1f} {100*nh/retx:9.1f} {rto/net:12.1f} {100*min(1,net/retx):8.1f} {med(rs,'src_dropped')/net:14.1f}")
