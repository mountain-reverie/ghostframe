// SPDX-License-Identifier: AGPL-3.0-or-later
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

/// Check server logs for proof that the acked-reference path ran, whichever
/// encoder carried it: see [`acked_ref_evidence_for`].
pub fn acked_ref_evidence(logs: &str) -> Result<AckedRefEvidence, String> {
    acked_ref_evidence_for(logs, &["libva-direct", "openh264"])
}

/// Check server logs for proof that the acked-reference path ran on one of
/// `backends`: a `h264 refs` line whose `backend` is one of them (the field is
/// only set once the `RefManager` planned a frame that the backend's worker
/// coded) with `plans > 0`, and an `h264 stats` line that does not say H.264
/// was given up on (`h264_unavailable`). For openh264 (observed mode) that line must
/// also show `marks_observed > 0`: the library marked a long-term picture and
/// the manager read it. (Those counters are on `h264 stats`, not `h264 refs`.)
/// The `h264 refs` line is logged only for a session that has a `RefManager`
/// (or had planned a frame).
pub fn acked_ref_evidence_for(logs: &str, backends: &[&str]) -> Result<AckedRefEvidence, String> {
    let refs_line = logs
        .lines()
        .rfind(|l| l.contains("h264 refs (acked-reference recovery"))
        .ok_or(
            "no `h264 refs` line: the RefManager path never ran (or no stats cadence elapsed)",
        )?;
    let backend = backends
        .iter()
        .find(|b| refs_line.contains(&format!(r#"backend="{b}""#)))
        .ok_or_else(|| format!("`h264 refs` backend is not one of {backends:?}: {refs_line}"))?;
    let plans = log_field_u64(refs_line, "plans").ok_or("no plans= on the refs line")?;
    if plans == 0 {
        return Err(format!("`h264 refs` shows plans=0: {refs_line}"));
    }
    let stats_line = logs
        .lines()
        .rfind(|l| l.contains("h264 stats (IDR causes"))
        .ok_or("no `h264 stats` line")?;
    if stats_line.contains(r#"h264_unavailable="process""#)
        || stats_line.contains(r#"h264_unavailable="this_size""#)
    {
        return Err(format!(
            "no encoder could code the stream (h264_unavailable is set): {stats_line}"
        ));
    }
    if *backend == "openh264" && log_field_u64(stats_line, "marks_observed").unwrap_or(0) == 0 {
        return Err(format!(
            "openh264 ran but the manager observed no long-term mark (marks_observed): {stats_line}"
        ));
    }
    Ok(AckedRefEvidence {
        refs_line: refs_line.to_string(),
        stats_line: stats_line.to_string(),
        plans,
    })
}

/// What a run showed about the acked-reference path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckedRefOutcome {
    /// The refs path ran: on libva-direct (its probe passed) or, where it did
    /// not, on openh264 in observed mode.
    Verified(AckedRefEvidence),
}

/// `GHOSTFRAME_E2E_REQUIRE_VAAPI=1` (any non-empty value but `0`): the stream
/// must be libva-direct's; a failed probe is a test failure, and so is a
/// session that openh264 carried. Unset on GitHub CI, which has no GPU.
pub fn require_vaapi() -> bool {
    std::env::var("GHOSTFRAME_E2E_REQUIRE_VAAPI").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Strict when the probe succeeded (or `require_vaapi`, which accepts nothing
/// but libva-direct): the refs path must have run. When the probe failed and
/// VA-API is not required, an openh264 session that ran the refs path counts
/// as the acked-reference outcome (the server image carries libopenh264, so
/// `auto` takes it); failing that, there is no outcome: no encoder carried
/// the stream, and the test fails.
pub fn acked_ref_outcome(logs: &str, require_vaapi: bool) -> Result<AckedRefOutcome, String> {
    let probe_ok = logs
        .lines()
        .any(|l| l.contains("libva-direct capability probe") && l.contains("ok=true"));
    if require_vaapi {
        if !probe_ok {
            return Err(
                "GHOSTFRAME_E2E_REQUIRE_VAAPI is set but the libva-direct capability probe did \
                 not pass (no `libva-direct capability probe ... ok=true` line)"
                    .into(),
            );
        }
        return acked_ref_evidence_for(logs, &["libva-direct"]).map(AckedRefOutcome::Verified);
    }
    if probe_ok {
        return acked_ref_evidence(logs).map(AckedRefOutcome::Verified);
    }
    acked_ref_evidence_for(logs, &["openh264"])
        .map(AckedRefOutcome::Verified)
        .map_err(|why| {
            format!(
                "the libva-direct probe did not pass and openh264 did not run the refs path: {why}"
            )
        })
}

/// Report an outcome: print the evidence. Panics on error.
pub fn report_acked_ref_outcome(outcome: Result<AckedRefOutcome, String>) {
    match outcome {
        Ok(AckedRefOutcome::Verified(ev)) => {
            eprintln!("[b5b] {}\n[b5b] {}", ev.refs_line, ev.stats_line)
        }
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
    const STATS: &str = r#"INFO ghostframe::h264: h264 stats (IDR causes, verdicts, frame skips, ledger accounting; cumulative) frame_seq=120 h264_unavailable="" idr_session=1"#;

    #[test]
    fn accepts_a_refs_session() {
        let ev = acked_ref_evidence(&format!("{REFS}\n{STATS}\n")).expect("evidence");
        assert_eq!(ev.plans, 118);
    }

    #[test]
    fn outcome_is_strict_only_with_a_passing_probe_or_the_switch() {
        use super::{acked_ref_outcome, AckedRefOutcome};
        let ok = "DEBUG vaapi_direct: libva-direct capability probe ms=10 ok=true";
        let bad = "DEBUG vaapi_direct: libva-direct capability probe ms=10 ok=false";
        // Probe passed but no refs line: failure.
        assert!(acked_ref_outcome(&format!("{ok}\n{STATS}"), false).is_err());
        assert!(matches!(
            acked_ref_outcome(&format!("{ok}\n{REFS}\n{STATS}"), false),
            Ok(AckedRefOutcome::Verified(_))
        ));
        // Probe failed and nothing else ran the refs path: a failure, whether
        // or not VA-API is required (there is no fallback encoder to excuse it).
        let logs = format!("{bad}\n{STATS}");
        assert!(acked_ref_outcome(&logs, false).is_err());
        assert!(acked_ref_outcome(&logs, true).is_err());
    }

    #[test]
    fn an_openh264_session_is_an_acked_reference_outcome_unless_vaapi_is_required() {
        use super::{acked_ref_outcome, AckedRefOutcome};
        let bad = "DEBUG vaapi_direct: libva-direct capability probe ms=10 ok=false";
        let refs = REFS.replace("libva-direct", "openh264");
        let stats = format!("{STATS} marks_observed=4");
        let logs = format!("{bad}\n{refs}\n{stats}");
        // No libva-direct on the host, openh264 carried the stream: verified.
        assert!(matches!(
            acked_ref_outcome(&logs, false),
            Ok(AckedRefOutcome::Verified(_))
        ));
        // The same with no probe line at all (the probe is DEBUG-level).
        assert!(matches!(
            acked_ref_outcome(&format!("{refs}\n{stats}"), false),
            Ok(AckedRefOutcome::Verified(_))
        ));
        // REQUIRE_VAAPI means libva-direct, whatever else ran.
        assert!(acked_ref_outcome(&logs, true).is_err());
        let ok = "DEBUG vaapi_direct: libva-direct capability probe ms=10 ok=true";
        assert!(acked_ref_outcome(&format!("{ok}\n{refs}\n{stats}"), true).is_err());
        // An openh264 session that the manager never saw mark anything did not
        // run the observed-mode path: not a verified outcome.
        let unmarked = STATS.to_string() + " marks_observed=0";
        let no_fallback = format!("{bad}\n{refs}\n{unmarked}");
        assert!(acked_ref_outcome(&no_fallback, false).is_err());
    }

    #[test]
    fn rejects_a_session_with_no_plans_or_no_encoder() {
        assert!(acked_ref_evidence(STATS).is_err(), "no refs line");
        let other = REFS.replace("libva-direct", "pcm");
        assert!(acked_ref_evidence(&format!("{other}\n{STATS}")).is_err());
        let zero = REFS.replace("plans=118", "plans=0");
        assert!(acked_ref_evidence(&format!("{zero}\n{STATS}")).is_err());
        let given_up = STATS.replace(r#"h264_unavailable="""#, r#"h264_unavailable="process""#);
        assert!(acked_ref_evidence(&format!("{REFS}\n{given_up}")).is_err());
    }
}
