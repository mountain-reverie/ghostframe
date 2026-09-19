//! A versioned slab: stable handles into a `Vec`, with reuse detection.
//!
//! Handles must be versioned because the transmission ledger keeps
//! `wire_seq -> Handle` tombstones that outlive the entries they name. An
//! unversioned index would let a late acknowledgement for a long-gone
//! transmission resolve to whatever work now occupies that slot, and
//! silently acknowledge the wrong tile.

/// A stable reference to a slab entry. Copy, 8 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle {
    pub index: u32,
    pub version: u32,
}

#[derive(Debug)]
struct Entry<T> {
    /// Even = vacant, odd = occupied. Incrementing on both insert and
    /// remove means a handle taken before a remove can never match after.
    version: u32,
    value: Option<T>,
}

#[derive(Debug)]
pub struct Slab<T> {
    entries: Vec<Entry<T>>,
    free: Vec<u32>,
    live: usize,
}

impl<T> Default for Slab<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Slab<T> {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            free: Vec::new(),
            live: 0,
        }
    }

    pub fn insert(&mut self, value: T) -> Handle {
        self.live += 1;
        if let Some(index) = self.free.pop() {
            let e = &mut self.entries[index as usize];
            e.version = e.version.wrapping_add(1);
            e.value = Some(value);
            return Handle {
                index,
                version: e.version,
            };
        }
        let index = self.entries.len() as u32;
        self.entries.push(Entry {
            version: 1,
            value: Some(value),
        });
        Handle { index, version: 1 }
    }

    fn entry(&self, h: Handle) -> Option<&Entry<T>> {
        let e = self.entries.get(h.index as usize)?;
        (e.version == h.version).then_some(e)
    }

    pub fn get(&self, h: Handle) -> Option<&T> {
        self.entry(h)?.value.as_ref()
    }

    pub fn get_mut(&mut self, h: Handle) -> Option<&mut T> {
        let e = self.entries.get_mut(h.index as usize)?;
        if e.version != h.version {
            return None;
        }
        e.value.as_mut()
    }

    pub fn remove(&mut self, h: Handle) -> Option<T> {
        let e = self.entries.get_mut(h.index as usize)?;
        if e.version != h.version {
            return None;
        }
        let value = e.value.take()?;
        e.version = e.version.wrapping_add(1);
        self.free.push(h.index);
        self.live -= 1;
        Some(value)
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn clear(&mut self) {
        for (i, e) in self.entries.iter_mut().enumerate() {
            if e.value.take().is_some() {
                e.version = e.version.wrapping_add(1);
                self.free.push(i as u32);
            }
        }
        self.live = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_then_get_returns_the_value() {
        let mut slab: Slab<u32> = Slab::new();
        let h = slab.insert(42);
        assert_eq!(slab.get(h), Some(&42));
    }

    #[test]
    fn remove_frees_the_entry() {
        let mut slab: Slab<u32> = Slab::new();
        let h = slab.insert(42);
        assert_eq!(slab.remove(h), Some(42));
        assert_eq!(slab.get(h), None, "a removed handle must not resolve");
        assert_eq!(slab.remove(h), None, "double remove must be a no-op");
    }

    /// The ABA hazard: a stale handle must never resolve to whatever now
    /// occupies its index. This is the property the transmission ledger
    /// depends on once it keeps tombstones past entry lifetime.
    #[test]
    fn a_stale_handle_never_resolves_to_the_slots_new_occupant() {
        let mut slab: Slab<u32> = Slab::new();
        let old = slab.insert(1);
        slab.remove(old);
        let new = slab.insert(2);
        assert_eq!(
            new.index, old.index,
            "test is vacuous unless the slot is reused"
        );
        assert_ne!(new.version, old.version, "reuse must bump the version");
        assert_eq!(
            slab.get(old),
            None,
            "stale handle resolved to the new occupant"
        );
        assert_eq!(slab.get(new), Some(&2));
    }

    #[test]
    fn len_counts_live_entries_only() {
        let mut slab: Slab<u32> = Slab::new();
        assert_eq!(slab.len(), 0);
        let a = slab.insert(1);
        let _b = slab.insert(2);
        assert_eq!(slab.len(), 2);
        slab.remove(a);
        assert_eq!(slab.len(), 1);
    }

    #[test]
    fn clear_drops_everything_and_invalidates_handles() {
        let mut slab: Slab<u32> = Slab::new();
        let h = slab.insert(7);
        slab.clear();
        assert_eq!(slab.len(), 0);
        assert_eq!(slab.get(h), None);
    }
}
