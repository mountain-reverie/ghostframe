//! Client side of the rev 6 framing (sans-IO).
//!
//! `blocks` unbundles SOURCE / REPAIR / PLAIN frames, keeps the coded symbols
//! of each block, rebuilds lost sources with the Reed-Solomon codec and decides
//! what to do when a block's recovery window ends.

mod blocks;
pub use blocks::*;

mod ack_v4;
mod loss_report;
pub use ack_v4::AckV4Batcher;
pub use loss_report::{LossReporter, LOSS_REPORT_COUNT_TRIGGER, LOSS_REPORT_INTERVAL_US};
