//! Bounded, sequence-aware history used for both application events and hardware-audit records.
//!
//! A full ring keeps accepting new items at constant capacity. Consumers follow it with a
//! *sequence cursor* (the last sequence they saw), never with a length: the length of a full ring
//! does not change, so a length comparison would silently stop delivering. When items were evicted
//! before a consumer saw them the delta reports the gap explicitly.

use serde::{Deserialize, Serialize};

/// Items carry the sequence assigned at emission.
pub trait HasSeq {
    fn seq(&self) -> u64;
}

/// Lifetime / retained / dropped accounting. Three different numbers, never conflated:
/// - `lifetime_total`: everything ever pushed (monotonic, survives eviction and clearing)
/// - `retained`: what is still held now
/// - `evicted`: pushed out by capacity; `cleared`: removed by an explicit clear
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RingStats {
    pub capacity: usize,
    pub lifetime_total: u64,
    pub retained: usize,
    pub evicted: u64,
    pub cleared: u64,
    pub first_retained_seq: Option<u64>,
    pub last_seq: u64,
}

/// New items since a cursor, plus how many were lost before they could be delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta<T> {
    pub items: Vec<T>,
    /// Items after the cursor that are no longer available (evicted or cleared).
    pub missed: u64,
    pub stats: RingStats,
}

#[derive(Debug, Clone)]
pub struct SeqRing<T: HasSeq> {
    capacity: usize,
    items: Vec<T>,
    lifetime_total: u64,
    evicted: u64,
    cleared: u64,
    last_seq: u64,
}

impl<T: HasSeq + Clone> SeqRing<T> {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            items: Vec::new(),
            lifetime_total: 0,
            evicted: 0,
            cleared: 0,
            last_seq: 0,
        }
    }

    /// Append an item. Sequences must be strictly increasing (they are assigned at emission).
    pub fn push(&mut self, item: T) {
        debug_assert!(item.seq() > self.last_seq, "sequence must increase");
        self.last_seq = item.seq();
        self.lifetime_total += 1;
        self.items.push(item);
        if self.items.len() > self.capacity {
            let excess = self.items.len() - self.capacity;
            self.items.drain(..excess);
            self.evicted += excess as u64;
        }
    }

    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.items
    }

    #[must_use]
    pub fn stats(&self) -> RingStats {
        RingStats {
            capacity: self.capacity,
            lifetime_total: self.lifetime_total,
            retained: self.items.len(),
            evicted: self.evicted,
            cleared: self.cleared,
            first_retained_seq: self.items.first().map(HasSeq::seq),
            last_seq: self.last_seq,
        }
    }

    /// Items with `seq > cursor`, and how many after the cursor are gone.
    #[must_use]
    pub fn since(&self, cursor: u64) -> Delta<T> {
        let items: Vec<T> = self
            .items
            .iter()
            .filter(|i| i.seq() > cursor)
            .cloned()
            .collect();
        // Sequences are consecutive for recorded items, so the number of items after the cursor
        // that should exist is `last_seq - cursor`; whatever was not delivered is missed.
        let expected = self.last_seq.saturating_sub(cursor);
        let missed = expected.saturating_sub(items.len() as u64);
        Delta {
            items,
            missed,
            stats: self.stats(),
        }
    }

    /// Drop everything currently retained. Lifetime totals are kept and the removal is counted
    /// as `cleared`, not as eviction.
    pub fn clear(&mut self) {
        self.cleared += self.items.len() as u64;
        self.items.clear();
    }

    /// The most recent `n` retained items (oldest first).
    #[must_use]
    pub fn tail(&self, n: usize) -> Vec<T> {
        let start = self.items.len().saturating_sub(n);
        self.items[start..].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Item(u64);
    impl HasSeq for Item {
        fn seq(&self) -> u64 {
            self.0
        }
    }

    #[test]
    fn full_ring_keeps_delivering_new_items_at_constant_capacity() {
        let mut ring = SeqRing::new(3);
        let mut cursor = 0;
        for seq in 1..=10_u64 {
            ring.push(Item(seq));
            assert!(ring.as_slice().len() <= 3, "capacity is a hard bound");
            let delta = ring.since(cursor);
            // A length-based consumer would see `3 == 3` from seq 3 onwards and stop; the
            // sequence cursor always receives exactly the new item.
            assert_eq!(delta.items, vec![Item(seq)], "seq {seq}");
            assert_eq!(delta.missed, 0);
            cursor = delta.items.last().map_or(cursor, HasSeq::seq);
        }
        let stats = ring.stats();
        assert_eq!(stats.lifetime_total, 10);
        assert_eq!(stats.retained, 3);
        assert_eq!(stats.evicted, 7);
        assert_eq!(stats.first_retained_seq, Some(8));
    }

    #[test]
    fn slow_consumer_sees_an_explicit_gap_after_eviction() {
        let mut ring = SeqRing::new(3);
        for seq in 1..=10_u64 {
            ring.push(Item(seq));
        }
        let delta = ring.since(2);
        assert_eq!(delta.items, vec![Item(8), Item(9), Item(10)]);
        assert_eq!(delta.missed, 5, "seq 3..=7 were evicted before delivery");

        let up_to_date = ring.since(10);
        assert!(up_to_date.items.is_empty());
        assert_eq!(up_to_date.missed, 0);
    }

    #[test]
    fn clearing_keeps_lifetime_counts_and_is_not_counted_as_eviction() {
        let mut ring = SeqRing::new(4);
        for seq in 1..=6_u64 {
            ring.push(Item(seq));
        }
        assert_eq!(ring.stats().evicted, 2);
        ring.clear();
        let stats = ring.stats();
        assert_eq!(stats.lifetime_total, 6, "lifetime survives clearing");
        assert_eq!(stats.retained, 0);
        assert_eq!(stats.evicted, 2);
        assert_eq!(stats.cleared, 4);

        ring.push(Item(7));
        let delta = ring.since(0);
        assert_eq!(delta.items, vec![Item(7)]);
        assert_eq!(delta.missed, 6, "a late reader is told 6 items are gone");
        assert_eq!(ring.stats().lifetime_total, 7);
    }

    #[test]
    fn tail_returns_the_newest_items_in_order() {
        let mut ring = SeqRing::new(5);
        for seq in 1..=5_u64 {
            ring.push(Item(seq));
        }
        assert_eq!(ring.tail(2), vec![Item(4), Item(5)]);
        assert_eq!(ring.tail(99).len(), 5);
    }
}
