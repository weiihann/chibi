//! Network fetches, each verified through `chibi_core::verify` before it is returned.
//!
//! Requests go through [`crate::peers::Paced`], which times each one from when it is sent.
//! Proofs get up to [`ATTEMPTS`] tries; a reply that proves its sender lied gets the sender
//! penalized. A key that cannot be proven is an error, never a zero.

use alloy_primitives::{Address, B256, U256};
use chibi_core::state::{HEADER_WINDOW, Key, Miss, Store};
use chibi_core::verify::{
    self, AccountQuery, CodeQuery, Proven, StorageQuery, VerifiedBlock, VerifiedBody,
    VerifiedReceipts,
};
use chibi_core::{TrustedHash, VerifiedHeader, VerifyError};
use futures::{Stream, StreamExt as _, TryStreamExt as _, stream};
use tracing::debug;

use crate::node::NodeError;
use crate::peers::{NetError, PeerKey, Peers};

/// Tries per proof before giving up.
const ATTEMPTS: u32 = 3;

/// Proofs in flight at once.
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
    let query = AccountQuery::new(at, address);
    retry(peers, || async {
        let reply = sent(peers.account_range(query.request()).await)?;
        query
            .verify(reply.value)
            .map_err(|err| Failure::verify(reply.peer, err))
    })
    .await
}

/// One slot and its account at `at`'s state root, both requests in flight together.
pub(crate) async fn slot<P: Peers>(
    peers: &P,
    at: &VerifiedHeader,
    address: Address,
    slot: U256,
) -> Result<Proven, NodeError> {
    let query = StorageQuery::new(at, address, slot);
    retry(peers, || async {
        let (account, storage) = tokio::join!(
            peers.account_range(query.account_request()),
            peers.storage_range(query.storage_request()),
        );
        let account = sent(account)?;
        let (storage_peer, storage, storage_failure) = match sent(storage) {
            Ok(reply) => (Some(reply.peer), Some(reply.value), None),
            Err(failure) => (None, None, Some(failure)),
        };
        let account_copy = account.value.clone();
        let err = match query.verify(account.value, storage) {
            Ok(proven) => return Ok(proven),
            Err(err) => err,
        };
        if !err.blames_peer() {
            return Err(storage_failure.unwrap_or(Failure::verify(account.peer, err)));
        }
        // Either half can carry the lie; the account half alone tells which.
        let account_ok = AccountQuery::new(at, address).verify(account_copy).is_ok();
        let liar = match (account_ok, storage_peer) {
            (true, Some(peer)) => peer,
            (true, None) | (false, _) => account.peer,
        };
        Err(Failure::verify(liar, err))
    })
    .await
}

/// Code by hash.
pub(crate) async fn code<P: Peers>(peers: &P, hash: B256) -> Result<Proven, NodeError> {
    let query = CodeQuery::new(hash);
    retry(peers, || async {
        let reply = sent(peers.byte_codes(vec![query.request()]).await)?;
        query
            .verify(reply.value.into_iter().next())
            .map_err(|err| Failure::verify(reply.peer, err))
    })
    .await
}

/// Proves `key` at `at`.
pub(crate) async fn key<P: Peers>(
    peers: &P,
    at: &VerifiedHeader,
    key: Key,
) -> Result<Proven, NodeError> {
    match key {
        Key::Account(address) => account(peers, at, address).await,
        Key::Slot(address, index) => slot(peers, at, address, index).await,
    }
}

/// Proves every key at `at` with bounded concurrency. Fails if any key fails.
pub(crate) async fn keys<P: Peers>(
    peers: &P,
    at: &VerifiedHeader,
    keys: Vec<Key>,
) -> Result<Vec<Proven>, NodeError> {
    stream::iter(keys)
        .map(|k| key(peers, at, k))
        .buffer_unordered(CONCURRENCY)
        .try_collect()
        .await
}

/// Proves each key at `at` with at most `in_flight` proofs at once, yielding every key's own
/// result, so one key that cannot be proven never sinks the others.
pub(crate) fn each<'a, P: Peers>(
    peers: &'a P,
    at: &'a VerifiedHeader,
    keys: Vec<Key>,
    in_flight: usize,
) -> impl Stream<Item = (Key, Result<Proven, NodeError>)> + 'a {
    stream::iter(keys)
        .map(move |k| async move { (k, key(peers, at, k).await) })
        .buffer_unordered(in_flight)
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
    let mut proofs = Vec::with_capacity(misses.len());
    for miss in misses {
        match miss {
            Miss::Account(address) => proofs.push(Key::Account(address).into()),
            Miss::Storage(address, index) => proofs.push(Key::Slot(address, index).into()),
            Miss::Code(hash) => proofs.push(Wanted::Code(hash)),
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
    let proven = stream::iter(proofs)
        .map(|wanted| async move {
            match wanted {
                Wanted::Key(k) => key(peers, at, k).await,
                Wanted::Code(hash) => code(peers, hash).await,
            }
        })
        .buffer_unordered(CONCURRENCY)
        .map_ok(Fetched::Proven)
        .try_collect::<Vec<_>>();
    let (ancestors, mut fetched) = tokio::try_join!(ancestors, proven)?;
    fetched.extend(ancestors.map(Fetched::Ancestors));
    Ok(fetched)
}

enum Wanted {
    Key(Key),
    Code(B256),
}

impl From<Key> for Wanted {
    fn from(key: Key) -> Self {
        Self::Key(key)
    }
}
