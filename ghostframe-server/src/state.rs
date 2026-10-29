//! The tsnet state directory: whether it is seeded, and the lock that keeps
//! two tsnet instances off one `tailscaled.state`.
//!
//! `capture`, `login` and `logout` each take an exclusive `flock` on
//! `<state_dir>/.lock` for as long as they use the directory. Nothing else
//! stopped a `login` from running against the state the service was using.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// The lock file's name inside the state directory.
pub const LOCK_FILE: &str = ".lock";

/// True once tsnet has written `tailscaled.state`, which it does after the
/// first successful join: the "has logged in" proxy.
pub fn state_dir_seeded(state_dir: &Path) -> bool {
    state_dir.join("tailscaled.state").exists()
}

#[derive(Debug)]
pub enum LockError {
    /// Another process holds the lock.
    Busy,
    /// The lock file could not be opened or locked.
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Busy => write!(
                f,
                "the ghostframe-server service is running; stop it first \
                 (systemctl --user stop ghostframe-server)"
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
                LockError::Busy
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
    fn seeded_only_once_tailscaled_state_exists() {
        let d = tempfile::tempdir().unwrap();
        assert!(!state_dir_seeded(d.path()));
        assert!(!state_dir_seeded(&d.path().join("missing")));
        std::fs::write(d.path().join("tailscaled.state"), b"{}").unwrap();
        assert!(state_dir_seeded(d.path()));
    }

    #[test]
    fn a_second_holder_is_refused_until_the_first_drops() {
        let d = tempfile::tempdir().unwrap();
        let first = StateLock::acquire(d.path()).unwrap();
        // flock locks belong to the open file description, so a second open
        // in the same process conflicts exactly as another process would.
        let second = StateLock::acquire(d.path());
        assert!(matches!(second, Err(LockError::Busy)), "{second:?}");
        drop(first);
        StateLock::acquire(d.path()).expect("free again once dropped");
    }

    #[test]
    fn the_busy_message_says_to_stop_the_service() {
        let msg = LockError::Busy.to_string();
        assert!(msg.contains("service is running; stop it first"), "{msg}");
    }
}
