//! The scan: which facts of one verified block concern a set of watched accounts.
//!
//! A forward scan holds the block's verified access list and grants [`Capability::Bal`] and
//! [`Capability::Logs`]. A backward scan holds only the header and grants logs. Either asks for
//! the block's contents only when the access list or the header bloom says a watched account
//! may be in them.

use std::collections::{BTreeMap, BTreeSet};

use alloy_consensus::Transaction as _;
use alloy_consensus::transaction::SignerRecoverable as _;
use alloy_eip7928::AccountChanges;
use alloy_eips::eip2718::Encodable2718 as _;
use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{Address, B256, BloomInput, Bytes, Log};

use super::{
    At, BlockFacts, BlockRow, Capability, LogRow, ScannedBlock, SlotChange, Touch, TxRow,
    WithdrawalRow,
};
use crate::verify::{VerifiedBlock, VerifiedBody, VerifiedHeader, VerifiedReceipts};
use crate::wire;

/// The outcome of the first look at a block: done, or waiting for its contents.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "a scan lives for one block and is matched at once; boxing either side only adds an allocation"
)]
pub enum Scan {
    /// Nothing in the block's contents can concern a watched account. The block still grants
    /// coverage.
    Done(ScannedBlock),
    /// A watched account may be in the block's contents: fetch its body and receipts and
    /// finish with [`Pending::complete`].
    NeedsContents(Pending),
}

/// A scan waiting for the block's body and receipts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    header: VerifiedHeader,
    watched: Vec<Address>,
    source: Source,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    /// Forward: the watched accounts' entries of the verified block access list.
    Bal(Vec<AccountChanges>),
    /// Backward: the header bloom alone.
    Bloom,
}

impl Source {
    fn capabilities(&self) -> Vec<Capability> {
        match self {
            Source::Bal(_) => vec![Capability::Bal, Capability::Logs],
            Source::Bloom => vec![Capability::Logs],
        }
    }

    fn entries(&self) -> &[AccountChanges] {
        match self {
            Source::Bal(entries) => entries,
            Source::Bloom => &[],
        }
    }
}

/// Body or receipts that cannot complete a [`Pending`] scan.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScanError {
    /// The body belongs to another block.
    #[error("the body is of block {got}, not of the scanned block {expected}")]
    BodyOfOtherBlock {
        /// The scanned block.
        expected: B256,
        /// The body's block.
        got: B256,
    },
    /// The receipts belong to another block.
    #[error("the receipts are of block {got}, not of the scanned block {expected}")]
    ReceiptsOfOtherBlock {
        /// The scanned block.
        expected: B256,
        /// The receipts' block.
        got: B256,
    },
    /// The receipt list and the transaction list differ in length.
    #[error("block {block} has {txs} transactions but {receipts} receipts")]
    ReceiptCount {
        /// The scanned block.
        block: B256,
        /// Transactions in the body.
        txs: usize,
        /// Receipts in the list.
        receipts: usize,
    },
    /// The block holds more transactions than a block access index can number.
    #[error("block {block} has {count} transactions, more than a block access index can number")]
    TxCount {
        /// The scanned block.
        block: B256,
        /// Transactions in the body.
        count: usize,
    },
    /// A transaction's signature yields no sender, so whether it is a watched account's own
    /// send cannot be told.
    #[error("the sender of transaction {index} in block {block} does not recover")]
    Sender {
        /// The scanned block.
        block: B256,
        /// The transaction's position in the block.
        index: u32,
    },
}

/// Scans a block the follower extended to, with its verified access list. The block's
/// contents are needed when a watched account has an access list entry or the header bloom
/// may hold a log naming it.
pub fn scan_forward(watched: &[Address], block: &VerifiedBlock) -> Scan {
    let watched = sorted(watched);
    let entries: Vec<AccountChanges> = block
        .bal()
        .iter()
        .filter(|changes| watched.binary_search(&changes.address).is_ok())
        .cloned()
        .collect();
    let fetch = !entries.is_empty() || bloom_may_name(block.header(), &watched);
    first_look(block.header(), watched, Source::Bal(entries), fetch)
}

/// Scans an older block from its header alone. The block's contents are needed when the
/// header bloom may hold a log naming a watched account.
pub fn scan_backward(watched: &[Address], header: &VerifiedHeader) -> Scan {
    let watched = sorted(watched);
    let fetch = bloom_may_name(header, &watched);
    first_look(header, watched, Source::Bloom, fetch)
}

fn first_look(header: &VerifiedHeader, watched: Vec<Address>, source: Source, fetch: bool) -> Scan {
    if fetch {
        Scan::NeedsContents(Pending {
            header: header.clone(),
            watched,
            source,
        })
    } else {
        Scan::Done(scanned(header, watched, &source, None))
    }
}

fn scanned(
    header: &VerifiedHeader,
    accounts: Vec<Address>,
    source: &Source,
    facts: Option<BlockFacts>,
) -> ScannedBlock {
    ScannedBlock {
        number: header.number(),
        hash: header.hash(),
        parent_hash: header.header().parent_hash,
        accounts,
        capabilities: source.capabilities(),
        facts,
    }
}

fn sorted(watched: &[Address]) -> Vec<Address> {
    let mut sorted = watched.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted
}

fn bloom_may_name(header: &VerifiedHeader, watched: &[Address]) -> bool {
    let bloom = &header.header().logs_bloom;
    watched.iter().any(|account| {
        bloom.contains_input(BloomInput::Raw(account.as_slice()))
            || bloom.contains_input(BloomInput::Raw(account.into_word().as_slice()))
    })
}

/// Whether `log` names `account` as its emitter or in topics 1-3. Topic 0 is an event
/// signature, and data words are not a trigger.
fn names(log: &Log, account: Address) -> bool {
    log.address == account
        || log
            .topics()
            .iter()
            .skip(1)
            .any(|topic| *topic == account.into_word())
}

impl Pending {
    /// The block whose contents the scan waits for.
    pub const fn header(&self) -> &VerifiedHeader {
        &self.header
    }

    /// Finishes the scan with the block's verified body and receipts. `facts` is `None` when
    /// nothing in the block concerned a watched account after all, as on a bloom false
    /// positive.
    pub fn complete(
        self,
        body: &VerifiedBody,
        receipts: &VerifiedReceipts,
    ) -> Result<ScannedBlock, ScanError> {
        let block = self.header.hash();
        if body.header().hash() != block {
            return Err(ScanError::BodyOfOtherBlock {
                expected: block,
                got: body.header().hash(),
            });
        }
        if receipts.header().hash() != block {
            return Err(ScanError::ReceiptsOfOtherBlock {
                expected: block,
                got: receipts.header().hash(),
            });
        }
        let (txs, receipts) = (body.transactions(), receipts.receipts());
        if txs.len() != receipts.len() {
            return Err(ScanError::ReceiptCount {
                block,
                txs: txs.len(),
                receipts: receipts.len(),
            });
        }
        let tx_count = u32::try_from(txs.len()).map_err(|_| ScanError::TxCount {
            block,
            count: txs.len(),
        })?;
        let mut found = Found::new(tx_count);
        for changes in self.source.entries() {
            found.bal(changes);
        }
        found.txs(block, &self.watched, txs, receipts)?;
        found.withdrawals(&self.watched, body.withdrawals());
        let facts = found.into_facts(&self.header);
        Ok(scanned(&self.header, self.watched, &self.source, facts))
    }
}

/// What a completed scan collects. Touches are keyed by (account, block access index), so
/// each relevant position of an account gets exactly one.
struct Found {
    tx_count: u32,
    touches: BTreeMap<(Address, u64), Touch>,
    /// Positions where an account's nonce, code or storage changed: the transactions there
    /// are its own.
    owning: BTreeSet<(Address, u64)>,
    slots: Vec<SlotChange>,
    txs: Vec<TxRow>,
    logs: Vec<LogRow>,
    withdrawals: Vec<WithdrawalRow>,
}

impl Found {
    const fn new(tx_count: u32) -> Self {
        Self {
            tx_count,
            touches: BTreeMap::new(),
            owning: BTreeSet::new(),
            slots: Vec::new(),
            txs: Vec::new(),
            logs: Vec::new(),
            withdrawals: Vec::new(),
        }
    }

    /// The touch of `account` at block access `index`, made empty when new. `None` for an
    /// index past the post-block index, which a verified access list never holds.
    fn touch(&mut self, account: Address, index: u64) -> Option<&mut Touch> {
        let at = At::from_index(index, self.tx_count)?;
        Some(self.touches.entry((account, index)).or_insert(Touch {
            account,
            at,
            balance: None,
            nonce: None,
            code: None,
        }))
    }

    fn bal(&mut self, changes: &AccountChanges) {
        let account = changes.address;
        for change in &changes.balance_changes {
            if let Some(touch) = self.touch(account, change.block_access_index.0) {
                touch.balance = Some(change.post_balance);
            }
        }
        for change in &changes.nonce_changes {
            let index = change.block_access_index.0;
            self.owning.insert((account, index));
            if let Some(touch) = self.touch(account, index) {
                touch.nonce = Some(change.new_nonce);
            }
        }
        for change in &changes.code_changes {
            let index = change.block_access_index.0;
            self.owning.insert((account, index));
            if let Some(touch) = self.touch(account, index) {
                touch.code = Some(change.new_code.clone());
            }
        }
        for slot in &changes.storage_changes {
            for change in &slot.changes {
                let index = change.block_access_index.0;
                self.owning.insert((account, index));
                if let Some(at) = self.touch(account, index).map(|touch| touch.at) {
                    self.slots.push(SlotChange {
                        account,
                        at,
                        contract: account,
                        slot: slot.slot,
                        value: change.new_value,
                    });
                }
            }
        }
    }

    fn txs(
        &mut self,
        block: B256,
        watched: &[Address],
        txs: &[wire::Transaction],
        receipts: &[wire::Receipt],
    ) -> Result<(), ScanError> {
        let mut prior_gas = 0;
        let mut next_log = 0;
        for ((position, tx), receipt) in (0u32..).zip(txs).zip(receipts) {
            let index = u64::from(position) + 1;
            let first_log_index = next_log;
            let log_count = receipt.logs.len() as u64;
            next_log += log_count;
            let gas_used = receipt.cumulative_gas_used.saturating_sub(prior_gas);
            prior_gas = receipt.cumulative_gas_used;
            let sender = tx.recover_signer().map_err(|_| ScanError::Sender {
                block,
                index: position,
            })?;
            // An authorization whose signature does not recover is skipped by the protocol
            // too, so it delegates nobody.
            let authorities: Vec<Address> = tx
                .authorization_list()
                .unwrap_or_default()
                .iter()
                .filter_map(|authorization| authorization.recover_authority().ok())
                .collect();
            let mut own = false;
            let mut relevant = false;
            for &account in watched {
                let owns = sender == account
                    || tx.to() == Some(account)
                    || authorities.contains(&account)
                    || self.owning.contains(&(account, index));
                if owns || receipt.logs.iter().any(|log| names(log, account)) {
                    relevant = true;
                    self.touch(account, index);
                }
                own |= owns;
            }
            if !relevant {
                continue;
            }
            self.logs.extend(
                (first_log_index..)
                    .zip(&receipt.logs)
                    .filter(|(_, log)| own || watched.iter().any(|&account| names(log, account)))
                    .map(|(index, log)| LogRow {
                        index,
                        log: log.clone(),
                    }),
            );
            self.txs.push(TxRow {
                index: position,
                hash: *tx.tx_hash(),
                sender,
                raw: own.then(|| Bytes::from(tx.encoded_2718())),
                success: receipt.success,
                cumulative_gas: receipt.cumulative_gas_used,
                gas_used,
                first_log_index,
                log_count,
            });
        }
        Ok(())
    }

    fn withdrawals(&mut self, watched: &[Address], withdrawals: &[Withdrawal]) {
        let post = u64::from(self.tx_count) + 1;
        for withdrawal in withdrawals {
            if watched.binary_search(&withdrawal.address).is_ok() {
                self.touch(withdrawal.address, post);
                self.withdrawals.push(WithdrawalRow {
                    index: withdrawal.index,
                    validator: withdrawal.validator_index,
                    address: withdrawal.address,
                    amount_gwei: withdrawal.amount,
                });
            }
        }
    }

    /// The block's facts, `None` when no account turned out relevant. Every stored fact has
    /// a touch at its position, so no touches means nothing to store.
    fn into_facts(self, header: &VerifiedHeader) -> Option<BlockFacts> {
        if self.touches.is_empty() {
            return None;
        }
        let h = header.header();
        Some(BlockFacts {
            block: BlockRow {
                number: header.number(),
                hash: header.hash(),
                timestamp: h.timestamp,
                beneficiary: h.beneficiary,
                base_fee: h.base_fee_per_gas,
                excess_blob_gas: h.excess_blob_gas,
                tx_count: self.tx_count,
            },
            txs: self.txs,
            logs: self.logs,
            touches: self.touches.into_values().collect(),
            slots: self.slots,
            withdrawals: self.withdrawals,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction as _, TxEip7702};
    use alloy_eips::eip7702::Authorization;
    use alloy_primitives::{Bytes, Log, U256, address, b256};
    use alloy_signer::SignerSync as _;
    use alloy_signer_local::PrivateKeySigner;

    use super::*;
    use crate::history::{
        At, BlockFacts, BlockRow, LogRow, SlotChange, Touch, TxRow, WithdrawalRow,
    };
    use crate::testing::{BlockWrites, Outcome, World, WorldAccount, eth, signed_transfer};
    use crate::verify::{self, VerifiedBlock};
    use crate::wire;

    const ALICE_KEY: B256 = B256::with_last_byte(1);
    const ALICE: Address = address!("0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf");
    const CAROL_KEY: B256 = B256::with_last_byte(2);
    const CAROL: Address = address!("0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF");
    const BOB: Address = address!("0x0000000000000000000000000000000000000b0b");
    const TOKEN: Address = address!("0x00000000000000000000000000000000000070c3");
    const ETH_LOGGER: Address = address!("0xfffffffffffffffffffffffffffffffffffffffe");
    const TRANSFER: B256 =
        b256!("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
    const NUMBER: u64 = 24_000_001;
    const TIMESTAMP: u64 = 1_790_000_012;

    fn world() -> World {
        World::genesis([(ALICE, WorldAccount::eoa(eth(10)))])
    }

    fn block(world: &World, hash: B256) -> VerifiedBlock {
        verify::bal(world.verified(hash), world.bal(hash)).unwrap()
    }

    fn contents(world: &World, hash: B256) -> (VerifiedBody, VerifiedReceipts) {
        let header = world.verified(hash);
        (
            verify::body(header.clone(), world.body(hash)).unwrap(),
            verify::receipts(header, world.receipts(hash)).unwrap(),
        )
    }

    fn complete(world: &World, hash: B256, scan: Scan) -> ScannedBlock {
        let Scan::NeedsContents(pending) = scan else {
            panic!("{scan:?} asks for no contents")
        };
        assert_eq!(pending.header().hash(), hash);
        let (body, receipts) = contents(world, hash);
        pending.complete(&body, &receipts).unwrap()
    }

    fn forward(world: &World, hash: B256, watched: &[Address]) -> ScannedBlock {
        complete(world, hash, scan_forward(watched, &block(world, hash)))
    }

    fn block_row(hash: B256, tx_count: u32) -> BlockRow {
        BlockRow {
            number: NUMBER,
            hash,
            timestamp: TIMESTAMP,
            beneficiary: Address::ZERO,
            base_fee: Some(1_000_000_000),
            excess_blob_gas: Some(0),
            tx_count,
        }
    }

    fn scanned(
        world: &World,
        hash: B256,
        accounts: Vec<Address>,
        facts: BlockFacts,
    ) -> ScannedBlock {
        ScannedBlock {
            number: NUMBER,
            hash,
            parent_hash: world.header(hash).parent_hash,
            accounts,
            capabilities: vec![Capability::Bal, Capability::Logs],
            facts: Some(facts),
        }
    }

    fn word(value: U256) -> Bytes {
        Bytes::copy_from_slice(&value.to_be_bytes::<32>())
    }

    fn raw(tx: &wire::Transaction) -> Option<Bytes> {
        Some(tx.encoded_2718().into())
    }

    fn touch(account: Address, at: At) -> Touch {
        Touch {
            account,
            at,
            balance: None,
            nonce: None,
            code: None,
        }
    }

    fn outcome(success: bool, gas_used: u64, logs: Vec<Log>) -> Outcome {
        Outcome {
            success,
            gas_used,
            logs,
        }
    }

    #[test]
    fn a_self_send_is_one_own_transaction_with_its_post_values_and_its_log() {
        let tx = signed_transfer(ALICE_KEY, 1, 0, ALICE, eth(1));
        let log = Log::new_unchecked(
            ETH_LOGGER,
            vec![TRANSFER, ALICE.into_word(), ALICE.into_word()],
            word(eth(1)),
        );
        let mut world = world();
        let hash = world.mine(
            BlockWrites::default()
                .at(1)
                .balance(ALICE, eth(9))
                .nonce(ALICE, 1)
                .tx_with(tx.clone(), outcome(true, 21_000, vec![log.clone()])),
        );
        assert_eq!(
            forward(&world, hash, &[ALICE]),
            scanned(
                &world,
                hash,
                vec![ALICE],
                BlockFacts {
                    block: block_row(hash, 1),
                    txs: vec![TxRow {
                        index: 0,
                        hash: *tx.tx_hash(),
                        sender: ALICE,
                        raw: raw(&tx),
                        success: true,
                        cumulative_gas: 21_000,
                        gas_used: 21_000,
                        first_log_index: 0,
                        log_count: 1,
                    }],
                    logs: vec![LogRow { index: 0, log }],
                    touches: vec![Touch {
                        balance: Some(eth(9)),
                        nonce: Some(1),
                        ..touch(ALICE, At::Tx(0))
                    }],
                    slots: vec![],
                    withdrawals: vec![],
                },
            )
        );
    }

    #[test]
    fn an_incoming_token_transfer_keeps_only_the_log_naming_the_account() {
        let first = signed_transfer(CAROL_KEY, 1, 0, BOB, eth(1));
        let token_tx = signed_transfer(CAROL_KEY, 1, 1, TOKEN, U256::ZERO);
        let unrelated = Log::new_unchecked(TOKEN, vec![B256::with_last_byte(9)], Bytes::new());
        let naming = Log::new_unchecked(
            TOKEN,
            vec![TRANSFER, BOB.into_word(), ALICE.into_word()],
            word(U256::from(1_000)),
        );
        let mut world = world();
        let hash = world.mine(BlockWrites::default().tx(first, 21_000, 1).tx_with(
            token_tx.clone(),
            outcome(true, 50_000, vec![unrelated, naming.clone()]),
        ));
        assert_eq!(
            forward(&world, hash, &[ALICE]),
            scanned(
                &world,
                hash,
                vec![ALICE],
                BlockFacts {
                    block: block_row(hash, 2),
                    txs: vec![TxRow {
                        index: 1,
                        hash: *token_tx.tx_hash(),
                        sender: CAROL,
                        raw: None,
                        success: true,
                        cumulative_gas: 71_000,
                        gas_used: 50_000,
                        first_log_index: 1,
                        log_count: 2,
                    }],
                    logs: vec![LogRow {
                        index: 2,
                        log: naming,
                    }],
                    touches: vec![touch(ALICE, At::Tx(1))],
                    slots: vec![],
                    withdrawals: vec![],
                },
            )
        );
    }

    #[test]
    fn a_failed_send_is_an_own_transaction_marked_failed() {
        let tx = signed_transfer(ALICE_KEY, 1, 0, BOB, eth(1));
        let mut world = world();
        let hash = world.mine(
            BlockWrites::default()
                .at(1)
                .balance(ALICE, eth(9))
                .nonce(ALICE, 1)
                .tx_with(tx.clone(), outcome(false, 21_000, vec![])),
        );
        assert_eq!(
            forward(&world, hash, &[ALICE]),
            scanned(
                &world,
                hash,
                vec![ALICE],
                BlockFacts {
                    block: block_row(hash, 1),
                    txs: vec![TxRow {
                        index: 0,
                        hash: *tx.tx_hash(),
                        sender: ALICE,
                        raw: raw(&tx),
                        success: false,
                        cumulative_gas: 21_000,
                        gas_used: 21_000,
                        first_log_index: 0,
                        log_count: 0,
                    }],
                    logs: vec![],
                    touches: vec![Touch {
                        balance: Some(eth(9)),
                        nonce: Some(1),
                        ..touch(ALICE, At::Tx(0))
                    }],
                    slots: vec![],
                    withdrawals: vec![],
                },
            )
        );
    }

    #[test]
    fn a_withdrawal_to_the_account_is_a_post_block_touch_and_a_withdrawal_row() {
        let mut world = world();
        let hash = world.mine(
            BlockWrites::default()
                .withdrawal(Withdrawal {
                    index: 7,
                    validator_index: 42,
                    address: ALICE,
                    amount: 32_000_000_000,
                })
                .withdrawal(Withdrawal {
                    index: 8,
                    validator_index: 43,
                    address: BOB,
                    amount: 5,
                })
                .at(1)
                .balance(ALICE, eth(42))
                .balance(BOB, U256::from(5_000_000_000u64)),
        );
        assert_eq!(
            forward(&world, hash, &[ALICE]),
            scanned(
                &world,
                hash,
                vec![ALICE],
                BlockFacts {
                    block: block_row(hash, 0),
                    txs: vec![],
                    logs: vec![],
                    touches: vec![Touch {
                        balance: Some(eth(42)),
                        ..touch(ALICE, At::Post)
                    }],
                    slots: vec![],
                    withdrawals: vec![WithdrawalRow {
                        index: 7,
                        validator: 42,
                        address: ALICE,
                        amount_gwei: 32_000_000_000,
                    }],
                },
            )
        );
    }

    #[test]
    fn a_backward_bloom_miss_is_done_with_logs_coverage_and_no_facts() {
        let mut world = world();
        let hash = world.mine(BlockWrites::default().at(1).nonce(ALICE, 1).tx(
            signed_transfer(ALICE_KEY, 1, 0, BOB, eth(1)),
            21_000,
            1,
        ));
        assert_eq!(
            scan_backward(&[ALICE, ALICE], &world.verified(hash)),
            Scan::Done(ScannedBlock {
                number: NUMBER,
                hash,
                parent_hash: world.header(hash).parent_hash,
                accounts: vec![ALICE],
                capabilities: vec![Capability::Logs],
                facts: None,
            })
        );
    }

    #[test]
    fn a_backward_bloom_false_positive_completes_with_no_facts() {
        let tx = signed_transfer(CAROL_KEY, 1, 0, TOKEN, U256::ZERO);
        let topic0 = Log::new_unchecked(TOKEN, vec![ALICE.into_word()], word(U256::from(1)));
        let mut world = world();
        let hash =
            world.mine(BlockWrites::default().tx_with(tx, outcome(true, 30_000, vec![topic0])));
        assert_eq!(
            complete(&world, hash, scan_backward(&[ALICE], &world.verified(hash))),
            ScannedBlock {
                number: NUMBER,
                hash,
                parent_hash: world.header(hash).parent_hash,
                accounts: vec![ALICE],
                capabilities: vec![Capability::Logs],
                facts: None,
            }
        );
    }

    #[test]
    fn a_backward_hit_stores_touches_without_post_values() {
        let tx = signed_transfer(CAROL_KEY, 1, 0, TOKEN, U256::ZERO);
        let naming = Log::new_unchecked(
            TOKEN,
            vec![TRANSFER, CAROL.into_word(), ALICE.into_word()],
            word(U256::from(1)),
        );
        let mut world = world();
        let hash = world.mine(
            BlockWrites::default()
                .at(1)
                .balance(ALICE, eth(11))
                .tx_with(tx.clone(), outcome(true, 30_000, vec![naming.clone()])),
        );
        assert_eq!(
            complete(&world, hash, scan_backward(&[ALICE], &world.verified(hash))),
            ScannedBlock {
                capabilities: vec![Capability::Logs],
                ..scanned(
                    &world,
                    hash,
                    vec![ALICE],
                    BlockFacts {
                        block: block_row(hash, 1),
                        txs: vec![TxRow {
                            index: 0,
                            hash: *tx.tx_hash(),
                            sender: CAROL,
                            raw: None,
                            success: true,
                            cumulative_gas: 30_000,
                            gas_used: 30_000,
                            first_log_index: 0,
                            log_count: 1,
                        }],
                        logs: vec![LogRow {
                            index: 0,
                            log: naming,
                        }],
                        touches: vec![touch(ALICE, At::Tx(0))],
                        slots: vec![],
                        withdrawals: vec![],
                    },
                )
            }
        );
    }

    #[test]
    fn a_forward_block_where_no_watched_account_appears_is_done_with_no_facts() {
        let mut world = world();
        let hash = world.mine(BlockWrites::default().at(1).balance(BOB, eth(1)).tx(
            signed_transfer(CAROL_KEY, 1, 0, BOB, eth(1)),
            21_000,
            1,
        ));
        assert_eq!(
            scan_forward(&[ALICE], &block(&world, hash)),
            Scan::Done(ScannedBlock {
                number: NUMBER,
                hash,
                parent_hash: world.header(hash).parent_hash,
                accounts: vec![ALICE],
                capabilities: vec![Capability::Bal, Capability::Logs],
                facts: None,
            })
        );
    }

    #[test]
    fn a_fee_recipient_gets_a_touch_per_credited_transaction_and_owns_none() {
        let mut world = world();
        let hash = world.mine(
            BlockWrites::default()
                .tx(signed_transfer(CAROL_KEY, 1, 0, BOB, eth(1)), 21_000, 0)
                .tx(signed_transfer(CAROL_KEY, 1, 1, BOB, eth(1)), 21_000, 0)
                .at(1)
                .balance(ALICE, eth(11))
                .at(2)
                .balance(ALICE, eth(12)),
        );
        assert_eq!(
            forward(&world, hash, &[ALICE]),
            scanned(
                &world,
                hash,
                vec![ALICE],
                BlockFacts {
                    block: block_row(hash, 2),
                    txs: vec![],
                    logs: vec![],
                    touches: vec![
                        Touch {
                            balance: Some(eth(11)),
                            ..touch(ALICE, At::Tx(0))
                        },
                        Touch {
                            balance: Some(eth(12)),
                            ..touch(ALICE, At::Tx(1))
                        },
                    ],
                    slots: vec![],
                    withdrawals: vec![],
                },
            )
        );
    }

    #[test]
    fn a_contract_storage_change_makes_the_transaction_its_own_and_stores_the_slot() {
        let tx = signed_transfer(CAROL_KEY, 1, 0, BOB, U256::ZERO);
        let mut world = world();
        let hash = world.mine(
            BlockWrites::default()
                .at(1)
                .slot(TOKEN, U256::from(7), U256::from(9))
                .read(TOKEN, U256::from(8))
                .tx_with(tx.clone(), outcome(true, 40_000, vec![])),
        );
        assert_eq!(
            forward(&world, hash, &[TOKEN]),
            scanned(
                &world,
                hash,
                vec![TOKEN],
                BlockFacts {
                    block: block_row(hash, 1),
                    txs: vec![TxRow {
                        index: 0,
                        hash: *tx.tx_hash(),
                        sender: CAROL,
                        raw: raw(&tx),
                        success: true,
                        cumulative_gas: 40_000,
                        gas_used: 40_000,
                        first_log_index: 0,
                        log_count: 0,
                    }],
                    logs: vec![],
                    touches: vec![touch(TOKEN, At::Tx(0))],
                    slots: vec![SlotChange {
                        account: TOKEN,
                        at: At::Tx(0),
                        contract: TOKEN,
                        slot: U256::from(7),
                        value: U256::from(9),
                    }],
                    withdrawals: vec![],
                },
            )
        );
    }

    #[test]
    fn an_eip7702_authority_owns_the_transaction_that_carries_its_authorization() {
        let alice = PrivateKeySigner::from_bytes(&ALICE_KEY).unwrap();
        let carol = PrivateKeySigner::from_bytes(&CAROL_KEY).unwrap();
        let authorization = Authorization {
            chain_id: U256::from(1),
            address: TOKEN,
            nonce: 0,
        };
        let signature = alice
            .sign_hash_sync(&authorization.signature_hash())
            .unwrap();
        let unsigned = TxEip7702 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 100_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1,
            to: BOB,
            authorization_list: vec![authorization.into_signed(signature)],
            ..TxEip7702::default()
        };
        let signature = carol.sign_hash_sync(&unsigned.signature_hash()).unwrap();
        let tx = wire::Transaction::Eip7702(unsigned.into_signed(signature));
        let mut world = world();
        let hash =
            world.mine(BlockWrites::default().tx_with(tx.clone(), outcome(true, 60_000, vec![])));
        let ScannedBlock { facts, .. } = complete(
            &world,
            hash,
            Scan::NeedsContents(Pending {
                header: world.verified(hash),
                watched: vec![ALICE],
                source: Source::Bloom,
            }),
        );
        let facts = facts.unwrap();
        assert_eq!(
            facts.txs,
            [TxRow {
                index: 0,
                hash: *tx.tx_hash(),
                sender: CAROL,
                raw: raw(&tx),
                success: true,
                cumulative_gas: 60_000,
                gas_used: 60_000,
                first_log_index: 0,
                log_count: 0,
            }]
        );
        assert_eq!(facts.touches, [touch(ALICE, At::Tx(0))]);
    }

    #[test]
    fn accounts_sharing_a_transaction_get_one_row_with_their_logs_once() {
        let tx = signed_transfer(ALICE_KEY, 1, 0, TOKEN, U256::ZERO);
        let to_bob = Log::new_unchecked(
            TOKEN,
            vec![TRANSFER, ALICE.into_word(), BOB.into_word()],
            word(U256::from(5)),
        );
        let unrelated = Log::new_unchecked(TOKEN, vec![B256::with_last_byte(9)], Bytes::new());
        let mut world = world();
        let hash = world.mine(BlockWrites::default().at(1).nonce(ALICE, 1).tx_with(
            tx.clone(),
            outcome(true, 50_000, vec![to_bob.clone(), unrelated.clone()]),
        ));
        assert_eq!(
            forward(&world, hash, &[ALICE, BOB, ALICE]),
            scanned(
                &world,
                hash,
                vec![BOB, ALICE],
                BlockFacts {
                    block: block_row(hash, 1),
                    txs: vec![TxRow {
                        index: 0,
                        hash: *tx.tx_hash(),
                        sender: ALICE,
                        raw: raw(&tx),
                        success: true,
                        cumulative_gas: 50_000,
                        gas_used: 50_000,
                        first_log_index: 0,
                        log_count: 2,
                    }],
                    logs: vec![
                        LogRow {
                            index: 0,
                            log: to_bob,
                        },
                        LogRow {
                            index: 1,
                            log: unrelated,
                        },
                    ],
                    touches: vec![
                        touch(BOB, At::Tx(0)),
                        Touch {
                            nonce: Some(1),
                            ..touch(ALICE, At::Tx(0))
                        },
                    ],
                    slots: vec![],
                    withdrawals: vec![],
                },
            )
        );
    }

    #[test]
    fn contents_of_another_block_do_not_complete_the_scan() {
        let mut world = world();
        let first = world.mine(BlockWrites::default().at(1).balance(ALICE, eth(9)).tx(
            signed_transfer(ALICE_KEY, 1, 0, BOB, eth(1)),
            21_000,
            0,
        ));
        let second = world.mine(BlockWrites::default().at(1).balance(ALICE, eth(8)));
        let Scan::NeedsContents(pending) = scan_forward(&[ALICE], &block(&world, second)) else {
            panic!("alice has a BAL entry")
        };
        let (body, receipts) = contents(&world, second);
        let (other_body, other_receipts) = contents(&world, first);
        assert_eq!(
            pending.clone().complete(&other_body, &receipts),
            Err(ScanError::BodyOfOtherBlock {
                expected: second,
                got: first,
            })
        );
        assert_eq!(
            pending.complete(&body, &other_receipts),
            Err(ScanError::ReceiptsOfOtherBlock {
                expected: second,
                got: first,
            })
        );
    }
}
