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
