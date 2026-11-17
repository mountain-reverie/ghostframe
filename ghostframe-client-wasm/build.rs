// SPDX-License-Identifier: MIT OR Apache-2.0
//! Emits `GHOSTFRAME_CLIENT_VERSION` (see `client_version`) and reruns when
//! the sources it watches move.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Sources whose change reruns this script (the version's dirty flag), relative
/// to the workspace root.
const WATCHED_DIRS: &[&str] = &["ghostframe-client-core/src", "ghostframe-protocol/src"];

/// Run `git` with `args` in the workspace root; `None` outside a checkout, or
/// if git is missing or fails.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        // `git status` refreshes stat data in `.git/index` when it may take
        // the lock. This script watches that file, so a refresh would make
        // the build re-run itself. Never write.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `0.1.0 (aab2fa2d)`, `0.1.0 (aab2fa2d, dirty)` or `0.1.0 (unknown)`.
///
/// Untracked files do not make a tree dirty: nothing here compiles them, and
/// a developer checkout always has some. No timestamp, so builds stay
/// reproducible.
///
/// `dirty` is **best-effort and display-only**. It is evaluated only when
/// this script reruns (HEAD, the index, the branch refs, or the protocol and
/// client-core sources move), so an edit to `ghostframe-client-wasm/src` or
/// `ghostframe-web-ui` leaves it stale until the next such change. The
/// panel's version match ignores it (`ClientStats`'s `versions_match`
/// compares the crate version and the commit only), so a stale flag never
/// raises the alert.
fn client_version(root: &Path) -> String {
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let Some(commit) = git(root, &["rev-parse", "--short=8", "HEAD"]).filter(|c| !c.is_empty())
    else {
        return format!("{pkg} (unknown)");
    };
    let dirty = git(root, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|s| !s.is_empty());
    if dirty {
        format!("{pkg} ({commit}, dirty)")
    } else {
        format!("{pkg} ({commit})")
    }
}

/// Rebuild when the commit or the index moves. `--git-path` resolves the
/// right place in a linked worktree, where `.git` is a file, not a directory.
fn rerun_on_git_changes(root: &Path) {
    for name in ["HEAD", "index"] {
        if let Some(p) = git(root, &["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={}", root.join(p).display());
        }
    }
    // A commit moves the branch ref, not HEAD (which names the branch).
    if let Some(p) = git(root, &["rev-parse", "--git-path", "refs/heads"]) {
        println!("cargo:rerun-if-changed={}", root.join(p).display());
    }
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .expect("stamp: crate has no parent dir")
        .to_path_buf();

    for rel in WATCHED_DIRS {
        println!("cargo:rerun-if-changed={}", root.join(rel).display());
    }

    println!(
        "cargo:rustc-env=GHOSTFRAME_CLIENT_VERSION={}",
        client_version(&root)
    );
    rerun_on_git_changes(&root);
}
