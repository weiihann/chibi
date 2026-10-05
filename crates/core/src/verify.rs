//! The trust boundary. Peer bytes become domain values here and nowhere else.
//!
//! The only trusted input is a [`TrustedHash`] from the light client. Every other argument is
//! untrusted wire data. Each returned type has private fields and no public constructor, so
//! holding one is proof the check ran.
//!
//! Each query type owns both directions of one single-key snap exchange: it builds the request
//! and verifies the reply against the root it was built for.

use alloy_consensus::{
    Header, Sealable, Sealed, TxReceipt,
    proofs::{calculate_ommers_root, calculate_transaction_root, calculate_withdrawals_root},
};
use alloy_eip7928::{AccountChanges, compute_block_access_list_hash};
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, BlockNumber, Bloom, Bytes, U256, keccak256};
use alloy_rlp::{Decodable, Encodable};
use alloy_trie::EMPTY_ROOT_HASH;
use reth_trie_common::range_proof::{RangeProofError, verify_range_proof};
use revm::bytecode::Bytecode;

use crate::state::{Account, Key};
use crate::tx::VerifiedReceipt;
use crate::wire::{self, AccountRangeRequest, AccountReply, StorageRangeRequest, StorageReply};

/// An execution block hash the sync-committee light client vouched for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TrustedHash(B256);

impl TrustedHash {
    /// Marks `hash` as attested by the light client. Only the light-client adapter calls this.
    pub const fn attested(hash: B256) -> Self {
        Self(hash)
    }

    /// The attested block hash.
    pub const fn hash(&self) -> B256 {
        self.0
    }
}

/// A header whose hash is a [`TrustedHash`] or links to one through `parent_hash`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedHeader(Sealed<Header>);

impl VerifiedHeader {
    /// The verified block hash.
    pub fn hash(&self) -> B256 {
        self.0.hash()
    }

    /// The block number.
    pub fn number(&self) -> BlockNumber {
        self.0.number
    }

    /// The full header.
    pub fn header(&self) -> &Header {
        self.0.inner()
    }

    /// Number and hash together.
    pub fn num_hash(&self) -> BlockNumHash {
        BlockNumHash::new(self.number(), self.hash())
    }

    /// Whether `self` is the direct parent of `child`.
    pub fn is_parent_of(&self, child: &Self) -> bool {
        child.header().parent_hash == self.hash() && child.number() == self.number() + 1
    }
}

/// Checks that `header` hashes to the light client's `trusted` hash.
pub fn header(trusted: TrustedHash, header: Header) -> Result<VerifiedHeader, VerifyError> {
    let sealed = header.seal_slow();
    if sealed.hash() != trusted.hash() {
        return Err(VerifyError::HeaderHash {
            expected: trusted.hash(),
            got: sealed.hash(),
        });
    }
    Ok(VerifiedHeader(sealed))
}

/// Checks that `header` is the parent of the already verified `child`.
pub fn parent(child: &VerifiedHeader, header: Header) -> Result<VerifiedHeader, VerifyError> {
    let sealed = header.seal_slow();
    let expected = child.header().parent_hash;
    if sealed.hash() != expected {
        return Err(VerifyError::HeaderHash {
            expected,
            got: sealed.hash(),
        });
    }
    Ok(VerifiedHeader(sealed))
}

/// Verifies ancestors of `child` given newest first, as a falling `GetBlockHeaders` returns
/// them. Returns them ascending, oldest first.
pub fn ancestors(
    child: &VerifiedHeader,
    headers: Vec<Header>,
) -> Result<Vec<VerifiedHeader>, VerifyError> {
    let mut verified: Vec<VerifiedHeader> = Vec::with_capacity(headers.len());
    for header in headers {
        let next = parent(verified.last().unwrap_or(child), header)?;
        verified.push(next);
    }
    verified.reverse();
    Ok(verified)
}

/// Verifies the headers strictly between `anchor` and `target`, given newest first. Returns
/// the chain ascending from the anchor's child through `target`.
///
/// A break means a reorg or a lying peer; either way the caller reanchors.
pub fn chain(
    anchor: &VerifiedHeader,
    target: VerifiedHeader,
    between: Vec<Header>,
) -> Result<Vec<VerifiedHeader>, VerifyError> {
    let mut chain = ancestors(&target, between)?;
    chain.push(target);
    let first = &chain[0];
    if !anchor.is_parent_of(first) {
        return Err(VerifyError::ChainBreak {
            anchor: anchor.hash(),
            number: first.number(),
        });
    }
    Ok(chain)
}

/// A verified header and its block access list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedBlock {
    header: VerifiedHeader,
    bal: Vec<AccountChanges>,
}

impl VerifiedBlock {
    /// The block's header.
    pub const fn header(&self) -> &VerifiedHeader {
        &self.header
    }

    /// The block access list, in canonical order.
    pub fn bal(&self) -> &[AccountChanges] {
        &self.bal
    }

    pub(crate) fn into_parts(self) -> (VerifiedHeader, Vec<AccountChanges>) {
        (self.header, self.bal)
    }
}

/// Checks one block access list against `header.block_access_list_hash`.
///
/// `raw` is one entry of a BAL reply. `None` means the peer does not have it, reported as
/// [`VerifyError::BalUnavailable`], which blames nobody.
pub fn bal(header: VerifiedHeader, raw: Option<Bytes>) -> Result<VerifiedBlock, VerifyError> {
    let block = header.hash();
    let expected = header
        .header()
        .block_access_list_hash
        .ok_or(VerifyError::NoBalHash { block })?;
    let raw = raw.ok_or(VerifyError::BalUnavailable { block })?;
    let mut buf = raw.as_ref();
    let bal = Vec::<AccountChanges>::decode(&mut buf)?;
    if !buf.is_empty() {
        return Err(VerifyError::Rlp(alloy_rlp::Error::UnexpectedLength));
    }
    let got = compute_block_access_list_hash(&bal);
    if got != expected {
        return Err(VerifyError::BalHash { expected, got });
    }
    Ok(VerifiedBlock { header, bal })
}

/// What a proof established, before it is bound to a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Fact {
    /// The account at `address`, or proven absence.
    Account {
        address: Address,
        account: Option<Account>,
    },
    /// One slot of a present account, proven at the same root as the account.
    Slot {
        address: Address,
        account: Account,
        slot: U256,
        value: U256,
    },
    /// Code whose keccak matched the requested hash.
    Code { hash: B256, code: Bytecode },
}

/// A state fact proven against a verified header's state root.
///
/// Code is content-addressed and valid at every block, so a code fact has no block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proven {
    at: Option<BlockNumHash>,
    fact: Fact,
}

impl Proven {
    /// The block whose state root the fact was proven against. `None` for code.
    pub const fn at(&self) -> Option<BlockNumHash> {
        self.at
    }

    /// The store key the fact covers. `None` for code.
    pub const fn key(&self) -> Option<Key> {
        match &self.fact {
            Fact::Account { address, .. } => Some(Key::Account(*address)),
            Fact::Slot { address, slot, .. } => Some(Key::Slot(*address, *slot)),
            Fact::Code { .. } => None,
        }
    }

    /// The proven account: `Some(None)` for proven absence, `None` for code facts.
    pub const fn account(&self) -> Option<Option<&Account>> {
        match &self.fact {
            Fact::Account { account, .. } => Some(account.as_ref()),
            Fact::Slot { account, .. } => Some(Some(account)),
            Fact::Code { .. } => None,
        }
    }

    /// The proven slot value, for slot facts.
    pub const fn slot_value(&self) -> Option<U256> {
        match &self.fact {
            Fact::Slot { value, .. } => Some(*value),
            Fact::Account { .. } | Fact::Code { .. } => None,
        }
    }

    pub(crate) const fn fact(&self) -> &Fact {
        &self.fact
    }
}

/// One single-key `GetAccountRange` bound to the root it is verified against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountQuery {
    at: BlockNumHash,
    root: B256,
    address: Address,
    key: B256,
}

impl AccountQuery {
    /// A query for `address` at the state root of `at`.
    pub fn new(at: &VerifiedHeader, address: Address) -> Self {
        Self {
            at: at.num_hash(),
            root: at.header().state_root,
            address,
            key: keccak256(address),
        }
    }

    /// The request to send: origin and limit are both `keccak256(address)`.
    pub const fn request(&self) -> AccountRangeRequest {
        AccountRangeRequest {
            root: self.root,
            key: self.key,
        }
    }

    /// Verifies `reply` and returns the account or its proven absence.
    pub fn verify(&self, reply: AccountReply) -> Result<Proven, VerifyError> {
        let account = self.prove(reply)?;
        Ok(Proven {
            at: Some(self.at),
            fact: Fact::Account {
                address: self.address,
                account,
            },
        })
    }

    fn prove(&self, reply: AccountReply) -> Result<Option<Account>, VerifyError> {
        let leaf = single_key(self.root, self.key, reply.accounts, &reply.proof)?;
        Ok(leaf.map(|account| Account::from(&account)))
    }
}

/// One slot, proven with its account at the same state root.
///
/// The account and storage requests are independent and can be sent concurrently: peers
/// locate the storage trie through the state root, so the storage request needs no storage
/// root up front.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageQuery {
    account: AccountQuery,
    slot: U256,
    slot_key: B256,
}

impl StorageQuery {
    /// A query for `slot` of `address` at the state root of `at`.
    pub fn new(at: &VerifiedHeader, address: Address, slot: U256) -> Self {
        Self {
            account: AccountQuery::new(at, address),
            slot,
            slot_key: keccak256(B256::from(slot)),
        }
    }

    /// The account half of the query.
    pub const fn account_request(&self) -> AccountRangeRequest {
        self.account.request()
    }

    /// The storage half: origin and limit are both `keccak256(slot)`.
    pub const fn storage_request(&self) -> StorageRangeRequest {
        StorageRangeRequest {
            root: self.account.root,
            account: self.account.key,
            slot: self.slot_key,
        }
    }

    /// Verifies both replies. An absent account, or one with an empty storage root, makes the
    /// slot a known zero, and `storage` is not consulted.
    pub fn verify(
        &self,
        account: AccountReply,
        storage: Option<StorageReply>,
    ) -> Result<Proven, VerifyError> {
        let at = Some(self.account.at);
        let address = self.account.address;
        let Some(leaf) = single_key(
            self.account.root,
            self.account.key,
            account.accounts,
            &account.proof,
        )?
        else {
            return Ok(Proven {
                at,
                fact: Fact::Account {
                    address,
                    account: None,
                },
            });
        };
        let value = if leaf.storage_root == EMPTY_ROOT_HASH {
            U256::ZERO
        } else {
            let storage = storage.ok_or(VerifyError::RootUnavailable {
                root: leaf.storage_root,
            })?;
            if storage.slots.iter().any(|(_, value)| value.is_zero()) {
                return Err(VerifyError::ZeroSlot);
            }
            single_key(
                leaf.storage_root,
                self.slot_key,
                storage.slots,
                &storage.proof,
            )?
            .unwrap_or_default()
        };
        let account = Account::from(&leaf);
        Ok(Proven {
            at,
            fact: Fact::Slot {
                address,
                account,
                slot: self.slot,
                value,
            },
        })
    }
}

/// Code by hash. Code is content-addressed, so the reply is checked by keccak alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeQuery {
    hash: B256,
}

impl CodeQuery {
    /// A query for the code hashing to `hash`.
    pub const fn new(hash: B256) -> Self {
        Self { hash }
    }

    /// The code hash to request.
    pub const fn request(&self) -> B256 {
        self.hash
    }

    /// Checks `code` against the hash. `None` means the peer did not return it.
    pub fn verify(&self, code: Option<Bytes>) -> Result<Proven, VerifyError> {
        let code = code.ok_or(VerifyError::CodeUnavailable { hash: self.hash })?;
        let got = keccak256(&code);
        if got != self.hash {
            return Err(VerifyError::CodeHash {
                expected: self.hash,
                got,
            });
        }
        let code = Bytecode::new_raw_checked(code).map_err(|_| VerifyError::Bytecode)?;
        Ok(Proven {
            at: None,
            fact: Fact::Code {
                hash: self.hash,
                code,
            },
        })
    }
}

/// Verifies a single-key range reply against `root`, mirroring reth's snap verifiers.
///
/// A reply may carry one boundary leaf past `key`. A leaf equal to `key` is presence; a
/// verified reply without one is proven absence. An empty reply with no proof is a peer that
/// lacks the root, never absence, unless the root is the empty trie.
fn single_key<V: Encodable>(
    root: B256,
    key: B256,
    leaves: Vec<(B256, V)>,
    proof: &[Bytes],
) -> Result<Option<V>, VerifyError> {
    if leaves.is_empty() && proof.is_empty() {
        return if root == EMPTY_ROOT_HASH {
            Ok(None)
        } else {
            Err(VerifyError::RootUnavailable { root })
        };
    }
    if leaves
        .iter()
        .filter(|(hash, _)| *hash > key)
        .nth(1)
        .is_some()
    {
        return Err(VerifyError::PastLimit { root });
    }
    let encoded = leaves
        .iter()
        .map(|(hash, value)| (*hash, alloy_rlp::encode(value)));
    verify_range_proof(root, key, key, encoded, proof)
        .map_err(|source| VerifyError::RangeProof { root, source })?;
    Ok(leaves
        .into_iter()
        .find(|(hash, _)| *hash == key)
        .map(|(_, value)| value))
}

/// A block's transactions, checked against the header's transaction, ommers, and withdrawals
/// roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedBody {
    header: VerifiedHeader,
    body: wire::Body,
}

impl VerifiedBody {
    /// The block's header.
    pub const fn header(&self) -> &VerifiedHeader {
        &self.header
    }

    /// The block's transactions in order.
    pub fn transactions(&self) -> &[wire::Transaction] {
        &self.body.transactions
    }
}

/// Checks `body` against the roots in `header`.
pub fn body(header: VerifiedHeader, body: wire::Body) -> Result<VerifiedBody, VerifyError> {
    let block = header.hash();
    let h = header.header();
    if calculate_transaction_root(&body.transactions) != h.transactions_root {
        return Err(VerifyError::Root {
            block,
            part: BlockPart::Transactions,
        });
    }
    if calculate_ommers_root(&body.ommers) != h.ommers_hash {
        return Err(VerifyError::Root {
            block,
            part: BlockPart::Ommers,
        });
    }
    let withdrawals_match = match (h.withdrawals_root, &body.withdrawals) {
        (Some(root), Some(withdrawals)) => calculate_withdrawals_root(withdrawals) == root,
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    };
    if !withdrawals_match {
        return Err(VerifyError::Root {
            block,
            part: BlockPart::Withdrawals,
        });
    }
    Ok(VerifiedBody { header, body })
}

/// A block's receipts, checked against the header's receipts root and logs bloom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedReceipts {
    header: VerifiedHeader,
    receipts: Vec<wire::Receipt>,
}

impl VerifiedReceipts {
    /// The block's header.
    pub const fn header(&self) -> &VerifiedHeader {
        &self.header
    }

    /// The receipts in transaction order.
    pub fn receipts(&self) -> &[wire::Receipt] {
        &self.receipts
    }

    /// The receipt view for the transaction at `index`.
    ///
    /// `None` if `body` is of a different block or `index` is out of range.
    pub fn receipt(&self, body: &VerifiedBody, index: usize) -> Option<VerifiedReceipt> {
        if body.header.hash() != self.header.hash() {
            return None;
        }
        let tx = body.transactions().get(index)?;
        let receipt = self.receipts.get(index)?;
        let previous = &self.receipts[..index];
        let prior_gas = previous.last().map_or(0, |r| r.cumulative_gas_used);
        let first_log_index = previous.iter().map(|r| r.logs.len() as u64).sum();
        Some(VerifiedReceipt::new(
            &self.header,
            index as u64,
            tx,
            receipt,
            receipt.cumulative_gas_used.saturating_sub(prior_gas),
            first_log_index,
        ))
    }
}

/// Checks eth/69 receipts against `header`. Blooms are recomputed because eth/69 carries none.
pub fn receipts(
    header: VerifiedHeader,
    receipts: Vec<wire::Receipt>,
) -> Result<VerifiedReceipts, VerifyError> {
    let block = header.hash();
    if wire::Receipt::calculate_receipt_root_no_memo(&receipts) != header.header().receipts_root {
        return Err(VerifyError::Root {
            block,
            part: BlockPart::Receipts,
        });
    }
    let bloom = receipts.iter().fold(Bloom::ZERO, |mut acc, receipt| {
        acc.accrue_bloom(&receipt.bloom());
        acc
    });
    if bloom != header.header().logs_bloom {
        return Err(VerifyError::Root {
            block,
            part: BlockPart::LogsBloom,
        });
    }
    Ok(VerifiedReceipts { header, receipts })
}

/// The header commitment a body or receipt list failed to match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockPart {
    /// `transactions_root`.
    Transactions,
    /// `ommers_hash`.
    Ommers,
    /// `withdrawals_root`, including presence.
    Withdrawals,
    /// `receipts_root`.
    Receipts,
    /// `logs_bloom`.
    LogsBloom,
}

/// A peer reply failed verification. The sender lied or the reply was garbled, except for the
/// `*Unavailable` variants, which mean the peer lacks the data and blame nobody.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    /// A header does not hash to the trusted or linked hash.
    #[error("header hash {got} does not match expected {expected}")]
    HeaderHash {
        /// Hash the header had to match.
        expected: B256,
        /// Hash the header actually has.
        got: B256,
    },
    /// The verified chain does not descend from the anchor.
    #[error("header {number} does not link to anchor {anchor}")]
    ChainBreak {
        /// Hash of the anchor the chain had to extend.
        anchor: B256,
        /// Number of the oldest header in the chain.
        number: BlockNumber,
    },
    /// The peer answered with neither leaves nor proof: it does not serve this root.
    #[error("peer does not serve state root {root}")]
    RootUnavailable {
        /// Root the request targeted.
        root: B256,
    },
    /// The reply carried more than one leaf past the requested key.
    #[error("range reply against {root} runs past the requested key")]
    PastLimit {
        /// Root the reply was checked against.
        root: B256,
    },
    /// A storage reply carried a zero value, which is never a trie leaf.
    #[error("storage reply carries a zero slot value")]
    ZeroSlot,
    /// The range proof does not authenticate the leaves against the root.
    #[error("range proof against {root} is invalid: {source}")]
    RangeProof {
        /// Root the reply was checked against.
        root: B256,
        /// Why the proof failed.
        #[source]
        source: RangeProofError,
    },
    /// The peer did not return the requested code.
    #[error("peer does not have code {hash}")]
    CodeUnavailable {
        /// Requested code hash.
        hash: B256,
    },
    /// Code does not hash to the requested hash.
    #[error("code hashes to {got}, expected {expected}")]
    CodeHash {
        /// Requested code hash.
        expected: B256,
        /// Hash of the returned code.
        got: B256,
    },
    /// Code with a matching hash is not valid bytecode.
    #[error("invalid bytecode")]
    Bytecode,
    /// The header predates block access lists.
    #[error("header {block} has no block access list hash")]
    NoBalHash {
        /// Block hash.
        block: B256,
    },
    /// The peer does not have the block access list.
    #[error("peer has no block access list for {block}")]
    BalUnavailable {
        /// Block hash.
        block: B256,
    },
    /// The block access list does not hash to the header's commitment.
    #[error("block access list hashes to {got}, expected {expected}")]
    BalHash {
        /// `block_access_list_hash` from the header.
        expected: B256,
        /// Hash of the returned list.
        got: B256,
    },
    /// A body or receipt list does not match a header commitment.
    #[error("{part:?} mismatch in block {block}")]
    Root {
        /// Block hash.
        block: B256,
        /// The commitment that failed.
        part: BlockPart,
    },
    /// Malformed RLP.
    #[error("malformed RLP: {0}")]
    Rlp(#[from] alloy_rlp::Error),
}

impl VerifyError {
    /// Whether the reply proves the peer sent bad data. A peer lacking data, a header without
    /// a BAL hash, and a chain break (which a reorg also causes) blame nobody.
    pub const fn blames_peer(&self) -> bool {
        match self {
            Self::RootUnavailable { .. }
            | Self::CodeUnavailable { .. }
            | Self::BalUnavailable { .. }
            | Self::NoBalHash { .. }
            | Self::ChainBreak { .. } => false,
            Self::HeaderHash { .. }
            | Self::PastLimit { .. }
            | Self::ZeroSlot
            | Self::RangeProof { .. }
            | Self::CodeHash { .. }
            | Self::Bytecode
            | Self::BalHash { .. }
            | Self::Root { .. }
            | Self::Rlp(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{BlockWrites, World, WorldAccount, eth};
    use alloy_primitives::address;

    const ALICE: Address = address!("0x00000000000000000000000000000000000a11ce");
    const TOKEN: Address = address!("0x0000000000000000000000000000000000070c3e");
    const NOBODY: Address = address!("0x000000000000000000000000000000000000dead");

    fn world() -> World {
        let fillers = (1u64..=32).map(|i| {
            (
                Address::left_padding_from(&(0x1000 + i).to_be_bytes()),
                WorldAccount::eoa(U256::from(i)),
            )
        });
        let token = WorldAccount::contract(vec![0x60, 0x00, 0x54, 0x00])
            .with_slot(U256::from(1), U256::from(11))
            .with_slot(U256::from(2), U256::from(22))
            .with_slot(U256::from(3), U256::from(33));
        World::genesis(fillers.chain([
            (ALICE, WorldAccount::eoa(eth(5)).with_nonce(7)),
            (TOKEN, token),
        ]))
    }

    fn account_reply(world: &World, query: &AccountQuery) -> AccountReply {
        world.account_reply(&query.request())
    }

    #[test]
    fn present_account_matches_world() {
        let world = world();
        let head = world.verified(world.head());
        let query = AccountQuery::new(&head, ALICE);
        let proven = query.verify(account_reply(&world, &query)).unwrap();
        let expected = Account {
            nonce: 7,
            balance: eth(5),
            ..Account::default()
        };
        assert_eq!(
            (proven.at(), proven.key(), proven.account()),
            (
                Some(head.num_hash()),
                Some(Key::Account(ALICE)),
                Some(Some(&expected))
            )
        );
    }

    #[test]
    fn absent_account_between_neighbors_is_known_absent() {
        let world = world();
        let head = world.verified(world.head());
        let query = AccountQuery::new(&head, NOBODY);
        let reply = account_reply(&world, &query);
        assert!(reply.accounts.len() == 1 && reply.accounts[0].0 > query.request().key);
        assert_eq!(query.verify(reply).unwrap().account(), Some(None));
    }

    #[test]
    fn storage_present_absent_and_empty_root() {
        let world = world();
        let head = world.verified(world.head());
        let slot = |address, slot: u64| {
            let query = StorageQuery::new(&head, address, U256::from(slot));
            let account = world.account_reply(&query.account_request());
            let storage = world.storage_reply(&query.storage_request());
            query.verify(account, Some(storage)).unwrap()
        };
        assert_eq!(slot(TOKEN, 2).slot_value(), Some(U256::from(22)));
        assert_eq!(slot(TOKEN, 9).slot_value(), Some(U256::ZERO));
        let absent = slot(NOBODY, 1);
        assert_eq!((absent.account(), absent.slot_value()), (Some(None), None));

        let query = StorageQuery::new(&head, ALICE, U256::from(1));
        let eoa = query
            .verify(world.account_reply(&query.account_request()), None)
            .unwrap();
        assert_eq!(eoa.slot_value(), Some(U256::ZERO));
        assert_eq!(eoa.key(), Some(Key::Slot(ALICE, U256::from(1))));
    }

    #[test]
    fn tampered_account_is_rejected() {
        let world = world();
        let head = world.verified(world.head());
        let query = AccountQuery::new(&head, ALICE);
        let mut reply = account_reply(&world, &query);
        reply.accounts[0].1.balance += U256::from(1);
        assert!(matches!(
            query.verify(reply),
            Err(VerifyError::RangeProof { .. })
        ));
    }

    #[test]
    fn tampered_slot_is_rejected() {
        let world = world();
        let head = world.verified(world.head());
        let query = StorageQuery::new(&head, TOKEN, U256::from(2));
        let account = world.account_reply(&query.account_request());
        let mut storage = world.storage_reply(&query.storage_request());
        storage.slots[0].1 = U256::from(23);
        assert!(matches!(
            query.verify(account, Some(storage)),
            Err(VerifyError::RangeProof { .. })
        ));
    }

    #[test]
    fn proof_against_another_root_is_rejected() {
        let mut world = world();
        let old_root = world.header(world.head()).state_root;
        let hash = world.mine(BlockWrites::default().balance(ALICE, eth(1)));
        let query = AccountQuery::new(&world.verified(hash), ALICE);
        let stale = world.account_reply(&AccountRangeRequest {
            root: old_root,
            ..query.request()
        });
        assert!(matches!(
            query.verify(stale),
            Err(VerifyError::RangeProof { .. })
        ));
    }

    #[test]
    fn dropped_proof_node_is_rejected() {
        let world = world();
        let head = world.verified(world.head());
        for address in [ALICE, NOBODY] {
            let query = AccountQuery::new(&head, address);
            let reply = account_reply(&world, &query);
            for dropped in 0..reply.proof.len() {
                let mut forged = reply.clone();
                forged.proof.remove(dropped);
                let error = query.verify(forged).unwrap_err();
                assert!(
                    error.blames_peer(),
                    "dropping node {dropped} gave {error:?}"
                );
            }
        }
    }

    #[test]
    fn withheld_leaf_cannot_fake_absence() {
        let world = world();
        let head = world.verified(world.head());
        let query = AccountQuery::new(&head, ALICE);
        let key = query.request().key;
        let past = B256::from(U256::from_be_bytes(key.0) + U256::from(1));
        let hiding = world.account_reply(&AccountRangeRequest {
            key: past,
            ..query.request()
        });
        assert!(matches!(
            query.verify(hiding),
            Err(VerifyError::RangeProof { .. })
        ));
    }

    #[test]
    fn extra_leaves_past_the_key_are_rejected() {
        let world = world();
        let head = world.verified(world.head());
        let (query, mut reply, next) = (0u64..)
            .map(|i| {
                AccountQuery::new(
                    &head,
                    Address::left_padding_from(&(0xdead_0000 + i).to_be_bytes()),
                )
            })
            .find_map(|query| {
                let reply = account_reply(&world, &query);
                let boundary = reply.accounts.first()?.0;
                let next = world.account_reply(&AccountRangeRequest {
                    key: B256::from(U256::from_be_bytes(boundary.0) + U256::from(1)),
                    ..query.request()
                });
                (!next.accounts.is_empty()).then_some((query, reply, next))
            })
            .unwrap();
        reply.accounts.extend(next.accounts);
        assert_eq!(
            query.verify(reply),
            Err(VerifyError::PastLimit {
                root: head.header().state_root
            })
        );
    }

    #[test]
    fn empty_reply_is_unavailable_not_absence() {
        let world = world();
        let head = world.verified(world.head());
        let root = head.header().state_root;
        let query = AccountQuery::new(&head, NOBODY);
        let error = query.verify(AccountReply::default()).unwrap_err();
        assert_eq!(error, VerifyError::RootUnavailable { root });
        assert!(!error.blames_peer());

        let query = StorageQuery::new(&head, TOKEN, U256::from(1));
        let account = world.account_reply(&query.account_request());
        assert!(matches!(
            query.verify(account.clone(), Some(StorageReply::default())),
            Err(VerifyError::RootUnavailable { .. })
        ));
        assert!(matches!(
            query.verify(account, None),
            Err(VerifyError::RootUnavailable { .. })
        ));
    }

    #[test]
    fn code_is_checked_by_hash() {
        let world = world();
        let code = world.account(TOKEN).unwrap().code.clone();
        let hash = keccak256(&code);
        let query = CodeQuery::new(hash);
        assert_eq!(query.verify(Some(code)).unwrap().at(), None);
        let forged = Bytes::from_static(&[0x00]);
        assert_eq!(
            query.verify(Some(forged.clone())),
            Err(VerifyError::CodeHash {
                expected: hash,
                got: keccak256(&forged)
            })
        );
        assert_eq!(
            query.verify(None),
            Err(VerifyError::CodeUnavailable { hash })
        );
    }

    #[test]
    fn chain_links_gap_headers_ascending() {
        let mut world = world();
        let anchor = world.verified(world.head());
        let hashes: Vec<B256> = (1..=3)
            .map(|i| world.mine(BlockWrites::default().balance(ALICE, eth(i))))
            .collect();
        let between = world.headers_between(anchor.hash(), hashes[2]);

        let linked = chain(&anchor, world.verified(hashes[2]), between.clone()).unwrap();
        assert_eq!(
            linked.iter().map(VerifiedHeader::hash).collect::<Vec<_>>(),
            hashes
        );

        let missing_middle = between[1..].to_vec();
        assert_eq!(
            chain(&anchor, world.verified(hashes[2]), missing_middle).unwrap_err(),
            VerifyError::HeaderHash {
                expected: hashes[1],
                got: hashes[0]
            }
        );
        assert_eq!(
            chain(&anchor, world.verified(hashes[2]), Vec::new()).unwrap_err(),
            VerifyError::ChainBreak {
                anchor: anchor.hash(),
                number: anchor.number() + 3
            }
        );
    }

    #[test]
    fn body_and_receipts_are_checked_against_roots() {
        let mut world = world();
        let tx = crate::testing::legacy_transfer(7, NOBODY, eth(1));
        let hash = world.mine(BlockWrites::default().tx(tx, 21_000, 2));
        let header = world.verified(hash);

        let mut body = world.body(hash);
        body.transactions.clear();
        assert_eq!(
            super::body(header.clone(), body).unwrap_err(),
            VerifyError::Root {
                block: hash,
                part: BlockPart::Transactions
            }
        );
        let mut receipts = world.receipts(hash);
        receipts[0].cumulative_gas_used += 1;
        assert_eq!(
            super::receipts(header, receipts).unwrap_err(),
            VerifyError::Root {
                block: hash,
                part: BlockPart::Receipts
            }
        );
    }
}
