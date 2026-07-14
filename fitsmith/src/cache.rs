//! A tiny fixed-capacity LRU cache keeping recently viewed FITS documents
//! resident, so re-selecting a file or blinking through the list re-renders
//! from memory instead of re-reading (and re-decompressing) from disk.
//!
//! Deliberately dependency-free and small — the working set is a handful of
//! frames, so a linear `Vec` scan is cheaper than a hash map plus a linked
//! list, and it keeps the eviction order trivial to reason about and test.

/// Entries are ordered least-recently-used first, most-recently-used last.
pub struct LruCache<K, V> {
    capacity: usize,
    items: Vec<(K, V)>,
}

impl<K: PartialEq, V> LruCache<K, V> {
    /// Create an empty cache holding at most `capacity` entries.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "LRU capacity must be positive");
        Self {
            capacity,
            items: Vec::new(),
        }
    }

    /// Look up `key`, marking it most-recently-used on a hit.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let idx = self.items.iter().position(|(k, _)| k == key)?;
        let entry = self.items.remove(idx);
        self.items.push(entry);
        self.items.last().map(|(_, v)| v)
    }

    /// Insert or refresh `key`, evicting the least-recently-used entry when the
    /// cache would exceed its capacity. The entry becomes most-recently-used.
    pub fn put(&mut self, key: K, value: V) {
        if let Some(idx) = self.items.iter().position(|(k, _)| *k == key) {
            self.items.remove(idx);
        }
        self.items.push((key, value));
        if self.items.len() > self.capacity {
            self.items.remove(0);
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_stored_values() {
        let mut cache = LruCache::new(2);
        cache.put("a", 1);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"missing"), None);
    }

    #[test]
    fn evicts_least_recently_used() {
        let mut cache = LruCache::new(2);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.put("c", 3); // evicts "a", the LRU
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.get(&"b"), Some(&2));
        assert_eq!(cache.get(&"c"), Some(&3));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn get_refreshes_recency() {
        let mut cache = LruCache::new(2);
        cache.put("a", 1);
        cache.put("b", 2);
        assert_eq!(cache.get(&"a"), Some(&1)); // "a" now most-recent
        cache.put("c", 3); // evicts "b", the LRU
        assert_eq!(cache.get(&"b"), None);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"c"), Some(&3));
    }

    #[test]
    fn put_refreshes_existing_key_without_growing() {
        let mut cache = LruCache::new(2);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.put("a", 10); // refresh value + recency, not a new slot
        assert_eq!(cache.len(), 2);
        cache.put("c", 3); // evicts "b"
        assert_eq!(cache.get(&"b"), None);
        assert_eq!(cache.get(&"a"), Some(&10));
    }
}
