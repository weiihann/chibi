use std::collections::BTreeSet;
use std::path::Path;

use alloy_consensus::{SignableTransaction as _, TxEip4844, TxLegacy};
use alloy_eips::eip1559::BaseFeeParams;
use alloy_eips::eip2718::Encodable2718 as _;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address};
use alloy_signer::SignerSync as _;
use alloy_signer_local::PrivateKeySigner;
use chibi_core::exec::{CallRequest, ExecutionResult};
use chibi_core::state::{HEADER_WINDOW, Key};
use chibi_core::testing::{
    BASE_FEE, BlockWrites, EIP1967_IMPLEMENTATION_SLOT, GAS_PRICE, WorldAccount,
    balance_of_calldata, delegating_proxy_code, eth, legacy_transfer, mapping_reader_code,
    mapping_slot,
};
use chibi_core::verify::VerifyError;
use chibi_core::wire::Transaction;

use crate::disk::{Disk, read_state};
use crate::fixture::{Fault, Fixture, FixturePeers, LIAR};
use crate::network::Token;
use crate::node::{BlockSel, Follower, Node, NodeError, Stepped, read_snapshot};

const ALICE: Address = address!("0x00000000000000000000000000000000000a11ce");
const BOB: Address = address!("0x0000000000000000000000000000000000000b0b");
const PROXY: Address = address!("0x00000000000000000000000000000000000070c3");
const IMPL: Address = address!("0x0000000000000000000000000000000000001111");

const TOKENS: &[Token] = &[Token {
    symbol: "TKN",
    address: PROXY,
    decimals: 18,
}];

async fn start(
    fixture: &Fixture,
    dir: &Path,
    watch: &[Address],
    tokens: &'static [Token],
) -> (Node<FixturePeers>, Follower<FixturePeers>) {
    Node::with_parts(
        Fixture::spec(tokens),
        fixture.peers(),
        fixture.heads(),
        Disk::open(dir).unwrap(),
        watch,
    )
    .await
    .unwrap()
}

fn impl_slot() -> U256 {
    U256::from_be_bytes(EIP1967_IMPLEMENTATION_SLOT.0)
}

fn token_world(balance: u64) -> Fixture {
    Fixture::genesis([
        (ALICE, WorldAccount::eoa(eth(2))),
        (
            PROXY,
            WorldAccount::contract(delegating_proxy_code())
                .with_slot(impl_slot(), U256::from_be_slice(IMPL.as_slice()))
                .with_slot(mapping_slot(ALICE), U256::from(balance)),
        ),
        (IMPL, WorldAccount::contract(mapping_reader_code())),
    ])
}

async fn balance(node: &Node<FixturePeers>, address: Address) -> Option<U256> {
    node.account(address, BlockSel::Latest)
        .await
        .unwrap()
        .map(|account| account.balance)
}

async fn token_balance(node: &Node<FixturePeers>, holder: Address) -> U256 {
    let request = CallRequest {
        to: TxKind::Call(PROXY),
        input: balance_of_calldata(holder),
        ..CallRequest::default()
    };
    match node.call(request, BlockSel::Latest).await.unwrap() {
        ExecutionResult::Success { output, .. } => U256::from_be_slice(output.data()),
        other @ (ExecutionResult::Revert { .. } | ExecutionResult::Halt { .. }) => {
            panic!("balanceOf failed: {other:?}")
        }
    }
}

fn signed_transfer(signer: &PrivateKeySigner, chain_id: u64, nonce: u64) -> Transaction {
    let tx = TxLegacy {
        chain_id: Some(chain_id),
        nonce,
        gas_price: u128::from(GAS_PRICE),
        gas_limit: 21_000,
        to: TxKind::Call(BOB),
        value: eth(1),
        input: Bytes::new(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    Transaction::Legacy(tx.into_signed(signature))
}

#[tokio::test]
async fn start_anchors_with_the_watched_balance_and_serves_it_without_fetching() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;
    let proofs = fixture.log().proofs;

    assert_eq!(balance(&node, ALICE).await, Some(eth(2)));
    assert_eq!(fixture.log().proofs, proofs);
    assert_eq!(node.head().hash(), fixture.world(|w| w.head()));
    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    let holding = &node.status().snapshot.holdings[0];
    assert_eq!((holding.balance, holding.nonce), (Some(eth(2)), Some(0)));
}

#[tokio::test]
async fn a_block_with_a_bal_extends_without_reproving() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;
    let genesis = node.head();
    let proofs = fixture.log().proofs;

    let hash = fixture.mine(BlockWrites::default().balance(ALICE, eth(1)));
    let head = fixture.world(|w| w.verified(hash)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Extended(head));
    assert_eq!(balance(&node, ALICE).await, Some(eth(1)));
    assert_eq!(fixture.log().proofs, proofs);

    assert_eq!(
        node.header(BlockSel::Number(genesis.number())).unwrap(),
        genesis
    );
    assert!(matches!(
        node.account(ALICE, BlockSel::Number(genesis.number())).await,
        Err(NodeError::Unsupported { head: n, .. }) if n == head.number
    ));
    assert!(matches!(
        node.header(BlockSel::Number(head.number + 1)),
        Err(NodeError::Unsupported { .. })
    ));
}

#[tokio::test]
async fn a_block_without_a_bal_reanchors() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;

    let hash = fixture.mine_without_bal(BlockWrites::default().balance(ALICE, eth(5)));
    let head = fixture.world(|w| w.verified(hash)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Reanchored(head));
    assert_eq!(balance(&node, ALICE).await, Some(eth(5)));
}

#[tokio::test]
async fn a_withheld_bal_falls_back_to_reanchor() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;

    fixture.fault(Fault::WithholdBals);
    let hash = fixture.mine(BlockWrites::default().balance(ALICE, eth(4)));
    let head = fixture.world(|w| w.verified(hash)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Reanchored(head));
    assert_eq!(balance(&node, ALICE).await, Some(eth(4)));
}

#[tokio::test]
async fn short_gaps_extend_and_gaps_past_32_reanchor() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;

    for n in 1..=3 {
        fixture.mine_unseen(BlockWrites::default().balance(ALICE, eth(10 + n)));
    }
    let head = fixture.world(|w| w.verified(w.head())).num_hash();
    fixture.publish(head.hash);
    assert_eq!(follower.step().await.unwrap(), Stepped::Extended(head));
    assert_eq!(balance(&node, ALICE).await, Some(eth(13)));

    for n in 1..=33 {
        fixture.mine_unseen(BlockWrites::default().balance(ALICE, eth(100 + n)));
    }
    let head = fixture.world(|w| w.verified(w.head())).num_hash();
    fixture.publish(head.hash);
    assert_eq!(follower.step().await.unwrap(), Stepped::Reanchored(head));
    assert_eq!(balance(&node, ALICE).await, Some(eth(133)));
}

#[tokio::test]
async fn token_seeding_tracks_proxy_keys_and_follows_bal_writes() {
    let fixture = token_world(250);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], TOKENS).await;

    assert_eq!(
        node.tracked(),
        BTreeSet::from([
            Key::Account(ALICE),
            Key::Account(PROXY),
            Key::Account(IMPL),
            Key::Slot(PROXY, impl_slot()),
            Key::Slot(PROXY, mapping_slot(ALICE)),
        ])
    );
    assert_eq!(
        node.status().snapshot.holdings[0].tokens,
        vec![("TKN".to_owned(), Some(U256::from(250)))]
    );

    let proofs = fixture.log().proofs;
    assert_eq!(token_balance(&node, ALICE).await, U256::from(250));
    assert_eq!(fixture.log().proofs, proofs);

    fixture.mine(BlockWrites::default().slot(PROXY, mapping_slot(ALICE), U256::from(300)));
    assert!(matches!(
        follower.step().await.unwrap(),
        Stepped::Extended(_)
    ));
    assert_eq!(token_balance(&node, ALICE).await, U256::from(300));
    assert_eq!(fixture.log().proofs, proofs);
    assert_eq!(
        node.status().snapshot.holdings[0].tokens,
        vec![("TKN".to_owned(), Some(U256::from(300)))]
    );
}

#[tokio::test]
async fn a_forged_reply_is_rejected_its_sender_penalized_and_an_honest_retry_succeeds() {
    let fixture = Fixture::genesis([
        (ALICE, WorldAccount::eoa(eth(2))),
        (BOB, WorldAccount::eoa(eth(3))),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;

    fixture.fault(Fault::LieAboutBalance(BOB, eth(1_000)));
    assert_eq!(balance(&node, BOB).await, Some(eth(3)));
    assert_eq!(fixture.log().penalized, vec![LIAR]);
}

#[tokio::test]
async fn a_submitted_transaction_is_persisted_before_broadcast_and_gets_a_verified_receipt() {
    let signer = PrivateKeySigner::random();
    let sender = signer.address();
    let fixture = Fixture::genesis([(sender, WorldAccount::eoa(eth(3)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[sender], &[]).await;
    fixture.watch_outbox(dir.path().to_path_buf());

    let tx = signed_transfer(&signer, node.chain_id(), 0);
    let hash = node.submit(&tx.encoded_2718()).await.unwrap();
    assert_eq!(hash, *tx.tx_hash());
    let broadcasts = fixture.log().broadcasts;
    assert_eq!(broadcasts.len(), 1);
    assert!(broadcasts[0].persisted);
    assert_eq!(
        node.transaction_count(sender, BlockSel::Pending)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        node.transaction_count(sender, BlockSel::Latest)
            .await
            .unwrap(),
        0
    );
    assert_eq!(node.receipt(hash).unwrap(), None);
    assert_eq!(node.transaction(hash).unwrap().unwrap().included, None);

    let mined = fixture.mine(included(sender, &tx));
    let block = fixture.world(|w| w.verified(mined)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Extended(block));

    let receipt = node.receipt(hash).unwrap().unwrap();
    assert_eq!(receipt.receipt.block(), block);
    assert_eq!(receipt.receipt.index(), 0);
    assert!(receipt.receipt.success());
    assert_eq!(receipt.receipt.gas_used(), 21_000);
    assert_eq!(receipt.receipt.logs().len(), 1);
    assert_eq!((receipt.from, receipt.to), (sender, Some(BOB)));
    let submitted = node.transaction(hash).unwrap().unwrap();
    assert_eq!(submitted.included, Some((block, 0)));
    assert_eq!(submitted.transaction, tx);
    assert!(read_state(dir.path()).unwrap().unwrap().outbox().is_empty());
    assert_eq!(
        node.transaction_count(sender, BlockSel::Pending)
            .await
            .unwrap(),
        1
    );
}

fn included(sender: Address, tx: &Transaction) -> BlockWrites {
    let cost = U256::from(21_000 * GAS_PRICE);
    BlockWrites::default()
        .balance(sender, eth(2) - cost)
        .nonce(sender, 1)
        .balance(BOB, eth(1))
        .tx(tx.clone(), 21_000, 1)
}

#[tokio::test]
async fn a_failed_body_search_keeps_the_mined_transaction_and_finds_it_next_head() {
    let signer = PrivateKeySigner::random();
    let sender = signer.address();
    let fixture = Fixture::genesis([(sender, WorldAccount::eoa(eth(3)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[sender], &[]).await;
    let tx = signed_transfer(&signer, node.chain_id(), 0);
    let hash = node.submit(&tx.encoded_2718()).await.unwrap();

    fixture.fault(Fault::WithholdBodies);
    let mined = fixture.mine(included(sender, &tx));
    let block = fixture.world(|w| w.verified(mined)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Extended(block));
    assert_eq!(
        node.transaction_count(sender, BlockSel::Latest)
            .await
            .unwrap(),
        1
    );
    assert_eq!(node.transaction(hash).unwrap().unwrap().included, None);
    assert_eq!(read_state(dir.path()).unwrap().unwrap().outbox().len(), 1);

    fixture.heal();
    fixture.mine(BlockWrites::default());
    assert!(matches!(
        follower.step().await.unwrap(),
        Stepped::Extended(_)
    ));
    assert_eq!(
        node.transaction(hash).unwrap().unwrap().included,
        Some((block, 0))
    );
    assert_eq!(node.receipt(hash).unwrap().unwrap().receipt.block(), block);
}

#[tokio::test]
async fn a_transaction_mined_in_a_gap_block_gets_its_receipt_after_a_reanchor() {
    let signer = PrivateKeySigner::random();
    let sender = signer.address();
    let fixture = Fixture::genesis([(sender, WorldAccount::eoa(eth(3)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[sender], &[]).await;
    let tx = signed_transfer(&signer, node.chain_id(), 0);
    let hash = node.submit(&tx.encoded_2718()).await.unwrap();

    let mined = fixture.mine_unseen(included(sender, &tx));
    fixture.mine_unseen(BlockWrites::default());
    let head = fixture.mine_without_bal(BlockWrites::default());
    let (block, head) = fixture.world(|w| (w.verified(mined).num_hash(), w.verified(head)));
    assert_eq!(
        follower.step().await.unwrap(),
        Stepped::Reanchored(head.num_hash())
    );
    assert!(matches!(
        node.header(BlockSel::Number(block.number)),
        Err(NodeError::Unsupported { .. })
    ));
    let receipt = node.receipt(hash).unwrap().unwrap();
    assert_eq!(receipt.receipt.block(), block);
    assert_eq!(
        node.transaction(hash).unwrap().unwrap().included,
        Some((block, 0))
    );
}

fn mine_empty_unseen(fixture: &Fixture, blocks: usize) -> B256 {
    let mut head = B256::ZERO;
    for _ in 0..blocks {
        head = fixture.mine_unseen(BlockWrites::default());
    }
    head
}

#[tokio::test]
async fn a_restart_past_the_header_window_finds_the_transaction_mined_after_submission() {
    let signer = PrivateKeySigner::random();
    let sender = signer.address();
    let fixture = Fixture::genesis([(sender, WorldAccount::eoa(eth(3)))]);
    let dir = tempfile::tempdir().unwrap();
    let tx = signed_transfer(&signer, 1, 0);
    let hash = *tx.tx_hash();
    {
        let (node, _follower) = start(&fixture, dir.path(), &[sender], &[]).await;
        node.submit(&tx.encoded_2718()).await.unwrap();
    }
    let mined = fixture.mine_unseen(included(sender, &tx));
    let head = mine_empty_unseen(&fixture, HEADER_WINDOW + 44);
    fixture.publish(head);
    let block = fixture.world(|w| w.verified(mined)).num_hash();

    let (node, mut follower) = start(&fixture, dir.path(), &[sender], &[]).await;
    assert_eq!(node.head().hash(), head);
    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    assert_eq!(
        node.transaction(hash).unwrap().unwrap().included,
        Some((block, 0))
    );
    assert_eq!(node.receipt(hash).unwrap().unwrap().receipt.block(), block);
    assert!(read_state(dir.path()).unwrap().unwrap().outbox().is_empty());
}

#[tokio::test]
async fn a_step_past_the_header_window_finds_the_transaction_and_does_not_search_again() {
    let signer = PrivateKeySigner::random();
    let sender = signer.address();
    let fixture = Fixture::genesis([(sender, WorldAccount::eoa(eth(3)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[sender], &[]).await;
    let tx = signed_transfer(&signer, node.chain_id(), 0);
    let hash = node.submit(&tx.encoded_2718()).await.unwrap();

    let mined = fixture.mine_unseen(included(sender, &tx));
    let head = mine_empty_unseen(&fixture, HEADER_WINDOW + 44);
    fixture.publish(head);
    let block = fixture.world(|w| w.verified(mined)).num_hash();
    assert!(matches!(
        follower.step().await.unwrap(),
        Stepped::Reanchored(_)
    ));
    assert_eq!(
        node.transaction(hash).unwrap().unwrap().included,
        Some((block, 0))
    );
    assert_eq!(node.receipt(hash).unwrap().unwrap().receipt.block(), block);

    let next = signed_transfer(&signer, node.chain_id(), 1);
    node.submit(&next.encoded_2718()).await.unwrap();
    let searched = fixture.log().bodies;
    fixture.mine(BlockWrites::default());
    follower.step().await.unwrap();
    assert_eq!(fixture.log().bodies - searched, 1);
    assert_eq!(
        node.transaction(*next.tx_hash()).unwrap().unwrap().included,
        None
    );
}

#[tokio::test]
async fn a_transaction_whose_nonce_another_took_is_dropped_once_its_blocks_were_searched() {
    let signer = PrivateKeySigner::random();
    let sender = signer.address();
    let fixture = Fixture::genesis([(sender, WorldAccount::eoa(eth(3)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[sender], &[]).await;
    let ours = signed_transfer(&signer, node.chain_id(), 0);
    let hash = node.submit(&ours.encoded_2718()).await.unwrap();

    let theirs = legacy_transfer(0, BOB, eth(1));
    fixture.mine(
        BlockWrites::default()
            .nonce(sender, 1)
            .tx(theirs, 21_000, 0),
    );
    follower.step().await.unwrap();
    assert_eq!(node.transaction(hash).unwrap(), None);
    assert!(read_state(dir.path()).unwrap().unwrap().outbox().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_submission_whose_outbox_cannot_be_saved_is_neither_recorded_nor_sent() {
    use std::os::unix::fs::PermissionsExt as _;
    let signer = PrivateKeySigner::random();
    let sender = signer.address();
    let fixture = Fixture::genesis([(sender, WorldAccount::eoa(eth(3)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = start(&fixture, dir.path(), &[sender], &[]).await;
    fixture.watch_outbox(dir.path().to_path_buf());
    let tx = signed_transfer(&signer, node.chain_id(), 0);

    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let refused = node.submit(&tx.encoded_2718()).await;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(refused, Err(NodeError::Disk(_))));
    assert_eq!(node.transaction(*tx.tx_hash()).unwrap(), None);
    assert_eq!(
        node.transaction_count(sender, BlockSel::Pending)
            .await
            .unwrap(),
        0
    );
    assert!(fixture.log().broadcasts.is_empty());

    node.submit(&tx.encoded_2718()).await.unwrap();
    let broadcasts = fixture.log().broadcasts;
    assert_eq!(broadcasts.len(), 1);
    assert!(broadcasts[0].persisted);
}

#[tokio::test]
async fn submit_rejects_wrong_chain_ids_blob_transactions_and_garbage() {
    let signer = PrivateKeySigner::random();
    let fixture = Fixture::genesis([(signer.address(), WorldAccount::eoa(eth(3)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = start(&fixture, dir.path(), &[], &[]).await;

    let other_chain = signed_transfer(&signer, 11_155_111, 0);
    let blob = TxEip4844 {
        chain_id: node.chain_id(),
        blob_versioned_hashes: vec![B256::repeat_byte(1)],
        ..TxEip4844::default()
    };
    let signature = signer.sign_hash_sync(&blob.signature_hash()).unwrap();
    let blob = Transaction::Eip4844(blob.into_signed(signature));
    for raw in [
        other_chain.encoded_2718(),
        blob.encoded_2718(),
        vec![0x02, 0xc0],
    ] {
        assert!(matches!(
            node.submit(&raw).await,
            Err(NodeError::InvalidTransaction(_))
        ));
    }
    assert!(fixture.log().broadcasts.is_empty());
}

#[tokio::test]
async fn a_restart_shows_the_saved_snapshot_but_reproves_every_saved_key() {
    let fixture = token_world(250);
    let dir = tempfile::tempdir().unwrap();
    let tracked = {
        let (node, _follower) = start(&fixture, dir.path(), &[ALICE], TOKENS).await;
        assert!(matches!(Disk::open(dir.path()), Err(NodeError::Disk(_))));
        node.tracked()
    };

    let saved = read_snapshot(dir.path()).unwrap().unwrap();
    assert_eq!(saved.holdings[0].balance, Some(eth(2)));
    assert_eq!(
        saved.holdings[0].tokens,
        vec![("TKN".to_owned(), Some(U256::from(250)))]
    );

    fixture.mine_without_bal(BlockWrites::default().balance(ALICE, eth(7)).slot(
        PROXY,
        mapping_slot(ALICE),
        U256::from(999),
    ));
    let (node, _follower) = start(&fixture, dir.path(), &[], TOKENS).await;
    assert_eq!(node.tracked(), tracked);
    let proofs = fixture.log().proofs;
    assert_eq!(balance(&node, ALICE).await, Some(eth(7)));
    assert_eq!(
        node.storage(PROXY, mapping_slot(ALICE), BlockSel::Latest)
            .await
            .unwrap(),
        U256::from(999)
    );
    assert_eq!(fixture.log().proofs, proofs);
}

#[tokio::test]
async fn a_restart_rebroadcasts_the_saved_outbox() {
    let signer = PrivateKeySigner::random();
    let fixture = Fixture::genesis([(signer.address(), WorldAccount::eoa(eth(3)))]);
    let dir = tempfile::tempdir().unwrap();
    let tx = signed_transfer(&signer, 1, 0);
    {
        let (node, _follower) = start(&fixture, dir.path(), &[], &[]).await;
        node.submit(&tx.encoded_2718()).await.unwrap();
    }
    let (node, _follower) = start(&fixture, dir.path(), &[], &[]).await;
    let broadcasts = fixture.log().broadcasts;
    assert_eq!(broadcasts.len(), 2);
    assert_eq!(broadcasts[1].transaction, tx);
    assert_eq!(
        node.transaction(*tx.tx_hash()).unwrap().unwrap().included,
        None
    );
}

#[tokio::test]
async fn a_key_that_cannot_be_proven_is_an_error_never_zero() {
    let fixture = Fixture::genesis([
        (ALICE, WorldAccount::eoa(eth(2))),
        (
            BOB,
            WorldAccount::eoa(eth(3)).with_slot(U256::from(1), U256::from(5)),
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;

    fixture.fault(Fault::Unavailable(BOB));
    assert!(matches!(
        node.account(BOB, BlockSel::Latest).await,
        Err(NodeError::Unprovable(VerifyError::RootUnavailable { .. }))
    ));
    assert!(matches!(
        node.storage(BOB, U256::from(1), BlockSel::Latest).await,
        Err(NodeError::Unprovable(VerifyError::RootUnavailable { .. }))
    ));
    let request = CallRequest {
        from: Some(BOB),
        to: TxKind::Call(ALICE),
        ..CallRequest::default()
    };
    assert!(node.call(request, BlockSel::Latest).await.is_err());
    assert!(fixture.log().penalized.is_empty());

    fixture.heal();
    fixture.fault(Fault::DropProofNode(BOB));
    assert!(matches!(
        node.account(BOB, BlockSel::Latest).await,
        Err(NodeError::Unprovable(VerifyError::RangeProof { .. }))
    ));
    assert_eq!(fixture.log().penalized.len(), 3);

    fixture.heal();
    assert_eq!(balance(&node, BOB).await, Some(eth(3)));
}

#[tokio::test]
async fn fees_come_from_verified_bodies_and_held_headers() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;
    for nonce in 0..2 {
        fixture.mine(BlockWrites::default().tx(legacy_transfer(nonce, BOB, eth(1)), 21_000, 0));
        follower.step().await.unwrap();
    }

    let tip = u128::from(GAS_PRICE - BASE_FEE);
    assert_eq!(node.max_priority_fee().await.unwrap(), tip);
    let next_base_fee = node
        .head()
        .header()
        .next_block_base_fee(BaseFeeParams::ethereum())
        .unwrap();
    assert!(next_base_fee < BASE_FEE);
    assert_eq!(
        node.gas_price().await.unwrap(),
        u128::from(next_base_fee) + tip
    );

    let history = node
        .fee_history(2, BlockSel::Latest, &[50.0])
        .await
        .unwrap();
    assert_eq!(history.oldest_block, node.head().number() - 1);
    assert_eq!(history.base_fee_per_gas.len(), 3);
    assert_eq!(history.gas_used_ratio.len(), 2);
    assert_eq!(history.base_fee_per_blob_gas.len(), 3);
    assert_eq!(history.reward, Some(vec![vec![tip], vec![tip]]));
}
