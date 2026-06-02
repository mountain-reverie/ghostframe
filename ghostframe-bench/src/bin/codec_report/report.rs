//! Markdown report writer. Implementation in Task 21.

use std::path::Path;
use crate::aggregate::ReportState;

pub fn load_criterion(_path: &Path) -> anyhow::Result<serde_json::Value> {
    Err(anyhow::anyhow!("not yet implemented"))
}

pub fn write(path: &Path, _state: &ReportState, _criterion: Option<&serde_json::Value>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, "# M3.5 Codec Bench Results — STUB (Task 19 scaffolding)\n")?;
    Ok(())
}
