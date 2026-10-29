//! A private Xvfb per test.
//!
//! Tests that paint root, resize the screen or run without MIT-SHM must not
//! share a server: CI runs the crate's tests in parallel under one
//! `xvfb-run`, and a developer's `$DISPLAY` is a live session. Each
//! `PrivateXvfb` picks a free display itself (`-displayfd`) and is killed on
//! drop.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

pub struct PrivateXvfb {
    child: Child,
    display: String,
}

impl PrivateXvfb {
    /// Start `Xvfb -displayfd 1 -nolisten tcp -noreset <args>` and wait until
    /// it reports its display number. `-noreset`: Xvfb otherwise resets, and
    /// clears root, whenever its last client disconnects.
    pub fn spawn(args: &[&str]) -> Self {
        let mut child = Command::new("Xvfb")
            .args(["-displayfd", "1", "-nolisten", "tcp", "-noreset"])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn Xvfb (CI installs it through _setup-system-deps)");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut line)
            .expect("read the display number Xvfb prints");
        let n: u32 = line
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("Xvfb printed {line:?}, not a display number"));
        Self {
            child,
            display: format!(":{n}"),
        }
    }

    /// `":N"`, for `x11rb::connect(Some(..))`.
    pub fn display(&self) -> &str {
        &self.display
    }
}

impl Drop for PrivateXvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
