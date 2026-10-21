//! One follower step: reach the light client's newest head, warm the working set, follow
//! finality, locate submitted transactions, and persist.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, TxHash, U256};
use chibi_core::history::{self, Capability, OpenState, Pending, Scan, ScannedBlock};
use chibi_core::state::{HEADER_WINDOW, Key, Plan, State, Store};
use chibi_core::tx::Lookup;
use chibi_core::verify::{VerifiedBody, VerifiedReceipts};
use chibi_core::{TrustedHash, VerifiedHeader};
use futures::StreamExt as _;
use tokio::sync::watch;
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
    /// The anchor the blocks extend.
    parent: BlockNumHash,
    /// Each block's scan, oldest first.
    scans: Vec<Scan>,
    /// The watched accounts the anchor's store held, at the anchor.
    opens: Vec<OpenState>,
}

/// Account history: the follower records each extended block once its scan is complete,
/// reconciling after a re-anchor, then walks back to fill older blocks. It exists only while
/// history is on.
#[derive(Debug)]
pub(crate) struct Recorder {
    /// Scans waiting for their block's contents, oldest first, consecutive.
    queue: VecDeque<Scan>,
    /// Coverage must reopen with proven state: at start, after a reconcile, and after a block
    /// was dropped.
    open_due: bool,
    /// Stored history may hold blocks off the followed chain: at start, after a re-anchor that
    /// could not reconcile yet, and after a write was refused as forking.
    reconcile_due: bool,
    /// The backward walk of this session.
    backfill: Backfill,
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
    /// The walk met the fork or the chain's first block.
    done: bool,
}

impl Recorder {
    /// A recorder that reconciles and opens coverage on its first step.
    pub(crate) fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            open_due: true,
            reconcile_due: true,
            backfill: Backfill::default(),
        }
    }

    /// Records forward, then takes one bounded step of the backward walk. Failures are logged
    /// and retried on a later step; they never stop following.
    async fn step<P: Peers>(
        &mut self,
        shared: &Arc<Shared<P>>,
        reanchored: bool,
        extension: Option<Extension>,
    ) {
        if reanchored {
            self.backfill = Backfill::default();
        }
        self.forward(shared, reanchored, extension).await;
        self.backfill.step(shared).await;
    }

    /// Records what this step extended, after a reconcile when one is due, then settles.
    async fn forward<P: Peers>(
        &mut self,
        shared: &Arc<Shared<P>>,
        reanchored: bool,
        extension: Option<Extension>,
    ) {
        if reanchored || self.reconcile_due {
            self.queue.clear();
            self.reconcile_due = true;
            if let Err(err) = reconcile(shared).await {
                warn!(target: "chibi::history", %err, "cannot reconcile history; holding writes until the next head");
                return;
            }
            self.reconcile_due = false;
            self.open_due = true;
        }
        let (anchor, opens) = match extension {
            Some(extension) => {
                self.queue.extend(extension.scans);
                (Some(extension.parent), extension.opens)
            }
            None => (None, Vec::new()),
        };
        let excess = self.queue.len().saturating_sub(HISTORY_QUEUE);
        if excess > 0 {
            self.queue.drain(..excess);
            self.open_due = true;
            warn!(target: "chibi::history", dropped = excess, "block contents unavailable too long; leaving a history gap");
        }
        let ready = self.complete(&shared.peers).await;
        if let Some(first) = ready.first() {
            let parent = BlockNumHash::new(first.number.saturating_sub(1), first.parent_hash);
            let recorded = self.record(shared, parent, ready).await;
            if recorded && self.open_due && anchor == Some(parent) {
                self.open_due = !open(shared, opens).await;
            }
        }
        if let Some(finalized) = shared.read(|s| s.finalized().map(VerifiedHeader::num_hash)) {
            let writer = Arc::clone(shared);
            if let Err(err) = blocking(move || writer.disk.settle_history(finalized)).await {
                warn!(target: "chibi::history", %err, "cannot settle history");
            }
        }
    }

    /// Completes queued scans oldest first, fetching contents where a scan needs them, and
    /// returns the consecutive blocks ready to record. Stops at the first fetch failure,
    /// keeping it and the rest queued; a block whose contents cannot complete its scan is
    /// dropped.
    async fn complete<P: Peers>(&mut self, peers: &Paced<P>) -> Vec<ScannedBlock> {
        let mut ready = Vec::with_capacity(self.queue.len());
        while let Some(scan) = self.queue.pop_front() {
            let pending = match scan {
                Scan::Done(block) => {
                    ready.push(block);
                    continue;
                }
                Scan::NeedsContents(pending) => pending,
            };
            let block = pending.header().number();
            match contents(peers, pending.header()).await {
                Ok((body, receipts)) => match pending.complete(&body, &receipts) {
                    Ok(scanned) => ready.push(scanned),
                    Err(err) => {
                        warn!(target: "chibi::history", block, %err, "block contents do not complete its scan; leaving a history gap");
                        self.open_due = true;
                        break;
                    }
                },
                Err(err) => {
                    warn!(target: "chibi::history", block, %err, "block contents unavailable; retrying next head");
                    self.queue.push_front(Scan::NeedsContents(pending));
                    break;
                }
            }
        }
        ready
    }

    /// Writes `ready` after `parent`, returning whether it was written. A fork from the stored
    /// history makes a reconcile due; any other refusal puts the blocks back to be written next
    /// step.
    async fn record<P: Peers>(
        &mut self,
        shared: &Arc<Shared<P>>,
        parent: BlockNumHash,
        ready: Vec<ScannedBlock>,
    ) -> bool {
        let writer = Arc::clone(shared);
        let written = blocking(move || match writer.disk.record_head(parent, &ready) {
            Ok(()) => Ok(Ok(())),
            Err(err) => Ok(Err((err, ready))),
        })
        .await;
        match written {
            Ok(Ok(())) => return true,
            Ok(Err((err @ NodeError::HistoryReconcile { .. }, _))) => {
                warn!(target: "chibi::history", %err, "history forks from the followed chain; reconciling next head");
                self.reconcile_due = true;
            }
            Ok(Err((err, ready))) => {
                warn!(target: "chibi::history", %err, "cannot record history; retrying next head");
                for block in ready.into_iter().rev() {
                    self.queue.push_front(Scan::Done(block));
                }
            }
            Err(err) => {
                warn!(target: "chibi::history", %err, "cannot record history; leaving a history gap");
                self.open_due = true;
            }
        }
        false
    }
}

impl Backfill {
    /// Scans settled blocks below the anchor, newest first, within one step's budget, and
    /// writes what completed in one write. Blocks above the settled mark are left to forward
    /// recording, so nothing is walked before history settles.
    async fn step<P: Peers>(&mut self, shared: &Arc<Shared<P>>) {
        let watched = &shared.watch.addresses;
        if watched.is_empty() || self.done && self.retry.is_empty() {
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
            if number <= settled && !covered.iter().all(|ranges| ranges.contains(number)) {
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
        if ready.is_empty() {
            return;
        }
        let blocks = ready.len();
        let writer = Arc::clone(shared);
        match blocking(move || writer.disk.augment_history(&ready)).await {
            Ok(()) => {
                let reached = self.reached.as_ref().map(VerifiedHeader::number);
                debug!(target: "chibi::history", blocks, reached, "older history added");
            }
            Err(err) => {
                warn!(target: "chibi::history", %err, blocks, "cannot add older history; leaving a history gap");
            }
        }
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

/// Records each watched account's proven state where its coverage opens. Returns whether
/// every one was written.
async fn open<P: Peers>(shared: &Arc<Shared<P>>, opens: Vec<OpenState>) -> bool {
    let writer = Arc::clone(shared);
    let opened = blocking(move || {
        opens
            .iter()
            .try_for_each(|open| writer.disk.open_history(open))
    })
    .await;
    if let Err(err) = &opened {
        warn!(target: "chibi::history", %err, "cannot record where history opens; retrying");
    }
    opened.is_ok()
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
        if let Some(recorder) = &mut self.history {
            let reanchored = matches!(stepped, Stepped::Reanchored(_));
            recorder.step(&shared, reanchored, extension).await;
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
        Ok(scans.zip(opens).map(|(scans, opens)| Extension {
            parent: anchor.num_hash(),
            scans,
            opens,
        }))
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
