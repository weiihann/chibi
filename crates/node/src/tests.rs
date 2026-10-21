use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use alloy_consensus::{SignableTransaction as _, TxEip4844, TxLegacy};
use alloy_eips::eip1559::BaseFeeParams;
use alloy_eips::eip2718::Encodable2718 as _;
use alloy_primitives::{Address, B256, Bytes, Log as EvmLog, TxKind, U256, address, keccak256};
use alloy_signer::SignerSync as _;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolValue as _;
use chibi_core::exec::{CallRequest, ExecutionResult};
use chibi_core::history::{At, Capability, LogRow, OpenState, Touch, TxRow};
use chibi_core::state::{HEADER_WINDOW, Key};
use chibi_core::testing::{
    BASE_FEE, BlockWrites, EIP1967_IMPLEMENTATION_SLOT, GAS_PRICE, Outcome, PAUSED_SLOT,
    WorldAccount, balance_of_calldata, delegating_proxy_code, eth, key_address, legacy_transfer,
    mapping_reader_code, mapping_slot, pausable_token_code, transfer_calldata,
};
use chibi_core::verify::VerifyError;
use chibi_core::wire::Transaction;

use crate::disk::{Disk, History, read_state};
use crate::fixture::{Fault, Fixture, FixturePeers, LIAR, Log};
use crate::network::{MAINNET_ENS_RESOLVER, Token};
use crate::node::{
    BlockSel, ContractSnapshot, Follower, Held, Node, NodeError, SEED_RETRY, Stepped,
    TrackedContract, Watchlist, read_snapshot,
};
use crate::peers::NetError;

const ALICE: Address = address!("0x00000000000000000000000000000000000a11ce");
const BOB: Address = address!("0x0000000000000000000000000000000000000b0b");
const PROXY: Address = address!("0x00000000000000000000000000000000000070c3");
const IMPL: Address = address!("0x0000000000000000000000000000000000001111");
const CAROL: Address = address!("0x00000000000000000000000000000000000ca201");
const PAUSABLE: Address = address!("0x0000000000000000000000000000000000009a05");
const PAUSABLE_IMPL: Address = address!("0x0000000000000000000000000000000000002222");
const NEW_IMPL: Address = address!("0x0000000000000000000000000000000000003333");

const TKN: Token = Token {
    symbol: "TKN",
    name: "Test token",
    address: PROXY,
    decimals: 18,
};

const PSE: Token = Token {
    symbol: "PSE",
    name: "Pausable test token",
    address: PAUSABLE,
    decimals: 6,
};

const TOKENS: &[Token] = &[TKN];

const BOTH: &[Token] = &[TKN, PSE];

/// Starts on a chain whose registry is `registry` and takes the first follower step, which
/// seeds and warms.
async fn start(
    fixture: &Fixture,
    dir: &Path,
    watch: &[Address],
    registry: &'static [Token],
) -> (Node<FixturePeers>, Follower<FixturePeers>) {
    start_with(fixture, dir, registry, &watchlist(watch)).await
}

fn watchlist(addresses: &[Address]) -> Watchlist {
    Watchlist {
        addresses: addresses.to_vec(),
        contracts: Vec::new(),
        history: false,
    }
}

/// Starts without stepping: nothing is proven yet.
async fn open(
    fixture: &Fixture,
    dir: &Path,
    registry: &'static [Token],
    watch: &Watchlist,
) -> (Node<FixturePeers>, Follower<FixturePeers>) {
    Node::with_parts(
        Fixture::spec(registry),
        fixture.peers(),
        fixture.heads(),
        Disk::open(dir).unwrap(),
        watch,
    )
    .await
    .unwrap()
}

async fn start_with(
    fixture: &Fixture,
    dir: &Path,
    registry: &'static [Token],
    watch: &Watchlist,
) -> (Node<FixturePeers>, Follower<FixturePeers>) {
    let (node, mut follower) = open(fixture, dir, registry, watch).await;
    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    (node, follower)
}

fn impl_slot() -> U256 {
    U256::from_be_bytes(EIP1967_IMPLEMENTATION_SLOT.0)
}

fn token_world(balance: u64) -> Fixture {
    Fixture::genesis(token_accounts(balance))
}

fn token_accounts(balance: u64) -> Vec<(Address, WorldAccount)> {
    vec![
        (ALICE, WorldAccount::eoa(eth(2))),
        (
            PROXY,
            WorldAccount::contract(delegating_proxy_code())
                .with_slot(impl_slot(), U256::from_be_slice(IMPL.as_slice()))
                .with_slot(mapping_slot(ALICE), U256::from(balance)),
        ),
        (IMPL, WorldAccount::contract(mapping_reader_code())),
    ]
}

/// [`token_world`] plus a proxied token whose transfer reads a pause flag, holding
/// `pausable` for ALICE.
fn two_token_world(balance: u64, pausable: u64) -> Fixture {
    let mut accounts = token_accounts(balance);
    accounts.extend([
        (
            PAUSABLE,
            WorldAccount::contract(delegating_proxy_code())
                .with_slot(impl_slot(), U256::from_be_slice(PAUSABLE_IMPL.as_slice()))
                .with_slot(mapping_slot(ALICE), U256::from(pausable)),
        ),
        (PAUSABLE_IMPL, WorldAccount::contract(pausable_token_code())),
    ]);
    Fixture::genesis(accounts)
}

fn pausable_keys() -> BTreeSet<Key> {
    BTreeSet::from([
        Key::Account(PAUSABLE),
        Key::Account(PAUSABLE_IMPL),
        Key::Slot(PAUSABLE, impl_slot()),
        Key::Slot(PAUSABLE, PAUSED_SLOT),
        Key::Slot(PAUSABLE, mapping_slot(ALICE)),
    ])
}

fn working_set_has_key_of(node: &Node<FixturePeers>, address: Address) -> bool {
    let (pins, learned) = node.working();
    pins.iter().chain(&learned).any(|key| match key {
        Key::Account(at) | Key::Slot(at, _) => *at == address,
    })
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

async fn anchor(fixture: &Fixture, dir: &Path) -> Result<Node<FixturePeers>, NodeError> {
    Node::with_parts(
        Fixture::spec(&[]),
        fixture.peers(),
        fixture.heads(),
        Disk::open(dir).unwrap(),
        &watchlist(&[ALICE]),
    )
    .await
    .map(|(node, _)| node)
}

#[tokio::test(start_paused = true)]
async fn anchoring_retries_rejected_header_fetches_until_one_succeeds() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    fixture.fault(Fault::RejectHeaders(5));

    let node = anchor(&fixture, dir.path()).await.unwrap();

    assert_eq!(node.head().hash(), fixture.world(|w| w.head()));
}

#[tokio::test(start_paused = true)]
async fn anchoring_gives_up_when_the_network_stops() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    fixture.fault(Fault::StopHeaders);

    assert!(matches!(
        anchor(&fixture, dir.path()).await,
        Err(NodeError::Net(NetError::Stopped))
    ));
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
    assert_eq!(
        balance_at(&node, ALICE, BlockSel::Number(genesis.number())).await,
        Some(eth(2))
    );
    assert!(matches!(
        node.header(BlockSel::Number(head.number + 1)),
        Err(NodeError::Unsupported { .. })
    ));
}

async fn balance_at(node: &Node<FixturePeers>, address: Address, block: BlockSel) -> Option<U256> {
    match node.account(address, block).await {
        Ok(account) => account.map(|account| account.balance),
        Err(err) => panic!("account at {block:?}: {err}"),
    }
}

/// The head an `Unsupported` refusal names, or the value served instead.
async fn refused_at(
    node: &Node<FixturePeers>,
    address: Address,
    block: BlockSel,
) -> Result<u64, String> {
    match node.account(address, block).await {
        Err(NodeError::Unsupported { head, .. }) => Ok(head),
        other => Err(format!("{other:?}")),
    }
}

#[tokio::test]
async fn the_block_before_the_head_serves_its_own_state_by_number_and_hash() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;
    let parent = node.head();

    let hash = fixture.mine(BlockWrites::default().balance(ALICE, eth(1)));
    let head = fixture.world(|w| w.verified(hash)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Extended(head));
    let proofs = fixture.log().proofs;

    for (block, expected) in [
        (BlockSel::Number(parent.number()), eth(2)),
        (BlockSel::Hash(parent.hash()), eth(2)),
        (BlockSel::Number(head.number), eth(1)),
        (BlockSel::Hash(head.hash), eth(1)),
    ] {
        assert_eq!(
            balance_at(&node, ALICE, block).await,
            Some(expected),
            "{block:?}"
        );
    }
    assert_eq!(fixture.log().proofs, proofs);
}

#[tokio::test]
async fn blocks_skipped_inside_one_extend_serve_their_state() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;
    let genesis = node.head().number();

    for n in 1..=3 {
        fixture.mine_unseen(BlockWrites::default().balance(ALICE, eth(10 + n)));
    }
    let head = fixture.world(|w| w.verified(w.head())).num_hash();
    fixture.publish(head.hash);
    assert_eq!(follower.step().await.unwrap(), Stepped::Extended(head));

    for (number, expected) in [
        (genesis, eth(2)),
        (genesis + 1, eth(11)),
        (genesis + 2, eth(12)),
        (genesis + 3, eth(13)),
    ] {
        assert_eq!(
            balance_at(&node, ALICE, BlockSel::Number(number)).await,
            Some(expected),
            "block {number}"
        );
    }
}

#[tokio::test]
async fn a_call_at_the_previous_block_proves_its_misses_there_and_keeps_them_out_of_the_head() {
    let fixture = token_world(250);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;
    let parent = node.head().number();
    let slot = Key::Slot(PROXY, mapping_slot(ALICE));
    assert!(!node.holds(slot));

    let hash =
        fixture.mine(BlockWrites::default().slot(PROXY, mapping_slot(ALICE), U256::from(300)));
    let head = fixture.world(|w| w.verified(hash)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Extended(head));
    let proofs = fixture.log().proofs;

    let request = CallRequest {
        to: TxKind::Call(PROXY),
        input: balance_of_calldata(ALICE),
        ..CallRequest::default()
    };
    match node.call(request, BlockSel::Number(parent)).await.unwrap() {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(U256::from_be_slice(output.data()), U256::from(250));
        }
        other @ (ExecutionResult::Revert { .. } | ExecutionResult::Halt { .. }) => {
            panic!("balanceOf at the parent failed: {other:?}")
        }
    }
    assert!(fixture.log().proofs > proofs);
    assert!(!node.holds(slot));
    assert_eq!(token_balance(&node, ALICE).await, U256::from(300));
}

#[tokio::test]
async fn state_older_than_the_retained_blocks_is_unsupported() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;
    let genesis = node.head().number();

    for n in 1..=9 {
        let hash = fixture.mine(BlockWrites::default().balance(ALICE, eth(10 + n)));
        let head = fixture.world(|w| w.verified(hash)).num_hash();
        assert_eq!(follower.step().await.unwrap(), Stepped::Extended(head));
    }

    assert_eq!(
        balance_at(&node, ALICE, BlockSel::Number(genesis + 1)).await,
        Some(eth(11))
    );
    assert_eq!(
        refused_at(&node, ALICE, BlockSel::Number(genesis)).await,
        Ok(genesis + 9)
    );
}

#[tokio::test]
async fn a_fork_switch_drops_the_retained_states() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;
    let genesis = node.head();

    let abandoned = fixture.mine(BlockWrites::default().balance(ALICE, eth(1)));
    let head = fixture.world(|w| w.verified(abandoned)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Extended(head));
    assert_eq!(
        balance_at(&node, ALICE, BlockSel::Hash(genesis.hash())).await,
        Some(eth(2))
    );

    let sibling = fixture.fork_head(BlockWrites::default().balance(ALICE, eth(5)));
    let head = fixture.world(|w| w.verified(sibling)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Reanchored(head));

    assert_eq!(
        refused_at(&node, ALICE, BlockSel::Hash(abandoned)).await,
        Ok(head.number)
    );
    assert_eq!(
        refused_at(&node, ALICE, BlockSel::Number(genesis.number())).await,
        Ok(head.number)
    );
    assert_eq!(
        balance_at(&node, ALICE, BlockSel::Number(head.number)).await,
        Some(eth(5))
    );
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
async fn token_seeding_pins_proxy_keys_and_follows_bal_writes() {
    let fixture = token_world(250);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], TOKENS).await;

    assert_eq!(
        node.working().0,
        BTreeSet::from([
            Key::Account(ALICE),
            Key::Account(MAINNET_ENS_RESOLVER),
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
async fn a_reseed_that_cannot_complete_keeps_seeded_pins_and_retries() {
    let mut accounts = token_accounts(250);
    accounts.push((NEW_IMPL, WorldAccount::contract(mapping_reader_code())));
    let fixture = Fixture::genesis(accounts);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], TOKENS).await;
    let seeded = BTreeSet::from([
        Key::Account(ALICE),
        Key::Account(MAINNET_ENS_RESOLVER),
        Key::Account(PROXY),
        Key::Account(IMPL),
        Key::Slot(PROXY, impl_slot()),
        Key::Slot(PROXY, mapping_slot(ALICE)),
    ]);
    assert_eq!(node.working().0, seeded);

    fixture.fault(Fault::Unavailable(NEW_IMPL));
    let upgrade = U256::from_be_slice(NEW_IMPL.as_slice());
    let hash = fixture.mine_without_bal(BlockWrites::default().slot(PROXY, impl_slot(), upgrade));
    let head = fixture.world(|w| w.verified(hash)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Reanchored(head));
    let (pins, learned) = node.working();
    assert_eq!(pins, seeded);
    assert!(learned.is_disjoint(&seeded), "no seeded pin is demoted");

    fixture.heal();
    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    let (pins, learned) = node.working();
    assert_eq!(
        pins,
        BTreeSet::from([
            Key::Account(ALICE),
            Key::Account(MAINNET_ENS_RESOLVER),
            Key::Account(PROXY),
            Key::Account(NEW_IMPL),
            Key::Slot(PROXY, impl_slot()),
            Key::Slot(PROXY, mapping_slot(ALICE)),
        ])
    );
    assert!(learned.contains(&Key::Account(IMPL)));

    let proofs = fixture.log().proofs;
    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    assert_eq!(fixture.log().proofs, proofs, "seeding is no longer due");
}

#[tokio::test]
async fn a_configured_contract_slot_is_pinned_and_follows_bal_writes() {
    let (set, unset) = (U256::from(7), U256::from(8));
    let fixture = Fixture::genesis([
        (ALICE, WorldAccount::eoa(eth(2))),
        (
            IMPL,
            WorldAccount::contract(mapping_reader_code()).with_slot(set, U256::from(42)),
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let watch = Watchlist {
        addresses: vec![ALICE],
        contracts: vec![TrackedContract {
            address: IMPL,
            slots: vec![set, unset],
        }],
        history: false,
    };
    let (node, mut follower) = start_with(&fixture, dir.path(), &[], &watch).await;
    assert_eq!(
        node.working().0,
        BTreeSet::from([
            Key::Account(ALICE),
            Key::Account(MAINNET_ENS_RESOLVER),
            Key::Account(IMPL),
            Key::Slot(IMPL, set),
            Key::Slot(IMPL, unset),
        ])
    );

    let proofs = fixture.log().proofs;
    let storage = |slot| node.storage(IMPL, slot, BlockSel::Latest);
    assert_eq!(storage(set).await.unwrap(), U256::from(42));
    assert_eq!(storage(unset).await.unwrap(), U256::ZERO);
    assert_eq!(fixture.log().proofs, proofs);

    fixture.mine(BlockWrites::default().slot(IMPL, set, U256::from(99)));
    assert!(matches!(
        follower.step().await.unwrap(),
        Stepped::Extended(_)
    ));
    assert_eq!(storage(set).await.unwrap(), U256::from(99));
    assert_eq!(fixture.log().proofs, proofs);

    let expected = vec![ContractSnapshot {
        address: IMPL,
        slots: vec![(set, Some(U256::from(99))), (unset, Some(U256::ZERO))],
    }];
    assert_eq!(node.status().snapshot.contracts, expected);
    assert_eq!(
        read_snapshot(dir.path()).unwrap().unwrap().contracts,
        expected
    );
}

#[tokio::test]
async fn seeding_pins_the_pause_flag_so_a_transfer_fetches_only_the_recipient() {
    let fixture = two_token_world(250, 40);
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = start(&fixture, dir.path(), &[ALICE], &[PSE]).await;

    let seeded = node.working().0;
    assert!(seeded.contains(&Key::Slot(PAUSABLE, PAUSED_SLOT)));
    assert!(seeded.contains(&Key::Slot(PAUSABLE, mapping_slot(ALICE))));
    assert_eq!(
        node.status().snapshot.holdings[0].tokens,
        vec![("PSE".to_owned(), Some(U256::from(40)))]
    );

    let before = fixture.log();
    let transfer = CallRequest {
        from: Some(ALICE),
        to: TxKind::Call(PAUSABLE),
        input: transfer_calldata(CAROL, U256::from(1)),
        ..CallRequest::default()
    };
    match node.call(transfer, BlockSel::Latest).await.unwrap() {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(U256::from_be_slice(output.data()), U256::from(1));
        }
        other @ (ExecutionResult::Revert { .. } | ExecutionResult::Halt { .. }) => {
            panic!("transfer failed: {other:?}")
        }
    }
    let after = fixture.log();
    assert_eq!(
        after.slots[before.slots.len()..],
        [(
            keccak256(PAUSABLE),
            keccak256(B256::from(mapping_slot(CAROL)))
        )],
        "only the recipient's balance slot is fetched"
    );
    assert_eq!(
        after.proofs - before.proofs,
        2,
        "one slot proof: the account range it is proven under and its storage range"
    );
    let (pins, learned) = node.working();
    let recipient = Key::Slot(PAUSABLE, mapping_slot(CAROL));
    assert_eq!(pins, seeded);
    assert!(
        learned.contains(&recipient),
        "the recipient's slot is learned"
    );
}

/// Seeds ALICE against `registry` with every proof taking 100 ms, returning the time it took
/// and the pins it left.
async fn timed_seed(registry: &'static [Token]) -> (Duration, BTreeSet<Key>) {
    let fixture = two_token_world(250, 40);
    fixture.serve_proofs(16, Duration::from_millis(100));
    let dir = tempfile::tempdir().unwrap();
    let (node, follower) = open(&fixture, dir.path(), registry, &watchlist(&[ALICE])).await;
    let started = tokio::time::Instant::now();
    assert!(follower.shared.seed().await);
    (started.elapsed(), node.working().0)
}

#[tokio::test(start_paused = true)]
async fn seeding_probes_every_token_at_once() {
    let (slowest, _) = timed_seed(&[PSE]).await;
    assert_eq!(slowest, Duration::from_millis(1000));

    let (both, pins) = timed_seed(BOTH).await;
    assert_eq!(both, slowest, "the faster token is probed alongside");
    let mut expected = pausable_keys();
    expected.extend([
        Key::Account(ALICE),
        Key::Account(MAINNET_ENS_RESOLVER),
        Key::Account(PROXY),
        Key::Account(IMPL),
        Key::Slot(PROXY, impl_slot()),
        Key::Slot(PROXY, mapping_slot(ALICE)),
    ]);
    assert_eq!(pins, expected);
}

/// Waits, in paused time, until the pins hold every key in `keys`.
async fn pinned(node: &Node<FixturePeers>, keys: &BTreeSet<Key>) -> bool {
    tokio::time::timeout(Duration::from_secs(60), async {
        while !node.working().0.is_superset(keys) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test(start_paused = true)]
async fn a_seed_that_cannot_complete_is_retried_on_a_wake_without_a_new_head() {
    let fixture = two_token_world(250, 40);
    fixture.fault(Fault::Unavailable(PAUSABLE_IMPL));
    let dir = tempfile::tempdir().unwrap();
    let (node, follower) = open(&fixture, dir.path(), BOTH, &watchlist(&[ALICE])).await;
    let head = node.head().num_hash();
    let shared = Arc::clone(&follower.shared);
    let running = tokio::spawn(follower.run());
    let first = BTreeSet::from([Key::Slot(PROXY, mapping_slot(ALICE))]);
    assert!(pinned(&node, &first).await, "the first seed ran");
    assert!(node.working().0.is_disjoint(&pausable_keys()));

    fixture.heal();
    tokio::time::sleep(SEED_RETRY).await;
    shared.wake.notify_one();
    assert!(
        pinned(&node, &pausable_keys()).await,
        "the wake reseeds the pausable token"
    );
    assert_eq!(node.head().num_hash(), head, "no block was mined");
    running.abort();
}

#[tokio::test(start_paused = true)]
async fn the_node_is_seeded_once_a_seed_completes() {
    let fixture = two_token_world(250, 40);
    fixture.fault(Fault::Unavailable(PAUSABLE_IMPL));
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = open(&fixture, dir.path(), BOTH, &watchlist(&[ALICE])).await;
    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    assert!(!node.health().seeded, "the first seed could not complete");

    fixture.heal();
    let shared = Arc::clone(&follower.shared);
    let running = tokio::spawn(follower.run());
    tokio::time::sleep(SEED_RETRY).await;
    shared.wake.notify_one();
    let seeded = tokio::time::timeout(Duration::from_secs(60), async {
        while !node.health().seeded {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(seeded.is_ok(), "the reseed completed");
    running.abort();
}

#[tokio::test]
async fn a_normal_start_is_seeded_after_the_first_step() {
    let fixture = two_token_world(250, 40);
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = start(&fixture, dir.path(), &[ALICE], BOTH).await;
    assert!(node.health().seeded);
}

#[tokio::test]
async fn every_registry_token_is_pinned_for_every_watched_address() {
    let fixture = two_token_world(250, 40);
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = start(&fixture, dir.path(), &[ALICE, CAROL], BOTH).await;
    assert_eq!(
        node.working().0,
        BTreeSet::from([
            Key::Account(ALICE),
            Key::Account(CAROL),
            Key::Account(MAINNET_ENS_RESOLVER),
            Key::Account(PROXY),
            Key::Account(IMPL),
            Key::Slot(PROXY, impl_slot()),
            Key::Slot(PROXY, mapping_slot(ALICE)),
            Key::Slot(PROXY, mapping_slot(CAROL)),
            Key::Account(PAUSABLE),
            Key::Account(PAUSABLE_IMPL),
            Key::Slot(PAUSABLE, impl_slot()),
            Key::Slot(PAUSABLE, PAUSED_SLOT),
            Key::Slot(PAUSABLE, mapping_slot(ALICE)),
            Key::Slot(PAUSABLE, mapping_slot(CAROL)),
        ])
    );

    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = start(&fixture, dir.path(), &[], BOTH).await;
    assert_eq!(node.working().0, BTreeSet::new());
}

#[tokio::test]
async fn a_restart_whose_registry_lacks_a_token_stops_pinning_it() {
    let fixture = two_token_world(250, 40);
    let dir = tempfile::tempdir().unwrap();
    {
        let (node, _follower) = start(&fixture, dir.path(), &[ALICE], BOTH).await;
        assert!(node.working().0.is_superset(&pausable_keys()));
    }

    fixture.mine_without_bal(BlockWrites::default().slot(
        PROXY,
        mapping_slot(ALICE),
        U256::from(999),
    ));
    let (node, _follower) = start(&fixture, dir.path(), &[ALICE], TOKENS).await;
    assert!(!working_set_has_key_of(&node, PAUSABLE));
    assert!(!working_set_has_key_of(&node, PAUSABLE_IMPL));
    assert_eq!(
        node.status().snapshot.holdings[0].tokens,
        vec![("TKN".to_owned(), Some(U256::from(999)))]
    );
    let proofs = fixture.log().proofs;
    assert_eq!(token_balance(&node, ALICE).await, U256::from(999));
    assert_eq!(fixture.log().proofs, proofs);
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

#[tokio::test]
async fn a_submission_whose_outbox_cannot_be_saved_is_neither_recorded_nor_sent() {
    let signer = PrivateKeySigner::random();
    let sender = signer.address();
    let fixture = Fixture::genesis([(sender, WorldAccount::eoa(eth(3)))]);
    let dir = tempfile::tempdir().unwrap();
    let (node, follower) = start(&fixture, dir.path(), &[sender], &[]).await;
    fixture.watch_outbox(dir.path().to_path_buf());
    let tx = signed_transfer(&signer, node.chain_id(), 0);

    let blocker = follower.shared.disk.block_writes();
    let refused = node.submit(&tx.encoded_2718()).await;
    drop(blocker);
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
async fn a_restart_shows_the_saved_snapshot_and_rederives_the_pinned_keys() {
    let fixture = token_world(250);
    let dir = tempfile::tempdir().unwrap();
    let working = {
        let (node, _follower) = start(&fixture, dir.path(), &[ALICE], TOKENS).await;
        assert!(matches!(Disk::open(dir.path()), Err(NodeError::Disk(_))));
        node.working()
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
    let (node, _follower) = start(&fixture, dir.path(), &[ALICE], TOKENS).await;
    assert_eq!(node.working(), working);
    let proofs = fixture.log().proofs;
    assert_eq!(balance(&node, ALICE).await, Some(eth(7)));
    assert_eq!(token_balance(&node, ALICE).await, U256::from(999));
    assert_eq!(fixture.log().proofs, proofs);
    let saved = read_snapshot(dir.path()).unwrap().unwrap();
    assert_eq!(saved.holdings[0].balance, Some(eth(7)));
    assert_eq!(
        saved.holdings[0].tokens,
        vec![("TKN".to_owned(), Some(U256::from(999)))]
    );
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

fn alice_and_bob() -> Fixture {
    Fixture::genesis([
        (ALICE, WorldAccount::eoa(eth(2))),
        (BOB, WorldAccount::eoa(eth(3))),
    ])
}

#[tokio::test]
async fn start_serves_reads_before_the_first_step_warms_the_pins() {
    let fixture = alice_and_bob();
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = open(&fixture, dir.path(), &[], &watchlist(&[ALICE])).await;
    assert_eq!(fixture.log().proofs, 0);
    assert!(!node.holds(Key::Account(ALICE)));
    assert_eq!(node.status().snapshot.holdings[0].balance, None);

    assert_eq!(balance(&node, BOB).await, Some(eth(3)));
    assert_eq!(fixture.log().proofs, 1);
    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    assert!(node.holds(Key::Account(ALICE)));
    assert!(node.holds(Key::Account(BOB)));
    assert_eq!(fixture.log().proofs, 3);
    assert_eq!(balance(&node, ALICE).await, Some(eth(2)));
    assert_eq!(balance(&node, BOB).await, Some(eth(3)));
    assert_eq!(fixture.log().proofs, 3);
}

fn hundred_holders() -> Vec<(Address, WorldAccount)> {
    (1..=100u64)
        .map(|i| {
            let address = Address::left_padding_from(&(0x1000 + i).to_be_bytes());
            (address, WorldAccount::eoa(eth(i)))
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn reads_queued_behind_one_busy_snap_peer_are_proven_once_and_never_time_out() {
    let holders = hundred_holders();
    let fixture = Fixture::genesis(holders.clone());
    fixture.serve_proofs(1, Duration::from_millis(340));
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = open(&fixture, dir.path(), &[], &watchlist(&[])).await;

    let balances =
        futures::future::join_all(holders.iter().map(|(address, _)| balance(&node, *address)))
            .await;
    assert_eq!(
        balances,
        (1..=100).map(|i| Some(eth(i))).collect::<Vec<_>>()
    );
    assert_eq!(fixture.log().proofs, 100);
}

#[tokio::test(start_paused = true)]
async fn reads_pipeline_sixteen_deep_to_one_snap_peer_and_never_exceed_that() {
    let holders = hundred_holders();
    let fixture = Fixture::genesis(holders.clone());
    fixture.serve_proofs(16, Duration::from_millis(200));
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = open(&fixture, dir.path(), &[], &watchlist(&[])).await;

    let started = tokio::time::Instant::now();
    let balances =
        futures::future::join_all(holders.iter().map(|(address, _)| balance(&node, *address)))
            .await;
    assert_eq!(started.elapsed(), Duration::from_millis(1400));
    assert_eq!(
        balances,
        (1..=100).map(|i| Some(eth(i))).collect::<Vec<_>>()
    );
    let log = fixture.log();
    assert_eq!(log.proofs, 100);
    assert_eq!(log.most_in_flight, 16);
}

const READER: Address = address!("0x0000000000000000000000000000000000005ead");
const NOBODY: Address = address!("0x000000000000000000000000000000000000dead");

/// Code that loads slots `0..count` of its own storage and stops.
fn slots_reader_code(count: u8) -> Bytes {
    let mut code: Vec<u8> = (0..count)
        .flat_map(|slot| [0x60, slot, 0x54, 0x50]) // pop(sload(slot))
        .collect();
    code.push(0x00);
    Bytes::from(code)
}

/// ALICE and a contract reading its six slots, two of them set.
fn reader_world() -> Fixture {
    Fixture::genesis([
        (ALICE, WorldAccount::eoa(eth(2))),
        (
            READER,
            WorldAccount::contract(slots_reader_code(6))
                .with_slot(U256::from(1), U256::from(11))
                .with_slot(U256::from(4), U256::from(44)),
        ),
    ])
}

fn reader_slots() -> Watchlist {
    Watchlist {
        addresses: Vec::new(),
        contracts: vec![TrackedContract {
            address: READER,
            slots: (0..6u64).map(U256::from).collect(),
        }],
        history: false,
    }
}

/// Account and storage requests for `address` served between `before` and `after`.
fn requests(before: &Log, after: &Log, address: Address) -> (usize, usize) {
    let key = keccak256(address);
    (
        after.accounts[before.accounts.len()..]
            .iter()
            .filter(|account| **account == key)
            .count(),
        after.slots[before.slots.len()..]
            .iter()
            .filter(|(account, _)| *account == key)
            .count(),
    )
}

#[tokio::test]
async fn a_call_reading_six_slots_of_one_contract_proves_its_account_once() {
    let fixture = reader_world();
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = open(&fixture, dir.path(), &[], &watchlist(&[])).await;
    assert_eq!(balance(&node, ALICE).await, Some(eth(2)));
    assert_eq!(
        node.code(READER, BlockSel::Latest).await.unwrap(),
        slots_reader_code(6)
    );

    let before = fixture.log();
    let call = CallRequest {
        from: Some(ALICE),
        to: TxKind::Call(READER),
        ..CallRequest::default()
    };
    assert!(matches!(
        node.call(call, BlockSel::Latest).await.unwrap(),
        ExecutionResult::Success { .. }
    ));
    assert_eq!(requests(&before, &fixture.log(), READER), (1, 6));
}

#[tokio::test]
async fn slots_of_an_absent_account_or_of_empty_storage_need_no_storage_requests() {
    let fixture = reader_world();
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = open(&fixture, dir.path(), &[], &watchlist(&[])).await;

    let before = fixture.log();
    let storage = |address| node.storage(address, U256::from(1), BlockSel::Latest);
    assert_eq!(storage(NOBODY).await.unwrap(), U256::ZERO);
    assert_eq!(storage(ALICE).await.unwrap(), U256::ZERO);
    let after = fixture.log();
    assert_eq!(requests(&before, &after, NOBODY), (1, 0));
    assert_eq!(requests(&before, &after, ALICE), (1, 0));
}

#[tokio::test]
async fn warming_and_reanchoring_prove_a_contract_account_once_for_all_its_pinned_slots() {
    let fixture = reader_world();
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start_with(&fixture, dir.path(), &[], &reader_slots()).await;
    assert_eq!(requests(&Log::default(), &fixture.log(), READER), (1, 6));
    assert_eq!(
        node.storage(READER, U256::from(4), BlockSel::Latest)
            .await
            .unwrap(),
        U256::from(44)
    );

    let before = fixture.log();
    let hash = fixture.mine_without_bal(BlockWrites::default().balance(ALICE, eth(5)));
    let head = fixture.world(|w| w.verified(hash)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Reanchored(head));
    assert_eq!(requests(&before, &fixture.log(), READER), (1, 6));
}

#[tokio::test]
async fn a_slot_no_peer_serves_leaves_the_other_slots_of_its_account_warm() {
    let fixture = reader_world();
    fixture.fault(Fault::WithholdSlot(READER, U256::from(1)));
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = start_with(&fixture, dir.path(), &[], &reader_slots()).await;

    assert!(node.holds(Key::Account(READER)));
    for slot in [0u64, 2, 3, 4, 5] {
        assert!(node.holds(Key::Slot(READER, U256::from(slot))), "{slot}");
    }
    assert!(!node.holds(Key::Slot(READER, U256::from(1))));
}

#[tokio::test]
async fn a_read_joins_the_working_set_and_bals_keep_it_current() {
    let fixture = alice_and_bob();
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;

    assert_eq!(balance(&node, BOB).await, Some(eth(3)));
    assert_eq!(node.working().1, BTreeSet::from([Key::Account(BOB)]));
    let proofs = fixture.log().proofs;
    fixture.mine(BlockWrites::default().balance(BOB, eth(4)));
    assert!(matches!(
        follower.step().await.unwrap(),
        Stepped::Extended(_)
    ));
    assert_eq!(balance(&node, BOB).await, Some(eth(4)));
    assert_eq!(fixture.log().proofs, proofs);
}

#[tokio::test(start_paused = true)]
async fn a_call_cut_by_a_head_move_leaves_the_next_call_warm() {
    let fixture = token_world(250);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;

    fixture.hold_proofs();
    let call = tokio::spawn({
        let node = node.clone();
        async move { token_balance(&node, ALICE).await }
    });
    fixture.proof_held().await;
    fixture.mine(BlockWrites::default().slot(PROXY, mapping_slot(ALICE), U256::from(300)));
    assert!(matches!(
        follower.step().await.unwrap(),
        Stepped::Extended(_)
    ));
    fixture.release_proofs();
    assert_eq!(call.await.unwrap(), U256::from(250));

    let read = [
        Key::Account(PROXY),
        Key::Account(IMPL),
        Key::Slot(PROXY, impl_slot()),
        Key::Slot(PROXY, mapping_slot(ALICE)),
    ];
    assert!(node.working().1.is_superset(&BTreeSet::from(read)));
    for key in read {
        assert!(!node.holds(key), "{key:?}");
    }
    follower.warm().await;
    let proofs = fixture.log().proofs;
    assert_eq!(token_balance(&node, ALICE).await, U256::from(300));
    assert_eq!(fixture.log().proofs, proofs);
}

#[tokio::test]
async fn a_reanchor_reproves_pins_then_warms_learned_keys() {
    let fixture = alice_and_bob();
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;
    assert_eq!(balance(&node, BOB).await, Some(eth(3)));

    let hash = fixture.mine_without_bal(BlockWrites::default().balance(BOB, eth(5)));
    let head = fixture.world(|w| w.verified(hash)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Reanchored(head));
    let proofs = fixture.log().proofs;
    assert_eq!(balance(&node, BOB).await, Some(eth(5)));
    assert_eq!(fixture.log().proofs, proofs);
}

#[tokio::test]
async fn a_learned_key_that_cannot_be_proven_never_blocks_the_head() {
    let fixture = alice_and_bob();
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;
    assert_eq!(balance(&node, BOB).await, Some(eth(3)));

    fixture.fault(Fault::Unavailable(BOB));
    let hash = fixture.mine_without_bal(BlockWrites::default());
    let head = fixture.world(|w| w.verified(hash)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Reanchored(head));
    assert!(node.holds(Key::Account(ALICE)));
    assert!(!node.holds(Key::Account(BOB)));

    fixture.heal();
    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    assert!(node.holds(Key::Account(BOB)));
}

#[tokio::test]
async fn a_pinned_key_that_cannot_be_proven_never_blocks_the_head() {
    let fixture = token_world(250);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], TOKENS).await;
    let held = Key::Slot(PROXY, mapping_slot(ALICE));
    let pins = node.working().0;
    assert!(pins.contains(&held));
    assert!(node.holds(held));

    fixture.fault(Fault::Unavailable(PROXY));
    let hash = fixture.mine_without_bal(BlockWrites::default().balance(ALICE, eth(4)));
    let head = fixture.world(|w| w.verified(hash)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Reanchored(head));
    assert_eq!(node.header(BlockSel::Latest).unwrap().num_hash(), head);
    assert_eq!(balance(&node, ALICE).await, Some(eth(4)));
    assert!(!node.holds(held));
    assert_eq!(node.working().0, pins, "an unproven pin stays pinned");

    fixture.heal();
    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    assert!(node.holds(held));
}

#[tokio::test]
async fn health_counts_the_pinned_and_learned_keys_the_head_holds() {
    let counts = |node: &Node<FixturePeers>| {
        let health = node.health();
        (health.pinned, health.learned, health.unheld_pinned)
    };
    let held = |held, total| Held { held, total };
    let fixture = alice_and_bob();
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = open(&fixture, dir.path(), &[], &watchlist(&[ALICE])).await;
    assert_eq!(
        counts(&node),
        (held(0, 1), held(0, 0), vec![Key::Account(ALICE)])
    );

    assert_eq!(balance(&node, BOB).await, Some(eth(3)));
    assert_eq!(
        counts(&node),
        (held(0, 1), held(1, 1), vec![Key::Account(ALICE)])
    );

    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    assert_eq!(
        node.working().0,
        BTreeSet::from([Key::Account(ALICE), Key::Account(MAINNET_ENS_RESOLVER)])
    );
    assert_eq!(counts(&node), (held(2, 2), held(1, 1), vec![]));

    fixture.fault(Fault::Unavailable(BOB));
    let hash = fixture.mine_without_bal(BlockWrites::default());
    let head = fixture.world(|w| w.verified(hash)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Reanchored(head));
    assert_eq!(counts(&node), (held(2, 2), held(0, 1), vec![]));
}

#[tokio::test]
async fn health_counts_without_computing_token_balances() {
    let fixture = two_token_world(250, 40);
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = open(&fixture, dir.path(), BOTH, &watchlist(&[ALICE, CAROL])).await;
    let working = node.working();

    let health = node.health();
    assert_eq!(
        (
            health.peers,
            health.state_peers,
            health.pinned,
            health.learned,
            health.unheld_pinned
        ),
        (
            1,
            1,
            Held { held: 0, total: 2 },
            Held { held: 0, total: 0 },
            vec![Key::Account(ALICE), Key::Account(CAROL)]
        )
    );
    assert_eq!(fixture.log().proofs, 0);
    assert_eq!(node.working(), working);
    assert_eq!(health.head, node.status().snapshot.block);
}

#[tokio::test]
async fn status_lists_a_token_only_when_its_balance_is_nonzero() {
    let tokens = |node: &Node<FixturePeers>, holder: usize| {
        node.status().snapshot.holdings[holder].tokens.clone()
    };
    let fixture = two_token_world(250, 40);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = open(&fixture, dir.path(), BOTH, &watchlist(&[ALICE, CAROL])).await;
    assert_eq!(tokens(&node, 0), vec![]);
    assert_eq!(tokens(&node, 1), vec![]);

    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    assert_eq!(
        tokens(&node, 0),
        vec![
            ("TKN".to_owned(), Some(U256::from(250))),
            ("PSE".to_owned(), Some(U256::from(40)))
        ]
    );
    assert_eq!(tokens(&node, 1), vec![]);
    assert_eq!(token_balance(&node, CAROL).await, U256::ZERO);
    assert_eq!(tokens(&node, 1), vec![]);
}

#[tokio::test]
async fn a_bal_write_to_an_unread_slot_is_not_held() {
    let fixture = token_world(250);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], TOKENS).await;

    fixture.mine(
        BlockWrites::default()
            .slot(PROXY, mapping_slot(BOB), U256::from(7))
            .slot(PROXY, mapping_slot(ALICE), U256::from(300)),
    );
    assert!(matches!(
        follower.step().await.unwrap(),
        Stepped::Extended(_)
    ));
    assert!(!node.holds(Key::Slot(PROXY, mapping_slot(BOB))));
    let before = fixture.log();
    assert_eq!(token_balance(&node, ALICE).await, U256::from(300));
    assert_eq!(fixture.log().proofs, before.proofs);

    assert_eq!(token_balance(&node, BOB).await, U256::from(7));
    assert_eq!(
        fixture.log().slots[before.slots.len()..],
        [(keccak256(PROXY), keccak256(B256::from(mapping_slot(BOB))))]
    );
    assert!(
        node.working()
            .1
            .contains(&Key::Slot(PROXY, mapping_slot(BOB)))
    );
}

async fn storage(node: &Node<FixturePeers>, address: Address, slot: u64) -> U256 {
    node.storage(address, U256::from(slot), BlockSel::Latest)
        .await
        .unwrap()
}

fn slot_hashes(address: Address, slots: &[u64]) -> Vec<(B256, B256)> {
    slots
        .iter()
        .map(|slot| (keccak256(address), keccak256(B256::from(U256::from(*slot)))))
        .collect()
}

#[tokio::test]
async fn a_restart_opens_with_nothing_proven_then_warms_remembered_keys_newest_first() {
    let fixture = token_world(250);
    let dir = tempfile::tempdir().unwrap();
    let watch = watchlist(&[ALICE]);
    {
        let (node, _follower) = start_with(&fixture, dir.path(), &[], &watch).await;
        for slot in [1, 2, 3] {
            assert_eq!(storage(&node, PROXY, slot).await, U256::ZERO);
        }
        node.save_keys().await.unwrap();
    }

    let before = fixture.log();
    let (node, mut follower) = open(&fixture, dir.path(), &[], &watch).await;
    assert_eq!(fixture.log().proofs, before.proofs);
    assert_eq!(
        node.working().1,
        BTreeSet::from([
            Key::Account(PROXY),
            Key::Slot(PROXY, U256::from(1)),
            Key::Slot(PROXY, U256::from(2)),
            Key::Slot(PROXY, U256::from(3)),
        ])
    );
    assert!(!node.holds(Key::Slot(PROXY, U256::from(3))));

    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    assert_eq!(fixture.log().proofs, before.proofs + 6);
    assert_eq!(
        fixture.log().slots[before.slots.len()..],
        slot_hashes(PROXY, &[3, 2, 1])
    );
    assert_eq!(storage(&node, PROXY, 1).await, U256::ZERO);
    assert_eq!(fixture.log().proofs, before.proofs + 6);
}

#[tokio::test]
async fn learned_keys_are_saved_only_when_membership_changes() {
    let fixture = alice_and_bob();
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;
    let saved = |follower: &Follower<FixturePeers>| follower.shared.disk.load_keys().unwrap();
    let extend = async |follower: &mut Follower<FixturePeers>| {
        fixture.mine(BlockWrites::default());
        assert!(matches!(
            follower.step().await.unwrap(),
            Stepped::Extended(_)
        ));
    };

    assert_eq!(balance(&node, BOB).await, Some(eth(3)));
    assert_eq!(balance(&node, CAROL).await, None);
    for _ in 0..4 {
        extend(&mut follower).await;
    }
    assert_eq!(saved(&follower), vec![]);
    extend(&mut follower).await;
    assert_eq!(
        saved(&follower),
        vec![Key::Account(CAROL), Key::Account(BOB)]
    );

    for _ in 0..12 {
        assert_eq!(balance(&node, BOB).await, Some(eth(3)));
        assert_eq!(balance(&node, ALICE).await, Some(eth(2)));
        extend(&mut follower).await;
    }
    assert_eq!(
        saved(&follower),
        vec![Key::Account(CAROL), Key::Account(BOB)]
    );

    node.save_keys().await.unwrap();
    assert_eq!(
        saved(&follower),
        vec![Key::Account(BOB), Key::Account(CAROL)]
    );
}

#[tokio::test]
async fn recency_survives_a_restart_so_eviction_takes_the_key_oldest_before_it() {
    let fixture = token_world(250);
    let dir = tempfile::tempdir().unwrap();
    let watch = watchlist(&[]);
    {
        let (node, _follower) = open(&fixture, dir.path(), &[], &watch).await;
        for slot in 0..1023 {
            storage(&node, PROXY, slot).await;
        }
        storage(&node, PROXY, 0).await;
        assert_eq!(node.working().1.len(), 1024);
        node.save_keys().await.unwrap();
    }

    let (node, _follower) = open(&fixture, dir.path(), &[], &watch).await;
    assert_eq!(node.working().1.len(), 1024);
    storage(&node, PROXY, 5000).await;
    let learned = node.working().1;
    assert_eq!(learned.len(), 1024);
    assert!(!learned.contains(&Key::Slot(PROXY, U256::from(1))));
    assert!(learned.contains(&Key::Slot(PROXY, U256::from(0))));
    assert!(learned.contains(&Key::Slot(PROXY, U256::from(2))));
    assert!(learned.contains(&Key::Slot(PROXY, U256::from(5000))));
}

#[tokio::test]
async fn garbage_key_rows_are_skipped_and_cost_nothing_else() {
    let fixture = alice_and_bob();
    let dir = tempfile::tempdir().unwrap();
    Disk::open(dir.path())
        .unwrap()
        .save_keys(&[Key::Account(BOB)])
        .unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("chibi.db")).unwrap();
    conn.execute_batch(
        "INSERT INTO keys (position, address, slot) VALUES
            (1, x'0b0b', NULL),
            (2, x'0000000000000000000000000000000000000b0b', x'01'),
            (3, 'not an address', NULL),
            (4, x'0000000000000000000000000000000000000b0b', 7);",
    )
    .unwrap();
    drop(conn);

    let (node, _follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;
    assert_eq!(node.working().1, BTreeSet::from([Key::Account(BOB)]));
    assert_eq!(fixture.log().proofs, 3);
    assert_eq!(balance(&node, BOB).await, Some(eth(3)));
    assert_eq!(fixture.log().proofs, 3);
}

/// What a stand-in resolver reports as the name's resolver and reverse resolver.
const NAME_RESOLVER: Address = address!("0x0000000000000000000000000000000000005e50");

/// Code answering `reverse` like the UniversalResolver: `name` for `named`, and the empty
/// name every unnamed address gets for any other address.
fn reverse_resolver_code(named: Address, name: &[u8]) -> Bytes {
    let unnamed_reply = (String::new(), Address::ZERO, NAME_RESOLVER).abi_encode_params();
    // `bytes` and `string` share an encoding, so a name need not be UTF-8.
    let named_reply =
        (Bytes::copy_from_slice(name), NAME_RESOLVER, NAME_RESOLVER).abi_encode_params();
    // Calldata is the selector, the bytes offset, the coin type, the bytes length, then the
    // address left-aligned in its word at 0x64.
    let mut code = vec![0x7f]; // push32 the expected word
    code.extend_from_slice(named.as_slice());
    code.extend_from_slice(&[0; 12]);
    code.extend_from_slice(&[0x60, 0x64, 0x35, 0x14]); // eq(calldataload(0x64), word)
    let branch = code.len() + 4 + RETURN_SLICE_CODE_LEN;
    code.push(0x61); // push2 the named branch
    code.extend_from_slice(&u16::try_from(branch).unwrap().to_be_bytes());
    code.push(0x57); // jumpi
    let data = branch + 1 + RETURN_SLICE_CODE_LEN;
    return_code_slice(&mut code, data, unnamed_reply.len());
    code.push(0x5b); // jumpdest
    return_code_slice(&mut code, data + unnamed_reply.len(), named_reply.len());
    code.extend_from_slice(&unnamed_reply);
    code.extend_from_slice(&named_reply);
    Bytes::from(code)
}

const RETURN_SLICE_CODE_LEN: usize = 15;

/// Appends code returning `len` bytes of the running code from `offset`.
fn return_code_slice(code: &mut Vec<u8>, offset: usize, len: usize) {
    let (offset, len) = (
        u16::try_from(offset).unwrap().to_be_bytes(),
        u16::try_from(len).unwrap().to_be_bytes(),
    );
    let start = code.len();
    code.push(0x61); // push2 len
    code.extend_from_slice(&len);
    code.push(0x61); // push2 offset
    code.extend_from_slice(&offset);
    code.extend_from_slice(&[0x60, 0x00, 0x39]); // codecopy(0, offset, len)
    code.push(0x61); // push2 len
    code.extend_from_slice(&len);
    code.extend_from_slice(&[0x60, 0x00, 0xf3]); // return(0, len)
    assert_eq!(code.len() - start, RETURN_SLICE_CODE_LEN);
}

/// Code that reverts every call, like the UniversalResolver on a forward-check mismatch or
/// an `OffchainLookup`.
fn reverting_code() -> Bytes {
    Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]) // revert(0, 0)
}

fn names(node: &Node<FixturePeers>) -> Vec<Option<String>> {
    node.status()
        .snapshot
        .holdings
        .into_iter()
        .map(|holding| holding.name)
        .collect()
}

#[tokio::test]
async fn a_watched_address_shows_its_primary_name_and_bals_keep_it_current() {
    let fixture = Fixture::genesis([
        (ALICE, WorldAccount::eoa(eth(2))),
        (BOB, WorldAccount::eoa(eth(3))),
        (
            MAINNET_ENS_RESOLVER,
            WorldAccount::contract(reverse_resolver_code(ALICE, b"alice.eth")),
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE, BOB], &[]).await;

    assert_eq!(names(&node), vec![Some("alice.eth".to_owned()), None]);
    assert!(
        node.working()
            .0
            .contains(&Key::Account(MAINNET_ENS_RESOLVER))
    );
    let saved = read_snapshot(dir.path()).unwrap().unwrap();
    assert_eq!(saved.holdings[0].name.as_deref(), Some("alice.eth"));
    assert_eq!(saved.holdings[1].name, None);

    let proofs = fixture.log().proofs;
    fixture.mine(
        BlockWrites::default().code(MAINNET_ENS_RESOLVER, reverse_resolver_code(BOB, b"bob.eth")),
    );
    assert!(matches!(
        follower.step().await.unwrap(),
        Stepped::Extended(_)
    ));
    assert_eq!(names(&node), vec![None, Some("bob.eth".to_owned())]);
    assert_eq!(fixture.log().proofs, proofs);
}

#[tokio::test]
async fn a_name_that_is_not_utf8_is_no_name() {
    let fixture = Fixture::genesis([
        (ALICE, WorldAccount::eoa(eth(2))),
        (
            MAINNET_ENS_RESOLVER,
            WorldAccount::contract(reverse_resolver_code(ALICE, b"\xff.eth")),
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (node, _follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;

    assert_eq!(names(&node), vec![None]);
}

#[tokio::test]
async fn a_reverting_resolver_gives_no_name_and_seeding_still_completes() {
    let mut accounts = token_accounts(250);
    accounts.push((
        MAINNET_ENS_RESOLVER,
        WorldAccount::contract(reverting_code()),
    ));
    let fixture = Fixture::genesis(accounts);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], TOKENS).await;

    let holding = &node.status().snapshot.holdings[0];
    assert_eq!(holding.name, None);
    assert_eq!(holding.balance, Some(eth(2)));
    assert_eq!(
        holding.tokens,
        vec![("TKN".to_owned(), Some(U256::from(250)))]
    );
    assert_eq!(
        node.working().0,
        BTreeSet::from([
            Key::Account(ALICE),
            Key::Account(MAINNET_ENS_RESOLVER),
            Key::Account(PROXY),
            Key::Account(IMPL),
            Key::Slot(PROXY, impl_slot()),
            Key::Slot(PROXY, mapping_slot(ALICE)),
        ])
    );

    let proofs = fixture.log().proofs;
    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    assert_eq!(fixture.log().proofs, proofs, "seeding is no longer due");
}

const PAYER_KEY: B256 = B256::repeat_byte(0x42);

/// The emitter of EIP-7708 ETH transfer logs.
const ETH_LOGS: Address = address!("0xfffffffffffffffffffffffffffffffffffffffe");

fn history_watch(addresses: &[Address]) -> Watchlist {
    Watchlist {
        history: true,
        ..watchlist(addresses)
    }
}

fn payment_tx(nonce: u64) -> Transaction {
    chibi_core::testing::signed_transfer(PAYER_KEY, 1, nonce, ALICE, eth(1))
}

/// The EIP-7708 log of an ether paid to `to`.
fn transfer_log(to: Address) -> EvmLog {
    EvmLog::new_unchecked(
        ETH_LOGS,
        vec![
            keccak256("Transfer(address,address,uint256)"),
            key_address(PAYER_KEY).into_word(),
            to.into_word(),
        ],
        Bytes::from(eth(1).to_be_bytes::<32>().to_vec()),
    )
}

fn payment_log() -> EvmLog {
    transfer_log(ALICE)
}

/// A block whose one transaction pays `to` an ether: its credit sits at the transaction's
/// block access index, and the receipt carries the EIP-7708 log.
fn transfer(to: Address, nonce: u64, to_after: U256) -> BlockWrites {
    BlockWrites::default()
        .tx_with(
            chibi_core::testing::signed_transfer(PAYER_KEY, 1, nonce, to, eth(1)),
            Outcome {
                success: true,
                gas_used: 21_000,
                logs: vec![transfer_log(to)],
            },
        )
        .at(1)
        .balance(to, to_after)
}

/// A block whose one transaction pays ALICE an ether.
fn payment(nonce: u64, alice_after: U256) -> BlockWrites {
    transfer(ALICE, nonce, alice_after)
}

fn history(dir: &Path) -> History {
    History::open(dir).unwrap().unwrap()
}

fn recorded_blocks(dir: &Path, account: Address) -> Vec<u64> {
    history(dir)
        .page(account, None, NonZeroUsize::new(100).unwrap())
        .unwrap()
        .blocks
        .iter()
        .map(|facts| facts.block.number)
        .collect()
}

fn covered(dir: &Path, account: Address) -> Vec<(Capability, Vec<(u64, u64)>)> {
    history(dir)
        .coverage(account)
        .unwrap()
        .ranges
        .into_iter()
        .map(|(capability, ranges)| (capability, ranges.iter().collect()))
        .collect()
}

fn both(ranges: &[(u64, u64)]) -> Vec<(Capability, Vec<(u64, u64)>)> {
    vec![
        (Capability::Bal, ranges.to_vec()),
        (Capability::Logs, ranges.to_vec()),
    ]
}

#[tokio::test]
async fn with_history_on_a_payment_to_a_watched_account_is_recorded_with_its_coverage() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (_node, mut follower) =
        start_with(&fixture, dir.path(), &[], &history_watch(&[ALICE])).await;

    let hash = fixture.mine(payment(0, eth(3)));
    let head = fixture.world(|w| w.verified(hash)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Extended(head));

    let page = history(dir.path())
        .page(ALICE, None, NonZeroUsize::new(10).unwrap())
        .unwrap();
    assert_eq!(page.blocks.len(), 1);
    let facts = &page.blocks[0];
    assert_eq!((facts.block.number, facts.block.hash), (head.number, hash));
    assert_eq!(
        facts.txs,
        [TxRow {
            index: 0,
            hash: *payment_tx(0).tx_hash(),
            sender: key_address(PAYER_KEY),
            raw: Some(Bytes::from(payment_tx(0).encoded_2718())),
            success: true,
            cumulative_gas: 21_000,
            gas_used: 21_000,
            first_log_index: 0,
            log_count: 1,
        }]
    );
    assert_eq!(
        facts.logs,
        [LogRow {
            index: 0,
            log: payment_log(),
        }]
    );
    assert_eq!(
        facts.touches,
        [Touch {
            account: ALICE,
            at: At::Tx(0),
            balance: Some(eth(3)),
            nonce: None,
            code: None,
        }]
    );
    assert_eq!(
        covered(dir.path(), ALICE),
        both(&[(head.number, head.number)])
    );
    assert_eq!((fixture.log().bodies, fixture.log().receipts), (1, 1));
}

#[tokio::test]
async fn with_history_off_the_same_chain_writes_no_history_and_fetches_no_contents() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (_node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;

    let hash = fixture.mine(payment(0, eth(3)));
    let head = fixture.world(|w| w.verified(hash)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Extended(head));

    assert_eq!((fixture.log().bodies, fixture.log().receipts), (0, 0));
    assert_eq!(recorded_blocks(dir.path(), ALICE), Vec::<u64>::new());
    let coverage = history(dir.path()).coverage(ALICE).unwrap();
    assert!(
        coverage
            .ranges
            .iter()
            .all(|(_, ranges)| ranges.first().is_none())
    );
    assert_eq!((coverage.opens, coverage.settled), (Vec::new(), None));
}

#[tokio::test]
async fn the_first_recorded_step_opens_each_watched_account_at_the_anchor_with_its_proven_state() {
    let fixture = Fixture::genesis([
        (ALICE, WorldAccount::eoa(eth(2)).with_nonce(5)),
        (CAROL, WorldAccount::eoa(eth(9))),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (node, mut follower) =
        start_with(&fixture, dir.path(), &[], &history_watch(&[ALICE, CAROL])).await;
    let anchor = node.head();

    fixture.mine(payment(0, eth(3)));
    follower.step().await.unwrap();
    fixture.mine(BlockWrites::default().balance(CAROL, eth(8)));
    follower.step().await.unwrap();

    let open = |account, balance, nonce| OpenState {
        account,
        number: anchor.number(),
        hash: anchor.hash(),
        balance,
        nonce,
        slots: Vec::new(),
    };
    let coverage = |account| history(dir.path()).coverage(account).unwrap().opens;
    assert_eq!(coverage(ALICE), [open(ALICE, eth(2), 5)]);
    assert_eq!(
        coverage(CAROL),
        [open(CAROL, eth(9), 0)],
        "opened only once"
    );
}

#[tokio::test]
async fn finalizing_a_recorded_block_settles_history_there() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (_node, mut follower) =
        start_with(&fixture, dir.path(), &[], &history_watch(&[ALICE])).await;
    let paid = fixture.mine(payment(0, eth(3)));
    follower.step().await.unwrap();
    fixture.mine(BlockWrites::default());
    follower.step().await.unwrap();

    fixture.finalize(paid);
    assert_eq!(follower.step().await.unwrap(), Stepped::Current);

    let number = fixture.world(|w| w.verified(paid)).number();
    assert_eq!(
        history(dir.path()).coverage(ALICE).unwrap().settled,
        Some(number)
    );
    assert_eq!(recorded_blocks(dir.path(), ALICE), [number]);
}

#[tokio::test]
async fn a_fork_that_reanchors_retracts_the_orphaned_block_and_records_the_new_branch() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (_node, mut follower) =
        start_with(&fixture, dir.path(), &[], &history_watch(&[ALICE])).await;
    let orphan = fixture.mine(payment(0, eth(3)));
    let orphan = fixture.world(|w| w.verified(orphan)).num_hash();
    follower.step().await.unwrap();
    assert_eq!(recorded_blocks(dir.path(), ALICE), [orphan.number]);

    let sibling = fixture.fork_head(BlockWrites::default().balance(ALICE, eth(5)));
    let sibling = fixture.world(|w| w.verified(sibling));
    assert_eq!(
        follower.step().await.unwrap(),
        Stepped::Reanchored(sibling.num_hash())
    );
    assert_eq!(recorded_blocks(dir.path(), ALICE), Vec::<u64>::new());
    assert_eq!(covered(dir.path(), ALICE), Vec::new());

    let child = fixture.mine(payment(0, eth(6)));
    let child = fixture.world(|w| w.verified(child));
    assert_eq!(
        follower.step().await.unwrap(),
        Stepped::Extended(child.num_hash())
    );
    let page = history(dir.path())
        .page(ALICE, None, NonZeroUsize::new(10).unwrap())
        .unwrap();
    let blocks: Vec<_> = page
        .blocks
        .iter()
        .map(|facts| (facts.block.number, facts.block.hash))
        .collect();
    assert_eq!(blocks, [(child.number(), child.hash())]);
    assert_eq!(
        covered(dir.path(), ALICE),
        both(&[(child.number(), child.number())])
    );
    let opens = history(dir.path()).coverage(ALICE).unwrap().opens;
    let opened: Vec<_> = opens.iter().map(|open| (open.number, open.hash)).collect();
    assert_eq!(opened, [(sibling.number(), sibling.hash())]);
}

#[tokio::test]
async fn contents_that_cannot_be_fetched_wait_for_a_later_head_and_are_recorded_in_order() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let (_node, mut follower) =
        start_with(&fixture, dir.path(), &[], &history_watch(&[ALICE])).await;

    fixture.fault(Fault::WithholdBodies);
    let paid = fixture.mine(payment(0, eth(3)));
    let paid = fixture.world(|w| w.verified(paid)).number();
    follower.step().await.unwrap();
    assert_eq!(recorded_blocks(dir.path(), ALICE), Vec::<u64>::new());

    fixture.heal();
    fixture.mine(BlockWrites::default());
    follower.step().await.unwrap();
    assert_eq!(recorded_blocks(dir.path(), ALICE), [paid]);
    assert_eq!(covered(dir.path(), ALICE), both(&[(paid, paid + 1)]));
}

#[tokio::test]
async fn a_restart_keeps_the_earlier_history_and_covers_all_but_the_blocks_it_missed() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let watch = history_watch(&[ALICE]);
    let first = {
        let (_node, mut follower) = start_with(&fixture, dir.path(), &[], &watch).await;
        let paid = fixture.mine(payment(0, eth(3)));
        follower.step().await.unwrap();
        fixture.mine(BlockWrites::default());
        follower.step().await.unwrap();
        fixture.world(|w| w.verified(paid)).number()
    };
    fixture.mine_unseen(BlockWrites::default());
    fixture.mine(BlockWrites::default());

    let (_node, mut follower) = start_with(&fixture, dir.path(), &[], &watch).await;
    let paid = fixture.mine(payment(1, eth(4)));
    let paid = fixture.world(|w| w.verified(paid)).number();
    follower.step().await.unwrap();

    assert_eq!(recorded_blocks(dir.path(), ALICE), [paid, first]);
    assert_eq!(
        covered(dir.path(), ALICE),
        both(&[(first, first + 1), (paid, paid)])
    );
}

#[tokio::test]
async fn a_restart_after_a_fork_retracts_only_the_orphaned_block() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let dir = tempfile::tempdir().unwrap();
    let watch = history_watch(&[ALICE]);
    let first = {
        let (_node, mut follower) = start_with(&fixture, dir.path(), &[], &watch).await;
        let first = fixture.mine(payment(0, eth(3)));
        follower.step().await.unwrap();
        fixture.mine(payment(1, eth(4)));
        follower.step().await.unwrap();
        fixture.world(|w| w.verified(first)).number()
    };
    assert_eq!(recorded_blocks(dir.path(), ALICE), [first + 1, first]);
    fixture.fork_head(BlockWrites::default());
    fixture.mine(BlockWrites::default());

    let (_node, _follower) = start_with(&fixture, dir.path(), &[], &watch).await;

    assert_eq!(recorded_blocks(dir.path(), ALICE), [first]);
    assert_eq!(covered(dir.path(), ALICE), both(&[(first, first)]));
}

fn number(fixture: &Fixture, hash: B256) -> u64 {
    fixture.world(|w| w.verified(hash)).number()
}

fn genesis_number(fixture: &Fixture) -> u64 {
    fixture.world(|w| w.verified(w.head())).number()
}

/// Starts at the chain's head with history on for ALICE, follows one empty block and
/// finalizes it. The step that settles history there also takes the backward walk's first
/// step. Returns the settled block's number.
async fn settle_after_start(
    fixture: &Fixture,
    dir: &Path,
) -> (Node<FixturePeers>, Follower<FixturePeers>, u64) {
    let (node, mut follower) = start_with(fixture, dir, &[], &history_watch(&[ALICE])).await;
    let settled = fixture.mine(BlockWrites::default());
    follower.step().await.unwrap();
    fixture.finalize(settled);
    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    (node, follower, number(fixture, settled))
}

fn logs_only(bal: &[(u64, u64)], logs: &[(u64, u64)]) -> Vec<(Capability, Vec<(u64, u64)>)> {
    vec![
        (Capability::Bal, bal.to_vec()),
        (Capability::Logs, logs.to_vec()),
    ]
}

#[tokio::test]
async fn once_history_settles_the_walk_back_records_an_older_payment_with_logs_coverage_only() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let genesis = genesis_number(&fixture);
    let paid = fixture.mine(payment(0, eth(3)));
    let paid = number(&fixture, paid);
    fixture.mine(BlockWrites::default());
    let dir = tempfile::tempdir().unwrap();
    let (_node, _follower, settled) = settle_after_start(&fixture, dir.path()).await;

    let page = history(dir.path())
        .page(ALICE, None, NonZeroUsize::new(10).unwrap())
        .unwrap();
    assert_eq!(page.blocks.len(), 1);
    let facts = &page.blocks[0];
    assert_eq!(facts.block.number, paid);
    assert_eq!(
        facts.txs,
        [TxRow {
            index: 0,
            hash: *payment_tx(0).tx_hash(),
            sender: key_address(PAYER_KEY),
            raw: Some(Bytes::from(payment_tx(0).encoded_2718())),
            success: true,
            cumulative_gas: 21_000,
            gas_used: 21_000,
            first_log_index: 0,
            log_count: 1,
        }]
    );
    assert_eq!(
        facts.logs,
        [LogRow {
            index: 0,
            log: payment_log(),
        }]
    );
    assert_eq!(
        covered(dir.path(), ALICE),
        logs_only(&[(settled, settled)], &[(genesis, settled)])
    );
    assert_eq!((fixture.log().bodies, fixture.log().receipts), (1, 1));
}

#[tokio::test]
async fn the_walk_back_stops_at_the_first_header_without_a_bal_hash() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    fixture.mine_without_bal(payment(0, eth(3)));
    let floor = fixture.mine_without_bal(BlockWrites::default());
    let floor = number(&fixture, floor);
    let paid = fixture.mine(payment(1, eth(4)));
    let paid = number(&fixture, paid);
    fixture.mine(BlockWrites::default());
    let dir = tempfile::tempdir().unwrap();
    let (_node, mut follower, settled) = settle_after_start(&fixture, dir.path()).await;
    follower.step().await.unwrap();
    follower.step().await.unwrap();

    assert_eq!(recorded_blocks(dir.path(), ALICE), [paid]);
    assert_eq!(
        covered(dir.path(), ALICE),
        logs_only(&[(settled, settled)], &[(floor + 1, settled)])
    );
    assert_eq!((fixture.log().bodies, fixture.log().receipts), (1, 1));
}

#[tokio::test]
async fn a_block_whose_bloom_cannot_name_a_watched_account_is_covered_without_its_contents() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let genesis = genesis_number(&fixture);
    fixture.mine(transfer(BOB, 0, eth(1)));
    fixture.mine(BlockWrites::default());
    let dir = tempfile::tempdir().unwrap();
    let (_node, _follower, settled) = settle_after_start(&fixture, dir.path()).await;

    assert_eq!(recorded_blocks(dir.path(), ALICE), Vec::<u64>::new());
    assert_eq!(
        covered(dir.path(), ALICE),
        logs_only(&[(settled, settled)], &[(genesis, settled)])
    );
    assert_eq!((fixture.log().bodies, fixture.log().receipts), (0, 0));
}

#[tokio::test]
async fn receipts_no_peer_has_leave_their_block_a_gap_after_three_steps_and_the_walk_goes_on() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let genesis = genesis_number(&fixture);
    fixture.mine(BlockWrites::default());
    let paid = fixture.mine(payment(0, eth(3)));
    let paid = number(&fixture, paid);
    fixture.mine(BlockWrites::default());
    fixture.fault(Fault::EmptyReceipts);
    let dir = tempfile::tempdir().unwrap();
    let (_node, mut follower, settled) = settle_after_start(&fixture, dir.path()).await;

    let gap = logs_only(
        &[(settled, settled)],
        &[(genesis, paid - 1), (paid + 1, settled)],
    );
    assert_eq!(covered(dir.path(), ALICE), gap);
    assert_eq!(fixture.log().receipts, 3, "one fetch, three attempts");
    follower.step().await.unwrap();
    follower.step().await.unwrap();
    assert_eq!(fixture.log().receipts, 9, "retried on each of three steps");

    fixture.heal();
    follower.step().await.unwrap();
    assert_eq!(fixture.log().receipts, 9, "left behind after three steps");
    assert_eq!(covered(dir.path(), ALICE), gap);
    assert_eq!(recorded_blocks(dir.path(), ALICE), Vec::<u64>::new());
    assert_eq!(fixture.log().penalized, Vec::new());
}

#[tokio::test]
async fn a_walk_longer_than_one_steps_budget_spans_steps_while_forward_recording_goes_on() {
    let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
    let genesis = genesis_number(&fixture);
    let paid: Vec<u64> = (0..20)
        .map(|nonce| number(&fixture, fixture.mine(payment(nonce, eth(3 + nonce)))))
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let (_node, mut follower) =
        start_with(&fixture, dir.path(), &[], &history_watch(&[ALICE])).await;
    let settled = fixture.mine(BlockWrites::default());
    follower.step().await.unwrap();
    fixture.finalize(settled);
    let settled = number(&fixture, settled);
    let newest_first = |blocks: &[u64]| blocks.iter().rev().copied().collect::<Vec<_>>();

    let first = number(&fixture, fixture.mine(payment(20, eth(23))));
    follower.step().await.unwrap();
    let mut expected = newest_first(&paid[4..]);
    expected.insert(0, first);
    assert_eq!(
        recorded_blocks(dir.path(), ALICE),
        expected,
        "the new block and sixteen older blocks' contents"
    );

    let second = number(&fixture, fixture.mine(payment(21, eth(24))));
    follower.step().await.unwrap();
    let mut expected = newest_first(&paid);
    expected.splice(0..0, [second, first]);
    assert_eq!(recorded_blocks(dir.path(), ALICE), expected);
    assert_eq!(
        covered(dir.path(), ALICE),
        logs_only(&[(settled, second)], &[(genesis, second)])
    );
}
