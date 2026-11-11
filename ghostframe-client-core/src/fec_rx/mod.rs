//! Client side of the rev 6 framing (sans-IO).
//!
//! `blocks` unbundles SOURCE / REPAIR / PLAIN frames, keeps the coded symbols
//! of each block, rebuilds lost sources with the Reed-Solomon codec and decides
//! what to do when a block's recovery window ends.

mod blocks;
pub use blocks::*;
