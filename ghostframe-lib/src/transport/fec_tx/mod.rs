//! Server-side FEC v2 transmit path. Under rev 6 step R6-c every bundle is a
//! PLAIN frame; R6-d adds the block mode of the packer, wired in by D3/D4.

pub(crate) mod block_table;
// Nothing outside the unit tests builds blocks until R6-d D4 wires them in.
#[allow(dead_code)]
pub(crate) mod blocks;
#[allow(dead_code)]
pub(crate) mod packer;
// Pure estimator; the bridge wiring is R6-e E5.
#[allow(dead_code)]
pub(crate) mod adapt;
