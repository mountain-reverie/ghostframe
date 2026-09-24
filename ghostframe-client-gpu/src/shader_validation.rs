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
//! The directory is enumerated, not the files named, so a new shader is
//! covered automatically -- the point of Task 7's review comment this closes.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    #[test]
    fn every_client_shader_parses_and_validates() {
        // `shaders/client/`, one level up from this crate's own manifest --
        // matches every `include_str!("../../../shaders/client/...")` in
        // `src/pipelines/*.rs`, which starts from `src/pipelines/` instead.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../shaders/client");

        let entries =
            fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));

        let mut checked = Vec::new();
        let mut failures = Vec::new();
        for entry in entries {
            let entry = entry.expect("read_dir entry");
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("wgsl") {
                continue;
            }
            let name = path
                .file_name()
                .expect("wgsl file has a name")
                .to_string_lossy()
                .into_owned();

            let src = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

            match naga::front::wgsl::parse_str(&src) {
                Ok(module) => {
                    let mut validator = naga::valid::Validator::new(
                        naga::valid::ValidationFlags::all(),
                        naga::valid::Capabilities::all(),
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
}
