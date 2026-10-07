//! The RPC module over a real [`Node`] backed by the offline fixture chain.

use std::path::Path;

use alloy_consensus::{SignableTransaction as _, TxLegacy};
use alloy_eips::eip2718::Encodable2718 as _;
use alloy_primitives::{Address, B256, Bytes, TxKind, U64, U256, address};
use alloy_rpc_types_eth::TransactionReceipt;
use alloy_signer::SignerSync as _;
use alloy_signer_local::PrivateKeySigner;
use chibi_core::testing::{
    BlockWrites, EIP1967_IMPLEMENTATION_SLOT, GAS_PRICE, WorldAccount, balance_of_calldata,
    delegating_proxy_code, eth, mapping_reader_code, mapping_slot,
};
use chibi_core::wire;
use chibi_node::fixture::{Fault, Fixture, FixturePeers};
use chibi_node::{Disk, Follower, Node, Stepped, Token, Watchlist};
use jsonrpsee::server::{MethodsError, RpcModule};
use jsonrpsee::types::ErrorObjectOwned;
use serde_json::{Value, json};

use super::module;

const BOB: Address = address!("0x0000000000000000000000000000000000000b0b");
const PROXY: Address = address!("0x00000000000000000000000000000000000070c3");
const IMPL: Address = address!("0x0000000000000000000000000000000000001111");

const TOKENS: &[Token] = &[Token {
    symbol: "TKN",
    name: "Test token",
    address: PROXY,
    decimals: 18,
}];

type Rpc = RpcModule<Node<FixturePeers>>;

async fn start(
    fixture: &Fixture,
    dir: &Path,
    watch: &[Address],
) -> (Rpc, Node<FixturePeers>, Follower<FixturePeers>) {
    let (node, follower) = Node::with_parts(
        Fixture::spec(TOKENS),
        fixture.peers(),
        fixture.heads(),
        Disk::open(dir).unwrap(),
        &Watchlist {
            addresses: watch.to_vec(),
            contracts: Vec::new(),
        },
    )
    .await
    .unwrap();
    (module(node.clone()).unwrap(), node, follower)
}

async fn call(rpc: &Rpc, method: &str, params: Vec<Value>) -> Result<Value, ErrorObjectOwned> {
    rpc.call(method, params).await.map_err(|error| match error {
        MethodsError::JsonRpc(error) => error,
        MethodsError::Parse(_) | MethodsError::InvalidSubscriptionId(_) => {
            panic!("{method}: {error}")
        }
    })
}

fn token_world(holder: Address, tokens: u64) -> Fixture {
    Fixture::genesis([
        (holder, WorldAccount::eoa(eth(3))),
        (
            PROXY,
            WorldAccount::contract(delegating_proxy_code())
                .with_slot(
                    U256::from_be_bytes(EIP1967_IMPLEMENTATION_SLOT.0),
                    U256::from_be_slice(IMPL.as_slice()),
                )
                .with_slot(mapping_slot(holder), U256::from(tokens)),
        ),
        (IMPL, WorldAccount::contract(mapping_reader_code())),
    ])
}

fn signed_transfer(signer: &PrivateKeySigner, chain_id: u64) -> wire::Transaction {
    let tx = TxLegacy {
        chain_id: Some(chain_id),
        nonce: 0,
        gas_price: u128::from(GAS_PRICE),
        gas_limit: 21_000,
        to: TxKind::Call(BOB),
        value: eth(1),
        input: Bytes::new(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    wire::Transaction::Legacy(tx.into_signed(signature))
}

#[tokio::test]
async fn serves_chain_id_balances_and_token_calls_from_the_node() {
    let holder = PrivateKeySigner::random().address();
    let fixture = token_world(holder, 250);
    let dir = tempfile::tempdir().unwrap();
    let (rpc, node, _follower) = start(&fixture, dir.path(), &[holder]).await;

    assert_eq!(
        call(&rpc, "eth_chainId", vec![]).await.unwrap(),
        json!(U64::from(node.chain_id()))
    );
    assert_eq!(
        call(&rpc, "net_version", vec![]).await.unwrap(),
        json!(node.chain_id().to_string())
    );
    assert_eq!(
        call(&rpc, "eth_blockNumber", vec![]).await.unwrap(),
        json!(U64::from(node.head().number()))
    );
    assert_eq!(
        call(&rpc, "eth_getBalance", vec![json!(holder), json!("latest")])
            .await
            .unwrap(),
        json!(eth(3))
    );
    assert_eq!(
        call(
            &rpc,
            "eth_call",
            vec![
                json!({"to": PROXY, "data": balance_of_calldata(holder)}),
                json!("latest"),
            ],
        )
        .await
        .unwrap(),
        json!(B256::from(U256::from(250)))
    );
}

#[tokio::test]
async fn a_signed_transaction_is_sent_then_shown_pending_then_included_with_its_receipt() {
    let signer = PrivateKeySigner::random();
    let sender = signer.address();
    let fixture = Fixture::genesis([(sender, WorldAccount::eoa(eth(3)))]);
    let dir = tempfile::tempdir().unwrap();
    let (rpc, node, mut follower) = start(&fixture, dir.path(), &[sender]).await;

    let tx = signed_transfer(&signer, node.chain_id());
    let hash = *tx.tx_hash();
    assert_eq!(
        call(
            &rpc,
            "eth_sendRawTransaction",
            vec![json!(Bytes::from(tx.encoded_2718()))]
        )
        .await
        .unwrap(),
        json!(hash)
    );

    let pending = call(&rpc, "eth_getTransactionByHash", vec![json!(hash)])
        .await
        .unwrap();
    assert_eq!(
        [
            &pending["hash"],
            &pending["from"],
            &pending["to"],
            &pending["blockHash"],
            &pending["blockNumber"],
        ],
        [
            &json!(hash),
            &json!(sender),
            &json!(BOB),
            &Value::Null,
            &Value::Null,
        ]
    );
    assert_eq!(
        call(&rpc, "eth_getTransactionReceipt", vec![json!(hash)])
            .await
            .unwrap(),
        Value::Null
    );
    assert_eq!(
        call(
            &rpc,
            "eth_getTransactionCount",
            vec![json!(sender), json!("pending")]
        )
        .await
        .unwrap(),
        json!(U64::from(1))
    );

    let cost = U256::from(21_000 * GAS_PRICE);
    let mined = fixture.mine(
        BlockWrites::default()
            .balance(sender, eth(2) - cost)
            .nonce(sender, 1)
            .balance(BOB, eth(1))
            .tx(tx, 21_000, 1),
    );
    let block = fixture.world(|w| w.verified(mined)).num_hash();
    assert_eq!(follower.step().await.unwrap(), Stepped::Extended(block));

    let receipt: TransactionReceipt = serde_json::from_value(
        call(&rpc, "eth_getTransactionReceipt", vec![json!(hash)])
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(receipt.inner.is_success());
    assert_eq!(
        (
            receipt.transaction_hash,
            receipt.block_hash,
            receipt.block_number,
            receipt.transaction_index,
            receipt.gas_used,
            receipt.effective_gas_price,
            receipt.from,
            receipt.to,
            receipt.contract_address,
        ),
        (
            hash,
            Some(block.hash),
            Some(block.number),
            Some(0),
            21_000,
            u128::from(GAS_PRICE),
            sender,
            Some(BOB),
            None,
        )
    );
    let logs = receipt.inner.logs();
    assert_eq!(logs.len(), 1);
    assert_eq!(
        (
            logs[0].block_hash,
            logs[0].transaction_hash,
            logs[0].log_index
        ),
        (Some(block.hash), Some(hash), Some(0))
    );

    let included = call(&rpc, "eth_getTransactionByHash", vec![json!(hash)])
        .await
        .unwrap();
    assert_eq!(
        [
            &included["blockHash"],
            &included["blockNumber"],
            &included["transactionIndex"],
        ],
        [
            &json!(block.hash),
            &json!(U64::from(block.number)),
            &json!(U64::ZERO),
        ]
    );
    assert_eq!(
        call(&rpc, "eth_getBalance", vec![json!(BOB), json!("latest")])
            .await
            .unwrap(),
        json!(eth(1))
    );
}

#[tokio::test]
async fn an_unprovable_key_is_a_json_rpc_error_never_zero() {
    let fixture = Fixture::genesis([
        (
            BOB,
            WorldAccount::eoa(eth(3)).with_slot(U256::from(1), U256::from(5)),
        ),
        (IMPL, WorldAccount::eoa(eth(1))),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (rpc, _node, _follower) = start(&fixture, dir.path(), &[IMPL]).await;

    fixture.fault(Fault::Unavailable(BOB));
    for (method, params) in [
        ("eth_getBalance", vec![json!(BOB), json!("latest")]),
        ("eth_getTransactionCount", vec![json!(BOB), json!("latest")]),
        ("eth_getCode", vec![json!(BOB), json!("latest")]),
        (
            "eth_getStorageAt",
            vec![json!(BOB), json!(U256::from(1)), json!("latest")],
        ),
        ("eth_call", vec![json!({"from": BOB, "to": IMPL})]),
    ] {
        let error = call(&rpc, method, params).await.unwrap_err();
        assert_eq!(error.code(), -32000, "{method}: {error}");
    }

    fixture.heal();
    assert_eq!(
        call(&rpc, "eth_getBalance", vec![json!(BOB), json!("latest")])
            .await
            .unwrap(),
        json!(eth(3))
    );
    assert_eq!(
        call(
            &rpc,
            "eth_getStorageAt",
            vec![json!(BOB), json!(U256::from(1)), json!("latest")],
        )
        .await
        .unwrap(),
        json!(B256::from(U256::from(5)))
    );
}

#[tokio::test]
async fn history_and_logs_name_the_held_head() {
    let fixture = Fixture::genesis([(BOB, WorldAccount::eoa(eth(1)))]);
    let dir = tempfile::tempdir().unwrap();
    let (rpc, node, _follower) = start(&fixture, dir.path(), &[BOB]).await;
    let head = node.head().number();

    let logs = call(&rpc, "eth_getLogs", vec![json!({})])
        .await
        .unwrap_err();
    assert_eq!(
        (logs.code(), logs.message()),
        (
            -32004,
            format!("eth_getLogs is not supported by chibi (verified head is {head})").as_str()
        )
    );
    let earliest = call(&rpc, "eth_getBalance", vec![json!(BOB), json!("earliest")])
        .await
        .unwrap_err();
    assert_eq!(earliest.code(), -32004, "{earliest}");
}

#[tokio::test]
async fn a_balance_at_the_block_before_the_head_is_served_as_wallets_ask_for_it() {
    let fixture = Fixture::genesis([(BOB, WorldAccount::eoa(eth(3)))]);
    let dir = tempfile::tempdir().unwrap();
    let (rpc, node, mut follower) = start(&fixture, dir.path(), &[BOB]).await;
    assert_eq!(follower.step().await.unwrap(), Stepped::Current);
    let parent = node.head().number();

    fixture.mine(BlockWrites::default().balance(BOB, eth(1)));
    assert!(matches!(
        follower.step().await.unwrap(),
        Stepped::Extended(_)
    ));
    assert_eq!(node.head().number(), parent + 1);

    for (block, expected) in [(parent, eth(3)), (parent + 1, eth(1))] {
        assert_eq!(
            call(
                &rpc,
                "eth_getBalance",
                vec![json!(BOB), json!(U64::from(block))]
            )
            .await
            .unwrap(),
            json!(expected),
            "block {block}"
        );
    }
}
