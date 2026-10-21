//! Account history: verified chain facts about watched accounts, stored as they came off the
//! chain and decoded into activity only when read.
//!
//! History is display-only. Its rows never answer an RPC request and never enter a `Store`.

use alloy_primitives::{Address, B256, Bytes, Log, TxHash, U256};

/// Where a fact sits in its block, from the block access list's block access index.
///
/// The index is 0 for system calls before the first transaction, n + 1 for transaction n,
/// and the transaction count + 1 for the post-block changes (withdrawals and the like).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum At {
    /// Before the first transaction: index 0.
    System,
    /// Inside transaction n: index n + 1.
    Tx(u32),
    /// After the last transaction: index tx_count + 1.
    Post,
}

impl At {
    /// The position of block access `index` in a block of `tx_count` transactions, or `None`
    /// when the index lies past the post-block index.
    pub fn from_index(index: u64, tx_count: u32) -> Option<At> {
        let post = u64::from(tx_count) + 1;
        match index {
            0 => Some(At::System),
            i if i == post => Some(At::Post),
            i if i < post => u32::try_from(i - 1).ok().map(At::Tx),
            _ => None,
        }
    }

    /// The block access index of this position in a block of `tx_count` transactions.
    pub fn index(self, tx_count: u32) -> u64 {
        match self {
            At::System => 0,
            At::Tx(n) => u64::from(n) + 1,
            At::Post => u64::from(tx_count) + 1,
        }
    }
}

/// What a coverage range promises is stored for an account. Readers ignore codes they do not
/// know, so a later version can add capabilities without breaking older readers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    /// Every block access list change of the account in the range is stored as a [`Touch`] or
    /// a [`SlotChange`].
    Bal,
    /// Every log naming the account as emitter or in topics 1-3 is stored with its transaction,
    /// and the account's own transactions are stored whole.
    Logs,
}

impl Capability {
    /// The code stored for this capability.
    pub fn code(self) -> u32 {
        match self {
            Capability::Bal => 1,
            Capability::Logs => 2,
        }
    }

    /// The capability stored as `code`, or `None` for a code this version does not know.
    pub fn from_code(code: u32) -> Option<Capability> {
        match code {
            1 => Some(Capability::Bal),
            2 => Some(Capability::Logs),
            _ => None,
        }
    }
}

/// A set of block numbers held as sorted, disjoint, non-adjacent inclusive ranges.
///
/// Adjacent ranges merge, so the set has exactly one representation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ranges(Vec<(u64, u64)>);

impl Ranges {
    /// Adds every block from `first` to `last` inclusive. An empty range (`first > last`) adds
    /// nothing.
    pub fn insert(&mut self, first: u64, last: u64) {
        if first > last {
            return;
        }
        // Saturating: a range ending at u64::MAX has nothing above it to be adjacent to.
        let start = self
            .0
            .partition_point(|&(_, l)| l.saturating_add(1) < first);
        let end = self
            .0
            .partition_point(|&(f, _)| f <= last.saturating_add(1));
        let merged = match self.0[start..end] {
            [(f, l)] | [(f, _), .., (_, l)] => (first.min(f), last.max(l)),
            [] => (first, last),
        };
        self.0.splice(start..end, std::iter::once(merged));
    }

    /// Whether block `n` is in the set.
    pub fn contains(&self, n: u64) -> bool {
        let i = self.0.partition_point(|&(_, l)| l < n);
        self.0.get(i).is_some_and(|&(f, _)| f <= n)
    }

    /// Drops every block above `n`.
    pub fn truncate_above(&mut self, n: u64) {
        self.0.retain(|&(f, _)| f <= n);
        if let Some((_, l)) = self.0.last_mut() {
            *l = (*l).min(n);
        }
    }

    /// The inclusive stretches of `lo..=hi` that are not in the set, in order.
    pub fn gaps(&self, lo: u64, hi: u64) -> Vec<(u64, u64)> {
        let mut gaps = Vec::new();
        if lo > hi {
            return gaps;
        }
        let mut next = lo;
        let start = self.0.partition_point(|&(_, l)| l < lo);
        for &(f, l) in &self.0[start..] {
            if f > hi {
                break;
            }
            if f > next {
                gaps.push((next, f - 1));
            }
            if l >= hi {
                return gaps;
            }
            next = l + 1;
        }
        gaps.push((next, hi));
        gaps
    }

    /// The lowest block in the set.
    pub fn first(&self) -> Option<u64> {
        self.0.first().map(|&(f, _)| f)
    }

    /// The highest block in the set.
    pub fn last(&self) -> Option<u64> {
        self.0.last().map(|&(_, l)| l)
    }

    /// The ranges as `(first, last)` pairs, lowest first.
    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.0.iter().copied()
    }
}

impl FromIterator<(u64, u64)> for Ranges {
    /// Builds the set from inclusive `(first, last)` ranges in any order, merging overlapping
    /// and adjacent ones and skipping empty ones.
    fn from_iter<I: IntoIterator<Item = (u64, u64)>>(iter: I) -> Self {
        let mut rows: Vec<(u64, u64)> = iter.into_iter().filter(|&(f, l)| f <= l).collect();
        rows.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(rows.len());
        for (f, l) in rows {
            match merged.last_mut() {
                Some((_, last)) if f <= last.saturating_add(1) => *last = (*last).max(l),
                _ => merged.push((f, l)),
            }
        }
        Ranges(merged)
    }
}

/// A block's own facts: the header fields history shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockRow {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: B256,
    /// Block timestamp in seconds.
    pub timestamp: u64,
    /// Fee recipient.
    pub beneficiary: Address,
    /// Base fee per gas, absent before London.
    pub base_fee: Option<u64>,
    /// Excess blob gas, absent before Cancun.
    pub excess_blob_gas: Option<u64>,
    /// Number of transactions in the block.
    pub tx_count: u32,
}

/// A transaction relevant to a watched account, with its receipt's outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxRow {
    /// Position of the transaction in its block.
    pub index: u32,
    /// Transaction hash.
    pub hash: TxHash,
    /// Recovered sender.
    pub sender: Address,
    /// The encoded transaction, kept only for the account's own transactions.
    pub raw: Option<Bytes>,
    /// Whether the receipt reports success.
    pub success: bool,
    /// Cumulative gas used in the block up to and including this transaction.
    pub cumulative_gas: u64,
    /// Gas this transaction used.
    pub gas_used: u64,
    /// Block-wide index of the transaction's first log.
    pub first_log_index: u64,
    /// The receipt's true log count, whether or not every log is stored.
    pub log_count: u64,
}

/// A stored log with its block-wide index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRow {
    /// Block-wide log index.
    pub index: u64,
    /// The log.
    pub log: Log,
}

/// An account's post-values at one block access index, from the block access list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Touch {
    /// The touched account.
    pub account: Address,
    /// Where in the block the change happened.
    pub at: At,
    /// Balance after the change, when it changed.
    pub balance: Option<U256>,
    /// Nonce after the change, when it changed.
    pub nonce: Option<u64>,
    /// Code after the change, when it changed.
    pub code: Option<Bytes>,
}

/// A storage change relevant to an account: its own storage or a token balance slot of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotChange {
    /// The watched account the change concerns.
    pub account: Address,
    /// Where in the block the change happened.
    pub at: At,
    /// The contract whose storage changed.
    pub contract: Address,
    /// The storage slot.
    pub slot: U256,
    /// The slot's value after the change.
    pub value: U256,
}

/// A consensus-layer withdrawal from a verified block body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WithdrawalRow {
    /// Withdrawal index.
    pub index: u64,
    /// Validator index.
    pub validator: u64,
    /// Recipient.
    pub address: Address,
    /// Amount in gwei.
    pub amount_gwei: u64,
}

/// Everything stored about one block: written in this shape and read back in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockFacts {
    /// The block's header fields.
    pub block: BlockRow,
    /// Relevant transactions.
    pub txs: Vec<TxRow>,
    /// Relevant logs.
    pub logs: Vec<LogRow>,
    /// Account post-values from the block access list.
    pub touches: Vec<Touch>,
    /// Relevant storage changes.
    pub slots: Vec<SlotChange>,
    /// Withdrawals to watched accounts.
    pub withdrawals: Vec<WithdrawalRow>,
}

/// One block scanned for a set of accounts: the coverage it grants and what it found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedBlock {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: B256,
    /// Parent block hash, to check that the block extends the stored tip.
    pub parent_hash: B256,
    /// The accounts the block was scanned for, which it adds to coverage.
    pub accounts: Vec<Address>,
    /// What the scan covered for those accounts.
    pub capabilities: Vec<Capability>,
    /// The facts found, `None` when nothing in the block concerned the accounts.
    pub facts: Option<BlockFacts>,
}

/// An account's proven state at the block before its forward coverage opens, so history can
/// show balances from where coverage starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenState {
    /// The account.
    pub account: Address,
    /// The block the state was proven at.
    pub number: u64,
    /// That block's hash.
    pub hash: B256,
    /// Balance at that block.
    pub balance: U256,
    /// Nonce at that block.
    pub nonce: u64,
    /// Pinned storage at that block as `(contract, slot, value)`.
    pub slots: Vec<(Address, U256, U256)>,
}

/// One page of an account's history: whole blocks, newest first, holding only what concerns
/// the account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    /// The account the page is about.
    pub account: Address,
    /// The account's blocks, newest first. Each holds the account's touches, slots and
    /// withdrawals, the transactions at its touched transaction indices, and the stored logs
    /// of those transactions, including logs stored for another account.
    pub blocks: Vec<BlockFacts>,
    /// The newest known balance strictly below the page's lowest block, as
    /// `(block number, balance)`, so the page's balance changes have a starting point: the
    /// newer of the newest balance-bearing touch and the newest proven open state below it,
    /// the touch when both are at one block.
    pub opening_balance: Option<(u64, U256)>,
    /// The cursor for the next older page: the lowest block on this page, when the account has
    /// blocks below it.
    pub next: Option<u64>,
}

/// What history holds for an account overall: where it is complete, how far it is final, and
/// what the send check needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coverage {
    /// The account.
    pub account: Address,
    /// The blocks covered for each capability this version knows, by capability code.
    pub ranges: Vec<(Capability, Ranges)>,
    /// The highest settled block, below which history no longer changes.
    pub settled: Option<u64>,
    /// The proven states where forward coverage opened, oldest first.
    pub opens: Vec<OpenState>,
    /// The account's own stored transactions as `(block number, transaction)`, oldest first,
    /// each with its encoding, which carries the nonce.
    pub sends: Vec<(u64, TxRow)>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    #[test]
    fn at_round_trips_every_block_access_index_of_a_block() {
        let n = 5;
        let cases = [
            (0, At::System),
            (1, At::Tx(0)),
            (5, At::Tx(4)),
            (6, At::Post),
        ];
        for (index, at) in cases {
            assert_eq!(At::from_index(index, n), Some(at));
            assert_eq!(at.index(n), index);
        }
    }

    #[test]
    fn at_rejects_an_index_past_the_post_block_index() {
        assert_eq!(At::from_index(7, 5), None);
        assert_eq!(At::from_index(u64::MAX, u32::MAX), None);
    }

    #[test]
    fn an_empty_block_has_only_system_and_post_positions() {
        assert_eq!(At::from_index(0, 0), Some(At::System));
        assert_eq!(At::from_index(1, 0), Some(At::Post));
        assert_eq!(At::from_index(2, 0), None);
    }

    #[test]
    fn capability_codes_are_one_for_bal_and_two_for_logs() {
        assert_eq!(Capability::Bal.code(), 1);
        assert_eq!(Capability::Logs.code(), 2);
        assert_eq!(Capability::from_code(1), Some(Capability::Bal));
        assert_eq!(Capability::from_code(2), Some(Capability::Logs));
    }

    #[test]
    fn unknown_capability_codes_read_as_none() {
        assert_eq!(Capability::from_code(0), None);
        assert_eq!(Capability::from_code(3), None);
    }

    fn ranges(rows: &[(u64, u64)]) -> Ranges {
        rows.iter().copied().collect()
    }

    fn pairs(ranges: &Ranges) -> Vec<(u64, u64)> {
        ranges.iter().collect()
    }

    #[test]
    fn inserting_a_range_inside_another_changes_nothing() {
        let mut set = ranges(&[(100, 200)]);
        set.insert(120, 130);
        assert_eq!(pairs(&set), vec![(100, 200)]);
    }

    #[test]
    fn a_range_filling_the_hole_between_two_ranges_merges_all_three() {
        let mut set = ranges(&[(100, 110), (120, 130)]);
        set.insert(111, 119);
        assert_eq!(pairs(&set), vec![(100, 130)]);
    }

    #[test]
    fn a_range_one_block_short_of_adjacent_stays_separate() {
        let mut set = ranges(&[(100, 110)]);
        set.insert(112, 115);
        assert_eq!(pairs(&set), vec![(100, 110), (112, 115)]);
        assert_eq!(set.gaps(100, 115), vec![(111, 111)]);
    }

    #[test]
    fn building_from_rows_merges_overlapping_and_adjacent_rows_in_any_order() {
        let set = ranges(&[(20, 25), (5, 9), (10, 12), (7, 8), (30, 30)]);
        assert_eq!(pairs(&set), vec![(5, 12), (20, 25), (30, 30)]);
        assert_eq!(set.first(), Some(5));
        assert_eq!(set.last(), Some(30));
    }

    #[test]
    fn truncating_cuts_a_range_that_straddles_the_bound() {
        let mut set = ranges(&[(100, 110), (120, 130)]);
        set.truncate_above(125);
        assert_eq!(pairs(&set), vec![(100, 110), (120, 125)]);
        set.truncate_above(99);
        assert_eq!(pairs(&set), vec![]);
        assert_eq!(set.first(), None);
        assert_eq!(set.last(), None);
    }

    #[test]
    fn ranges_reaching_u64_max_merge_and_report_gaps() {
        let mut set = ranges(&[(u64::MAX - 1, u64::MAX)]);
        set.insert(0, u64::MAX - 3);
        assert_eq!(set.gaps(0, u64::MAX), vec![(u64::MAX - 2, u64::MAX - 2)]);
        set.insert(u64::MAX - 2, u64::MAX - 2);
        assert_eq!(pairs(&set), vec![(0, u64::MAX)]);
        assert!(set.contains(u64::MAX));
        assert_eq!(set.gaps(0, u64::MAX), vec![]);
    }

    #[test]
    fn gaps_of_an_empty_window_are_empty() {
        assert_eq!(Ranges::default().gaps(5, 4), vec![]);
    }

    #[derive(Clone, Debug)]
    enum Op {
        Insert(u64, u64),
        Truncate(u64),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            3 => (0u64..64, 0u64..64).prop_map(|(a, b)| Op::Insert(a.min(b), a.max(b))),
            1 => (0u64..64).prop_map(Op::Truncate),
        ]
    }

    fn model_runs(model: &BTreeSet<u64>, lo: u64, hi: u64) -> Vec<(u64, u64)> {
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for n in (lo..=hi).filter(|n| !model.contains(n)) {
            match runs.last_mut() {
                Some((_, last)) if *last + 1 == n => *last = n,
                _ => runs.push((n, n)),
            }
        }
        runs
    }

    fn assert_canonical(set: &Ranges) {
        for (first, last) in set.iter() {
            assert!(first <= last, "{set:?}");
        }
        for window in pairs(set).windows(2) {
            assert!(window[0].1 + 1 < window[1].0, "{set:?}");
        }
    }

    proptest! {
        #[test]
        fn ranges_agree_with_a_set_of_numbers(
            ops in prop::collection::vec(op(), 0..24),
            lo in 0u64..64,
            hi in 0u64..64,
        ) {
            let mut set = Ranges::default();
            let mut model = BTreeSet::new();
            for op in ops {
                match op {
                    Op::Insert(first, last) => {
                        set.insert(first, last);
                        model.extend(first..=last);
                    }
                    Op::Truncate(n) => {
                        set.truncate_above(n);
                        model.retain(|&m| m <= n);
                    }
                }
                assert_canonical(&set);
            }
            for n in 0..66 {
                prop_assert_eq!(set.contains(n), model.contains(&n), "block {}", n);
            }
            prop_assert_eq!(set.first(), model.first().copied());
            prop_assert_eq!(set.last(), model.last().copied());
            prop_assert_eq!(set.gaps(lo, hi), model_runs(&model, lo, hi));
        }

        #[test]
        fn building_from_rows_agrees_with_a_set_of_numbers(
            rows in prop::collection::vec((0u64..64, 0u64..64), 0..12),
        ) {
            let rows: Vec<(u64, u64)> = rows.into_iter().map(|(a, b)| (a.min(b), a.max(b))).collect();
            let set: Ranges = rows.iter().copied().collect();
            let model: BTreeSet<u64> = rows.iter().flat_map(|&(first, last)| first..=last).collect();
            assert_canonical(&set);
            prop_assert_eq!(set.gaps(0, 65), model_runs(&model, 0, 65));
        }
    }
}
