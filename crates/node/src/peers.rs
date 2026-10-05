//! The network seam. [`Peers`] carries untrusted replies in [`chibi_core::wire`] shapes and
//! verifies nothing. The devp2p adapter implements it over reth-network; the test fixture
//! implements it over in-process tries, so offline tests run the production verifier.

use alloy_consensus::Header;
use alloy_primitives::{B256, B512, Bytes};
use chibi_core::VerifiedHeader;
use chibi_core::wire::{
    AccountRangeRequest, AccountReply, Body, Receipt, StorageRangeRequest, StorageReply,
    Transaction,
};

/// Identifies the peer that sent a reply, so a failed verification can penalize it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeerKey(pub B512);

/// A reply and the peer that sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply<T> {
    /// The sender.
    pub peer: PeerKey,
    /// The untrusted payload.
    pub value: T,
}

/// Untrusted access to the peer-to-peer network.
///
/// Every request goes to one connected peer the adapter picks. Callers verify each reply
/// through `chibi_core::verify` and call [`Peers::penalize`] when verification fails.
pub trait Peers: Send + Sync + 'static {
    /// Up to `count` headers walking parent links from `start`, newest first.
    fn headers(
        &self,
        start: B256,
        count: u64,
    ) -> impl Future<Output = Result<Reply<Vec<Header>>, NetError>> + Send;

    /// A single-key snap/2 `GetAccountRange`.
    fn account_range(
        &self,
        request: AccountRangeRequest,
    ) -> impl Future<Output = Result<Reply<AccountReply>, NetError>> + Send;

    /// A single-key snap/2 `GetStorageRanges` for one account.
    fn storage_range(
        &self,
        request: StorageRangeRequest,
    ) -> impl Future<Output = Result<Reply<StorageReply>, NetError>> + Send;

    /// Contract code by hash (snap/2 `GetByteCodes`).
    fn byte_codes(
        &self,
        hashes: Vec<B256>,
    ) -> impl Future<Output = Result<Reply<Vec<Bytes>>, NetError>> + Send;

    /// Raw RLP block access lists by block hash, `None` where the peer lacks one. Tries snap/2
    /// then eth/71, and fails fast instead of waiting when no peer offers either.
    fn block_access_lists(
        &self,
        blocks: Vec<B256>,
    ) -> impl Future<Output = Result<Reply<Vec<Option<Bytes>>>, NetError>> + Send;

    /// Block bodies by block hash.
    fn bodies(
        &self,
        blocks: Vec<B256>,
    ) -> impl Future<Output = Result<Reply<Vec<Body>>, NetError>> + Send;

    /// All receipts of one block.
    fn receipts(
        &self,
        block: B256,
    ) -> impl Future<Output = Result<Reply<Vec<Receipt>>, NetError>> + Send;

    /// Sends `transaction` in full to several connected peers. Returns how many received it.
    fn broadcast(
        &self,
        transaction: Transaction,
    ) -> impl Future<Output = Result<usize, NetError>> + Send;

    /// Reports a peer whose reply failed verification.
    fn penalize(&self, peer: PeerKey);

    /// Advertises `head` as chibi's latest block in its status, keeping the fork id current.
    fn announce(&self, head: &VerifiedHeader);
}

/// A request could not be completed. None of these say anything about chain state.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NetError {
    /// No connected peer offers the capability the request needs.
    #[error("no connected peer serves {capability}")]
    NoPeer {
        /// The missing capability, such as `snap/2` or `eth/71`.
        capability: &'static str,
    },
    /// The peer did not answer in time.
    #[error("request timed out")]
    Timeout,
    /// The peer answered with a protocol error or closed the session.
    #[error("peer rejected the request")]
    Rejected,
    /// The network task has stopped.
    #[error("network stopped")]
    Stopped,
}
