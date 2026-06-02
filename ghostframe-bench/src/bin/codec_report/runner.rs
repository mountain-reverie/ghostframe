//! Scene runner. Implementation in Task 20.
//!
//! NOTE: `ghostframe-e2e` cannot be a direct dep of `ghostframe-bench` because
//! `ghostframe-e2e` → `ghostframe-test-pattern` → `ghostframe-bench` would
//! form a cycle. Task 20 resolves this by migrating ContentClass out of
//! `ghostframe-bench` into `ghostframe-lib` (or a thin shared crate) to break
//! the cycle, at which point this stub imports
//! `ghostframe_e2e::harness::SceneSpec` directly.

use std::time::Duration;
use crate::aggregate::ReportState;

/// Placeholder mirroring `ghostframe_e2e::harness::SceneSpec`.
/// Replaced by the real type in Task 20 once the dep-cycle is resolved.
#[derive(Clone, Debug)]
pub struct SceneSpec {
    pub name: String,
    pub test_pattern_args: Vec<String>,
    pub duration: Duration,
}

pub fn resolve_scenes(_names: &[String], _duration: Duration) -> Vec<SceneSpec> {
    Vec::new()
}

pub async fn run_one_scene(_spec: &SceneSpec, _state: &mut ReportState) -> anyhow::Result<()> {
    Ok(())
}
