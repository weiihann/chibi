//! An offline chain for tests: [`Peers`] over `chibi_core::testing::World`, with faults a real
//! network produces, and light-client heads driven by hand.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alloy_consensus::Header;
use alloy_eips::eip2718::Encodable2718 as _;
use alloy_primitives::{Address, B256, B512, Bytes, U256, keccak256};
use chibi_core::TrustedHash;
use chibi_core::state::Store;
use chibi_core::testing::{BlockWrites, GENESIS_NUMBER, World, WorldAccount};
use chibi_core::wire::{
    AccountRangeRequest, AccountReply, Body, Receipt, StorageRangeRequest, StorageReply,
    Transaction,
};
use parking_lot::Mutex;
use tokio::sync::{Semaphore, oneshot, watch};

use crate::disk::read_state;
use crate::heads::{Heads, HeadsSender};
use crate::network::{Network, NetworkSpec, Token};
use crate::peers::{Census, NetError, PeerKey, Peers, Reply};

/// The peer that answers truthfully.
pub const HONEST: PeerKey = PeerKey(B512::repeat_byte(0x11));

/// The peer that sends forged replies.
pub const LIAR: PeerKey = PeerKey(B512::repeat_byte(0x66));

/// A fault the fixture injects into its replies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Account replies for this address lose a proof node.
    DropProofNode(Address),
    /// Replies for this address come back empty, as from a peer outside its snap window.
    Unavailable(Address),
    /// Storage replies for this slot of this address come back empty.
    WithholdSlot(Address, U256),
    /// BAL requests get no lists.
    WithholdBals,
    /// Body requests fail.
    WithholdBodies,
    /// The next account reply for this address claims this balance, sent by [`LIAR`].
    LieAboutBalance(Address, U256),
    /// The census counts no state peer.
    NoStatePeers,
    /// The next this many header requests fail as rejected.
    RejectHeaders(u32),
    /// Header requests fail as if the network had stopped.
    StopHeaders,
}

/// A transaction the fixture was asked to broadcast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Broadcast {
    /// The transaction.
    pub transaction: Transaction,
    /// Whether the saved outbox held it when the broadcast happened.
    pub persisted: bool,
}

/// What the fixture observed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Log {
    /// Peers penalized, in order.
    pub penalized: Vec<PeerKey>,
    /// Account, storage, and code requests served.
    pub proofs: usize,
    /// `keccak256(address)` of every account request served, in order.
    pub accounts: Vec<B256>,
    /// `(keccak256(address), keccak256(slot))` of every storage request served, in order.
    pub slots: Vec<(B256, B256)>,
    /// Most proof requests sent and not yet answered at once, while [`Fixture::serve_proofs`]
    /// is in effect.
    pub most_in_flight: usize,
    /// Block bodies served.
    pub bodies: usize,
    /// Broadcasts, in order.
    pub broadcasts: Vec<Broadcast>,
}

/// How proof requests are served after [`Fixture::serve_proofs`].
#[derive(Clone, Debug)]
struct Wire {
    /// How long each request takes once the peer works on it.
    service: Duration,
    /// One permit per request the peer works on at once, handed out in arrival order.
    line: Arc<Semaphore>,
}

#[derive(Debug, Default)]
struct Faults {
    drop_proof_node: HashSet<B256>,
    unavailable: HashSet<B256>,
    withheld_slots: HashSet<(B256, B256)>,
    withhold_bals: bool,
    withhold_bodies: bool,
    lie: Option<(B256, U256)>,
    no_state_peers: bool,
    rejected_headers: u32,
    stop_headers: bool,
}

#[derive(Debug)]
struct Inner {
    world: Mutex<World>,
    faults: Mutex<Faults>,
    log: Mutex<Log>,
    outbox_dir: Mutex<Option<PathBuf>>,
    /// While true, proof requests wait.
    holding: watch::Sender<bool>,
    /// Proof requests that have waited at the gate.
    parked: watch::Sender<usize>,
    /// While set, proof requests are served at most a few at a time, each taking a while.
    wire: Mutex<Option<Wire>>,
    /// Proof requests sent over the wire and not yet answered.
    in_flight: Mutex<usize>,
}

/// An offline chain and the light client watching it.
#[derive(Debug)]
pub struct Fixture {
    inner: Arc<Inner>,
    sender: HeadsSender,
    heads: Heads,
}

impl Fixture {
    /// A chain whose genesis holds `accounts`, with genesis published as the latest head.
    pub fn genesis(accounts: impl IntoIterator<Item = (Address, WorldAccount)>) -> Self {
        let world = World::genesis(accounts);
        let head = world.head();
        let (sender, heads) = Heads::channel();
        sender
            .latest
            .send_replace(Some(TrustedHash::attested(head)));
        Self {
            inner: Arc::new(Inner {
                world: Mutex::new(world),
                faults: Mutex::new(Faults::default()),
                log: Mutex::new(Log::default()),
                outbox_dir: Mutex::new(None),
                holding: watch::Sender::new(false),
                parked: watch::Sender::new(0),
                wire: Mutex::new(None),
                in_flight: Mutex::new(0),
            }),
            sender,
            heads,
        }
    }

    /// The network spec matching the world's chain, tracking `tokens`.
    pub fn spec(tokens: &'static [Token]) -> NetworkSpec {
        NetworkSpec {
            tokens,
            ..Network::Mainnet.spec()
        }
    }

    /// A [`Peers`] serving this chain.
    pub fn peers(&self) -> FixturePeers {
        FixturePeers {
            inner: Arc::clone(&self.inner),
        }
    }

    /// The light client's view of this chain.
    pub fn heads(&self) -> Heads {
        self.heads.clone()
    }

    /// Mines a block with a BAL and publishes it as the latest head.
    pub fn mine(&self, writes: BlockWrites) -> B256 {
        let hash = self.inner.world.lock().mine(writes);
        self.publish(hash);
        hash
    }

    /// Mines a block without a BAL hash and publishes it as the latest head.
    pub fn mine_without_bal(&self, writes: BlockWrites) -> B256 {
        let hash = self.inner.world.lock().mine_without_bal(writes);
        self.publish(hash);
        hash
    }

    /// Mines a block with a BAL without publishing it, as when heads arrive faster than the
    /// follower steps.
    pub fn mine_unseen(&self, writes: BlockWrites) -> B256 {
        self.inner.world.lock().mine(writes)
    }

    /// Mines a sibling of the head with a BAL and publishes it as the latest head.
    pub fn fork_head(&self, writes: BlockWrites) -> B256 {
        let hash = self.inner.world.lock().fork_head(writes);
        self.publish(hash);
        hash
    }

    /// Publishes `hash` as the latest attested head.
    pub fn publish(&self, hash: B256) {
        self.sender
            .latest
            .send_replace(Some(TrustedHash::attested(hash)));
    }

    /// Publishes `hash` as the finalized head.
    pub fn finalize(&self, hash: B256) {
        self.sender
            .finalized
            .send_replace(Some(TrustedHash::attested(hash)));
    }

    /// Reads the world.
    pub fn world<R>(&self, f: impl FnOnce(&World) -> R) -> R {
        f(&self.inner.world.lock())
    }

    /// Injects a fault.
    pub fn fault(&self, fault: Fault) {
        let mut faults = self.inner.faults.lock();
        match fault {
            Fault::DropProofNode(address) => {
                faults.drop_proof_node.insert(keccak256(address));
            }
            Fault::Unavailable(address) => {
                faults.unavailable.insert(keccak256(address));
            }
            Fault::WithholdSlot(address, slot) => {
                faults
                    .withheld_slots
                    .insert((keccak256(address), keccak256(B256::from(slot))));
            }
            Fault::WithholdBals => faults.withhold_bals = true,
            Fault::WithholdBodies => faults.withhold_bodies = true,
            Fault::NoStatePeers => faults.no_state_peers = true,
            Fault::RejectHeaders(count) => faults.rejected_headers = count,
            Fault::StopHeaders => faults.stop_headers = true,
            Fault::LieAboutBalance(address, balance) => {
                faults.lie = Some((keccak256(address), balance));
            }
        }
    }

    /// Removes every fault.
    pub fn heal(&self) {
        *self.inner.faults.lock() = Faults::default();
    }

    /// What the fixture observed so far.
    pub fn log(&self) -> Log {
        self.inner.log.lock().clone()
    }

    /// Makes each broadcast record whether the state saved in `dir` already held the
    /// transaction in its outbox.
    pub fn watch_outbox(&self, dir: PathBuf) {
        *self.inner.outbox_dir.lock() = Some(dir);
    }

    /// Makes proof requests wait until [`Fixture::release_proofs`].
    pub fn hold_proofs(&self) {
        self.inner.holding.send_replace(true);
    }

    /// Waits until a proof request is waiting at the gate.
    pub async fn proof_held(&self) {
        let mut parked = self.inner.parked.subscribe();
        parked
            .wait_for(|parked| *parked > 0)
            .await
            .map(|_| ())
            .unwrap_or_default();
    }

    /// Lets waiting and later proof requests through.
    pub fn release_proofs(&self) {
        self.inner.holding.send_replace(false);
    }

    /// Serves proof requests like one snap/2 peer working on up to `at_once` of them, each
    /// taking `service`, in arrival order. A request already sent is served even after its
    /// caller stopped waiting.
    pub fn serve_proofs(&self, at_once: usize, service: Duration) {
        *self.inner.wire.lock() = Some(Wire {
            service,
            line: Arc::new(Semaphore::new(at_once)),
        });
    }
}

/// [`Peers`] over a [`Fixture`]'s world.
#[derive(Clone, Debug)]
pub struct FixturePeers {
    inner: Arc<Inner>,
}

impl FixturePeers {
    fn honest<T>(value: T) -> Result<Reply<T>, NetError> {
        Ok(Reply {
            peer: HONEST,
            value,
        })
    }

    fn proof_served(&self) {
        self.inner.log.lock().proofs += 1;
    }

    async fn pass_gate(&self) {
        let mut holding = self.inner.holding.subscribe();
        if *holding.borrow_and_update() {
            self.inner.parked.send_modify(|parked| *parked += 1);
            holding
                .wait_for(|holding| !*holding)
                .await
                .map(|_| ())
                .unwrap_or_default();
        }
    }

    /// Runs `serve` now, or, while proofs are served over the wire, once the peer has room and
    /// the service time has passed, whether or not the caller is still waiting.
    async fn on_the_wire<T: Send + 'static>(
        &self,
        serve: impl FnOnce(&Self) -> T + Send + 'static,
    ) -> Result<T, NetError> {
        let Some(wire) = self.inner.wire.lock().clone() else {
            return Ok(serve(self));
        };
        {
            let mut in_flight = self.inner.in_flight.lock();
            *in_flight += 1;
            let mut log = self.inner.log.lock();
            log.most_in_flight = log.most_in_flight.max(*in_flight);
        }
        let (reply, answer) = oneshot::channel();
        let peers = self.clone();
        let mut queued = Box::pin(async move {
            let _turn = wire.line.acquire().await;
            tokio::time::sleep(wire.service).await;
            let value = serve(&peers);
            *peers.inner.in_flight.lock() -= 1;
            reply.send(value).ok();
        });
        // Join the line before returning, so requests queue in the order they were made.
        if futures::poll!(queued.as_mut()).is_pending() {
            tokio::spawn(queued);
        }
        answer.await.map_err(|_| NetError::Stopped)
    }

    fn account_reply(
        &self,
        request: &AccountRangeRequest,
    ) -> Result<Reply<AccountReply>, NetError> {
        self.proof_served();
        self.inner.log.lock().accounts.push(request.key);
        let mut faults = self.inner.faults.lock();
        if faults.unavailable.contains(&request.key) {
            return Self::honest(AccountReply::default());
        }
        let mut reply = self.inner.world.lock().account_reply(request);
        if faults.drop_proof_node.contains(&request.key) && !reply.proof.is_empty() {
            reply.proof.remove(0);
        }
        if let Some((key, balance)) = faults.lie.filter(|(key, _)| *key == request.key) {
            faults.lie = None;
            if let Some((_, account)) = reply.accounts.iter_mut().find(|(k, _)| *k == key) {
                account.balance = balance;
            }
            return Ok(Reply {
                peer: LIAR,
                value: reply,
            });
        }
        Self::honest(reply)
    }

    fn storage_reply(
        &self,
        request: &StorageRangeRequest,
    ) -> Result<Reply<StorageReply>, NetError> {
        self.proof_served();
        self.inner
            .log
            .lock()
            .slots
            .push((request.account, request.slot));
        let withheld = {
            let faults = self.inner.faults.lock();
            faults.unavailable.contains(&request.account)
                || faults
                    .withheld_slots
                    .contains(&(request.account, request.slot))
        };
        if withheld {
            return Self::honest(StorageReply::default());
        }
        Self::honest(self.inner.world.lock().storage_reply(request))
    }

    fn code_reply(&self, hashes: &[B256]) -> Result<Reply<Vec<Bytes>>, NetError> {
        self.proof_served();
        let world = self.inner.world.lock();
        Self::honest(hashes.iter().filter_map(|hash| world.code(*hash)).collect())
    }
}

impl Peers for FixturePeers {
    async fn headers(&self, start: B256, count: u64) -> Result<Reply<Vec<Header>>, NetError> {
        {
            let mut faults = self.inner.faults.lock();
            if faults.stop_headers {
                return Err(NetError::Stopped);
            }
            if faults.rejected_headers > 0 {
                faults.rejected_headers -= 1;
                return Err(NetError::Rejected);
            }
        }
        let world = self.inner.world.lock();
        let mut headers = Vec::new();
        let mut cursor = start;
        if start == B256::ZERO {
            return Self::honest(headers);
        }
        while (headers.len() as u64) < count {
            let header = world.header(cursor);
            cursor = header.parent_hash;
            let at_genesis = header.number == GENESIS_NUMBER;
            headers.push(header);
            if at_genesis {
                break;
            }
        }
        Self::honest(headers)
    }

    async fn account_range(
        &self,
        request: AccountRangeRequest,
    ) -> Result<Reply<AccountReply>, NetError> {
        self.pass_gate().await;
        self.on_the_wire(move |peers| peers.account_reply(&request))
            .await?
    }

    async fn storage_range(
        &self,
        request: StorageRangeRequest,
    ) -> Result<Reply<StorageReply>, NetError> {
        self.pass_gate().await;
        self.on_the_wire(move |peers| peers.storage_reply(&request))
            .await?
    }

    async fn byte_codes(&self, hashes: Vec<B256>) -> Result<Reply<Vec<Bytes>>, NetError> {
        self.pass_gate().await;
        self.on_the_wire(move |peers| peers.code_reply(&hashes))
            .await?
    }

    async fn block_access_lists(
        &self,
        blocks: Vec<B256>,
    ) -> Result<Reply<Vec<Option<Bytes>>>, NetError> {
        if self.inner.faults.lock().withhold_bals {
            return Self::honest(vec![None; blocks.len()]);
        }
        let world = self.inner.world.lock();
        Self::honest(blocks.iter().map(|hash| world.bal(*hash)).collect())
    }

    async fn bodies(&self, blocks: Vec<B256>) -> Result<Reply<Vec<Body>>, NetError> {
        if self.inner.faults.lock().withhold_bodies {
            return Err(NetError::Rejected);
        }
        self.inner.log.lock().bodies += blocks.len();
        let world = self.inner.world.lock();
        Self::honest(blocks.iter().map(|hash| world.body(*hash)).collect())
    }

    async fn receipts(&self, block: B256) -> Result<Reply<Vec<Receipt>>, NetError> {
        Self::honest(self.inner.world.lock().receipts(block))
    }

    async fn broadcast(&self, transaction: Transaction) -> Result<usize, NetError> {
        let dir = self.inner.outbox_dir.lock().clone();
        let raw = Bytes::from(transaction.encoded_2718());
        let persisted = dir
            .and_then(|dir| read_state(&dir).ok().flatten())
            .is_some_and(|file| file.outbox().iter().any(|saved| saved.raw == raw));
        self.inner.log.lock().broadcasts.push(Broadcast {
            transaction,
            persisted,
        });
        Ok(1)
    }

    fn penalize(&self, peer: PeerKey) {
        self.inner.log.lock().penalized.push(peer);
    }

    fn announce(&self, _store: &Arc<Store>) {}

    fn census(&self) -> Census {
        Census {
            peers: 1,
            state_peers: usize::from(!self.inner.faults.lock().no_state_peers),
            snap_records: Vec::new(),
        }
    }
}
