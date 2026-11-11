//! Server-side FEC v2 transmit path. Under rev 6 step R6-c every bundle is a
//! PLAIN frame; blocks arrive in R6-d.

// Wired in C5.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod packer;
