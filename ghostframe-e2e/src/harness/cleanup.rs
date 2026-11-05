/// Read `docker logs <container>` (stdout + stderr concatenated) and strip
/// ANSI escape sequences so substring assertions are stable across TTY /
/// non-TTY contexts. Used by tests that assert on tracing-subscriber output.
pub fn read_server_logs_stripped(container_name: &str) -> String {
    let out = std::process::Command::new("docker")
        .args(["logs", container_name])
        .output()
        .expect("running docker logs");
    let raw = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    raw.chars()
        .fold((String::new(), false), |(mut acc, in_esc), c| {
            if in_esc {
                (acc, c != 'm')
            } else if c == '\x1b' {
                (acc, true)
            } else {
                acc.push(c);
                (acc, false)
            }
        })
        .0
}

/// Pre-test hygiene: remove stale `/tmp/.X11-unix/X<N>` socket files and
/// matching `/tmp/.X<N>-lock` lock files for `N in 0..=20`, but skip any
/// socket that's currently backed by a live X server (connect probe returns
/// Ok), plus leftover `/tmp/ghostframe-weston-*` runtime dirs and log files.
/// Without this, accumulated stale sockets from prior test runs eventually
/// exhaust the low display numbers XWayland picks from (it starts at :1).
///
/// Called once at the top of `setup_e2e_inner` so it runs once per test.
///
/// **Serialization assumption**: this helper assumes the project's
/// `--test-threads=1` convention — only one test setup runs at a
/// time. The cleanup → `spawn_weston_headless` sequence within a SINGLE
/// test is safe because both are synchronous.
pub fn cleanup_stale_xvfb_sockets() {
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    let mut removed = 0;
    // XWayland-launched-by-Weston picks low display numbers (typically :1).
    // Old Xvfb-based runs used :99..:199 — sweep both ranges so this works
    // during the transition and stays robust afterwards.
    let ranges: &[std::ops::RangeInclusive<u32>] = &[0..=20, 99..=199];
    for range in ranges {
        for n in range.clone() {
            let socket = format!("/tmp/.X11-unix/X{n}");
            let lock = format!("/tmp/.X{n}-lock");
            if !Path::new(&socket).exists() && !Path::new(&lock).exists() {
                continue;
            }
            match UnixStream::connect(&socket) {
                Ok(_) => continue, // live X server bound; keep
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    let _ = std::fs::remove_file(&socket);
                    let _ = std::fs::remove_file(&lock);
                    removed += 1;
                }
                Err(_) => continue,
            }
        }
    }
    // Sweep stale Weston runtime dirs + log files belonging to PIDs that
    // are no longer alive. Cheap: stat /proc/<pid>.
    if let Ok(entries) = std::fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let pid_str = if let Some(rest) = name.strip_prefix("ghostframe-weston-") {
                rest.split('-')
                    .next()
                    .or(Some(rest.trim_end_matches(".log")))
            } else {
                None
            };
            let Some(pid_str) = pid_str else { continue };
            let Ok(pid) = pid_str.parse::<u32>() else {
                continue;
            };
            if Path::new(&format!("/proc/{pid}")).exists() {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
            removed += 1;
        }
    }
    if removed > 0 {
        eprintln!("cleanup_stale_xvfb_sockets: removed {removed} stale entries");
    }
}

/// What the server's own stats lines say about the H.264 acked-reference
/// path (Phase B): see `docs/design/h264-transport.md` section 10.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckedRefEvidence {
    /// The last `h264 refs` line, verbatim.
    pub refs_line: String,
    /// The last `h264 stats` line, verbatim.
    pub stats_line: String,
    /// `plans` on the refs line: frames the `RefManager` planned.
    pub plans: u64,
    /// `idr_gop_periodic` on the stats line.
    pub idr_gop_periodic: u64,
}

/// Value of the numeric field `name=<digits>` on a log line.
fn log_field_u64(line: &str, name: &str) -> Option<u64> {
    let needle = format!("{name}=");
    let mut from = 0;
    while let Some(i) = line[from..].find(&needle) {
        let at = from + i;
        // Whole-word match: `plans=` must not match `late_plans=`.
        if at == 0
            || !line.as_bytes()[at - 1].is_ascii_alphanumeric() && line.as_bytes()[at - 1] != b'_'
        {
            let rest = &line[at + needle.len()..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            return rest[..end].parse().ok();
        }
        from = at + needle.len();
    }
    None
}

/// Check server logs for proof that the acked-reference path ran: a
/// `h264 refs` line whose `backend` is `libva-direct` (the field is only set
/// once the `RefManager` planned a frame that the libva-direct worker coded)
/// with `plans > 0`, and an `h264 stats` line showing no scheduled GOP IDR
/// (a refs session has none). The `h264 refs` line is logged only for a
/// session that has a `RefManager` (or had planned a frame), so its mere
/// presence with a non-zero `plans` rules out the ffmpeg path.
pub fn acked_ref_evidence(logs: &str) -> Result<AckedRefEvidence, String> {
    let refs_line = logs
        .lines()
        .rfind(|l| l.contains("h264 refs (acked-reference recovery"))
        .ok_or(
            "no `h264 refs` line: the RefManager path never ran (or no stats cadence elapsed)",
        )?;
    if !refs_line.contains(r#"backend="libva-direct""#) {
        return Err(format!(
            "`h264 refs` backend is not libva-direct: {refs_line}"
        ));
    }
    let plans = log_field_u64(refs_line, "plans").ok_or("no plans= on the refs line")?;
    if plans == 0 {
        return Err(format!("`h264 refs` shows plans=0: {refs_line}"));
    }
    let stats_line = logs
        .lines()
        .rfind(|l| l.contains("h264 stats (IDR causes"))
        .ok_or("no `h264 stats` line")?;
    let idr_gop_periodic = log_field_u64(stats_line, "idr_gop_periodic")
        .ok_or("no idr_gop_periodic= on the stats line")?;
    if idr_gop_periodic != 0 {
        return Err(format!(
            "a scheduled GOP IDR was emitted (idr_gop_periodic={idr_gop_periodic}); a refs session has none: {stats_line}"
        ));
    }
    Ok(AckedRefEvidence {
        refs_line: refs_line.to_string(),
        stats_line: stats_line.to_string(),
        plans,
        idr_gop_periodic,
    })
}

/// What a run showed about the acked-reference path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckedRefOutcome {
    /// The libva-direct probe passed and the refs path ran.
    Verified(AckedRefEvidence),
    /// The probe failed and the server logged the ffmpeg fallback: the refs
    /// path was NOT exercised (a runner with no VA-API driver).
    FallbackNotExercised(String),
}

/// `GHOSTFRAME_E2E_REQUIRE_VAAPI=1` (any non-empty value but `0`): a failed
/// libva-direct probe is a test failure. Unset on GitHub CI, which has no GPU.
pub fn require_vaapi() -> bool {
    std::env::var("GHOSTFRAME_E2E_REQUIRE_VAAPI").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Strict when the probe succeeded (or `require_vaapi`): the refs path must
/// have run. When the probe failed and VA-API is not required, the fallback
/// line must be there, and the outcome says loudly that nothing was proved.
pub fn acked_ref_outcome(logs: &str, require_vaapi: bool) -> Result<AckedRefOutcome, String> {
    let probe_ok = logs
        .lines()
        .any(|l| l.contains("libva-direct capability probe") && l.contains("ok=true"));
    if probe_ok || require_vaapi {
        if !probe_ok {
            return Err(
                "GHOSTFRAME_E2E_REQUIRE_VAAPI is set but the libva-direct capability probe did \
                 not pass (no `libva-direct capability probe ... ok=true` line)"
                    .into(),
            );
        }
        return acked_ref_evidence(logs).map(AckedRefOutcome::Verified);
    }
    let fallback = logs
        .lines()
        .find(|l| l.contains("acked-reference H.264 unavailable, using the ffmpeg encoder"))
        .ok_or("the libva-direct probe did not pass and no fallback line was logged")?;
    Ok(AckedRefOutcome::FallbackNotExercised(fallback.to_string()))
}

/// Report an outcome: print the evidence, or shout that the refs path was not
/// exercised. Panics on error.
pub fn report_acked_ref_outcome(outcome: Result<AckedRefOutcome, String>) {
    match outcome {
        Ok(AckedRefOutcome::Verified(ev)) => {
            eprintln!("[b5b] {}\n[b5b] {}", ev.refs_line, ev.stats_line)
        }
        Ok(AckedRefOutcome::FallbackNotExercised(line)) => eprintln!(
            "[b5b] WARNING: THE ACKED-REFERENCE PATH WAS NOT EXERCISED (no libva-direct on this \
             machine; the server fell back to ffmpeg: {line}). Set GHOSTFRAME_E2E_REQUIRE_VAAPI=1 \
             to make this a failure."
        ),
        Err(why) => panic!("the acked-reference path did not run: {why}"),
    }
}

/// Poll the container's logs until `acked_ref_outcome` is Ok or `timeout`
/// passes; the stats lines come every 60 frames, so a short test has to wait
/// for the first. Returns the last error on timeout, with the logs.
pub async fn wait_for_acked_ref_outcome(
    container_name: &str,
    timeout: std::time::Duration,
) -> Result<AckedRefOutcome, String> {
    let deadline = std::time::Instant::now() + timeout;
    let require = require_vaapi();
    loop {
        let logs = read_server_logs_stripped(container_name);
        match acked_ref_outcome(&logs, require) {
            Ok(e) => return Ok(e),
            Err(why) if std::time::Instant::now() >= deadline => {
                return Err(format!("{why}\nserver logs:\n{logs}"));
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
}

#[cfg(test)]
mod acked_ref_tests {
    use super::acked_ref_evidence;

    const REFS: &str = r#"INFO ghostframe::h264: h264 refs (acked-reference recovery; cumulative) frame_seq=120 backend="libva-direct" plans=118 holds=0"#;
    const STATS: &str = "INFO ghostframe::h264: h264 stats (IDR causes, verdicts, hold, frame skips, ledger accounting; cumulative) frame_seq=120 idr_session=1 idr_gop_periodic=0";

    #[test]
    fn accepts_a_refs_session() {
        let ev = acked_ref_evidence(&format!("{REFS}\n{STATS}\n")).expect("evidence");
        assert_eq!((ev.plans, ev.idr_gop_periodic), (118, 0));
    }

    #[test]
    fn outcome_is_strict_only_with_a_passing_probe_or_the_switch() {
        use super::{acked_ref_outcome, AckedRefOutcome};
        let ok = "DEBUG vaapi_direct: libva-direct capability probe ms=10 ok=true";
        let bad = "DEBUG vaapi_direct: libva-direct capability probe ms=10 ok=false";
        let fb = "INFO io_bridge: acked-reference H.264 unavailable, using the ffmpeg encoder: x";
        // Probe passed but no refs line: failure.
        assert!(acked_ref_outcome(&format!("{ok}\n{STATS}"), false).is_err());
        assert!(matches!(
            acked_ref_outcome(&format!("{ok}\n{REFS}\n{STATS}"), false),
            Ok(AckedRefOutcome::Verified(_))
        ));
        // Probe failed: lenient with the fallback line, failure when required.
        let logs = format!("{bad}\n{fb}\n{STATS}");
        assert!(matches!(
            acked_ref_outcome(&logs, false),
            Ok(AckedRefOutcome::FallbackNotExercised(_))
        ));
        assert!(acked_ref_outcome(&logs, true).is_err());
        // Probe failed and no fallback line: failure even when lenient.
        assert!(acked_ref_outcome(&format!("{bad}\n{STATS}"), false).is_err());
    }

    #[test]
    fn rejects_the_ffmpeg_path_and_a_gop_idr() {
        assert!(acked_ref_evidence(STATS).is_err(), "no refs line");
        let ffmpeg = REFS.replace("libva-direct", "pcm");
        assert!(acked_ref_evidence(&format!("{ffmpeg}\n{STATS}")).is_err());
        let zero = REFS.replace("plans=118", "plans=0");
        assert!(acked_ref_evidence(&format!("{zero}\n{STATS}")).is_err());
        let gop = STATS.replace("idr_gop_periodic=0", "idr_gop_periodic=2");
        assert!(acked_ref_evidence(&format!("{REFS}\n{gop}")).is_err());
    }
}
