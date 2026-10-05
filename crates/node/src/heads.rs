//! The light-client seam: the newest and the finalized execution block hash the light client
//! vouches for. The beacon adapter feeds it from helios; tests feed it directly.

use chibi_core::TrustedHash;
use tokio::sync::watch;

/// Latest-only views of the light client's output. A slow reader never blocks the producer.
#[derive(Clone, Debug)]
pub struct Heads {
    /// The newest attested execution block hash.
    pub latest: watch::Receiver<Option<TrustedHash>>,
    /// The newest finalized execution block hash.
    pub finalized: watch::Receiver<Option<TrustedHash>>,
}

/// The producing side of [`Heads`].
#[derive(Debug)]
pub struct HeadsSender {
    /// Publishes a new latest hash.
    pub latest: watch::Sender<Option<TrustedHash>>,
    /// Publishes a new finalized hash.
    pub finalized: watch::Sender<Option<TrustedHash>>,
}

impl Heads {
    /// A connected sender and receiver pair with nothing published yet.
    pub fn channel() -> (HeadsSender, Self) {
        let (latest_tx, latest) = watch::channel(None);
        let (finalized_tx, finalized) = watch::channel(None);
        (HeadsSender { latest: latest_tx, finalized: finalized_tx }, Self { latest, finalized })
    }
}
