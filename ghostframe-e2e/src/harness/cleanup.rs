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
