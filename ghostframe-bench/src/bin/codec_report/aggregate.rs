//! Per-scene aggregation. Implementation in Task 20.

use crate::cli::Cli;

pub struct ReportState {
    pub args: Cli,
}

impl ReportState {
    pub fn new(args: &Cli) -> Self {
        Self { args: args.clone() }
    }
}
