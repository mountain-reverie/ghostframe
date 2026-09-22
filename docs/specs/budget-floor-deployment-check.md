# Reading a real session against the budget-floor change

**Date:** 2026-09-22
**Change under test:** `191d599` -- the scheduler tick budget floors at one
datagram instead of a flat 256 KiB.

The browserless harness cannot settle whether this fixes the reported symptom
(regions that stay blurry on a quiet screen). Its netsim consumes the token
bucket in `send_to_all_sessions`, *below* quinn's `datagram_send_buffer`, so
it never exercises the backpressure path production has. This is the
procedure for deciding it on a real session instead.

## Deploy

```bash
just build-release                              # build-web FIRST -- the client is //go:embed'd
sudo ./packaging/install.sh <user> --force
sudo machinectl shell <user>@ /usr/bin/systemctl --user restart ghostframe.target
```

Then **hard-reload the browser tab** (Ctrl+Shift+R). The wasm bundle is cached
and a soft reload keeps the old one -- a stale bundle looks like a protocol
regression.

Confirm the embedded bundle actually changed rather than trusting the build:

```bash
HASH=$(ls ghostframe-web-client/dist/assets | grep -oE 'index-[A-Za-z0-9_-]+' | sed 's/\.js$//' | head -1)
strings target/release/ghostframe-xdaemon | grep -c "$HASH"   # expect >= 1
```

The hash is vite's base64-ish digest (`index-C-pTSjk7`), not lowercase hex,
and it can itself contain `-` and `_`. Two patterns that look right and are
not: `[a-f0-9]{8}` matches nothing and prints `0` as if the build failed;
`[A-Za-z0-9]+` stops at the first `-` and "passes" on a prefix match, which
would also pass against a *stale* bundle whose hash shares that prefix.

### Three traps in this build, all hit on 2026-09-22

1. **`wasm-pack` must be on `PATH`.** It lives in `~/.cargo/bin`, which is
   not on every shell's `PATH` (a rustup-managed `/usr/lib/rustup/bin` is
   not the same thing). Without it `build-web` dies at `build:wasm` with
   `command not found`.
2. **Do not pipe `just build-release` into anything.** A pipeline's exit
   status is the *last* command's, so `just build-release | tail` reports
   success for a build that failed with exit 127. This is how a stale bundle
   gets deployed while the log says the build was fine.
3. **Check the artifact timestamps, not the build's word.** `dist/assets`
   and `target/release/ghostframe-xdaemon` should both be newer than your
   last source edit. A stale `dist/` is silent: the daemon embeds whatever is
   there at compile time.

A positive check that the *content* changed, not just the hash -- name a
symbol your edit removed:

```bash
grep -c 'cdf53PassHist' ghostframe-web-client/dist/assets/*.js   # expect 0
```

## Server: what to read, and where

```bash
journalctl _UID=$(id -u guest) -f            # live, as cedric, no sudo
journalctl _UID=$(id -u guest) | grep 'cumulative emit'
```

One line every 60 frames at `info`, which is the shipped default filter
(`ghostframe=info`); the first lands at `frame_seq=60`. A short or idle
session may produce none at all -- a solid-colour e2e scene reached only ~3
scheduler ticks and logged nothing, so give the session real activity before
concluding the field is missing. The fields that matter here:

| field | reading |
|---|---|
| `base_budget_bytes` | the per-tick emission budget actually in force |
| `tick_budget_floor_bytes` | what it is floored against -- should be ~one MTU |
| `bytes_per_us` | the estimate driving it (`cwnd / smoothed_rtt`) |
| `queued_critical_latency_mean_us` / `_max_us` | **the headline metric** |
| `send_datagram_errs_total` | quinn refusing datagrams -- overdrive |
| `retransmit_attempts_total` | wasted work |

### Grepping these out

The formatter emits ANSI escapes *between* the field name and its value, so
`grep -oE 'base_budget_bytes=[0-9]+'` matches **nothing** and looks like a
missing field. Strip them first:

```bash
journalctl _UID=$(id -u guest) | grep 'cumulative emit' \
  | sed -r 's/\x1B\[[0-9;]*[mK]//g' \
  | grep -oE '(base_budget_bytes|tick_budget_floor_bytes|bytes_per_us|queued_critical_latency_(mean|max)_us)=[0-9.e+-]+'
```

Verified against a real container session on 2026-09-22:

```
base_budget_bytes=2039581   tick_budget_floor_bytes=1198
bytes_per_us=67.98          smoothed_rtt_us=6148.65
queued_critical_latency_mean_us=152486   queued_critical_latency_max_us=162237
send_datagram_errs_total=0  retransmit_attempts_total=0
```

`1198` is one MTU, which is the shipped floor. `67.98 * 33333 * 0.90 =
2,039,575`, matching `base_budget_bytes` -- the bandwidth term is setting the
budget and the floor is inert, which is the intended state. (That container
link is ~544 Mbit, above the ~70 Mbit knee, so the old floor would not have
bound there either. A slow real link is where the change matters.)

**The first check is that the change is live at all:** if
`base_budget_bytes == tick_budget_floor_bytes` on every line, the estimate is
still not binding and the floor is still the policy. Before this change that
was true on every link below ~70 Mbit/s, which is all of them.

These four fields were previously `debug!`-only on a per-frame site, which the
`ghostframe=info` default drops -- so a deployed build could not show whether
the budget was tracking the path. They now ride the existing 60-frame `info`
line, deliberately: journald's `RateLimitBurst` silently drops a burst and
leaves a hole that reads as a stalled capture loop, which has already caused
one misdiagnosis here. Check for `Suppressed N messages` before concluding
anything from a gap.

## The comparison

The baseline, from production before the change:

| metric | before |
|---|---|
| critical `queued->ACK` mean | 2,289,529 us (2.3 s) |
| critical `queued->ACK` max | 16.8 s |
| critical `last_sent->ACK` mean | 22,748 us (22.7 ms) |
| separation | ~100x |

Compare against the same fields on a session of similar length and activity.
The metric is cumulative since startup, so compare like for like -- a short
session is dominated by the opening burst.

**Expected direction:** down, because the over-popping that fills quinn's send
buffer is exactly what the flat floor caused. **This is an argument, not a
measurement** -- it is the thing this deployment is meant to test, so treat a
null or negative result as data, not as a deployment fault.

## Client: what to read

Browser console, printed on change with idle suppression:

- `cdf53-coverage: ...` -- includes `partial` and, critically, `gave_up`.
  `gave_up` counts tiles that exhausted `MAX_TAIL_SWEEP_ATTEMPTS` and stopped
  asking for the passes they are missing. Those tiles are stuck at a partial
  pass set and **will render wrong for the rest of the session** -- this is
  the client-side signature of the reported symptom.
- `cdf53-incomplete: (x,y) missing=0b... sweeps=N` -- names the stuck tiles
  and which bit-planes they never got. Only printed when `partial > 0 ||
  gave_up > 0`.

Cross-check against the screenshot: if the blurry regions match the
coordinates on the `cdf53-incomplete` line, the symptom is stuck refinement
and not, say, a classifier mode flip.

### One caution about client counters

`stats: rx=` and `lastSeq=` print an explicit *not measured* sentinel rather
than `0`. Their writers were deleted in the wasm cutover and only the readers
remained; printed as `0` they read as "no tiles received" on a session that
decoded thousands. Do not reintroduce a bare number there. A sibling block
computing a Cdf53 pass histogram from the same orphaned map was dead in the
same way and was removed on 2026-09-22 -- it produced values nothing printed.

## If the numbers do not move

The next suspect is not the floor. `base_budget_bytes` is clamped afterwards
by `clamp_to_quinn_capacity`, which is `0.80 * send_buffer_space()`. On a real
path that clamp can still be the binding constraint even with a correct
budget, and it is the one the browserless harness cannot exercise. See
`docs/specs/blocked-path-redesign.md`.
