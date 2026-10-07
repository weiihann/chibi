//! Network fetches, each verified through `chibi_core::verify` before it is returned.
//!
//! Requests go through [`crate::peers::Paced`], which times each one from when it is sent.
//! Proofs get up to [`ATTEMPTS`] tries; a reply that proves its sender lied gets the sender
//! penalized. A key that cannot be proven is an error, never a zero.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use chibi_core::state::{HEADER_WINDOW, Key, Miss, Store};
use chibi_core::verify::{
    self, AccountQuery, CodeQuery, Proven, ProvenAccount, SlotProof, StorageQuery, VerifiedBlock,
    VerifiedBody, VerifiedReceipts,
};
use chibi_core::{TrustedHash, VerifiedHeader, VerifyError};
use futures::stream::{BoxStream, FuturesUnordered};
use futures::{Stream, StreamExt as _, TryStreamExt as _, stream};
use tokio::sync::Semaphore;
use tracing::debug;

use crate::node::NodeError;
use crate::peers::{NetError, PeerKey, Peers};

/// Tries per proof before giving up.
const ATTEMPTS: u32 = 3;

/// Requests one fetch keeps in flight at once.
pub(crate) const CONCURRENCY: usize = 16;

/// Why one attempt failed.
#[derive(Debug)]
enum Failure {
    Net(NetError),
    Lie(PeerKey, Box<VerifyError>),
    Unavailable(Box<VerifyError>),
}

impl Failure {
    fn verify(peer: PeerKey, err: VerifyError) -> Self {
        if err.blames_peer() {
            Self::Lie(peer, Box::new(err))
        } else {
            Self::Unavailable(Box::new(err))
        }
    }

    fn into_error(self) -> NodeError {
        match self {
            Self::Net(err) => NodeError::Net(err),
            Self::Lie(_, err) | Self::Unavailable(err) => NodeError::Unprovable(*err),
        }
    }
}

/// What a miss fetch produced.
#[derive(Debug)]
pub(crate) enum Fetched {
    /// A proven account, slot, or code.
    Proven(Proven),
    /// Verified ancestors of the oldest held header, oldest first.
    Ancestors(Vec<VerifiedHeader>),
}

fn sent<T>(result: Result<T, NetError>) -> Result<T, Failure> {
    result.map_err(Failure::Net)
}

/// Runs `attempt` until it succeeds, a retry cannot help, or attempts run out.
async fn retry<P, T, F, Fut>(peers: &P, mut attempt: F) -> Result<T, NodeError>
where
    P: Peers,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Failure>>,
{
    let mut last = None;
    for round in 1..=ATTEMPTS {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(Failure::Net(err @ (NetError::NoPeer { .. } | NetError::Stopped))) => {
                return Err(NodeError::Net(err));
            }
            Err(Failure::Lie(peer, err)) => {
                debug!(target: "chibi::fetch", round, %err, "reply failed verification");
                peers.penalize(peer);
                last = Some(Failure::Lie(peer, err));
            }
            Err(failure @ (Failure::Net(_) | Failure::Unavailable(_))) => {
                debug!(target: "chibi::fetch", round, ?failure, "attempt failed");
                last = Some(failure);
            }
        }
    }
    Err(last.map_or(NodeError::Net(NetError::Rejected), Failure::into_error))
}

/// The header the light client attested as `trusted`.
pub(crate) async fn header<P: Peers>(
    peers: &P,
    trusted: TrustedHash,
) -> Result<VerifiedHeader, NodeError> {
    retry(peers, || async {
        let reply = sent(peers.headers(trusted.hash(), 1).await)?;
        let header = reply
            .value
            .into_iter()
            .next()
            .ok_or(Failure::Net(NetError::Rejected))?;
        verify::header(trusted, header).map_err(|err| Failure::verify(reply.peer, err))
    })
    .await
}

/// Up to `count` verified ancestors of `child`, oldest first. A peer may return fewer.
pub(crate) async fn ancestors<P: Peers>(
    peers: &P,
    child: &VerifiedHeader,
    count: u64,
) -> Result<Vec<VerifiedHeader>, NodeError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    retry(peers, || async {
        let reply = sent(peers.headers(child.header().parent_hash, count).await)?;
        if reply.value.is_empty() {
            return Err(Failure::Net(NetError::Rejected));
        }
        verify::ancestors(child, reply.value).map_err(|err| Failure::verify(reply.peer, err))
    })
    .await
}

/// The verified chain from `anchor`'s child through `target`. One attempt: a break means a
/// reorg or a short reply, and the caller reanchors either way.
pub(crate) async fn chain<P: Peers>(
    peers: &P,
    anchor: &VerifiedHeader,
    target: VerifiedHeader,
) -> Result<Vec<VerifiedHeader>, NodeError> {
    let between = target.number().saturating_sub(anchor.number() + 1);
    let (peer, headers) = if between == 0 {
        (None, Vec::new())
    } else {
        let reply = sent(peers.headers(target.header().parent_hash, between).await)
            .map_err(Failure::into_error)?;
        (Some(reply.peer), reply.value)
    };
    verify::chain(anchor, target, headers).map_err(|err| {
        if let Some(peer) = peer.filter(|_| err.blames_peer()) {
            peers.penalize(peer);
        }
        NodeError::Unprovable(err)
    })
}

/// One BAL per header, each checked against its header. One attempt: any failure makes the
/// caller reanchor instead.
pub(crate) async fn bals<P: Peers>(
    peers: &P,
    headers: Vec<VerifiedHeader>,
) -> Result<Vec<VerifiedBlock>, NodeError> {
    let hashes = headers.iter().map(VerifiedHeader::hash).collect();
    let reply = sent(peers.block_access_lists(hashes).await).map_err(Failure::into_error)?;
    let mut raws = reply.value.into_iter();
    let mut blocks = Vec::with_capacity(headers.len());
    for header in headers {
        match verify::bal(header, raws.next().flatten()) {
            Ok(block) => blocks.push(block),
            Err(err) => {
                if err.blames_peer() {
                    peers.penalize(reply.peer);
                }
                return Err(NodeError::Unprovable(err));
            }
        }
    }
    Ok(blocks)
}

/// The verified bodies of `headers`, in order. Peers cap replies by size, so a reply that
/// verified at least one more body is progress; only replies without any count as attempts.
pub(crate) async fn bodies<P: Peers>(
    peers: &P,
    headers: &[VerifiedHeader],
) -> Result<Vec<VerifiedBody>, NodeError> {
    let mut verified = Vec::with_capacity(headers.len());
    let mut stalls = 0;
    let mut last = None;
    while verified.len() < headers.len() && stalls < ATTEMPTS {
        let remaining = &headers[verified.len()..];
        let hashes = remaining.iter().map(VerifiedHeader::hash).collect();
        let before = verified.len();
        match sent(peers.bodies(hashes).await) {
            Ok(reply) => {
                for (header, body) in remaining.iter().zip(reply.value) {
                    match verify::body(header.clone(), body) {
                        Ok(body) => verified.push(body),
                        Err(err) => {
                            if err.blames_peer() {
                                peers.penalize(reply.peer);
                            }
                            last = Some(NodeError::Unprovable(err));
                            break;
                        }
                    }
                }
            }
            Err(Failure::Net(err @ (NetError::NoPeer { .. } | NetError::Stopped))) => {
                return Err(NodeError::Net(err));
            }
            Err(failure) => last = Some(failure.into_error()),
        }
        if verified.len() == before {
            stalls += 1;
        }
    }
    if verified.len() == headers.len() {
        Ok(verified)
    } else {
        Err(last.unwrap_or(NodeError::Net(NetError::Rejected)))
    }
}

/// The receipts of `header`'s block, checked against its receipts root.
pub(crate) async fn receipts<P: Peers>(
    peers: &P,
    header: &VerifiedHeader,
) -> Result<VerifiedReceipts, NodeError> {
    retry(peers, || async {
        let reply = sent(peers.receipts(header.hash()).await)?;
        verify::receipts(header.clone(), reply.value)
            .map_err(|err| Failure::verify(reply.peer, err))
    })
    .await
}

/// The account at `at`'s state root.
pub(crate) async fn account<P: Peers>(
    peers: &P,
    at: &VerifiedHeader,
    address: Address,
) -> Result<Proven, NodeError> {
    proven_account(peers, &Semaphore::new(1), AccountQuery::new(at, address))
        .await
        .map(|account| account.proven())
}

/// Holds one of `gate`'s permits while `request` is out.
async fn gated<T>(
    gate: &Semaphore,
    request: impl Future<Output = Result<T, NetError>>,
) -> Result<T, Failure> {
    let _permit = gate
        .acquire()
        .await
        .map_err(|_| Failure::Net(NetError::Stopped))?;
    sent(request.await)
}

async fn proven_account<P: Peers>(
    peers: &P,
    gate: &Semaphore,
    query: AccountQuery,
) -> Result<ProvenAccount, NodeError> {
    retry(peers, || async {
        let reply = gated(gate, peers.account_range(query.request())).await?;
        query
            .verify(reply.value)
            .map_err(|err| Failure::verify(reply.peer, err))
    })
    .await
}

async fn proven_slot<P: Peers>(
    peers: &P,
    gate: &Semaphore,
    query: StorageQuery,
) -> Result<Proven, NodeError> {
    retry(peers, || async {
        let reply = gated(gate, peers.storage_range(query.request())).await?;
        query
            .verify(reply.value)
            .map_err(|err| Failure::verify(reply.peer, err))
    })
    .await
}

async fn code<P: Peers>(peers: &P, gate: &Semaphore, hash: B256) -> Result<Proven, NodeError> {
    let query = CodeQuery::new(hash);
    retry(peers, || async {
        let reply = gated(gate, peers.byte_codes(vec![query.request()])).await?;
        query
            .verify(reply.value.into_iter().next())
            .map_err(|err| Failure::verify(reply.peer, err))
    })
    .await
}

/// Keys that could not be proven, and why.
#[derive(Debug)]
pub(crate) struct Unproven {
    pub(crate) keys: Vec<Key>,
    pub(crate) error: Box<NodeError>,
}

/// The keys of one account wanted in a round: its own fact, its slots, or both.
struct Wanted {
    address: Address,
    account: bool,
    slots: Vec<U256>,
}

impl Wanted {
    fn keys(&self) -> Vec<Key> {
        let account = self.account.then_some(Key::Account(self.address));
        let slots = self.slots.iter().map(|slot| Key::Slot(self.address, *slot));
        account.into_iter().chain(slots).collect()
    }
}

/// `keys` grouped by account, in order of first appearance, without repeats.
fn by_account(keys: impl IntoIterator<Item = Key>) -> Vec<Wanted> {
    let mut wanted: Vec<Wanted> = Vec::new();
    let mut index = HashMap::new();
    for key in keys {
        let address = match key {
            Key::Account(address) | Key::Slot(address, _) => address,
        };
        let at = *index.entry(address).or_insert_with(|| {
            wanted.push(Wanted {
                address,
                account: false,
                slots: Vec::new(),
            });
            wanted.len() - 1
        });
        let entry = &mut wanted[at];
        match key {
            Key::Account(_) => entry.account = true,
            Key::Slot(_, slot) if !entry.slots.contains(&slot) => entry.slots.push(slot),
            Key::Slot(..) => {}
        }
    }
    wanted
}

/// Proves `keys` at `at`, each account once for all its wanted slots, with at most `gate`'s
/// permits in requests at once. A bad account reply blames its sender and sinks every key of
/// the account; a bad storage reply blames its sender and sinks only its slot.
fn proofs<'a, P: Peers>(
    peers: &'a P,
    at: &'a VerifiedHeader,
    keys: Vec<Key>,
    gate: Arc<Semaphore>,
) -> impl Stream<Item = Result<Proven, Unproven>> + Send + 'a {
    stream::iter(by_account(keys))
        .map(move |wanted| account_keys(peers, at, wanted, Arc::clone(&gate)))
        .flatten_unordered(None)
}

fn account_keys<'a, P: Peers>(
    peers: &'a P,
    at: &'a VerifiedHeader,
    wanted: Wanted,
    gate: Arc<Semaphore>,
) -> BoxStream<'a, Result<Proven, Unproven>> {
    let query = AccountQuery::new(at, wanted.address);
    let account_gate = Arc::clone(&gate);
    stream::once(async move { proven_account(peers, &account_gate, query).await })
        .flat_map(move |proven| match proven {
            Err(error) => stream::iter([Err(Unproven {
                keys: wanted.keys(),
                error: Box::new(error),
            })])
            .left_stream(),
            Ok(account) => {
                let fact = wanted.account.then(|| Ok(account.proven()));
                let slots: FuturesUnordered<_> = wanted
                    .slots
                    .iter()
                    .map(|slot| {
                        let (key, proof) = (Key::Slot(wanted.address, *slot), account.slot(*slot));
                        let gate = Arc::clone(&gate);
                        async move {
                            match proof {
                                SlotProof::Known(proven) => Ok(proven),
                                SlotProof::Wanted(query) => proven_slot(peers, &gate, query)
                                    .await
                                    .map_err(|error| Unproven {
                                        keys: vec![key],
                                        error: Box::new(error),
                                    }),
                            }
                        }
                    })
                    .collect();
                stream::iter(fact).chain(slots).right_stream()
            }
        })
        .boxed()
}

/// Proves every key at `at`. Fails if any key fails.
pub(crate) async fn keys<P: Peers>(
    peers: &P,
    at: &VerifiedHeader,
    keys: Vec<Key>,
) -> Result<Vec<Proven>, NodeError> {
    proofs(peers, at, keys, Arc::new(Semaphore::new(CONCURRENCY)))
        .map_err(|unproven| *unproven.error)
        .try_collect()
        .await
}

/// Proves each key at `at` with at most `in_flight` requests at once, yielding every proof
/// and every failure with the keys it sank, so a key that cannot be proven never sinks the
/// keys it does not depend on.
pub(crate) fn each<'a, P: Peers>(
    peers: &'a P,
    at: &'a VerifiedHeader,
    keys: Vec<Key>,
    in_flight: usize,
) -> impl Stream<Item = Result<Proven, Unproven>> + 'a {
    proofs(peers, at, keys, Arc::new(Semaphore::new(in_flight)))
}

/// Fetches everything `misses` names at `store`'s anchor, in parallel. Block-hash misses
/// become one ancestor fetch reaching the oldest of them.
pub(crate) async fn misses<P: Peers>(
    peers: &P,
    store: &Store,
    misses: Vec<Miss>,
) -> Result<Vec<Fetched>, NodeError> {
    let at = store.anchor();
    let mut deepest: Option<u64> = None;
    let mut keys = Vec::new();
    let mut codes = Vec::new();
    for miss in misses {
        match miss {
            Miss::Account(address) => keys.push(Key::Account(address)),
            Miss::Storage(address, index) => keys.push(Key::Slot(address, index)),
            Miss::Code(hash) => codes.push(hash),
            Miss::BlockHash(number) => {
                deepest = Some(deepest.map_or(number, |deepest| deepest.min(number)));
            }
        }
    }
    let ancestors = async {
        let Some(number) = deepest else {
            return Ok(None);
        };
        let Some(oldest) = store.headers().next() else {
            return Ok(None);
        };
        let held = store.headers().count() - 1;
        let room = HEADER_WINDOW.saturating_sub(held) as u64;
        let count = oldest.number().saturating_sub(number).min(room);
        if count == 0 {
            return Err(NodeError::Unsupported {
                what: "block hashes older than the header window",
                head: at.number(),
            });
        }
        ancestors(peers, oldest, count).await.map(Some)
    };
    let gate = Arc::new(Semaphore::new(CONCURRENCY));
    let codes = stream::iter(codes)
        .map(|hash| {
            let gate = Arc::clone(&gate);
            async move { code(peers, &gate, hash).await }
        })
        .buffer_unordered(CONCURRENCY);
    let keys = proofs(peers, at, keys, Arc::clone(&gate)).map_err(|unproven| *unproven.error);
    let proven = stream::select(keys, codes)
        .map_ok(Fetched::Proven)
        .try_collect::<Vec<_>>();
    let (ancestors, mut fetched) = tokio::try_join!(ancestors, proven)?;
    fetched.extend(ancestors.map(Fetched::Ancestors));
    Ok(fetched)
}
