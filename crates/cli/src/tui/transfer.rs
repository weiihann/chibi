//! Unlocking the keystore, and building, signing and submitting a transfer from it.

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
