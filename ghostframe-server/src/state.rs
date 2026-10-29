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

/// True once this directory's node has been registered (once: key expiry or
/// removal in the admin console later does not clear it; `capture` fails
/// cleanly on those, see `non_interactive`). NOT "tailscaled.state exists":
/// tsnet writes that file (`{}`, then the machine key) when it starts, before
/// any registration, so a `login` that was interrupted, timed out or was given
/// a bad control URL leaves it behind. Treating that as joined made `capture`
/// start an interactive tsnet that logs a claimable sign-in URL to the journal.
/// (tailscale.com v1.104.1, ipn/store/stores.go NewFileStore.)
///
/// Three ways to ask the same question, from cheapest:
/// - this: the marker, written by `login`/`capture` after a successful join;
/// - [`never_joined`]: no marker and the state holds nothing but the machine
///   key, so there is certainly nothing to deregister (used by `logout`);
/// - [`state_is_registered`]: reads the profile prefs offline
///   (`Config.NodeID`, `UserProfile.LoginName`, both empty until control has
///   answered; ipn/ipnlocal/profiles.go `SetPrefs`), to adopt state that
///   predates the marker.
pub fn state_dir_seeded(state_dir: &Path) -> bool {
    state_dir.join(JOINED_FILE).exists()
}

/// Record that a join completed. Durable before it returns: the marker is
/// what keeps `capture` from starting an interactive tsnet.
pub fn mark_joined(state_dir: &Path) -> std::io::Result<()> {
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(state_dir.join(JOINED_FILE))?;
    f.sync_all()?;
    File::open(state_dir)?.sync_all()
}

/// Offline check (no tsnet) that `tailscaled.state` holds a node that
/// completed authorisation, for adopting state written before the marker
/// existed. All of these must hold, else false (fail closed):
/// - the file is a JSON map of base64 values;
/// - `_current-profile` decodes to a `profile-...` key present in the map;
/// - that profile decodes to prefs JSON with a non-empty `Config.NodeID` and
///   `Config.UserProfile.LoginName` (both empty until control answers; see
///   `SetPrefs`) and `LoggedOut` not true.
///
/// Field names: tailscale.com v1.104.1 ipn/prefs.go (`Persist` is serialised
/// as `Config`, `LoggedOut`), types/persist/persist.go (`NodeID`,
/// `UserProfile`), tailcfg.UserProfile (`LoginName`), ipn/store.go
/// (`_current-profile`, whose value is the state key itself).
pub fn state_is_registered(state_dir: &Path) -> bool {
    use base64::Engine;
    fn inner(state_dir: &Path) -> Option<bool> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let bytes = std::fs::read(state_dir.join("tailscaled.state")).ok()?;
        let map: std::collections::HashMap<String, String> = serde_json::from_slice(&bytes).ok()?;
        let get = |k: &str| -> Option<Vec<u8>> { b64.decode(map.get(k)?).ok() };
        let cur = String::from_utf8(get("_current-profile")?).ok()?;
        if !cur.starts_with("profile-") {
            return None;
        }
        let prefs: serde_json::Value = serde_json::from_slice(&get(&cur)?).ok()?;
        let nonempty = |v: Option<&serde_json::Value>| {
            v.and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty())
        };
        let cfg = prefs.get("Config")?;
        Some(
            nonempty(cfg.get("NodeID"))
                && nonempty(cfg.get("UserProfile").and_then(|u| u.get("LoginName")))
                && prefs.get("LoggedOut").and_then(|v| v.as_bool()) != Some(true),
        )
    }
    inner(state_dir).unwrap_or(false)
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

/// What tsnet (tailscale.com v1.104.1) and this crate create inside the
/// state directory, with the type each must have. Sources: tsnet/tsnet.go
/// (`tailscaled.state`, `tailscaled.log.conf`, filch's `tailscaled.log1.txt`
/// and `log2.txt`), atomicfile (`tailscaled.state.tmp*`),
/// feature/acme/certstore.go (`certs/`), feature/taildrop/ext.go (`files/`),
/// ipn/ipnlocal (`profile-data/`, `tka-profiles/`), ipn/auditlog/store.go
/// (`audit-log.json`), plus this crate's `.lock` and `.joined`. A live node's
/// directory was also listed: `.lock tailscaled.log.conf tailscaled.log1.txt
/// tailscaled.log2.txt tailscaled.state`. Anything else, or a known name of
/// the wrong type (a symlink, a directory called `.lock`), is unknown.
fn is_known_entry(name: &str, ft: std::fs::FileType) -> bool {
    let file = ft.is_file();
    let dir = ft.is_dir();
    match name {
        LOCK_FILE
        | JOINED_FILE
        | "tailscaled.state"
        | "tailscaled.log.conf"
        | "tailscaled.log1.txt"
        | "tailscaled.log2.txt"
        | "audit-log.json"
        | "derpmap.cached.json" => file,
        "certs" | "files" | "profile-data" | "tka-profiles" => dir,
        n => n.starts_with("tailscaled.state.tmp") && file,
    }
}

/// The entries to delete, or the unknown ones. Deletes nothing: `logout`
/// calls this BEFORE deregistering, so a foreign file refuses the whole
/// command while the node is still registered and still marked joined.
pub fn plan_removal(state_dir: &Path) -> Result<Vec<PathBuf>, RemoveError> {
    let mut entries = Vec::new();
    let mut unknown = Vec::new();
    for e in std::fs::read_dir(state_dir).map_err(RemoveError::Io)? {
        let e = e.map_err(RemoveError::Io)?;
        let name = e.file_name().to_string_lossy().into_owned();
        // file_type() does not follow symlinks.
        let ft = e.file_type().map_err(RemoveError::Io)?;
        if is_known_entry(&name, ft) {
            entries.push(e.path());
        } else {
            unknown.push(name);
        }
    }
    if !unknown.is_empty() {
        unknown.sort();
        return Err(RemoveError::Unknown(unknown));
    }
    Ok(entries)
}

/// Delete the marker (absent is fine) and make that durable: a crash must not
/// bring back a marker for a node that has been deregistered.
pub fn remove_marker(state_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(state_dir.join(JOINED_FILE)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    File::open(state_dir)?.sync_all()
}

/// Delete the state directory, but only if everything in it is known. The
/// path is user-supplied (`TS_STATE_DIR`); a recursive delete of whatever it
/// names would wipe a home directory. `.joined` goes first, so that if
/// anything after it fails the directory no longer claims a completed join.
pub fn remove_state_dir(state_dir: &Path) -> Result<(), RemoveError> {
    let mut entries = plan_removal(state_dir)?;
    // First, and before anything that can fail part-way.
    remove_marker(state_dir).map_err(RemoveError::Io)?;
    entries.retain(|p| p.file_name() != Some(std::ffi::OsStr::new(JOINED_FILE)));
    for p in entries {
        if std::fs::symlink_metadata(&p)
            .map_err(RemoveError::Io)?
            .file_type()
            .is_dir()
        {
            std::fs::remove_dir_all(&p).map_err(RemoveError::Io)?;
        } else {
            std::fs::remove_file(&p).map_err(RemoveError::Io)?;
        }
    }
    std::fs::remove_dir(state_dir).map_err(RemoveError::Io)
}

#[derive(Debug)]
pub enum RemoveError {
    /// Entries ghostframe did not create.
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

    /// A synthetic state file to the real schema (see `state_is_registered`).
    fn write_state(dir: &Path, node_id: &str, login: &str, logged_out: bool, cur: Option<&str>) {
        write_state_at(dir, node_id, login, logged_out, cur, "profile-ab12");
    }

    fn write_state_at(
        dir: &Path,
        node_id: &str,
        login: &str,
        logged_out: bool,
        cur: Option<&str>,
        stored_under: &str,
    ) {
        use base64::Engine;
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let prefs = serde_json::json!({
            "ControlURL": "https://controlplane.tailscale.com",
            "LoggedOut": logged_out,
            "Config": {
                "PrivateNodeKey": "privkey:00",
                "NodeID": node_id,
                "UserProfile": {"ID": 1, "LoginName": login, "DisplayName": "x"},
            },
        });
        let mut m = serde_json::Map::new();
        m.insert("_machinekey".into(), b64(b"mk").into());
        m.insert(
            stored_under.into(),
            b64(prefs.to_string().as_bytes()).into(),
        );
        if let Some(c) = cur {
            m.insert("_current-profile".into(), b64(c.as_bytes()).into());
        }
        std::fs::write(
            dir.join("tailscaled.state"),
            serde_json::Value::Object(m).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn a_registered_node_is_recognised_offline() {
        let d = tempfile::tempdir().unwrap();
        write_state(
            d.path(),
            "n123CNTRL",
            "a@example.com",
            false,
            Some("profile-ab12"),
        );
        assert!(state_is_registered(d.path()));
    }

    #[test]
    fn anything_short_of_a_completed_registration_is_not_registered() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        let state = p.join("tailscaled.state");
        assert!(!state_is_registered(p), "no file");
        std::fs::write(&state, b"{}").unwrap();
        assert!(!state_is_registered(p), "{{}}");
        std::fs::write(&state, br#"{"_machinekey":"bWs="}"#).unwrap();
        assert!(!state_is_registered(p), "machine key only");
        write_state(p, "", "a@example.com", false, Some("profile-ab12"));
        assert!(!state_is_registered(p), "empty NodeID");
        write_state(p, "n1", "", false, Some("profile-ab12"));
        assert!(!state_is_registered(p), "empty LoginName");
        write_state(p, "n1", "a@example.com", true, Some("profile-ab12"));
        assert!(!state_is_registered(p), "LoggedOut");
        write_state(p, "n1", "a@example.com", false, None);
        assert!(!state_is_registered(p), "no _current-profile");
        write_state(p, "n1", "a@example.com", false, Some("profile-zzzz"));
        assert!(!state_is_registered(p), "dangling _current-profile");
        write_state_at(
            p,
            "n1",
            "a@example.com",
            false,
            Some("other-ab12"),
            "other-ab12",
        );
        assert!(
            !state_is_registered(p),
            "valid prefs under a key that is not profile-..."
        );
        std::fs::write(&state, b"\xff garbage").unwrap();
        assert!(!state_is_registered(p), "garbage");
        std::fs::write(
            &state,
            br#"{"_current-profile":"!!!","profile-ab12":"!!!"}"#,
        )
        .unwrap();
        assert!(!state_is_registered(p), "bad base64");
    }

    #[test]
    fn the_marker_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        mark_joined(d.path()).unwrap();
        let mode = std::fs::metadata(d.path().join(JOINED_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn known_names_of_the_wrong_type_are_unknown() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("state");
        std::fs::create_dir_all(dir.join("tailscaled.state")).unwrap();
        std::os::unix::fs::symlink("/etc", dir.join("certs")).unwrap();
        match plan_removal(&dir) {
            Err(RemoveError::Unknown(u)) => assert_eq!(u, ["certs", "tailscaled.state"]),
            other => panic!("{other:?}"),
        }
        assert!(Path::new("/etc/passwd").exists());
    }

    #[test]
    fn the_marker_is_removed_first_so_a_partial_removal_is_not_joined() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("state");
        let locked = dir.join("certs/inner");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("f"), b"x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        mark_joined(&dir).unwrap();
        std::fs::write(dir.join("tailscaled.state"), b"{}").unwrap();
        let r = remove_state_dir(&dir);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(r.is_err(), "removal must fail part-way for this test");
        assert!(!state_dir_seeded(&dir), "marker must be gone first");
    }

    #[test]
    fn remove_marker_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        mark_joined(d.path()).unwrap();
        remove_marker(d.path()).unwrap();
        assert!(!state_dir_seeded(d.path()));
        remove_marker(d.path()).unwrap();
    }
}
