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

    // Link against the generated archive. These directives propagate to
    // whatever links this crate, so `ghostframe-lib` and any future client
    // binary pick them up transitively without repeating them.
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

    // The SPA tree lives outside ghostbridge/ and is rsync'd in by the
    // Makefile. Without this rerun-if-changed, edits to the web client
    // won't trigger a re-link.
    let web_dist = PathBuf::from(&manifest_dir).join("../ghostframe-web-client/dist");
    println!("cargo:rerun-if-changed={}", web_dist.display());
}
