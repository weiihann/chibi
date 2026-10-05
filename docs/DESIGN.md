# chibi design

chibi is a local Ethereum JSON-RPC endpoint for a browser wallet. It answers from state it proved itself instead of from an RPC provider. It holds no keys: the wallet (MetaMask, Rabby) signs, and chibi verifies, simulates, and broadcasts.

## Problem

Behind the RPC sits a partial-state node. The helios light client vouches for execution block hashes. devp2p supplies headers, single-key snap/2 range proofs, and EIP-7928 block access lists (BALs). A local EVM runs over the keys the user touches and fetches any key it lacks.

These constraints shape the design:

- Snap peers serve only about the last 128 state roots, so every fetch targets a recent verified head, and one request pins one head from start to end.
- BALs carry post-values for balances, nonces, code, and slots, but no storage roots.
- A key never fetched is a miss. A miss must never read as zero. A key proven absent is a known zero.
- Sepolia has no BALs until 2026-10-06 13:53:36 UTC, and mainnet has none until its own Glamsterdam fork. Advancing without BALs is a first-class path.
- Wallets send concurrent requests.
- Peers drop clients that never answer, so chibi answers every inbound request with an empty response.
- On 2026-10-05 a Nethermind peer was the only Sepolia peer seen serving snap/2.

## Usage

```text
$ ./install.sh
installed ~/.cargo/bin/chibi
Network [sepolia]:
Watch-only address (blank to finish): 0x8f3C...a1
Watch-only address (blank to finish):
wrote ~/.config/chibi/config.toml (sepolia, 1 address, tracking USDC)

$ chibi run
light client synced (sepolia)
head 9318004 verified; tracking 1 account + USDC (5 keys)
JSON-RPC on http://127.0.0.1:8545, chain id 11155111

$ chibi status
sepolia head 9318211 (saved 8s ago, not re-verified)
0x8f3C...a1   1.2034 ETH   250.00 USDC
```

The user adds `http://127.0.0.1:8545` as a custom network in the wallet. Balances arrive through `eth_getBalance`, USDC through `eth_call(balanceOf)`, and sends through `eth_estimateGas`, `eth_feeHistory`, `eth_sendRawTransaction`, and `eth_getTransactionReceipt`.

`chibi run` in the binary:

```rust
let (node, follower) = Node::start(&config).await?; // returns at a freshly verified head
tokio::select! {
    r = follower.run() => r,
    r = rpc::serve(node, config.rpc) => r,
    _ = tokio::signal::ctrl_c() => Ok(()),
}
```

An offline test drives the production verifier against real in-process tries:

```rust
let chain = Fixture::genesis([(alice, eth(2))]);
let (node, mut follower) = Node::with_parts(Network::Sepolia.spec(), chain.peers(), chain.heads(), Disk::temp()?, &[alice]).await?;
chain.mine(BlockWrites::balance(alice, eth(1)));
follower.step().await?;
assert_eq!(node.account(alice, BlockSel::Latest).await?.map(|a| a.balance), Some(eth(1)));
```

## Shape

```text
chibi/
  install.sh
  crates/core   chibi-core, no tokio: verify.rs state.rs exec.rs tx.rs
  crates/node   chibi-node: node.rs follow.rs fetch.rs peers.rs devp2p.rs beacon.rs disk.rs network.rs fixture.rs
  crates/cli    chibi (bin): main.rs (init, run, status), config.rs, rpc.rs
```

Dependencies point one way: `cli -> node -> core`. reth-network and helios compile only into `node`. JSON-RPC types appear only in `cli/rpc.rs`.

### Data

`Store` is partial state at one verified block, its anchor. It holds `accounts: HashMap<Address, Entry>` with `Entry::Absent | Present { account, slots }`, code by hash, and a window of up to 256 verified headers ending at the anchor. Reads return `Result<_, Miss>`. A slot of an absent account is a known zero without a fetch. Slots live inside their account, so a slot without a proven account cannot be represented. Entries carry no block number of their own: the whole store moves together, so "valid at which block" has one answer. No storage root is held, because BALs would leave it stale. Each slot proof fetches its account proof at the same root, and both requests go out concurrently.

`State` is the canonical value the follower owns. It wraps `Arc<Store>`, the tracked key set, the finalized header, and the `TxBook` of submitted transactions. Tracked keys are the watched accounts plus every key the token seeding run read. Untracked keys fetched for dapp calls form a cache that a reanchor drops.

### Trust boundary

`core::verify` is the only code that reads peer bytes. Its inputs are a `TrustedHash` from the light client and chibi's own reply types (`AccountReply`, `StorageReply`, raw BAL bytes, `Header`, block bodies, receipts). The node adapter converts reth wire messages into those types, so reth's snap messages never reach core or the test seam. Its outputs are `VerifiedHeader`, `VerifiedBlock`, `Proven`, `VerifiedBody`, and `VerifiedReceipt`, whose fields are private and which have no public constructors. `AccountQuery`, `StorageQuery`, and `CodeQuery` each build one single-key request and verify the reply against the root they were built for, using `reth_trie_common::range_proof::verify_range_proof` with origin equal to limit. An empty reply without a proof means the peer lacks the root. It never means absence.

Values enter a `Store` only through `Store::insert(Proven)` at the anchor, `State::advance(VerifiedBlock)` for the anchor's child, or a reanchor that builds a new store from `Proven` facts. Values read back from disk never enter a `Store`.

### Advancing to a new head

`State::plan(&target)` is pure and returns `Current`, `Extend { blocks }`, or `Reanchor`. It returns `Extend` when the gap is at most 32 blocks and every header in it has a `block_access_list_hash`. For `Extend` the follower verifies the header chain down to the anchor, fetches each BAL with snap/2 first and eth/71 with `BalRequirement::Optional` second, checks `keccak(raw)` against the header, and applies the BALs in one `update`. BAL semantics follow reth's `block_access_list_update`: last change wins, unknown addresses are ignored, an account left empty becomes `Absent` with its slots (EIP-161), and new code is stored by hash.

`Reanchor` proves every tracked key at the target into a fresh store while the old store stays published, then swaps. Startup, reopen, headers without BALs, missing BALs, reorgs, and long gaps all take this path. After each reanchor the token seeding run repeats, so an upgraded token contract that moves its balance slot heals itself.

### Requests and misses

`Node` pins `Arc<Store>` and runs a pure step on it. `exec::execute` uses a revm `Database` whose error type cannot be constructed. A miss is recorded and reads as empty for the rest of that run only, and any recorded miss withholds the result (`Result<_, Misses>`, where `Misses` is non-empty by construction). One run can find several misses. The node proves them in one parallel batch at the pinned anchor, inserts them into the request's own copy (`Arc::make_mut`), and runs again, so round trips scale with dependency depth rather than key count. At the end `State::merge` keeps the new facts if the anchor has not moved and drops them otherwise. Gas estimation is reth's bisection as a pure function inside the same loop.

### Concurrency

| Pair | Outcome |
|---|---|
| Follower advance and RPC read | The read keeps its pinned `Arc<Store>`. It sees one head. |
| Follower advance and a request's merge | Serialized by the mutex. A merge for an old anchor is dropped. |
| Two requests missing the same key | Both fetch. The second merge re-inserts an equal value. |
| Reanchor and RPC reads | Reads use the old store until the swap. |
| Submit and follower | The tx is written to the outbox and persisted before broadcast. Inclusion is found by a later step. |

The canonical `State` sits behind one `parking_lot::Mutex`, reachable only through `read` and `update`, which take synchronous closures. A closure cannot await, so no guard crosses an await. Exactly one `Follower` exists. It is not `Clone`, and it is the only code that advances, reanchors, or writes the state file. helios's block channel feeds a latest-only `watch`, so a slow follower never blocks the light client.

### Persistence

After each head the follower writes the state file to a temp path, fsyncs, and renames it. The file holds the tracked keys, a display snapshot of their values with its block number, and the outbox. `chibi status` prints the snapshot labeled with its block. On start the keys feed the first reanchor, and pending outbox transactions are re-broadcast. A crash loses at most one head of merged cache facts.

### Transactions and fees

`rpc.rs` decodes EIP-2718 bytes and rejects blob transactions. `Node::submit` checks the chain id, recovers the sender, records the transaction, and broadcasts it to up to 8 peers with snap or eth sessions. While anything is pending, the follower fetches the body of every verified header it passes, including gap headers, verifies it against `transactions_root`, and matches hashes. On a match it verifies the block's receipts against `receipts_root` and stores the receipt. This works with and without BALs. `eth_getTransactionCount(pending)` adds pending outbox transactions to the anchor nonce. Fees use the verified base fee and tips sampled from verified bodies, fetched on demand and cached per block.

### Block tags and unsupported methods

`latest` and `pending` resolve to the anchor. `safe` and `finalized` resolve to the finalized header for block queries. State queries at other blocks, history, and `eth_getLogs` return a JSON-RPC error that names the held head.

### Networks

`node::network` keeps every chain fact in one `NetworkSpec` per `Network` variant: the reth `ChainSpec` (which drives both the devp2p fork id and the EVM fork schedule and blob parameters), the helios network config, the default beacon API URL, boot nodes, and tracked tokens. Sepolia is the default. Its beacon default is `https://ethereum-sepolia-beacon-api.publicnode.com`, which served the light-client endpoints on 2026-10-05. A beacon endpoint can stall chibi but cannot make it accept a wrong header. chibi sets helios's `checkpoint` only from a user override, so helios's saved checkpoint is used on later runs.

### Inbound requests

The node installs an `EthRequestHandler` channel and answers every `IncomingEthRequest` variant with an empty response of the matching type, through an exhaustive match.

### Errors

Core has `VerifyError` (lying or garbled peer), `StateError` (stale proof, non-child block, unlinked ancestors), and `ExecError`. `NodeError` adds light-client failure, checkpoint expiry, `Unprovable` after three peers, `TooManyMissRounds`, `Unsupported`, and `StoreLocked`. `cli::rpc` holds the only mapping from errors to JSON-RPC codes.

### RPC exposure

The server binds to loopback. Cross-origin browser requests are allowed only from `chrome-extension://` and `moz-extension://` origins by default, so an ordinary website cannot read tracked balances. The allowlist is configurable.

## Synthesis decision

Three structurally different designs were produced: an actor that owns all state (A), a pure value core with a thin async shell (B), and a redb ledger as the shared state (C). An independent judge scored A 30, B 28, and C 22 and recommended A.

B is the base. A and B were close, and B has the shorter call chains (5 to 6 files and one task for a miss, against 8 files and two tasks), three crates instead of four or seven, and the cheapest first end-to-end slice. A's advantages graft onto B without its actor:

- From A: reopen always reanchors, and persisted values are display-only. The core seam uses chibi reply types instead of reth snap messages. BALs are fetched with `BalRequirement::Optional` so a missing BAL falls back to reanchor instead of hanging, which fixes a judge finding against B.
- From C: the outbox is persisted before broadcast. The concurrency table. Reorg and snap-window faults in the test fixture.
- From the judge: never set helios's `checkpoint` from the shipped default.

Rejected: A's 64-BAL roll-forward window, which adds a second insertion path and carried a judge-found bug where an empty window made "unchanged since the pin" vacuously true. C's redb, which is outside the proven dependency set and holds a read transaction across awaits.

## Tradeoffs accepted

- We accept dropping a request's fetched facts when the head moves mid-request, in exchange for no roll-forward machinery.
- We accept duplicate fetches when concurrent requests miss the same key, in exchange for no shared in-flight table.
- We accept re-proving tracked keys at every head before Glamsterdam (tens of keys every 12 s), in exchange for one advance path.
- We accept that dapp keys are dropped at each reanchor and fetched again on the next call.
- We accept rewriting the whole state file at each head (a few KB), in exchange for atomic crash semantics without a write-ahead log.
- We accept one mutex around the canonical value, in exchange for no actor. It guards only synchronous pure calls.
- We accept receipts only for transactions submitted through chibi.
- We accept that `eth_call` and `eth_estimateGas` with `pending` use the anchor's block environment.

## Alternatives considered

- An actor that owns the store. Every write becomes a message, and late facts need a roll-forward window. It hides the same complexity behind a larger surface and a task to supervise.
- A redb ledger as the shared state. It gives MVCC and atomic commits, but adds a dependency and a schema, and its read transactions would be held across awaits.
- revm's `WrapDatabaseAsync`. It puts awaits inside the EVM, needs fibers and a multi-thread runtime, and costs one serial round trip per miss.
- Per-entry block numbers. Every read would need a freshness check, and one run could mix entries of different ages.
- A per-token storage-layout table. Running `balanceOf` once and tracking the keys it read covers proxies and nonstandard layouts without per-token code.

## Open questions and risks

- Some wallets send state queries tagged `finalized`. Should those serve the anchor instead of an error?
- Should the RPC fail closed when the anchor falls behind the light client because no snap peer is reachable?
- Will Nethermind peers serve BALs over snap/2 after the fork, and will reth peers enable snap? Should the registry pin known snap/2 peers?
- Before the first header is verified, the devp2p status uses a wall-clock fork id. The 2026-10-05 probe shows peers accept it.
- Does MetaMask's background page send an `Origin` header to a custom RPC? If it does not, the CORS allowlist never applies to it.

## Next implementation step

Build `chibi-core::verify` and `State::{insert, advance, plan}` against `fixture`'s real tries, with tests for a present key, an absent key, a forged proof, a BAL update, and a missing BAL falling back to reanchor.
