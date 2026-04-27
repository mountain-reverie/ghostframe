# e2e_resolution_change Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Validate that the capture → encode → transport → decode pipeline survives a mid-stream resolution change. Pre-M3 the trigger is a server-side `xrandr` invocation (the spec's client→server `DisplayLayout` protocol is M4). The test asserts that after the switch the browser canvas dimensions catch up to the new mode and the new content renders correctly.

**Architecture:** Add a multi-mode `xorg-multi.conf` to the test-server image, install `xrandr` in the runtime image, and add a `docker_run_in_container` helper to the e2e harness so the test can invoke commands inside a running server container. The test starts at 1024×768, asserts a red square, switches to 640×480, re-paints the test pattern, and asserts the canvas resized + still shows red.

**Tech Stack:** Rust, `tokio::process::Command` (safe — no shell), `chromiumoxide` page evaluation, the X dummy driver with multiple `Modeline` declarations.

**Likely first failure:** The `GpuFrameProcessor` and/or `FullFrameEncoder` allocate width-and-height-sized internal buffers on construction in `IoBridge`. If they do not handle a frame at a different size, the test will surface the bug — which is exactly what it's meant to do. Fix is in scope of this plan only if it's a one-line patch; deeper fixes get a follow-up plan.

---

## File Structure

```
tests/containers/test-server/
├── xorg-multi.conf              # NEW — declares 1024x768 and 640x480 modes
└── Dockerfile                   # add x11-xserver-utils + COPY xorg-multi.conf

ghostframe-lib/tests/
├── e2e/helpers.rs               # add docker_run_in_container
└── e2e.rs                       # add e2e_resolution_change
```

---

## Task 1: Multi-mode Xorg config and xrandr binary

**Files:**
- Create: `tests/containers/test-server/xorg-multi.conf`
- Modify: `tests/containers/test-server/Dockerfile`

- [ ] **Step 1: Write xorg-multi.conf**

Create `tests/containers/test-server/xorg-multi.conf`:

```
Section "Device"
    Identifier "Dummy"
    Driver     "dummy"
    VideoRam   256000
EndSection

Section "Monitor"
    Identifier "Monitor0"
    HorizSync  28.0-80.0
    VertRefresh 48.0-75.0
    # Two modelines so xrandr can switch between them at runtime.
    Modeline "1024x768" 65.00 1024 1048 1184 1344 768 771 777 806 -HSync -VSync
    Modeline "640x480"  25.175 640  656  752  800 480 490 492 525
EndSection

Section "Screen"
    Identifier "Screen0"
    Device     "Dummy"
    Monitor    "Monitor0"
    DefaultDepth 24
    SubSection "Display"
        Depth  24
        # First mode in the list is the default at server start.
        Modes  "1024x768" "640x480"
    EndSubSection
EndSection
```

- [ ] **Step 2: Add xrandr to the runtime image**

Edit `tests/containers/test-server/Dockerfile`. Find the runtime apt-get install block (around line 49). Add `x11-xserver-utils` to the package list:

```dockerfile
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    xserver-xorg-core \
    xserver-xorg-video-dummy \
    x11-xserver-utils \
    x11-utils \
    mesa-utils \
    libgl1-mesa-dri \
    libavcodec60 \
    libavdevice60 \
    libavutil58 \
    libswscale7 \
    libswresample4 \
    libx264-164 \
    && rm -rf /var/lib/apt/lists/*
```

- [ ] **Step 3: COPY xorg-multi.conf into the image**

Edit the same `Dockerfile`. Find the existing `COPY tests/containers/test-server/xorg-odd.conf` line and add a sibling line after it:

```dockerfile
COPY tests/containers/test-server/xorg-multi.conf /etc/X11/xorg-multi.conf
```

- [ ] **Step 4: Rebuild the test-server image**

Run: `docker build -f tests/containers/test-server/Dockerfile -t ghostframe/test-server:latest .`
Expected: clean build, `xrandr` and `xorg-multi.conf` are now present in the image.

- [ ] **Step 5: Sanity-check xrandr starts with two modes**

Run:
```bash
docker run --rm --entrypoint bash ghostframe/test-server:latest -c \
  'Xorg :88 -config /etc/X11/xorg-multi.conf & sleep 2 && DISPLAY=:88 xrandr'
```
Expected: output lists `default connected ... 1024x768 ... 640x480`. The `default` output name is what the dummy driver typically uses; capture it for the next task.

If the output name isn't `default` (some Xorg builds use `VGA-0` or `Virtual-1`), record what xrandr actually prints — Task 4 step 1 references this name.

- [ ] **Step 6: Commit**

```bash
git add tests/containers/test-server/xorg-multi.conf tests/containers/test-server/Dockerfile
git commit -m "test(container): xorg-multi.conf with two modes; install xrandr"
```

---

## Task 2: docker_run_in_container helper

**Files:**
- Modify: `ghostframe-lib/tests/e2e/helpers.rs`

- [ ] **Step 1: Append the helper**

Append to `ghostframe-lib/tests/e2e/helpers.rs`:

```rust
/// Run `docker exec [-e <env>...] <container> <args...>` against an
/// already-running container, and return `(stdout, stderr, exit_code)`.
///
/// Uses `tokio::process::Command` (no shell — args are passed directly to
/// `execve`). Pass environment variables as `("KEY", "value")` tuples —
/// useful for invoking X11 client tools that need `DISPLAY=:99`.
pub async fn docker_run_in_container(
    container: &str,
    env: &[(&str, &str)],
    args: &[&str],
) -> Result<(String, String, i32)> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("exec");
    for (k, v) in env {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }
    cmd.arg(container).args(args);
    let out = cmd.output().await.context("running docker exec")?;
    Ok((
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    ))
}
```

- [ ] **Step 2: Verify the helper compiles**

Run: `cargo build -p ghostframe-lib --tests`
Expected: clean build.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/tests/e2e/helpers.rs
git commit -m "test(e2e): docker_run_in_container helper for in-container commands"
```

---

## Task 3: Probe the actual xrandr output name

This step generates a test artifact that the next task references.

**Files:**
- (no source changes — adds a one-time documentation note)

- [ ] **Step 1: Confirm the dummy driver's output name**

Run:
```bash
docker run --rm --entrypoint bash ghostframe/test-server:latest -c \
  'Xorg :88 -config /etc/X11/xorg-multi.conf & sleep 2 && DISPLAY=:88 xrandr | head -1'
```
Expected: a line of the form `<output-name> connected ...`. Record the `<output-name>`.

- [ ] **Step 2: If the name is not `default`, update the test code below**

The test in Task 4 hard-codes the output name as `default`. If your environment reports a different name (e.g. `Virtual-0`), substitute it in Task 4 step 1's `xrandr --output ...` invocation.

---

## Task 4: e2e_resolution_change test

**Files:**
- Modify: `ghostframe-lib/tests/e2e.rs`

- [ ] **Step 1: Append the test**

Append to `ghostframe-lib/tests/e2e.rs`:

```rust
/// Validates the pipeline survives a mid-stream resolution change.
///
/// Pre-M3 there is no client→server `DisplayLayout` protocol — the test
/// triggers the change via xrandr inside the server container. Post-M4,
/// a sibling test `e2e_resolution_change_via_protocol` will exercise the
/// real protocol path.
#[tokio::test]
async fn e2e_resolution_change() -> Result<()> {
    // Phase A: 1024×768 — server starts in this mode (first entry in
    // xorg-multi.conf's Modes list).
    let setup = setup_e2e_with_env(
        "--solid-red",
        &[("XORG_CONF", "/etc/X11/xorg-multi.conf")],
    ).await?;

    // Wait for QUIC slow-start + initial frames.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Assert: canvas is 1024×768 and a center pixel is red.
    let dims_a: (u32, u32) = setup.page
        .evaluate(r#"
            (() => {
                const c = document.getElementById('canvas');
                return [c.width, c.height];
            })()
        "#)
        .await?
        .into_value()?;
    assert_eq!(dims_a, (1024, 768), "phase A: canvas dimensions");

    let red_a: bool = setup.page
        .evaluate(r#"
            (() => {
                const c = document.getElementById('canvas').getContext('2d');
                const p = c.getImageData(512, 384, 1, 1).data;
                return p[0] > 180 && p[1] < 80 && p[2] < 80;
            })()
        "#)
        .await?
        .into_value()?;
    assert!(red_a, "phase A: center pixel not red");

    // ── Trigger the resolution change. ──────────────────────────────────────

    // 1. xrandr to switch the dummy driver to 640×480.
    let (out, err, status) = helpers::docker_run_in_container(
        "ghostframe-server",
        &[("DISPLAY", ":99")],
        &["xrandr", "--output", "default", "--mode", "640x480"],
    ).await?;
    assert_eq!(
        status, 0,
        "xrandr exited with {status}: stdout={out:?} stderr={err:?}"
    );

    // 2. Re-paint the root window — dummy driver clears framebuffer on mode change.
    let (_, _, status) = helpers::docker_run_in_container(
        "ghostframe-server",
        &[("DISPLAY", ":99")],
        &["/usr/local/bin/ghostframe-test-pattern", "--solid-red"],
    ).await?;
    // The test-pattern process forks/daemonises; if status != 0 the binary failed.
    assert_eq!(status, 0, "re-paint after resolution change failed");

    // ── Phase B: assert the client picks up the new dimensions. ─────────────

    // Allow time for: encoder reset, keyframe, several frames, canvas resize.
    tokio::time::sleep(Duration::from_secs(8)).await;

    let dims_b: (u32, u32) = setup.page
        .evaluate(r#"
            (() => {
                const c = document.getElementById('canvas');
                return [c.width, c.height];
            })()
        "#)
        .await?
        .into_value()?;
    assert_eq!(dims_b, (640, 480), "phase B: canvas did not resize to 640x480");

    let red_b: bool = setup.page
        .evaluate(r#"
            (() => {
                const c = document.getElementById('canvas').getContext('2d');
                const p = c.getImageData(320, 240, 1, 1).data;
                return p[0] > 180 && p[1] < 80 && p[2] < 80;
            })()
        "#)
        .await?
        .into_value()?;
    assert!(red_b, "phase B: center pixel not red after resize");

    Ok(())
}
```

- [ ] **Step 2: Verify the test compiles**

Run: `cargo build -p ghostframe-lib --tests`
Expected: clean build.

- [ ] **Step 3: Run the test (single-threaded, with logs)**

Run: `cargo test -p ghostframe-lib --test e2e e2e_resolution_change -- --test-threads=1 --nocapture`
Expected: PASS within ~60 s.

If the test fails, see Task 5 for likely failure modes and the debug procedure.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/tests/e2e.rs
git commit -m "test(e2e): e2e_resolution_change validates pipeline survives mid-stream resize"
```

---

## Task 5: Triage if the test fails (debug procedure)

This task is conditional: only execute if Task 4 step 3 fails.

**Files:**
- (none — this is a debugging procedure)

- [ ] **Step 1: Capture container logs and screenshots**

Add this block temporarily inside the test, just before each assertion:

```rust
let png = setup.page
    .screenshot(chromiumoxide::page::ScreenshotParams::default())
    .await?;
std::fs::write(format!("/tmp/e2e_resize_{}.png", line!()), &png)?;
let logs = tokio::process::Command::new("docker")
    .args(["logs", "--tail", "100", "ghostframe-server"])
    .output().await?;
eprintln!("=== container logs ===\n{}\n=== end ===", String::from_utf8_lossy(&logs.stdout));
```

Re-run the test and inspect the screenshots and server logs.

- [ ] **Step 2: Match failure to a likely root cause**

| Failure | Likely cause | Where to look |
|---|---|---|
| Phase A canvas dims wrong (e.g. 640×480) | First mode in xorg-multi.conf isn't 1024×768 | `tests/containers/test-server/xorg-multi.conf` |
| `xrandr` exits non-zero with "cannot find output" | Output name isn't `default` | Task 3 step 1 — substitute the real name |
| Phase B canvas dims still 1024×768 | Server keeps emitting frames at the old size — encoder/GPU buffers cached | `ghostframe-lib/src/transport/io_bridge.rs` — search for `FullFrameEncoder::new` and `GpuFrameProcessor::new`; they likely lock width/height at construction |
| Phase B canvas dims update but red_b is false | Re-paint didn't reach the framebuffer — root window may need re-mapping after mode change | `tests/containers/test-server/entrypoint.sh` ordering |
| Process panics in xdaemon | Capture loop assumes constant dimensions | `ghostframe-xdaemon/src/x11_capture.rs` or `drm_capture.rs` |

- [ ] **Step 3: Decide scope**

- If the fix is a one-line patch (e.g. force `FullFrameEncoder` to re-create on size change), include it in this plan as Task 6.
- If the fix touches multiple files or design (e.g. a proper resize protocol), open a follow-up plan and mark this test `#[ignore = "blocked on resize support"]` so the harness still compiles. Record the follow-up in `MEMORY.md` as a project memory.

- [ ] **Step 4: Remove the temporary debug block**

Delete the screenshot/logs block added in step 1 before committing the fix.

---

## Final verification

- [ ] **Step 1: Workspace build**

Run: `cargo build --workspace --tests`
Expected: clean build.

- [ ] **Step 2: Run every e2e test serially**

Run: `cargo test -p ghostframe-lib --test e2e -- --test-threads=1`
Expected: every prior e2e test still passes plus `e2e_resolution_change` (or it is `#[ignore]`d with a documented follow-up plan).

- [ ] **Step 3: Confirm the M2 `e2e_edge_tiles` test still passes**

Run: `cargo test -p ghostframe-lib --test e2e e2e_edge_tiles -- --test-threads=1 --nocapture`
Expected: PASS. The `xorg-odd.conf` path is independent of the new `xorg-multi.conf` work, but a regression here would mean the Dockerfile changes broke an existing config.
