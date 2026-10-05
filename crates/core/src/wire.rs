//! Untrusted requests and replies exchanged with peers, in chibi's own shapes.
//!
//! The network adapter converts protocol messages into these types, so protocol crates never
//! reach the domain or the test seam. Nothing here is verified; [`crate::verify`] is the only
//! consumer of replies.

use alloy_consensus::{BlockBody, EthereumReceipt, EthereumTxEnvelope, TxEip4844};
use alloy_primitives::{B256, Bytes, U256};
use alloy_trie::TrieAccount;

/// A signed transaction as it travels on the `eth` wire. Blob sidecars are not carried.
pub type Transaction = EthereumTxEnvelope<TxEip4844>;

/// A block body as returned by `GetBlockBodies`.
pub type Body = BlockBody<Transaction>;

/// A receipt as returned by eth/69 and later, without a bloom.
pub type Receipt = EthereumReceipt;

/// A single-key `GetAccountRange`: origin and limit are both `key` (the hashed address).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountRangeRequest {
    /// State root the reply must be proven against.
    pub root: B256,
    /// `keccak256(address)`.
    pub key: B256,
}

/// A single-key `GetStorageRanges` for one account: origin and limit are both `slot`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageRangeRequest {
    /// State root of the block. Peers locate the storage trie through it.
    pub root: B256,
    /// `keccak256(address)`.
    pub account: B256,
    /// `keccak256(slot)`.
    pub slot: B256,
}

/// An `AccountRange` reply: leaves in key order plus the boundary proof.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountReply {
    /// `(hashed address, account)` leaves.
    pub accounts: Vec<(B256, TrieAccount)>,
    /// Trie nodes proving the range against the state root.
    pub proof: Vec<Bytes>,
}

/// A `StorageRanges` reply for one account: leaves in key order plus the boundary proof.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageReply {
    /// `(hashed slot, value)` leaves.
    pub slots: Vec<(B256, U256)>,
    /// Trie nodes proving the range against the account's storage root.
    pub proof: Vec<Bytes>,
}
