# Wasm Cutover Step 3a — `ghostframe-client-wasm` Crate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `ghostframe-client-wasm` crate — a thin wasm-bindgen wrapper exposing `ghostframe-client-core` to JavaScript — plus its build wiring and CI gate, while the TypeScript protocol layer remains live and untouched.

**Architecture:** A new workspace member compiles `client-core` to wasm via `wasm-pack`. It exports two layers: a `WasmClientCore` session object (the real boundary that `main.ts` will use at step 4), and a set of thin per-unit shims (`WasmAckBatcher`, `WasmPaletteShadow`, …) that exist so the *existing* vitest suites can be retargeted at Rust in step 3b without being rewritten as integration tests. Events cross as plain JS objects via `serde-wasm-bindgen`. Nothing in the web client changes in this plan.

**Tech Stack:** Rust, `wasm-bindgen` 0.2, `serde-wasm-bindgen` 0.6, `console_error_panic_hook`, `wasm-pack` 0.15.0, vitest 2, vite 6.

---

## Scope

This plan covers **step 3a only**: the crate, its exports, its build wiring, and its CI gate.

**Step 3b** — retargeting the eleven vitest suites at these exports — gets its own plan once this lands. It is the step the spec flags as most likely to expand ("budget debugging, not a clean port"), and it is not usefully planned until the exports it targets actually exist.

**Step 4** — the cutover — remains a separate, irreversible plan.

Nothing in this plan modifies `ghostframe-web-client/src/`. The TS protocol layer stays live.

---

## Toolchain decisions (verified by spike, not assumed)

A spike built a representative wasm-bindgen crate — a stateful struct taking `&[u8]` and returning a tagged-enum array containing `Vec<u8>` and `Vec<[u8;4]>` — and loaded it three ways. Results:

| Target | Under vitest (`environment: 'node'`) | Under `vite build` |
|---|---|---|
| `--target nodejs` | **works** (plain ESM `import` *and* `createRequire`) | n/a |
| `--target bundler` | **fails**: *"ESM integration proposal for Wasm is not supported currently. Use vite-plugin-wasm…"* | would need the plugin |
| `--target web` | n/a | **works**, zero plugins; emits a hashed `.wasm` asset |

**Decision: build two targets.** `--target nodejs` → `pkg-node/` for vitest; `--target web` → `pkg-web/` for the browser bundle. The alternative (one `bundler` target) costs `vite-plugin-wasm` + `vite-plugin-top-level-await`, and `ghostframe-web-client` currently has **zero** runtime dependencies — worth preserving.

**Constraint carried into step 4:** `--target web` exports a default async `init()`. It must be awaited *inside* an async function. A top-level `await init()` fails the vite build outright:

```
ERROR: Top-level await is not available in the configured target environment
("chrome87", "edge88", "es2020", "firefox78", "safari14" + 2 overrides)
```

`main.ts` already does its setup inside async functions, so this is a note, not a problem.

**Confirmed:** `serde-wasm-bindgen` round-trips `Vec<u8>` (arrives as `Uint8Array`), `Vec<[u8;4]>` (arrives as nested arrays), and `#[serde(tag = "kind")]` enums (arrive as tagged plain objects) with no manual glue.

---

## Two corrections to the spec

The spec's Testing section sorts the suites into three buckets. Two entries are in the wrong bucket. Fix the spec as Task 0.

**`solid_pack` is not a protocol test.** It is listed under *retarget then delete*. It has no import from `src/` at all — it **replicates** the packing formula inside the test file, with a comment saying so:

```ts
// We can't import the real SolidPipeline (it needs GPUDevice). Test the
// pack-to-Uint32Array math by replicating the formula here. If this test
// drifts from the real implementation, the formula in solid.ts changed —
// update both.
```

Its subject is `src/webgpu/solid.ts`, which is an explicit **non-goal** ("Replacing, porting, or touching `src/webgpu/` or any WGSL shader"). It belongs in *untouched*. Retargeting it at wasm would move a GPU-side test onto a protocol boundary that does not own the behaviour.

**`tile_key` has no Rust analogue to retarget onto.** It is listed under *retarget then delete*. It asserts a **string** key shape:

```ts
const k = tileKey(0xCAFE, 17, 23, 5);
expect(k).toBe('51966:17:23:5');
```

Rust has no string key — `TileKey` is a `#[derive(Hash, Eq)]` struct with `pass_idx` as a field, so the cross-pass collision the test guards is impossible by construction rather than by assertion. There is nothing to export a shim for. It belongs in *split or reconsider*: in step 3b it becomes a behavioural test driving `handle_datagram` with two passes under one `frame_seq` and asserting they do not merge. Note this; do not try to export a `tile_key` function to satisfy the old assertion.

---

## File structure

**Created:**

| Path | Responsibility |
|---|---|
| `ghostframe-client-wasm/Cargo.toml` | Crate manifest; `cdylib` + `rlib`. |
| `ghostframe-client-wasm/build.rs` | Emits the protocol version stamp. |
| `ghostframe-client-wasm/src/lib.rs` | Crate root, panic hook, stamp export. |
| `ghostframe-client-wasm/src/boundary.rs` | Serde-serialisable mirrors of `Event` / `PollOutput`. |
| `ghostframe-client-wasm/src/core.rs` | `WasmClientCore` — the real session boundary. |
| `ghostframe-client-wasm/src/units.rs` | Per-unit shims for the retargeted suites. |
| `ghostframe-client-wasm/src/input.rs` | Input-encoder free-function exports. |
| `ghostframe-web-client/tests/wasm_smoke.test.ts` | Proves the module loads and the stamp matches. |
| `ghostframe-web-client/scripts/protocol_stamp.mjs` | Recomputes the stamp in JS for the staleness guard. |

**Modified:**

| Path | Change |
|---|---|
| `Cargo.toml` | Add `ghostframe-client-wasm` to `members`. |
| `tests/containers/test-server/Dockerfile` | Add the crate's `COPY` lines (see Task 1 — omitting this breaks every e2e test). |
| `ghostframe-web-client/package.json` | `build:wasm`, `build:wasm:node` scripts; `build` depends on the former. |
| `ghostframe-web-client/.gitignore` | Ignore `pkg-node/`, `pkg-web/`. |
| `.github/workflows/client-core.yml` | Install `wasm-pack`, build both targets, run the smoke test. |
| `docs/superpowers/specs/2026-09-11-wasm-cutover-design.md` | The two bucket corrections (Task 0). |

---

## Task 0: Correct the two miscategorised suites in the spec

**Files:**
- Modify: `docs/superpowers/specs/2026-09-11-wasm-cutover-design.md`

- [ ] **Step 1: Move `solid_pack` out of the retarget bucket**

In the `## Testing` section, the *retarget then delete* list currently ends `..., tile_key, solid_pack.` Remove both `tile_key` and `solid_pack` from it, so it reads:

```markdown
- *Retarget then delete*: `ack`, `nack`, `cdf53_coverage`,
  `decode_error_batcher`, `feedback`, `palette_shadow`, `parity_decoder`,
  `prevalidate`, `prevalidate_cdf53`.
```

- [ ] **Step 2: Add `solid_pack` to the untouched bucket**

```markdown
- *Untouched*: `bootstrap`, `diagnostics`, `renderer_idle_skip`, `sanity`,
  `solid_pack` — their subjects survive the cutover. (`solid_pack` imports
  nothing from `src/`; it replicates `src/webgpu/solid.ts`'s packing formula
  inline, so its subject is the GPU path, which is a non-goal.)
```

- [ ] **Step 3: Add `tile_key` to the reconsider bucket**

Append to the *split or reconsider* bullet:

```markdown
  `tile_key` asserts a *string* key shape (`'51966:17:23:5'`) produced by
  `decoder.ts`; Rust's `TileKey` is a `#[derive(Hash, Eq)]` struct whose
  `pass_idx` field makes the cross-pass collision impossible by construction.
  There is no shim to retarget onto — it becomes a behavioural test driving
  `handle_datagram` with two passes under one `frame_seq`.
```

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-09-11-wasm-cutover-design.md
git commit -m "docs(spec): recategorise solid_pack and tile_key suites

solid_pack replicates the WebGPU packing formula inline and imports
nothing from src/ — its subject is a non-goal. tile_key asserts a string
key shape with no Rust analogue."
```

---

## Task 1: Scaffold the crate

**Files:**
- Create: `ghostframe-client-wasm/Cargo.toml`
- Create: `ghostframe-client-wasm/src/lib.rs`
- Modify: `Cargo.toml`
- Modify: `tests/containers/test-server/Dockerfile`

> **Landmine:** `tests/containers/test-server/Dockerfile` COPYs each workspace
> member's manifest by name (lines 34–59). A new member that is not listed
> makes `docker buildx` fail with a cargo workspace error **before any e2e
> test runs**, and `cargo test` does not rebuild the image, so the failure
> surfaces far from its cause. Add the COPY lines in this task, not later.

- [ ] **Step 1: Write the manifest**

`ghostframe-client-wasm/Cargo.toml`:

```toml
[package]
name = "ghostframe-client-wasm"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
ghostframe-client-core = { path = "../ghostframe-client-core" }
ghostframe-protocol = { path = "../ghostframe-protocol" }
wasm-bindgen = "0.2"
serde = { version = "1", features = ["derive"] }
serde-wasm-bindgen = "0.6"
console_error_panic_hook = "0.1"
```

- [ ] **Step 2: Write the crate root**

`ghostframe-client-wasm/src/lib.rs`:

```rust
//! wasm-bindgen boundary for `ghostframe-client-core`.
//!
//! Two layers live here:
//!
//! * [`core::WasmClientCore`] — the real session boundary. `main.ts` drives
//!   this at cutover: datagrams in, events out, bytes back to the wire.
//! * [`units`] — thin per-unit shims. These exist so the *existing* vitest
//!   suites can be retargeted at Rust without being rewritten as integration
//!   tests; the suites encode the old implementation's real behaviour, which
//!   is what makes them able to detect divergence.
//!
//! A Rust panic in wasm aborts the module: every later call traps, so one
//! malformed datagram would end the session rather than drop a frame. No
//! export here may `unwrap` on wire-derived data.

use wasm_bindgen::prelude::*;

pub mod boundary;
pub mod core;
pub mod input;
pub mod units;

/// Hash of the `client-core` + `protocol` sources this module was built
/// from. See `build.rs`; asserted against the workspace by
/// `tests/wasm_smoke.test.ts` so a stale `dist/` fails instead of silently
/// serving yesterday's protocol.
pub const PROTOCOL_STAMP: &str = env!("GHOSTFRAME_PROTOCOL_STAMP");

/// Installs the panic hook. Idempotent; call once at module load.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
pub fn protocol_stamp() -> String {
    PROTOCOL_STAMP.to_string()
}
```

Create empty placeholder modules so this compiles:

```bash
cd /home/cedric/work/ghostframe
for m in boundary core input units; do
  printf '//! Placeholder; filled in by a later task.\n' > "ghostframe-client-wasm/src/$m.rs"
done
```

`build.rs` does not exist yet, so `env!("GHOSTFRAME_PROTOCOL_STAMP")` will fail
to compile. That is expected — Task 2 adds it. To keep this task's steps
independently verifiable, temporarily stub the constant:

```rust
pub const PROTOCOL_STAMP: &str = "unstamped";
```

and restore the `env!` form in Task 2 Step 4.

- [ ] **Step 3: Add to the workspace**

In the root `Cargo.toml`, add the member after `ghostframe-client-net`:

```toml
members = [
    "ghostframe-protocol",
    "ghostframe-client-core",
    "ghostframe-client-net",
    "ghostframe-client-wasm",
    "ghostframe-lib",
    "ghostframe-xdaemon",
    "ghostframe-test-pattern",
    "ghostframe-e2e",
    "ghostframe-bench",
]
```

- [ ] **Step 4: Add the Dockerfile COPY lines**

In `tests/containers/test-server/Dockerfile`, immediately after the
`ghostframe-client-net` pair (lines 45–46), add:

```dockerfile
COPY ghostframe-client-wasm/Cargo.toml ghostframe-client-wasm/Cargo.toml
COPY ghostframe-client-wasm/src/ ghostframe-client-wasm/src/
COPY ghostframe-client-wasm/build.rs ghostframe-client-wasm/build.rs
```

> The `build.rs` line refers to a file Task 2 creates. Docker `COPY` of a
> missing path fails the build, so **either** create an empty `build.rs` now
> **or** add that third line in Task 2. Creating it now is simpler:
> `printf 'fn main() {}\n' > ghostframe-client-wasm/build.rs`

- [ ] **Step 5: Verify the native workspace still builds**

```bash
cargo build --workspace 2>&1 | tail -5
```

Expected: `Finished` with no errors. A `cdylib` builds a `.so` on native — harmless and unused.

- [ ] **Step 6: Verify the wasm build works**

```bash
cd /home/cedric/work/ghostframe/ghostframe-client-wasm
export PATH="$HOME/.cargo/bin:$PATH"
wasm-pack build --target nodejs --out-dir pkg-node 2>&1 | tail -4
```

Expected: `Your wasm pkg is ready to publish at .../pkg-node.`

> If `wasm-pack: command not found`, install it:
> `cargo install wasm-pack --version 0.15.0 --locked`
> Note `~/.cargo/bin` is not on PATH in non-login shells here; export it.

- [ ] **Step 7: Verify the e2e image still builds**

```bash
cd /home/cedric/work/ghostframe && just containers-build 2>&1 | tail -5
```

Expected: builds to completion. This is the check that catches a missed COPY line.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml ghostframe-client-wasm tests/containers/test-server/Dockerfile
git commit -m "feat(wasm): scaffold ghostframe-client-wasm crate

New workspace member; adds the per-crate Dockerfile COPY lines the
test-server image needs, without which docker buildx fails before any
e2e test runs."
```

---

## Task 2: Protocol version stamp

The stamp turns a stale `dist/` — wasm built from an older `client-core`, which
compiles and passes CI — into a test failure. It hashes the sources the
protocol behaviour actually comes from.

FNV-1a is used deliberately: it is ~10 lines in both Rust and JS, so the guard
adds no dependency on either side. It is not a security hash and does not need
to be.

**Files:**
- Create: `ghostframe-client-wasm/build.rs` (replacing the Task 1 stub)
- Create: `ghostframe-web-client/scripts/protocol_stamp.mjs`
- Modify: `ghostframe-client-wasm/src/lib.rs`

- [ ] **Step 1: Write the build script**

`ghostframe-client-wasm/build.rs`:

```rust
//! Emits `GHOSTFRAME_PROTOCOL_STAMP`: an FNV-1a hash over the sorted
//! relative paths and contents of the crates whose sources define protocol
//! behaviour. `ghostframe-web-client/scripts/protocol_stamp.mjs` recomputes
//! the identical value in JS; a smoke test asserts they match, so a wasm
//! built from stale sources fails instead of silently serving yesterday's
//! protocol.
//!
//! Paths are hashed relative to the workspace root so the stamp does not
//! depend on the build directory.

use std::path::{Path, PathBuf};

/// Directories that define protocol behaviour, relative to the workspace root.
const STAMPED_DIRS: &[&str] = &["ghostframe-client-core/src", "ghostframe-protocol/src"];

/// FNV-1a 64-bit prime, 2^40 + 2^8 + 0xb3. Grouped in fours from the right
/// so a wrong digit count is visible: an extra zero here still compiles and
/// still produces a stable-looking hash, but one that diverges from the JS
/// twin only in the high bits.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("stamp: cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("stamp: bad dir entry").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .expect("stamp: crate has no parent dir")
        .to_path_buf();

    let mut files = Vec::new();
    for rel in STAMPED_DIRS {
        let dir = root.join(rel);
        println!("cargo:rerun-if-changed={}", dir.display());
        collect(&dir, &mut files);
    }
    // Deterministic order: the hash must not depend on readdir order.
    files.sort();

    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for path in &files {
        let rel = path
            .strip_prefix(&root)
            .expect("stamp: file outside workspace root");
        // Normalise separators so the stamp matches the JS side on any host.
        let rel = rel.to_string_lossy().replace('\\', "/");
        hash = fnv1a(rel.as_bytes(), hash);
        let body = std::fs::read(path)
            .unwrap_or_else(|e| panic!("stamp: cannot read {}: {e}", path.display()));
        hash = fnv1a(&body, hash);
        println!("cargo:rerun-if-changed={}", path.display());
    }

    println!("cargo:rustc-env=GHOSTFRAME_PROTOCOL_STAMP={hash:016x}");
}
```

- [ ] **Step 2: Write the JS twin**

`ghostframe-web-client/scripts/protocol_stamp.mjs`:

```js
// Recomputes ghostframe-client-wasm's GHOSTFRAME_PROTOCOL_STAMP in JS.
// Must stay byte-for-byte equivalent to ../../ghostframe-client-wasm/build.rs:
// same directories, same sort order, same relative-path normalisation, same
// FNV-1a constants. A divergence here makes the staleness guard vacuous, so
// change both or neither.
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(fileURLToPath(new URL('.', import.meta.url)), '..', '..');
const STAMPED_DIRS = ['ghostframe-client-core/src', 'ghostframe-protocol/src'];

const OFFSET = 0xcbf29ce484222325n;
const PRIME = 0x100000001b3n;
const MASK = (1n << 64n) - 1n;

function fnv1a(bytes, hash) {
  for (const b of bytes) {
    hash ^= BigInt(b);
    hash = (hash * PRIME) & MASK;
  }
  return hash;
}

function collect(dir, out) {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) collect(path, out);
    else if (name.endsWith('.rs')) out.push(path);
  }
}

export function protocolStamp() {
  const files = [];
  for (const rel of STAMPED_DIRS) collect(join(ROOT, rel), files);
  // Rust sorts PathBuf, which compares as OS strings — byte order on unix.
  files.sort();

  let hash = OFFSET;
  for (const path of files) {
    const rel = relative(ROOT, path).split(sep).join('/');
    hash = fnv1a(Buffer.from(rel, 'utf8'), hash);
    hash = fnv1a(readFileSync(path), hash);
  }
  return hash.toString(16).padStart(16, '0');
}
```

- [ ] **Step 3: Verify both sides agree**

```bash
cd /home/cedric/work/ghostframe/ghostframe-client-wasm
export PATH="$HOME/.cargo/bin:$PATH"
cargo build 2>&1 | tail -2
echo "rust: $(grep -ao 'GHOSTFRAME_PROTOCOL_STAMP=[0-9a-f]*' ../target/debug/build/ghostframe-client-wasm-*/output | head -1)"
cd ../ghostframe-web-client
node -e "import('./scripts/protocol_stamp.mjs').then(m => console.log('js:  ', m.protocolStamp()))"
```

Expected: the two 16-hex-digit values are identical.

If they differ, the likely causes in order: sort order (Rust sorts full
`PathBuf`s, JS sorts full path strings — both absolute, same prefix, so they
agree on unix); a non-`.rs` file included on one side; or `BigInt` vs `u64`
wrapping. Do not "fix" this by loosening the test.

- [ ] **Step 4: Restore the real constant**

In `ghostframe-client-wasm/src/lib.rs`, replace the Task 1 stub:

```rust
pub const PROTOCOL_STAMP: &str = env!("GHOSTFRAME_PROTOCOL_STAMP");
```

- [ ] **Step 5: Verify it still builds**

```bash
cd /home/cedric/work/ghostframe && cargo build -p ghostframe-client-wasm 2>&1 | tail -3
```

Expected: `Finished`.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-client-wasm/build.rs ghostframe-client-wasm/src/lib.rs \
        ghostframe-web-client/scripts/protocol_stamp.mjs
git commit -m "feat(wasm): protocol version stamp with a JS twin

Hashes client-core + protocol sources at build time; the JS twin lets a
test assert the loaded module matches the workspace, turning a stale
dist/ from a silent wrong-protocol session into a failure."
```

---

## Task 3: Boundary event types

`Event` and `PollOutput` are plain Rust enums in `client-core`; they cannot
derive `Serialize` there without adding `serde` to a crate that
`ghostframe-lib` and `ghostframe-e2e` depend on. Mirror them here instead.

Note `Event::TilePayload` and `Event::PaletteUpdated` already exist in
`client-core` — they landed in step 2. `TileReady` also still exists and is
what `Decoded` consumers get; the browser will take `Payload`, but the mirror
must cover both so the type is total.

**Files:**
- Create: `ghostframe-client-wasm/src/boundary.rs`
- Test: `ghostframe-client-wasm/src/boundary.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Append to `ghostframe-client-wasm/src/boundary.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ghostframe_client_core::{DecodeErrorCode, Event};
    use ghostframe_protocol::protocol::Codec;

    #[test]
    fn tile_payload_mirrors_every_field() {
        let ev = Event::TilePayload {
            frame_seq: 0x1234_5678,
            tile_x: 3,
            tile_y: 7,
            pass_idx: 13,
            generation: 2,
            codec: Codec::Cdf53,
            payload: vec![1, 2, 3],
        };
        match WasmEvent::from(&ev) {
            WasmEvent::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                generation,
                codec,
                payload,
            } => {
                assert_eq!(frame_seq, 0x1234_5678);
                assert_eq!(tile_x, 3);
                assert_eq!(tile_y, 7);
                assert_eq!(pass_idx, 13);
                assert_eq!(generation, 2);
                assert_eq!(codec, Codec::Cdf53 as u8);
                assert_eq!(payload, vec![1, 2, 3]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn decode_error_carries_the_code_discriminant() {
        let ev = Event::DecodeError {
            codec: Codec::PalRle,
            tile_x: 1,
            tile_y: 2,
            code: DecodeErrorCode::IndexOob,
        };
        match WasmEvent::from(&ev) {
            WasmEvent::DecodeError { code, .. } => assert_eq!(code, 5),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn palette_updated_preserves_colour_order() {
        let ev = Event::PaletteUpdated {
            palette_id: 9,
            colors: vec![[1, 2, 3, 4], [5, 6, 7, 8]],
        };
        match WasmEvent::from(&ev) {
            WasmEvent::PaletteUpdated { palette_id, colors } => {
                assert_eq!(palette_id, 9);
                assert_eq!(colors, vec![[1, 2, 3, 4], [5, 6, 7, 8]]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Run it to confirm it fails**

```bash
cd /home/cedric/work/ghostframe && cargo test -p ghostframe-client-wasm 2>&1 | tail -15
```

Expected: compile error — `WasmEvent` not found.

- [ ] **Step 3: Write the mirror**

Prepend to `ghostframe-client-wasm/src/boundary.rs`:

```rust
//! Serde mirrors of `client-core`'s `Event` and `PollOutput`.
//!
//! `client-core` is consumed by `ghostframe-lib` and `ghostframe-e2e` on
//! native targets; neither should acquire `serde` or `wasm-bindgen` in its
//! graph just so the browser can have JSON-shaped events. The mirror lives
//! here instead.
//!
//! Serialised with `#[serde(tag = "kind")]`, so JS sees plain objects like
//! `{ kind: 'TilePayload', tile_x: 3, payload: Uint8Array }`.

use ghostframe_client_core::{Event, PollOutput};
use serde::Serialize;

#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "kind")]
pub enum WasmEvent {
    TileReady {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        rgba: Vec<u8>,
    },
    TilePayload {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        pass_idx: u8,
        generation: u8,
        /// `Codec` discriminant; `Codec` lives in `ghostframe-protocol` and
        /// is not `Serialize`, so it crosses as its `repr(u8)` value.
        codec: u8,
        payload: Vec<u8>,
    },
    PaletteUpdated {
        palette_id: u8,
        colors: Vec<[u8; 4]>,
    },
    FrameDimensions {
        width: u32,
        height: u32,
    },
    NeedsH264 {
        frame_seq: u32,
        timestamp_us: u32,
        is_keyframe: bool,
        payload: Vec<u8>,
    },
    DecodeError {
        codec: u8,
        tile_x: u8,
        tile_y: u8,
        /// `DecodeErrorCode` discriminant, 1..=10.
        code: u8,
    },
}

impl From<&Event> for WasmEvent {
    fn from(ev: &Event) -> Self {
        match ev {
            Event::TileReady {
                frame_seq,
                tile_x,
                tile_y,
                rgba,
            } => WasmEvent::TileReady {
                frame_seq: *frame_seq,
                tile_x: *tile_x,
                tile_y: *tile_y,
                rgba: rgba.clone(),
            },
            Event::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                generation,
                codec,
                payload,
            } => WasmEvent::TilePayload {
                frame_seq: *frame_seq,
                tile_x: *tile_x,
                tile_y: *tile_y,
                pass_idx: *pass_idx,
                generation: *generation,
                codec: *codec as u8,
                payload: payload.clone(),
            },
            Event::PaletteUpdated { palette_id, colors } => WasmEvent::PaletteUpdated {
                palette_id: *palette_id,
                colors: colors.clone(),
            },
            Event::FrameDimensions { width, height } => WasmEvent::FrameDimensions {
                width: *width,
                height: *height,
            },
            Event::NeedsH264 {
                frame_seq,
                timestamp_us,
                is_keyframe,
                payload,
            } => WasmEvent::NeedsH264 {
                frame_seq: *frame_seq,
                timestamp_us: *timestamp_us,
                is_keyframe: *is_keyframe,
                payload: payload.clone(),
            },
            Event::DecodeError {
                codec,
                tile_x,
                tile_y,
                code,
            } => WasmEvent::DecodeError {
                codec: *codec as u8,
                tile_x: *tile_x,
                tile_y: *tile_y,
                code: *code as u8,
            },
        }
    }
}

/// Which wire an outbound buffer belongs on. The browser sends datagrams via
/// `transport.datagrams.writable` and stream bytes via the bidi stream
/// writer; it must not conflate them.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "kind")]
pub enum WasmPollOutput {
    Datagram { bytes: Vec<u8> },
    Stream { bytes: Vec<u8> },
}

impl From<PollOutput> for WasmPollOutput {
    fn from(out: PollOutput) -> Self {
        match out {
            PollOutput::Datagram(bytes) => WasmPollOutput::Datagram { bytes },
            PollOutput::Stream(bytes) => WasmPollOutput::Stream { bytes },
        }
    }
}
```

> The `match` on `Event` here is **exhaustive on purpose**. It is the only
> exhaustive match on `Event` in the tree, which makes it the place a newly
> added variant is caught at compile time instead of being silently dropped
> at the browser boundary. Do not add a `_ => {}` arm.

- [ ] **Step 4: Run the tests**

```bash
cd /home/cedric/work/ghostframe && cargo test -p ghostframe-client-wasm 2>&1 | tail -8
```

Expected: `test result: ok. 3 passed`.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-wasm/src/boundary.rs
git commit -m "feat(wasm): serde mirrors for Event and PollOutput

Exhaustive match on Event so a new variant is a compile error rather
than a silently dropped browser event."
```

---

## Task 4: `WasmClientCore`

**Files:**
- Create: `ghostframe-client-wasm/src/core.rs`

- [ ] **Step 1: Write the wrapper**

`ghostframe-client-wasm/src/core.rs`:

```rust
//! The session boundary. `main.ts` drives this at cutover.
//!
//! Every export returns a value JS can inspect; none may `unwrap` on
//! wire-derived data. A panic here aborts the whole module — every later
//! call traps — so one malformed datagram would end the session rather than
//! drop a frame.

use ghostframe_client_core::{ClientConfig, ClientCore, TileDelivery};
use wasm_bindgen::prelude::*;

use crate::boundary::{WasmEvent, WasmPollOutput};

/// Serialises `T` for JS, mapping serialisation failure to a `JsValue` error
/// rather than panicking.
fn to_js<T: serde::Serialize>(value: &T) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(value).map_err(|e| JsValue::from_str(&e.to_string()))
}

#[wasm_bindgen]
pub struct WasmClientCore {
    inner: ClientCore,
}

#[wasm_bindgen]
impl WasmClientCore {
    /// `tile_delivery_payload = true` gives the browser undecoded, validated
    /// payloads for the GPU (`TileDelivery::Payload`). Native consumers pass
    /// `false` and keep `Decoded`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        indices_raw_enabled: bool,
        supports_h264: bool,
        tile_delivery_payload: bool,
        now_us: u64,
    ) -> WasmClientCore {
        let config = ClientConfig {
            indices_raw_enabled,
            supports_h264,
            tile_delivery: if tile_delivery_payload {
                TileDelivery::Payload
            } else {
                TileDelivery::Decoded
            },
        };
        WasmClientCore {
            inner: ClientCore::new(config, now_us),
        }
    }

    /// Feed one inbound datagram; returns the resulting events as an array.
    ///
    /// Inbound is datagrams-only: the browser's bidi stream is used for
    /// outbound feedback, and the server sends tiles, dimensions, palettes
    /// and H.264 access units over `transport.datagrams`.
    #[wasm_bindgen(js_name = handleDatagram)]
    pub fn handle_datagram(&mut self, bytes: &[u8], now_us: u64) -> Result<JsValue, JsValue> {
        let events = self.inner.handle_datagram(bytes, now_us);
        let mirrored: Vec<WasmEvent> = events.iter().map(WasmEvent::from).collect();
        to_js(&mirrored)
    }

    /// Fire due timers (ACK/NACK flush, assembly timeout, tail sweep,
    /// periodic feedback); returns the resulting events.
    #[wasm_bindgen(js_name = onTimeout)]
    pub fn on_timeout(&mut self, now_us: u64) -> Result<JsValue, JsValue> {
        let events = self.inner.on_timeout(now_us);
        let mirrored: Vec<WasmEvent> = events.iter().map(WasmEvent::from).collect();
        to_js(&mirrored)
    }

    /// Drain one pending outbound buffer; returns `undefined` when empty.
    /// Call until it returns `undefined`.
    #[wasm_bindgen(js_name = pollTransmit)]
    pub fn poll_transmit(&mut self, now_us: u64) -> Result<JsValue, JsValue> {
        match self.inner.poll_transmit(now_us) {
            Some(out) => to_js(&WasmPollOutput::from(out)),
            None => Ok(JsValue::UNDEFINED),
        }
    }

    /// Earliest µs deadline at which `onTimeout` must be called.
    ///
    /// Returns `undefined` only if the core has no armed deadline. In
    /// practice the tail-sweep and feedback deadlines are always armed, so
    /// this is always a number — but JS must not assume that.
    #[wasm_bindgen(js_name = pollTimeout)]
    pub fn poll_timeout(&self) -> Option<u64> {
        self.inner.poll_timeout()
    }

    /// Encode a receiver-feedback report for the bidi stream.
    #[wasm_bindgen(js_name = encodeFeedback)]
    pub fn encode_feedback(&mut self, now_us: u64) -> Vec<u8> {
        self.inner.encode_feedback(now_us)
    }
}
```

- [ ] **Step 2: Verify it builds for both targets**

```bash
cd /home/cedric/work/ghostframe && cargo build -p ghostframe-client-wasm 2>&1 | tail -3
cd ghostframe-client-wasm && export PATH="$HOME/.cargo/bin:$PATH"
wasm-pack build --target nodejs --out-dir pkg-node 2>&1 | tail -3
```

Expected: both `Finished` / `ready to publish`.

- [ ] **Step 3: Check the generated TypeScript declarations**

```bash
cd /home/cedric/work/ghostframe/ghostframe-client-wasm && cat pkg-node/ghostframe_client_wasm.d.ts
```

Expected: `WasmClientCore` with `handleDatagram`, `onTimeout`, `pollTransmit`,
`pollTimeout`, `encodeFeedback`. Confirm `now_us: bigint` — `u64` crosses as
`BigInt`, not `number`. This is load-bearing for step 3b: every call site must
pass `BigInt(...)`, and mixing the two throws at runtime.

> If `bigint` in the signature proves awkward at step 4, the alternative is
> taking `f64` µs and casting. Do not change it now — record the observation
> and decide with the call sites in view.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-client-wasm/src/core.rs
git commit -m "feat(wasm): WasmClientCore session boundary"
```

---

## Task 5: Input encoder exports

**Files:**
- Create: `ghostframe-client-wasm/src/input.rs`

- [ ] **Step 1: Write the exports**

`ghostframe-client-wasm/src/input.rs`:

```rust
//! Input **encoding** only. Capture is platform-bound and stays in
//! `src/input/wire.ts`; this is the half that moves.

use ghostframe_client_core::input;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(js_name = encodePointerMove)]
pub fn encode_pointer_move(x: i16, y: i16) -> Vec<u8> {
    input::encode_pointer_move(x, y).to_vec()
}

#[wasm_bindgen(js_name = encodePointerButton)]
pub fn encode_pointer_button(x: i16, y: i16, button: u8, down: bool) -> Vec<u8> {
    input::encode_pointer_button(x, y, button, down).to_vec()
}

#[wasm_bindgen(js_name = encodeWheel)]
pub fn encode_wheel(dx: i16, dy: i16) -> Vec<u8> {
    input::encode_wheel(dx, dy).to_vec()
}

#[wasm_bindgen(js_name = encodeKeyDown)]
pub fn encode_key_down(keysym: u32) -> Vec<u8> {
    input::encode_key_down(keysym).to_vec()
}

#[wasm_bindgen(js_name = encodeKeyUp)]
pub fn encode_key_up(keysym: u32) -> Vec<u8> {
    input::encode_key_up(keysym).to_vec()
}

/// `undefined` for keys with no keysym mapping — the caller drops the event.
#[wasm_bindgen(js_name = keyToKeysym)]
pub fn key_to_keysym(key: &str) -> Option<u32> {
    input::key_to_keysym(key)
}
```

- [ ] **Step 2: Verify it builds**

```bash
cd /home/cedric/work/ghostframe && cargo build -p ghostframe-client-wasm 2>&1 | tail -3
```

Expected: `Finished`.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-client-wasm/src/input.rs
git commit -m "feat(wasm): export input encoders"
```

---

## Task 6: Unit shims — ACK, NACK, decode-error batchers

These three exist so `tests/ack.test.ts`, `tests/nack.test.ts` and
`tests/decode_error_batcher.test.ts` can be retargeted in step 3b.

The impedance mismatch is real and deliberate. TS is callback-and-fake-timer
shaped:

```ts
const b = new AckBatcher((dg) => sent.push(dg));
b.add({ frameSeq: 1, tileX: 0, tileY: 0, passIdx: 0, arrivalTimeMsLo16: 0 });
b.flush();
```

Rust is poll-and-injected-time: `add(entry, now_us) -> Option<Vec<u8>>`. The
shim does **not** re-add a callback. Time becomes explicit, which is the
improvement; the step-3b suites absorb that in their own shims.

**Files:**
- Create: `ghostframe-client-wasm/src/units.rs`

- [ ] **Step 1: Write the shims**

`ghostframe-client-wasm/src/units.rs`:

```rust
//! Thin per-unit exports, existing so the pre-cutover vitest suites can be
//! retargeted at Rust without being rewritten as integration tests. Those
//! suites encode the *old* implementation's real behaviour, which is what
//! lets them detect divergence that tests written alongside the new code
//! cannot.
//!
//! Nothing in `main.ts` should use these — it drives `WasmClientCore`.

use ghostframe_client_core::{
    ack_batcher::AckBatcher, decode_error_batcher::DecodeErrorBatcher,
    nack_batcher::{NackBatcher, NackEntry},
};
use ghostframe_protocol::ack::{AckBatch, AckEntry};
use wasm_bindgen::prelude::*;

// `Option<Vec<u8>>` crosses the boundary as `Uint8Array | undefined`, so the
// batchers' natural return type needs no adaptation.

#[wasm_bindgen]
pub struct WasmAckBatcher {
    inner: AckBatcher,
}

#[wasm_bindgen]
impl WasmAckBatcher {
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmAckBatcher {
        WasmAckBatcher {
            inner: AckBatcher::new(),
        }
    }

    /// Returns the encoded datagram when the fresh-entry cap forces an
    /// immediate flush, otherwise `undefined`.
    pub fn add(
        &mut self,
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        pass_idx: u8,
        arrival_time_ms_lo16: u16,
        now_us: u64,
    ) -> Option<Vec<u8>> {
        self.inner.add(
            AckEntry {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                arrival_time_ms_lo16,
            },
            now_us,
        )
    }

    #[wasm_bindgen(js_name = pollTimeout)]
    pub fn poll_timeout(&self) -> Option<u64> {
        self.inner.poll_timeout()
    }

    #[wasm_bindgen(js_name = onTimeout)]
    pub fn on_timeout(&mut self, now_us: u64) -> Option<Vec<u8>> {
        self.inner.on_timeout(now_us)
    }

    pub fn flush(&mut self) -> Option<Vec<u8>> {
        self.inner.flush()
    }
}

impl Default for WasmAckBatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Decodes an ACK envelope — the replacement for the TS suite's
/// `parseAckEnvelopeForTest`. Returns an array of entries with named fields,
/// or `null` on a malformed envelope. A malformed envelope is a **value**,
/// not an exception, so a suite can assert rejection without `expect(...).toThrow`.
#[wasm_bindgen(js_name = parseAckEnvelope)]
pub fn parse_ack_envelope(bytes: &[u8]) -> Result<JsValue, JsValue> {
    let entries: Option<Vec<WasmAckEntry>> = AckBatch::decode(bytes)
        .ok()
        .map(|b| b.entries.iter().map(WasmAckEntry::from).collect());
    serde_wasm_bindgen::to_value(&entries).map_err(|e| JsValue::from_str(&e.to_string()))
}

#[wasm_bindgen]
pub struct WasmNackBatcher {
    inner: NackBatcher,
}

#[wasm_bindgen]
impl WasmNackBatcher {
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmNackBatcher {
        WasmNackBatcher {
            inner: NackBatcher::new(),
        }
    }

    pub fn add(
        &mut self,
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        pass_idx: u8,
        frag_idx: u8,
        now_us: u64,
    ) -> Option<Vec<u8>> {
        self.inner.add(
            NackEntry {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                frag_idx,
            },
            now_us,
        )
    }

    #[wasm_bindgen(js_name = pollTimeout)]
    pub fn poll_timeout(&self) -> Option<u64> {
        self.inner.poll_timeout()
    }

    #[wasm_bindgen(js_name = onTimeout)]
    pub fn on_timeout(&mut self, now_us: u64) -> Option<Vec<u8>> {
        self.inner.on_timeout(now_us)
    }
}

impl Default for WasmNackBatcher {
    fn default() -> Self {
        Self::new()
    }
}
```

Append the third shim to the same file:

```rust
#[wasm_bindgen]
pub struct WasmDecodeErrorBatcher {
    inner: DecodeErrorBatcher,
}

#[wasm_bindgen]
impl WasmDecodeErrorBatcher {
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmDecodeErrorBatcher {
        WasmDecodeErrorBatcher {
            inner: DecodeErrorBatcher::new(),
        }
    }

    /// Returns the 5-byte stream message `[0x04, codec, tile_x, tile_y,
    /// code]` when allowed, `undefined` when rate-limited (per-key: <=1 per
    /// 1000 ms; global: <=32 per rolling 1000 ms).
    ///
    /// `codec` and `code` cross as their `repr(u8)` discriminants. An
    /// unrecognised value returns `undefined` rather than panicking — this
    /// is a wire-adjacent export and must not abort the module.
    pub fn report(
        &mut self,
        codec: u8,
        tile_x: u8,
        tile_y: u8,
        code: u8,
        now_us: u64,
    ) -> Option<Vec<u8>> {
        let codec = codec_from_u8(codec)?;
        let code = decode_error_code_from_u8(code)?;
        self.inner.report(codec, tile_x, tile_y, code, now_us)
    }
}

impl Default for WasmDecodeErrorBatcher {
    fn default() -> Self {
        Self::new()
    }
}
```

`Codec` and `DecodeErrorCode` are `repr(u8)` but have no `TryFrom<u8>`. Add
the two converters at the top of `units.rs`:

```rust
/// `None` for an unrecognised discriminant. Callers return `undefined`
/// rather than panicking: a panic aborts the whole wasm module.
///
/// `Codec::from_u8` (ghostframe-protocol/src/protocol.rs:68) returns
/// `Result<Codec, ProtocolError>`; the error carries no information the
/// caller acts on, so it is discarded here.
fn codec_from_u8(v: u8) -> Option<Codec> {
    Codec::from_u8(v).ok()
}

fn decode_error_code_from_u8(v: u8) -> Option<DecodeErrorCode> {
    use DecodeErrorCode::*;
    Some(match v {
        1 => PayloadTooShort,
        2 => CountOutOfRange,
        3 => ThinUncachedPalette,
        4 => BundledTruncated,
        5 => IndexOob,
        6 => RleOvershoot,
        7 => RleUndershoot,
        8 => Cdf53BadPass,
        9 => Cdf53Truncated,
        10 => Cdf53RleLength,
        _ => return None,
    })
}
```

> `Codec::from_u8` is confirmed to exist at
> `ghostframe-protocol/src/protocol.rs:68`, covering discriminants 0..=5
> (`Skip`, `H264`, `PalRle`, `Solid`, `Raw`, `Cdf53`). `DecodeErrorCode` has
> no equivalent, hence the explicit `match` above. **Do not use
> `std::mem::transmute` for either** — an out-of-range discriminant would be
> instant UB on a value that came from the wire.

Extend the import at the top of the file accordingly:

```rust
use ghostframe_client_core::DecodeErrorCode;
use ghostframe_protocol::protocol::Codec;
```

- [ ] **Step 2: Add the ACK entry mirror**

`parse_ack_envelope` returns named fields, so it needs a serde mirror.
`AckEntry` lives in `ghostframe-protocol` and is not `Serialize`. Append to
`ghostframe-client-wasm/src/boundary.rs`:

```rust
use ghostframe_protocol::ack::AckEntry;

#[derive(Debug, Serialize, PartialEq)]
pub struct WasmAckEntry {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
    pub arrival_time_ms_lo16: u16,
}

impl From<&AckEntry> for WasmAckEntry {
    fn from(e: &AckEntry) -> Self {
        WasmAckEntry {
            frame_seq: e.frame_seq,
            tile_x: e.tile_x,
            tile_y: e.tile_y,
            pass_idx: e.pass_idx,
            arrival_time_ms_lo16: e.arrival_time_ms_lo16,
        }
    }
}
```

and import it in `units.rs`:

```rust
use crate::boundary::WasmAckEntry;
```

> No `js-sys` dependency is needed. Returning `Vec<js_sys::Array>` across
> `#[wasm_bindgen]` is not reliably supported, and positional tuples would
> make the retargeted suites read worse than the TS they replace.

- [ ] **Step 3: Verify it builds**

```bash
cd /home/cedric/work/ghostframe && cargo build -p ghostframe-client-wasm 2>&1 | tail -3
```

Expected: `Finished`.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-client-wasm/src/units.rs ghostframe-client-wasm/src/boundary.rs
git commit -m "feat(wasm): ACK/NACK/decode-error batcher shims"
```

---

## Task 7: Unit shims — palette shadow, parity decoder, loss tracker

**Files:**
- Modify: `ghostframe-client-wasm/src/units.rs`

- [ ] **Step 1: Read the three Rust surfaces**

```bash
cd /home/cedric/work/ghostframe/ghostframe-client-core
sed -n '1,50p' src/palette_shadow.rs
sed -n '25,127p' src/parity_decoder.rs
sed -n '1,94p' src/loss_tracker.rs
```

Mirror each method exactly. The confirmed public surfaces are:

- `PaletteShadow`: `new`, `has(id) -> bool`, `count(id) -> u8`, `put(id, count)`, `clear()`
- `ParityDecoder`: `new(window_capacity)`, `has_source(wire_seq) -> bool`, `record_source(wire_seq, bytes) -> Option<Vec<u8>>`, `receive_parity(&TileParityEnvelope) -> Option<Vec<u8>>`
- `LossTracker`: `new`, `on_datagram(now_us)`, `on_stale_tile(expected, received)`, `on_fec_recovery()`, `encode_feedback(now_us) -> Vec<u8>`

- [ ] **Step 2: Write the shims**

Append to `ghostframe-client-wasm/src/units.rs`:

```rust
use ghostframe_client_core::{
    loss_tracker::LossTracker, palette_shadow::PaletteShadow, parity_decoder::ParityDecoder,
};
use ghostframe_protocol::protocol::TileParityEnvelope;

#[wasm_bindgen]
pub struct WasmPaletteShadow {
    inner: PaletteShadow,
}

#[wasm_bindgen]
impl WasmPaletteShadow {
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmPaletteShadow {
        WasmPaletteShadow {
            inner: PaletteShadow::new(),
        }
    }

    pub fn has(&self, id: u8) -> bool {
        self.inner.has(id)
    }

    pub fn count(&self, id: u8) -> u8 {
        self.inner.count(id)
    }

    pub fn put(&mut self, id: u8, count: u8) {
        self.inner.put(id, count)
    }

    pub fn clear(&mut self) {
        self.inner.clear()
    }
}

impl Default for WasmPaletteShadow {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
pub struct WasmParityDecoder {
    inner: ParityDecoder,
}

#[wasm_bindgen]
impl WasmParityDecoder {
    #[wasm_bindgen(constructor)]
    pub fn new(window_capacity: usize) -> WasmParityDecoder {
        WasmParityDecoder {
            inner: ParityDecoder::new(window_capacity),
        }
    }

    #[wasm_bindgen(js_name = hasSource)]
    pub fn has_source(&self, wire_seq: u32) -> bool {
        self.inner.has_source(wire_seq)
    }

    /// Returns a recovered source datagram if this arrival unlocked a
    /// buffered parity, otherwise `undefined`.
    #[wasm_bindgen(js_name = recordSource)]
    pub fn record_source(&mut self, wire_seq: u32, bytes: &[u8]) -> Option<Vec<u8>> {
        self.inner.record_source(wire_seq, bytes)
    }

    /// Takes the raw envelope bytes and parses internally, mirroring the TS
    /// suite's `parseParityEnvelope` + `receiveParity` pairing. Returns
    /// `undefined` both for a malformed envelope and for one that recovers
    /// nothing — the suite distinguishes those by also asserting on
    /// `hasSource`.
    #[wasm_bindgen(js_name = receiveParity)]
    pub fn receive_parity(&mut self, envelope_bytes: &[u8]) -> Option<Vec<u8>> {
        let env = TileParityEnvelope::decode(envelope_bytes).ok()?;
        self.inner.receive_parity(&env)
    }
}

#[wasm_bindgen]
pub struct WasmLossTracker {
    inner: LossTracker,
}

#[wasm_bindgen]
impl WasmLossTracker {
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmLossTracker {
        WasmLossTracker {
            inner: LossTracker::new(),
        }
    }

    #[wasm_bindgen(js_name = onDatagram)]
    pub fn on_datagram(&mut self, now_us: u64) {
        self.inner.on_datagram(now_us)
    }

    #[wasm_bindgen(js_name = onStaleTile)]
    pub fn on_stale_tile(&mut self, expected: usize, received: usize) {
        self.inner.on_stale_tile(expected, received)
    }

    #[wasm_bindgen(js_name = onFecRecovery)]
    pub fn on_fec_recovery(&mut self) {
        self.inner.on_fec_recovery()
    }

    #[wasm_bindgen(js_name = encodeFeedback)]
    pub fn encode_feedback(&mut self, now_us: u64) -> Vec<u8> {
        self.inner.encode_feedback(now_us)
    }
}

impl Default for WasmLossTracker {
    fn default() -> Self {
        Self::new()
    }
}
```

> `receiveParity` taking raw bytes rather than a parsed envelope is a
> deliberate choice: `TileParityEnvelope` is a `ghostframe-protocol` struct
> with a `Vec<u8>` field, so exposing it across the boundary would mean a
> second mirror type for no gain. If `tests/parity_decoder.test.ts` asserts
> on *parsed envelope fields* (not just recovery outcomes), also export a
> `parse_parity_envelope` returning a serde mirror — check that suite before
> deciding, since preserving its assertions is the entire point of the shim.

- [ ] **Step 3: Verify it builds**

```bash
cd /home/cedric/work/ghostframe && cargo build -p ghostframe-client-wasm 2>&1 | tail -3
```

- [ ] **Step 4: Commit**

```bash
git add ghostframe-client-wasm/src/units.rs
git commit -m "feat(wasm): palette-shadow, parity-decoder, loss-tracker shims"
```

---

## Task 8: Unit shims — prevalidation and CDF53 coverage

**Files:**
- Modify: `ghostframe-client-wasm/src/units.rs`

- [ ] **Step 1: Read the three Rust surfaces**

```bash
cd /home/cedric/work/ghostframe/ghostframe-client-core
sed -n '1,60p' src/pal_rle_decode.rs
sed -n '1,51p' src/cdf53_prevalidate.rs
sed -n '1,60p' src/cdf53_coverage.rs
```

Targets: `prevalidate_pal_rle`, `prevalidate_cdf53`, `apply_cdf53_arrival`.
All three are free functions returning structured results
(`PrevalidatedPalRle`, `PrevalidatedCdf53`, `ArrivalOutcome`).

- [ ] **Step 2: Add serde mirrors for the three result types**

All three return `Result<T, DecodeErrorCode>` or a plain struct. A
prevalidation *rejection* is a normal outcome the suites assert on, so it must
cross as a **value**, not a thrown exception. Append to
`ghostframe-client-wasm/src/boundary.rs`:

```rust
use ghostframe_client_core::{
    cdf53_coverage::{ArrivalOutcome, CoverageEntry},
    cdf53_prevalidate::PrevalidatedCdf53,
    pal_rle_decode::{PalRleVariant, PrevalidatedPalRle},
};

/// A prevalidation outcome. `ok: false` carries the `DecodeErrorCode`
/// discriminant in `code`; `ok: true` carries the payload. Modelled as one
/// struct rather than a tagged enum so the suites can write
/// `expect(r.ok).toBe(false); expect(r.code).toBe(3)` without narrowing.
#[derive(Debug, Serialize)]
pub struct WasmPrevalidatedPalRle {
    pub ok: bool,
    pub code: u8,
    /// 0 = Bundled, 1 = Thin, 2 = IndicesRaw.
    pub variant: u8,
    pub palette_id: u8,
    pub count: u8,
    /// 512 bytes, 2 pixels/byte, low nibble first. Empty when `ok` is false.
    pub indices: Vec<u8>,
    /// `count * 4` BGRA bytes for Bundled; empty otherwise.
    pub palette_upsert: Vec<u8>,
    /// Distinguishes "Bundled with an empty upsert" from "not Bundled".
    pub has_palette_upsert: bool,
}

impl From<Result<PrevalidatedPalRle, ghostframe_client_core::DecodeErrorCode>>
    for WasmPrevalidatedPalRle
{
    fn from(r: Result<PrevalidatedPalRle, ghostframe_client_core::DecodeErrorCode>) -> Self {
        match r {
            Ok(p) => WasmPrevalidatedPalRle {
                ok: true,
                code: 0,
                variant: match p.variant {
                    PalRleVariant::Bundled => 0,
                    PalRleVariant::Thin => 1,
                    PalRleVariant::IndicesRaw => 2,
                },
                palette_id: p.palette_id,
                count: p.count,
                indices: p.indices,
                has_palette_upsert: p.palette_upsert.is_some(),
                palette_upsert: p.palette_upsert.unwrap_or_default(),
            },
            Err(code) => WasmPrevalidatedPalRle {
                ok: false,
                code: code as u8,
                variant: 0,
                palette_id: 0,
                count: 0,
                indices: Vec::new(),
                palette_upsert: Vec::new(),
                has_palette_upsert: false,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WasmPrevalidatedCdf53 {
    pub ok: bool,
    pub code: u8,
    pub generation: u8,
    pub pass_idx: u8,
    /// 384 bytes = 3 channels x 128, packed B, G, R. Empty when `ok` is false.
    pub bit_planes: Vec<u8>,
}

impl From<Result<PrevalidatedCdf53, ghostframe_client_core::DecodeErrorCode>>
    for WasmPrevalidatedCdf53
{
    fn from(r: Result<PrevalidatedCdf53, ghostframe_client_core::DecodeErrorCode>) -> Self {
        match r {
            Ok(p) => WasmPrevalidatedCdf53 {
                ok: true,
                code: 0,
                generation: p.generation,
                pass_idx: p.pass_idx,
                bit_planes: p.bit_planes,
            },
            Err(code) => WasmPrevalidatedCdf53 {
                ok: false,
                code: code as u8,
                generation: 0,
                pass_idx: 0,
                bit_planes: Vec::new(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WasmCoverageEntry {
    pub generation: u8,
    pub frame_seq: u32,
    pub pass_mask: u16,
    pub nacked_mask: u16,
    pub last_change_us: u64,
}

impl From<CoverageEntry> for WasmCoverageEntry {
    fn from(e: CoverageEntry) -> Self {
        WasmCoverageEntry {
            generation: e.generation,
            frame_seq: e.frame_seq,
            pass_mask: e.pass_mask,
            nacked_mask: e.nacked_mask,
            last_change_us: e.last_change_us,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WasmArrivalOutcome {
    pub entry: WasmCoverageEntry,
    pub nack_passes: Vec<u8>,
}

impl From<ArrivalOutcome> for WasmArrivalOutcome {
    fn from(o: ArrivalOutcome) -> Self {
        WasmArrivalOutcome {
            entry: o.entry.into(),
            nack_passes: o.nack_passes,
        }
    }
}
```

- [ ] **Step 3: Write the exports**

Append to `ghostframe-client-wasm/src/units.rs`:

```rust
use ghostframe_client_core::{cdf53_coverage, cdf53_prevalidate, pal_rle_decode};

use crate::boundary::{
    WasmArrivalOutcome, WasmCoverageEntry, WasmPrevalidatedCdf53, WasmPrevalidatedPalRle,
};

/// Validates and expands a PalRLE payload. Updates nothing — neither the
/// shadow nor any palette table — matching `prevalidatePalRle` in
/// `prevalidate.ts`.
#[wasm_bindgen(js_name = prevalidatePalRle)]
pub fn prevalidate_pal_rle(
    payload: &[u8],
    shadow: &WasmPaletteShadow,
) -> Result<JsValue, JsValue> {
    let out = WasmPrevalidatedPalRle::from(pal_rle_decode::prevalidate_pal_rle(
        payload,
        shadow.inner_ref(),
    ));
    serde_wasm_bindgen::to_value(&out).map_err(|e| JsValue::from_str(&e.to_string()))
}

#[wasm_bindgen(js_name = prevalidateCdf53)]
pub fn prevalidate_cdf53(
    payload: &[u8],
    generation: u8,
    pass_idx: u8,
) -> Result<JsValue, JsValue> {
    let out = WasmPrevalidatedCdf53::from(cdf53_prevalidate::prevalidate_cdf53(
        payload, generation, pass_idx,
    ));
    serde_wasm_bindgen::to_value(&out).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// `prev` is the previous coverage entry, or `undefined` for a first
/// arrival. Passing the entry back in each call keeps this a pure function,
/// matching the TS `applyCdf53Arrival(prev, ...)` shape.
#[wasm_bindgen(js_name = applyCdf53Arrival)]
#[allow(clippy::too_many_arguments)]
pub fn apply_cdf53_arrival(
    prev_generation: Option<u8>,
    prev_frame_seq: u32,
    prev_pass_mask: u16,
    prev_nacked_mask: u16,
    prev_last_change_us: u64,
    generation: u8,
    pass_idx: u8,
    frame_seq: u32,
    now_us: u64,
    prevalidation_ok: bool,
) -> Result<JsValue, JsValue> {
    let prev = prev_generation.map(|g| CoverageEntry {
        generation: g,
        frame_seq: prev_frame_seq,
        pass_mask: prev_pass_mask,
        nacked_mask: prev_nacked_mask,
        last_change_us: prev_last_change_us,
    });
    let out = WasmArrivalOutcome::from(cdf53_coverage::apply_cdf53_arrival(
        prev,
        generation,
        pass_idx,
        frame_seq,
        now_us,
        prevalidation_ok,
    ));
    serde_wasm_bindgen::to_value(&out).map_err(|e| JsValue::from_str(&e.to_string()))
}
```

`prevalidate_pal_rle` needs the wrapped `PaletteShadow`. Add an accessor to
`WasmPaletteShadow` (a plain `impl`, **not** `#[wasm_bindgen]`, so it stays
Rust-only):

```rust
impl WasmPaletteShadow {
    pub(crate) fn inner_ref(&self) -> &PaletteShadow {
        &self.inner
    }
}
```

Also import `CoverageEntry` into `units.rs`:

```rust
use ghostframe_client_core::cdf53_coverage::CoverageEntry;
```

> `apply_cdf53_arrival`'s ten flattened parameters are ugly. The alternative
> — a `#[wasm_bindgen]` `WasmCoverageEntry` the caller constructs — trades
> the parameter list for a second mirror type that must round-trip
> bidirectionally. Flattening keeps the function pure and the boundary
> one-directional. If step 3b finds the call sites unreadable, revisit then,
> with the suites in view.

- [ ] **Step 4: Verify it builds**

```bash
cd /home/cedric/work/ghostframe && cargo build -p ghostframe-client-wasm 2>&1 | tail -3
```

Expected: `Finished`.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-client-wasm/src/units.rs ghostframe-client-wasm/src/boundary.rs
git commit -m "feat(wasm): prevalidation and cdf53-coverage shims"
```

---

## Task 9: Build wiring and the smoke test

**Files:**
- Modify: `ghostframe-web-client/package.json`
- Modify: `ghostframe-web-client/.gitignore`
- Create: `ghostframe-web-client/tests/wasm_smoke.test.ts`

- [ ] **Step 1: Add the build scripts**

In `ghostframe-web-client/package.json`, replace the `scripts` block:

```json
  "scripts": {
    "dev": "vite",
    "build": "npm run build:wasm && tsc && vite build",
    "build:wasm": "wasm-pack build ../ghostframe-client-wasm --target web --out-dir ../ghostframe-web-client/pkg-web",
    "build:wasm:node": "wasm-pack build ../ghostframe-client-wasm --target nodejs --out-dir ../ghostframe-web-client/pkg-node",
    "preview": "vite preview",
    "test": "npm run build:wasm:node && vitest run",
    "test:watch": "vitest"
  },
```

> `test` builds the node-target wasm first. Without it, a retargeted suite in
> step 3b would run against whatever `pkg-node/` happened to be on disk —
> exactly the staleness the stamp exists to catch, but at test time rather
> than deploy time.

- [ ] **Step 2: Ignore the build outputs**

Append to `ghostframe-web-client/.gitignore`:

```
pkg-web/
pkg-node/
```

Per spec D6 the wasm is built in CI, never committed — `dist/` is already a
build artefact, and a committed binary can drift from source silently.

- [ ] **Step 3: Write the smoke test**

`ghostframe-web-client/tests/wasm_smoke.test.ts`:

```ts
// Proves the wasm module loads under vitest's node environment and that it
// was built from the current workspace sources. Everything else about the
// wasm is tested by the retargeted suites; this is the load-bearing check
// that they are testing today's Rust.
import { describe, it, expect } from 'vitest';
import { protocolStamp } from '../scripts/protocol_stamp.mjs';
import * as wasm from '../pkg-node/ghostframe_client_wasm.js';

describe('wasm module', () => {
  it('loads and constructs a core', () => {
    const core = new wasm.WasmClientCore(false, false, true, 0n);
    expect(core).toBeDefined();
  });

  it('emits the Hello message on the stream at construction', () => {
    const core = new wasm.WasmClientCore(true, true, true, 0n);
    const out = core.pollTransmit(0n);
    expect(out).toBeDefined();
    expect(out.kind).toBe('Stream');
    // [0x03, caps]; bit0 = indices_raw_enabled, bit1 = supports_h264.
    expect(out.bytes[0]).toBe(0x03);
    expect(out.bytes[1] & 0b11).toBe(0b11);
  });

  it('was built from the current client-core and protocol sources', () => {
    expect(wasm.protocol_stamp()).toBe(protocolStamp());
  });

  it('encodes input without a session', () => {
    expect(Array.from(wasm.encodePointerMove(1, 2))).toHaveLength(6);
    expect(wasm.keyToKeysym('Enter')).toBeDefined();
  });
});
```

> The Hello assertion is the one that would catch a wrapper that constructs
> a core but drops its outbox — a failure mode a bare "it loads" test cannot
> distinguish from success.

- [ ] **Step 4: Run the suite**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client
export PATH="$HOME/.cargo/bin:$PATH"
npm test 2>&1 | tail -20
```

Expected: the new `wasm_smoke.test.ts` passes **and the 17 existing suites
still pass** — nothing in `src/` changed.

- [ ] **Step 5: Verify the staleness guard actually discriminates**

A stamp test that passes no matter what is worthless. Prove it fails on drift:

```bash
cd /home/cedric/work/ghostframe
printf '\n// stamp drift probe\n' >> ghostframe-client-core/src/palette_shadow.rs
cd ghostframe-web-client && npx vitest run tests/wasm_smoke.test.ts 2>&1 | tail -12
```

Expected: the stamp test **FAILS** — `pkg-node/` is now stale relative to the
source. (`npx vitest` is used deliberately here instead of `npm test`, which
would rebuild the wasm and mask the drift.)

Then revert and confirm it passes again:

```bash
cd /home/cedric/work/ghostframe && git checkout ghostframe-client-core/src/palette_shadow.rs
cd ghostframe-web-client && npm test 2>&1 | tail -6
```

- [ ] **Step 6: Commit**

```bash
git add ghostframe-web-client/package.json ghostframe-web-client/.gitignore \
        ghostframe-web-client/tests/wasm_smoke.test.ts
git commit -m "build(web): wire wasm-pack into build and test

Two targets: --target web for the bundle, --target nodejs for vitest.
Verified by spike: --target bundler does not load under vitest without
vite-plugin-wasm, and --target web builds under vite with no plugins,
keeping the web client dependency-free."
```

---

## Task 10: CI gate

**Files:**
- Modify: `.github/workflows/client-core.yml`

- [ ] **Step 1: Read the current workflow**

```bash
cd /home/cedric/work/ghostframe && cat .github/workflows/client-core.yml
```

Note the existing job installs the `wasm32-unknown-unknown` target and runs
`cargo build -p ghostframe-protocol -p ghostframe-client-core -p ghostframe-client-net --target wasm32-unknown-unknown`.

- [ ] **Step 2: Extend the wasm build to the new crate**

Add `-p ghostframe-client-wasm` to that `cargo build` line.

- [ ] **Step 3: Add a job that builds the module and runs the smoke test**

Append to the `jobs:` block (match the file's existing indentation and its
checkout/toolchain step style):

```yaml
  wasm-module:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: wasm32-unknown-unknown }
      - uses: Swatinem/rust-cache@v2
      - uses: actions/setup-node@v4
        with:
          node-version: '22'
          cache: npm
          cache-dependency-path: ghostframe-web-client/package-lock.json
      - name: Install wasm-pack
        run: cargo install wasm-pack --version 0.15.0 --locked
      - name: Install web client deps
        working-directory: ghostframe-web-client
        run: npm ci
      - name: Build wasm and run the vitest suites
        working-directory: ghostframe-web-client
        run: npm test
```

> `npm test` runs `build:wasm:node` first, so this job gates the stamp
> guard, the smoke test, and the 17 existing suites together.

- [ ] **Step 4: Verify the workflow parses**

```bash
cd /home/cedric/work/ghostframe && python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/client-core.yml')); print('ok')"
```

Expected: `ok`.

- [ ] **Step 5: Check whether a `--target web` build is also gated**

The browser bundle uses `--target web`, which this job does not build. A break
there would surface only at step 4. Add it to the same job:

```yaml
      - name: Build the browser-target wasm
        working-directory: ghostframe-web-client
        run: npm run build:wasm
```

- [ ] **Step 6: Commit**

```bash
git add .github/workflows/client-core.yml
git commit -m "ci: gate the wasm module build and smoke test

Builds both wasm-pack targets and runs the vitest suites, so a break in
the browser target surfaces here rather than at cutover."
```

---

## Task 11: Baseline the bundle

Spec risk row: *"Bundle size or load time regresses — measure at step 3, while
both implementations are live and comparison is free."* This is that
measurement. It is a record, not a gate.

**Files:**
- Create: `docs/specs/wasm-bundle-baseline.md`

- [ ] **Step 1: Measure the current bundle, before wasm is in it**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client
git stash list >/dev/null
npx vite build 2>&1 | tail -8
```

Record each emitted asset and its gzip size.

- [ ] **Step 2: Measure the wasm module on its own**

```bash
cd /home/cedric/work/ghostframe/ghostframe-web-client
export PATH="$HOME/.cargo/bin:$PATH"
npm run build:wasm
ls -l pkg-web/*.wasm
gzip -c pkg-web/*.wasm | wc -c
```

- [ ] **Step 3: Record both**

Write `docs/specs/wasm-bundle-baseline.md` with a table of: asset, raw bytes,
gzip bytes, measured-on date, and the commit SHA. Add one line stating which
TS modules step 4 will delete (the 1147-line protocol layer across `ack.ts`,
`nack.ts`, `fec.ts`, `parity_decoder.ts`, `feedback.ts`, `prevalidate.ts`,
`prevalidate_cdf53.ts`, `cdf53_coverage.ts`, `decode_error_batcher.ts`,
`palette_shadow.ts`, `decoder.ts`) so the step-4 comparison is
like-for-like rather than "bundle got bigger".

- [ ] **Step 4: Commit**

```bash
git add docs/specs/wasm-bundle-baseline.md
git commit -m "docs: baseline web bundle size before the wasm cutover"
```

---

## Done criteria

- [ ] `cargo build --workspace` passes on native.
- [ ] `cargo test -p ghostframe-client-wasm` passes.
- [ ] `just containers-build` succeeds — the Dockerfile COPY lines are right.
- [ ] `npm test` in `ghostframe-web-client` passes: 17 existing suites plus `wasm_smoke`.
- [ ] The stamp test was **observed failing** under induced drift (Task 9 Step 5).
- [ ] Both `--target web` and `--target nodejs` build in CI.
- [ ] Nothing under `ghostframe-web-client/src/` was modified.

## What this plan deliberately does not do

- Touch `main.ts` or any TS protocol module. That is step 4.
- Delete any test. Step 3b retargets; step 4 deletes.
- Add `vite-plugin-wasm`. The spike showed `--target web` needs no plugin.
- Retarget the eleven suites. That is step 3b, planned separately once these
  exports exist and their real signatures are known.
