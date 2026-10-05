//! The trust boundary. Peer bytes become domain values here and nowhere else.
//!
//! The only trusted input is a [`TrustedHash`] from the light client. Every other argument is
//! untrusted wire data. Each returned type has private fields and no public constructor, so
//! holding one is proof the check ran.

use alloy_consensus::{Header, Sealable, Sealed};
use alloy_primitives::{B256, BlockNumber};

/// An execution block hash the sync-committee light client vouched for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TrustedHash(B256);

impl TrustedHash {
    /// Marks `hash` as attested by the light client. Only the light-client adapter calls this.
    pub const fn attested(hash: B256) -> Self {
        Self(hash)
    }

    /// The attested block hash.
    pub const fn hash(&self) -> B256 {
        self.0
    }
}

/// A header whose hash is a [`TrustedHash`] or links to one through `parent_hash`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedHeader(Sealed<Header>);

impl VerifiedHeader {
    /// The verified block hash.
    pub fn hash(&self) -> B256 {
        self.0.hash()
    }

    /// The block number.
    pub fn number(&self) -> BlockNumber {
        self.0.number
    }

    /// The full header.
    pub fn header(&self) -> &Header {
        self.0.inner()
    }
}

/// Checks that `header` hashes to the light client's `trusted` hash.
pub fn header(trusted: TrustedHash, header: Header) -> Result<VerifiedHeader, VerifyError> {
    let sealed = header.seal_slow();
    if sealed.hash() != trusted.hash() {
        return Err(VerifyError::HeaderHash { expected: trusted.hash(), got: sealed.hash() });
    }
    Ok(VerifiedHeader(sealed))
}

/// Checks that `header` is the parent of the already verified `child`.
pub fn parent(child: &VerifiedHeader, header: Header) -> Result<VerifiedHeader, VerifyError> {
    let sealed = header.seal_slow();
    let expected = child.header().parent_hash;
    if sealed.hash() != expected {
        return Err(VerifyError::HeaderHash { expected, got: sealed.hash() });
    }
    Ok(VerifiedHeader(sealed))
}

/// A peer reply failed verification. The sender lied or the reply was garbled.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    /// A header does not hash to the trusted or linked hash.
    #[error("header hash {got} does not match expected {expected}")]
    HeaderHash {
        /// Hash the header had to match.
        expected: B256,
        /// Hash the header actually has.
        got: B256,
    },
}
