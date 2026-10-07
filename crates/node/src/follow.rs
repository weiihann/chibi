//! One follower step: reach the light client's newest head, warm the working set, follow
//! finality, locate submitted transactions, and persist.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use alloy_eips::BlockNumHash;
use alloy_primitives::{B256, TxHash};
use chibi_core::state::{HEADER_WINDOW, Key, Plan};
use chibi_core::tx::Lookup;
use chibi_core::{TrustedHash, VerifiedHeader};
use futures::StreamExt as _;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::fetch;
use crate::heads::Heads;
use crate::node::{Follower, NodeError, Shared, Stepped};
use crate::peers::Peers;

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

/// Warm proofs in flight at once. reth keeps one snap request in flight per peer, so a wallet
/// request or a BAL fetch queues behind at most this many warm proofs.
const WARM_IN_FLIGHT: usize = 2;

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

impl<P: Peers> Follower<P> {
    pub(crate) async fn follow(&mut self) -> Result<Stepped, NodeError> {
        let trusted = (*self.heads.latest.borrow_and_update()).ok_or(NodeError::NotReady)?;
        let shared = Arc::clone(&self.shared);
        let anchor = shared.read(|s| s.anchor().clone());
        let stepped = if trusted.hash() == anchor.hash() {
            Stepped::Current
        } else {
            let target = fetch::header(&shared.peers, trusted).await?;
            let head = target.num_hash();
            match shared.read(|s| s.plan(&target)) {
                Plan::Current => Stepped::Current,
                Plan::Extend { blocks } => match shared.extend(&anchor, target.clone()).await {
                    Ok(()) => Stepped::Extended(head),
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
            self.seed_due = !shared.seed().await;
        }
        shared.warm(self.heads.latest.clone()).await;

        shared.follow_finalized(&self.heads).await;
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
        shared.peers.announce(&anchor);
        Ok(stepped)
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
    /// Applies the BALs from the anchor's child through `target`.
    async fn extend(
        &self,
        anchor: &VerifiedHeader,
        target: VerifiedHeader,
    ) -> Result<(), NodeError> {
        let chain = fetch::chain(&self.peers, anchor, target).await?;
        let blocks = fetch::bals(&self.peers, chain).await?;
        self.update(|s| blocks.into_iter().try_for_each(|block| s.advance(block)))?;
        Ok(())
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
                    Some((_, Ok(proven))) => {
                        batch.push(proven);
                        if batch.len() == WARM_MERGE {
                            self.update(|s| s.merge(anchor.hash(), &batch));
                            batch.clear();
                        }
                    }
                    Some((key, Err(err))) => {
                        failed += 1;
                        debug!(target: "chibi::follow", ?key, %err, "warming a key failed; retrying next pass");
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
