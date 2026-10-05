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
- A census on 2026-10-05 found five Sepolia peers serving snap/2 in 12 minutes, all Nethermind. Discovery alone took from about one to several minutes to reach one.

## Usage

```text
$ ./install.sh
Network [sepolia]:
Watch-only address (blank to finish): 0x3D76...5ce6
Watch-only address (blank to finish):
wrote ~/Library/Application Support/chibi/config.toml (sepolia, 1 address, tracking USDC)

$ chibi run
INFO chibi::node: light client synced network=sepolia
INFO chibi::node: waiting for a snap/2 peer peers=0 snap_peers=0 dialed=6 elapsed_s=0
INFO chibi::node: anchored head=11848650 keys=8
INFO chibi::rpc: JSON-RPC on http://127.0.0.1:8545, chain id 11155111
INFO chibi::follow: head verified head=11848651 stepped=Reanchored(..)

$ chibi status
sepolia head 11848656 (saved 7s ago, not re-verified)
0x3D767Bf85779E2b550aB5Fcd7E3bE39FB77A5ce6   0.000710619962413892 ETH   226.00 USDC
```

The user adds `http://127.0.0.1:8545` as a custom network in the wallet. Balances arrive through `eth_getBalance`, USDC through `eth_call(balanceOf)`, and sends through `eth_estimateGas`, `eth_feeHistory`, `eth_sendRawTransaction`, and `eth_getTransactionReceipt`.

`chibi run` in the binary:

```rust
let (node, follower) = Node::start(node_config).await?; // returns at a freshly verified head
tokio::select! {
    result = follower.run() => result.wrap_err("head follower stopped"),
    result = rpc::serve(node, rpc_config) => result.wrap_err("JSON-RPC server stopped"),
    result = tokio::signal::ctrl_c() => { result?; Ok(()) }
}
```

An offline test drives the production verifier against real in-process tries (`crates/node/src/tests.rs`):

```rust
let fixture = Fixture::genesis([(ALICE, WorldAccount::eoa(eth(2)))]);
let (node, mut follower) = start(&fixture, dir.path(), &[ALICE], &[]).await;
fixture.fault(Fault::WithholdBals);
let hash = fixture.mine(BlockWrites::default().balance(ALICE, eth(4)));
let head = fixture.world(|w| w.verified(hash)).num_hash();
assert_eq!(follower.step().await.unwrap(), Stepped::Reanchored(head));
assert_eq!(balance(&node, ALICE).await, Some(eth(4)));
```

## Shape

```text
chibi/
  install.sh
  crates/core   chibi-core, no tokio: verify.rs state.rs exec.rs tx.rs
  crates/node   chibi-node: node.rs follow.rs fetch.rs disk.rs peers.rs heads.rs devp2p.rs beacon.rs network.rs fixture.rs
  crates/cli    chibi (bin): main.rs (init, run, status), config.rs, rpc.rs
```

Dependencies point one way: `cli -> node -> core`. reth-network and helios compile only into `node`. JSON-RPC types appear only in `cli/rpc.rs`.

### Data

`Store` is partial state at one verified block, its anchor. It holds `accounts: HashMap<Address, Entry>` with `Entry::Absent | Present { account, slots }`, code by hash, and a window of the anchor plus up to 256 verified ancestors, enough for `BLOCKHASH`. Reads return `Result<_, Miss>`. A slot of an absent account is a known zero without a fetch. Slots live inside their account, so a slot without a proven account cannot be represented. Entries carry no block number of their own: the whole store moves together, so "valid at which block" has one answer. No storage root is held, because BALs would leave it stale. Each slot proof fetches its account proof at the same root, and both requests go out concurrently.

`State` is the canonical value the follower owns. It wraps `Arc<Store>`, the tracked key set, the finalized header, and the `TxBook` of submitted transactions. Tracked keys are the watched accounts plus every key the token seeding runs read. Seeding runs two calls per watched address and selected token: `balanceOf(holder)` and a simulated `transfer(holder, 0)` from the holder. Together they cover proxy slots, pause flags, fee parameters, blacklist entries, and the holder's balance, so a send only has to fetch the recipient's slot. Recipient and allowance slots depend on the transaction and stay fetch-on-miss. Untracked keys fetched for dapp calls form a cache that a reanchor drops. `State::advance` and `State::reanchor` both return located transactions whose block left the held chain to pending, so a reorg cannot leave a stale receipt.

### Trust boundary

`core::verify` is the only code that reads peer bytes. Its inputs are a `TrustedHash` from the light client and chibi's own reply types (`AccountReply`, `StorageReply`, raw BAL bytes, `Header`, block bodies, receipts). The node adapter converts reth wire messages into those types, so reth's snap messages never reach core or the test seam. Its outputs are `VerifiedHeader`, `VerifiedBlock`, `Proven`, `VerifiedBody`, and `VerifiedReceipt`, whose fields are private and which have no public constructors. `AccountQuery`, `StorageQuery`, and `CodeQuery` each build one single-key request and verify the reply against the root they were built for, using `reth_trie_common::range_proof::verify_range_proof` with origin equal to limit. An empty reply without a proof means the peer lacks the root. It never means absence.

Values enter a `Store` only through `Store::insert(Proven)` at the anchor, `State::advance(VerifiedBlock)` for the anchor's child, or a reanchor that builds a new store from `Proven` facts. Values read back from disk never enter a `Store`.

### Advancing to a new head

`State::plan(&target)` is pure and returns `Current`, `Extend { blocks }`, or `Reanchor`. It returns `Extend` when the gap is at most 32 blocks and every header in it has a `block_access_list_hash`. For `Extend` the follower verifies the header chain down to the anchor, fetches each BAL with snap/2 first and eth/71 with `BalRequirement::Optional` second, checks `keccak(raw)` against the header, and applies the BALs in one `update`. BAL semantics follow reth's `block_access_list_update`: last change wins, unknown addresses are ignored, an account left empty becomes `Absent` with its slots (EIP-161), and new code is stored by hash.

`Reanchor` proves every tracked key at the target into a fresh store while the old store stays published, then swaps. Startup, reopen, headers without BALs, missing BALs, reorgs, and long gaps all take this path. After each reanchor the token seeding runs repeat, so an upgraded token contract that moves its balance slot heals itself.

### Requests and misses

`Node` pins `Arc<Store>` and runs a pure step on it. `exec::execute` uses a revm `Database` whose error type cannot be constructed. A miss is recorded and reads as empty for the rest of that run only, and any recorded miss withholds the result (`Result<_, Misses>`, where `Misses` is non-empty by construction). One run can find several misses. The node proves them in one parallel batch at the pinned anchor, inserts them into the request's own copy (`Arc::make_mut`), and runs again, so round trips scale with dependency depth rather than key count. At the end `State::merge` keeps the new facts if the anchor has not moved and drops them otherwise. Gas estimation is reth's bisection as a pure function inside the same loop.

### Concurrency

| Pair | Outcome |
|---|---|
| Follower advance and RPC read | The read keeps its pinned `Arc<Store>`. It sees one head. |
| Follower advance and a request's merge | Serialized by the mutex. A merge for an old anchor is dropped. |
| Two requests missing the same key | Both fetch. The second merge re-inserts an equal value. |
| Reanchor and RPC reads | Reads use the old store until the swap. |
| Submit and follower | Both write the state file through `Disk`, always taking the disk lock before the state lock. Submit writes the outbox first and records the tx only if that write succeeded, then broadcasts. Inclusion is found by a later step. |

The canonical `State` sits behind one `parking_lot::Mutex`, reachable only through `read` and `update`, which take synchronous closures. A closure cannot await, so no guard crosses an await. Exactly one `Follower` exists. It is not `Clone`, and it is the only code that advances or reanchors the store. helios's block channel feeds a latest-only `watch`, so a slow follower never blocks the light client.

### Persistence

The state file is `state.json` in the per-network data directory. Writes go to a temp path, then fsync, rename, and a directory fsync, on a blocking thread. The file holds a display snapshot of the watched values with its block number, the outbox, and up to 16 remembered snap/2 peers. Tracked keys are not saved: they are derived from the watchlist at every start, so a token removed from the config stops being tracked. Only peers reached through outbound sessions are remembered, because an inbound session's address is an ephemeral port. `chibi status` prints the snapshot labeled with its block. On start, pending outbox transactions are re-broadcast. A lock file lets one `chibi run` own the directory, and it holds that process's id for `chibi stop` and `chibi status`.

### Transactions and fees

`rpc.rs` decodes EIP-2718 bytes and rejects blob transactions. `Node::submit` checks the chain id, recovers the sender, persists the outbox, records the transaction with the anchor chibi held at submission, and broadcasts it to up to 8 connected peers. While anything is pending, the follower walks back from the anchor through every block it has not searched yet, down to the oldest pending transaction's submission anchor (at most 8192 blocks, past the header window when needed), fetches and verifies those bodies against `transactions_root`, and matches hashes. On a match it verifies the block's receipts against `receipts_root` and stores the receipt, including for blocks a reanchor skipped. Pending transactions whose nonce was used are pruned only after every block since their submission anchor was searched, so a failed body fetch, a restart, or a long sleep cannot drop a mined transaction. This works with and without BALs. `eth_getTransactionCount(pending)` adds pending outbox transactions to the anchor nonce. Fees use the verified base fee and tips sampled from verified bodies, fetched on demand and cached per block.

### Block tags and unsupported methods

`latest` and `pending` resolve to the anchor. `safe` and `finalized` resolve to the finalized header for block queries. State queries at other blocks, history, and `eth_getLogs` return a JSON-RPC error that names the held head.

### Networks

`node::network` keeps every chain fact in one `NetworkSpec` per `Network` variant: the reth `ChainSpec` (which drives both the devp2p fork id and the EVM fork schedule and blob parameters), the helios network config, the default beacon API URL, boot nodes, pinned snap/2 peers, and the registry of known tokens. The user picks which registry tokens to track; `NodeConfig.watch` carries the addresses and the picked tokens. On Sepolia, USDC is Circle's deployment and USDT and DAI are Aave's test tokens, because Tether and Maker have no Sepolia deployments. Sepolia is the default. Its beacon default is `https://ethereum-sepolia-beacon-api.publicnode.com`, which served the light-client endpoints on 2026-10-05. A beacon endpoint can stall chibi but cannot make it accept a wrong header.

The light-client checkpoint follows one precedence: a user override, else the checkpoint helios saved in the data directory (a file of exactly 32 bytes), else the newest checkpoint from the community checkpoint services, logged as a warning. chibi never passes helios's shipped default, which is older than helios's 14-day limit. helios saves only checkpoints newer than the one it started from, so after a first-run sync chibi saves that checkpoint itself in helios's file format.

At start chibi dials the pinned and remembered snap/2 peers as static peers and waits, with a progress log every 30 seconds, until one snap/2 session is up. It does not give up, because discovery of these rare peers has taken from under a minute to several minutes.

### Inbound requests

The node installs an `EthRequestHandler` channel and answers every `IncomingEthRequest` variant with an empty response of the matching type, through an exhaustive match.

### Errors

Core has `VerifyError` (lying or garbled peer), `StateError` (stale proof, non-child block, unlinked ancestors, full header window), `ExecError`, and `EstimateError`. `NodeError` adds light-client and devp2p failures, disk errors (including a data directory held by another process), `Unprovable` after three attempts, `TooManyMissRounds`, `Unsupported`, and `InvalidTransaction`. `cli::rpc` holds the only mapping from errors to JSON-RPC codes.

### RPC exposure

The server binds to loopback. A `Host` header filter admits only `localhost`, `127.0.0.1`, and `[::1]` by default, which blocks DNS rebinding. Cross-origin browser requests are allowed only from `chrome-extension://` and `moz-extension://` origins by default, so an ordinary website cannot read tracked balances. Both lists are configurable.

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
- A per-token storage-layout table, or asking users for storage slot keys. Running `balanceOf` and a self-transfer and tracking the keys they read covers proxies, pause flags, fees, and nonstandard layouts without per-token code.

## Open questions and risks

- Some wallets send state queries tagged `finalized`. Should those serve the anchor instead of an error?
- Should the RPC fail closed when the anchor falls behind the light client because no snap peer is reachable?
- Will Nethermind peers serve BALs over snap/2 after the fork, and will geth and reth peers enable snap/2? Today only Nethermind does on Sepolia, and the five pinned peers are a single point of slowness.
- One failed tracked-key proof aborts a whole reanchor, which keeps the old head published until the next step. Is that acceptable with one or two snap/2 peers?
- The first run trusts the community checkpoint services for weak subjectivity unless the user sets `checkpoint`. Should a release ship a recent checkpoint instead?
- Does MetaMask's background page send an `Origin` header to a custom RPC? If it does not, the CORS allowlist never applies to it.

## Next implementation step

After Sepolia's Glamsterdam fork on 2026-10-06 13:53:36 UTC, run `chibi run` and confirm heads log `Stepped::Extended` with BALs, then send a funded transaction from a wallet pointed at chibi and confirm its receipt.
