//! An LRU cache keeping recently rendered previews resident, so
//! re-selecting a file or blinking back to it re-displays instantly instead of
//! re-reading, re-debayering, and re-stretching. Entries carry an explicit cost
//! (their pixel-buffer size); the cache evicts the least-recently-used entries
//! once the total cost exceeds its capacity.
//!
//! Deliberately dependency-free and small — the working set is a handful of
//! frames, so a linear `Vec` scan is cheaper than a hash map plus a linked
//! list, and it keeps the eviction order trivial to reason about and test.

/// Entries are ordered least-recently-used first, most-recently-used last.
pub struct LruCache<K, V> {
    /// Maximum total cost (bytes) of resident entries.
    capacity: usize,
    /// Sum of the resident entries' costs.
    total: usize,
    items: Vec<Entry<K, V>>,
}

struct Entry<K, V> {
    key: K,
    value: V,
    cost: usize,
}

impl<K: PartialEq, V> LruCache<K, V> {
    /// Create an empty cache holding entries totaling at most `capacity` bytes.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "LRU capacity must be positive");
        Self {
            capacity,
            total: 0,
            items: Vec::new(),
        }
    }

    /// Look up `key`, marking it most-recently-used on a hit.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let idx = self.items.iter().position(|e| &e.key == key)?;
        let entry = self.items.remove(idx);
        self.items.push(entry);
        self.items.last().map(|e| &e.value)
    }

    /// Insert or refresh `key` with the given `cost` (bytes), evicting the
    /// least-recently-used entries until the total cost is within capacity. A
    /// single entry larger than the whole budget is kept resident on its own
    /// rather than evicting itself. The entry becomes most-recently-used.
    pub fn put(&mut self, key: K, value: V, cost: usize) {
        if let Some(idx) = self.items.iter().position(|e| e.key == key) {
            self.total -= self.items[idx].cost;
            self.items.remove(idx);
        }
        self.total += cost;
        self.items.push(Entry { key, value, cost });
        while self.total > self.capacity && self.items.len() > 1 {
            let evicted = self.items.remove(0);
            self.total -= evicted.cost;
        }
    }

    /// Drop every entry (e.g. when a setting invalidates all rendered previews).
    pub fn clear(&mut self) {
        self.items.clear();
        self.total = 0;
    }

    /// Sum of the resident entries' costs, in bytes (drives the status-bar
    /// memory readout).
    pub fn total_bytes(&self) -> usize {
        self.total
    }

    /// The configured capacity (maximum resident bytes).
    pub fn capacity(&self) -> usize {
        self.capacity
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
        let mut cache = LruCache::new(100);
        cache.put("a", 1, 10);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"missing"), None);
        assert_eq!(cache.total_bytes(), 10);
    }

    #[test]
    fn evicts_least_recently_used_by_cost() {
        let mut cache = LruCache::new(25);
        cache.put("a", 1, 10);
        cache.put("b", 2, 10);
        cache.put("c", 3, 10); // total would be 30 > 25 → evict "a", the LRU
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.get(&"b"), Some(&2));
        assert_eq!(cache.get(&"c"), Some(&3));
        assert_eq!(cache.total_bytes(), 20);
    }

    #[test]
    fn get_refreshes_recency() {
        let mut cache = LruCache::new(25);
        cache.put("a", 1, 10);
        cache.put("b", 2, 10);
        assert_eq!(cache.get(&"a"), Some(&1)); // "a" now most-recent
        cache.put("c", 3, 10); // evicts "b", the LRU
        assert_eq!(cache.get(&"b"), None);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"c"), Some(&3));
    }

    #[test]
    fn put_refreshes_existing_key_without_double_counting() {
        let mut cache = LruCache::new(100);
        cache.put("a", 1, 10);
        cache.put("a", 10, 30); // refresh value + cost, not a new slot
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.total_bytes(), 30);
        assert_eq!(cache.get(&"a"), Some(&10));
    }

    #[test]
    fn oversized_entry_is_kept_alone() {
        let mut cache = LruCache::new(25);
        cache.put("a", 1, 10);
        cache.put("big", 2, 1000); // exceeds capacity but must not evict itself
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.get(&"big"), Some(&2));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn clear_empties_the_cache() {
        let mut cache = LruCache::new(100);
        cache.put("a", 1, 10);
        cache.put("b", 2, 10);
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.total_bytes(), 0);
        assert_eq!(cache.get(&"a"), None);
    }
}
