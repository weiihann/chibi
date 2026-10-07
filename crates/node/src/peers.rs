//! The network seam. [`Peers`] carries untrusted replies in [`chibi_core::wire`] shapes and
//! verifies nothing. The devp2p adapter implements it over reth-network; the test fixture
//! implements it over in-process tries, so offline tests run the production verifier.
//! [`Paced`] wraps either one with the timeouts every request gets.

use std::time::{Duration, Instant};

use alloy_consensus::Header;
use alloy_primitives::{B256, B512, Bytes};
use chibi_core::VerifiedHeader;
use chibi_core::wire::{
    AccountRangeRequest, AccountReply, Body, Receipt, StorageRangeRequest, StorageReply,
    Transaction,
};
use parking_lot::Mutex;
use reth_network_peers::NodeRecord;
use tokio::sync::{Semaphore, SemaphorePermit};
use tracing::debug;

/// How long a peer may take to answer one request, counted from when it is sent.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// snap/2 requests one peer works on at once. reth's fetcher sends a peer one request at a
/// time and queues the rest.
const SNAP_PER_PEER: usize = 1;

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

/// The connected sessions at one moment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Census {
    /// Connected sessions.
    pub peers: usize,
    /// Connected sessions that negotiated snap/2.
    pub snap_peers: usize,
    /// Addresses of the connected snap/2 peers, dialed first on the next start because
    /// discovery takes minutes to find one.
    pub snap_records: Vec<NodeRecord>,
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

    /// The sessions connected now.
    fn census(&self) -> Census;
}

/// [`Peers`] with a timeout on every request, and snap/2 requests paced to what the connected
/// snap/2 peers can work on at once. reth queues a request it cannot send yet, so a timeout
/// started at submission would count time spent behind chibi's own requests: one busy peer
/// would time out healthy requests, and their retries would queue behind the abandoned ones,
/// which reth still sends.
#[derive(Debug)]
pub(crate) struct Paced<P> {
    inner: P,
    /// One permit per request the snap/2 peers can work on at once. Fair, so a retry cannot
    /// jump ahead of a request that was already waiting, such as the follower's next BALs.
    slots: Semaphore,
    /// Permits the semaphore stands for, followed to the snap/2 peer count.
    capacity: Mutex<usize>,
}

impl<P: Peers> Paced<P> {
    pub(crate) fn new(inner: P) -> Self {
        Self {
            inner,
            slots: Semaphore::new(SNAP_PER_PEER),
            capacity: Mutex::new(SNAP_PER_PEER),
        }
    }

    fn wanted(&self) -> usize {
        self.inner.census().snap_peers.max(1) * SNAP_PER_PEER
    }

    /// Follows the connected snap/2 peers, keeping at least one slot so that reth fails a
    /// request at once when no snap/2 peer is connected. Free slots are retired here; a slot
    /// held while the peers shrink is retired when its holder lets go of it.
    fn resize(&self) {
        let wanted = self.wanted();
        let mut capacity = self.capacity.lock();
        if wanted > *capacity {
            self.slots.add_permits(wanted - *capacity);
            *capacity = wanted;
        } else if wanted < *capacity {
            *capacity -= self.slots.forget_permits(*capacity - wanted);
        }
    }

    async fn snap<T>(
        &self,
        kind: &'static str,
        request: impl Future<Output = Result<T, NetError>>,
    ) -> Result<T, NetError> {
        let queued = Instant::now();
        self.resize();
        let permit = self.slots.acquire().await.map_err(|_| NetError::Stopped)?;
        let _slot = Slot {
            paced: self,
            permit: Some(permit),
        };
        let sent = Instant::now();
        let result = timed(request).await;
        debug!(
            target: "chibi::peers",
            kind,
            waited_ms = sent.duration_since(queued).as_millis(),
            took_ms = sent.elapsed().as_millis(),
            ok = result.is_ok(),
            "snap request",
        );
        result
    }
}

/// A held snap slot. On release the slots follow the snap/2 peers in both directions: the slot
/// is retired if peers have dropped below the slots in use, and slots are added if peers have
/// connected, so requests already queued use them without waiting for a new request.
struct Slot<'a, P: Peers> {
    paced: &'a Paced<P>,
    permit: Option<SemaphorePermit<'a>>,
}

impl<P: Peers> Drop for Slot<'_, P> {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        let wanted = self.paced.wanted();
        let mut capacity = self.paced.capacity.lock();
        if *capacity > wanted {
            permit.forget();
            *capacity -= 1;
        } else if wanted > *capacity {
            self.paced.slots.add_permits(wanted - *capacity);
            *capacity = wanted;
        }
    }
}

async fn timed<T>(request: impl Future<Output = Result<T, NetError>>) -> Result<T, NetError> {
    tokio::time::timeout(REQUEST_TIMEOUT, request)
        .await
        .unwrap_or(Err(NetError::Timeout))
}

impl<P: Peers> Peers for Paced<P> {
    async fn headers(&self, start: B256, count: u64) -> Result<Reply<Vec<Header>>, NetError> {
        timed(self.inner.headers(start, count)).await
    }

    async fn account_range(
        &self,
        request: AccountRangeRequest,
    ) -> Result<Reply<AccountReply>, NetError> {
        self.snap("account", self.inner.account_range(request))
            .await
    }

    async fn storage_range(
        &self,
        request: StorageRangeRequest,
    ) -> Result<Reply<StorageReply>, NetError> {
        self.snap("storage", self.inner.storage_range(request))
            .await
    }

    async fn byte_codes(&self, hashes: Vec<B256>) -> Result<Reply<Vec<Bytes>>, NetError> {
        self.snap("code", self.inner.byte_codes(hashes)).await
    }

    async fn block_access_lists(
        &self,
        blocks: Vec<B256>,
    ) -> Result<Reply<Vec<Option<Bytes>>>, NetError> {
        self.snap("bal", self.inner.block_access_lists(blocks))
            .await
    }

    async fn bodies(&self, blocks: Vec<B256>) -> Result<Reply<Vec<Body>>, NetError> {
        timed(self.inner.bodies(blocks)).await
    }

    async fn receipts(&self, block: B256) -> Result<Reply<Vec<Receipt>>, NetError> {
        timed(self.inner.receipts(block)).await
    }

    async fn broadcast(&self, transaction: Transaction) -> Result<usize, NetError> {
        self.inner.broadcast(transaction).await
    }

    fn penalize(&self, peer: PeerKey) {
        self.inner.penalize(peer);
    }

    fn announce(&self, head: &VerifiedHeader) {
        self.inner.announce(head);
    }

    fn census(&self) -> Census {
        self.inner.census()
    }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::{mpsc, oneshot};

    use super::*;

    /// Answers each account request when the test says so, with a snap/2 peer count the test
    /// sets.
    #[derive(Debug)]
    struct Stub {
        snap_peers: AtomicUsize,
        arrived: mpsc::UnboundedSender<oneshot::Sender<()>>,
    }

    impl Peers for Stub {
        async fn headers(&self, _: B256, _: u64) -> Result<Reply<Vec<Header>>, NetError> {
            unreachable!("headers")
        }

        async fn account_range(
            &self,
            _: AccountRangeRequest,
        ) -> Result<Reply<AccountReply>, NetError> {
            let (answer, answered) = oneshot::channel();
            self.arrived.send(answer).unwrap();
            answered.await.map_err(|_| NetError::Stopped)?;
            Ok(Reply {
                peer: PeerKey(B512::ZERO),
                value: AccountReply::default(),
            })
        }

        async fn storage_range(
            &self,
            _: StorageRangeRequest,
        ) -> Result<Reply<StorageReply>, NetError> {
            unreachable!("storage_range")
        }

        async fn byte_codes(&self, _: Vec<B256>) -> Result<Reply<Vec<Bytes>>, NetError> {
            unreachable!("byte_codes")
        }

        async fn block_access_lists(
            &self,
            _: Vec<B256>,
        ) -> Result<Reply<Vec<Option<Bytes>>>, NetError> {
            unreachable!("block_access_lists")
        }

        async fn bodies(&self, _: Vec<B256>) -> Result<Reply<Vec<Body>>, NetError> {
            unreachable!("bodies")
        }

        async fn receipts(&self, _: B256) -> Result<Reply<Vec<Receipt>>, NetError> {
            unreachable!("receipts")
        }

        async fn broadcast(&self, _: Transaction) -> Result<usize, NetError> {
            unreachable!("broadcast")
        }

        fn penalize(&self, _: PeerKey) {}

        fn announce(&self, _: &VerifiedHeader) {}

        fn census(&self) -> Census {
            Census {
                snap_peers: self.snap_peers.load(Ordering::SeqCst),
                ..Census::default()
            }
        }
    }

    async fn settle() {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn after_snap_peers_drop_from_four_to_one_only_one_request_is_out_at_a_time() {
        let (arrived, mut arrivals) = mpsc::unbounded_channel();
        let paced = Arc::new(Paced::new(Stub {
            snap_peers: AtomicUsize::new(4),
            arrived,
        }));
        let request = AccountRangeRequest {
            root: B256::ZERO,
            key: B256::ZERO,
        };
        let calls: Vec<_> = (0..8)
            .map(|_| {
                let paced = Arc::clone(&paced);
                tokio::spawn(async move { paced.account_range(request).await })
            })
            .collect();

        settle().await;
        let mut first = Vec::new();
        while let Ok(answer) = arrivals.try_recv() {
            first.push(answer);
        }
        assert_eq!(first.len(), 4);

        paced.inner.snap_peers.store(1, Ordering::SeqCst);
        for answer in first {
            answer.send(()).unwrap();
        }
        for _ in 0..4 {
            settle().await;
            let mut out = Vec::new();
            while let Ok(answer) = arrivals.try_recv() {
                out.push(answer);
            }
            assert_eq!(out.len(), 1, "requests out at the one remaining peer");
            for answer in out {
                answer.send(()).unwrap();
            }
        }
        for call in calls {
            assert!(call.await.unwrap().is_ok());
        }
    }

    #[tokio::test]
    async fn requests_queued_before_snap_peers_grow_from_one_to_four_go_out_four_at_a_time() {
        let (arrived, mut arrivals) = mpsc::unbounded_channel();
        let paced = Arc::new(Paced::new(Stub {
            snap_peers: AtomicUsize::new(1),
            arrived,
        }));
        let request = AccountRangeRequest {
            root: B256::ZERO,
            key: B256::ZERO,
        };
        let calls: Vec<_> = (0..9)
            .map(|_| {
                let paced = Arc::clone(&paced);
                tokio::spawn(async move { paced.account_range(request).await })
            })
            .collect();

        settle().await;
        let first = arrivals.try_recv().unwrap();
        assert!(arrivals.try_recv().is_err());

        paced.inner.snap_peers.store(4, Ordering::SeqCst);
        first.send(()).unwrap();
        for _ in 0..2 {
            settle().await;
            let mut out = Vec::new();
            while let Ok(answer) = arrivals.try_recv() {
                out.push(answer);
            }
            assert_eq!(out.len(), 4, "requests out across four snap/2 peers");
            for answer in out {
                answer.send(()).unwrap();
            }
        }
        for call in calls {
            assert!(call.await.unwrap().is_ok());
        }
    }
}
