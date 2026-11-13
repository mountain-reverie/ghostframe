// SPDX-License-Identifier: AGPL-3.0-or-later
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `0.1.0 (aab2fa2d)`, `0.1.0 (aab2fa2d, dirty)` or, with no commit,
/// `0.1.0 (unknown)`.
fn format_version(pkg: &str, commit: Option<&str>, dirty: bool) -> String {
    match commit {
        Some(c) if dirty => format!("{pkg} ({c}, dirty)"),
        Some(c) => format!("{pkg} ({c})"),
        None => format!("{pkg} (unknown)"),
    }
}

/// Run git in `dir`; `None` when git is missing, fails, or prints nothing.
///
/// `--no-optional-locks` (and `GIT_OPTIONAL_LOCKS=0`) is load-bearing:
/// `git status` otherwise refreshes and rewrites `.git/index` whenever stat
/// data is stale (after every `git checkout -- <file>`, or any outside
/// `git status` from a shell or an IDE). The index is a `rerun-if-changed`
/// input below, so a script that writes the file it watches would make the
/// next cargo run rerun it and recompile `ghostframe-lib` and everything above
/// it for nothing.
fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Rerun when the checked-out commit or the tree's tracked state changes.
///
/// `git rev-parse --git-path` finds the real files in a worktree, where
/// `.git` is a file. HEAD alone is not enough: a commit moves the branch ref,
/// not HEAD, and may leave the index untouched. A path that does not exist
/// would make cargo rerun this script on every build, so only existing paths
/// are named.
fn emit_git_reruns(dir: &str) {
    let mut paths: Vec<String> = ["HEAD", "index", "packed-refs"]
        .iter()
        .filter_map(|p| git(dir, &["rev-parse", "--git-path", p]))
        .collect();
    if let Some(head_ref) = git(dir, &["symbolic-ref", "-q", "HEAD"]) {
        paths.extend(git(dir, &["rev-parse", "--git-path", &head_ref]));
    }
    for p in paths {
        let p = Path::new(&p);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            Path::new(dir).join(p)
        };
        if abs.exists() {
            println!("cargo:rerun-if-changed={}", abs.display());
        }
    }
}

fn emit_build_version(manifest_dir: &str) {
    let pkg = env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let commit = git(manifest_dir, &["rev-parse", "--short=8", "HEAD"]);
    // Tracked files only: this repository keeps untracked notes in its root,
    // and a flag that is always on says nothing.
    let dirty = commit.is_some()
        && git(
            manifest_dir,
            &["status", "--porcelain", "--untracked-files=no"],
        )
        .is_some();
    println!(
        "cargo:rustc-env=GHOSTFRAME_BUILD_VERSION={}",
        format_version(&pkg, commit.as_deref(), dirty)
    );
    emit_git_reruns(manifest_dir);
    // The dirty flag also follows this crate's own sources (and the protocol
    // crate's, which the header generation below parses). A source edit in
    // another crate does not rerun this script: the flag can lag there until
    // the next commit, index or ghostframe-lib edit. That is accepted, and is
    // why the client compares crate version and commit only (see the Task 8
    // preamble); widening this list to every embedded crate would recompile
    // ghostframe-lib on every client edit.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=cbindgen.toml");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=../ghostframe-protocol/src");
}

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    emit_build_version(&manifest_dir);

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
