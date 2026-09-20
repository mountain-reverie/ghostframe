//! ## Catch-all match arms
//!
//! `clippy::wildcard_enum_match_arm` is on for non-test code in this crate.
//! A `_ =>` arm that silently absorbs unknown variants has twice hidden real
//! defects here: a wrong classifier that stood for three months, and inbound
//! routing that fed every non-NACK datagram to the ACK parser. Listing the
//! variants makes adding one a compile error at every site that must decide
//! about it.
//!
//! Wildcards that fail *loudly* (`other => panic!(...)` in a test) are fine,
//! which is why the lint is scoped to `not(test)`. A wildcard that is
//! genuinely required -- a foreign enum we do not control -- takes a local
//! `#[allow]` with a reason, so every exception is a decision on the record.
#![cfg_attr(not(test), warn(clippy::wildcard_enum_match_arm))]

pub mod ack;
pub mod codec;
pub mod fec;
pub mod feedback;
pub mod protocol;
pub mod tile;
