use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    // The ghostbridge c-archive build and its link directives moved to
    // `ghostframe-tsnet`, which owns that FFI surface. Cargo propagates a
    // dependency's `cargo:rustc-link-*` output to whatever links it, so this
    // crate still gets the archive without repeating the recipe.

    // Generate a C header with cbindgen. Non-fatal while ghostframe-lib has
    // no `pub extern "C"` exports — it becomes fatal once M1 starts exporting
    // real symbols; at that point a parse error here should fail the build.
    let cbindgen_config = cbindgen::Config::from_root_or_default(&manifest_dir);
    let include_dir = PathBuf::from(&manifest_dir).join("include");
    std::fs::create_dir_all(&include_dir).expect("Failed to create include/ directory");
    let bindings = cbindgen::Builder::new()
        .with_config(cbindgen_config)
        .with_crate(manifest_dir.clone())
        .with_language(cbindgen::Language::C)
        .generate()
        .expect("cbindgen failed");
    bindings.write_to_file(include_dir.join("ghostframe.h"));
}
