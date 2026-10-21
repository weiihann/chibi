//! One follower step: reach the light client's newest head, warm the working set, follow
//! finality, hand account history how the store moved, locate submitted transactions, and
//! persist. Account history does its fetches and writes in a task of its own.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, TxHash, U256};
use chibi_core::history::{self, Capability, OpenState, Pending, Scan, ScannedBlock};
use chibi_core::state::{HEADER_WINDOW, Key, Plan, State, Store};
use chibi_core::tx::Lookup;
use chibi_core::verify::{VerifiedBody, VerifiedReceipts};
use chibi_core::{TrustedHash, VerifiedHeader};
use futures::StreamExt as _;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::fetch;
use crate::heads::Heads;
use crate::node::{Follower, NodeError, Shared, Stepped, blocking};
use crate::peers::{NetError, Paced, Peers};

/// Heads between re-broadcasts of transactions not yet found in a block.
const REBROADCAST_EVERY: u64 = 5;

/// Deepest a search reaches below the anchor for a transaction's submission block. Older
/// submissions stay pending until they are found; they are never pruned as replaced.
const DEEP_SEARCH: u64 = 8192;

/// Headers asked for per request while walking back.
const ANCESTOR_BATCH: u64 = 512;

/// Bodies fetched, searched, and released per batch, so a deep search holds few bodies.
const BODY_BATCH: usize = 64;

/// Proofs a warm pass gathers before merging them, so requests see its progress early.
const WARM_MERGE: usize = 64;

/// Warm requests in flight at once: half of one state peer's slots, so a wallet request or a
/// BAL fetch always finds the other half.
const WARM_IN_FLIGHT: usize = 8;

/// Most headers one history reconcile fetches below the anchor.
const RECONCILE_CAP: u64 = 4096;

/// Most blocks waiting for their contents. Past it the oldest are dropped and become a
/// coverage gap, so peers that withhold contents cannot grow the queue without bound.
const HISTORY_QUEUE: usize = 64;

/// Most headers the backward walk fetches in one follower step.
const BACKFILL_HEADERS: u64 = 512;

/// Most blocks whose contents the backward walk fetches in one follower step.
const BACKFILL_CONTENTS: usize = 16;

/// Follower steps in which a block's contents may fail to arrive before the backward walk
/// leaves the block uncovered.
const BACKFILL_ATTEMPTS: u32 = 3;

/// Longest one follower step waits for history work. Work still running then goes on in its
/// own task while the follower moves on.
pub(crate) const HISTORY_BUDGET: Duration = Duration::from_secs(2);

/// Which blocks have been searched for pending transactions.
///
/// A pending transaction is dropped as replaced only once every block since its submission
/// was searched, so a failed fetch or a long absence delays the search instead of losing it.
#[derive(Debug, Default)]
pub(crate) struct Search {
    /// Every block at or below this one on its chain is covered back to `covered_from`.
    /// `None` after a restart that restored transactions.
    floor: Option<BlockNumHash>,
    /// Every block from this height up to the floor was searched, or predates every
    /// submission still pending. A transaction submitted at or above it can be pruned.
    covered_from: Option<u64>,
    /// Blocks above the floor whose bodies were searched.
    searched: HashSet<B256>,
    /// Verified headers above the floor that the store may not hold, by hash.
    known: HashMap<B256, VerifiedHeader>,
    /// Transactions already reported as out of the search's reach.
    unreachable: HashSet<TxHash>,
}

impl Search {
    /// A search with everything up to `floor` covered, or nothing when `floor` is `None`.
    pub(crate) fn new(floor: Option<BlockNumHash>) -> Self {
        Self {
            floor,
            covered_from: floor.map(|floor| floor.number),
            ..Self::default()
        }
    }

    /// Marks everything up to `anchor` covered.
    fn settle(&mut self, anchor: BlockNumHash) {
        self.floor = Some(anchor);
        self.searched.clear();
        self.known.clear();
    }

    /// Whether a transaction submitted at `since` was searched for in every block it could
    /// sit in.
    fn covers(&self, since: BlockNumHash) -> bool {
        self.covered_from.is_some_and(|from| since.number >= from)
    }
}

/// How a walk back from the anchor ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    /// It met the floor; coverage below it still holds.
    Floor,
    /// It reached the oldest submission searched for.
    Target(u64),
    /// A header could not be fetched.
    Short,
}

/// What one extension hands the history recorder: its blocks' scans, taken before the store
/// consumed their BALs, and the watched accounts' proven state at the block they extend.
#[derive(Debug)]
pub(crate) struct Extension {
    /// Each block's scan, oldest first.
    scans: Vec<Scan>,
    /// The watched accounts the anchor's store held, at the anchor.
    opens: Vec<OpenState>,
}

/// How a follower step moved the store, as history needs to know it.
#[derive(Debug)]
pub(crate) enum Moved {
    /// The store extended through these blocks.
    Extended(Extension),
    /// The store re-anchored, so stored history may have left the followed chain.
    Reanchored,
}

/// Account history beside the follower. Its work runs in a task of its own, one run at a time,
/// so a slow or silent peer delays only history. Each follower step hands it how the store
/// moved, in step order, starts a run when none is going, and waits for runs at most
/// [`HISTORY_BUDGET`]. A run is never cancelled before it ends, so no fetch or write is ever
/// cut short; it exists only while history is on.
#[derive(Debug)]
pub(crate) struct HistoryTask {
    /// The recorder while no run holds it. Exactly one of it and `running` is set.
    idle: Option<Box<Recorder>>,
    /// The run holding the recorder.
    running: Option<Running>,
    /// Moves not yet handed to a run, oldest first.
    backlog: Vec<Moved>,
}

/// A history run in its own task, handing the recorder back when it ends. Dropping it aborts
/// the run: the node is going away, and each write is one transaction, so none is half done.
#[derive(Debug)]
struct Running(JoinHandle<Box<Recorder>>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Running {
    fn spawn<P: Peers>(
        shared: Arc<Shared<P>>,
        mut recorder: Box<Recorder>,
        moves: Vec<Moved>,
    ) -> Self {
        Self(tokio::spawn(async move {
            recorder.run(&shared, moves).await;
            recorder
        }))
    }

    /// The recorder once the run ends, or `None` when `deadline` passes or a newer head
    /// arrives first; the run goes on either way.
    async fn ended(
        &mut self,
        deadline: Instant,
        latest: &mut watch::Receiver<Option<TrustedHash>>,
    ) -> Option<Box<Recorder>> {
        tokio::select! {
            biased;
            ended = &mut self.0 => Some(ended.unwrap_or_else(|err| {
                warn!(target: "chibi::history", %err, "history run failed; starting over");
                Box::default()
            })),
            Ok(()) = latest.changed() => None,
            () = tokio::time::sleep_until(deadline) => None,
        }
    }
}

impl HistoryTask {
    /// History whose first run reconciles and opens coverage.
    pub(crate) fn new() -> Self {
        Self {
            idle: Some(Box::default()),
            running: None,
            backlog: Vec::new(),
        }
    }

    /// Hands history how this step moved the store, then gives runs at most
    /// [`HISTORY_BUDGET`], less if `latest` moves: a run that ended hands back the recorder,
    /// and an idle recorder starts a run with everything handed since the last one. Every
    /// await leaves the recorder or its run stored, so dropping this future loses nothing.
    async fn step<P: Peers>(
        &mut self,
        shared: &Arc<Shared<P>>,
        moved: Option<Moved>,
        mut latest: watch::Receiver<Option<TrustedHash>>,
    ) {
        self.backlog.extend(moved);
        let deadline = Instant::now() + HISTORY_BUDGET;
        if let Some(running) = &mut self.running {
            let Some(recorder) = running.ended(deadline, &mut latest).await else {
                return;
            };
            self.idle = Some(recorder);
            self.running = None;
        }
        if let Some(recorder) = self.idle.take() {
            let moves = std::mem::take(&mut self.backlog);
            self.running = Some(Running::spawn(Arc::clone(shared), recorder, moves));
        }
        if let Some(running) = &mut self.running
            && let Some(recorder) = running.ended(deadline, &mut latest).await
        {
            self.idle = Some(recorder);
            self.running = None;
        }
    }
}

/// Account history: a run records each extended block once its scan is complete,
/// reconciling after a re-anchor, then walks back to fill older blocks.
#[derive(Debug)]
struct Recorder {
    /// Blocks waiting for their contents or their write, oldest first, consecutive.
    queue: VecDeque<Queued>,
    /// Watched accounts whose forward coverage opened without a recorded opening state.
    unopened: Unopened,
    /// Stored history may hold blocks off the followed chain: at start, after a re-anchor that
    /// could not reconcile yet, and after a write was refused as forking.
    reconcile_due: bool,
    /// The backward walk of this session.
    backfill: Backfill,
}

/// A block waiting to be recorded.
#[derive(Debug)]
struct Queued {
    scan: Scan,
    /// The watched accounts' proven state at the block's parent when the block is the first of
    /// its extension, else empty. Kept with the block until it is recorded, since only then
    /// does coverage open there.
    opens: Vec<OpenState>,
}

/// Which watched accounts still need an opening state where their forward coverage opened.
#[derive(Debug, Default)]
enum Unopened {
    /// Every watched account: at start, after a reconcile, and after a block was dropped.
    #[default]
    All,
    /// These accounts, whose state the store lacked where coverage opened or whose opening
    /// write failed. Empty when every account has one.
    Accounts(BTreeSet<Address>),
}

/// The backward walk: settled blocks below the anchor scanned from their header blooms alone,
/// newest first, down to the first header without a BAL hash. It is kept only in memory and
/// starts again at the anchor after a restart or a re-anchor, so gaps left by downtime fill
/// newest first.
#[derive(Debug, Default)]
struct Backfill {
    /// The lowest header the walk has passed; it goes on with that header's parent. `None`
    /// until it starts at the anchor.
    reached: Option<VerifiedHeader>,
    /// Verified headers below `reached` not yet scanned, newest first.
    ahead: VecDeque<VerifiedHeader>,
    /// Scans whose contents did not arrive, with the steps they failed in.
    retry: Vec<(Pending, u32)>,
    /// Scanned blocks whose write failed. The walk writes them before it goes further: it
    /// already passed them, so it would not scan them again.
    unwritten: Vec<ScannedBlock>,
    /// The walk met the fork or the chain's first block.
    done: bool,
}

impl Default for Recorder {
    /// A recorder that reconciles and opens coverage on its first run.
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            unopened: Unopened::All,
            reconcile_due: true,
            backfill: Backfill::default(),
        }
    }
}

impl Recorder {
    /// Takes in each move in order, records forward, then takes one bounded step of the
    /// backward walk. Failures are logged and retried on a later run; they never stop
    /// following.
    async fn run<P: Peers>(&mut self, shared: &Arc<Shared<P>>, moves: Vec<Moved>) {
        for moved in moves {
            match moved {
                Moved::Extended(extension) => self.extended(extension),
                Moved::Reanchored => self.reanchored(),
            }
        }
        self.forward(shared).await;
        self.backfill.step(shared).await;
    }

    /// Queues an extension's blocks, the opening states with the first. Past
    /// [`HISTORY_QUEUE`] the oldest are dropped, leaving a gap where coverage opens anew.
    fn extended(&mut self, extension: Extension) {
        let mut opens = extension.opens;
        for scan in extension.scans {
            let opens = std::mem::take(&mut opens);
            self.queue.push_back(Queued { scan, opens });
        }
        let excess = self.queue.len().saturating_sub(HISTORY_QUEUE);
        if excess > 0 {
            self.queue.drain(..excess);
            self.unopened = Unopened::All;
            warn!(target: "chibi::history", dropped = excess, "block contents unavailable too long; leaving a history gap");
        }
    }

    /// Drops the queued blocks, which may be off the new chain, makes a reconcile due before
    /// the next write, and starts the backward walk again at the new anchor.
    fn reanchored(&mut self) {
        self.queue.clear();
        self.reconcile_due = true;
        self.backfill.restart();
    }

    /// Records the queued blocks that are complete, after a reconcile when one is due, writes
    /// the opening states they carry, then settles. While a reconcile fails nothing is written
    /// and the queue keeps every extended block, up to [`HISTORY_QUEUE`].
    async fn forward<P: Peers>(&mut self, shared: &Arc<Shared<P>>) {
        if self.reconcile_due {
            if let Err(err) = reconcile(shared).await {
                warn!(target: "chibi::history", %err, "cannot reconcile history; holding writes until the next head");
                return;
            }
            self.reconcile_due = false;
            self.unopened = Unopened::All;
        }
        let (blocks, opens) = self.complete(&shared.peers).await;
        if let Some(opens) = self.record(shared, blocks, opens).await {
            self.open(shared, opens).await;
        }
        if let Some(finalized) = shared.read(|s| s.finalized().map(VerifiedHeader::num_hash)) {
            let writer = Arc::clone(shared);
            if let Err(err) = blocking(move || writer.disk.settle_history(finalized)).await {
                warn!(target: "chibi::history", %err, "cannot settle history");
            }
        }
    }

    /// Completes queued scans oldest first, fetching contents where a scan needs them, and
    /// returns the consecutive blocks ready to record with the opening states each carries.
    /// Stops at the first fetch failure, keeping it and the rest queued; a block whose contents
    /// cannot complete its scan is dropped.
    async fn complete<P: Peers>(
        &mut self,
        peers: &Paced<P>,
    ) -> (Vec<ScannedBlock>, Vec<Vec<OpenState>>) {
        let mut blocks = Vec::with_capacity(self.queue.len());
        let mut opens = Vec::with_capacity(self.queue.len());
        while let Some(Queued {
            scan,
            opens: carried,
        }) = self.queue.pop_front()
        {
            let pending = match scan {
                Scan::Done(block) => {
                    blocks.push(block);
                    opens.push(carried);
                    continue;
                }
                Scan::NeedsContents(pending) => pending,
            };
            let block = pending.header().number();
            match contents(peers, pending.header()).await {
                Ok((body, receipts)) => match pending.complete(&body, &receipts) {
                    Ok(scanned) => {
                        blocks.push(scanned);
                        opens.push(carried);
                    }
                    Err(err) => {
                        warn!(target: "chibi::history", block, %err, "block contents do not complete its scan; leaving a history gap");
                        self.unopened = Unopened::All;
                        break;
                    }
                },
                Err(err) => {
                    warn!(target: "chibi::history", block, %err, "block contents unavailable; retrying next head");
                    let scan = Scan::NeedsContents(pending);
                    self.queue.push_front(Queued {
                        scan,
                        opens: carried,
                    });
                    break;
                }
            }
        }
        (blocks, opens)
    }

    /// Writes `blocks` after the first one's parent, returning the opening states they carry
    /// once written. A refusal puts the blocks back to be written on a later run, after a
    /// reconcile when they fork from the stored history.
    async fn record<P: Peers>(
        &mut self,
        shared: &Arc<Shared<P>>,
        blocks: Vec<ScannedBlock>,
        opens: Vec<Vec<OpenState>>,
    ) -> Option<Vec<Vec<OpenState>>> {
        let first = blocks.first()?;
        let parent = BlockNumHash::new(first.number.saturating_sub(1), first.parent_hash);
        let writer = Arc::clone(shared);
        let written = blocking(move || {
            Ok(writer
                .disk
                .record_head(parent, &blocks)
                .map_err(|err| (err, blocks)))
        })
        .await;
        match written {
            Ok(Ok(())) => return Some(opens),
            Ok(Err((err, blocks))) => {
                if matches!(err, NodeError::HistoryReconcile { .. }) {
                    warn!(target: "chibi::history", %err, "history forks from the followed chain; reconciling next head");
                    self.reconcile_due = true;
                } else {
                    warn!(target: "chibi::history", %err, "cannot record history; retrying next head");
                }
                for (block, opens) in blocks.into_iter().zip(opens).rev() {
                    let scan = Scan::Done(block);
                    self.queue.push_front(Queued { scan, opens });
                }
            }
            Err(err) => {
                warn!(target: "chibi::history", %err, "cannot record history; leaving a history gap");
                self.unopened = Unopened::All;
            }
        }
        None
    }

    /// Records, from `opens` in block order, the first opening state of each account still
    /// owed one. An account the store lacked stays owed until a later block carries its state.
    async fn open<P: Peers>(&mut self, shared: &Arc<Shared<P>>, opens: Vec<Vec<OpenState>>) {
        let mut owed = match std::mem::take(&mut self.unopened) {
            Unopened::All => shared.watch.addresses.iter().copied().collect(),
            Unopened::Accounts(owed) => owed,
        };
        let due: Vec<OpenState> = opens
            .into_iter()
            .flatten()
            .filter(|open| owed.remove(&open.account))
            .collect();
        if !due.is_empty() {
            let accounts: Vec<Address> = due.iter().map(|open| open.account).collect();
            let writer = Arc::clone(shared);
            let opened = blocking(move || {
                due.iter()
                    .try_for_each(|open| writer.disk.open_history(open))
            })
            .await;
            if let Err(err) = opened {
                warn!(target: "chibi::history", %err, "cannot record where history opens; retrying");
                owed.extend(accounts);
            }
        }
        self.unopened = Unopened::Accounts(owed);
    }
}

impl Backfill {
    /// Scans settled blocks below the anchor, newest first, within one step's budget, and
    /// writes what completed in one write. Covered blocks above the settled mark are left to
    /// forward recording; the walk waits at an uncovered one until history settles there.
    async fn step<P: Peers>(&mut self, shared: &Arc<Shared<P>>) {
        let watched = &shared.watch.addresses;
        if watched.is_empty() || self.done && self.retry.is_empty() && self.unwritten.is_empty() {
            return;
        }
        let unwritten = std::mem::take(&mut self.unwritten);
        if !self.write(shared, unwritten).await {
            return;
        }
        let reader = Arc::clone(shared);
        let known = blocking(move || {
            let settled = reader.disk.history_settled()?;
            let covered = reader
                .watch
                .addresses
                .iter()
                .map(|&account| reader.disk.history_ranges(account, Capability::Logs))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((settled, covered))
        })
        .await;
        let (settled, covered) = match known {
            Ok((Some(settled), covered)) => (settled, covered),
            Ok((None, _)) => return,
            Err(err) => {
                warn!(target: "chibi::history", %err, "cannot read history coverage; walking back next head");
                return;
            }
        };
        let mut ready = Vec::new();
        let mut contents_left = BACKFILL_CONTENTS;
        for (pending, failed) in std::mem::take(&mut self.retry) {
            if contents_left == 0 {
                self.retry.push((pending, failed));
                continue;
            }
            contents_left -= 1;
            self.complete(&shared.peers, pending, failed, &mut ready)
                .await;
        }
        let mut fetched = 0;
        while !self.done {
            let header = match self.ahead.pop_front() {
                Some(header) => header,
                None => match &self.reached {
                    None => shared.read(|s| s.anchor().clone()),
                    Some(reached) if reached.header().parent_hash == B256::ZERO => {
                        self.done = true;
                        break;
                    }
                    Some(_) if fetched >= BACKFILL_HEADERS => break,
                    Some(reached) => {
                        let count = BACKFILL_HEADERS - fetched;
                        match fetch::ancestors(&shared.peers, reached, count).await {
                            Ok(ancestors) if !ancestors.is_empty() => {
                                fetched += ancestors.len() as u64;
                                self.ahead.extend(ancestors.into_iter().rev());
                                continue;
                            }
                            Ok(_) => break,
                            Err(err) => {
                                warn!(target: "chibi::history", block = reached.number(), %err, "ancestor headers unavailable; walking back next head");
                                break;
                            }
                        }
                    }
                },
            };
            if header.header().block_access_list_hash.is_none() {
                debug!(target: "chibi::history", block = header.number(), "history walked back to the fork");
                self.done = true;
                self.ahead.clear();
                break;
            }
            let number = header.number();
            if !covered.iter().all(|ranges| ranges.contains(number)) {
                if number > settled {
                    self.ahead.push_front(header);
                    break;
                }
                match history::scan_backward(watched, &header) {
                    Scan::Done(block) => ready.push(block),
                    Scan::NeedsContents(_) if contents_left == 0 => {
                        self.ahead.push_front(header);
                        break;
                    }
                    Scan::NeedsContents(pending) => {
                        contents_left -= 1;
                        self.complete(&shared.peers, pending, 0, &mut ready).await;
                    }
                }
            }
            self.reached = Some(header);
        }
        self.write(shared, ready).await;
    }

    /// Writes `ready` in one write and returns whether it was written. A failed write is kept
    /// to write again before the walk goes further; the write is idempotent.
    async fn write<P: Peers>(&mut self, shared: &Arc<Shared<P>>, ready: Vec<ScannedBlock>) -> bool {
        if ready.is_empty() {
            return true;
        }
        let blocks = ready.len();
        let writer = Arc::clone(shared);
        let written = blocking(move || {
            Ok(writer
                .disk
                .augment_history(&ready)
                .map_err(|err| (err, ready)))
        })
        .await;
        match written {
            Ok(Ok(())) => {
                let reached = self.reached.as_ref().map(VerifiedHeader::number);
                debug!(target: "chibi::history", blocks, reached, "older history added");
                return true;
            }
            Ok(Err((err, ready))) => {
                warn!(target: "chibi::history", %err, blocks, "cannot add older history; retrying next head");
                self.unwritten = ready;
            }
            Err(err) => {
                warn!(target: "chibi::history", %err, blocks, "cannot add older history; walking back again from the anchor");
                self.restart();
            }
        }
        false
    }

    /// Starts the walk again at the anchor, keeping blocks whose write is still owed.
    fn restart(&mut self) {
        *self = Self {
            unwritten: std::mem::take(&mut self.unwritten),
            ..Self::default()
        };
    }

    /// Completes `pending` with its block's contents into `ready`. A block whose contents do
    /// not arrive is retried next step until it has failed in [`BACKFILL_ATTEMPTS`] steps;
    /// then, or when its contents cannot complete the scan, it is left uncovered.
    async fn complete<P: Peers>(
        &mut self,
        peers: &Paced<P>,
        pending: Pending,
        failed: u32,
        ready: &mut Vec<ScannedBlock>,
    ) {
        let block = pending.header().number();
        match contents(peers, pending.header()).await {
            Ok((body, receipts)) => match pending.complete(&body, &receipts) {
                Ok(scanned) => ready.push(scanned),
                Err(err) => {
                    warn!(target: "chibi::history", block, %err, "block contents do not complete its scan; leaving a history gap");
                }
            },
            Err(err) if failed + 1 < BACKFILL_ATTEMPTS => {
                warn!(target: "chibi::history", block, %err, "block contents unavailable; retrying next head");
                self.retry.push((pending, failed + 1));
            }
            Err(err) => {
                warn!(target: "chibi::history", block, %err, "block contents unavailable; leaving a history gap");
            }
        }
    }
}

/// A block's verified body and receipts.
async fn contents<P: Peers>(
    peers: &Paced<P>,
    header: &VerifiedHeader,
) -> Result<(VerifiedBody, VerifiedReceipts), NodeError> {
    let body = fetch::bodies(peers, std::slice::from_ref(header))
        .await?
        .pop()
        .ok_or(NodeError::Net(NetError::Rejected))?;
    let receipts = fetch::receipts(peers, header).await?;
    Ok((body, receipts))
}

/// Retracts stored history that left the chain the anchor sits on, comparing every unsettled
/// block against the anchor's verified ancestry, up to [`RECONCILE_CAP`] headers deep.
async fn reconcile<P: Peers>(shared: &Arc<Shared<P>>) -> Result<(), NodeError> {
    let reader = Arc::clone(shared);
    let Some(lowest) = blocking(move || reader.disk.lowest_unsettled()).await? else {
        return Ok(());
    };
    let anchor = shared.read(|s| s.anchor().clone());
    let mut ancestry = BTreeMap::from([(anchor.number(), anchor.hash())]);
    let mut cursor = anchor;
    let mut fetched = 0;
    while cursor.number() > lowest && fetched < RECONCILE_CAP {
        let count = (cursor.number() - lowest).min(RECONCILE_CAP - fetched);
        let ancestors = fetch::ancestors(&shared.peers, &cursor, count).await?;
        let Some(oldest) = ancestors.first().cloned() else {
            break;
        };
        fetched += ancestors.len() as u64;
        ancestry.extend(
            ancestors
                .iter()
                .map(|header| (header.number(), header.hash())),
        );
        cursor = oldest;
    }
    let writer = Arc::clone(shared);
    let tip = blocking(move || writer.disk.reconcile_history(&ancestry)).await?;
    debug!(target: "chibi::history", tip = tip.map(|tip| tip.number), "history reconciled");
    Ok(())
}

/// The watched accounts' proven state at `store`'s anchor. An account the store lacks is left
/// out; one proven absent opens empty.
fn open_states(store: &Store, watched: &[Address]) -> Vec<OpenState> {
    let anchor = store.anchor();
    watched
        .iter()
        .filter_map(|&account| {
            let (balance, nonce) = store
                .account(account)
                .ok()?
                .map_or((U256::ZERO, 0), |held| (held.balance, held.nonce));
            Some(OpenState {
                account,
                number: anchor.number(),
                hash: anchor.hash(),
                balance,
                nonce,
                slots: Vec::new(),
            })
        })
        .collect()
}

impl<P: Peers> Follower<P> {
    pub(crate) async fn follow(&mut self) -> Result<Stepped, NodeError> {
        let trusted = (*self.heads.latest.borrow_and_update()).ok_or(NodeError::NotReady)?;
        let shared = Arc::clone(&self.shared);
        let anchor = shared.read(|s| s.anchor().clone());
        let mut extension = None;
        let stepped = if trusted.hash() == anchor.hash() {
            Stepped::Current
        } else {
            let target = fetch::header(&shared.peers, trusted).await?;
            let head = target.num_hash();
            match shared.read(|s| s.plan(&target)) {
                Plan::Current => Stepped::Current,
                Plan::Extend { blocks } => match shared.extend(&anchor, target.clone()).await {
                    Ok(extended) => {
                        extension = extended;
                        Stepped::Extended(head)
                    }
                    Err(err) => {
                        info!(
                            target: "chibi::follow",
                            head = head.number, blocks, %err,
                            "cannot extend with BALs; reanchoring",
                        );
                        shared.reanchor(target).await?;
                        Stepped::Reanchored(head)
                    }
                },
                Plan::Reanchor => {
                    shared.reanchor(target).await?;
                    Stepped::Reanchored(head)
                }
            }
        };
        let moved = match &stepped {
            Stepped::Current => false,
            Stepped::Extended(head) | Stepped::Reanchored(head) => {
                self.moves += 1;
                info!(target: "chibi::follow", head = head.number, hash = %head.hash, ?stepped, "head verified");
                true
            }
        };
        if matches!(stepped, Stepped::Reanchored(_)) {
            self.seed_due = true;
        }
        if self.seed_due {
            self.seed().await;
        }
        shared.warm(self.heads.latest.clone()).await;

        shared.follow_finalized(&self.heads).await;
        if let Some(history) = &mut self.history {
            let moved = match stepped {
                Stepped::Reanchored(_) => Some(Moved::Reanchored),
                Stepped::Extended(_) | Stepped::Current => extension.map(Moved::Extended),
            };
            history
                .step(&shared, moved, self.heads.latest.clone())
                .await;
        }
        let anchor = shared.read(|s| s.anchor().clone());
        if shared.read(|s| s.txs().has_pending()) {
            if self.search_pending(&anchor).await {
                self.search.settle(anchor.num_hash());
                shared.prune(&self.search).await;
            }
            if moved && self.moves.is_multiple_of(REBROADCAST_EVERY) {
                let pending: Vec<_> =
                    shared.read(|s| s.txs().pending().iter().map(|p| p.tx.clone()).collect());
                for transaction in pending {
                    shared.broadcast(transaction).await;
                }
            }
        } else {
            self.search.settle(anchor.num_hash());
            self.search.covered_from = Some(anchor.number());
        }
        self.collect_receipts().await;

        shared.persist().await?;
        shared.save_keys(Some(self.moves)).await?;
        shared.peers.announce(&shared.read(State::snapshot));
        Ok(stepped)
    }

    /// Seeds, recording when, and leaves seeding due if it could not complete.
    pub(crate) async fn seed(&mut self) {
        self.seeded_at = tokio::time::Instant::now();
        self.seed_due = !self.shared.seed().await;
    }

    /// Searches every unsearched block between `anchor` and the oldest submission not yet
    /// covered, stopping at the floor. Returns whether the walk and every body fetch finished;
    /// partial progress is kept for the next step.
    async fn search_pending(&mut self, anchor: &VerifiedHeader) -> bool {
        let shared = Arc::clone(&self.shared);
        let target = self.target(anchor);
        let mut unsearched = Vec::new();
        let mut cursor = anchor.clone();
        let reach = loop {
            if self.search.floor.is_some_and(|f| f.hash == cursor.hash()) {
                break Reach::Floor;
            }
            if !self.search.searched.contains(&cursor.hash()) {
                unsearched.push(cursor.clone());
            }
            if cursor.number() <= target {
                break Reach::Target(target);
            }
            match self.parent(&cursor, target).await {
                Some(parent) => cursor = parent,
                None => break Reach::Short,
            }
        };
        unsearched.reverse();
        for batch in unsearched.chunks(BODY_BATCH) {
            if !shared.read(|s| s.txs().has_pending()) {
                return true;
            }
            let bodies = match fetch::bodies(&shared.peers, batch).await {
                Ok(bodies) => bodies,
                Err(err) => {
                    warn!(target: "chibi::follow", from = batch[0].number(), blocks = batch.len(), %err, "bodies unavailable; pending transactions searched next head");
                    return false;
                }
            };
            shared.cache_samples(&bodies);
            for body in bodies {
                self.search.searched.insert(body.header().hash());
                let found = shared.update(|s| s.txs_mut().locate(&body));
                for (hash, index) in &found {
                    info!(target: "chibi::follow", %hash, block = body.header().number(), index, "transaction included");
                }
                if !found.is_empty() {
                    self.unreceipted.push(body);
                }
            }
        }
        match reach {
            Reach::Floor => true,
            Reach::Target(target) => {
                self.search.covered_from = Some(target);
                true
            }
            Reach::Short => false,
        }
    }

    /// The lowest block the search must reach: the oldest submission of a pending transaction
    /// within [`DEEP_SEARCH`], else the header window. The walk usually stops earlier, at the
    /// floor; it goes this deep only when the floor is unknown or left the chain. Uncovered
    /// transactions out of reach are reported once and stay pending.
    fn target(&mut self, anchor: &VerifiedHeader) -> u64 {
        let pending: Vec<(TxHash, BlockNumHash)> = self.shared.read(|s| {
            s.txs()
                .pending()
                .iter()
                .map(|p| (p.hash, p.since))
                .collect()
        });
        let mut target = None;
        for (hash, since) in pending {
            if anchor.number().saturating_sub(since.number) <= DEEP_SEARCH {
                target = Some(target.map_or(since.number, |t: u64| t.min(since.number)));
            } else if !self.search.covers(since) && self.search.unreachable.insert(hash) {
                warn!(
                    target: "chibi::follow",
                    %hash, since = since.number, head = anchor.number(),
                    "submitted too far back to search; it stays pending until found in a new block",
                );
            }
        }
        target.unwrap_or_else(|| anchor.number().saturating_sub(HEADER_WINDOW as u64))
    }

    /// The verified parent of `child`, from the store, from headers fetched earlier, or from
    /// peers, fetching a batch of ancestors toward the floor or `target`.
    async fn parent(&mut self, child: &VerifiedHeader, target: u64) -> Option<VerifiedHeader> {
        let parent_hash = child.header().parent_hash;
        let held = self.shared.read(|s| {
            s.store()
                .header(child.number() - 1)
                .filter(|held| held.hash() == parent_hash)
                .cloned()
        });
        if let Some(parent) = held.or_else(|| self.search.known.get(&parent_hash).cloned()) {
            return Some(parent);
        }
        let deepest = self
            .search
            .floor
            .filter(|floor| floor.number < child.number())
            .map_or(target, |floor| floor.number.max(target));
        let count = (child.number() - deepest).clamp(1, ANCESTOR_BATCH);
        match fetch::ancestors(&self.shared.peers, child, count).await {
            Ok(ancestors) => {
                let parent = ancestors.last().cloned();
                self.search
                    .known
                    .extend(ancestors.into_iter().map(|header| (header.hash(), header)));
                parent
            }
            Err(err) => {
                warn!(target: "chibi::follow", block = child.number(), %err, "ancestor headers unavailable; pending transactions searched next head");
                None
            }
        }
    }

    /// Fetches and verifies receipts for located transactions. A block is kept for the next
    /// step until its receipts arrive, and dropped once the store holds a different block at
    /// its height or none of its located transactions still lacks a receipt.
    async fn collect_receipts(&mut self) {
        let mut waiting = Vec::with_capacity(self.unreceipted.len());
        for body in std::mem::take(&mut self.unreceipted) {
            let header = body.header();
            let wanted = self.shared.read(|s| {
                let orphaned = s
                    .store()
                    .header(header.number())
                    .is_some_and(|held| held.hash() != header.hash());
                let needed = body.transactions().iter().any(|tx| {
                    matches!(
                        s.txs().lookup(*tx.tx_hash()),
                        Lookup::Located(located)
                            if located.block == header.num_hash() && located.receipt.is_none()
                    )
                });
                !orphaned && needed
            });
            if !wanted {
                continue;
            }
            match fetch::receipts(&self.shared.peers, header).await {
                Ok(receipts) => self.shared.update(|s| {
                    for index in 0..body.transactions().len() {
                        if let Some(receipt) = receipts.receipt(&body, index) {
                            s.txs_mut().store_receipt(receipt);
                        }
                    }
                }),
                Err(err) => {
                    warn!(target: "chibi::follow", block = header.number(), %err, "receipts unavailable; retrying next head");
                    waiting.push(body);
                }
            }
        }
        self.unreceipted = waiting;
    }
}

impl<P: Peers> Shared<P> {
    /// Applies the BALs from the anchor's child through `target`. With history on, returns
    /// each block's scan, taken before the store consumes its BAL, and the watched accounts'
    /// proven state at the anchor.
    async fn extend(
        &self,
        anchor: &VerifiedHeader,
        target: VerifiedHeader,
    ) -> Result<Option<Extension>, NodeError> {
        let chain = fetch::chain(&self.peers, anchor, target).await?;
        let blocks = fetch::bals(&self.peers, chain).await?;
        let watched = &self.watch.addresses;
        let scans = self.watch.history.then(|| {
            blocks
                .iter()
                .map(|block| history::scan_forward(watched, block))
                .collect::<Vec<_>>()
        });
        let opens = self.update(|s| {
            let opens = scans.is_some().then(|| open_states(s.store(), watched));
            blocks
                .into_iter()
                .try_for_each(|block| s.advance(block))
                .map(|()| opens)
        })?;
        Ok(scans
            .zip(opens)
            .map(|(scans, opens)| Extension { scans, opens }))
    }

    /// Proves the working-set keys the store lacks at the anchor, pins first, then learned
    /// keys most recently used first, merging every [`WARM_MERGE`] proofs. Each key is tried
    /// once; a failure leaves it unheld for the next pass. Stops as soon as `latest` changes,
    /// merging what completed. Only the follower moves the anchor, so no merge is dropped.
    pub(crate) async fn warm(&self, mut latest: watch::Receiver<Option<TrustedHash>>) {
        let (anchor, keys) = self.read(|s| (s.anchor().clone(), warm_order(s.unheld())));
        if keys.is_empty() {
            return;
        }
        let wanted = keys.len();
        let mut proofs = std::pin::pin!(fetch::each(&self.peers, &anchor, keys, WARM_IN_FLIGHT));
        let mut batch = Vec::with_capacity(WARM_MERGE);
        let mut failed = 0;
        let interrupted = loop {
            tokio::select! {
                biased;
                _ = latest.changed() => break true,
                next = proofs.next() => match next {
                    None => break false,
                    Some(Ok(proven)) => {
                        batch.push(proven);
                        if batch.len() == WARM_MERGE {
                            self.update(|s| s.merge(anchor.hash(), &batch));
                            batch.clear();
                        }
                    }
                    Some(Err(unproven)) => {
                        failed += unproven.keys.len();
                        debug!(target: "chibi::follow", keys = ?unproven.keys, err = %unproven.error, "warming keys failed; retrying next pass");
                    }
                },
            }
        };
        self.update(|s| s.merge(anchor.hash(), &batch));
        debug!(target: "chibi::follow", head = anchor.number(), wanted, failed, interrupted, "warmed");
    }

    /// Records the light client's finalized header once it is fetched and verified.
    async fn follow_finalized(&self, heads: &Heads) {
        let Some(trusted) = *heads.finalized.borrow() else {
            return;
        };
        if self.read(|s| s.finalized().map(VerifiedHeader::hash)) == Some(trusted.hash()) {
            return;
        }
        match fetch::header(&self.peers, trusted).await {
            Ok(header) => self.update(|s| s.set_finalized(header)),
            Err(err) => warn!(target: "chibi::follow", %err, "finalized header unavailable"),
        }
    }

    /// Drops pending transactions whose nonce the anchor already consumed, proving senders'
    /// accounts the store lacks. A sender is skipped while any of its stale transactions was
    /// submitted below what `search` covers: it may sit in a block nobody searched.
    async fn prune(&self, search: &Search) {
        let senders: BTreeSet<_> =
            self.read(|s| s.txs().pending().iter().map(|p| p.sender).collect());
        let anchor = self.read(|s| s.anchor().clone());
        for sender in senders {
            let held = self.read(|s| {
                s.store()
                    .account(sender)
                    .map(|account| account.map_or(0, |a| a.nonce))
            });
            let nonce = match held {
                Ok(nonce) => nonce,
                Err(_) => match fetch::account(&self.peers, &anchor, sender).await {
                    Ok(proven) => {
                        self.update(|s| s.merge(anchor.hash(), std::slice::from_ref(&proven)));
                        proven.account().flatten().map_or(0, |a| a.nonce)
                    }
                    Err(err) => {
                        debug!(target: "chibi::follow", %sender, %err, "sender nonce unavailable");
                        continue;
                    }
                },
            };
            let uncovered = self.read(|s| {
                s.txs()
                    .pending()
                    .iter()
                    .any(|p| p.sender == sender && p.nonce < nonce && !search.covers(p.since))
            });
            if uncovered {
                debug!(target: "chibi::follow", %sender, "not pruning: a stale transaction's blocks were not all searched");
                continue;
            }
            for hash in self.update(|s| s.txs_mut().prune(sender, nonce)) {
                info!(target: "chibi::follow", %hash, %sender, "dropping a transaction whose nonce was used by another");
            }
        }
    }
}

/// The keys to prove for `unheld`, in order. An account is left out when one of its slots is
/// listed, because a slot proof carries its account.
fn warm_order(unheld: impl Iterator<Item = Key>) -> Vec<Key> {
    let keys: Vec<Key> = unheld.collect();
    let with_slots: HashSet<_> = keys
        .iter()
        .filter_map(|key| match key {
            Key::Slot(address, _) => Some(*address),
            Key::Account(_) => None,
        })
        .collect();
    keys.into_iter()
        .filter(|key| !matches!(key, Key::Account(address) if with_slots.contains(address)))
        .collect()
}
