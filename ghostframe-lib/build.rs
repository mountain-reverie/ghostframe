use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let ghostbridge_dir = PathBuf::from(&manifest_dir).join("../ghostbridge");

    // Build the ghostbridge Go c-archive. This shells out to `make` because
    // mixing `go build` and Cargo's build graph directly is a known rabbit
    // hole; the Makefile keeps the glue trivial.
    let output = Command::new("make")
        .args(["-C", ghostbridge_dir.to_str().unwrap(), "archive"])
        .output()
        .expect("Failed to build ghostbridge. Is `make` and `go` installed?");

    if !output.status.success() {
        panic!(
            "ghostbridge build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Link against the generated archive.
    println!(
        "cargo:rustc-link-search=native={}",
        ghostbridge_dir.display()
    );
    println!("cargo:rustc-link-lib=static=ghostbridge");

    // Go c-archive pulls in the Go runtime, which needs pthread and libm.
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=m");

    // Rerun if anything in ghostbridge/ changes. Watching the directory
    // catches new .go files, go.sum updates, and the Makefile.
    println!("cargo:rerun-if-changed={}", ghostbridge_dir.display());

    // Generate a C header with cbindgen. Non-fatal while ghostframe-lib has
    // no `pub extern "C"` exports — it becomes fatal once M1 starts exporting
    // real symbols; at that point a parse error here should fail the build.
    let cbindgen_config = cbindgen::Config::from_root_or_default(&manifest_dir);
    let include_dir = PathBuf::from(&manifest_dir).join("include");
    std::fs::create_dir_all(&include_dir).expect("Failed to create include/ directory");
    let result = cbindgen::Builder::new()
        .with_config(cbindgen_config)
        .with_crate(manifest_dir.clone())
        .with_language(cbindgen::Language::C)
        .generate();
    match result {
        Ok(bindings) => {
            bindings.write_to_file(include_dir.join("ghostframe.h"));
        }
        Err(e) => {
            println!("cargo:warning=cbindgen header generation skipped (no exports yet): {e}");
        }
    }
}
