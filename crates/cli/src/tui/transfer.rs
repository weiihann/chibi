//! Unlocking the keystore, and building, signing and submitting a transfer from it.

use std::io;
use std::path::{Path, PathBuf};

use alloy_consensus::{SignableTransaction as _, TxEip1559};
use alloy_eips::eip2718::Encodable2718 as _;
use alloy_primitives::{Address, Bytes, TxHash, TxKind, U256};
use alloy_signer::SignerSync as _;
use alloy_signer_local::{LocalSignerError, PrivateKeySigner};
use alloy_sol_types::SolCall as _;
use chibi_core::exec::{CallRequest, EstimateError};
use chibi_core::wire;
use chibi_node::{BlockSel, Node, NodeError, Peers, Token};

alloy_sol_types::sol! {
    function transfer(address to, uint256 amount) returns (bool);
}

/// What a transfer moves: ether or a registry token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Asset {
    /// Ether, sent as the transaction's value.
    Eth,
    /// An ERC-20 token, sent by calling its `transfer`.
    Token(Token),
}

impl Asset {
    /// The ticker shown to the user.
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Eth => "ETH",
            Self::Token(token) => token.symbol,
        }
    }

    /// Decimal places of one whole unit.
    pub const fn decimals(self) -> u8 {
        match self {
            Self::Eth => crate::ETH_DECIMALS,
            Self::Token(token) => token.decimals,
        }
    }
}

/// A checked transfer, before fees: `amount` base units of `asset` to `to`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Draft {
    /// What is sent.
    pub asset: Asset,
    /// The recipient.
    pub to: Address,
    /// Base units sent.
    pub amount: U256,
}

impl Draft {
    /// The transaction's target, value and calldata.
    fn call(&self) -> (Address, U256, Bytes) {
        match self.asset {
            Asset::Eth => (self.to, self.amount, Bytes::new()),
            Asset::Token(token) => (
                token.address,
                U256::ZERO,
                transferCall {
                    to: self.to,
                    amount: self.amount,
                }
                .abi_encode()
                .into(),
            ),
        }
    }
}

/// The gas limit and EIP-1559 fee caps of one transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fee {
    /// Gas the transfer may use, as estimated.
    pub gas_limit: u64,
    /// Twice the next block's base fee plus the tip.
    pub max_fee_per_gas: u128,
    /// The tip the node suggests.
    pub max_priority_fee_per_gas: u128,
}

impl Fee {
    /// The most the transfer can cost in fees, in wei.
    pub fn max_cost(&self) -> U256 {
        U256::from(self.gas_limit) * U256::from(self.max_fee_per_gas)
    }
}

/// Why a transfer was not estimated or sent.
#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    /// The transfer reverts at the head; nothing is signed.
    #[error("the transfer reverts: {0}")]
    Reverts(String),
    /// The node could not estimate, sign for, or submit the transfer.
    #[error(transparent)]
    Node(#[from] NodeError),
    /// Signing failed.
    #[error("cannot sign: {0}")]
    Sign(#[from] alloy_signer::Error),
}

/// Why the keystore did not unlock.
#[derive(Debug, thiserror::Error)]
pub enum UnlockError {
    /// The keystore file cannot be read.
    #[error("cannot read the keystore {}: {source}", path.display())]
    Unreadable {
        /// The keystore path.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// The password does not decrypt the keystore.
    #[error("wrong password")]
    WrongPassword,
    /// The file is not a keystore chibi can decrypt.
    #[error("cannot decrypt the keystore {}: {reason}", path.display())]
    Invalid {
        /// The keystore path.
        path: PathBuf,
        /// What the decryption reported.
        reason: String,
    },
}

/// Checks that the keystore at `path` can be read, before anything else starts.
pub fn check_readable(path: &Path) -> Result<(), UnlockError> {
    std::fs::File::open(path)
        .map(drop)
        .map_err(|source| UnlockError::Unreadable {
            path: path.to_owned(),
            source,
        })
}

/// Decrypts the keystore at `path` with `password`. Slow by design (scrypt or PBKDF2).
pub fn unlock(path: &Path, password: &str) -> Result<PrivateKeySigner, UnlockError> {
    check_readable(path)?;
    PrivateKeySigner::decrypt_keystore(path, password).map_err(|err| match err {
        LocalSignerError::EthKeystoreError(eth_keystore::KeystoreError::MacMismatch) => {
            UnlockError::WrongPassword
        }
        other => UnlockError::Invalid {
            path: path.to_owned(),
            reason: other.to_string(),
        },
    })
}

/// Estimates the transfer's gas at the head and caps its fees: twice the next block's base
/// fee plus the suggested tip, so the cap survives several full blocks.
pub async fn estimate<P: Peers>(
    node: &Node<P>,
    from: Address,
    draft: &Draft,
) -> Result<Fee, TransferError> {
    let tip = node.max_priority_fee().await?;
    let history = node.fee_history(1, BlockSel::Latest, &[]).await?;
    let next_base_fee = history.base_fee_per_gas.last().copied().unwrap_or_default();
    let (to, value, input) = draft.call();
    let request = CallRequest {
        from: Some(from),
        to: TxKind::Call(to),
        value,
        input,
        ..CallRequest::default()
    };
    let gas_limit = node
        .estimate_gas(request, BlockSel::Latest)
        .await
        .map_err(|err| match err {
            NodeError::Estimate(EstimateError::Reverted { output }) => {
                TransferError::Reverts(revert_reason(&output))
            }
            other => TransferError::Node(other),
        })?;
    Ok(Fee {
        gas_limit,
        max_fee_per_gas: next_base_fee.saturating_mul(2).saturating_add(tip),
        max_priority_fee_per_gas: tip,
    })
}

/// Signs the transfer with `signer` at its next pending nonce and submits it.
pub async fn send<P: Peers>(
    node: &Node<P>,
    signer: &PrivateKeySigner,
    draft: &Draft,
    fee: &Fee,
) -> Result<TxHash, TransferError> {
    let nonce = node
        .transaction_count(signer.address(), BlockSel::Pending)
        .await?;
    let (to, value, input) = draft.call();
    let tx = TxEip1559 {
        chain_id: node.chain_id(),
        nonce,
        gas_limit: fee.gas_limit,
        max_fee_per_gas: fee.max_fee_per_gas,
        max_priority_fee_per_gas: fee.max_priority_fee_per_gas,
        to: TxKind::Call(to),
        value,
        access_list: Default::default(),
        input,
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash())?;
    let signed = wire::Transaction::Eip1559(tx.into_signed(signature));
    Ok(node.submit(&signed.encoded_2718()).await?)
}

/// The revert reason the output carries, as shown to the user.
fn revert_reason(output: &[u8]) -> String {
    if output.is_empty() {
        return "no reason given".to_owned();
    }
    alloy_sol_types::decode_revert_reason(output)
        .unwrap_or_else(|| format!("revert data {}", Bytes::copy_from_slice(output)))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use chibi_core::testing::{
        WorldAccount, eth, mapping_slot, pausable_token_code, transfer_calldata,
    };
    use chibi_node::fixture::{Fixture, FixturePeers};
    use chibi_node::{Disk, Follower, Watchlist};

    use super::*;

    const BOB: Address = address!("0x0000000000000000000000000000000000000b0b");
    const TOKEN: Address = address!("0x00000000000000000000000000000000000070c3");
    const TKN: Token = Token {
        symbol: "TKN",
        name: "Test token",
        address: TOKEN,
        decimals: 6,
    };
    const TOKENS: &[Token] = &[TKN];
    const KEY: [u8; 32] = [0x42; 32];

    async fn start(
        fixture: &Fixture,
        dir: &Path,
        sender: Address,
    ) -> (Node<FixturePeers>, Follower<FixturePeers>) {
        Node::with_parts(
            Fixture::spec(TOKENS),
            fixture.peers(),
            fixture.heads(),
            Disk::open(dir).unwrap(),
            &Watchlist {
                addresses: vec![sender],
                contracts: Vec::new(),
            },
        )
        .await
        .unwrap()
    }

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::from_slice(&KEY).unwrap()
    }

    #[tokio::test]
    async fn an_eth_transfer_is_estimated_signed_and_submitted_pending() {
        let signer = signer();
        let sender = signer.address();
        let fixture = Fixture::genesis([(sender, WorldAccount::eoa(eth(3)))]);
        let dir = tempfile::tempdir().unwrap();
        let (node, _follower) = start(&fixture, dir.path(), sender).await;
        let draft = Draft {
            asset: Asset::Eth,
            to: BOB,
            amount: eth(1),
        };

        let fee = estimate(&node, sender, &draft).await.unwrap();
        assert_eq!(
            fee,
            Fee {
                gas_limit: 21_000,
                max_fee_per_gas: 2_750_000_000,
                max_priority_fee_per_gas: 1_000_000_000,
            }
        );
        assert_eq!(fee.max_cost(), U256::from(57_750_000_000_000u64));

        let hash = send(&node, &signer, &draft, &fee).await.unwrap();
        let broadcasts = fixture.log().broadcasts;
        assert_eq!(broadcasts.len(), 1);
        let sent = &broadcasts[0].transaction;
        assert_eq!(*sent.tx_hash(), hash);
        let wire::Transaction::Eip1559(signed) = sent else {
            panic!("sent {sent:?}, not an EIP-1559 transaction");
        };
        assert_eq!(
            signed.tx(),
            &TxEip1559 {
                chain_id: 1,
                nonce: 0,
                gas_limit: 21_000,
                max_fee_per_gas: 2_750_000_000,
                max_priority_fee_per_gas: 1_000_000_000,
                to: TxKind::Call(BOB),
                value: eth(1),
                access_list: Default::default(),
                input: Bytes::new(),
            }
        );
        let pending = node.transaction(hash).unwrap().unwrap();
        assert_eq!((pending.from, pending.included), (sender, None));
        assert_eq!(node.receipt(hash).unwrap(), None);
        assert_eq!(
            node.transaction_count(sender, BlockSel::Pending)
                .await
                .unwrap(),
            1
        );
    }

    fn token_world(holder: Address, balance: u64) -> Fixture {
        Fixture::genesis([
            (holder, WorldAccount::eoa(eth(3))),
            (
                TOKEN,
                WorldAccount::contract(pausable_token_code())
                    .with_slot(mapping_slot(holder), U256::from(balance)),
            ),
        ])
    }

    #[tokio::test]
    async fn a_token_transfer_calls_transfer_on_the_token_contract() {
        let signer = signer();
        let sender = signer.address();
        let fixture = token_world(sender, 500_000_000);
        let dir = tempfile::tempdir().unwrap();
        let (node, _follower) = start(&fixture, dir.path(), sender).await;
        let draft = Draft {
            asset: Asset::Token(TKN),
            to: BOB,
            amount: U256::from(200_000_000),
        };

        let fee = estimate(&node, sender, &draft).await.unwrap();
        let hash = send(&node, &signer, &draft, &fee).await.unwrap();
        let broadcasts = fixture.log().broadcasts;
        assert_eq!(broadcasts.len(), 1);
        let wire::Transaction::Eip1559(signed) = &broadcasts[0].transaction else {
            panic!("not an EIP-1559 transaction");
        };
        let tx = signed.tx();
        assert_eq!(
            (tx.to, tx.value, &tx.input),
            (
                TxKind::Call(TOKEN),
                U256::ZERO,
                &transfer_calldata(BOB, U256::from(200_000_000))
            )
        );
        assert_eq!(
            tx.input.to_string(),
            "0xa9059cbb0000000000000000000000000000000000000000000000000000000000000b0b000000000000000000000000000000000000000000000000000000000bebc200"
        );
        assert_eq!(*signed.hash(), hash);
        assert_eq!(node.transaction(hash).unwrap().unwrap().included, None);
    }

    #[tokio::test]
    async fn a_reverting_transfer_shows_why_and_signs_nothing() {
        let signer = signer();
        let sender = signer.address();
        let fixture = token_world(sender, 100);
        let dir = tempfile::tempdir().unwrap();
        let (node, _follower) = start(&fixture, dir.path(), sender).await;
        let draft = Draft {
            asset: Asset::Token(TKN),
            to: BOB,
            amount: U256::from(101),
        };
        let error = estimate(&node, sender, &draft).await.unwrap_err();
        assert_eq!(error.to_string(), "the transfer reverts: no reason given");
        assert!(fixture.log().broadcasts.is_empty());
        assert_eq!(
            revert_reason(&alloy_primitives::hex!(
                "08c379a0000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000047061757300000000000000000000000000000000000000000000000000000000"
            )),
            "revert: paus"
        );
    }

    #[test]
    fn the_keystore_unlocks_with_the_right_password_only() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = PrivateKeySigner::encrypt_keystore(
            dir.path(),
            &mut rand::thread_rng(),
            KEY,
            "hunter2",
            Some("key.json"),
        )
        .unwrap();
        let path = dir.path().join("key.json");

        assert_eq!(
            unlock(&path, "hunter2").unwrap().address(),
            address!("0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025")
        );
        assert!(matches!(
            unlock(&path, "hunter3"),
            Err(UnlockError::WrongPassword)
        ));

        let missing = dir.path().join("missing.json");
        let error = unlock(&missing, "hunter2").unwrap_err().to_string();
        assert!(
            error.starts_with(&format!("cannot read the keystore {}: ", missing.display())),
            "{error}"
        );
    }
}
