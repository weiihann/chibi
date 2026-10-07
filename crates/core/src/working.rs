//! The keys chibi keeps proven at the anchor: pins plus keys requests read, bounded by a cap
//! with least-recently-used eviction. Only keys live here; values live in the store.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, U256};

use crate::state::Key;

/// Most keys kept, pins included.
pub const WORKING_SET_CAP: usize = 1024;

/// Pins plus learned keys.
///
/// Invariants: closed under `Slot(a, _) => Account(a)`; pins and learned keys are disjoint;
/// `len() <= max(cap, pins)`. Only [`crate::state::State`] mutates it.
#[derive(Clone, Debug)]
pub struct WorkingSet {
    cap: usize,
    /// Watched accounts and configured contract accounts and slots.
    configured: BTreeSet<Key>,
    /// Keys read by the last seeding of pinned tokens, minus configured ones.
    seeded: BTreeSet<Key>,
    /// Key to the tick of its last use. Ordered so an account's slots form one range.
    learned: BTreeMap<Key, u64>,
    /// Eviction order, oldest first. An account sorts before its slots at equal ticks.
    by_age: BTreeSet<(u64, Key)>,
    /// The next use's tick. Each `touch` is one use.
    tick: u64,
    membership: u64,
    recency: u64,
}

impl WorkingSet {
    /// A set holding `configured` pins and `remembered` keys, most recently used first.
    /// Remembered keys beyond capacity are dropped from the end.
    pub fn new(
        cap: usize,
        configured: impl IntoIterator<Item = Key>,
        remembered: Vec<Key>,
    ) -> Self {
        let mut set = Self {
            cap,
            configured: closed(configured),
            seeded: BTreeSet::new(),
            learned: BTreeMap::new(),
            by_age: BTreeSet::new(),
            tick: 0,
            membership: 0,
            recency: 0,
        };
        for key in remembered.into_iter().rev() {
            set.touch_one(key);
            set.tick += 1;
        }
        set.evict();
        set
    }

    /// Whether `key` is pinned or learned.
    pub fn contains(&self, key: Key) -> bool {
        self.is_pin(key) || self.learned.contains_key(&key)
    }

    /// Pinned keys: configured ones first, then seeded ones.
    pub fn pins(&self) -> impl Iterator<Item = Key> + '_ {
        self.configured.iter().chain(&self.seeded).copied()
    }

    /// Learned keys, most recently used first.
    pub fn learned(&self) -> impl Iterator<Item = Key> + '_ {
        self.by_age.iter().rev().map(|(_, key)| *key)
    }

    /// Pins plus learned keys.
    pub fn len(&self) -> usize {
        self.configured.len() + self.seeded.len() + self.learned.len()
    }

    /// Whether the set holds no key.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A count that changes whenever a key is learned or dropped.
    pub const fn membership(&self) -> u64 {
        self.membership
    }

    /// A count that changes whenever a use moves an already learned key ahead.
    pub const fn recency(&self) -> u64 {
        self.recency
    }

    /// The learned keys to persist, most recently used first: what [`WorkingSet::new`] takes
    /// back as `remembered`.
    pub fn remembered(&self) -> Vec<Key> {
        self.learned().collect()
    }

    /// Records one use of `keys`, and of the account of each slot. Pins are left alone.
    /// Returns the keys evicted to stay within the cap.
    pub(crate) fn touch(&mut self, keys: impl IntoIterator<Item = Key>) -> Vec<Key> {
        self.tick += 1;
        for key in keys {
            self.touch_one(key);
        }
        self.evict()
    }

    /// Replaces the seeded pins. Keys no longer seeded become learned as just used. Returns
    /// the keys evicted to stay within the cap.
    pub(crate) fn set_seeded(&mut self, keys: BTreeSet<Key>) -> Vec<Key> {
        let seeded: BTreeSet<Key> = closed(keys)
            .into_iter()
            .filter(|key| !self.configured.contains(key))
            .collect();
        if seeded == self.seeded {
            return Vec::new();
        }
        let dropped = std::mem::replace(&mut self.seeded, seeded);
        for key in &self.seeded {
            if let Some(tick) = self.learned.remove(key) {
                self.by_age.remove(&(tick, *key));
                self.membership += 1;
            }
        }
        self.touch(dropped)
    }

    fn is_pin(&self, key: Key) -> bool {
        self.configured.contains(&key) || self.seeded.contains(&key)
    }

    fn touch_one(&mut self, key: Key) {
        if let Key::Slot(address, _) = key {
            self.stamp(Key::Account(address));
        }
        self.stamp(key);
    }

    fn stamp(&mut self, key: Key) {
        if self.is_pin(key) {
            return;
        }
        let tick = self.tick;
        match self.learned.insert(key, tick) {
            None => self.membership += 1,
            Some(old) if old != tick => {
                self.by_age.remove(&(old, key));
                self.recency += 1;
            }
            Some(_) => {}
        }
        self.by_age.insert((tick, key));
    }

    /// Drops the least recently used keys until the set fits. Evicting an account evicts its
    /// learned slots, which closure keeps no fresher than the account.
    fn evict(&mut self) -> Vec<Key> {
        let mut evicted = Vec::new();
        while self.len() > self.cap {
            let Some((_, oldest)) = self.by_age.pop_first() else {
                break;
            };
            self.learned.remove(&oldest);
            self.membership += 1;
            evicted.push(oldest);
            if let Key::Account(address) = oldest {
                let slots: Vec<(Key, u64)> = self
                    .learned
                    .range(slot_range(address))
                    .map(|(key, tick)| (*key, *tick))
                    .collect();
                for (key, tick) in slots {
                    self.learned.remove(&key);
                    self.by_age.remove(&(tick, key));
                    evicted.push(key);
                }
            }
        }
        evicted
    }
}

fn closed(keys: impl IntoIterator<Item = Key>) -> BTreeSet<Key> {
    let mut set = BTreeSet::new();
    for key in keys {
        if let Key::Slot(address, _) = key {
            set.insert(Key::Account(address));
        }
        set.insert(key);
    }
    set
}

fn slot_range(address: Address) -> std::ops::RangeInclusive<Key> {
    Key::Slot(address, U256::ZERO)..=Key::Slot(address, U256::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const P: Address = address!("0x00000000000000000000000000000000000000aa");
    const A: Address = address!("0x00000000000000000000000000000000000000a1");
    const B: Address = address!("0x00000000000000000000000000000000000000b1");
    const C: Address = address!("0x00000000000000000000000000000000000000c1");
    const X: Address = address!("0x00000000000000000000000000000000000000e1");

    fn account(address: Address) -> Key {
        Key::Account(address)
    }

    #[test]
    fn the_least_recently_used_key_goes_first_and_pins_stay() {
        let mut set = WorkingSet::new(3, [account(P)], Vec::new());
        assert_eq!(set.touch([account(A)]), vec![]);
        assert_eq!(set.touch([account(B)]), vec![]);
        assert_eq!(set.touch([account(A)]), vec![]);
        assert_eq!(set.touch([account(C)]), vec![account(B)]);
        assert_eq!(
            set.learned().collect::<Vec<_>>(),
            vec![account(C), account(A)]
        );
        assert_eq!(set.pins().collect::<Vec<_>>(), vec![account(P)]);
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn evicting_an_account_evicts_its_slots() {
        let slot = Key::Slot(X, U256::from(1));
        let mut set = WorkingSet::new(3, [account(P)], Vec::new());
        assert_eq!(set.touch([slot]), vec![]);
        assert!(set.contains(account(X)));
        assert_eq!(set.touch([account(A), account(B)]), vec![account(X), slot]);
        assert_eq!(
            set.learned().collect::<BTreeSet<_>>(),
            BTreeSet::from([account(A), account(B)])
        );
    }

    #[test]
    fn remembered_keys_keep_their_order_and_fill_up_to_the_cap() {
        let slot = Key::Slot(X, U256::from(1));
        let set = WorkingSet::new(
            4,
            [account(P)],
            vec![
                account(A),
                slot,
                account(P),
                account(B),
                account(A),
                account(C),
            ],
        );
        assert_eq!(
            set.learned().collect::<Vec<_>>(),
            vec![account(A), slot, account(X)]
        );
    }

    #[test]
    fn membership_counts_learned_and_dropped_keys_and_recency_counts_reuse() {
        let mut set = WorkingSet::new(2, [account(P)], Vec::new());
        let start = (set.membership(), set.recency());
        set.touch([account(A)]);
        assert_eq!((set.membership(), set.recency()), (start.0 + 1, start.1));
        set.touch([account(A), account(P)]);
        assert_eq!(
            (set.membership(), set.recency()),
            (start.0 + 1, start.1 + 1)
        );
        set.touch([account(B)]);
        assert_eq!(
            (set.membership(), set.recency()),
            (start.0 + 3, start.1 + 1)
        );
        assert_eq!(set.remembered(), vec![account(B)]);
    }

    #[test]
    fn seeded_keys_are_pins_until_seeding_stops_reading_them() {
        let slot = Key::Slot(X, U256::from(1));
        let mut set = WorkingSet::new(8, [account(P)], Vec::new());
        set.touch([slot]);
        assert_eq!(set.set_seeded(BTreeSet::from([slot])), vec![]);
        assert_eq!(set.learned().count(), 0);
        assert_eq!(
            set.pins().collect::<Vec<_>>(),
            vec![account(P), account(X), slot]
        );
        set.touch([slot]);
        assert_eq!(set.learned().count(), 0);

        set.set_seeded(BTreeSet::new());
        assert_eq!(set.pins().collect::<Vec<_>>(), vec![account(P)]);
        assert_eq!(set.remembered(), vec![slot, account(X)]);
    }
}
