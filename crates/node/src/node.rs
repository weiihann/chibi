//! The request-side node API. Every method pins one verified head for its whole run.
//!
//! This file is the contract between the node implementation and the JSON-RPC layer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy_consensus::Transaction as _;
use alloy_consensus::transaction::SignerRecoverable as _;
use alloy_eips::BlockNumHash;
use alloy_eips::eip2718::{Decodable2718 as _, Encodable2718 as _};
use alloy_primitives::{Address, B256, BlockNumber, Bytes, TxHash, TxKind, U256};
use chibi_core::VerifiedHeader;
use chibi_core::exec::{
    self, CallRequest, ChainEnv, EstimateError, ExecError, ExecutionResult, HaltReason, Misses,
};
use chibi_core::state::{Account, Key, Miss, State, StateError, Store};
use chibi_core::tx::{self, FeeSample, Lookup, PendingTx, VerifiedReceipt};
use chibi_core::verify::{VerifiedBody, VerifyError};
use chibi_core::wire::Transaction;
use parking_lot::Mutex;
use reth_chainspec::EthChainSpec as _;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use url::Url;

use crate::beacon::{self, Beacon, BeaconConfig, BeaconError};
use crate::devp2p::{Devp2p, Devp2pError, HeadHint, Ports};
use crate::disk::{Disk, OutboxTx, StateFile};
use crate::fetch::{self, Fetched};
use crate::follow::Search;
use crate::heads::Heads;
use crate::network::{Network, NetworkSpec, Token};
use crate::peers::{NetError, Peers};

/// Fetch rounds one request may need before it gives up.
const MISS_ROUNDS: u32 = 8;

/// Recent blocks sampled for the suggested tip, as reth's gas price oracle does.
const FEE_BLOCKS: usize = 20;

/// Lowest tips taken from each sampled block, as reth's oracle does.
const TIPS_PER_BLOCK: usize = 3;

/// Percentile of the sampled tips suggested.
const TIP_PERCENTILE: usize = 60;

/// Suggested tip when the sampled blocks carry no transactions; reth's oracle default.
const DEFAULT_TIP: u128 = 1_000_000_000;

/// Highest tip ever suggested; reth's oracle cap.
const MAX_TIP: u128 = 500_000_000_000;

/// Fee samples kept before the oldest are dropped.
const FEE_CACHE: usize = 512;

/// Most blocks `eth_feeHistory` covers, as in reth.
const MAX_FEE_HISTORY: u64 = 1024;

/// Seconds between blocks, for the next block's fee parameters.
const SLOT_SECONDS: u64 = 12;

/// Tries at fetching a community checkpoint on the first run.
const CHECKPOINT_ATTEMPTS: u32 = 4;

/// helios's checkpoint file inside the data directory.
const HELIOS_CHECKPOINT: &str = "checkpoint";

/// The length of a checkpoint helios can load; any other file makes it use its shipped default.
const CHECKPOINT_BYTES: u64 = 32;

/// How often `start` reports that it is still waiting for a snap/2 peer.
const SNAP_PEER_PROGRESS: Duration = Duration::from_secs(30);

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
    /// What to track: watch-only addresses and the tokens followed for each.
    pub watch: Watchlist,
    /// devp2p listen ports.
    pub ports: Ports,
}

/// What the user asked chibi to track.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Watchlist {
    /// Watch-only addresses whose accounts are proven and kept current.
    pub addresses: Vec<Address>,
    /// Tokens whose balances are followed for every watched address.
    pub tokens: Vec<Token>,
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
    shared: Arc<Shared<P>>,
}

impl<P: Peers> Clone for Node<P> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

/// The only task that advances the store, reanchors it, and writes the state file.
/// Not `Clone`: exactly one exists per node.
#[derive(Debug)]
pub struct Follower<P: Peers = Devp2p> {
    pub(crate) shared: Arc<Shared<P>>,
    pub(crate) heads: Heads,
    pub(crate) beacon: Option<Beacon>,
    /// Heads moved past since start, for pacing rebroadcasts.
    pub(crate) moves: u64,
    /// Which blocks have been searched for pending transactions.
    pub(crate) search: Search,
    /// Blocks holding located transactions whose receipts are not yet verified.
    pub(crate) unreceipted: Vec<VerifiedBody>,
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

/// State every [`Node`] clone and the [`Follower`] share.
#[derive(Debug)]
pub(crate) struct Shared<P> {
    pub(crate) spec: NetworkSpec,
    pub(crate) env: ChainEnv,
    pub(crate) peers: P,
    pub(crate) watch: Watchlist,
    pub(crate) disk: Disk,
    /// The canonical state. Reached only through [`Shared::read`] and [`Shared::update`],
    /// whose closures cannot await, so no guard crosses an await.
    state: Mutex<State>,
    fees: Mutex<HashMap<B256, FeeSample>>,
}

impl Node<Devp2p> {
    /// Opens the data dir (taking its lock), starts the light client and devp2p, and returns
    /// once the state is anchored at a fresh verified head with every tracked key proven.
    pub async fn start(config: NodeConfig) -> Result<(Self, Follower<Devp2p>), NodeError> {
        let spec = config.network.spec();
        let disk = Disk::open(&config.data_dir)?;
        let secret_key = disk.secret_key()?;
        let known_peers: Vec<_> = spec
            .snap_peers
            .iter()
            .copied()
            .chain(disk.read(StateFile::peers))
            .collect();
        let checkpoint = checkpoint(&spec, config.checkpoint, &config.data_dir).await?;
        let url = match config.beacon_url {
            Some(url) => url,
            None => Url::parse(spec.default_beacon)
                .map_err(|err| BeaconError::Start(format!("default beacon url: {err}")))?,
        };

        let (heads_tx, mut heads) = Heads::channel();
        let beacon = Beacon::start(
            &spec,
            BeaconConfig {
                url,
                data_dir: config.data_dir.clone(),
                checkpoint,
            },
            heads_tx,
        )?;
        beacon.wait_synced().await?;
        if let Some(checkpoint) = checkpoint {
            remember_checkpoint(&config.data_dir, checkpoint).await?;
        }
        let trusted = wait_latest(&mut heads).await?;
        info!(target: "chibi::node", network = %spec.network, hash = %trusted.hash(), "light client synced");

        let net =
            Devp2p::start(&spec, secret_key, config.ports, HeadHint::Attested(trusted)).await?;
        net.add_peers(&known_peers);
        wait_for_snap_peer(&net, known_peers.len()).await?;

        let (node, mut follower) = Node::with_parts(spec, net, heads, disk, &config.watch).await?;
        follower.beacon = Some(beacon);
        Ok((node, follower))
    }
}

/// Waits until a snap/2 session is up, however long discovery takes, reporting progress.
async fn wait_for_snap_peer(net: &Devp2p, dialed: usize) -> Result<(), NodeError> {
    let started = Instant::now();
    let mut sessions = net.sessions();
    loop {
        let census = net.census();
        info!(
            target: "chibi::node",
            peers = census.peers,
            snap_peers = census.snap_peers,
            dialed,
            elapsed_s = started.elapsed().as_secs(),
            "waiting for a snap/2 peer",
        );
        let waited = tokio::time::timeout(
            SNAP_PEER_PROGRESS,
            sessions.wait_for(|sessions| sessions.values().any(|session| session.snap)),
        )
        .await;
        match waited {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(_)) => return Err(NodeError::Net(NetError::Stopped)),
            Err(_) => {}
        }
    }
}

/// The light-client checkpoint to pass to helios. A user override wins; otherwise helios's
/// own saved checkpoint (passing none lets helios load it); otherwise, on a first run, the
/// community checkpoint services.
async fn checkpoint(
    spec: &NetworkSpec,
    user: Option<B256>,
    data_dir: &Path,
) -> Result<Option<B256>, NodeError> {
    if let Some(checkpoint) = user {
        return Ok(Some(checkpoint));
    }
    if has_saved_checkpoint(data_dir).await {
        return Ok(None);
    }
    let mut attempt = 1;
    loop {
        match beacon::community_checkpoint(spec).await {
            Ok(checkpoint) => {
                warn!(
                    target: "chibi::node",
                    %checkpoint,
                    "no saved light-client checkpoint; trusting the community checkpoint services for this first run",
                );
                return Ok(Some(checkpoint));
            }
            Err(err) if attempt < CHECKPOINT_ATTEMPTS => {
                warn!(target: "chibi::node", attempt, %err, "community checkpoint failed; retrying");
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

/// Whether helios will load its saved checkpoint. helios rewrites the file in place without
/// fsync and silently falls back to its shipped default unless it holds exactly 32 bytes.
async fn has_saved_checkpoint(data_dir: &Path) -> bool {
    tokio::fs::metadata(data_dir.join(HELIOS_CHECKPOINT))
        .await
        .is_ok_and(|meta| meta.len() == CHECKPOINT_BYTES)
}

/// Saves a checkpoint the light client just synced from, in helios's own file format, unless
/// helios already saved one. helios writes only checkpoints newer than the one it started
/// from, so without this a run shorter than a finality update would leave no saved
/// checkpoint and every start would fall back to the community services.
async fn remember_checkpoint(data_dir: &Path, checkpoint: B256) -> Result<(), NodeError> {
    if has_saved_checkpoint(data_dir).await {
        return Ok(());
    }
    let path = data_dir.join(HELIOS_CHECKPOINT);
    let temp = path.with_extension("tmp");
    let saved = async {
        tokio::fs::write(&temp, checkpoint.as_slice()).await?;
        tokio::fs::rename(&temp, &path).await
    };
    saved
        .await
        .map_err(|err| NodeError::Disk(format!("saving the light-client checkpoint: {err}")))
}

async fn wait_latest(heads: &mut Heads) -> Result<chibi_core::TrustedHash, NodeError> {
    let latest = *heads
        .latest
        .wait_for(Option::is_some)
        .await
        .map_err(|_| NodeError::Stopped)?;
    latest.ok_or(NodeError::NotReady)
}

impl<P: Peers> Node<P> {
    /// Builds a node from its parts and anchors it at the light client's latest head: every
    /// watched account is proven, token seeding runs, and pending outbox transactions are
    /// re-broadcast.
    pub async fn with_parts(
        spec: NetworkSpec,
        peers: P,
        mut heads: Heads,
        disk: Disk,
        watch: &Watchlist,
    ) -> Result<(Self, Follower<P>), NodeError> {
        let trusted = wait_latest(&mut heads).await?;
        let anchor = fetch::header(&peers, trusted).await?;
        let tracked: Vec<Key> = watch.addresses.iter().copied().map(Key::Account).collect();
        let outbox = disk.read(|file| file.outbox().to_vec());
        let shared = Arc::new(Shared {
            env: ChainEnv::new(Arc::clone(&spec.chain)),
            spec,
            peers,
            watch: watch.clone(),
            disk,
            state: Mutex::new(State::new(anchor.clone(), tracked)),
            fees: Mutex::new(HashMap::new()),
        });
        shared.reanchor(anchor.clone()).await?;
        shared.restore_outbox(&outbox).await;
        shared.persist().await?;
        let (number, keys, restored) = shared.read(|s| {
            (
                s.anchor().number(),
                s.tracked().count(),
                s.txs().has_pending(),
            )
        });
        info!(target: "chibi::node", head = number, keys, "anchored");
        // Restored transactions may already sit in blocks this run never passed; the search
        // reaches back to their submission anchors.
        let floor = (!restored).then(|| anchor.num_hash());
        Ok((
            Self {
                shared: Arc::clone(&shared),
            },
            Follower {
                shared,
                heads,
                beacon: None,
                moves: 0,
                search: Search::new(floor),
                unreceipted: Vec::new(),
            },
        ))
    }

    /// The chain id served by `eth_chainId`.
    pub fn chain_id(&self) -> u64 {
        self.shared.spec.chain_id()
    }

    /// The registry entry for the followed chain.
    pub fn network(&self) -> &NetworkSpec {
        &self.shared.spec
    }

    /// The anchor header.
    pub fn head(&self) -> VerifiedHeader {
        self.shared.read(|s| s.anchor().clone())
    }

    /// A held header. `Safe` and `Finalized` give the finalized header.
    pub fn header(&self, block: BlockSel) -> Result<VerifiedHeader, NodeError> {
        self.shared.read(|s| {
            let store = s.store();
            let head = store.anchor().number();
            let held = match block {
                BlockSel::Latest | BlockSel::Pending => Some(store.anchor()),
                BlockSel::Safe | BlockSel::Finalized => {
                    return s.finalized().cloned().ok_or(NodeError::NotReady);
                }
                BlockSel::Number(number) => store
                    .header(number)
                    .or_else(|| s.finalized().filter(|f| f.number() == number)),
                BlockSel::Hash(hash) => store
                    .headers()
                    .rev()
                    .find(|header| header.hash() == hash)
                    .or_else(|| s.finalized().filter(|f| f.hash() == hash)),
            };
            held.cloned().ok_or(NodeError::Unsupported {
                what: "blocks outside the held header window",
                head,
            })
        })
    }

    /// The account at `block`, `None` if proven absent. Fetches and proves it on a miss.
    pub async fn account(
        &self,
        address: Address,
        block: BlockSel,
    ) -> Result<Option<Account>, NodeError> {
        let store = self.shared.pin(block)?;
        self.shared
            .resolve(store, |s| {
                s.account(address)
                    .map(Option::<&Account>::copied)
                    .map_err(|miss| vec![miss])
            })
            .await
    }

    /// A storage slot at `block`. Fetches and proves it on a miss.
    pub async fn storage(
        &self,
        address: Address,
        slot: U256,
        block: BlockSel,
    ) -> Result<U256, NodeError> {
        let store = self.shared.pin(block)?;
        self.shared
            .resolve(store, |s| {
                s.slot(address, slot).map_err(|miss| {
                    // One storage proof carries its account proof.
                    vec![match miss {
                        Miss::Account(address) => Miss::Storage(address, slot),
                        other @ (Miss::Storage(..) | Miss::Code(_) | Miss::BlockHash(_)) => other,
                    }]
                })
            })
            .await
    }

    /// The account's code at `block`, empty for accounts without code.
    pub async fn code(&self, address: Address, block: BlockSel) -> Result<Bytes, NodeError> {
        let store = self.shared.pin(block)?;
        self.shared
            .resolve(store, |s| {
                let Some(account) = s.account(address).map_err(|miss| vec![miss])? else {
                    return Ok(Bytes::new());
                };
                s.code(account.code_hash)
                    .map(|code| code.original_bytes())
                    .map_err(|miss| vec![miss])
            })
            .await
    }

    /// The account nonce. `Pending` adds chibi-submitted transactions not yet included.
    pub async fn transaction_count(
        &self,
        address: Address,
        block: BlockSel,
    ) -> Result<u64, NodeError> {
        let nonce = self
            .account(address, block)
            .await?
            .map_or(0, |account| account.nonce);
        Ok(match block {
            BlockSel::Pending => self.shared.read(|s| s.txs().pending_nonce(address, nonce)),
            BlockSel::Latest
            | BlockSel::Safe
            | BlockSel::Finalized
            | BlockSel::Number(_)
            | BlockSel::Hash(_) => nonce,
        })
    }

    /// Runs `request` locally without committing anything.
    pub async fn call(
        &self,
        request: CallRequest,
        block: BlockSel,
    ) -> Result<ExecutionResult<HaltReason>, NodeError> {
        let store = self.shared.pin(block)?;
        let env = &self.shared.env;
        let result = self
            .shared
            .resolve(store, |s| {
                exec::execute(s, env, &request)
                    .map(|executed| executed.result)
                    .map_err(misses)
            })
            .await?;
        result.map_err(NodeError::Exec)
    }

    /// Estimates gas for `request` with reth's bisection.
    pub async fn estimate_gas(
        &self,
        request: CallRequest,
        block: BlockSel,
    ) -> Result<u64, NodeError> {
        let store = self.shared.pin(block)?;
        let env = &self.shared.env;
        let estimate = self
            .shared
            .resolve(store, |s| {
                exec::estimate_gas(s, env, &request).map_err(misses)
            })
            .await?;
        estimate.map_err(NodeError::Estimate)
    }

    /// Suggested legacy gas price: next base fee plus the suggested priority fee.
    pub async fn gas_price(&self) -> Result<u128, NodeError> {
        let tip = self.max_priority_fee().await?;
        let anchor = self.head();
        let params = self
            .shared
            .spec
            .chain
            .base_fee_params_at_timestamp(anchor.header().timestamp + SLOT_SECONDS);
        let base_fee = tx::next_base_fee(&anchor, params).unwrap_or_default();
        Ok(u128::from(base_fee) + tip)
    }

    /// Suggested priority fee from tips in recent verified bodies.
    pub async fn max_priority_fee(&self) -> Result<u128, NodeError> {
        let headers: Vec<VerifiedHeader> = self.shared.read(|s| {
            s.store()
                .headers()
                .rev()
                .take(FEE_BLOCKS)
                .cloned()
                .collect()
        });
        let samples = self.shared.samples(&headers).await?;
        Ok(suggest_tip(&samples))
    }

    /// Fee history over up to `blocks` held headers ending at `newest`.
    pub async fn fee_history(
        &self,
        blocks: u64,
        newest: BlockSel,
        percentiles: &[f64],
    ) -> Result<FeeHistory, NodeError> {
        if blocks == 0 {
            return Ok(FeeHistory {
                oldest_block: 0,
                base_fee_per_gas: Vec::new(),
                gas_used_ratio: Vec::new(),
                base_fee_per_blob_gas: Vec::new(),
                blob_gas_used_ratio: Vec::new(),
                reward: None,
            });
        }
        let newest = self.header(newest)?;
        let headers: Vec<VerifiedHeader> = self.shared.read(|s| {
            let store = s.store();
            if store
                .header(newest.number())
                .is_none_or(|held| held.hash() != newest.hash())
            {
                return Err(NodeError::Unsupported {
                    what: "fee history outside the held header window",
                    head: store.anchor().number(),
                });
            }
            let oldest = newest
                .number()
                .saturating_sub(blocks.min(MAX_FEE_HISTORY) - 1);
            Ok((oldest..=newest.number())
                .filter_map(|number| store.header(number).cloned())
                .collect())
        })?;
        let chain = &self.shared.spec.chain;
        let mut history = FeeHistory {
            oldest_block: headers
                .first()
                .map_or(newest.number(), VerifiedHeader::number),
            base_fee_per_gas: Vec::with_capacity(headers.len() + 1),
            gas_used_ratio: Vec::with_capacity(headers.len()),
            base_fee_per_blob_gas: Vec::with_capacity(headers.len() + 1),
            blob_gas_used_ratio: Vec::with_capacity(headers.len()),
            reward: None,
        };
        for verified in &headers {
            let header = verified.header();
            let blob_params = chain.blob_params_at_timestamp(header.timestamp);
            history
                .base_fee_per_gas
                .push(header.base_fee_per_gas.map_or(0, u128::from));
            history
                .gas_used_ratio
                .push(ratio(header.gas_used, header.gas_limit));
            history.base_fee_per_blob_gas.push(
                blob_params
                    .and_then(|params| header.blob_fee(params))
                    .unwrap_or_default(),
            );
            history
                .blob_gas_used_ratio
                .push(blob_params.map_or(0.0, |params| {
                    ratio(
                        header.blob_gas_used.unwrap_or_default(),
                        params.max_blob_gas_per_block(),
                    )
                }));
        }
        let last = newest.header();
        let next_timestamp = last.timestamp + SLOT_SECONDS;
        history.base_fee_per_gas.push(
            last.next_block_base_fee(chain.base_fee_params_at_timestamp(next_timestamp))
                .map_or(0, u128::from),
        );
        history.base_fee_per_blob_gas.push(
            chain
                .blob_params_at_timestamp(next_timestamp)
                .and_then(|params| last.next_block_blob_fee(params))
                .unwrap_or_default(),
        );
        if !percentiles.is_empty() {
            let samples = self.shared.samples(&headers).await?;
            history.reward = Some(
                samples
                    .iter()
                    .map(|sample| {
                        percentiles
                            .iter()
                            .map(|p| tx::tip_percentile([sample], *p).unwrap_or_default())
                            .collect()
                    })
                    .collect(),
            );
        }
        Ok(history)
    }

    /// Decodes an EIP-2718 transaction, checks its chain id, records it, and broadcasts it.
    ///
    /// The outbox is persisted before the broadcast, and the hash is returned even when no
    /// peer received the transaction; the follower re-broadcasts it.
    pub async fn submit(&self, raw: &[u8]) -> Result<TxHash, NodeError> {
        let transaction = Transaction::decode_2718_exact(raw)
            .map_err(|_| NodeError::InvalidTransaction("malformed EIP-2718 transaction"))?;
        let since = self.shared.read(|s| s.anchor().num_hash());
        let pending = self.shared.accept(transaction, since)?;
        let hash = pending.hash;
        let transaction = pending.tx.clone();
        self.shared.record(pending).await?;
        self.shared.broadcast(transaction).await;
        Ok(hash)
    }

    /// The receipt of a chibi-submitted transaction, `None` while pending or unknown.
    pub fn receipt(&self, hash: TxHash) -> Result<Option<TxReceipt>, NodeError> {
        Ok(self.shared.read(|s| match s.txs().lookup(hash) {
            Lookup::Located(located) => located.receipt.as_ref().map(|receipt| TxReceipt {
                receipt: receipt.clone(),
                from: located.sender,
                to: receipt.kind().to().copied(),
                contract_address: receipt.contract_address(located.sender, located.nonce),
            }),
            Lookup::Pending(_) | Lookup::Unknown => None,
        }))
    }

    /// A chibi-submitted transaction, `None` if chibi did not submit it.
    pub fn transaction(&self, hash: TxHash) -> Result<Option<SubmittedTx>, NodeError> {
        Ok(self.shared.read(|s| match s.txs().lookup(hash) {
            Lookup::Located(located) => Some(SubmittedTx {
                transaction: located.tx.clone(),
                from: located.sender,
                included: Some((located.block, located.index)),
            }),
            Lookup::Pending(pending) => Some(SubmittedTx {
                transaction: pending.tx.clone(),
                from: pending.sender,
                included: None,
            }),
            Lookup::Unknown => None,
        }))
    }

    #[cfg(test)]
    pub(crate) fn tracked(&self) -> std::collections::BTreeSet<Key> {
        self.shared.read(|s| s.tracked().collect())
    }

    /// Current status.
    pub fn status(&self) -> Status {
        let store = self.shared.read(State::snapshot);
        let census = self.shared.peers.census();
        Status {
            snapshot: self.shared.snapshot(&store),
            peers: census.peers,
            snap_peers: census.snap_peers,
        }
    }
}

impl<P: Peers> Follower<P> {
    /// Follows the light client until the node shuts down.
    pub async fn run(mut self) -> Result<(), NodeError> {
        loop {
            match self.step().await {
                Ok(stepped) => debug!(target: "chibi::follow", ?stepped, "step"),
                Err(err @ (NodeError::Stopped | NodeError::Net(NetError::Stopped))) => {
                    return Err(err);
                }
                Err(err) => {
                    warn!(target: "chibi::follow", %err, "step failed; retrying at the next head");
                }
            }
            self.heads
                .latest
                .changed()
                .await
                .map_err(|_| NodeError::Stopped)?;
        }
    }

    /// Moves to the newest attested head once, then persists the state file.
    pub async fn step(&mut self) -> Result<Stepped, NodeError> {
        self.follow().await
    }
}

/// Reads the last saved snapshot without starting a node or taking the data-dir lock.
pub fn read_snapshot(data_dir: &Path) -> Result<Option<Snapshot>, NodeError> {
    Ok(crate::disk::read_state(data_dir)?.and_then(|file| file.snapshot().cloned()))
}

impl<P: Peers> Shared<P> {
    pub(crate) fn read<R>(&self, f: impl FnOnce(&State) -> R) -> R {
        f(&self.state.lock())
    }

    pub(crate) fn update<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock())
    }

    /// The store a state query at `block` reads. Only the anchor serves state.
    fn pin(&self, block: BlockSel) -> Result<Arc<Store>, NodeError> {
        let store = self.read(State::snapshot);
        let anchor = store.anchor();
        let serves = match block {
            BlockSel::Latest | BlockSel::Pending => true,
            BlockSel::Number(number) => number == anchor.number(),
            BlockSel::Hash(hash) => hash == anchor.hash(),
            BlockSel::Safe | BlockSel::Finalized => false,
        };
        if serves {
            Ok(store)
        } else {
            Err(NodeError::Unsupported {
                what: "state at blocks other than the verified head",
                head: anchor.number(),
            })
        }
    }

    /// Runs `step` on `store`, proving its misses at the same anchor between runs, then folds
    /// the new facts into the canonical state unless the anchor moved meanwhile.
    pub(crate) async fn resolve<T>(
        &self,
        mut store: Arc<Store>,
        step: impl Fn(&Store) -> Result<T, Vec<Miss>>,
    ) -> Result<T, NodeError> {
        let anchor = store.anchor().hash();
        let mut facts = Vec::new();
        let mut round = 0;
        let outcome = loop {
            let misses = match step(&store) {
                Ok(value) => break Ok(value),
                Err(misses) => misses,
            };
            if round == MISS_ROUNDS {
                break Err(NodeError::TooManyMissRounds {
                    rounds: MISS_ROUNDS,
                });
            }
            round += 1;
            let fetched = match fetch::misses(&self.peers, &store, misses).await {
                Ok(fetched) => fetched,
                Err(err) => break Err(err),
            };
            let held = Arc::make_mut(&mut store);
            for fetched in fetched {
                match fetched {
                    Fetched::Proven(proven) => {
                        held.insert(&proven)?;
                        facts.push(proven);
                    }
                    Fetched::Ancestors(headers) => held.insert_ancestors(headers)?,
                }
            }
        };
        if !facts.is_empty() {
            self.update(|s| s.merge(anchor, &facts));
        }
        outcome
    }

    /// Proves every tracked key at `target` into a fresh store while the old one stays
    /// published, swaps, and reruns token seeding.
    pub(crate) async fn reanchor(&self, target: VerifiedHeader) -> Result<(), NodeError> {
        let keys: Vec<Key> = self.read(|s| s.tracked().collect());
        let facts = fetch::keys(&self.peers, &target, keys).await?;
        self.update(|s| s.reanchor(target, &facts))?;
        self.seed().await;
        Ok(())
    }

    /// Runs `balanceOf(holder)` and a zero `transfer` to the holder from the holder for every
    /// watched address and token, and tracks every key either run read, whether or not the
    /// transfer succeeds. That covers proxies, pause flags, fee parameters, blacklists, and
    /// nonstandard layouts without per-token code.
    async fn seed(&self) {
        for holder in &self.watch.addresses {
            for token in &self.watch.tokens {
                for request in [balance_of(token, *holder), transfer_to_self(token, *holder)] {
                    let store = self.read(State::snapshot);
                    let seeded = self
                        .resolve(store, |s| {
                            exec::execute(s, &self.env, &request).map_err(misses)
                        })
                        .await;
                    match seeded {
                        Ok(executed) => self.update(|s| s.track(executed.read)),
                        Err(err) => warn!(
                            target: "chibi::node",
                            token = token.symbol, %holder, %err,
                            "token seeding failed; its keys are fetched on demand until the next head",
                        ),
                    }
                }
            }
        }
    }

    /// The display snapshot at `store`, computed locally without fetching.
    pub(crate) fn snapshot(&self, store: &Store) -> Snapshot {
        let holdings = self
            .watch
            .addresses
            .iter()
            .map(|address| {
                let account = store.account(*address).ok();
                Holding {
                    address: *address,
                    balance: account.map(|a| a.map_or(U256::ZERO, |a| a.balance)),
                    nonce: account.map(|a| a.map_or(0, |a| a.nonce)),
                    tokens: self
                        .watch
                        .tokens
                        .iter()
                        .map(|token| {
                            (
                                token.symbol.to_owned(),
                                token_balance(store, &self.env, token, *address),
                            )
                        })
                        .collect(),
                }
            })
            .collect();
        Snapshot {
            network: self.spec.network,
            block: store.anchor().num_hash(),
            saved_at: unix_now(),
            holdings,
        }
    }

    /// Writes the snapshot, the outbox, and snap/2 peers to the state file.
    pub(crate) async fn persist(self: &Arc<Self>) -> Result<(), NodeError> {
        let shared = Arc::clone(self);
        blocking(move || {
            let census = shared.peers.census();
            shared.disk.save(|file| {
                let (outbox, store) = shared.read(|s| (outbox(s.txs().pending()), s.snapshot()));
                file.set_outbox(outbox);
                file.set_snapshot(shared.snapshot(&store));
                file.remember_peers(&census.snap_records);
            })
        })
        .await
    }

    /// Persists an outbox holding `pending`, and only once that is durable records it in the
    /// book, so a submission reported as failed is never broadcast or counted.
    async fn record(self: &Arc<Self>, pending: PendingTx) -> Result<(), NodeError> {
        let shared = Arc::clone(self);
        let entry = OutboxTx {
            raw: Bytes::from(pending.tx.encoded_2718()),
            since: pending.since,
        };
        let (hash, sender, nonce) = (pending.hash, pending.sender, pending.nonce);
        blocking(move || {
            shared.disk.save_then(
                |file| {
                    let outbox = shared.read(|s| {
                        let txs = s.txs();
                        if !matches!(txs.lookup(hash), Lookup::Unknown) {
                            return outbox(txs.pending());
                        }
                        let mut entries = outbox(
                            txs.pending()
                                .iter()
                                .filter(|p| p.sender != sender || p.nonce != nonce),
                        );
                        entries.push(entry);
                        entries
                    });
                    file.set_outbox(outbox);
                },
                || shared.update(|s| s.txs_mut().submit(pending)),
            )
        })
        .await
    }

    /// Checks a submitted transaction and recovers its sender. `since` is the anchor read
    /// before the outbox write; an older one only widens the search for the transaction.
    fn accept(
        &self,
        transaction: Transaction,
        since: BlockNumHash,
    ) -> Result<PendingTx, NodeError> {
        if matches!(transaction, Transaction::Eip4844(_)) {
            return Err(NodeError::InvalidTransaction(
                "blob transactions are not supported",
            ));
        }
        match transaction.chain_id() {
            None => {
                return Err(NodeError::InvalidTransaction(
                    "transaction has no chain id (pre-EIP-155)",
                ));
            }
            Some(id) if id != self.spec.chain_id() => {
                return Err(NodeError::InvalidTransaction(
                    "transaction is for another chain",
                ));
            }
            Some(_) => {}
        }
        let sender = transaction
            .recover_signer()
            .map_err(|_| NodeError::InvalidTransaction("invalid signature"))?;
        Ok(PendingTx::new(transaction, sender, since))
    }

    /// Sends `transaction` to peers. Failure is logged: the follower re-broadcasts.
    pub(crate) async fn broadcast(&self, transaction: Transaction) {
        let hash = *transaction.tx_hash();
        match tokio::time::timeout(fetch::REQUEST_TIMEOUT, self.peers.broadcast(transaction)).await
        {
            Ok(Ok(sent)) => debug!(target: "chibi::node", %hash, sent, "broadcast"),
            Ok(Err(err)) => {
                warn!(target: "chibi::node", %hash, %err, "broadcast reached no peer; will retry");
            }
            Err(_) => warn!(target: "chibi::node", %hash, "broadcast timed out; will retry"),
        }
    }

    /// Records and re-broadcasts outbox transactions saved by an earlier run.
    async fn restore_outbox(&self, outbox: &[OutboxTx]) {
        for saved in outbox {
            let accepted = Transaction::decode_2718_exact(&saved.raw)
                .map_err(|_| NodeError::InvalidTransaction("malformed EIP-2718 transaction"))
                .and_then(|transaction| self.accept(transaction, saved.since));
            match accepted {
                Ok(pending) => {
                    let transaction = pending.tx.clone();
                    self.update(|s| s.txs_mut().submit(pending));
                    self.broadcast(transaction).await;
                }
                Err(err) => warn!(target: "chibi::node", %err, "dropping a saved outbox entry"),
            }
        }
    }

    /// Fee samples for `headers`, fetching and verifying the bodies not cached yet.
    async fn samples(&self, headers: &[VerifiedHeader]) -> Result<Vec<FeeSample>, NodeError> {
        let missing: Vec<VerifiedHeader> = {
            let cache = self.fees.lock();
            headers
                .iter()
                .filter(|header| !cache.contains_key(&header.hash()))
                .cloned()
                .collect()
        };
        if !missing.is_empty() {
            let bodies = fetch::bodies(&self.peers, &missing).await?;
            self.cache_samples(&bodies);
        }
        let cache = self.fees.lock();
        headers
            .iter()
            .map(|header| {
                cache
                    .get(&header.hash())
                    .cloned()
                    .ok_or(NodeError::Net(NetError::Rejected))
            })
            .collect()
    }

    pub(crate) fn cache_samples(&self, bodies: &[VerifiedBody]) {
        let mut cache = self.fees.lock();
        for body in bodies {
            cache.insert(body.header().hash(), tx::sample(body));
        }
        if cache.len() > FEE_CACHE {
            let newest = cache
                .values()
                .map(|s| s.block.number)
                .max()
                .unwrap_or_default();
            let keep = newest.saturating_sub(FEE_CACHE as u64 / 2);
            cache.retain(|_, sample| sample.block.number > keep);
        }
    }
}

/// reth's oracle: the lowest few tips of each block, then a percentile across them.
fn suggest_tip(samples: &[FeeSample]) -> u128 {
    let mut tips: Vec<u128> = samples
        .iter()
        .flat_map(|sample| sample.tips.iter().take(TIPS_PER_BLOCK).copied())
        .collect();
    if tips.is_empty() {
        return DEFAULT_TIP;
    }
    tips.sort_unstable();
    tips[(tips.len() - 1) * TIP_PERCENTILE / 100].min(MAX_TIP)
}

fn ratio(used: u64, limit: u64) -> f64 {
    if limit == 0 {
        0.0
    } else {
        used as f64 / limit as f64
    }
}

fn misses(misses: Misses) -> Vec<Miss> {
    misses.into_iter().collect()
}

fn outbox<'a>(pending: impl IntoIterator<Item = &'a PendingTx>) -> Vec<OutboxTx> {
    pending
        .into_iter()
        .map(|p| OutboxTx {
            raw: Bytes::from(p.tx.encoded_2718()),
            since: p.since,
        })
        .collect()
}

/// Runs blocking disk work off the async workers.
async fn blocking<R: Send + 'static>(
    work: impl FnOnce() -> Result<R, NodeError> + Send + 'static,
) -> Result<R, NodeError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| NodeError::Disk(format!("state file writer failed: {err}")))?
}

/// `balanceOf(holder)` sent by the holder, so the run reads no account besides those it needs.
fn balance_of(token: &Token, holder: Address) -> CallRequest {
    let mut input = Vec::with_capacity(36);
    input.extend_from_slice(&[0x70, 0xa0, 0x82, 0x31]);
    input.extend_from_slice(B256::left_padding_from(holder.as_slice()).as_slice());
    token_call(token, holder, input)
}

/// `transfer(holder, 0)` sent by the holder: the checks a real transfer makes, on the
/// holder's keys only.
fn transfer_to_self(token: &Token, holder: Address) -> CallRequest {
    let mut input = Vec::with_capacity(68);
    input.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
    input.extend_from_slice(B256::left_padding_from(holder.as_slice()).as_slice());
    input.extend_from_slice(&[0; 32]);
    token_call(token, holder, input)
}

fn token_call(token: &Token, holder: Address, input: Vec<u8>) -> CallRequest {
    CallRequest {
        from: Some(holder),
        to: TxKind::Call(token.address),
        input: Bytes::from(input),
        ..CallRequest::default()
    }
}

/// `balanceOf` run locally over held keys only; `None` if a key is missing or the call fails.
fn token_balance(store: &Store, env: &ChainEnv, token: &Token, holder: Address) -> Option<U256> {
    let executed = exec::execute(store, env, &balance_of(token, holder)).ok()?;
    if let Ok(ExecutionResult::Success { output, .. }) = executed.result {
        let data = output.data();
        return (data.len() == 32).then(|| U256::from_be_slice(data));
    }
    None
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(tips: &[u128]) -> FeeSample {
        FeeSample {
            block: BlockNumHash::default(),
            base_fee: 0,
            gas_used_ratio: 0.0,
            tips: tips.to_vec(),
        }
    }

    #[test]
    fn tip_uses_the_lowest_tips_of_each_block() {
        let samples = [sample(&[1, 2, 3, 1_000]), sample(&[4, 5, 6, 2_000])];
        assert_eq!(suggest_tip(&samples), 4);
    }

    #[test]
    fn tip_without_transactions_is_reths_default_and_is_capped() {
        assert_eq!(suggest_tip(&[sample(&[])]), DEFAULT_TIP);
        assert_eq!(suggest_tip(&[sample(&[u128::MAX])]), MAX_TIP);
    }

    #[tokio::test]
    async fn only_a_32_byte_checkpoint_file_counts_as_saved() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!has_saved_checkpoint(dir.path()).await);
        let path = dir.path().join(HELIOS_CHECKPOINT);
        for (len, saved) in [(0, false), (31, false), (32, true), (33, false)] {
            std::fs::write(&path, vec![7u8; len]).unwrap();
            assert_eq!(has_saved_checkpoint(dir.path()).await, saved, "{len} bytes");
        }
        let user = B256::repeat_byte(1);
        assert_eq!(
            checkpoint(&Network::Sepolia.spec(), Some(user), dir.path())
                .await
                .unwrap(),
            Some(user)
        );
    }

    #[tokio::test]
    async fn a_synced_first_run_checkpoint_is_saved_once_for_later_runs() {
        let dir = tempfile::tempdir().unwrap();
        let first = B256::repeat_byte(1);
        remember_checkpoint(dir.path(), first).await.unwrap();
        assert_eq!(
            checkpoint(&Network::Sepolia.spec(), None, dir.path())
                .await
                .unwrap(),
            None,
            "a saved checkpoint means helios loads it and no override is passed"
        );
        remember_checkpoint(dir.path(), B256::repeat_byte(2))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(dir.path().join(HELIOS_CHECKPOINT)).unwrap(),
            first.to_vec(),
            "a checkpoint helios saved is never replaced"
        );
    }
}
