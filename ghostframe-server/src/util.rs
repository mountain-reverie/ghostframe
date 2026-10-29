//! Small helpers shared by the capture backends and `main`.

/// Read CLOCK_MONOTONIC and return the value as nanoseconds.
///
/// Stamps capture completion for the bench telemetry; both capture backends
/// must use the same clock for the numbers to be comparable.
pub fn monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime is async-signal-safe and writes only to the
    // caller-owned timespec. CLOCK_MONOTONIC is always available on Linux.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

/// Interpret an environment value as a boolean flag: set, non-empty and not
/// `"0"` means on. `None` (unset) is off.
pub fn env_flag(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_flag_truth_table() {
        assert!(!env_flag(None));
        assert!(!env_flag(Some("")));
        assert!(!env_flag(Some("0")));
        assert!(env_flag(Some("1")));
        assert!(env_flag(Some("true")));
        // Only the exact string "0" is off.
        assert!(env_flag(Some("00")));
    }

    #[test]
    fn monotonic_clock_is_nonzero_and_nondecreasing() {
        let a = monotonic_now_ns();
        let b = monotonic_now_ns();
        assert!(a > 0);
        assert!(b >= a);
    }
}

/// Create (mode 0700) or validate the tsnet state directory, which holds the
/// node's private keys. Fails closed: an existing directory that is not a
/// directory, is owned by another user, or is accessible to group or others
/// is refused rather than repaired, since its contents may already have been
/// read. A symlink is refused too (`symlink_metadata`), so a planted link
/// cannot redirect the keys.
pub fn ensure_private_dir(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|e| format!("cannot create state dir {}: {e}", path.display()))?;
        }
        Err(e) => return Err(format!("cannot stat state dir {}: {e}", path.display())),
        Ok(_) => {}
    }
    let md = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot stat state dir {}: {e}", path.display()))?;
    if !md.is_dir() {
        return Err(format!(
            "state dir {} is not a directory (symlinks are refused)",
            path.display()
        ));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if md.uid() != euid {
        return Err(format!(
            "state dir {} is owned by uid {}, not the current user (uid {euid}); refusing to start",
            path.display(),
            md.uid()
        ));
    }
    let mode = md.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "state dir {} has mode {mode:03o}, which lets other users read the tailnet node keys; \
             run `chmod 700 {}` and re-check who may have read it",
            path.display(),
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod state_dir_tests {
    use super::ensure_private_dir;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn tmp(label: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gf-statedir-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn creates_missing_nested_dir_as_0700() {
        let root = tmp("create");
        let d = root.join("a/b/ghostframe");
        ensure_private_dir(&d).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&d), 0o700);
        ensure_private_dir(&d).expect("idempotent on an existing 0700 dir");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_group_or_world_accessible() {
        let root = tmp("loose");
        for bad in [0o770, 0o750, 0o705, 0o755, 0o701] {
            let d = root.join(format!("m{bad:o}"));
            std::fs::create_dir(&d).unwrap();
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(bad)).unwrap();
            let e = ensure_private_dir(&d).expect_err(&format!("{bad:o} must be refused"));
            assert!(e.contains("chmod 700"), "{e}");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_symlink_and_file() {
        let root = tmp("link");
        let real = root.join("real");
        ensure_private_dir(&real).unwrap();
        let link = root.join("link");
        symlink(&real, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
        let f = root.join("file");
        std::fs::write(&f, b"x").unwrap();
        assert!(ensure_private_dir(&f).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_dir_owned_by_another_user() {
        // /proc/1 is root-owned; when not root the ownership branch trips.
        if unsafe { libc::geteuid() } != 0 {
            let e = ensure_private_dir(std::path::Path::new("/proc/1")).unwrap_err();
            assert!(e.contains("owned by uid") || e.contains("mode"), "{e}");
        }
    }
}
