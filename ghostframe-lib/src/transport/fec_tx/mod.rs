// SPDX-License-Identifier: AGPL-3.0-or-later
//! Server-side FEC v2 transmit path: the packer (PLAIN frames, or SOURCE
//! frames grouped into Reed-Solomon blocks), the block table, and the
//! adaptive controller and sizer.

pub(crate) mod adapt;
pub(crate) mod adapt_size;
pub(crate) mod block_table;
pub(crate) mod blocks;
pub(crate) mod packer;
pub(crate) mod queue_loss;
