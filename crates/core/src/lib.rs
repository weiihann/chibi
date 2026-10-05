//! Pure domain logic for chibi. Nothing in this crate performs I/O or depends on tokio.
//!
//! [`verify`] is the trust boundary: untrusted [`wire`] replies become verified values there
//! and nowhere else.

pub mod verify;
pub mod wire;

pub use verify::{TrustedHash, VerifiedHeader, VerifyError};
