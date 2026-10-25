//! The loss path of D1 F4 part 2, as a function of the two things it joins:
//! the transmission ledger (what is outstanding, in departure order) and the
//! reliable emitter (what content is still current, and the probe timer).
//!
//! Kept apart from `IoBridge` so the scripted-path tests drive exactly the code
//! production runs.

use crate::transport::reliable_emitter::{ReliableTileEmitter, RetxCause, RetxRequest};
use crate::transport::rtt_estimator::RttEstimator;
use crate::transport::transmission_ledger::{Transmission, TransmissionLedger};
use std::time::{Duration, Instant};

/// Transmissions a probe sends.
pub(crate) const PROBE_COUNT: usize = 2;
/// Records looked at from the back of the departure order when probing.
pub(crate) const PROBE_SCAN: usize = 32;

/// RFC 9002 section 6.1.2: a transmission that left more than
/// `9/8 x max(srtt, latest_rtt)` before the newest acknowledged departure is
/// lost. The estimator is the emitter's own (departure-based, application
/// acknowledgements), not quinn's: the acknowledgement being waited for is the
/// application's.
pub(crate) fn time_threshold(rtt: &RttEstimator) -> Duration {
    rtt.srtt().max(rtt.latest_rtt()) * 9 / 8
}

/// What a pass of the loss path decided.
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    /// Transmissions declared lost, to be reported to the estimators.
    pub lost: Vec<(u32, Transmission)>,
    /// Retransmissions queued on the emitter.
    pub retx_queued: u64,
}

/// Declare lost what the acknowledgements so far put out of contention (packet
/// and time threshold), and queue one retransmission for each whose content is
/// still current.
pub(crate) fn detect(
    ledger: &mut TransmissionLedger,
    emitter: &mut ReliableTileEmitter,
) -> Outcome {
    let thr = time_threshold(&emitter.rtt);
    let mut out = Outcome::default();
    for (ws, tx, _reason) in ledger.detect_losses(thr) {
        if emitter.request_retransmit(tx.key, ws, RetxCause::Loss) == RetxRequest::Queued {
            out.retx_queued += 1;
        }
        out.lost.push((ws, tx));
    }
    out
}

/// The probe timeout as the tail probe. One timer per connection, not per
/// pass: when it expires, the newest one or two outstanding transmissions are
/// sent again, never the whole set. That draws an acknowledgement, and the
/// acknowledgement lets threshold loss find whatever else was lost.
///
/// A transmission whose content is gone (acknowledged through another
/// transmission, or superseded) has nothing to send and is declared lost on the
/// spot, so a dead tail cannot occupy the probe forever. One whose pass has a
/// newer transmission in flight is skipped: that one is what the next
/// acknowledgement is waiting for.
///
/// The timer restarts at every expiry even when nothing was probed, so a timer
/// with no candidates does not spin. Returns `None` while it is not due.
pub(crate) fn probe(
    ledger: &mut TransmissionLedger,
    emitter: &mut ReliableTileEmitter,
    now: Instant,
) -> Option<Outcome> {
    if !emitter.pto_due(now) {
        return None;
    }
    emitter.note_pto_fire(now);
    let mut out = Outcome::default();
    for (ws, tx) in ledger.newest_outstanding(PROBE_SCAN, PROBE_SCAN) {
        if out.retx_queued as usize >= PROBE_COUNT {
            break;
        }
        match emitter.request_retransmit(tx.key, ws, RetxCause::Probe) {
            RetxRequest::Queued => out.retx_queued += 1,
            RetxRequest::ContentGone => {
                if let Some(tx) = ledger.declare_lost_by_pto(ws) {
                    out.lost.push((ws, tx));
                }
            }
            RetxRequest::NewerInFlight | RetxRequest::AlreadyQueued | RetxRequest::Disabled => {}
        }
    }
    Some(out)
}
