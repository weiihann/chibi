//! One follower step: reach the light client's newest head, follow finality, locate submitted
//! transactions, and persist.

use std::collections::BTreeSet;
use std::sync::Arc;

use chibi_core::VerifiedHeader;
use chibi_core::state::{HEADER_WINDOW, Plan};
use chibi_core::verify::VerifiedBody;
use tracing::{debug, info, warn};

use crate::fetch;
use crate::heads::Heads;
use crate::node::{Follower, NodeError, Shared, Stepped};
use crate::peers::Peers;

/// Heads between re-broadcasts of transactions not yet found in a block.
const REBROADCAST_EVERY: u64 = 5;

impl<P: Peers> Follower<P> {
    pub(crate) async fn follow(&mut self) -> Result<Stepped, NodeError> {
        let trusted = (*self.heads.latest.borrow_and_update()).ok_or(NodeError::NotReady)?;
        let shared = Arc::clone(&self.shared);
        let anchor = shared.read(|s| s.anchor().clone());
        let (stepped, passed) = if trusted.hash() == anchor.hash() {
            (Stepped::Current, Vec::new())
        } else {
            let target = fetch::header(&shared.peers, trusted).await?;
            match shared.read(|s| s.plan(&target)) {
                Plan::Current => (Stepped::Current, Vec::new()),
                Plan::Extend { blocks } => match shared.extend(&anchor, target.clone()).await {
                    Ok(passed) => (Stepped::Extended(target.num_hash()), passed),
                    Err(err) => {
                        info!(
                            target: "chibi::follow",
                            head = target.number(), blocks, %err,
                            "cannot extend with BALs; reanchoring",
                        );
                        shared.reanchor_passing(&anchor, target).await?
                    }
                },
                Plan::Reanchor => shared.reanchor_passing(&anchor, target).await?,
            }
        };
        match &stepped {
            Stepped::Current => {}
            Stepped::Extended(head) | Stepped::Reanchored(head) => {
                self.moves += 1;
                info!(target: "chibi::follow", head = head.number, hash = %head.hash, ?stepped, "head verified");
            }
        }

        shared.follow_finalized(&self.heads).await;
        let found = shared.locate(&passed).await;
        self.unreceipted.extend(found);
        self.collect_receipts().await;
        if shared.read(|s| s.txs().has_pending()) {
            shared.prune().await;
            let rebroadcast = !passed.is_empty() && self.moves.is_multiple_of(REBROADCAST_EVERY);
            if rebroadcast {
                let pending: Vec<_> =
                    shared.read(|s| s.txs().pending().iter().map(|p| p.tx.clone()).collect());
                for transaction in pending {
                    shared.broadcast(transaction).await;
                }
            }
        }

        shared.persist()?;
        let head = shared.read(|s| s.anchor().clone());
        shared.peers.announce(&head);
        Ok(stepped)
    }

    /// Fetches and verifies receipts for located transactions, keeping blocks whose receipts
    /// could not be fetched for the next step while they stay in the header window.
    async fn collect_receipts(&mut self) {
        let mut waiting = Vec::with_capacity(self.unreceipted.len());
        for body in std::mem::take(&mut self.unreceipted) {
            let header = body.header();
            let held = self.shared.read(|s| {
                s.store()
                    .header(header.number())
                    .is_some_and(|held| held.hash() == header.hash())
            });
            if !held {
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
    /// Applies the BALs from the anchor's child through `target`. Returns the headers passed.
    async fn extend(
        &self,
        anchor: &VerifiedHeader,
        target: VerifiedHeader,
    ) -> Result<Vec<VerifiedHeader>, NodeError> {
        let chain = fetch::chain(&self.peers, anchor, target).await?;
        let blocks = fetch::bals(&self.peers, chain.clone()).await?;
        self.update(|s| blocks.into_iter().try_for_each(|block| s.advance(block)))?;
        Ok(chain)
    }

    /// Reanchors at `target`. While transactions are pending, also returns the verified
    /// headers between the old anchor and `target`, so their bodies can be searched.
    async fn reanchor_passing(
        &self,
        anchor: &VerifiedHeader,
        target: VerifiedHeader,
    ) -> Result<(Stepped, Vec<VerifiedHeader>), NodeError> {
        let mut passed = Vec::new();
        if self.read(|s| s.txs().has_pending()) {
            let gap = target
                .number()
                .saturating_sub(anchor.number() + 1)
                .min(HEADER_WINDOW as u64);
            match fetch::ancestors(&self.peers, &target, gap).await {
                Ok(ancestors) => passed = ancestors,
                Err(err) => {
                    warn!(target: "chibi::follow", %err, "gap headers unavailable; searching only the new head");
                }
            }
            passed.retain(|header| header.number() > anchor.number());
            passed.push(target.clone());
        }
        let head = target.num_hash();
        self.reanchor(target).await?;
        Ok((Stepped::Reanchored(head), passed))
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

    /// Searches the verified bodies of `passed` for pending transactions. Returns the bodies
    /// that held one.
    async fn locate(&self, passed: &[VerifiedHeader]) -> Vec<VerifiedBody> {
        if passed.is_empty() || !self.read(|s| s.txs().has_pending()) {
            return Vec::new();
        }
        let bodies = match fetch::bodies(&self.peers, passed).await {
            Ok(bodies) => bodies,
            Err(err) => {
                warn!(target: "chibi::follow", %err, "bodies unavailable; pending transactions not searched");
                return Vec::new();
            }
        };
        self.cache_samples(&bodies);
        bodies
            .into_iter()
            .filter(|body| {
                let found = self.update(|s| s.txs_mut().locate(body));
                for (hash, index) in &found {
                    info!(target: "chibi::follow", %hash, block = body.header().number(), index, "transaction included");
                }
                !found.is_empty()
            })
            .collect()
    }

    /// Drops pending transactions whose nonce the anchor already consumed, proving senders'
    /// accounts the store lacks.
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
