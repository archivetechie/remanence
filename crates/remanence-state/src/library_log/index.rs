//! Hash indexes with deterministic probe accounting for replay cost regressions.
//! Counts keyed reads/updates, not hash collisions or allocator work. Iteration is
//! intentionally unavailable so history-sized scans cannot hide behind the metric.
use std::{borrow::Borrow, cell::Cell, collections::HashMap, hash::Hash};

#[derive(Clone, Debug)]
pub(super) struct Index<K, V> {
    entries: HashMap<K, V>,
    probes: Cell<u64>,
}
impl<K, V> Default for Index<K, V> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            probes: Cell::new(0),
        }
    }
}
impl<K: Eq + Hash, V> Index<K, V> {
    fn probe(&self) {
        self.probes.set(self.probes.get() + 1);
    }
    pub fn probes(&self) -> u64 {
        self.probes.get()
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn get<Q: ?Sized + Eq + Hash>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        self.probe();
        self.entries.get(key)
    }
    pub fn get_mut<Q: ?Sized + Eq + Hash>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
    {
        self.probe();
        self.entries.get_mut(key)
    }
    pub fn contains_key<Q: ?Sized + Eq + Hash>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.probe();
        self.entries.contains_key(key)
    }
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.probe();
        self.entries.insert(key, value)
    }
    pub fn remove<Q: ?Sized + Eq + Hash>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        self.probe();
        let removed = self.entries.remove(key);
        // Bound allocated buckets by live resources after a burst of concurrent work.
        // Geometric shrinking keeps total resize work amortized linear.
        if self.entries.capacity() > 16 && self.entries.len() < self.entries.capacity() / 4 {
            self.entries
                .shrink_to(self.entries.len().saturating_mul(2).max(16));
        }
        removed
    }
    pub fn entry(&mut self, key: K) -> std::collections::hash_map::Entry<'_, K, V> {
        self.probe();
        self.entries.entry(key)
    }
}

#[cfg(test)]
mod tests {
    use super::Index;
    /// A finished burst must release buckets as well as decoded records.
    #[test]
    fn closed_resources_release_index_capacity() {
        let mut index = Index::default();
        for key in 0..100_000 {
            index.insert(key, key);
        }
        assert!(index.entries.capacity() >= 100_000);
        for key in 0..100_000 {
            assert_eq!(index.remove(&key), Some(key));
        }
        assert_eq!(index.len(), 0);
        assert!(index.entries.capacity() <= 28);
        assert_eq!(index.probes(), 200_000);
    }
}
