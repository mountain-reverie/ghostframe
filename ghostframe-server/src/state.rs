//! The tsnet state directory: whether it is seeded, and the lock that keeps
//! two tsnet instances off one `tailscaled.state`.
//!
//! `capture`, `login` and `logout` each take an exclusive `flock` on
//! `<state_dir>/.lock` for as long as they use the directory. Nothing else
//! stopped a `login` from running against the state the service was using.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The lock file's name inside the state directory.
pub const LOCK_FILE: &str = ".lock";

/// Written once a join has completed (`login` after `up()` returned, or
/// `capture` after an auth-key join). Its absence means "never joined".
pub const JOINED_FILE: &str = ".joined";

/// True once a join completed. NOT "tailscaled.state exists": tsnet writes
/// that file (`{}`, then the machine key) when it starts, before any
/// authorisation, so a `login` that was interrupted, timed out or was given
/// a bad control URL leaves it behind. Treating that as joined made `capture`
/// start an interactive tsnet that logs a claimable sign-in URL to the journal.
/// (tailscale.com v1.104.1, ipn/store/stores.go NewFileStore.) The state
/// file's other keys cannot stand in for the marker: the node key and its
/// profile are also created before authorisation.
pub fn state_dir_seeded(state_dir: &Path) -> bool {
    state_dir.join(JOINED_FILE).exists()
}

/// Record that a join completed.
pub fn mark_joined(state_dir: &Path) -> std::io::Result<()> {
    std::fs::write(state_dir.join(JOINED_FILE), b"")
}

/// True when the state is certainly from a node that never registered: no
/// marker, and `tailscaled.state` holds nothing beyond the machine key (or is
/// absent or unreadable as JSON object).
pub fn never_joined(state_dir: &Path) -> bool {
    if state_dir_seeded(state_dir) {
        return false;
    }
    let Ok(bytes) = std::fs::read(state_dir.join("tailscaled.state")) else {
        return true;
    };
    match serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes) {
        Ok(m) => m.keys().all(|k| k == "_machinekey"),
        Err(_) => false,
    }
}

/// Entries tsnet (tailscale.com v1.104.1) and this crate create inside the
/// state directory. Sources: tsnet/tsnet.go (`tailscaled.state`,
/// `tailscaled.log.conf`, filch's `tailscaled.log1.txt`/`log2.txt`),
/// feature/acme/certstore.go (`certs/`), feature/taildrop/ext.go (`files/`),
/// ipn/ipnlocal (`profile-data/`, `tka-profiles/`, netmap cache under
/// `profile-data/`), ipn/auditlog/store.go (`audit-log.json`), plus this
/// crate's `.lock` and `.joined`. A live node's directory was also listed:
/// `.lock tailscaled.log.conf tailscaled.log1.txt tailscaled.log2.txt
/// tailscaled.state`.
fn is_known_entry(name: &str) -> bool {
    matches!(
        name,
        LOCK_FILE
            | JOINED_FILE
            | "certs"
            | "files"
            | "profile-data"
            | "tka-profiles"
            | "audit-log.json"
            | "derpmap.cached.json"
    ) || name.starts_with("tailscaled.state")
        || name.starts_with("tailscaled.log")
}

/// Delete the state directory, but only if everything in it is known. The
/// path is user-supplied (`TS_STATE_DIR`); a recursive delete of whatever it
/// names would wipe a home directory. Returns the unknown entries, deleting
/// nothing, when there are any.
pub fn remove_state_dir(state_dir: &Path) -> Result<(), RemoveError> {
    let mut entries = Vec::new();
    let mut unknown = Vec::new();
    for e in std::fs::read_dir(state_dir).map_err(RemoveError::Io)? {
        let e = e.map_err(RemoveError::Io)?;
        let name = e.file_name().to_string_lossy().into_owned();
        if is_known_entry(&name) {
            entries.push(e.path());
        } else {
            unknown.push(name);
        }
    }
    if !unknown.is_empty() {
        unknown.sort();
        return Err(RemoveError::Unknown(unknown));
    }
    for p in entries {
        // symlink_metadata: a symlink named like a known entry is unlinked,
        // never followed.
        let is_dir = std::fs::symlink_metadata(&p)
            .map_err(RemoveError::Io)?
            .file_type()
            .is_dir();
        if is_dir {
            std::fs::remove_dir_all(&p).map_err(RemoveError::Io)?;
        } else {
            std::fs::remove_file(&p).map_err(RemoveError::Io)?;
        }
    }
    std::fs::remove_dir(state_dir).map_err(RemoveError::Io)
}

#[derive(Debug)]
pub enum RemoveError {
    /// Entries ghostframe did not create; nothing was deleted.
    Unknown(Vec<String>),
    Io(std::io::Error),
}

#[derive(Debug)]
pub enum LockError {
    /// Another process holds the lock; `path` is the lock file.
    Busy { path: PathBuf },
    /// The lock file could not be opened or locked.
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Busy { path } => write!(
                f,
                "the ghostframe-server service is running; stop it first \
                 (systemctl --user stop ghostframe-server; lock: {}; \
                 `fuser {}` shows the holder)",
                path.display(),
                path.display()
            ),
            LockError::Io(e) => write!(f, "cannot lock the state directory: {e}"),
        }
    }
}

impl std::error::Error for LockError {}

/// Held for as long as this process uses the state directory; dropping it,
/// or exiting, releases it.
#[derive(Debug)]
pub struct StateLock {
    _file: File,
}

impl StateLock {
    /// Take the lock, or fail at once with [`LockError::Busy`].
    pub fn acquire(state_dir: &Path) -> Result<Self, LockError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(state_dir.join(LOCK_FILE))
            .map_err(LockError::Io)?;
        // SAFETY: flock on a descriptor this function owns; LOCK_NB makes it
        // fail instead of waiting.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.kind() == std::io::ErrorKind::WouldBlock {
                LockError::Busy {
                    path: state_dir.join(LOCK_FILE),
                }
            } else {
                LockError::Io(e)
            });
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsnet_start_alone_does_not_count_as_joined() {
        let d = tempfile::tempdir().unwrap();
        assert!(!state_dir_seeded(&d.path().join("missing")));
        // What tsnet writes before any authorisation.
        std::fs::write(d.path().join("tailscaled.state"), b"{}").unwrap();
        assert!(!state_dir_seeded(d.path()));
        std::fs::write(
            d.path().join("tailscaled.state"),
            br#"{"_machinekey":"AAAA"}"#,
        )
        .unwrap();
        assert!(!state_dir_seeded(d.path()));
        mark_joined(d.path()).unwrap();
        assert!(state_dir_seeded(d.path()));
    }

    #[test]
    fn never_joined_tells_a_bare_start_from_a_registered_node() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("tailscaled.state");
        assert!(never_joined(d.path()), "no state at all");
        std::fs::write(&state, br#"{"_machinekey":"AAAA"}"#).unwrap();
        assert!(never_joined(d.path()));
        std::fs::write(&state, br#"{"_machinekey":"AAAA","profile-ab":"BBBB"}"#).unwrap();
        assert!(!never_joined(d.path()), "more than the machine key");
        std::fs::write(&state, b"not json").unwrap();
        assert!(!never_joined(d.path()), "unreadable: do not assume");
        std::fs::write(&state, b"{}").unwrap();
        mark_joined(d.path()).unwrap();
        assert!(!never_joined(d.path()), "the marker wins");
    }

    #[test]
    fn remove_state_dir_deletes_known_entries_and_the_directory() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("state");
        std::fs::create_dir_all(dir.join("certs")).unwrap();
        std::fs::write(dir.join("certs/a.crt"), b"x").unwrap();
        for n in [
            ".lock",
            ".joined",
            "tailscaled.state",
            "tailscaled.log.conf",
            "tailscaled.log1.txt",
            "tailscaled.log2.txt",
            "derpmap.cached.json",
        ] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        remove_state_dir(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn remove_state_dir_refuses_unknown_entries_and_deletes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("state");
        std::fs::create_dir_all(dir.join("Documents")).unwrap();
        std::fs::write(dir.join("Documents/thesis.txt"), b"precious").unwrap();
        std::fs::write(dir.join("notes.txt"), b"precious").unwrap();
        std::fs::write(dir.join("tailscaled.state"), b"{}").unwrap();
        let canary = d.path().join("canary");
        std::fs::write(&canary, b"x").unwrap();
        match remove_state_dir(&dir) {
            Err(RemoveError::Unknown(u)) => assert_eq!(u, ["Documents", "notes.txt"]),
            other => panic!("{other:?}"),
        }
        assert!(dir.join("Documents/thesis.txt").exists());
        assert!(dir.join("notes.txt").exists());
        assert!(
            dir.join("tailscaled.state").exists(),
            "known entries kept too"
        );
        assert!(canary.exists());
    }

    #[test]
    fn a_second_holder_is_refused_until_the_first_drops() {
        let d = tempfile::tempdir().unwrap();
        let first = StateLock::acquire(d.path()).unwrap();
        // flock locks belong to the open file description, so a second open
        // in the same process conflicts exactly as another process would.
        let second = StateLock::acquire(d.path());
        assert!(matches!(second, Err(LockError::Busy { .. })), "{second:?}");
        drop(first);
        StateLock::acquire(d.path()).expect("free again once dropped");
    }

    #[test]
    fn the_busy_message_says_to_stop_the_service() {
        let path = PathBuf::from("/state/.lock");
        let msg = LockError::Busy { path: path.clone() }.to_string();
        assert!(msg.contains("service is running; stop it first"), "{msg}");
        assert!(msg.contains("lock: /state/.lock"), "{msg}");
        assert!(msg.contains("`fuser /state/.lock`"), "{msg}");
    }
}
