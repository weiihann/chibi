//! Pure domain logic for chibi. Nothing in this crate performs I/O or depends on tokio.
//!
//! [`verify`] is the trust boundary: untrusted [`wire`] replies become verified values there
//! and nowhere else. [`state`] holds partial state built only from verified values, [`exec`]
//! runs the EVM over it and reports the keys it lacks, and [`tx`] tracks submitted
//! transactions, their receipts, and fee data.

pub mod exec;
pub mod state;
#[cfg(any(test, feature = "test-utils"))]
pub mod testing;
pub mod tx;
pub mod verify;
pub mod wire;

pub use verify::{TrustedHash, VerifiedHeader, VerifyError};
