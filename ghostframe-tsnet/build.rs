use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let ghostbridge_dir = PathBuf::from(&manifest_dir).join("../ghostbridge");

    // Which ghostbridge archive to build. With the `web-embed` feature the
    // archive carries the browser SPA, which means dist/ must be built
    // first; without it, `archive-noweb` drops the //go:embed and the whole
    // npm/vite step with it. The native client takes the second path -- it
    // links ghostbridge for tsnet and never serves a browser anything.
    let web_embed = env::var_os("CARGO_FEATURE_WEB_EMBED").is_some();
    let target = if web_embed {
        "archive"
    } else {
        "archive-noweb"
    };

    // Build the ghostbridge Go c-archive. This shells out to `make` because
    // mixing `go build` and Cargo's build graph directly is a known rabbit
    // hole; the Makefile keeps the glue trivial.
    //
    // The archive goes into Cargo's OUT_DIR, never into ghostbridge/. Cargo
    // fingerprints rerun-if-changed paths by mtime, and writing the output
    // into a watched directory made every cargo invocation rerun this
    // script and recompile everything above ghostframe-tsnet. OUT_DIR is
    // also per feature set, so the web-embed and noweb archives cannot
    // clobber each other.
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let archive = out_dir.join("libghostbridge.a");
    let output = Command::new("make")
        .args(["-C", ghostbridge_dir.to_str().unwrap(), target])
        .arg(format!("OUT={}", archive.display()))
        .output()
        .expect("Failed to build ghostbridge. Is `make` and `go` installed?");

    if !output.status.success() {
        panic!(
            "ghostbridge `make {target}` failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Link against the generated archive. These directives propagate to
    // whatever links this crate, so `ghostframe-lib` and any future client
    // binary pick them up transitively without repeating them.
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=ghostbridge");

    // Go c-archive pulls in the Go runtime, which needs pthread and libm.
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=m");

    // Rerun when a ghostbridge input changes. The directory itself must not
    // be watched: Cargo scans it recursively, and `make` rsyncs dist/ into
    // it, which would make the script look perpetually stale. So list the
    // inputs: every top-level file (.go, go.mod, go.sum, Makefile) and every
    // subdirectory except dist/. Tradeoff: a brand-new top-level file is only
    // noticed once some already-watched file changes (or the build script
    // itself does).
    let entries = std::fs::read_dir(&ghostbridge_dir).expect("read ghostbridge/");
    for entry in entries {
        let path = entry.expect("read ghostbridge/ entry").path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name == "dist" || name.starts_with("libghostbridge.") {
            continue;
        }
        println!("cargo:rerun-if-changed={}", path.display());
    }

    // The SPA tree lives outside ghostbridge/ and is rsync'd in by the
    // Makefile. Without this rerun-if-changed, edits to the web client
    // won't trigger a re-link. Only meaningful when the SPA is actually
    // embedded -- a noweb archive does not contain it, so watching it would
    // just re-link the native client whenever the web client is rebuilt.
    if web_embed {
        let web_dist = PathBuf::from(&manifest_dir).join("../ghostframe-web-client/dist");
        println!("cargo:rerun-if-changed={}", web_dist.display());
    }
}
