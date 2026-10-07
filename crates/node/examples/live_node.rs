//! Live smoke test of `Node::start` on Sepolia.
//!
//! Starts a node in a temporary data directory watching the address given on the command
//! line, logs its status once anchored, then after each of two follower steps one slot apart.
//!
//! Run with `cargo run -p chibi-node --example live_node -- 0x7f02C3E3c98b133055B8B348B2Ac625669Ed295D`.
//! `RUST_LOG` overrides the log filter.

use std::error::Error;
use std::time::Duration;

use alloy_primitives::Address;
use chibi_node::{NetConfig, Network, Node, NodeConfig, Watchlist};
use tracing::info;
use tracing_subscriber::EnvFilter;

const SLOT: Duration = Duration::from_secs(12);

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,chibi=info")),
        )
        .init();

    let watched: Address = std::env::args()
        .nth(1)
        .ok_or("usage: live_node <address>")?
        .parse()?;
    let data_dir = tempfile::tempdir()?;
    let config = NodeConfig {
        network: Network::Sepolia,
        data_dir: data_dir.path().to_path_buf(),
        beacon_url: None,
        checkpoint: None,
        watch: Watchlist {
            addresses: vec![watched],
            contracts: Vec::new(),
        },
        net: NetConfig::default(),
    };

    let (node, mut follower) = Node::start(config, || info!("found a state peer")).await?;
    info!(status = ?node.status(), "started");
    for step in 1..=2 {
        tokio::time::sleep(SLOT).await;
        let stepped = follower.step().await?;
        info!(step, ?stepped, status = ?node.status(), "stepped");
    }
    Ok(())
}
