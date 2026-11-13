//! The server's stats metric table and the numbers behind it.
//!
//! Internal on purpose: nothing here is `pub`, so none of it reaches the
//! cbindgen header. It is the one list a later OpenTelemetry exporter will
//! read, so the panel and the exporter cannot drift apart.

/// `0.1.0 (aab2fa2d)`, `0.1.0 (aab2fa2d, dirty)`, or `0.1.0 (unknown)` outside a
/// git checkout. Captured by `build.rs`; see its doc for what triggers a rerun.
pub(crate) const SERVER_VERSION: &str = env!("GHOSTFRAME_BUILD_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_version_has_the_documented_shape() {
        let rest = SERVER_VERSION
            .strip_prefix(env!("CARGO_PKG_VERSION"))
            .unwrap_or_else(|| panic!("{SERVER_VERSION:?} must start with the crate version"));
        let inner = rest
            .strip_prefix(" (")
            .and_then(|r| r.strip_suffix(')'))
            .unwrap_or_else(|| panic!("{SERVER_VERSION:?} must end in ` (<commit>)`"));
        let commit = inner.strip_suffix(", dirty").unwrap_or(inner);
        assert!(
            commit == "unknown"
                || (commit.len() >= 8 && commit.chars().all(|c| c.is_ascii_hexdigit())),
            "commit part is `unknown` or 8+ hex digits: {SERVER_VERSION:?}"
        );
    }

    #[test]
    fn server_version_names_the_checked_out_commit() {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--short=8", "HEAD"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output();
        let Ok(out) = out else { return }; // no git on this machine
        if !out.status.success() {
            return; // not a checkout (a source tarball): `unknown` is right
        }
        let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(
            SERVER_VERSION.contains(&head),
            "build.rs captured a stale commit: version {SERVER_VERSION:?}, HEAD {head}"
        );
    }
}
