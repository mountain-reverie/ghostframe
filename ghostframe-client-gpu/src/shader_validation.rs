//! Nothing in CI parses a client shader otherwise.
//!
//! `naga` only parses WGSL at `create_shader_module` time, and every place
//! this crate calls that is inside a GPU test (`tests/gpu_*.rs`), which CI
//! never runs (no runner here has a suitable device -- see those files'
//! module docs). So a plain syntax error in any of the files under
//! `shaders/client/` would compile clean and reach `master` green, and would
//! only surface on a developer's own machine the next time they happened to
//! exercise the one pipeline that loads it.
//!
//! This test needs no GPU at all: it runs naga's own frontend and validator
//! directly against the source text, which is exactly what
//! `create_shader_module` does before it ever touches a device. So
//! `cargo test -p ghostframe-client-gpu --lib` (CI: `ci.yml`'s `cargo test
//! --workspace --lib`) covers it.
//!
//! The directory is walked recursively, not the files named, so a new
//! shader is covered automatically -- including one added in a
//! subdirectory later, not just a new file directly under `shaders/client/`
//! -- which is the point of Task 7's review comment this closes.
//!
//! Validated against `naga::valid::Capabilities::default() |
//! Capabilities::TEXTURE_EXTERNAL`, not `::all()`: 10 of these 11 shaders
//! are also loaded by the web client (`wgpu` targeting WebGPU in a
//! browser), so a capability beyond what a real browser supports would
//! validate green here and then fail in Chrome.
//!
//! `default()` alone is NOT that set, though it looks like it should be --
//! tried first, it rejects `h264_blit.wgsl`, a real shipped shader that
//! samples a `texture_external` (`GPUExternalTexture`, from
//! `importExternalTexture` on a WebCodecs `VideoFrame`, per this crate's own
//! header comment). `TEXTURE_EXTERNAL` is real WebGPU-specified behaviour --
//! `wgpu_types::Features::EXTERNAL_TEXTURE`'s own doc calls it "WebGPU-
//! specified behavior that is not optional in the standard" -- that `wgpu`
//! merely gates behind an opt-in native `Features` flag "until the
//! implementation is more complete" (see
//! `wgpu-naga-bridge::features_to_naga_capabilities`, the actual function
//! `wgpu` itself uses to turn a device's features into a `Capabilities` set
//! at `create_shader_module` time -- there is no single built-in "browser
//! baseline" constant in `naga` to defer to instead). So `default()` was a
//! false-positive risk in the other direction: correct FOR shaders that
//! don't need it, but a real gate failure against code already shipping.
//! Adding just `TEXTURE_EXTERNAL` back keeps everything `default()` already
//! excludes (`RAY_QUERY`, `MESH_SHADER`, `SHADER_INT64`,
//! `ACCELERATION_STRUCTURE_BINDING_ARRAY`, ... -- all native-only, none of
//! which a browser's WebGPU implementation supports) while accepting the one
//! capability this crate's own shaders are already known to need.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// `shaders/client/`, one level up from this crate's own manifest --
    /// matches every `include_str!("../../../shaders/client/...")` in
    /// `src/pipelines/*.rs`, which starts from `src/pipelines/` instead.
    fn client_shader_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../shaders/client")
    }

    /// Every `.wgsl` file under `dir`, recursively.
    fn find_wgsl_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries =
            fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.expect("read_dir entry");
            let path = entry.path();
            if path.is_dir() {
                find_wgsl_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("wgsl") {
                out.push(path);
            }
        }
    }

    #[test]
    fn every_client_shader_parses_and_validates() {
        let dir = client_shader_dir();
        let mut paths = Vec::new();
        find_wgsl_files(&dir, &mut paths);

        let mut checked = Vec::new();
        let mut failures = Vec::new();
        for path in paths {
            let name = path
                .strip_prefix(&dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();

            let src = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

            match naga::front::wgsl::parse_str(&src) {
                Ok(module) => {
                    let mut validator = naga::valid::Validator::new(
                        naga::valid::ValidationFlags::all(),
                        naga::valid::Capabilities::default()
                            | naga::valid::Capabilities::TEXTURE_EXTERNAL,
                    );
                    if let Err(e) = validator.validate(&module) {
                        failures.push(format!("{name}: validation error: {e}"));
                    }
                }
                Err(e) => failures.push(format!("{name}: parse error: {e}")),
            }
            checked.push(name);
        }

        assert!(
            !checked.is_empty(),
            "found no .wgsl files under {} -- the directory walk itself is broken, \
             not just this assertion",
            dir.display()
        );
        assert!(
            failures.is_empty(),
            "{} of {} shader(s) under shaders/client/ failed to parse/validate:\n{}",
            failures.len(),
            checked.len(),
            failures.join("\n")
        );
    }

    /// The smallest `maxComputeInvocationsPerWorkgroup` any target this
    /// project ships to provides.
    ///
    /// WebGPU guarantees 256, so the browser client alone would allow twice
    /// this. GLES 3.1's spec *minimum* is 128, and Mali-T860 reports exactly
    /// 128 (`tools/hw-probe/glprobe.c`) -- so the native client on that GPU is
    /// what sets the bar, and a shader that fits here fits everywhere.
    ///
    /// This is a hard gate, not a style preference: `request_device` fails
    /// outright when a limit is unavailable, so a 256-invocation workgroup does
    /// not degrade on Mali, it prevents the device from being created at all --
    /// before any shader runs, which reads as "the GLES backend is broken"
    /// rather than "one shader is too wide".
    ///
    /// Raising this re-breaks the GLES client. If you need to, raise the
    /// hardware floor in the design doc first and say which GPU you dropped.
    const MAX_WORKGROUP_INVOCATIONS: u32 = 128;

    /// No compute entry point under `shaders/client/` asks for more
    /// invocations per workgroup than the weakest target can give.
    ///
    /// Needs no GPU: the limit is a property of the shader source, and naga
    /// hands us the declared `@workgroup_size` after a parse. So this runs in
    /// CI (`cargo test --workspace --lib`) even though nothing there can
    /// create a device to check it against.
    #[test]
    fn no_compute_shader_exceeds_the_portable_workgroup_limit() {
        let dir = client_shader_dir();
        let mut paths = Vec::new();
        find_wgsl_files(&dir, &mut paths);
        // Deterministic failure output: `read_dir` order is not stable, and a
        // list that reorders between runs is miserable to diff.
        paths.sort();

        let mut compute_entry_points = 0usize;
        let mut failures = Vec::new();
        for path in &paths {
            let name = path
                .strip_prefix(&dir)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            let src =
                fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

            // A parse failure is `every_client_shader_parses_and_validates`'s
            // job. Reporting it here too would double every message for one
            // defect and make this test look like it found something it did not.
            let Ok(module) = naga::front::wgsl::parse_str(&src) else {
                continue;
            };

            for ep in &module.entry_points {
                // `workgroup_size` is only meaningful on compute stages; it is
                // `[0, 0, 0]` on vertex and fragment, whose product would pass
                // this check for all the wrong reasons.
                if ep.stage != naga::ShaderStage::Compute {
                    continue;
                }
                compute_entry_points += 1;

                // An override-sized workgroup means the literal array is not
                // the real size, so this test would be reading the wrong
                // number and silently stop guarding. None of the shaders does
                // this today; fail loudly if one starts, rather than passing
                // on a value that means nothing.
                //
                // `Some([None, None, None])` is not an override -- check the
                // elements, not just the outer Option.
                let overridden = ep
                    .workgroup_size_overrides
                    .is_some_and(|o| o.iter().any(Option::is_some));
                if overridden {
                    failures.push(format!(
                        "{name}: entry point `{}` sizes its workgroup with override \
                         expressions, so this test cannot see the real size. Teach it \
                         to evaluate the overrides, or go back to a literal \
                         @workgroup_size.",
                        ep.name
                    ));
                    continue;
                }

                let [x, y, z] = ep.workgroup_size;
                // saturating: a declared size cannot realistically overflow
                // u32, but a wrong answer here would be a *passing* test.
                let invocations = x.saturating_mul(y).saturating_mul(z);
                if invocations > MAX_WORKGROUP_INVOCATIONS {
                    failures.push(format!(
                        "{name}: entry point `{}` declares @workgroup_size({x}, {y}, {z}) \
                         = {invocations} invocations, over the {MAX_WORKGROUP_INVOCATIONS} \
                         limit. Reshape it so each invocation covers more than one \
                         element; keep the workgroup *count* the same so the dispatch \
                         sites in src/pipelines/ and ghostframe-web-client/src/webgpu/ \
                         do not have to change.",
                        ep.name
                    ));
                }
            }
        }

        // Without this, deleting every compute shader -- or breaking the
        // directory walk, or having naga stop reporting compute stages --
        // reports success.
        assert!(
            compute_entry_points > 0,
            "found no compute entry points under {} -- this test checked nothing, \
             which is not the same as passing",
            dir.display()
        );
        assert!(
            failures.is_empty(),
            "{} compute entry point(s) of {compute_entry_points} exceed the portable \
             workgroup limit:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// `nv12_reference.rs`'s own module doc: "the same six coefficients and
    /// the 0.502 chroma centre appear in three places, not two: this file,
    /// the WGSL, and the .comp shader they both invert. All three must
    /// change together." Nothing enforced the Rust/WGSL half of that claim
    /// without a GPU -- `h264_nv12_blit.wgsl`'s own header says "nothing
    /// else links them", and on CI that was true, since the only thing that
    /// ever compared the two was the GPU oracle in `nv12_oracle_tests.rs`,
    /// which self-skips where there is no device.
    ///
    /// This scrapes the seven constants (six matrix coefficients plus the
    /// chroma centre) straight out of the shader's source text and asserts
    /// them against `nv12_reference`'s Rust constants, so the claim is
    /// checked on every push, not just on a machine with a GPU.
    #[test]
    fn shader_constants_match_the_rust_reference() {
        let path = client_shader_dir().join("h264_nv12_blit.wgsl");
        let src =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

        let centre_line = src
            .lines()
            .find(|l| l.trim_start().starts_with("const CHROMA_CENTRE"))
            .unwrap_or_else(|| panic!("no `const CHROMA_CENTRE` line in {}", path.display()));
        let centre = parse_float_after(centre_line, '=', ';');
        assert_eq!(
            centre,
            crate::nv12_reference::CHROMA_CENTRE,
            "CHROMA_CENTRE drifted between h264_nv12_blit.wgsl and nv12_reference.rs"
        );

        for (channel, want) in [
            ('r', crate::nv12_reference::R_COEFF),
            ('g', crate::nv12_reference::G_COEFF),
            ('b', crate::nv12_reference::B_COEFF),
        ] {
            let prefix = format!("let {channel} = y");
            let line = src
                .lines()
                .find(|l| l.trim_start().starts_with(&prefix))
                .unwrap_or_else(|| panic!("no `{prefix}` line in {}", path.display()));
            let got = extract_uv_coeffs(line);
            assert_eq!(
                got,
                [want[1], want[2]],
                "channel {channel}: shader coefficients {got:?} drifted from \
                 nv12_reference::{}_COEFF[1..] = {:?}",
                channel.to_ascii_uppercase(),
                &want[1..]
            );
        }
    }

    /// Parse the float literal between `start` and `end` in `line` (e.g.
    /// `"const CHROMA_CENTRE: f32 = 0.502;"` with `start = '='`, `end =
    /// ';'`), panicking with the raw line on anything unexpected -- a
    /// scraper for one known, fixed shape, not a general parser, and a line
    /// that doesn't match it is worth failing loudly on rather than
    /// skipping.
    fn parse_float_after(line: &str, start: char, end: char) -> f32 {
        let after_start = line
            .split(start)
            .nth(1)
            .unwrap_or_else(|| panic!("no `{start}` in {line:?}"));
        let literal = after_start
            .split(end)
            .next()
            .unwrap_or_else(|| panic!("no `{end}` after `{start}` in {line:?}"))
            .trim();
        literal
            .parse()
            .unwrap_or_else(|e| panic!("bad float {literal:?} in {line:?}: {e}"))
    }

    /// Extract the two signed coefficients multiplying `u` and `v` out of a
    /// line shaped exactly like `h264_nv12_blit.wgsl`'s three colour-channel
    /// lines, e.g. `let r = y - 0.000927 * u + 1.401687 * v;` -> `[-0.000927,
    /// 1.401687]`. A scraper for that one fixed shape, not a general WGSL
    /// expression parser -- panics with the raw line on anything else.
    fn extract_uv_coeffs(line: &str) -> [f32; 2] {
        let mut coeffs = [0.0f32; 2];
        let mut rest = line;
        for (i, var) in ["u", "v"].into_iter().enumerate() {
            let marker = format!("* {var}");
            let star_pos = rest
                .find(&marker)
                .unwrap_or_else(|| panic!("no `{marker}` in {line:?}"));
            let before = rest[..star_pos].trim_end();
            let sign_pos = before
                .rfind(['+', '-'])
                .unwrap_or_else(|| panic!("no sign before `{marker}` in {line:?}"));
            let sign = &before[sign_pos..=sign_pos];
            let magnitude: f32 = before[sign_pos + 1..]
                .trim()
                .parse()
                .unwrap_or_else(|e| panic!("bad float before `{marker}` in {line:?}: {e}"));
            coeffs[i] = if sign == "-" { -magnitude } else { magnitude };
            rest = &rest[star_pos + marker.len()..];
        }
        coeffs
    }
}
