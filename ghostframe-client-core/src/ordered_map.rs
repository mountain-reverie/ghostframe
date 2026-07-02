//! Tiny insertion-ordered key/value store backed by a `Vec<(K, V)>`.
//!
//! Used in place of `std::collections::HashMap` wherever iteration order
//! must match JS `Map` semantics (insertion order, with `set` on an
//! existing key updating the value *in place* rather than moving it to the
//! end). These collections are always small (a handful of pending parity
//! groups, or a handful of fragment groups per tile), so linear scans are
//! fine and avoid pulling in an external `indexmap` dependency.

/// Insertion-ordered map. Mirrors the subset of JS `Map` behavior this
/// crate relies on: `set` on an existing key updates the value in place
/// (keeping its original position); `set` on a new key appends it.
#[derive(Debug, Clone)]
pub struct OrderedMap<K, V> {
    entries: Vec<(K, V)>,
}

impl<K: PartialEq, V> Default for OrderedMap<K, V> {
    fn default() -> Self {
        OrderedMap { entries: Vec::new() }
    }
}

impl<K: PartialEq, V> OrderedMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.entries.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Insert or update `key` -> `value`. If `key` already exists, its
    /// value is updated *in place* (position unchanged) — matching JS
    /// `Map.set` on an existing key. Otherwise the entry is appended.
    pub fn set(&mut self, key: K, value: V) {
        if let Some(existing) = self.get_mut(&key) {
            *existing = value;
        } else {
            self.entries.push((key, value));
        }
    }

    /// Remove `key`, preserving the relative order of the remaining
    /// entries.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let idx = self.entries.iter().position(|(k, _)| k == key)?;
        Some(self.entries.remove(idx).1)
    }

    /// Iterate entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &(K, V)> {
        self.entries.iter()
    }
}
