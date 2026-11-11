//! FEC v2 revision 6: one Reed-Solomon erasure code over every
//! server -> client datagram (docs/specs/fec-v2/fec-v2-rev6.md).
//!
//! Every datagram the server sends is a frame (`frame`): a SOURCE bundle that
//! belongs to a block, a REPAIR symbol of a block, or a PLAIN bundle outside
//! any block. Repair symbols are GF(2^8) Cauchy combinations of the block's
//! sources (`coef`, `codec`). The client reports arrivals, loss and content
//! release with the messages in `feedback` and `repair_status`.

pub mod codec;
pub mod coef;
#[cfg(test)]
pub(crate) mod oracle;
