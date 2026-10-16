//! The devp2p adapter: [`Peers`] over a standalone reth-network.
//!
//! reth's wire messages are converted into [`chibi_core::wire`] shapes here, so no reth type
//! reaches the rest of the node. chibi serves only the headers its store holds, but it answers
//! every inbound request, with an empty response of the matching type when it holds nothing
//! asked for, because peers drop sessions whose requests go unanswered.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_consensus::Header;
use alloy_eips::BlockHashOrNumber;
use alloy_primitives::{B256, Bytes, U256};
use chibi_core::state::Store;
use chibi_core::wire::{
    AccountRangeRequest, AccountReply, Body, Receipt, StorageRangeRequest, StorageReply,
    Transaction,
};
use chibi_core::{TrustedHash, VerifiedHeader};
use futures::StreamExt;
use parking_lot::Mutex;
use reth_chainspec::{ChainSpec, EthChainSpec, Head};
use reth_eth_wire_types::snap::{
    AccountData, AccountRangeMessage, BlockAccessListsMessage, ByteCodesMessage,
    GetAccountRangeMessage, GetBlockAccessListsMessage, GetByteCodesMessage,
    GetStorageRangesMessage, SnapProtocolMessage, StorageRangesMessage,
};
use reth_eth_wire_types::{
    BlockAccessLists, BlockBodies, BlockHeaders, BlockRangeUpdate, Capability, Cells, EthMessageID,
    EthVersion, GetBlockHeaders, HeadersDirection, NodeData, RawCapabilityMessage, Receipts,
    Receipts69, Receipts70,
};
use reth_network::eth_requests::IncomingEthRequest;
use reth_network::events::{NetworkEvent, PeerEvent, SessionInfo};
use reth_network::message::PeerMessage;
use reth_network::{
    FetchClient, NetworkConfigBuilder, NetworkEventListenerProvider, NetworkHandle, NetworkManager,
    PeersConfig,
};
use reth_network_api::{Direction, PeerRequest, Peers as _};
use reth_network_p2p::block_access_lists::client::{BalRequirement, BlockAccessListsClient};
use reth_network_p2p::bodies::client::BodiesClient;
use reth_network_p2p::download::DownloadClient;
use reth_network_p2p::error::{RequestError, RequestResult};
use reth_network_p2p::headers::client::{HeadersClient, HeadersRequest};
use reth_network_p2p::receipts::client::ReceiptsClient;
use reth_network_p2p::snap::client::SnapResponse;
use reth_network_p2p::sync::NetworkSyncUpdater as _;
pub use reth_network_peers::NodeRecord;
use reth_network_peers::{PeerId, TrustedPeer};
use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
use tokio::runtime::Handle;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, info, trace};

pub use reth_network::config::{SecretKey, rng_secret_key};

use crate::network::{BlockClock, NetworkSpec};
use crate::peers::{Census, NetError, PeerKey, Peers, Reply, SNAP_PER_PEER};
use crate::snap1::{Retired, Snap1};

/// Soft cap on snap response sizes, matching reth's own snap downloaders.
const RESPONSE_BYTES: u64 = 512 * 1024;

/// Most peers a transaction is sent to.
const BROADCAST_FANOUT: usize = 8;

/// Inbound requests buffered before reth drops new ones; reth's own handler uses the same.
const INBOUND_CAPACITY: usize = 256;

/// The first eth version with `BlockRangeUpdate`.
const ETH_69: u8 = EthVersion::Eth69 as u8;

const ETH: &str = "eth";
const SNAP: &str = "state (snap)";

/// The capability named when neither BAL source is connected.
const BAL_SOURCES: &str = "snap/2 or eth/71";

pub(crate) const SNAP_1: Capability = Capability::new_static("snap", 1);
const SNAP_2: Capability = Capability::snap_2();

/// How long a snap/1 session stays connected before it counts as a state peer. Network
/// crawlers announce snap/1 and disconnect within a millisecond.
pub const SNAP1_SETTLE: Duration = Duration::from_secs(3);

/// Local listening ports. Zero lets the operating system pick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ports {
    /// RLPx TCP port.
    pub rlpx: u16,
    /// discv4 UDP port.
    pub discovery: u16,
}

/// How chibi joins the devp2p network beyond what its [`NetworkSpec`] gives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetConfig {
    /// Local listening ports.
    pub ports: Ports,
    /// Nodes always dialed and kept, besides the network's built-in state peers.
    pub trusted_peers: Vec<NodeRecord>,
    /// Connect to `trusted_peers` only, dialing no built-in, remembered or discovered peer.
    pub trusted_only: bool,
    /// Discovery boot nodes in place of the network's.
    pub boot_nodes: Option<Vec<NodeRecord>>,
    /// Most outbound sessions; reth's default when `None`.
    pub max_outbound: Option<usize>,
    /// Most inbound sessions; reth's default when `None`.
    pub max_inbound: Option<usize>,
    /// Run no discovery, dialing only the peers given.
    pub disable_discovery: bool,
}

impl NetConfig {
    /// The nodes discovery starts from: the configured ones, else the network's, except when
    /// only trusted peers may connect.
    fn boot_nodes<'a>(&'a self, spec: &'a NetworkSpec) -> &'a [NodeRecord] {
        match &self.boot_nodes {
            Some(nodes) => nodes,
            None if self.trusted_only => &[],
            None => &spec.boot_nodes,
        }
    }

    /// The peers to dial at start besides the trusted ones, which reth dials itself: the
    /// network's built-in state peers and `remembered`, unless only trusted peers may connect.
    pub fn known_peers(&self, spec: &NetworkSpec, remembered: Vec<NodeRecord>) -> Vec<NodeRecord> {
        if self.trusted_only {
            return Vec::new();
        }
        spec.state_peers.iter().copied().chain(remembered).collect()
    }

    fn peers_config(&self) -> PeersConfig {
        PeersConfig::default()
            .with_trusted_nodes(
                self.trusted_peers
                    .iter()
                    .copied()
                    .map(TrustedPeer::from)
                    .collect(),
            )
            .with_trusted_nodes_only(self.trusted_only)
            .with_max_outbound_opt(self.max_outbound)
            .with_max_inbound_opt(self.max_inbound)
    }
}

/// The head chibi advertises when the network starts.
#[derive(Clone, Copy, Debug)]
pub enum HeadHint<'a> {
    /// A header already verified.
    Verified(&'a VerifiedHeader),
    /// Only the light client's hash is known. The number is `clock`'s floor for now, so it
    /// does not pass the real head, and the timestamp is now, which alone selects the right
    /// fork id.
    Attested {
        /// The light client's latest block hash.
        hash: TrustedHash,
        /// The newest block known to be on chain, counted forward from.
        clock: BlockClock,
    },
}

/// The snap version a state peer serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Snap {
    /// snap/1: proofs only, over chibi's own snap/1 sub-protocol.
    V1,
    /// snap/2: proofs and BALs, over reth's snap/2 session.
    V2,
}

impl Snap {
    /// The highest version among the `capabilities` a peer announced, which is the one RLPx
    /// negotiates with chibi.
    fn announced(capabilities: &[Capability]) -> Option<Self> {
        if capabilities.contains(&SNAP_2) {
            Some(Self::V2)
        } else if capabilities.contains(&SNAP_1) {
            Some(Self::V1)
        } else {
            None
        }
    }

    /// What is missing when no session serves this version or a later one.
    const fn missing(self) -> &'static str {
        match self {
            Self::V1 => SNAP,
            Self::V2 => "snap/2",
        }
    }

    /// The version for logs: `1`, `2`, or `none`.
    const fn label(snap: Option<Self>) -> &'static str {
        match snap {
            Some(Self::V1) => "1",
            Some(Self::V2) => "2",
            None => "none",
        }
    }
}

/// One established peer session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// Client name and version the peer announced.
    pub client: Arc<str>,
    /// Negotiated eth protocol version.
    pub eth: u8,
    /// The snap version the peer announced, `None` if it announced no snap. The session serves
    /// state over it as [`Session::serving`] says.
    pub snap: Option<Snap>,
    /// The record chibi dialed for a state peer session it opened, `None` otherwise. Only such
    /// records are worth remembering for the next start.
    pub dialable: Option<NodeRecord>,
    /// The publication sequence of the last block range sent to the peer.
    pub range_sent: Option<u64>,
    /// When the session was established.
    pub since: Instant,
}

impl Session {
    /// The snap version the session serves state over at `now`, which makes it a state peer:
    /// snap/2 at once, and snap/1 once the session has stayed connected [`SNAP1_SETTLE`].
    pub fn serving(&self, now: Instant) -> Option<Snap> {
        match self.snap? {
            Snap::V2 => Some(Snap::V2),
            Snap::V1 => (now >= self.since + SNAP1_SETTLE).then_some(Snap::V1),
        }
    }
}

/// Established sessions by peer.
pub type Sessions = HashMap<PeerKey, Session>;

/// Waits until `sessions` changes or one of its snap/1 sessions unsettled at `checked`, the
/// instant [`Session::serving`] was last asked, settles, which changes what that returns
/// without changing the sessions. Returns at once when such a session settled since
/// `checked`. Fails once the sender is gone.
pub async fn sessions_changed(
    sessions: &mut watch::Receiver<Sessions>,
    checked: Instant,
) -> Result<(), watch::error::RecvError> {
    let settles = sessions
        .borrow()
        .values()
        .filter(|session| session.snap == Some(Snap::V1))
        .map(|session| session.since + SNAP1_SETTLE)
        .filter(|at| *at > checked)
        .min();
    match settles {
        Some(at) => tokio::select! {
            changed = sessions.changed() => changed,
            () = tokio::time::sleep_until(at) => Ok(()),
        },
        None => sessions.changed().await,
    }
}

/// A snap reply as the session resolves it.
pub(crate) type SnapReply = oneshot::Receiver<RequestResult<SnapResponse>>;

/// chibi's snap requests in flight, by session.
#[derive(Debug, Default)]
struct SnapLoad {
    load: Mutex<HashMap<PeerKey, usize>>,
    /// Signalled each time a request leaves a session.
    released: Notify,
}

impl SnapLoad {
    /// Hands `send` the session in `sessions` serving `need` or a later snap version with the
    /// fewest requests in flight, and waits for the reply it returns. A request counts against
    /// the session until it is retired, even if the caller stops waiting first: a snap/2
    /// request when reth resolves its reply, and a request `send` returns a [`Retired`] for
    /// when that closes. Such a request is withdrawn when its caller stops waiting.
    async fn submit(
        self: &Arc<Self>,
        mut sessions: watch::Receiver<Sessions>,
        need: Snap,
        send: impl FnOnce(PeerId, Snap) -> (SnapReply, Option<Retired>),
    ) -> Result<(PeerId, SnapResponse), NetError> {
        let lease = self.lease(&mut sessions, need).await?;
        let peer = lease.peer.0;
        let (reply, retired) = send(peer, lease.snap);
        let (mut forward, forwarded) = oneshot::channel();
        tokio::spawn(async move {
            let reply = match retired {
                None => Some(reply.await),
                Some(retired) => {
                    let reply = tokio::select! {
                        reply = reply => Some(reply),
                        () = forward.closed() => None,
                    };
                    retired.await.ok();
                    reply
                }
            };
            drop(lease);
            if let Some(reply) = reply {
                forward.send(reply).ok();
            }
        });
        match forwarded.await {
            Ok(Ok(Ok(response))) => Ok((peer, response)),
            Ok(Ok(Err(err))) => Err(net_error(&err, SNAP)),
            Ok(Err(_)) | Err(_) => Err(NetError::Rejected),
        }
    }

    /// A request counted against the least loaded session serving `need` or later below
    /// [`SNAP_PER_PEER`], waiting while every such session is full or a snap/1 one has yet to
    /// settle. Fails at once when none is connected.
    async fn lease(
        self: &Arc<Self>,
        sessions: &mut watch::Receiver<Sessions>,
        need: Snap,
    ) -> Result<Lease, NetError> {
        loop {
            let released = self.released.notified();
            let mut released = std::pin::pin!(released);
            released.as_mut().enable();
            let now = Instant::now();
            if let Some((peer, snap)) =
                self.least_loaded(&sessions.borrow_and_update(), need, now)?
            {
                return Ok(Lease {
                    load: Arc::clone(self),
                    peer,
                    snap,
                });
            }
            tokio::select! {
                () = released => {}
                changed = sessions_changed(sessions, now) => changed.map_err(|_| NetError::Stopped)?,
            }
        }
    }

    fn least_loaded(
        &self,
        sessions: &Sessions,
        need: Snap,
        now: Instant,
    ) -> Result<Option<(PeerKey, Snap)>, NetError> {
        if !sessions
            .values()
            .any(|session| session.snap.is_some_and(|snap| snap >= need))
        {
            return Err(NetError::NoPeer {
                capability: need.missing(),
            });
        }
        let mut load = self.load.lock();
        let Some((_, peer, snap)) = sessions
            .iter()
            .filter_map(|(peer, session)| {
                let snap = session.serving(now).filter(|snap| *snap >= need)?;
                Some((load.get(peer).copied().unwrap_or_default(), *peer, snap))
            })
            .filter(|(count, ..)| *count < SNAP_PER_PEER)
            .min_by_key(|(count, ..)| *count)
        else {
            return Ok(None);
        };
        *load.entry(peer).or_default() += 1;
        Ok(Some((peer, snap)))
    }
}

/// One request counted against `peer`, a session serving `snap`.
#[derive(Debug)]
struct Lease {
    load: Arc<SnapLoad>,
    peer: PeerKey,
    snap: Snap,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut load = self.load.load.lock();
        if let Some(count) = load.get_mut(&self.peer) {
            *count -= 1;
            if *count == 0 {
                load.remove(&self.peer);
            }
        }
        drop(load);
        self.load.released.notify_waiters();
    }
}

/// devp2p failed to start.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Devp2pError {
    /// [`Devp2p::start`] ran outside a tokio runtime.
    #[error("devp2p must start inside a tokio runtime")]
    NoRuntime,
    /// reth's task runtime could not be built.
    #[error("failed to build the devp2p task runtime: {0}")]
    Runtime(String),
    /// The network could not bind or initialise discovery.
    #[error("failed to start devp2p: {0}")]
    Network(String),
}

/// A running devp2p node that implements [`Peers`].
///
/// Dropping it stops the network tasks.
#[derive(Debug)]
pub struct Devp2p {
    handle: NetworkHandle,
    fetch: FetchClient,
    chain: Arc<ChainSpec>,
    sessions: Arc<watch::Sender<Sessions>>,
    snap_load: Arc<SnapLoad>,
    snap1: Snap1,
    /// The store announced last, whose held headers inbound requests are answered from, with
    /// its publication sequence.
    served: watch::Sender<Option<(u64, Arc<Store>)>>,
    tasks: [JoinHandle<()>; 3],
}

impl Devp2p {
    /// Starts discovery and sessions for `spec` as `net` says, advertising snap/1, snap/2 and
    /// `head`.
    pub async fn start(
        spec: &NetworkSpec,
        secret_key: SecretKey,
        net: &NetConfig,
        head: HeadHint<'_>,
    ) -> Result<Self, Devp2pError> {
        let tokio = Handle::try_current().map_err(|_| Devp2pError::NoRuntime)?;
        let runtime = RuntimeBuilder::new(
            RuntimeConfig::default().with_tokio(TokioConfig::existing_handle(tokio)),
        )
        .build()
        .map_err(|err| Devp2pError::Runtime(err.to_string()))?;

        let head = match head {
            HeadHint::Verified(header) => status_head(&spec.chain, header),
            HeadHint::Attested { hash, clock } => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                Head {
                    number: clock.floor(now),
                    hash: hash.hash(),
                    difficulty: U256::ZERO,
                    total_difficulty: total_difficulty(&spec.chain),
                    timestamp: now,
                }
            }
        };
        let snap1 = Snap1::default();
        let config = NetworkConfigBuilder::eth(secret_key, runtime)
            .boot_nodes(net.boot_nodes(spec).iter().copied())
            .listener_port(net.ports.rlpx)
            .discovery_port(net.ports.discovery)
            .peer_config(net.peers_config())
            .disable_discovery_if(net.disable_discovery)
            .set_head(head)
            .with_snap(true)
            .add_rlpx_sub_protocol(snap1.clone())
            .disable_tx_gossip(true)
            .build_with_noop_provider(Arc::clone(&spec.chain));

        let (requests_tx, requests) = mpsc::channel(INBOUND_CAPACITY);
        let manager = NetworkManager::eth(config)
            .await
            .map_err(|err| Devp2pError::Network(err.to_string()))?
            .with_eth_request_handler(requests_tx);
        let fetch = manager.fetch_client();
        let handle = manager.handle().clone();
        let events = handle.event_listener();
        let sessions = Arc::new(watch::Sender::new(Sessions::new()));
        let (served, announced) = watch::channel(None);

        info!(
            target: "chibi::devp2p",
            network = %spec.network,
            peer = %handle.peer_id(),
            head = head.number,
            "devp2p started",
        );
        let tasks = [
            tokio::spawn(manager),
            tokio::spawn(track_sessions(
                events,
                handle.clone(),
                Arc::clone(&sessions),
                served.subscribe(),
            )),
            tokio::spawn(answer_inbound(requests, announced)),
        ];
        Ok(Self {
            handle,
            fetch,
            chain: Arc::clone(&spec.chain),
            sessions,
            snap_load: Arc::default(),
            snap1,
            served,
            tasks,
        })
    }

    /// The established sessions, updated as peers connect and disconnect.
    pub fn sessions(&self) -> watch::Receiver<Sessions> {
        self.sessions.subscribe()
    }

    /// Asks the network to dial `records` as static peers, such as state peers pinned in the
    /// registry or remembered from an earlier run.
    pub fn add_peers(&self, records: &[NodeRecord]) {
        for record in records {
            self.handle.add_peer(record.id, record.tcp_addr());
        }
    }

    /// Disconnects every session and stops the network tasks.
    pub async fn shutdown(self) {
        // An error only means the network task has already stopped.
        let _ = self.handle.shutdown().await;
    }

    fn reply<T>(peer: PeerId, value: T) -> Reply<T> {
        Reply {
            peer: PeerKey(peer),
            value,
        }
    }

    /// Penalizes a peer whose reply cannot be parsed and turns it into an error.
    fn garbled(&self, peer: PeerId) -> NetError {
        self.fetch.report_bad_message(peer);
        NetError::Rejected
    }

    /// Sends `request` to the connected session serving `need` or a later snap version with
    /// the fewest chibi requests in flight: through reth's snap/2 session, or chibi's snap/1
    /// connection. reth's fetcher would hold each peer to one request at a time.
    async fn snap(
        &self,
        need: Snap,
        request: SnapProtocolMessage,
    ) -> Result<(PeerId, SnapResponse), NetError> {
        self.snap_load
            .submit(self.sessions.subscribe(), need, |peer, snap| match snap {
                Snap::V1 => {
                    let (reply, retired) = self.snap1.send(peer, request);
                    (reply, Some(retired))
                }
                Snap::V2 => {
                    let (response, reply) = oneshot::channel();
                    self.handle
                        .send_request(peer, PeerRequest::GetSnap { request, response });
                    (reply, None)
                }
            })
            .await
    }

    async fn snap_block_access_lists(
        &self,
        blocks: Vec<B256>,
    ) -> Result<Reply<Vec<Option<Bytes>>>, NetError> {
        let requested = blocks.len();
        let message = GetBlockAccessListsMessage {
            request_id: 0,
            block_hashes: blocks,
            response_bytes: RESPONSE_BYTES,
        };
        let (peer, response) = self
            .snap(Snap::V2, SnapProtocolMessage::GetBlockAccessLists(message))
            .await?;
        match response {
            SnapResponse::BlockAccessLists(message) => {
                align(message.block_access_lists.0, requested)
                    .map(|lists| Self::reply(peer, lists))
                    .ok_or_else(|| self.garbled(peer))
            }
            SnapResponse::AccountRange(_)
            | SnapResponse::StorageRanges(_)
            | SnapResponse::ByteCodes(_) => Err(self.garbled(peer)),
        }
    }

    async fn eth_block_access_lists(
        &self,
        blocks: Vec<B256>,
    ) -> Result<Reply<Vec<Option<Bytes>>>, NetError> {
        let requested = blocks.len();
        let (peer, lists) = self
            .fetch
            .get_block_access_lists_with_requirement(blocks, BalRequirement::Optional)
            .await
            .map_err(|err| net_error(&err, "eth/71"))?
            .split();
        align(lists.0, requested)
            .map(|lists| Self::reply(peer, lists))
            .ok_or_else(|| self.garbled(peer))
    }
}

impl Drop for Devp2p {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Peers for Devp2p {
    async fn headers(&self, start: B256, count: u64) -> Result<Reply<Vec<Header>>, NetError> {
        let (peer, headers) = self
            .fetch
            .get_headers(HeadersRequest::falling(start.into(), count))
            .await
            .map_err(|err| net_error(&err, ETH))?
            .split();
        Ok(Self::reply(peer, headers))
    }

    async fn account_range(
        &self,
        request: AccountRangeRequest,
    ) -> Result<Reply<AccountReply>, NetError> {
        let message = GetAccountRangeMessage {
            request_id: 0,
            root_hash: request.root,
            starting_hash: request.key,
            limit_hash: request.key,
            response_bytes: RESPONSE_BYTES,
        };
        let (peer, response) = self
            .snap(Snap::V1, SnapProtocolMessage::GetAccountRange(message))
            .await?;
        match response {
            SnapResponse::AccountRange(range) => Ok(Self::reply(
                peer,
                AccountReply {
                    accounts: range
                        .accounts
                        .into_iter()
                        .map(AccountData::into_trie_entry)
                        .collect(),
                    proof: range.proof,
                },
            )),
            SnapResponse::StorageRanges(_)
            | SnapResponse::ByteCodes(_)
            | SnapResponse::BlockAccessLists(_) => Err(self.garbled(peer)),
        }
    }

    async fn storage_range(
        &self,
        request: StorageRangeRequest,
    ) -> Result<Reply<StorageReply>, NetError> {
        let message = GetStorageRangesMessage {
            request_id: 0,
            root_hash: request.root,
            account_hashes: vec![request.account],
            starting_hash: request.slot.into(),
            limit_hash: request.slot.into(),
            response_bytes: RESPONSE_BYTES,
        };
        let (peer, response) = self
            .snap(Snap::V1, SnapProtocolMessage::GetStorageRanges(message))
            .await?;
        let ranges = match response {
            SnapResponse::StorageRanges(ranges) => ranges,
            SnapResponse::AccountRange(_)
            | SnapResponse::ByteCodes(_)
            | SnapResponse::BlockAccessLists(_) => return Err(self.garbled(peer)),
        };
        let mut accounts = ranges.slots.into_iter();
        let slots = accounts.next().unwrap_or_default();
        if accounts.next().is_some() {
            return Err(self.garbled(peer));
        }
        let slots = slots
            .into_iter()
            .map(|slot| slot.value().map(|value| (slot.hash, value)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| self.garbled(peer))?;
        Ok(Self::reply(
            peer,
            StorageReply {
                slots,
                proof: ranges.proof,
            },
        ))
    }

    async fn byte_codes(&self, hashes: Vec<B256>) -> Result<Reply<Vec<Bytes>>, NetError> {
        let message = GetByteCodesMessage {
            request_id: 0,
            hashes,
            response_bytes: RESPONSE_BYTES,
        };
        let (peer, response) = self
            .snap(Snap::V1, SnapProtocolMessage::GetByteCodes(message))
            .await?;
        match response {
            SnapResponse::ByteCodes(codes) => Ok(Self::reply(peer, codes.codes)),
            SnapResponse::AccountRange(_)
            | SnapResponse::StorageRanges(_)
            | SnapResponse::BlockAccessLists(_) => Err(self.garbled(peer)),
        }
    }

    async fn block_access_lists(
        &self,
        blocks: Vec<B256>,
    ) -> Result<Reply<Vec<Option<Bytes>>>, NetError> {
        let snap = self.snap_block_access_lists(blocks.clone()).await;
        if snap.as_ref().is_ok_and(has_any) {
            return snap;
        }
        let eth = self.eth_block_access_lists(blocks).await;
        match (snap, eth) {
            (Ok(snap), Ok(eth)) => Ok(if has_any(&eth) { eth } else { snap }),
            (Ok(reply), Err(_)) | (Err(_), Ok(reply)) => Ok(reply),
            (Err(snap), Err(eth)) => Err(both_failed(snap, eth)),
        }
    }

    async fn bodies(&self, blocks: Vec<B256>) -> Result<Reply<Vec<Body>>, NetError> {
        let (peer, bodies) = self
            .fetch
            .get_block_bodies(blocks)
            .await
            .map_err(|err| net_error(&err, ETH))?
            .split();
        Ok(Self::reply(peer, bodies))
    }

    async fn receipts(&self, block: B256) -> Result<Reply<Vec<Receipt>>, NetError> {
        let (peer, response) = self
            .fetch
            .get_receipts(vec![block])
            .await
            .map_err(|err| net_error(&err, ETH))?
            .split();
        let mut blocks = response.receipts.into_iter();
        let receipts = blocks.next().unwrap_or_default();
        if blocks.next().is_some() {
            return Err(self.garbled(peer));
        }
        Ok(Self::reply(peer, receipts))
    }

    async fn broadcast(&self, transaction: Transaction) -> Result<usize, NetError> {
        let transaction = Arc::new(transaction);
        let mut sent = 0;
        for peer in self.sessions.borrow().keys().take(BROADCAST_FANOUT) {
            self.handle
                .send_transactions(peer.0, vec![Arc::clone(&transaction)]);
            sent += 1;
        }
        if sent == 0 {
            return Err(NetError::NoPeer { capability: ETH });
        }
        debug!(target: "chibi::devp2p", hash = %transaction.hash(), sent, "broadcast transaction");
        Ok(sent)
    }

    fn penalize(&self, peer: PeerKey) {
        debug!(target: "chibi::devp2p", peer = %peer.0, "penalizing peer");
        self.fetch.report_bad_message(peer.0);
    }

    fn announce(&self, store: &Arc<Store>) {
        let head = store.anchor();
        self.handle.update_status(status_head(&self.chain, head));
        let range = block_range(head);
        self.handle.update_block_range(range.0.clone());
        let mut seq = 0;
        self.served.send_modify(|served| {
            seq = served.as_ref().map_or(1, |(last, _)| last + 1);
            *served = Some((seq, Arc::clone(store)));
        });
        let peers: Vec<PeerKey> = self.sessions.borrow().keys().copied().collect();
        for peer in peers {
            let sent = offer_range(&self.sessions, peer, seq, &range, |peer, message| {
                self.handle
                    .send_eth_message(peer, PeerMessage::Other(message.clone()));
            });
            if sent {
                debug!(target: "chibi::devp2p", peer = %peer.0, head = range.0.latest, "sent block range");
            }
        }
    }

    fn census(&self) -> Census {
        census(&self.sessions.borrow())
    }
}

/// The session counts in `sessions` now: the state peers are the sessions serving a snap
/// version, and their dialable records are the ones to remember.
fn census(sessions: &Sessions) -> Census {
    let now = Instant::now();
    let serving = || {
        sessions
            .values()
            .filter(move |session| session.serving(now).is_some())
    };
    Census {
        peers: sessions.len(),
        state_peers: serving().count(),
        snap_records: serving().filter_map(|session| session.dialable).collect(),
    }
}

/// The status head for a verified header.
fn status_head(chain: &ChainSpec, header: &VerifiedHeader) -> Head {
    Head {
        number: header.number(),
        hash: header.hash(),
        difficulty: header.header().difficulty,
        total_difficulty: total_difficulty(chain),
        timestamp: header.header().timestamp,
    }
}

/// Total difficulty stopped growing at the merge, so every post-merge head carries the final one.
fn total_difficulty(chain: &ChainSpec) -> U256 {
    chain.final_paris_total_difficulty().unwrap_or_default()
}

/// The eth/69 `BlockRangeUpdate` for `head` and its encoding as a raw eth message. The head is
/// both the earliest and the latest block chibi serves: it holds no bodies or receipts, and
/// state only near the head.
fn block_range(head: &VerifiedHeader) -> (BlockRangeUpdate, RawCapabilityMessage) {
    let update = BlockRangeUpdate {
        earliest: head.number(),
        latest: head.number(),
        latest_hash: head.hash(),
    };
    let payload = alloy_rlp::encode(&update).into();
    let message = RawCapabilityMessage::eth(EthMessageID::BlockRangeUpdate, payload);
    (update, message)
}

/// Sends `range`, published as `seq`, to `peer` through `send` if it is an eth/69 session not
/// yet sent this publication or a later one, and returns whether it sent. Check, send and
/// record happen under the sessions lock, so a range read before a newer one was sent cannot
/// follow it. reth only refreshes a live session's range every 384 s, and drops a typed
/// `PeerMessage::BlockRangeUpdated` sent from outside, so `send` takes it as a raw eth message.
fn offer_range(
    sessions: &watch::Sender<Sessions>,
    peer: PeerKey,
    seq: u64,
    (_, message): &(BlockRangeUpdate, RawCapabilityMessage),
    send: impl FnOnce(PeerId, &RawCapabilityMessage),
) -> bool {
    let mut sent = false;
    sessions.send_if_modified(|sessions| {
        if let Some(session) = sessions.get_mut(&peer)
            && session.eth >= ETH_69
            && session.range_sent.is_none_or(|last| seq > last)
        {
            send(peer.0, message);
            session.range_sent = Some(seq);
            sent = true;
        }
        false
    });
    sent
}

fn net_error(err: &RequestError, capability: &'static str) -> NetError {
    match err {
        RequestError::ChannelClosed => NetError::Stopped,
        RequestError::UnsupportedCapability => NetError::NoPeer { capability },
        RequestError::Timeout => NetError::Timeout,
        RequestError::ConnectionDropped | RequestError::BadResponse => NetError::Rejected,
    }
}

/// Pads a BAL reply to one entry per requested block. Peers may truncate at their size limit;
/// a longer reply cannot correspond to the request.
fn align(mut lists: Vec<Option<Bytes>>, requested: usize) -> Option<Vec<Option<Bytes>>> {
    if lists.len() > requested {
        return None;
    }
    lists.resize(requested, None);
    Some(lists)
}

fn has_any(reply: &Reply<Vec<Option<Bytes>>>) -> bool {
    reply.value.iter().any(Option::is_some)
}

/// The error for a BAL request that failed on both protocols: the snap/2 failure unless snap/2
/// simply had no peer.
fn both_failed(snap: NetError, eth: NetError) -> NetError {
    match (snap, eth) {
        (NetError::NoPeer { .. }, NetError::NoPeer { .. }) => NetError::NoPeer {
            capability: BAL_SOURCES,
        },
        (NetError::NoPeer { .. }, eth) => eth,
        (snap, _) => snap,
    }
}

/// Keeps `sessions` current and sends each new eth/69 session the range of the store in
/// `served`, if one was announced yet.
async fn track_sessions(
    events: impl futures::Stream<Item = NetworkEvent>,
    handle: NetworkHandle,
    sessions: Arc<watch::Sender<Sessions>>,
    served: watch::Receiver<Option<(u64, Arc<Store>)>>,
) {
    let mut events = std::pin::pin!(events);
    while let Some(event) = events.next().await {
        match event {
            NetworkEvent::ActivePeerSession { info, .. }
            | NetworkEvent::Peer(PeerEvent::SessionEstablished(info)) => {
                let (peer, mut session) = session(&info);
                if session.snap.is_some() {
                    session.dialable = dialable(&handle, peer).await;
                }
                debug!(
                    target: "chibi::devp2p",
                    %peer, client = %session.client, eth = session.eth,
                    snap = Snap::label(session.snap),
                    dialable = session.dialable.is_some(),
                    "session established",
                );
                sessions.send_modify(|sessions| {
                    sessions.insert(PeerKey(peer), session);
                });
                let latest = served.borrow().clone();
                if let Some((seq, store)) = latest {
                    let range = block_range(store.anchor());
                    let sent =
                        offer_range(&sessions, PeerKey(peer), seq, &range, |peer, message| {
                            handle.send_eth_message(peer, PeerMessage::Other(message.clone()));
                        });
                    if sent {
                        debug!(target: "chibi::devp2p", %peer, head = range.0.latest, "sent block range");
                    }
                }
            }
            NetworkEvent::Peer(PeerEvent::SessionClosed { peer_id, reason }) => {
                debug!(target: "chibi::devp2p", peer = %peer_id, ?reason, "session closed");
                sessions.send_modify(|sessions| {
                    sessions.remove(&PeerKey(peer_id));
                });
            }
            NetworkEvent::Peer(PeerEvent::PeerAdded(_) | PeerEvent::PeerRemoved(_)) => {}
        }
    }
}

fn session(info: &SessionInfo) -> (PeerId, Session) {
    let session = Session {
        client: Arc::clone(&info.client_version),
        eth: info.version as u8,
        snap: Snap::announced(info.capabilities.capabilities()),
        dialable: None,
        range_sent: None,
        since: Instant::now(),
    };
    (info.peer_id, session)
}

/// The record chibi dialed to reach `peer`. Inbound sessions have none: their remote address
/// carries the peer's ephemeral source port, not its listening port.
async fn dialable(handle: &NetworkHandle, peer: PeerId) -> Option<NodeRecord> {
    let info = handle.get_peer_by_id(peer).await.ok().flatten()?;
    match info.direction {
        Direction::Outgoing(_) => info.enode.parse().ok(),
        Direction::Incoming => None,
    }
}

async fn answer_inbound(
    mut requests: mpsc::Receiver<IncomingEthRequest>,
    served: watch::Receiver<Option<(u64, Arc<Store>)>>,
) {
    while let Some(request) = requests.recv().await {
        let store = served.borrow().as_ref().map(|(_, store)| Arc::clone(store));
        answer(request, store.as_deref());
    }
}

/// Replies to `request` from the headers `store` holds, and with an empty response of the
/// matching type to anything else. Dropping the sender or replying `Err` would send nothing,
/// and the peer would time out and drop chibi.
fn answer(request: IncomingEthRequest, store: Option<&Store>) {
    let (peer, delivered) = match request {
        IncomingEthRequest::GetBlockHeaders {
            peer_id,
            request,
            response,
        } => {
            let headers = store.map_or_else(Vec::new, |store| held_headers(store, &request));
            (peer_id, response.send(Ok(BlockHeaders(headers))).is_ok())
        }
        IncomingEthRequest::GetBlockBodies {
            peer_id, response, ..
        } => (peer_id, response.send(Ok(BlockBodies(Vec::new()))).is_ok()),
        IncomingEthRequest::GetNodeData {
            peer_id, response, ..
        } => (peer_id, response.send(Ok(NodeData(Vec::new()))).is_ok()),
        IncomingEthRequest::GetReceipts {
            peer_id, response, ..
        } => (peer_id, response.send(Ok(Receipts(Vec::new()))).is_ok()),
        IncomingEthRequest::GetReceipts69 {
            peer_id, response, ..
        } => (peer_id, response.send(Ok(Receipts69(Vec::new()))).is_ok()),
        IncomingEthRequest::GetReceipts70 {
            peer_id, response, ..
        } => {
            let empty = Receipts70 {
                last_block_incomplete: false,
                receipts: Vec::new(),
            };
            (peer_id, response.send(Ok(empty)).is_ok())
        }
        IncomingEthRequest::GetBlockAccessLists {
            peer_id, response, ..
        } => (
            peer_id,
            response.send(Ok(BlockAccessLists(Vec::new()))).is_ok(),
        ),
        IncomingEthRequest::GetCells {
            peer_id, response, ..
        } => (peer_id, response.send(Ok(Cells::default())).is_ok()),
        IncomingEthRequest::GetSnap {
            peer_id,
            request,
            response,
        } => (peer_id, response.send(Ok(empty_snap(&request))).is_ok()),
    };
    trace!(target: "chibi::devp2p", %peer, delivered, "answered inbound request");
}

/// The headers `request` asks for, as far as `store` holds them: from its start block, stepping
/// `skip + 1` blocks in its direction, until `limit` headers or the first block not held.
fn held_headers(store: &Store, request: &GetBlockHeaders) -> Vec<Header> {
    let start = match request.start_block {
        BlockHashOrNumber::Hash(hash) => store.headers().rev().find(|held| held.hash() == hash),
        BlockHashOrNumber::Number(number) => store.header(number),
    };
    let Some(start) = start else {
        return Vec::new();
    };
    let step = u64::from(request.skip) + 1;
    let direction = request.direction;
    std::iter::successors(Some(start.number()), |number| match direction {
        HeadersDirection::Rising => number.checked_add(step),
        HeadersDirection::Falling => number.checked_sub(step),
    })
    .map_while(|number| store.header(number))
    .take(usize::try_from(request.limit).unwrap_or(usize::MAX))
    .map(|held| held.header().clone())
    .collect()
}

/// The empty snap/2 response for `request`. The session routes response-shaped messages to
/// their pending request, so only requests arrive here; the response arms exist for
/// exhaustiveness.
fn empty_snap(request: &SnapProtocolMessage) -> SnapResponse {
    let request_id = request.request_id();
    match request {
        SnapProtocolMessage::GetAccountRange(_) | SnapProtocolMessage::AccountRange(_) => {
            SnapResponse::AccountRange(AccountRangeMessage {
                request_id,
                accounts: Vec::new(),
                proof: Vec::new(),
            })
        }
        SnapProtocolMessage::GetStorageRanges(_) | SnapProtocolMessage::StorageRanges(_) => {
            SnapResponse::StorageRanges(StorageRangesMessage {
                request_id,
                slots: Vec::new(),
                proof: Vec::new(),
            })
        }
        SnapProtocolMessage::GetByteCodes(_) | SnapProtocolMessage::ByteCodes(_) => {
            SnapResponse::ByteCodes(ByteCodesMessage {
                request_id,
                codes: Vec::new(),
            })
        }
        SnapProtocolMessage::GetBlockAccessLists(_) | SnapProtocolMessage::BlockAccessLists(_) => {
            SnapResponse::BlockAccessLists(BlockAccessListsMessage {
                request_id,
                block_access_lists: BlockAccessLists(Vec::new()),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::bytes::BytesMut;
    use alloy_primitives::{B128, B512, BlockNumber, b256};
    use chibi_core::testing::{BlockWrites, GENESIS_NUMBER, World};
    use reth_eth_wire_types::{
        EthMessage, EthNetworkPrimitives, GetBlockAccessLists, GetBlockBodies, GetCells,
        GetNodeData, GetReceipts, GetReceipts70, ProtocolMessage,
    };

    use super::*;
    use crate::network::Network;
    use crate::peers::REQUEST_TIMEOUT;

    /// A session established [`SNAP1_SETTLE`] ago, which serves whatever snap version it has.
    fn session(snap: Option<Snap>) -> Session {
        Session {
            client: Arc::from("test"),
            eth: 69,
            snap,
            dialable: None,
            range_sent: None,
            since: Instant::now() - SNAP1_SETTLE,
        }
    }

    /// A snap/1 session established just now, as a crawler's is.
    fn fresh_snap1() -> Session {
        Session {
            since: Instant::now(),
            ..session(Some(Snap::V1))
        }
    }

    fn snap_sessions(peers: &[PeerKey]) -> watch::Sender<Sessions> {
        watch::Sender::new(
            peers
                .iter()
                .map(|peer| (*peer, session(Some(Snap::V2))))
                .collect(),
        )
    }

    fn answer() -> RequestResult<SnapResponse> {
        Ok(SnapResponse::ByteCodes(ByteCodesMessage {
            request_id: 0,
            codes: Vec::new(),
        }))
    }

    type Responder = oneshot::Sender<RequestResult<SnapResponse>>;

    /// Requests [`SnapLoad`] submitted, with the snap version each went out on, held
    /// unanswered like a session holds them until the peer replies.
    #[derive(Clone, Debug, Default)]
    struct Wire(Arc<Mutex<Vec<(PeerKey, Snap, Responder)>>>);

    impl Wire {
        /// A proof request, which any state peer serves.
        fn request(
            &self,
            load: &Arc<SnapLoad>,
            sessions: &watch::Sender<Sessions>,
        ) -> JoinHandle<Result<(PeerId, SnapResponse), NetError>> {
            self.submit(Snap::V1, load, sessions)
        }

        /// A BAL request, which only snap/2 peers serve.
        fn bal(
            &self,
            load: &Arc<SnapLoad>,
            sessions: &watch::Sender<Sessions>,
        ) -> JoinHandle<Result<(PeerId, SnapResponse), NetError>> {
            self.submit(Snap::V2, load, sessions)
        }

        fn submit(
            &self,
            need: Snap,
            load: &Arc<SnapLoad>,
            sessions: &watch::Sender<Sessions>,
        ) -> JoinHandle<Result<(PeerId, SnapResponse), NetError>> {
            let (load, sessions, wire) = (Arc::clone(load), sessions.subscribe(), self.clone());
            tokio::spawn(async move {
                load.submit(sessions, need, |peer, snap| {
                    let (response, reply) = oneshot::channel();
                    wire.0.lock().push((PeerKey(peer), snap, response));
                    (reply, None)
                })
                .await
            })
        }

        /// Requests out at `peer` that someone still waits for.
        fn outstanding(&self, peer: PeerKey) -> usize {
            self.0
                .lock()
                .iter()
                .filter(|(on, _, responder)| *on == peer && !responder.is_closed())
                .count()
        }

        /// The snap versions of the requests sent to `peer`, oldest first.
        fn versions(&self, peer: PeerKey) -> Vec<Snap> {
            self.0
                .lock()
                .iter()
                .filter(|(on, ..)| *on == peer)
                .map(|(_, snap, _)| *snap)
                .collect()
        }

        /// Answers up to `count` of the oldest requests sent to `peer`.
        fn answer(&self, peer: PeerKey, count: usize) {
            let mut sent = self.0.lock();
            for _ in 0..count {
                if let Some(at) = sent.iter().position(|(on, ..)| *on == peer) {
                    sent.remove(at).2.send(answer()).ok();
                }
            }
        }

        /// Drops every request sent to `peer`, as a session does when it closes.
        fn close(&self, peer: PeerKey) {
            self.0.lock().retain(|(on, ..)| *on != peer);
        }
    }

    #[tokio::test]
    async fn after_one_of_two_full_snap_sessions_drops_the_survivor_keeps_its_share() {
        let sessions = snap_sessions(&[A, B]);
        let (load, wire) = (Arc::new(SnapLoad::default()), Wire::default());

        let first: Vec<_> = (0..2 * SNAP_PER_PEER)
            .map(|_| wire.request(&load, &sessions))
            .collect();
        settle().await;
        assert_eq!(
            (wire.outstanding(A), wire.outstanding(B)),
            (SNAP_PER_PEER, SNAP_PER_PEER)
        );

        let waiting: Vec<_> = (0..2 * SNAP_PER_PEER)
            .map(|_| wire.request(&load, &sessions))
            .collect();
        settle().await;
        assert_eq!(
            (wire.outstanding(A), wire.outstanding(B)),
            (SNAP_PER_PEER, SNAP_PER_PEER)
        );

        sessions.send_modify(|sessions| {
            sessions.remove(&B);
        });
        wire.close(B);
        settle().await;
        for _ in 0..2 * SNAP_PER_PEER / 4 {
            assert_eq!(wire.outstanding(A), SNAP_PER_PEER);
            wire.answer(A, 4);
            settle().await;
        }
        assert_eq!(wire.outstanding(A), SNAP_PER_PEER);
        wire.answer(A, SNAP_PER_PEER);

        let answered = futures::future::join_all(first.into_iter().chain(waiting)).await;
        let failed = answered
            .into_iter()
            .filter(|call| call.as_ref().unwrap().is_err())
            .count();
        assert_eq!(failed, SNAP_PER_PEER, "only the requests out at B fail");
    }

    #[tokio::test]
    async fn requests_whose_callers_gave_up_keep_their_peer_slots_until_the_peer_answers() {
        let sessions = snap_sessions(&[A]);
        let (load, wire) = (Arc::new(SnapLoad::default()), Wire::default());

        let abandoned: Vec<_> = (0..SNAP_PER_PEER)
            .map(|_| wire.request(&load, &sessions))
            .collect();
        settle().await;
        assert_eq!(wire.outstanding(A), SNAP_PER_PEER);
        for call in abandoned {
            call.abort();
        }
        settle().await;

        let calls: Vec<_> = (0..SNAP_PER_PEER)
            .map(|_| wire.request(&load, &sessions))
            .collect();
        settle().await;
        assert_eq!(wire.outstanding(A), SNAP_PER_PEER);
        for _ in 0..2 * SNAP_PER_PEER / 4 {
            wire.answer(A, 4);
            settle().await;
            assert!(wire.outstanding(A) <= SNAP_PER_PEER);
        }
        for call in calls {
            assert!(call.await.unwrap().is_ok());
        }
    }

    async fn settle() {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    }

    const A: PeerKey = PeerKey(B512::repeat_byte(1));
    const B: PeerKey = PeerKey(B512::repeat_byte(2));

    fn range_at(head: BlockNumber, hash: u8) -> (BlockRangeUpdate, RawCapabilityMessage) {
        let update = BlockRangeUpdate {
            earliest: head,
            latest: head,
            latest_hash: B256::repeat_byte(hash),
        };
        let payload = alloy_rlp::encode(&update).into();
        let message = RawCapabilityMessage::eth(EthMessageID::BlockRangeUpdate, payload);
        (update, message)
    }

    fn offer(
        sessions: &watch::Sender<Sessions>,
        peer: PeerKey,
        seq: u64,
        head: BlockNumber,
        hash: u8,
    ) -> Vec<(u64, B256)> {
        let mut sent = Vec::new();
        let reported = offer_range(sessions, peer, seq, &range_at(head, hash), |to, message| {
            assert_eq!(to, peer.0);
            let update: BlockRangeUpdate =
                alloy_rlp::Decodable::decode(&mut &message.payload[..]).unwrap();
            sent.push((update.latest, update.latest_hash));
        });
        assert_eq!(reported, !sent.is_empty());
        sent
    }

    #[test]
    fn a_session_is_sent_each_later_publication_once() {
        let sessions = snap_sessions(&[A]);
        assert_eq!(
            offer(&sessions, A, 1, 105, 1),
            [(105, B256::repeat_byte(1))]
        );
        assert_eq!(offer(&sessions, A, 1, 105, 1), []);
        assert_eq!(
            offer(&sessions, A, 2, 106, 2),
            [(106, B256::repeat_byte(2))]
        );
    }

    #[test]
    fn a_reorged_head_at_the_same_or_a_lower_height_is_sent() {
        let sessions = snap_sessions(&[A]);
        assert_eq!(
            offer(&sessions, A, 1, 105, 1),
            [(105, B256::repeat_byte(1))]
        );
        assert_eq!(
            offer(&sessions, A, 2, 105, 2),
            [(105, B256::repeat_byte(2))]
        );
        assert_eq!(
            offer(&sessions, A, 3, 104, 3),
            [(104, B256::repeat_byte(3))]
        );
    }

    #[test]
    fn a_connect_time_range_read_before_a_newer_announce_is_not_sent_after_it() {
        let sessions = snap_sessions(&[A]);
        let announced = offer(&sessions, A, 2, 104, 2);
        let connected = offer(&sessions, A, 1, 105, 1);
        assert_eq!(
            [announced, connected].concat(),
            [(104, B256::repeat_byte(2))]
        );
    }

    #[test]
    fn eth68_and_unknown_sessions_are_never_sent_a_range() {
        let sessions = watch::Sender::new(Sessions::from([(
            A,
            Session {
                eth: 68,
                ..session(Some(Snap::V2))
            },
        )]));
        assert_eq!(offer(&sessions, A, 1, 105, 1), []);
        assert_eq!(offer(&sessions, B, 1, 105, 1), []);
    }

    #[tokio::test]
    async fn snap_requests_go_to_the_least_loaded_snap_session_and_need_one() {
        let eth = PeerKey(B512::repeat_byte(3));
        let sessions = snap_sessions(&[A, B]);
        sessions.send_modify(|sessions| {
            sessions.insert(eth, session(None));
        });
        let (load, wire) = (Arc::new(SnapLoad::default()), Wire::default());

        let calls: Vec<_> = (0..4).map(|_| wire.request(&load, &sessions)).collect();
        settle().await;
        assert_eq!(
            (
                wire.outstanding(A),
                wire.outstanding(B),
                wire.outstanding(eth)
            ),
            (2, 2, 0)
        );
        wire.answer(A, 2);
        wire.answer(B, 2);
        for call in calls {
            assert!(call.await.unwrap().is_ok());
        }
        assert!(load.load.lock().is_empty());

        let no_snap = watch::Sender::new(Sessions::from([(eth, session(None))]));
        let no_peer = wire.request(&load, &no_snap).await.unwrap().unwrap_err();
        assert_eq!(no_peer.to_string(), "no connected peer serves state (snap)");
    }

    #[test]
    fn the_snap_version_is_the_highest_one_announced() {
        let eth = Capability::new_static("eth", 69);
        let snap1 = Capability::new_static("snap", 1);
        let snap2 = Capability::new_static("snap", 2);
        assert_eq!(
            Snap::announced(&[eth.clone(), snap1.clone(), snap2.clone()]),
            Some(Snap::V2)
        );
        assert_eq!(Snap::announced(&[snap2, eth.clone()]), Some(Snap::V2));
        assert_eq!(Snap::announced(&[eth.clone(), snap1]), Some(Snap::V1));
        assert_eq!(Snap::announced(&[eth]), None);
        assert_eq!(Snap::announced(&[]), None);
    }

    #[tokio::test]
    async fn proofs_go_to_either_snap_version_and_bals_to_snap2_only() {
        let sessions = watch::Sender::new(Sessions::from([
            (A, session(Some(Snap::V1))),
            (PeerKey(B512::repeat_byte(3)), session(None)),
        ]));
        let (load, wire) = (Arc::new(SnapLoad::default()), Wire::default());

        let proof = wire.request(&load, &sessions);
        let no_snap2 = wire.bal(&load, &sessions).await.unwrap().unwrap_err();
        assert_eq!(
            no_snap2,
            NetError::NoPeer {
                capability: "snap/2"
            }
        );
        settle().await;
        assert_eq!(wire.versions(A), [Snap::V1]);

        sessions.send_modify(|sessions| {
            sessions.insert(B, session(Some(Snap::V2)));
        });
        let bals: Vec<_> = (0..4).map(|_| wire.bal(&load, &sessions)).collect();
        settle().await;
        let proofs: Vec<_> = (0..3).map(|_| wire.request(&load, &sessions)).collect();
        settle().await;
        assert_eq!(wire.versions(A), [Snap::V1; 4]);
        assert_eq!(wire.versions(B), [Snap::V2; 4]);

        wire.answer(A, 4);
        wire.answer(B, 4);
        for call in [proof].into_iter().chain(bals).chain(proofs) {
            assert!(call.await.unwrap().is_ok());
        }
        assert!(load.load.lock().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_fresh_snap1_session_gets_proofs_by_load_only_once_settled() {
        let sessions = snap_sessions(&[B]);
        let (load, wire) = (Arc::new(SnapLoad::default()), Wire::default());
        let mut calls: Vec<_> = (0..3).map(|_| wire.request(&load, &sessions)).collect();
        settle().await;

        sessions.send_modify(|sessions| {
            sessions.insert(A, fresh_snap1());
        });
        calls.push(wire.request(&load, &sessions));
        settle().await;
        assert_eq!((wire.outstanding(A), wire.outstanding(B)), (0, 4));

        tokio::time::advance(SNAP1_SETTLE).await;
        calls.extend((0..4).map(|_| wire.request(&load, &sessions)));
        settle().await;
        assert_eq!((wire.outstanding(A), wire.outstanding(B)), (4, 4));

        wire.answer(A, 4);
        wire.answer(B, 4);
        for call in calls {
            assert!(call.await.unwrap().is_ok());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_snap1_session_that_closes_within_a_second_never_gets_a_request() {
        let sessions = watch::Sender::new(Sessions::from([(A, fresh_snap1())]));
        let (load, wire) = (Arc::new(SnapLoad::default()), Wire::default());

        let call = wire.request(&load, &sessions);
        settle().await;
        tokio::time::advance(Duration::from_millis(999)).await;
        settle().await;
        assert_eq!(wire.versions(A), []);

        sessions.send_modify(|sessions| {
            sessions.remove(&A);
        });
        assert_eq!(
            call.await.unwrap().unwrap_err(),
            NetError::NoPeer {
                capability: "state (snap)"
            }
        );
        assert_eq!(wire.versions(A), []);
    }

    #[tokio::test(start_paused = true)]
    async fn with_only_a_fresh_snap1_session_a_proof_waits_for_it_to_settle_and_a_bal_has_no_peer()
    {
        let sessions = watch::Sender::new(Sessions::from([(A, fresh_snap1())]));
        let (load, wire) = (Arc::new(SnapLoad::default()), Wire::default());

        let no_snap2 = wire.bal(&load, &sessions).await.unwrap().unwrap_err();
        assert_eq!(
            no_snap2,
            NetError::NoPeer {
                capability: "snap/2"
            }
        );
        let proof = wire.request(&load, &sessions);
        settle().await;
        tokio::time::advance(SNAP1_SETTLE / 2).await;
        settle().await;
        assert_eq!(wire.versions(A), []);

        tokio::time::advance(SNAP1_SETTLE / 2 + Duration::from_millis(1)).await;
        settle().await;
        assert_eq!(wire.versions(A), [Snap::V1]);
        wire.answer(A, 1);
        assert!(proof.await.unwrap().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_settle_that_passes_after_the_check_wakes_the_wait_at_once() {
        let sessions = watch::Sender::new(Sessions::from([(A, fresh_snap1())]));
        let mut receiver = sessions.subscribe();
        tokio::time::advance(SNAP1_SETTLE - Duration::from_millis(1)).await;
        let checked = Instant::now();
        assert!(receiver.borrow()[&A].serving(checked).is_none());
        tokio::time::advance(Duration::from_millis(2)).await;

        let woke = Instant::now();
        sessions_changed(&mut receiver, checked).await.unwrap();
        assert_eq!(woke.elapsed(), Duration::ZERO);
    }

    fn code_request() -> SnapProtocolMessage {
        SnapProtocolMessage::GetByteCodes(GetByteCodesMessage {
            request_id: 0,
            hashes: vec![B256::repeat_byte(0x44)],
            response_bytes: 1024,
        })
    }

    fn snap1_submit(
        load: &Arc<SnapLoad>,
        sessions: &watch::Sender<Sessions>,
        snap1: &Snap1,
    ) -> JoinHandle<Result<(PeerId, SnapResponse), NetError>> {
        let (load, sessions, snap1) = (Arc::clone(load), sessions.subscribe(), snap1.clone());
        tokio::spawn(async move {
            load.submit(sessions, Snap::V1, |peer, _| {
                let (reply, retired) = snap1.send(peer, code_request());
                (reply, Some(retired))
            })
            .await
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_snap1_peer_holds_its_share_until_the_requests_time_out() {
        let sessions = watch::Sender::new(Sessions::from([(A, session(Some(Snap::V1)))]));
        let (load, snap1) = (Arc::new(SnapLoad::default()), Snap1::default());
        let (_to_chibi, inbound) = futures::channel::mpsc::unbounded::<BytesMut>();
        let mut connection = snap1.open(A.0, inbound);
        let sent = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&sent);
        tokio::spawn(async move {
            while connection.next().await.is_some() {
                *counted.lock() += 1;
            }
        });

        let abandoned: Vec<_> = (0..SNAP_PER_PEER)
            .map(|_| snap1_submit(&load, &sessions, &snap1))
            .collect();
        settle().await;
        assert_eq!(*sent.lock(), SNAP_PER_PEER);
        for call in abandoned {
            call.abort();
        }
        settle().await;

        let calls: Vec<_> = (0..SNAP_PER_PEER)
            .map(|_| snap1_submit(&load, &sessions, &snap1))
            .collect();
        settle().await;
        assert_eq!(*sent.lock(), SNAP_PER_PEER);

        tokio::time::advance(REQUEST_TIMEOUT).await;
        settle().await;
        assert_eq!(*sent.lock(), 2 * SNAP_PER_PEER);

        tokio::time::advance(REQUEST_TIMEOUT).await;
        for call in calls {
            assert_eq!(call.await.unwrap().unwrap_err(), NetError::Timeout);
        }
        assert!(load.load.lock().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_snap1_request_given_up_before_it_is_sent_is_never_sent() {
        let sessions = watch::Sender::new(Sessions::from([(A, session(Some(Snap::V1)))]));
        let (load, snap1) = (Arc::new(SnapLoad::default()), Snap1::default());
        let (_to_chibi, inbound) = futures::channel::mpsc::unbounded::<BytesMut>();
        let mut connection = snap1.open(A.0, inbound);

        let call = snap1_submit(&load, &sessions, &snap1);
        settle().await;
        call.abort();
        settle().await;
        assert!(
            connection
                .poll_next_unpin(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        settle().await;
        assert!(load.load.lock().is_empty());
    }

    #[test]
    fn the_census_counts_state_peers_of_either_snap_version() {
        let sessions = Sessions::from([
            (
                A,
                Session {
                    dialable: Some(node(1)),
                    ..session(Some(Snap::V1))
                },
            ),
            (
                B,
                Session {
                    dialable: Some(node(2)),
                    ..session(Some(Snap::V2))
                },
            ),
            (PeerKey(B512::repeat_byte(3)), session(Some(Snap::V2))),
            (PeerKey(B512::repeat_byte(4)), session(None)),
        ]);
        let census = census(&sessions);
        assert_eq!((census.peers, census.state_peers), (4, 3));
        let mut records = census.snap_records;
        records.sort_by_key(|record| record.id);
        assert_eq!(records, [node(1), node(2)]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_census_neither_counts_nor_remembers_a_fresh_snap1_session() {
        let sessions = Sessions::from([
            (
                A,
                Session {
                    dialable: Some(node(1)),
                    ..fresh_snap1()
                },
            ),
            (
                B,
                Session {
                    dialable: Some(node(2)),
                    since: Instant::now(),
                    ..session(Some(Snap::V2))
                },
            ),
        ]);
        let census = census(&sessions);
        assert_eq!((census.peers, census.state_peers), (2, 1));
        assert_eq!(census.snap_records, [node(2)]);
    }

    const PEER: PeerId = B512::repeat_byte(7);
    const HASH: B256 = B256::repeat_byte(1);

    /// Hands `answer` a request built around a fresh response channel, with `store` announced,
    /// and returns the reply it sent.
    fn answered<T>(
        store: Option<&Store>,
        request: impl FnOnce(oneshot::Sender<RequestResult<T>>) -> IncomingEthRequest,
    ) -> RequestResult<T> {
        let (response, mut reply) = oneshot::channel();
        super::answer(request(response), store);
        reply
            .try_recv()
            .expect("answer must reply before returning")
    }

    /// A chain of six blocks from genesis and a store anchored at its head holding every
    /// block but genesis, with the block hashes by height above genesis.
    fn window() -> (World, Vec<B256>, Store) {
        let mut world = World::genesis([]);
        let mut hashes = vec![world.head()];
        for _ in 0..5 {
            hashes.push(world.mine(BlockWrites::default()));
        }
        let head = world.verified(hashes[5]);
        let between = world.headers_between(hashes[0], hashes[5]);
        let mut store = Store::new(head.clone());
        store
            .insert_ancestors(chibi_core::verify::ancestors(&head, between).unwrap())
            .unwrap();
        (world, hashes, store)
    }

    fn headers(
        store: Option<&Store>,
        start: impl Into<BlockHashOrNumber>,
        limit: u64,
        skip: u32,
        direction: HeadersDirection,
    ) -> Vec<Header> {
        let request = GetBlockHeaders {
            start_block: start.into(),
            limit,
            skip,
            direction,
        };
        answered(store, |response| IncomingEthRequest::GetBlockHeaders {
            peer_id: PEER,
            request,
            response,
        })
        .unwrap()
        .0
    }

    #[test]
    fn header_requests_are_answered_from_the_held_window_only() {
        use HeadersDirection::{Falling, Rising};
        let (world, hashes, store) = window();
        let at = |heights: &[usize]| -> Vec<Header> {
            heights.iter().map(|&h| world.header(hashes[h])).collect()
        };
        let number = |height: u64| GENESIS_NUMBER + height;
        let held = Some(&store);
        assert_eq!(headers(held, number(2), 3, 0, Rising), at(&[2, 3, 4]));
        assert_eq!(headers(held, hashes[5], 16, 1, Falling), at(&[5, 3, 1]));
        assert_eq!(headers(held, number(1), 4, 2, Rising), at(&[1, 4]));
        assert_eq!(headers(held, hashes[3], 1, 0, Falling), at(&[3]));
        assert_eq!(headers(held, number(4), 16, 0, Rising), at(&[4, 5]));
        assert_eq!(headers(held, hashes[2], 16, 0, Falling), at(&[2, 1]));
        assert_eq!(headers(held, number(5), 16, u32::MAX, Falling), at(&[5]));
        assert_eq!(headers(held, number(5), 16, u32::MAX, Rising), at(&[5]));
        assert_eq!(headers(held, number(5), 0, 0, Falling), at(&[]));
        assert_eq!(headers(held, number(0), 16, 0, Rising), at(&[]));
        assert_eq!(headers(held, hashes[0], 16, 0, Rising), at(&[]));
        assert_eq!(headers(held, number(6), 16, 0, Falling), at(&[]));
        assert_eq!(headers(held, HASH, 16, 0, Falling), at(&[]));
        assert_eq!(headers(None, hashes[5], 16, 0, Falling), at(&[]));
    }

    #[test]
    fn the_block_range_message_decodes_to_the_head_as_earliest_and_latest() {
        let hash = b256!("0x7972d020ff8e1aeaaaa90febfa53ed72217398cbacb826f39e533c4d79c2fd8a");
        let header = Header {
            number: 11_875_421,
            timestamp: 1_791_525_744,
            ..Header::default()
        };
        let head = chibi_core::verify::header(TrustedHash::attested(hash), header).unwrap();
        let expected = BlockRangeUpdate {
            earliest: 11_875_421,
            latest: 11_875_421,
            latest_hash: hash,
        };

        let (update, message) = block_range(&head);
        assert_eq!(update, expected);
        let encoded = message.encoded();
        let decoded = ProtocolMessage::<EthNetworkPrimitives>::decode_message(
            EthVersion::Eth69,
            &mut &encoded[..],
        )
        .expect("a well-formed eth/69 message");
        assert_eq!(decoded.message, EthMessage::BlockRangeUpdate(expected));
    }

    #[test]
    fn every_other_eth_request_gets_an_empty_reply_of_its_type() {
        let (_, hashes, store) = window();
        let (head, held) = (hashes[5], Some(&store));
        assert_eq!(
            answered(held, |response| IncomingEthRequest::GetBlockBodies {
                peer_id: PEER,
                request: GetBlockBodies(vec![head]),
                response,
            }),
            Ok(BlockBodies(Vec::new())),
        );
        assert_eq!(
            answered(held, |response| IncomingEthRequest::GetNodeData {
                peer_id: PEER,
                request: GetNodeData(vec![head]),
                response,
            }),
            Ok(NodeData(Vec::new())),
        );
        assert_eq!(
            answered(held, |response| IncomingEthRequest::GetReceipts {
                peer_id: PEER,
                request: GetReceipts(vec![head]),
                response,
            }),
            Ok(Receipts(Vec::new())),
        );
        assert_eq!(
            answered(held, |response| IncomingEthRequest::GetReceipts69 {
                peer_id: PEER,
                request: GetReceipts(vec![head]),
                response,
            }),
            Ok(Receipts69(Vec::new())),
        );
        assert_eq!(
            answered(held, |response| IncomingEthRequest::GetReceipts70 {
                peer_id: PEER,
                request: GetReceipts70 {
                    first_block_receipt_index: 3,
                    block_hashes: vec![head]
                },
                response,
            }),
            Ok(Receipts70 {
                last_block_incomplete: false,
                receipts: Vec::new()
            }),
        );
        assert_eq!(
            answered(held, |response| IncomingEthRequest::GetBlockAccessLists {
                peer_id: PEER,
                request: GetBlockAccessLists(vec![head]),
                response,
            }),
            Ok(BlockAccessLists(Vec::new())),
        );
        assert_eq!(
            answered(held, |response| IncomingEthRequest::GetCells {
                peer_id: PEER,
                request: GetCells {
                    hashes: vec![head],
                    cell_mask: B128::repeat_byte(0xff)
                },
                response,
            }),
            Ok(Cells::default()),
        );
    }

    #[test]
    fn every_snap_request_gets_an_empty_reply_keeping_its_id() {
        let requests = [
            SnapProtocolMessage::GetAccountRange(GetAccountRangeMessage {
                request_id: 11,
                root_hash: HASH,
                starting_hash: B256::ZERO,
                limit_hash: B256::repeat_byte(0xff),
                response_bytes: RESPONSE_BYTES,
            }),
            SnapProtocolMessage::GetStorageRanges(GetStorageRangesMessage {
                request_id: 12,
                root_hash: HASH,
                account_hashes: vec![HASH],
                starting_hash: HASH.into(),
                limit_hash: HASH.into(),
                response_bytes: RESPONSE_BYTES,
            }),
            SnapProtocolMessage::GetByteCodes(GetByteCodesMessage {
                request_id: 13,
                hashes: vec![HASH],
                response_bytes: RESPONSE_BYTES,
            }),
            SnapProtocolMessage::GetBlockAccessLists(GetBlockAccessListsMessage {
                request_id: 14,
                block_hashes: vec![HASH],
                response_bytes: RESPONSE_BYTES,
            }),
        ];
        let expected = [
            SnapResponse::AccountRange(AccountRangeMessage {
                request_id: 11,
                accounts: Vec::new(),
                proof: Vec::new(),
            }),
            SnapResponse::StorageRanges(StorageRangesMessage {
                request_id: 12,
                slots: Vec::new(),
                proof: Vec::new(),
            }),
            SnapResponse::ByteCodes(ByteCodesMessage {
                request_id: 13,
                codes: Vec::new(),
            }),
            SnapResponse::BlockAccessLists(BlockAccessListsMessage {
                request_id: 14,
                block_access_lists: BlockAccessLists(Vec::new()),
            }),
        ];
        for (request, expected) in requests.into_iter().zip(expected) {
            let reply = answered(None, |response| IncomingEthRequest::GetSnap {
                peer_id: PEER,
                request,
                response,
            });
            assert_eq!(reply, Ok(expected));
        }
    }

    #[test]
    fn bal_replies_align_to_the_request() {
        let bal = Bytes::from_static(&[0xc0]);
        assert_eq!(
            align(vec![Some(bal.clone())], 3),
            Some(vec![Some(bal.clone()), None, None])
        );
        assert_eq!(align(Vec::new(), 2), Some(vec![None, None]));
        assert_eq!(align(vec![Some(bal), None], 1), None);
    }

    #[test]
    fn bal_errors_prefer_the_informative_failure() {
        let no_snap = || NetError::NoPeer {
            capability: "snap/2",
        };
        let no_eth = || NetError::NoPeer {
            capability: "eth/71",
        };
        assert_eq!(
            both_failed(no_snap(), no_eth()),
            NetError::NoPeer {
                capability: BAL_SOURCES
            }
        );
        assert_eq!(both_failed(no_snap(), NetError::Timeout), NetError::Timeout);
        assert_eq!(
            both_failed(NetError::Rejected, no_eth()),
            NetError::Rejected
        );
    }

    fn node(byte: u8) -> NodeRecord {
        NodeRecord::new(([10, 0, 0, byte], 30303).into(), B512::repeat_byte(byte))
    }

    #[test]
    fn the_default_net_config_keeps_the_networks_peers_and_reths_limits() {
        let spec = Network::Sepolia.spec();
        let net = NetConfig::default();
        assert_eq!(net.boot_nodes(&spec), spec.boot_nodes.as_slice());
        assert_eq!(
            net.known_peers(&spec, vec![node(9)]),
            spec.state_peers
                .iter()
                .copied()
                .chain([node(9)])
                .collect::<Vec<_>>()
        );
        let peers = net.peers_config();
        let defaults = PeersConfig::default();
        assert!(peers.trusted_nodes.is_empty() && !peers.trusted_nodes_only);
        assert_eq!(
            (
                peers.connection_info.max_outbound,
                peers.connection_info.max_inbound
            ),
            (100, 30)
        );
        assert_eq!(
            peers.connection_info.max_concurrent_outbound_dials,
            defaults.connection_info.max_concurrent_outbound_dials
        );
    }

    #[test]
    fn configured_peers_replace_and_restrict_the_networks() {
        let spec = Network::Sepolia.spec();
        let net = NetConfig {
            trusted_peers: vec![node(1), node(2)],
            boot_nodes: Some(vec![node(3)]),
            max_outbound: Some(5),
            max_inbound: Some(0),
            ..NetConfig::default()
        };
        assert_eq!(net.boot_nodes(&spec), [node(3)]);
        assert_eq!(net.known_peers(&spec, Vec::new()), spec.state_peers);
        let peers = net.peers_config();
        assert_eq!(
            peers.trusted_nodes,
            vec![TrustedPeer::from(node(1)), TrustedPeer::from(node(2))]
        );
        assert!(!peers.trusted_nodes_only);
        assert_eq!(
            (
                peers.connection_info.max_outbound,
                peers.connection_info.max_inbound
            ),
            (5, 0)
        );

        let only = NetConfig {
            trusted_peers: vec![node(1)],
            trusted_only: true,
            ..NetConfig::default()
        };
        assert_eq!(only.boot_nodes(&spec), []);
        assert_eq!(only.known_peers(&spec, vec![node(9)]), []);
        assert!(only.peers_config().trusted_nodes_only);
        let own_boot = NetConfig {
            boot_nodes: Some(vec![node(3)]),
            ..only
        };
        assert_eq!(own_boot.boot_nodes(&spec), [node(3)]);
    }
}
