//! The light-client adapter. helios runs its own tasks; two forwarders copy its output into the
//! latest-only [`crate::Heads`] channels, so a slow follower never blocks helios's sync loop.
//!
//! helios's checkpoint is set only from a user override. Otherwise helios resumes from the
//! checkpoint it saved in the data directory, falling back to its shipped default. The shipped
//! default must never be passed as an override, or helios would ignore its saved checkpoint and
//! fail once the default is older than the maximum checkpoint age.

use std::path::PathBuf;
use std::sync::Arc;

use alloy_primitives::B256;
use chibi_core::TrustedHash;
use helios_consensus_core::consensus_spec::MainnetConsensusSpec;
use helios_consensus_core::errors::ConsensusError;
use helios_core::consensus::Consensus;
use helios_ethereum::config::Config;
use helios_ethereum::config::checkpoints::CheckpointFallback;
use helios_ethereum::consensus::ConsensusClient;
use helios_ethereum::database::FileDB;
use helios_ethereum::rpc::http_rpc::HttpRpc;
use tokio::task::JoinHandle;
use tracing::{debug, info};
use url::Url;

use crate::heads::HeadsSender;
use crate::network::NetworkSpec;

type Client = ConsensusClient<MainnetConsensusSpec, HttpRpc, FileDB>;

/// Where and how the light client syncs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeaconConfig {
    /// Beacon REST endpoint serving the light-client API.
    pub url: Url,
    /// Directory where helios persists its newest finalized checkpoint.
    pub data_dir: PathBuf,
    /// A beacon block root the user chose to trust. `None` resumes from the saved checkpoint.
    pub checkpoint: Option<B256>,
}

/// A running helios light client feeding [`crate::Heads`].
///
/// Dropping it shuts helios down.
pub struct Beacon {
    client: Client,
    forwarders: [JoinHandle<()>; 2],
}

impl Beacon {
    /// Starts helios for `spec` and forwards its attested hashes into `heads`.
    ///
    /// Must be called inside a tokio runtime; helios spawns its own tasks.
    pub fn start(
        spec: &NetworkSpec,
        config: BeaconConfig,
        heads: HeadsSender,
    ) -> Result<Self, BeaconError> {
        let mut base = spec.helios.to_base_config();
        base.consensus_rpc = Some(config.url.clone());
        base.data_dir = Some(config.data_dir);
        base.load_external_fallback = false;
        base.strict_checkpoint_age = true;
        let mut helios_config = Config::from(base);
        helios_config.checkpoint = config.checkpoint;

        let mut client = Client::new(&config.url, Arc::new(helios_config))
            .map_err(|err| BeaconError::Start(format!("{err:#}")))?;
        let (Some(mut blocks), Some(mut finalized)) =
            (client.block_recv(), client.finalized_block_recv())
        else {
            return Err(BeaconError::Start(
                "helios handed out its receivers twice".to_owned(),
            ));
        };
        let HeadsSender {
            latest,
            finalized: finalized_tx,
        } = heads;

        let latest_forwarder = tokio::spawn(async move {
            while let Some(block) = blocks.recv().await {
                let hash = block.block_hash();
                debug!(target: "chibi::beacon", %hash, "attested latest");
                latest.send_replace(Some(TrustedHash::attested(hash)));
            }
        });
        let finalized_forwarder = tokio::spawn(async move {
            loop {
                let hash = finalized
                    .borrow_and_update()
                    .as_ref()
                    .map(|block| block.block_hash());
                if let Some(hash) = hash {
                    debug!(target: "chibi::beacon", %hash, "attested finalized");
                    finalized_tx.send_replace(Some(TrustedHash::attested(hash)));
                }
                if finalized.changed().await.is_err() {
                    break;
                }
            }
        });

        info!(target: "chibi::beacon", network = %spec.network, url = %config.url, "light client started");
        Ok(Self {
            client,
            forwarders: [latest_forwarder, finalized_forwarder],
        })
    }

    /// Waits until helios has verified a bootstrap, the sync-committee updates, and a finality
    /// update, and has published its first heads.
    pub async fn wait_synced(&self) -> Result<(), BeaconError> {
        self.client.wait_synced().await.map_err(|err| {
            // helios reports sync failures as strings, so expiry is recognized by its message.
            let message = format!("{err:#}");
            if message.ends_with(&ConsensusError::CheckpointTooOld.to_string()) {
                BeaconError::CheckpointExpired
            } else {
                BeaconError::Sync(message)
            }
        })
    }

    /// Stops helios's tasks and the forwarders. The [`crate::Heads`] channels then close.
    pub fn shutdown(&self) {
        // An error only means helios's tasks have already exited.
        let _ = self.client.shutdown();
        for forwarder in &self.forwarders {
            forwarder.abort();
        }
    }
}

impl Drop for Beacon {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for Beacon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Beacon").finish_non_exhaustive()
    }
}

/// The newest finalized beacon block root that most of the community checkpoint-sync services
/// (ethPandaOps's list) agree on.
///
/// This trusts those services for weak subjectivity. It suits a first run, when helios has no
/// saved checkpoint and its shipped default is too old; pass the result as
/// [`BeaconConfig::checkpoint`].
pub async fn community_checkpoint(spec: &NetworkSpec) -> Result<B256, BeaconError> {
    let services = CheckpointFallback::new()
        .build()
        .await
        .map_err(|err| BeaconError::Checkpoint(format!("{err:#}")))?;
    services
        .fetch_latest_checkpoint(&spec.helios)
        .await
        .map_err(|err| BeaconError::Checkpoint(format!("{err:#}")))
}

/// The light client failed. helios reports errors as text, which these variants carry.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BeaconError {
    /// helios could not be constructed.
    #[error("light client failed to start: {0}")]
    Start(String),
    /// The checkpoint is older than the network's weak-subjectivity limit.
    #[error("light client checkpoint is too old; supply a recent checkpoint")]
    CheckpointExpired,
    /// Bootstrap, update, or finality verification or download failed.
    #[error("light client sync failed: {0}")]
    Sync(String),
    /// No community checkpoint could be fetched.
    #[error("no community checkpoint available: {0}")]
    Checkpoint(String),
}
