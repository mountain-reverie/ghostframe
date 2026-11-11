//! Pure block builder (R6-d D2): collects the coded symbols of the sources a
//! packer seals into one Reed-Solomon block and hands the block over closed.
//! No I/O, no clock of its own: every time is a caller-supplied microsecond.

use ghostframe_protocol::fec::frame::FEC_REPAIR_HEADER_LEN;

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
    pub bundle_seqs: Vec<u32>,
    pub kinds: Vec<SourceKind>,
    /// When the block's first source was sealed (microseconds), the origin of
    /// the T close.
    pub first_sealed_at_us: u64,
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
    bundle_seqs: Vec<u32>,
    kinds: Vec<SourceKind>,
    first_sealed_at_us: u64,
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

    /// Adds the source whose `(block_id, idx)` `peek` returned.
    pub(crate) fn add(&mut self, symbol: Vec<u8>, bundle_seq: u32, kind: SourceKind, now_us: u64) {
        if self.open.is_none() {
            self.open = Some(Open {
                block_id: self.next_id,
                symbols: Vec::new(),
                bundle_seqs: Vec::new(),
                kinds: Vec::new(),
                first_sealed_at_us: now_us,
                longest: 0,
            });
            self.next_id = self.next_id.wrapping_add(1);
        }
        let o = self.open.as_mut().expect("just opened");
        o.longest = o.longest.max(symbol.len());
        o.symbols.push(symbol);
        o.bundle_seqs.push(bundle_seq);
        o.kinds.push(kind);
    }

    /// Number of sources in the open block (0 when none is open).
    pub(crate) fn len(&self) -> usize {
        self.open.as_ref().map_or(0, |o| o.symbols.len())
    }

    /// T has elapsed since the first source was sealed.
    pub(crate) fn due(&self, now_us: u64, t_us: u64) -> bool {
        self.open
            .as_ref()
            .is_some_and(|o| now_us >= o.first_sealed_at_us.saturating_add(t_us))
    }

    /// A repair of the open block's longest symbol no longer fits `max_dg`.
    pub(crate) fn outgrown_by(&self, max_dg: usize) -> bool {
        self.open
            .as_ref()
            .is_some_and(|o| o.longest + FEC_REPAIR_HEADER_LEN > max_dg)
    }

    /// When the open block's first source was sealed (microseconds).
    pub(crate) fn first_sealed_at_us(&self) -> Option<u64> {
        self.open.as_ref().map(|o| o.first_sealed_at_us)
    }

    /// Closes the open block, if it has sources.
    pub(crate) fn close(&mut self) -> Option<ClosedBlock> {
        let o = self.open.take()?;
        Some(ClosedBlock {
            block_id: o.block_id,
            k: u8::try_from(o.symbols.len()).expect("a block never holds 256 sources"),
            symbols: o.symbols,
            bundle_seqs: o.bundle_seqs,
            kinds: o.kinds,
            first_sealed_at_us: o.first_sealed_at_us,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_next_id_for_test(&mut self, id: u16) {
        self.next_id = id;
    }

    /// Forgets the open block and restarts the id counter.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}
