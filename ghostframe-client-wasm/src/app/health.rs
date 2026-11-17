// SPDX-License-Identifier: MIT OR Apache-2.0
//! Swallowed-exception accounting and a small console ring: the Rust port of
//! `ghostframe-web-client/src/health.ts`. The client keeps running after a
//! throw (one bad event must not freeze the session), so this keeps a count,
//! the first message and the most recent ones, for a failing e2e run to read.

use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};

const MAX_RECENT: usize = 20;
const MAX_CONSOLE: usize = 300;
/// The first lines are kept apart, so a long run cannot rotate them out.
const MAX_CONSOLE_HEAD: usize = 100;
const MAX_MARKS: usize = 12;
const MAX_TALLY_KEYS: usize = 64;

/// A tile remembered for pixel read-back on failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Mark {
    pub tile_x: u32,
    pub tile_y: u32,
    pub frame_seq: u64,
    pub what: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthSnapshot {
    pub swallowed: u64,
    pub by_site: BTreeMap<String, u32>,
    pub first: String,
    pub recent: Vec<String>,
    pub console: Vec<String>,
    pub console_head: Vec<String>,
    pub tallies: BTreeMap<String, u32>,
    pub marks: Vec<Mark>,
}

#[derive(Debug, Default)]
pub struct Health {
    swallowed: u64,
    by_site: BTreeMap<String, u32>,
    first: String,
    recent: VecDeque<String>,
    console_ring: VecDeque<String>,
    console_head: Vec<String>,
    tallies: BTreeMap<String, u32>,
    marks: Vec<Mark>,
}

impl Health {
    /// Remember a few interesting tiles (bounded).
    pub fn mark(&mut self, tile_x: u32, tile_y: u32, frame_seq: u64, what: &str) {
        if self.marks.len() < MAX_MARKS {
            self.marks.push(Mark {
                tile_x,
                tile_y,
                frame_seq,
                what: what.to_owned(),
            });
        }
    }

    /// Count an observation (bounded: at most 64 distinct keys; an existing
    /// key still counts once the bound is reached).
    pub fn tally(&mut self, key: &str) {
        if let Some(n) = self.tallies.get_mut(key) {
            *n += 1;
        } else if self.tallies.len() < MAX_TALLY_KEYS {
            self.tallies.insert(key.to_owned(), 1);
        }
    }

    /// Record a swallowed error as `"{site}: {message}"`.
    pub fn record(&mut self, site: &str, message: &str) {
        self.swallowed += 1;
        *self.by_site.entry(site.to_owned()).or_insert(0) += 1;
        let msg = format!("{site}: {message}");
        if self.first.is_empty() {
            self.first = msg.clone();
        }
        if self.recent.len() == MAX_RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(msg);
    }

    /// Append a console line to the head (the first lines) and the ring.
    pub fn note(&mut self, line: &str) {
        if self.console_head.len() < MAX_CONSOLE_HEAD {
            self.console_head.push(line.to_owned());
        }
        if self.console_ring.len() == MAX_CONSOLE {
            self.console_ring.pop_front();
        }
        self.console_ring.push_back(line.to_owned());
    }

    pub fn snapshot(&self) -> HealthSnapshot {
        HealthSnapshot {
            swallowed: self.swallowed,
            by_site: self.by_site.clone(),
            first: self.first.clone(),
            recent: self.recent.iter().cloned().collect(),
            console: self.console_ring.iter().cloned().collect(),
            console_head: self.console_head.clone(),
            tallies: self.tallies.clone(),
            marks: self.marks.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_swallowed_errors_per_site_and_keeps_the_first_message() {
        let mut h = Health::default();
        h.record("handleEvent:TilePayload", "boom");
        h.record("handleEvent:TilePayload", "again");
        h.record("encodeAndPresentFrame", "str");
        let s = h.snapshot();
        assert_eq!(s.swallowed, 3);
        assert_eq!(s.by_site["handleEvent:TilePayload"], 2);
        assert_eq!(s.first, "handleEvent:TilePayload: boom");
        assert_eq!(s.recent.len(), 3);
    }

    #[test]
    fn bounds_the_recent_list_and_the_console_ring() {
        let mut h = Health::default();
        for i in 0..25 {
            h.record("x", &i.to_string());
        }
        for i in 0..350 {
            h.note(&format!("l{i}"));
        }
        let s = h.snapshot();
        assert_eq!(s.recent.len(), MAX_RECENT);
        assert_eq!(s.recent.last().unwrap(), "x: 24");
        assert_eq!(s.console.len(), MAX_CONSOLE);
        assert_eq!(s.console.last().unwrap(), "l349");
        assert_eq!(s.console_head.len(), MAX_CONSOLE_HEAD);
        assert_eq!(s.console_head[0], "l0");
    }

    #[test]
    fn tallies_and_marks_are_bounded() {
        let mut h = Health::default();
        for i in 0..70 {
            h.tally(&format!("k{i}"));
        }
        // An existing key still counts once the bound is reached.
        h.tally("k0");
        for i in 0..15 {
            h.mark(i, 0, u64::from(i), "m");
        }
        let s = h.snapshot();
        assert_eq!(s.tallies.len(), MAX_TALLY_KEYS);
        assert_eq!(s.tallies["k0"], 2);
        assert_eq!(s.marks.len(), MAX_MARKS);
    }
}
