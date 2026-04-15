use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let ghostbridge_dir = PathBuf::from(&manifest_dir).join("../ghostbridge");

    // Build ghostbridge Go C archive
    let output = Command::new("make")
        .args(["-C", ghostbridge_dir.to_str().unwrap(), "archive"])
        .output()
        .expect("Failed to build ghostbridge. Is Go installed?");

    if !output.status.success() {
        panic!("ghostbridge build failed:\n{}", String::from_utf8_lossy(&output.stderr));
    }

    // Link against the generated archive
    let lib_dir = ghostbridge_dir.clone();
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=ghostbridge");

    // Go c-archive needs -lpthread -lm on Linux
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=m");

    // Rerun if Go source changes
    println!("cargo:rerun-if-changed={}", ghostbridge_dir.join("main.go").display());
    println!("cargo:rerun-if-changed={}", ghostbridge_dir.join("go.mod").display());

    // Generate C header with cbindgen
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
            eprintln!("cargo:warning=cbindgen header generation skipped: {e}");
        }
    }
}
