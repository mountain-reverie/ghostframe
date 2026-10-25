# Generates a scripted-bottleneck trace on stdout (deterministic: seeded).
import heapq, random, sys
rnd = random.Random(12345)
ev = []  # (t, order, kind, ...)
t = 0
nxt = 1
inflight = 0
lost_bytes = 0
out = []
# link
link_free = 0
def serial(rate_bpus, size): return max(1, size * 1000 // rate_bpus) if False else None
PROP = 20000
phases = [
 # (duration_us, mode, rate bytes/ms, ack_every, loss_pct)
 (400_000, 'sat', 1500, 1, 0),
 (400_000, 'sat', 1500, 2, 0),
 (300_000, 'sparse', 1500, 2, 0),
 (400_000, 'sat', 600, 3, 4),
 (300_000, 'sparse', 600, 1, 0),
 (400_000, 'sat', 2400, 2, 3),
]
pending_acks = []   # (ack_time, order, pktlist)
inf = {}            # pkt -> size
heap = []           # (time, seq, kind, payload)
seq = 0
def push(tm, kind, payload):
    global seq
    seq += 1
    heapq.heappush(heap, (tm, seq, kind, payload))
now = 0
link_free = 0
unacked_batch = []
phase_end = 0
for dur, mode, rate, every, lossp in phases:
    phase_end += dur
    sparse_next = now
    while now < phase_end:
        # process due ack events first
        while heap and heap[0][0] <= now:
            tm, _, kind, pl = heapq.heappop(heap)
            if kind == 'A':
                lst = [p for p in pl if p in inf]
                dup = [p for p in pl if p not in inf]
                if rnd.random() < 0.1: rnd.shuffle(pl)
                out.append(f"A {tm} {','.join(map(str, pl))}")
                for p in pl:
                    if p in inf:
                        inflight -= inf.pop(p)
        if mode == 'sat':
            can = inflight < 40*1200
            if can:
                size = rnd.choice([1200, 1200, 1200, 700, 1350])
                out.append(f"S {now} {nxt} {size} {inflight} {lost_bytes}")
                inf[nxt] = size
                inflight += size
                start = max(now, link_free)
                link_free = start + size * 1000 // rate
                arr = link_free + PROP
                if rnd.random() * 100 >= lossp:
                    unacked_batch.append((arr, nxt))
                    if len(unacked_batch) >= every:
                        at_ = max(a for a, _ in unacked_batch) + 200
                        push(at_, 'A', [p for _, p in unacked_batch])
                        unacked_batch = []
                else:
                    lost_bytes += size
                nxt += 1
                now += 100
            else:
                now = heap[0][0] if heap else now + 1000
                if not heap: now += 1000
        else:
            if now >= sparse_next:
                size = 1200
                out.append(f"S {now} {nxt} {size} {inflight} {lost_bytes}")
                out.append("L 1")
                inf[nxt] = size
                inflight += size
                start = max(now, link_free)
                link_free = start + size * 1000 // rate
                arr = link_free + PROP
                push(arr + 200, 'A', [nxt])
                if rnd.random() < 0.2:
                    push(arr + 1500, 'A', [nxt])   # duplicate ack
                nxt += 1
                sparse_next = now + 7000
            now = min(sparse_next, heap[0][0]) if heap else sparse_next
            if now < 0: now = 0
        if unacked_batch and mode == 'sparse':
            unacked_batch = []
    out.append("L 0") if False else None
    # flush leftover partial batch at phase end
    if unacked_batch:
        at_ = max(a for a, _ in unacked_batch) + 200
        push(at_, 'A', [p for _, p in unacked_batch])
        unacked_batch = []
    # explicit app-limited clearing between phases is left to the estimator
while heap:
    tm, _, kind, pl = heapq.heappop(heap)
    out.append(f"A {tm} {','.join(map(str, pl))}")
    for p in pl:
        if p in inf: inf.pop(p)
sys.stdout.write("\n".join(out) + "\n")
