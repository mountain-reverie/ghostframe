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

/// This machine's host name, for the default tsnet node name.
pub fn system_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// The effective user's passwd entry: login name and home directory.
pub struct Passwd {
    pub name: String,
    pub home: String,
}

/// The effective user's passwd entry. Used to check `HOME` and to name the
/// user; never to derive a path to write under.
pub fn current_passwd() -> Option<Passwd> {
    // SAFETY: getpwuid returns NULL or a pointer to static storage that stays
    // valid until the next getpw* call; both strings are copied out before any.
    unsafe {
        let pw = libc::getpwuid(libc::geteuid());
        if pw.is_null() || (*pw).pw_name.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        let s = |p: *const libc::c_char| std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
        Some(Passwd {
            name: s((*pw).pw_name),
            home: s((*pw).pw_dir),
        })
    }
}

/// Ignore SIGHUP for the rest of the process: `install` may stop the very
/// session it was started from.
pub fn ignore_sighup() {
    // SAFETY: installing SIG_IGN for a standard signal has no preconditions.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

/// The effective uid.
pub fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_username_is_the_owner_of_this_process() {
        let id = std::process::Command::new("id")
            .arg("-un")
            .output()
            .unwrap();
        let want = String::from_utf8(id.stdout).unwrap();
        assert_eq!(
            current_passwd().map(|p| p.name).as_deref(),
            Some(want.trim())
        );
    }

    #[test]
    fn system_hostname_matches_the_kernel() {
        let kernel = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        assert_eq!(system_hostname().as_deref(), Some(kernel.trim()));
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
