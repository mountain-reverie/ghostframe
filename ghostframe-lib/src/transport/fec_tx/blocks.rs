// SPDX-License-Identifier: AGPL-3.0-or-later
//! Pure block builder (R6-d D2): collects the coded symbols of the sources a
//! packer seals into one Reed-Solomon block and hands the block over closed.
//! No I/O, no clock of its own: every time is a caller-supplied microsecond.

use ghostframe_protocol::fec::frame::FEC_REPAIR_HEADER_LEN;
use ghostframe_protocol::fec::timing;

use crate::transport::admission::QUEUE_TARGET;

/// What a block advertises and closes on, fixed when it opens: a block that
/// outlives a knob change still closes on, and advertises, the span it began with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockSpan {
    /// The `T_adv` code every source of the block carries.
    pub t_adv: u8,
    /// The block closes this long after its first source was sealed (microseconds).
    pub t_us: u64,
}

impl BlockSpan {
    /// The span of a block opened under `t_ms`: it advertises the smallest
    /// table entry at least `t_ms` plus the queue allowance, so a source that
    /// waited a queue in the pacer still arrives inside the advertised span.
    pub(crate) fn for_t_ms(t_ms: u8) -> Self {
        let t_us = u64::from(t_ms) * 1_000;
        let queue_us = u64::try_from(QUEUE_TARGET.as_micros()).unwrap_or(u64::MAX);
        Self {
            t_adv: timing::t_adv_code_for(t_us.saturating_add(queue_us)),
            t_us,
        }
    }
}

/// What a source carries, for the deadline a block keeps for it (plan
/// decision 5: H.264 sources and tile sources have different release rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    Tile,
    H264,
}

/// A block that has stopped taking sources. Its repairs are encoded from
/// `symbols` (the sources' coded symbols, in `idx` order) by whoever closes
/// it over; this type does not encode.
#[derive(Debug, Clone)]
pub(crate) struct ClosedBlock {
    pub block_id: u16,
    /// Number of sources, `symbols.len()`.
    pub k: u8,
    pub symbols: Vec<Vec<u8>>,
    /// `bundle_seq` of each source, in `idx` order.
    #[cfg(test)]
    pub bundle_seqs: Vec<u32>,
    pub kinds: Vec<SourceKind>,
    /// When the block's first source was sealed (microseconds), the origin of
    /// the T close.
    #[cfg(test)]
    pub first_sealed_at_us: u64,
    /// The `T_adv` code its sources carried.
    pub t_adv: u8,
}

impl ClosedBlock {
    /// Length of the longest coded symbol (every repair payload is this long).
    pub(crate) fn symbol_len(&self) -> usize {
        self.symbols.iter().map(Vec::len).max().unwrap_or(0)
    }
}

#[derive(Debug)]
struct Open {
    block_id: u16,
    symbols: Vec<Vec<u8>>,
    #[cfg(test)]
    bundle_seqs: Vec<u32>,
    kinds: Vec<SourceKind>,
    first_sealed_at_us: u64,
    span: BlockSpan,
    /// Longest coded symbol so far, kept so the MTU test is O(1) per send.
    longest: usize,
}

#[derive(Debug, Default)]
pub(crate) struct BlockBuilder {
    /// The id the next new block takes; wraps at 65 535.
    next_id: u16,
    open: Option<Open>,
}

impl BlockBuilder {
    /// The `(block_id, idx)` the next source will take.
    pub(crate) fn peek(&self) -> (u16, u8) {
        match &self.open {
            Some(o) => (
                o.block_id,
                u8::try_from(o.symbols.len()).expect("a block never holds 256 sources"),
            ),
            None => (self.next_id, 0),
        }
    }

    /// The span the next source advertises: the open block's own, or `fresh`
    /// when the source will open a block.
    pub(crate) fn span_or(&self, fresh: BlockSpan) -> BlockSpan {
        self.open.as_ref().map_or(fresh, |o| o.span)
    }

    /// Adds the source whose `(block_id, idx)` `peek` returned. `span` is
    /// stored if this source opens the block and ignored otherwise.
    pub(crate) fn add(
        &mut self,
        symbol: Vec<u8>,
        _bundle_seq: u32,
        kind: SourceKind,
        now_us: u64,
        span: BlockSpan,
    ) {
        if self.open.is_none() {
            self.open = Some(Open {
                block_id: self.next_id,
                symbols: Vec::new(),
                #[cfg(test)]
                bundle_seqs: Vec::new(),
                kinds: Vec::new(),
                first_sealed_at_us: now_us,
                span,
                longest: 0,
            });
            self.next_id = self.next_id.wrapping_add(1);
        }
        let o = self.open.as_mut().expect("just opened");
        o.longest = o.longest.max(symbol.len());
        o.symbols.push(symbol);
        #[cfg(test)]
        o.bundle_seqs.push(_bundle_seq);
        o.kinds.push(kind);
    }

    /// Number of sources in the open block (0 when none is open).
    pub(crate) fn len(&self) -> usize {
        self.open.as_ref().map_or(0, |o| o.symbols.len())
    }

    /// The block's own T has elapsed since its first source was sealed.
    pub(crate) fn due(&self, now_us: u64) -> bool {
        self.open
            .as_ref()
            .is_some_and(|o| now_us >= o.first_sealed_at_us.saturating_add(o.span.t_us))
    }

    /// When the open block's own T elapses (microseconds).
    pub(crate) fn t_close_at_us(&self) -> Option<u64> {
        self.open
            .as_ref()
            .map(|o| o.first_sealed_at_us.saturating_add(o.span.t_us))
    }

    /// A repair of the open block's longest symbol no longer fits `max_dg`.
    pub(crate) fn outgrown_by(&self, max_dg: usize) -> bool {
        self.open
            .as_ref()
            .is_some_and(|o| o.longest + FEC_REPAIR_HEADER_LEN > max_dg)
    }

    /// Closes the open block, if it has sources.
    pub(crate) fn close(&mut self) -> Option<ClosedBlock> {
        let o = self.open.take()?;
        Some(ClosedBlock {
            block_id: o.block_id,
            k: u8::try_from(o.symbols.len()).expect("a block never holds 256 sources"),
            symbols: o.symbols,
            #[cfg(test)]
            bundle_seqs: o.bundle_seqs,
            kinds: o.kinds,
            #[cfg(test)]
            first_sealed_at_us: o.first_sealed_at_us,
            t_adv: o.span.t_adv,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_next_id_for_test(&mut self, id: u16) {
        self.next_id = id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_advertises_the_smallest_span_that_holds_t_plus_the_queue() {
        // The documented spans in ms by code, written out.
        let spans = [33u64, 10, 15, 20, 25, 40, 50, 66];
        let want = [
            (5u8, 15u64),
            (10, 20),
            (15, 25),
            (20, 33),
            (30, 40),
            (33, 50),
            (50, 66),
        ];
        for (t_ms, adv_ms) in want {
            let s = BlockSpan::for_t_ms(t_ms);
            assert_eq!(spans[usize::from(s.t_adv)], adv_ms, "t_ms {t_ms}");
            assert_eq!(s.t_us, u64::from(t_ms) * 1_000);
            // The inequality the old cross-crate test asserted, now by construction.
            let queue_ms = QUEUE_TARGET.as_millis() as u64;
            assert!(u64::from(t_ms) + queue_ms <= adv_ms, "t_ms {t_ms}");
        }
        // T = 20 ms is the one that advertises "nothing" (code 0): tests that
        // want a non-zero code must not use it.
        assert_eq!(BlockSpan::for_t_ms(20).t_adv, 0);
        for t_ms in [5u8, 10, 15, 30, 33, 50] {
            assert_ne!(BlockSpan::for_t_ms(t_ms).t_adv, 0, "t_ms {t_ms}");
        }
    }
}
