//! Every environment variable `ghostframe-server` reads, parsed once.
//!
//! `ServerConfig::from_env()` is called once at the top of `main()` and the
//! result is passed down. This is the only file in the crate allowed to touch
//! `std::env` (CI greps for it), which keeps three things true:
//!
//! - a bad value is handled in one place, with a warning and a default,
//!   instead of an `expect` somewhere deep in startup;
//! - nothing mutates the process environment at runtime (`remove_var` is
//!   unsound in a multi-threaded process on glibc);
//! - this list is the complete, documented set.
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `DISPLAY` | X display; passed explicitly to the capture, damage and input connections | unset |
//! | `WAYLAND_DISPLAY` | only to refuse a Wayland-only session with a clear error | unset |
//! | `GHOSTFRAME_LOG_FORMAT` | `json` for JSON logs | text |
//! | `TS_HOSTNAME` | tsnet node name | `<hostname>-ghostframe` (`ghostframe-server` if the host has no name) |
//! | `TS_STATE_DIR` | tsnet state directory (holds node keys) | `$XDG_STATE_HOME/ghostframe`, else `~/.local/state/ghostframe` |
//! | `XDG_STATE_HOME`, `HOME` | the default state directory; `HOME` also roots `install` | unset |
//! | `TS_AUTHKEY` | tsnet auth key: `login` without a browser | unset |
//! | `TS_CONTROL_URL` | tsnet control server | empty |
//! | `CAPTURE_FPS` | capture rate; invalid or 0 falls back | 30 |
//! | `GHOSTFRAME_CAPTURE_DUMP_DIR` | write each frame as PNG here (test builds only) | off |
//! | `GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY` | trace heartbeat every N frames | 5 |
//! | `GHOSTFRAME_DUMP_FIRST_FRAME` | dump first raw frame to this path (test builds only) | off |
//! | `GHOSTFRAME_RANDR_SETTLE_MS` | least wait after a mode change before checking the desktop kept it | 5000 (`0` turns the read-back off) |
//! | `GHOSTFRAME_INSTALL_DETACHED` | `1`: `install` is the re-exec'd copy running as a transient unit; never re-exec again | unset |
//! | `XDG_RUNTIME_DIR` | whether a user systemd manager can be asked about the old xdaemon (`login`, `capture`) | unset |
//! | `GHOSTFRAME_MAX_RESOLUTION` | `WxH` resolution ceiling, or `none` | none (the unit `install` writes sets `1920x1080`) |

pub const DEFAULT_CAPTURE_FPS: u64 = 30;
const DEFAULT_HEARTBEAT_EVERY: u64 = 5;
/// How long after a mode change the server waits before it checks that the
/// desktop kept it. It then also waits for 1.5 s without a RandR event, and
/// gives up waiting after 12 s. Observed on the owner's Enlightenment: stock
/// E reverts a change about 1.7 s after it; the patched E defers adopting or
/// reverting for about 3 s plus its 1 s `hotplug_response`, so about 4 s.
/// 5 s clears both.
pub const DEFAULT_RANDR_SETTLE_MS: u64 = 5000;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub display: Option<String>,
    pub wayland_display: Option<String>,
    pub log_json: bool,
    pub ts_hostname: String,
    /// Explicit `TS_STATE_DIR`, if set and non-empty. Use [`Self::state_dir`].
    ts_state_dir: Option<String>,
    xdg_state_home: Option<String>,
    install_detached: bool,
    xdg_runtime_dir: Option<String>,
    home: Option<String>,
    pub ts_authkey: Option<String>,
    pub ts_control_url: String,
    /// Always >= 1.
    pub capture_fps: u64,
    pub capture_dump_dir: Option<std::path::PathBuf>,
    /// Always >= 1 (it is used as a modulus).
    pub heartbeat_every: u64,
    pub dump_first_frame: Option<String>,
    pub max_resolution: Option<String>,
    /// `GHOSTFRAME_RANDR_SETTLE_MS`; zero turns the read-back off.
    pub randr_settle: std::time::Duration,
}

impl ServerConfig {
    /// The tsnet state directory: `TS_STATE_DIR`, else
    /// `$XDG_STATE_HOME/ghostframe`, else `$HOME/.local/state/ghostframe`.
    /// Never a world-writable location: with none of the variables set there
    /// is no default, and the daemon refuses to start.
    pub fn state_dir(&self) -> Result<std::path::PathBuf, String> {
        use std::path::{Path, PathBuf};
        let abs = |what: &str, v: &str| {
            if Path::new(v).is_absolute() {
                Ok(PathBuf::from(v))
            } else {
                Err(format!("{what}={v:?} is not an absolute path"))
            }
        };
        if let Some(d) = &self.ts_state_dir {
            return abs("TS_STATE_DIR", d);
        }
        if let Some(x) = &self.xdg_state_home {
            return abs("XDG_STATE_HOME", x).map(|p| p.join("ghostframe"));
        }
        match &self.home {
            Some(h) => abs("HOME", h).map(|p| p.join(".local/state/ghostframe")),
            None => Err("no state directory: set TS_STATE_DIR, XDG_STATE_HOME or HOME".into()),
        }
    }

    /// `$HOME`, if set and non-empty: where `install` puts everything.
    pub fn home(&self) -> Option<&str> {
        self.home.as_deref()
    }

    /// The state directory the installed unit uses: its
    /// `TS_STATE_DIR=%S/ghostframe`, i.e. `$XDG_STATE_HOME/ghostframe` or
    /// `<home>/.local/state/ghostframe`. Ignores `TS_STATE_DIR`, which the
    /// unit overrides anyway.
    pub fn unit_state_dir(&self, home: &std::path::Path) -> Result<std::path::PathBuf, String> {
        match &self.xdg_state_home {
            Some(x) if std::path::Path::new(x).is_absolute() => {
                Ok(std::path::Path::new(x).join("ghostframe"))
            }
            Some(x) => Err(format!("XDG_STATE_HOME={x:?} is not an absolute path")),
            None => Ok(home.join(".local/state/ghostframe")),
        }
    }

    /// `XDG_STATE_HOME` as set in this shell, if any.
    pub fn xdg_state_home(&self) -> Option<&str> {
        self.xdg_state_home.as_deref()
    }

    /// Whether this is the re-exec'd copy of `install` (see `GHOSTFRAME_INSTALL_DETACHED`).
    pub fn install_detached(&self) -> bool {
        self.install_detached
    }

    /// Whether a user systemd manager is plausibly reachable (`XDG_RUNTIME_DIR` set).
    pub fn user_manager_possible(&self) -> bool {
        self.xdg_runtime_dir.is_some()
    }

    /// `TS_STATE_DIR` as set in this shell, if any.
    pub fn ts_state_dir(&self) -> Option<&str> {
        self.ts_state_dir.as_deref()
    }

    /// Read the process environment. Call once, from `main()`.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok(), crate::util::system_hostname())
    }

    /// Build from an arbitrary lookup and host name, so tests need no
    /// process-global state.
    pub fn from_lookup(
        get: impl Fn(&str) -> Option<String>,
        system_hostname: Option<String>,
    ) -> Self {
        // The two dump knobs write captured screen contents to a path the
        // environment names. They are honoured only in a test build; the
        // release daemon ignores them. ghostframe-server has no cargo features of its
        // own, so it follows the lib's `test-loss-injection`, which the e2e
        // image enables and `ci/check-release-features.sh` forbids in a
        // release build.
        let nonempty = |k: &str| get(k).filter(|s| !s.is_empty());
        let knob = |k: &str| {
            if ghostframe_lib::config::test_knobs_compiled() {
                get(k)
            } else {
                None
            }
        };
        Self {
            display: nonempty("DISPLAY"),
            wayland_display: nonempty("WAYLAND_DISPLAY"),
            log_json: get("GHOSTFRAME_LOG_FORMAT").as_deref() == Some("json"),
            ts_hostname: nonempty("TS_HOSTNAME")
                .unwrap_or_else(|| default_hostname(system_hostname)),
            ts_state_dir: nonempty("TS_STATE_DIR"),
            xdg_state_home: nonempty("XDG_STATE_HOME"),
            install_detached: get("GHOSTFRAME_INSTALL_DETACHED").as_deref() == Some("1"),
            xdg_runtime_dir: nonempty("XDG_RUNTIME_DIR"),
            home: nonempty("HOME"),
            ts_authkey: get("TS_AUTHKEY"),
            ts_control_url: get("TS_CONTROL_URL").unwrap_or_default(),
            capture_fps: parse_positive("CAPTURE_FPS", get("CAPTURE_FPS"), DEFAULT_CAPTURE_FPS),
            capture_dump_dir: knob("GHOSTFRAME_CAPTURE_DUMP_DIR")
                .filter(|s| !s.is_empty())
                .map(Into::into),
            heartbeat_every: parse_positive(
                "GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY",
                get("GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY"),
                DEFAULT_HEARTBEAT_EVERY,
            ),
            dump_first_frame: knob("GHOSTFRAME_DUMP_FIRST_FRAME"),
            max_resolution: nonempty("GHOSTFRAME_MAX_RESOLUTION"),
            randr_settle: std::time::Duration::from_millis(parse_millis(
                "GHOSTFRAME_RANDR_SETTLE_MS",
                get("GHOSTFRAME_RANDR_SETTLE_MS"),
                DEFAULT_RANDR_SETTLE_MS,
            )),
        }
    }
}

/// `<host>-ghostframe`, the name `login` and `capture` both register when
/// `TS_HOSTNAME` is unset; they must agree, or the node is renamed on every
/// start.
fn default_hostname(system_hostname: Option<String>) -> String {
    match system_hostname {
        Some(h) => format!("{h}-ghostframe"),
        None => "ghostframe-server".to_string(),
    }
}

/// Parse a non-negative integer (zero is meaningful); anything else warns and
/// yields `default`.
fn parse_millis(name: &str, raw: Option<String>, default: u64) -> u64 {
    let Some(raw) = raw else { return default };
    raw.trim().parse::<u64>().unwrap_or_else(|_| {
        eprintln!("warning: {name}={raw:?} is not a non-negative integer; using {default}");
        default
    })
}

/// Parse a strictly positive integer; anything else warns and yields `default`.
///
/// Logging is via `eprintln!` because this runs before the tracing subscriber
/// exists (the log format is itself configured here).
fn parse_positive(name: &str, raw: Option<String>, default: u64) -> u64 {
    let Some(raw) = raw else { return default };
    match raw.trim().parse::<u64>() {
        Ok(n) if n >= 1 => n,
        _ => {
            eprintln!("warning: {name}={raw:?} is not a positive integer; using {default}");
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> ServerConfig {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        ServerConfig::from_lookup(|k| m.get(k).cloned(), Some("testhost".into()))
    }

    #[test]
    fn unit_state_dir_follows_the_unit_not_ts_state_dir() {
        let h = std::path::Path::new("/h");
        let d = |pairs: &[(&str, &str)]| cfg(pairs).unit_state_dir(h);
        assert_eq!(d(&[]).unwrap(), h.join(".local/state/ghostframe"));
        assert_eq!(
            d(&[("TS_STATE_DIR", "/elsewhere")]).unwrap(),
            h.join(".local/state/ghostframe")
        );
        assert_eq!(
            d(&[("XDG_STATE_HOME", "/x")]).unwrap(),
            std::path::Path::new("/x/ghostframe")
        );
        assert!(d(&[("XDG_STATE_HOME", "rel")]).is_err());
    }

    #[test]
    fn randr_settle_defaults_parses_and_allows_zero() {
        assert_eq!(cfg(&[]).randr_settle.as_millis(), 5000);
        let ms = |v: &str| {
            cfg(&[("GHOSTFRAME_RANDR_SETTLE_MS", v)])
                .randr_settle
                .as_millis()
        };
        assert_eq!(ms("400"), 400);
        assert_eq!(ms("0"), 0);
        assert_eq!(ms("soon"), 5000);
    }

    #[test]
    fn defaults() {
        let c = cfg(&[]);
        assert_eq!(c.capture_fps, 30);
        assert_eq!(c.heartbeat_every, 5);
        assert!(!c.log_json);
        assert!(c.capture_dump_dir.is_none());
    }

    #[test]
    fn hostname_defaults_to_the_host_name() {
        assert_eq!(cfg(&[]).ts_hostname, "testhost-ghostframe");
        assert_eq!(
            cfg(&[("TS_HOSTNAME", "")]).ts_hostname,
            "testhost-ghostframe"
        );
        assert_eq!(cfg(&[("TS_HOSTNAME", "box")]).ts_hostname, "box");
        let nameless = ServerConfig::from_lookup(|_| None, None);
        assert_eq!(nameless.ts_hostname, "ghostframe-server");
    }

    #[test]
    fn max_resolution_is_read_from_its_new_name_only() {
        assert_eq!(
            cfg(&[("GHOSTFRAME_MAX_RESOLUTION", "2560x1440")])
                .max_resolution
                .as_deref(),
            Some("2560x1440")
        );
        assert_eq!(
            cfg(&[("GHOSTFRAME_ATTACH_MAX_RESOLUTION", "2560x1440")]).max_resolution,
            None
        );
        // An empty value means unset, like the neighbouring keys.
        assert_eq!(
            cfg(&[("GHOSTFRAME_MAX_RESOLUTION", "")]).max_resolution,
            None
        );
    }

    #[test]
    fn state_dir_precedence_and_no_tmp_default() {
        use std::path::PathBuf;
        let p = |pairs: &[(&str, &str)]| cfg(pairs).state_dir();
        assert_eq!(
            p(&[
                ("TS_STATE_DIR", "/x/s"),
                ("XDG_STATE_HOME", "/xdg"),
                ("HOME", "/h")
            ]),
            Ok(PathBuf::from("/x/s"))
        );
        assert_eq!(
            p(&[("XDG_STATE_HOME", "/xdg"), ("HOME", "/h")]),
            Ok(PathBuf::from("/xdg/ghostframe"))
        );
        assert_eq!(
            p(&[("XDG_STATE_HOME", ""), ("HOME", "/h")]),
            Ok(PathBuf::from("/h/.local/state/ghostframe"))
        );
        assert!(p(&[]).is_err(), "no variables: fail closed, never /tmp");
        assert!(p(&[("XDG_STATE_HOME", "rel")]).is_err());
        assert!(p(&[("HOME", "rel")]).is_err());
    }

    /// `CAPTURE_FPS` used to `expect()` (non-numeric) and divide by zero (0).
    #[test]
    fn bad_capture_fps_falls_back_to_default() {
        for bad in ["0", "abc", "", "-5", "30.5"] {
            assert_eq!(cfg(&[("CAPTURE_FPS", bad)]).capture_fps, 30, "{bad:?}");
        }
        assert_eq!(cfg(&[("CAPTURE_FPS", "60")]).capture_fps, 60);
    }

    /// The heartbeat value is a modulus; 0 would panic in `is_multiple_of`-style use.
    #[test]
    fn zero_heartbeat_falls_back() {
        assert_eq!(
            cfg(&[("GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY", "0")]).heartbeat_every,
            5
        );
        assert_eq!(
            cfg(&[("GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY", "9")]).heartbeat_every,
            9
        );
    }

    #[test]
    fn flags_and_paths() {
        let c = cfg(&[
            ("GHOSTFRAME_CAPTURE_DUMP_DIR", ""),
            ("GHOSTFRAME_LOG_FORMAT", "json"),
        ]);
        assert!(c.log_json);
        assert!(c.capture_dump_dir.is_none());
    }

    /// The dump knobs write screen contents to disk, so a release build must
    /// ignore them. In this crate's own test run the lib is built without
    /// `test-loss-injection`, so this exercises the ignored branch; under the
    /// e2e image's feature set it exercises the honoured one. Either way the
    /// result must follow `test_knobs_compiled()`.
    #[test]
    fn dump_knobs_follow_the_test_knob_switch() {
        let c = cfg(&[
            ("GHOSTFRAME_CAPTURE_DUMP_DIR", "/var/dump"),
            ("GHOSTFRAME_DUMP_FIRST_FRAME", "/var/first"),
        ]);
        let on = ghostframe_lib::config::test_knobs_compiled();
        assert_eq!(c.capture_dump_dir.is_some(), on);
        assert_eq!(c.dump_first_frame.is_some(), on);
    }
}
