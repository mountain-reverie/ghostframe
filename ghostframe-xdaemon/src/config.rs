//! Every environment variable `ghostframe-xdaemon` reads, parsed once.
//!
//! `XdaemonConfig::from_env()` is called once at the top of `main()` and the
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
//! | `DISPLAY` | X display (diagnostics only; x11rb reads it itself) | unset |
//! | `GHOSTFRAME_LOG_FORMAT` | `json` for JSON logs | text |
//! | `TS_HOSTNAME` | tsnet node name | `ghostframe-server` |
//! | `TS_STATE_DIR` | tsnet state directory | `/tmp/ghostframe-ts` |
//! | `TS_AUTHKEY` | tsnet auth key (needed for `--init`) | unset |
//! | `TS_CONTROL_URL` | tsnet control server | empty |
//! | `CAPTURE_FPS` | capture rate; invalid or 0 falls back | 30 |
//! | `GHOSTFRAME_NO_X11_WAIT` | skip the X11 readiness wait (DRM-direct e2e) | off |
//! | `GHOSTFRAME_X11_CAPTURE_ONLY` | skip the DRM probe | off |
//! | `GHOSTFRAME_CAPTURE_DUMP_DIR` | write each frame as PNG here | off |
//! | `GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY` | trace heartbeat every N frames | 5 |
//! | `GHOSTFRAME_DUMP_FIRST_FRAME` | dump first raw frame to this path | off |
//! | `GHOSTFRAME_FORCE_ROOT_GET_IMAGE` | bypass compositor probe | off |
//! | `GHOSTFRAME_ATTACH_MAX_RESOLUTION` | `WxH` ceiling in attach mode | none |

use crate::util::env_flag;

pub const DEFAULT_CAPTURE_FPS: u64 = 30;
const DEFAULT_HEARTBEAT_EVERY: u64 = 5;

#[derive(Debug, Clone)]
pub struct XdaemonConfig {
    pub display: Option<String>,
    pub log_json: bool,
    pub ts_hostname: String,
    pub ts_state_dir: String,
    pub ts_authkey: Option<String>,
    pub ts_control_url: String,
    /// Always >= 1.
    pub capture_fps: u64,
    pub no_x11_wait: bool,
    pub x11_capture_only: bool,
    pub capture_dump_dir: Option<std::path::PathBuf>,
    /// Always >= 1 (it is used as a modulus).
    pub heartbeat_every: u64,
    pub dump_first_frame: Option<String>,
    pub force_root_get_image: bool,
    pub attach_max_resolution: Option<String>,
}

impl XdaemonConfig {
    /// Read the process environment. Call once, from `main()`.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Build from an arbitrary lookup, so tests need no process-global state.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let nonempty = |k: &str| get(k).filter(|s| !s.is_empty());
        Self {
            display: get("DISPLAY"),
            log_json: get("GHOSTFRAME_LOG_FORMAT").as_deref() == Some("json"),
            ts_hostname: get("TS_HOSTNAME").unwrap_or_else(|| "ghostframe-server".into()),
            ts_state_dir: get("TS_STATE_DIR").unwrap_or_else(|| "/tmp/ghostframe-ts".into()),
            ts_authkey: get("TS_AUTHKEY"),
            ts_control_url: get("TS_CONTROL_URL").unwrap_or_default(),
            capture_fps: parse_positive("CAPTURE_FPS", get("CAPTURE_FPS"), DEFAULT_CAPTURE_FPS),
            no_x11_wait: env_flag(get("GHOSTFRAME_NO_X11_WAIT").as_deref()),
            x11_capture_only: env_flag(get("GHOSTFRAME_X11_CAPTURE_ONLY").as_deref()),
            capture_dump_dir: nonempty("GHOSTFRAME_CAPTURE_DUMP_DIR").map(Into::into),
            heartbeat_every: parse_positive(
                "GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY",
                get("GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY"),
                DEFAULT_HEARTBEAT_EVERY,
            ),
            dump_first_frame: get("GHOSTFRAME_DUMP_FIRST_FRAME"),
            force_root_get_image: env_flag(get("GHOSTFRAME_FORCE_ROOT_GET_IMAGE").as_deref()),
            attach_max_resolution: get("GHOSTFRAME_ATTACH_MAX_RESOLUTION"),
        }
    }
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

    fn cfg(pairs: &[(&str, &str)]) -> XdaemonConfig {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        XdaemonConfig::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn defaults() {
        let c = cfg(&[]);
        assert_eq!(c.capture_fps, 30);
        assert_eq!(c.heartbeat_every, 5);
        assert_eq!(c.ts_state_dir, "/tmp/ghostframe-ts");
        assert!(!c.log_json && !c.no_x11_wait && !c.x11_capture_only);
        assert!(c.capture_dump_dir.is_none());
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
            ("GHOSTFRAME_X11_CAPTURE_ONLY", "1"),
            ("GHOSTFRAME_NO_X11_WAIT", "0"),
            ("GHOSTFRAME_CAPTURE_DUMP_DIR", ""),
            ("GHOSTFRAME_LOG_FORMAT", "json"),
        ]);
        assert!(c.x11_capture_only && c.log_json);
        assert!(!c.no_x11_wait);
        assert!(c.capture_dump_dir.is_none());
    }
}
