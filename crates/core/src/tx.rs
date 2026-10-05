//! Submitted transactions, verified receipts, and fee data.
//!
//! chibi holds no keys. Transactions arrive signed from the wallet; chibi records them, finds
//! the verified block that included them, and keeps the verified receipt.

use std::collections::{HashMap, VecDeque};

use alloy_consensus::{Transaction as _, TxReceipt as _};
use alloy_eips::{BlockNumHash, eip1559::BaseFeeParams};
use alloy_primitives::{Address, Bloom, Log, TxHash, TxKind};

use crate::state::Store;
use crate::verify::{VerifiedBody, VerifiedHeader};
use crate::wire;

/// Located transactions kept for receipt lookups.
const LOCATED_LIMIT: usize = 1024;

/// A transaction chibi accepted for broadcast and has not yet found in a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingTx {
    /// Transaction hash.
    pub hash: TxHash,
    /// Recovered sender.
    pub sender: Address,
    /// Sender nonce the transaction uses.
    pub nonce: u64,
    /// The signed transaction.
    pub tx: wire::Transaction,
}

impl PendingTx {
    /// A pending record for `tx` signed by `sender`. The caller recovered `sender`.
    pub fn new(tx: wire::Transaction, sender: Address) -> Self {
        Self {
            hash: *tx.tx_hash(),
            sender,
            nonce: tx.nonce(),
            tx,
        }
    }
}

/// A transaction found in a verified block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    /// The including block.
    pub block: BlockNumHash,
    /// Position in the block.
    pub index: u64,
    /// Sender recorded at submission.
    pub sender: Address,
    /// Nonce recorded at submission.
    pub nonce: u64,
    /// The signed transaction, kept so a reorg can return it to pending.
    pub tx: wire::Transaction,
    /// The verified receipt, once fetched.
    pub receipt: Option<VerifiedReceipt>,
}

/// What chibi knows about one transaction hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// Never submitted through chibi.
    Unknown,
    /// Submitted, not yet found in a verified block.
    Pending(&'a PendingTx),
    /// Found in a verified block.
    Located(&'a Located),
}

/// Transactions submitted through chibi, pending and located.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TxBook {
    pending: Vec<PendingTx>,
    located: HashMap<TxHash, Located>,
    located_order: VecDeque<TxHash>,
}

impl TxBook {
    /// Records a submission. A resubmission of the same hash is ignored; a different
    /// transaction with the same sender and nonce replaces the earlier one.
    pub fn submit(&mut self, tx: PendingTx) {
        if self.pending.iter().any(|p| p.hash == tx.hash) || self.located.contains_key(&tx.hash) {
            return;
        }
        self.pending
            .retain(|p| p.sender != tx.sender || p.nonce != tx.nonce);
        self.pending.push(tx);
    }

    /// Whether any submission is still unlocated.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Unlocated submissions.
    pub fn pending(&self) -> &[PendingTx] {
        &self.pending
    }

    /// The next nonce for `sender`, counting consecutive pending submissions on top of the
    /// anchor nonce.
    pub fn pending_nonce(&self, sender: Address, anchor_nonce: u64) -> u64 {
        let mut next = anchor_nonce;
        while self
            .pending
            .iter()
            .any(|p| p.sender == sender && p.nonce == next)
        {
            next += 1;
        }
        next
    }

    /// Moves pending transactions found in `body` to located. Returns their hashes and
    /// indexes in block order.
    pub fn locate(&mut self, body: &VerifiedBody) -> Vec<(TxHash, u64)> {
        let mut found = Vec::new();
        if self.pending.is_empty() {
            return found;
        }
        let block = body.header().num_hash();
        for (index, tx) in (0u64..).zip(body.transactions()) {
            let hash = *tx.tx_hash();
            let Some(position) = self.pending.iter().position(|p| p.hash == hash) else {
                continue;
            };
            let pending = self.pending.swap_remove(position);
            self.insert_located(
                hash,
                Located {
                    block,
                    index,
                    sender: pending.sender,
                    nonce: pending.nonce,
                    tx: pending.tx,
                    receipt: None,
                },
            );
            found.push((hash, index));
        }
        found
    }

    /// Returns located transactions to pending, dropping their receipts, when `store` no
    /// longer has their block on its chain: the held header at that number differs, or the
    /// block is above the anchor. Blocks older than the header window are kept, since the
    /// store cannot judge them. Returns the revoked hashes.
    pub fn revalidate(&mut self, store: &Store) -> Vec<TxHash> {
        let anchor = store.anchor().number();
        let revoked: Vec<TxHash> = self
            .located
            .iter()
            .filter(|(_, located)| {
                located.block.number > anchor
                    || store
                        .header(located.block.number)
                        .is_some_and(|held| held.hash() != located.block.hash)
            })
            .map(|(hash, _)| *hash)
            .collect();
        for hash in &revoked {
            if let Some(located) = self.located.remove(hash) {
                self.pending.push(PendingTx {
                    hash: *hash,
                    sender: located.sender,
                    nonce: located.nonce,
                    tx: located.tx,
                });
            }
        }
        if !revoked.is_empty() {
            self.located_order
                .retain(|hash| self.located.contains_key(hash));
        }
        revoked
    }

    /// Drops pending transactions of `sender` whose nonce the anchor already consumed: another
    /// transaction took the nonce. Returns the dropped hashes.
    pub fn prune(&mut self, sender: Address, anchor_nonce: u64) -> Vec<TxHash> {
        let mut dropped = Vec::new();
        self.pending.retain(|p| {
            let stale = p.sender == sender && p.nonce < anchor_nonce;
            if stale {
                dropped.push(p.hash);
            }
            !stale
        });
        dropped
    }

    /// Attaches a verified receipt to its located transaction. Returns whether it matched.
    pub fn store_receipt(&mut self, receipt: VerifiedReceipt) -> bool {
        match self.located.get_mut(&receipt.tx_hash) {
            Some(located) if located.block == receipt.block && located.index == receipt.index => {
                located.receipt = Some(receipt);
                true
            }
            Some(_) | None => false,
        }
    }

    /// Looks up a transaction hash.
    pub fn lookup(&self, hash: TxHash) -> Lookup<'_> {
        if let Some(located) = self.located.get(&hash) {
            return Lookup::Located(located);
        }
        self.pending
            .iter()
            .find(|p| p.hash == hash)
            .map_or(Lookup::Unknown, Lookup::Pending)
    }

    fn insert_located(&mut self, hash: TxHash, located: Located) {
        self.located.insert(hash, located);
        self.located_order.push_back(hash);
        while self.located_order.len() > LOCATED_LIMIT {
            if let Some(oldest) = self.located_order.pop_front() {
                self.located.remove(&oldest);
            }
        }
    }
}

/// A receipt proven against its block's receipts root, with the fields
/// `eth_getTransactionReceipt` needs beyond what the submission recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedReceipt {
    block: BlockNumHash,
    index: u64,
    tx_hash: TxHash,
    kind: TxKind,
    success: bool,
    cumulative_gas_used: u64,
    gas_used: u64,
    effective_gas_price: u128,
    first_log_index: u64,
    logs: Vec<Log>,
    logs_bloom: Bloom,
}

impl VerifiedReceipt {
    pub(crate) fn new(
        header: &VerifiedHeader,
        index: u64,
        tx: &wire::Transaction,
        receipt: &wire::Receipt,
        gas_used: u64,
        first_log_index: u64,
    ) -> Self {
        Self {
            block: header.num_hash(),
            index,
            tx_hash: *tx.tx_hash(),
            kind: tx.kind(),
            success: receipt.success,
            cumulative_gas_used: receipt.cumulative_gas_used,
            gas_used,
            effective_gas_price: tx.effective_gas_price(header.header().base_fee_per_gas),
            first_log_index,
            logs: receipt.logs.clone(),
            logs_bloom: receipt.bloom(),
        }
    }

    /// The including block.
    pub const fn block(&self) -> BlockNumHash {
        self.block
    }

    /// Position in the block.
    pub const fn index(&self) -> u64 {
        self.index
    }

    /// Transaction hash.
    pub const fn tx_hash(&self) -> TxHash {
        self.tx_hash
    }

    /// Call target or contract creation.
    pub const fn kind(&self) -> TxKind {
        self.kind
    }

    /// Whether execution succeeded.
    pub const fn success(&self) -> bool {
        self.success
    }

    /// Gas used by this and every earlier transaction in the block.
    pub const fn cumulative_gas_used(&self) -> u64 {
        self.cumulative_gas_used
    }

    /// Gas used by this transaction.
    pub const fn gas_used(&self) -> u64 {
        self.gas_used
    }

    /// Price per gas actually paid.
    pub const fn effective_gas_price(&self) -> u128 {
        self.effective_gas_price
    }

    /// Block-wide index of the first log.
    pub const fn first_log_index(&self) -> u64 {
        self.first_log_index
    }

    /// Logs in emission order.
    pub fn logs(&self) -> &[Log] {
        &self.logs
    }

    /// Bloom over this receipt's logs.
    pub const fn logs_bloom(&self) -> Bloom {
        self.logs_bloom
    }

    /// The created contract address, given the sender and nonce recorded at submission.
    pub fn contract_address(&self, sender: Address, nonce: u64) -> Option<Address> {
        match self.kind {
            TxKind::Create => Some(sender.create(nonce)),
            TxKind::Call(_) => None,
        }
    }
}

/// The base fee of the block after `header`. `None` before London.
pub fn next_base_fee(header: &VerifiedHeader, params: BaseFeeParams) -> Option<u64> {
    header.header().next_block_base_fee(params)
}

/// Fee data from one verified block body.
#[derive(Clone, Debug, PartialEq)]
pub struct FeeSample {
    /// The sampled block.
    pub block: BlockNumHash,
    /// The block's base fee.
    pub base_fee: u64,
    /// `gas_used / gas_limit` of the block.
    pub gas_used_ratio: f64,
    /// Effective priority fees paid, ascending.
    pub tips: Vec<u128>,
}

/// Samples the effective priority fees of a verified body.
pub fn sample(body: &VerifiedBody) -> FeeSample {
    let header = body.header().header();
    let base_fee = header.base_fee_per_gas.unwrap_or_default();
    let mut tips: Vec<u128> = body
        .transactions()
        .iter()
        .filter_map(|tx| tx.effective_tip_per_gas(base_fee))
        .collect();
    tips.sort_unstable();
    let gas_used_ratio = if header.gas_limit == 0 {
        0.0
    } else {
        header.gas_used as f64 / header.gas_limit as f64
    };
    FeeSample {
        block: body.header().num_hash(),
        base_fee,
        gas_used_ratio,
        tips,
    }
}

/// The tip at `percentile` (0 to 100) across every sampled transaction. `None` without
/// samples.
pub fn tip_percentile<'a>(
    samples: impl IntoIterator<Item = &'a FeeSample>,
    percentile: f64,
) -> Option<u128> {
    let mut tips: Vec<u128> = samples
        .into_iter()
        .flat_map(|s| s.tips.iter().copied())
        .collect();
    if tips.is_empty() {
        return None;
    }
    tips.sort_unstable();
    let rank = (percentile.clamp(0.0, 100.0) / 100.0 * (tips.len() - 1) as f64).round() as usize;
    tips.get(rank).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{BlockWrites, World, WorldAccount, eth, legacy_transfer};
    use crate::verify;
    use alloy_primitives::address;

    #[test]
    fn located_receipt_has_per_tx_gas_and_log_offsets() {
        let alice = address!("0x00000000000000000000000000000000000a11ce");
        let bob = address!("0x0000000000000000000000000000000000000b0b");
        let mut world = World::genesis([(alice, WorldAccount::eoa(eth(10)))]);
        let first = legacy_transfer(0, bob, eth(1));
        let second = legacy_transfer(1, bob, eth(1));
        let pending = PendingTx::new(second.clone(), alice);
        let mut book = TxBook::default();
        book.submit(pending.clone());
        assert_eq!(book.pending_nonce(alice, 1), 2);
        assert_eq!(book.lookup(pending.hash), Lookup::Pending(&pending));

        let hash = world.mine(
            BlockWrites::default()
                .tx(first, 21_000, 1)
                .tx(second, 21_000, 2)
                .balance(alice, eth(8)),
        );
        let header = world.verified(hash);
        let body = verify::body(header.clone(), world.body(hash)).unwrap();
        let receipts = verify::receipts(header.clone(), world.receipts(hash)).unwrap();

        assert_eq!(book.locate(&body), vec![(pending.hash, 1)]);
        assert!(!book.has_pending());
        let receipt = receipts.receipt(&body, 1).unwrap();
        assert_eq!(
            (
                receipt.gas_used(),
                receipt.cumulative_gas_used(),
                receipt.first_log_index()
            ),
            (21_000, 42_000, 1)
        );
        assert_eq!(receipt.logs().len(), 2);
        assert!(book.store_receipt(receipt.clone()));
        let Lookup::Located(located) = book.lookup(pending.hash) else {
            panic!("not located")
        };
        assert_eq!(
            located,
            &Located {
                block: header.num_hash(),
                index: 1,
                sender: alice,
                nonce: 1,
                tx: pending.tx,
                receipt: Some(receipt),
            }
        );
    }

    #[test]
    fn reorged_location_returns_to_pending() {
        let alice = address!("0x00000000000000000000000000000000000a11ce");
        let bob = address!("0x0000000000000000000000000000000000000b0b");
        let mut world = World::genesis([(alice, WorldAccount::eoa(eth(10)))]);
        let pending = PendingTx::new(legacy_transfer(0, bob, eth(1)), alice);
        let mut book = TxBook::default();
        book.submit(pending.clone());

        let first = world.mine(BlockWrites::default().tx(pending.tx.clone(), 21_000, 0));
        let first_body = verify::body(world.verified(first), world.body(first)).unwrap();
        assert_eq!(book.locate(&first_body), vec![(pending.hash, 0)]);
        let receipts = verify::receipts(world.verified(first), world.receipts(first)).unwrap();
        assert!(book.store_receipt(receipts.receipt(&first_body, 0).unwrap()));
        assert_eq!(
            book.revalidate(&Store::new(world.verified(first))),
            Vec::<TxHash>::new()
        );
        assert_eq!(book.pending_nonce(alice, 0), 0);

        let sibling = world.fork_head(BlockWrites::default());
        assert_eq!(world.header(sibling).number, world.header(first).number);
        let reorged = Store::new(world.verified(sibling));
        assert_eq!(book.revalidate(&reorged), vec![pending.hash]);
        assert_eq!(book.lookup(pending.hash), Lookup::Pending(&pending));
        assert_eq!(book.pending_nonce(alice, 0), 1);

        let next = world.mine(BlockWrites::default().tx(pending.tx.clone(), 21_000, 0));
        let header = world.verified(next);
        let body = verify::body(header.clone(), world.body(next)).unwrap();
        assert_eq!(book.locate(&body), vec![(pending.hash, 0)]);
        let Lookup::Located(located) = book.lookup(pending.hash) else {
            panic!("not located")
        };
        assert_eq!(
            (located.block, located.receipt.as_ref()),
            (header.num_hash(), None)
        );
        assert_eq!(book.revalidate(&Store::new(header)), Vec::<TxHash>::new());
    }

    #[test]
    fn replaced_nonce_is_pruned() {
        let bob = address!("0x0000000000000000000000000000000000000b0b");
        let alice = address!("0x00000000000000000000000000000000000a11ce");
        let mut book = TxBook::default();
        let tx = PendingTx::new(legacy_transfer(3, bob, eth(1)), alice);
        book.submit(tx.clone());
        assert_eq!(book.prune(alice, 4), vec![tx.hash]);
        assert_eq!(book.lookup(tx.hash), Lookup::Unknown);
    }

    #[test]
    fn tips_come_from_verified_bodies() {
        let alice = address!("0x00000000000000000000000000000000000a11ce");
        let bob = address!("0x0000000000000000000000000000000000000b0b");
        let mut world = World::genesis([(alice, WorldAccount::eoa(eth(10)))]);
        let hash =
            world.mine(BlockWrites::default().tx(legacy_transfer(0, bob, eth(1)), 21_000, 0));
        let header = world.verified(hash);
        let body = verify::body(header.clone(), world.body(hash)).unwrap();
        let sample = sample(&body);
        let base_fee = header.header().base_fee_per_gas.unwrap();
        assert_eq!(
            sample.tips,
            vec![u128::from(crate::testing::GAS_PRICE - base_fee)]
        );
        assert_eq!(tip_percentile([&sample], 50.0), Some(sample.tips[0]));
        let target = crate::testing::GAS_LIMIT / 2;
        let decrease = base_fee * (target - 21_000) / target / 8;
        assert_eq!(
            next_base_fee(&header, BaseFeeParams::ethereum()),
            Some(base_fee - decrease)
        );
    }
}
