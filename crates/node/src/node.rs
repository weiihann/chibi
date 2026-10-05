//! The request-side node API. Every method pins one verified head for its whole run.
//!
//! This file is the contract between the node implementation and the JSON-RPC layer. Bodies
//! marked `todo!()` are filled in by the node work; signatures change only with the lead.

use std::path::{Path, PathBuf};

use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, BlockNumber, Bytes, TxHash, U256};
use chibi_core::VerifiedHeader;
use chibi_core::exec::{CallRequest, EstimateError, ExecError, ExecutionResult, HaltReason};
use chibi_core::state::{Account, StateError};
use chibi_core::tx::VerifiedReceipt;
use chibi_core::verify::VerifyError;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::beacon::BeaconError;
use crate::devp2p::{Devp2p, Devp2pError, Ports};
use crate::network::{Network, NetworkSpec};
use crate::peers::{NetError, Peers};

/// Everything `Node::start` needs. The CLI builds it from the config file and flags.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// The chain to follow.
    pub network: Network,
    /// Per-network data directory: helios checkpoint, devp2p key, peers, and the state file.
    pub data_dir: PathBuf,
    /// Beacon API endpoint. `None` uses the network's default.
    pub beacon_url: Option<Url>,
    /// Weak-subjectivity checkpoint override (a finalized beacon block root).
    pub checkpoint: Option<B256>,
    /// Watch-only addresses whose accounts and token balances are tracked.
    pub watch: Vec<Address>,
    /// devp2p listen ports.
    pub ports: Ports,
}

/// Which block a request reads. Only verified heads chibi holds can be served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockSel {
    /// The anchor.
    Latest,
    /// The anchor, plus chibi-submitted pending transactions where that matters (nonces).
    Pending,
    /// The light client's finalized header. Block queries only.
    Safe,
    /// The light client's finalized header. Block queries only.
    Finalized,
    /// A block by number. Served only for headers chibi holds and only for block queries,
    /// except the anchor's own number, which also serves state.
    Number(BlockNumber),
    /// A block by hash, with the same limits as [`BlockSel::Number`].
    Hash(B256),
}

/// Handle to a running node. Cheap to clone; every clone shares one state.
#[derive(Debug)]
pub struct Node<P: Peers = Devp2p> {
    _peers: std::marker::PhantomData<P>,
}

impl<P: Peers> Clone for Node<P> {
    fn clone(&self) -> Self {
        Self {
            _peers: std::marker::PhantomData,
        }
    }
}

/// The only task that advances the store, reanchors it, and writes the state file.
/// Not `Clone`: exactly one exists per node.
#[derive(Debug)]
pub struct Follower<P: Peers = Devp2p> {
    _peers: std::marker::PhantomData<P>,
}

/// Fee data for `eth_feeHistory`, oldest block first.
#[derive(Clone, Debug, PartialEq)]
pub struct FeeHistory {
    /// Number of the oldest block in the window.
    pub oldest_block: BlockNumber,
    /// Base fee per gas for each block, plus the next block's.
    pub base_fee_per_gas: Vec<u128>,
    /// `gas_used / gas_limit` for each block.
    pub gas_used_ratio: Vec<f64>,
    /// Blob base fee for each block, plus the next block's.
    pub base_fee_per_blob_gas: Vec<u128>,
    /// `blob_gas_used / max_blob_gas` for each block.
    pub blob_gas_used_ratio: Vec<f64>,
    /// Effective priority fee at each requested percentile, per block.
    pub reward: Option<Vec<Vec<u128>>>,
}

/// A transaction chibi submitted, and where it was included once located.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmittedTx {
    /// The signed transaction as broadcast.
    pub transaction: Transaction,
    /// The sender recovered at submission.
    pub from: Address,
    /// The including block and the transaction's index in it, once located in a verified body.
    pub included: Option<(BlockNumHash, u64)>,
}

/// A verified receipt joined with what chibi recorded at submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxReceipt {
    /// The receipt, checked against its block's `receipts_root`.
    pub receipt: VerifiedReceipt,
    /// The sender recovered at submission.
    pub from: Address,
    /// The recipient, `None` for contract creation.
    pub to: Option<Address>,
    /// The created contract, for contract creation.
    pub contract_address: Option<Address>,
}

/// A watched address and its tracked balances at one block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holding {
    /// The watched address.
    pub address: Address,
    /// ETH balance in wei, `None` if not yet proven.
    pub balance: Option<U256>,
    /// Account nonce, `None` if not yet proven.
    pub nonce: Option<u64>,
    /// Token balances in base units by token symbol, `None` if not yet computed.
    pub tokens: Vec<(String, Option<U256>)>,
}

/// What `chibi status` shows. Written by the follower at each head and readable without a
/// running node. Values in a saved snapshot are display-only and never re-enter the store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The chain the snapshot belongs to.
    pub network: Network,
    /// The verified block the values hold at.
    pub block: BlockNumHash,
    /// Unix time the snapshot was written.
    pub saved_at: u64,
    /// One entry per watched address.
    pub holdings: Vec<Holding>,
}

/// Live node status for `chibi status` and logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The current snapshot at the anchor.
    pub snapshot: Snapshot,
    /// Connected devp2p sessions.
    pub peers: usize,
    /// Connected sessions that negotiated snap/2.
    pub snap_peers: usize,
}

/// What one follower step did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stepped {
    /// The light client has not moved past the anchor.
    Current,
    /// BALs moved the anchor forward to this block.
    Extended(BlockNumHash),
    /// Tracked keys were re-proven at this block.
    Reanchored(BlockNumHash),
}

impl Node<Devp2p> {
    /// Opens the data dir (taking its lock), starts the light client and devp2p, and returns
    /// once the state is anchored at a fresh verified head with every tracked key proven.
    pub async fn start(config: NodeConfig) -> Result<(Self, Follower<Devp2p>), NodeError> {
        let _ = config;
        todo!()
    }
}

impl<P: Peers> Node<P> {
    /// The chain id served by `eth_chainId`.
    pub fn chain_id(&self) -> u64 {
        todo!()
    }

    /// The registry entry for the followed chain.
    pub fn network(&self) -> &NetworkSpec {
        todo!()
    }

    /// The anchor header.
    pub fn head(&self) -> VerifiedHeader {
        todo!()
    }

    /// A held header. `Safe` and `Finalized` give the finalized header.
    pub fn header(&self, block: BlockSel) -> Result<VerifiedHeader, NodeError> {
        let _ = block;
        todo!()
    }

    /// The account at `block`, `None` if proven absent. Fetches and proves it on a miss.
    pub async fn account(
        &self,
        address: Address,
        block: BlockSel,
    ) -> Result<Option<Account>, NodeError> {
        let _ = (address, block);
        todo!()
    }

    /// A storage slot at `block`. Fetches and proves it on a miss.
    pub async fn storage(
        &self,
        address: Address,
        slot: U256,
        block: BlockSel,
    ) -> Result<U256, NodeError> {
        let _ = (address, slot, block);
        todo!()
    }

    /// The account's code at `block`, empty for accounts without code.
    pub async fn code(&self, address: Address, block: BlockSel) -> Result<Bytes, NodeError> {
        let _ = (address, block);
        todo!()
    }

    /// The account nonce. `Pending` adds chibi-submitted transactions not yet included.
    pub async fn transaction_count(
        &self,
        address: Address,
        block: BlockSel,
    ) -> Result<u64, NodeError> {
        let _ = (address, block);
        todo!()
    }

    /// Runs `request` locally without committing anything.
    pub async fn call(
        &self,
        request: CallRequest,
        block: BlockSel,
    ) -> Result<ExecutionResult<HaltReason>, NodeError> {
        let _ = (request, block);
        todo!()
    }

    /// Estimates gas for `request` with reth's bisection.
    pub async fn estimate_gas(
        &self,
        request: CallRequest,
        block: BlockSel,
    ) -> Result<u64, NodeError> {
        let _ = (request, block);
        todo!()
    }

    /// Suggested legacy gas price: next base fee plus the suggested priority fee.
    pub async fn gas_price(&self) -> Result<u128, NodeError> {
        todo!()
    }

    /// Suggested priority fee from tips in recent verified bodies.
    pub async fn max_priority_fee(&self) -> Result<u128, NodeError> {
        todo!()
    }

    /// Fee history over up to `blocks` held headers ending at `newest`.
    pub async fn fee_history(
        &self,
        blocks: u64,
        newest: BlockSel,
        percentiles: &[f64],
    ) -> Result<FeeHistory, NodeError> {
        let _ = (blocks, newest, percentiles);
        todo!()
    }

    /// Decodes an EIP-2718 transaction, checks its chain id, records it, and broadcasts it.
    pub async fn submit(&self, raw: &[u8]) -> Result<TxHash, NodeError> {
        let _ = raw;
        todo!()
    }

    /// The receipt of a chibi-submitted transaction, `None` while pending or unknown.
    pub fn receipt(&self, hash: TxHash) -> Result<Option<TxReceipt>, NodeError> {
        let _ = hash;
        todo!()
    }

    /// A chibi-submitted transaction, `None` if chibi did not submit it.
    pub fn transaction(&self, hash: TxHash) -> Result<Option<SubmittedTx>, NodeError> {
        let _ = hash;
        todo!()
    }

    /// Current status.
    pub fn status(&self) -> Status {
        todo!()
    }
}

impl<P: Peers> Follower<P> {
    /// Follows the light client until the node shuts down.
    pub async fn run(self) -> Result<(), NodeError> {
        todo!()
    }

    /// Moves to the newest attested head once, then persists the state file.
    pub async fn step(&mut self) -> Result<Stepped, NodeError> {
        todo!()
    }
}

/// Reads the last saved snapshot without starting a node or taking the data-dir lock.
pub fn read_snapshot(data_dir: &Path) -> Result<Option<Snapshot>, NodeError> {
    let _ = data_dir;
    todo!()
}

/// Why a node operation failed.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// The light client failed or its checkpoint expired.
    #[error(transparent)]
    Beacon(#[from] BeaconError),
    /// devp2p could not start.
    #[error(transparent)]
    Devp2p(#[from] Devp2pError),
    /// The data directory could not be read, written, or locked.
    #[error("data directory: {0}")]
    Disk(String),
    /// A network request failed after retries.
    #[error(transparent)]
    Net(#[from] NetError),
    /// Every peer asked returned an unverifiable reply.
    #[error("no peer returned a verifiable reply: {0}")]
    Unprovable(VerifyError),
    /// A state transition was refused.
    #[error(transparent)]
    State(#[from] StateError),
    /// Local execution failed before producing a result.
    #[error(transparent)]
    Exec(#[from] ExecError),
    /// Gas estimation failed.
    #[error(transparent)]
    Estimate(#[from] EstimateError),
    /// A request kept discovering new misses past the round limit.
    #[error("execution needed more than {rounds} rounds of fetches")]
    TooManyMissRounds {
        /// The limit that was hit.
        rounds: u32,
    },
    /// The request needs data chibi does not keep, such as history or logs.
    #[error("{what} is not supported by chibi (verified head is {head})")]
    Unsupported {
        /// What was asked for.
        what: &'static str,
        /// The anchor's block number.
        head: BlockNumber,
    },
    /// A submitted transaction was malformed or for another chain.
    #[error("invalid transaction: {0}")]
    InvalidTransaction(&'static str),
    /// The node is stopping.
    #[error("node stopped")]
    Stopped,
    /// The light client has not produced a head yet.
    #[error("no verified head yet")]
    NotReady,
}
