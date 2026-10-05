//! One follower step: reach the light client's newest head, follow finality, locate submitted
//! transactions, and persist.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use chibi_core::VerifiedHeader;
use chibi_core::state::{HEADER_WINDOW, Plan};
use chibi_core::tx::Lookup;
use tracing::{debug, info, warn};

use crate::fetch;
use crate::heads::Heads;
use crate::node::{Follower, NodeError, Shared, Stepped};
use crate::peers::Peers;

/// Heads between re-broadcasts of transactions not yet found in a block.
const REBROADCAST_EVERY: u64 = 5;

/// Which blocks have been searched for pending transactions.
///
/// A pending transaction is dropped as replaced only once every block it could sit in was
/// searched, so a failed body fetch delays the search instead of losing the transaction.
#[derive(Debug, Default)]
pub(crate) struct Search {
    /// Every block at or below this one on its chain is covered: searched, or older than
    /// every pending transaction. `None` after a restart that restored transactions, which
    /// may sit anywhere in the header window.
    floor: Option<BlockNumHash>,
    /// Blocks above the floor whose bodies were searched.
    searched: HashSet<B256>,
    /// Verified headers above the floor that the store may not hold, by hash.
    known: HashMap<B256, VerifiedHeader>,
}

impl Search {
    pub(crate) fn new(floor: Option<BlockNumHash>) -> Self {
        Self {
            floor,
            ..Self::default()
        }
    }

    /// Marks everything up to `anchor` covered.
    fn settle(&mut self, anchor: BlockNumHash) {
        self.floor = Some(anchor);
        self.searched.clear();
        self.known.clear();
    }
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

        shared.follow_finalized(&self.heads).await;
        let anchor = shared.read(|s| s.anchor().clone());
        if shared.read(|s| s.txs().has_pending()) {
            if self.search_pending(&anchor).await {
                self.search.settle(anchor.num_hash());
                shared.prune().await;
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
        }
        self.collect_receipts().await;

        shared.persist().await?;
        shared.peers.announce(&anchor);
        Ok(stepped)
    }

    /// Searches every unsearched block between the floor and `anchor` for pending
    /// transactions. Returns whether all of them are now covered.
    async fn search_pending(&mut self, anchor: &VerifiedHeader) -> bool {
        let shared = Arc::clone(&self.shared);
        let lowest = anchor.number().saturating_sub(HEADER_WINDOW as u64);
        let mut unsearched = Vec::new();
        let mut cursor = anchor.clone();
        let complete = loop {
            let is_floor = self.search.floor.is_some_and(|f| f.hash == cursor.hash());
            if is_floor || cursor.number() <= lowest {
                break true;
            }
            if !self.search.searched.contains(&cursor.hash()) {
                unsearched.push(cursor.clone());
            }
            if cursor.number() - 1 <= lowest {
                break true;
            }
            match self.parent(&cursor, lowest).await {
                Some(parent) => cursor = parent,
                None => break false,
            }
        };
        if unsearched.is_empty() {
            return complete;
        }
        unsearched.reverse();
        let bodies = match fetch::bodies(&shared.peers, &unsearched).await {
            Ok(bodies) => bodies,
            Err(err) => {
                warn!(target: "chibi::follow", blocks = unsearched.len(), %err, "bodies unavailable; pending transactions searched next head");
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
        complete
    }

    /// The verified parent of `child`, from the store, from headers fetched earlier, or from
    /// peers, fetching every ancestor down to the floor or the window bound in one request.
    async fn parent(&mut self, child: &VerifiedHeader, lowest: u64) -> Option<VerifiedHeader> {
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
            .map_or(lowest + 1, |floor| floor.number.max(lowest + 1));
        let count = child.number() - deepest;
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
    /// accounts the store lacks. Only sound once every block they could sit in was searched.
    async fn prune(&self) {
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
            for hash in self.update(|s| s.txs_mut().prune(sender, nonce)) {
                info!(target: "chibi::follow", %hash, %sender, "dropping a transaction whose nonce was used by another");
            }
        }
    }
}
