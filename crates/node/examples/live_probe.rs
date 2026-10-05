//! Live check of the beacon and devp2p adapters against Sepolia.
//!
//! Syncs helios in a temporary directory from a community checkpoint, takes its newest attested
//! block hash, fetches that header over devp2p and checks its hash, then fetches raw single-key
//! snap/2 replies for the deposit contract's account and one USDC proxy slot, and the header's
//! BAL. Proof verification is chibi-core's job; this shows the raw replies are well-formed and
//! keyed correctly.
//!
//! Run with `cargo run -p chibi-node --example live_probe`. `RUST_LOG` overrides the log filter.

use std::collections::BTreeMap;
use std::error::Error;
use std::time::Duration;

use alloy_primitives::{Address, B256, address, b256, keccak256};
use chibi_core::verify;
use chibi_core::wire::{AccountRangeRequest, StorageRangeRequest};
use chibi_node::beacon::{self, Beacon, BeaconConfig};
use chibi_node::devp2p::{Devp2p, HeadHint, Ports, Sessions, rng_secret_key};
use chibi_node::{Heads, NetError, Network, NetworkSpec, Peers};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;

const DEPOSIT_CONTRACT: Address = address!("0x7f02C3E3c98b133055B8B348B2Ac625669Ed295D");
const DEPOSIT_CONTRACT_KEY: B256 =
    b256!("0xe745f3e97d33e9831bb765e53aca0f4ee529a7520b2db85c0ad7ca05bba40d9b");
const USDC: Address = address!("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");
/// `keccak256("org.zeppelinos.proxy.admin")`, the admin slot of USDC's proxy.
const PROXY_ADMIN_SLOT: B256 =
    b256!("0x10d6a54a4754c8869d6886b5f5d7fbfa5b4522237ea5c60d11bc4e7a1ff9390b");

const SNAP_PEER_WAIT: Duration = Duration::from_secs(120);
const ATTEMPTS: usize = 5;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,chibi::devp2p=info")),
        )
        .init();

    let spec = Network::Sepolia.spec();
    let data_dir = tempfile::tempdir()?;
    let checkpoint = community_checkpoint(&spec).await?;
    info!(%checkpoint, "community checkpoint");

    let (heads_tx, mut heads) = Heads::channel();
    let config = BeaconConfig {
        url: Url::parse(spec.default_beacon)?,
        data_dir: data_dir.path().join("helios"),
        checkpoint: Some(checkpoint),
    };
    let light_client = Beacon::start(&spec, config, heads_tx)?;
    light_client.wait_synced().await?;
    let trusted = (*heads.latest.wait_for(Option::is_some).await?).ok_or("no attested head")?;
    info!(hash = %trusted.hash(), "light client synced");

    let net = Devp2p::start(
        &spec,
        rng_secret_key(),
        Ports {
            rlpx: 0,
            discovery: 0,
        },
        HeadHint::Attested(trusted),
    )
    .await?;
    let mut sessions = net.sessions();
    let found = tokio::time::timeout(
        SNAP_PEER_WAIT,
        sessions.wait_for(|sessions| sessions.values().any(|session| session.snap)),
    )
    .await
    .is_ok_and(|seen| seen.is_ok());
    if !found {
        warn!(wait = ?SNAP_PEER_WAIT, "no snap/2 peer yet");
    }
    log_sessions(&sessions.borrow());

    let header = fetch_header(&net, trusted).await?;
    net.announce(&header);
    let state_root = header.header().state_root;
    info!(
        number = header.number(),
        hash = %header.hash(),
        %state_root,
        bal_hash = ?header.header().block_access_list_hash,
        "header verified against the light client",
    );

    let key = keccak256(DEPOSIT_CONTRACT);
    if key != DEPOSIT_CONTRACT_KEY {
        return Err("deposit contract hashes to an unexpected key".into());
    }
    let account = net
        .account_range(AccountRangeRequest {
            root: state_root,
            key,
        })
        .await?;
    let Some((_, deposit)) = account.value.accounts.iter().find(|(hash, _)| *hash == key) else {
        return Err(format!("account reply lacks {key}: {:?}", account.value.accounts).into());
    };
    info!(
        peer = %account.peer.0,
        leaves = account.value.accounts.len(),
        proof_nodes = account.value.proof.len(),
        nonce = deposit.nonce,
        balance = %deposit.balance,
        storage_root = %deposit.storage_root,
        code_hash = %deposit.code_hash,
        "deposit contract account reply",
    );

    let storage = net
        .storage_range(StorageRangeRequest {
            root: state_root,
            account: keccak256(USDC),
            slot: keccak256(PROXY_ADMIN_SLOT),
        })
        .await?;
    if storage.value.slots.is_empty() && storage.value.proof.is_empty() {
        return Err("storage reply is empty".into());
    }
    info!(
        peer = %storage.peer.0,
        slots = ?storage.value.slots,
        proof_nodes = storage.value.proof.len(),
        "USDC proxy admin slot reply",
    );

    match net.block_access_lists(vec![header.hash()]).await {
        Ok(reply) => info!(
            peer = %reply.peer.0,
            present = reply.value.iter().filter(|bal| bal.is_some()).count(),
            requested = reply.value.len(),
            "BAL reply",
        ),
        Err(err @ NetError::NoPeer { .. }) => info!(%err, "BAL reply"),
        Err(err) => return Err(err.into()),
    }

    log_sessions(&net.sessions().borrow());
    net.shutdown().await;
    light_client.shutdown();
    Ok(())
}

/// The community checkpoint, retrying because individual checkpoint services fail transiently.
async fn community_checkpoint(spec: &NetworkSpec) -> Result<B256, Box<dyn Error>> {
    let mut attempt = 1;
    loop {
        match beacon::community_checkpoint(spec).await {
            Ok(checkpoint) => return Ok(checkpoint),
            Err(err) if attempt < ATTEMPTS => warn!(attempt, %err, "community checkpoint failed"),
            Err(err) => return Err(err.into()),
        }
        attempt += 1;
    }
}

/// Fetches the header for `trusted` and checks its hash, retrying on peers that lack it.
async fn fetch_header(
    net: &Devp2p,
    trusted: chibi_core::TrustedHash,
) -> Result<chibi_core::VerifiedHeader, Box<dyn Error>> {
    for attempt in 1..=ATTEMPTS {
        match net.headers(trusted.hash(), 1).await {
            Ok(reply) => match reply.value.into_iter().next() {
                Some(header) => return Ok(verify::header(trusted, header)?),
                None => warn!(attempt, peer = %reply.peer.0, "peer returned no header"),
            },
            Err(err) => warn!(attempt, %err, "header request failed"),
        }
    }
    Err("no peer served the attested header".into())
}

fn log_sessions(sessions: &Sessions) {
    let mut clients: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for session in sessions.values() {
        let name = session.client.split('/').next().unwrap_or_default();
        let entry = clients.entry(name).or_default();
        entry.0 += 1;
        entry.1 += usize::from(session.snap);
    }
    let snap = sessions.values().filter(|session| session.snap).count();
    info!(
        peers = sessions.len(),
        snap_peers = snap,
        ?clients,
        "sessions (client: (peers, snap/2))"
    );
}
