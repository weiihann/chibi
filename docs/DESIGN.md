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
wrote ~/.config/chibi/config.toml (sepolia, 1 address)

$ chibi run
INFO chibi::node: light client synced network=sepolia
INFO chibi::node: waiting for a snap/2 peer peers=0 snap_peers=0 dialed=6 elapsed_s=0
INFO chibi::node: anchored head=11848650 keys=1
INFO chibi::rpc: JSON-RPC on http://127.0.0.1:8545, chain id 11155111
INFO chibi::follow: head verified head=11848651 stepped=Reanchored(..)

$ chibi status
sepolia head 11848656 (saved 7s ago, not re-verified)
0x3D767Bf85779E2b550aB5Fcd7E3bE39FB77A5ce6   0.000710619962413892 ETH   226.00 USDC
```

The user adds `http://127.0.0.1:8545` as a custom network in the wallet. Balances arrive through `eth_getBalance`, USDC through `eth_call(balanceOf)`, and sends through `eth_estimateGas`, `eth_feeHistory`, `eth_sendRawTransaction`, and `eth_getTransactionReceipt`. USDC shows in `chibi status` once the wallet has read its keys, as long as the balance is not zero.

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
  crates/core   chibi-core, no tokio: verify.rs state.rs working.rs exec.rs tx.rs
  crates/node   chibi-node: node.rs follow.rs fetch.rs disk.rs peers.rs heads.rs devp2p.rs beacon.rs network.rs fixture.rs
  crates/cli    chibi (bin): main.rs (init, run, status), config.rs, rpc.rs
```

Dependencies point one way: `cli -> node -> core`. reth-network and helios compile only into `node`. JSON-RPC types appear only in `cli/rpc.rs`.

### Data

`Store` is partial state at one verified block, its anchor. It holds `accounts: HashMap<Address, Entry>` with `Entry::Absent | Present { account, slots }`, code by hash, and a window of the anchor plus up to 256 verified ancestors, enough for `BLOCKHASH`. Reads return `Result<_, Miss>`. A slot of an absent account is a known zero without a fetch. Slots live inside their account, so a slot without a proven account cannot be represented. Entries carry no block number of their own: the whole store moves together, so "valid at which block" has one answer. No storage root is held, because BALs would leave it stale. Each slot proof fetches its account proof at the same root, and both requests go out concurrently.

`State` is the canonical value the follower owns. It wraps `Arc<Store>`, the `WorkingSet`, the finalized header, and the `TxBook` of submitted transactions. The `WorkingSet` (`core::working`) is the one source of truth for which keys chibi keeps; the store holds a key only if the working set does, and working-set keys the store lacks are unheld. Pins are the watched accounts, each configured `[[contracts]]` account with its listed raw storage slots, and every key the seeding runs read. Seeding runs ENS's `reverse(address, 60)` on the network's UniversalResolver once per watched address, sent from that address, and two calls per watched address and pinned token: `balanceOf(holder)` and a simulated `transfer(holder, 0)` from the holder. The resolver call covers the reverse registrar, the name's resolver, and the forward check the UniversalResolver makes itself (ENSIP-19), so the primary name stays computable from held keys. The token calls cover proxy slots, pause flags, fee parameters, blacklist entries, and the holder's balance, so a send only has to fetch the recipient's slot. Every other key a request reads (`eth_getBalance`, `eth_getTransactionCount`, `eth_getCode`, `eth_getStorageAt`, `eth_call`, `eth_estimateGas`) is learned. Keys are accounts and storage slots. While pins plus learned keys exceed `WORKING_SET_CAP` = 1024, the least recently used learned key is evicted. Pins are never evicted, so a pinned set larger than the cap is kept whole and leaves no room for learned keys; the cap bounds learned keys, not the total. Evicting an account evicts its learned slots, and evicted values leave the store. `State::advance` and `State::reanchor` both return located transactions whose block left the held chain to pending, so a reorg cannot leave a stale receipt.

### Trust boundary

`core::verify` is the only code that reads peer bytes. Its inputs are a `TrustedHash` from the light client and chibi's own reply types (`AccountReply`, `StorageReply`, raw BAL bytes, `Header`, block bodies, receipts). The node adapter converts reth wire messages into those types, so reth's snap messages never reach core or the test seam. Its outputs are `VerifiedHeader`, `VerifiedBlock`, `Proven`, `VerifiedBody`, and `VerifiedReceipt`, whose fields are private and which have no public constructors. `AccountQuery`, `StorageQuery`, and `CodeQuery` each build one single-key request and verify the reply against the root they were built for, using `reth_trie_common::range_proof::verify_range_proof` with origin equal to limit. An empty reply without a proof means the peer lacks the root. It never means absence.

Values enter a `Store` only through `Store::insert(Proven)` at the anchor, `State::advance(VerifiedBlock)` for the anchor's child, or a reanchor that builds a new store from `Proven` facts. Values read back from disk never enter a `Store`.

### Start

Once the trusted head's header is fetched and verified, `Node::start` builds an empty store at that anchor with the working set (the pins plus the learned keys the last run saved), restores the outbox, persists, and returns, and JSON-RPC opens. Nothing is proven before RPC opens. The follower's first step runs the seeding calls and warms everything unheld. Requests during warm-up fetch their misses on demand.

### Advancing to a new head

`State::plan(&target)` is pure and returns `Current`, `Extend { blocks }`, or `Reanchor`. It returns `Extend` when the gap is at most 32 blocks and every header in it has a `block_access_list_hash`. For `Extend` the follower verifies the header chain down to the anchor, fetches each BAL with snap/2 first and eth/71 with `BalRequirement::Optional` second, checks `keccak(raw)` against the header, and applies the BALs in one `update`. BAL semantics follow reth's `block_access_list_update`: last change wins, an account left empty becomes `Absent` with its slots (EIP-161), and new code is stored by hash. Application updates only held keys: unheld accounts and slots are ignored, including a written slot of a held account.

`Reanchor` proves the pins at the target into a fresh store while the old store stays published, then swaps; one failed pin proof aborts it. Headers without BALs, failed BAL fetches, reorgs, and long gaps take this path. After the swap the seeding runs repeat, so an upgraded token contract that moves its balance slot, or a primary name moved to another resolver, heals itself, and then learned keys are warmed. A seeding run that fails keeps every previous seeded pin, adds the keys the other runs read, and seeding is retried at each step until every run succeeds, so a peer that cannot yet serve an upgraded implementation never demotes a token's pins. Learned keys cannot block or abort a reanchor. Before Glamsterdam every head reanchors, so learned keys only partly re-warm between heads.

### Warming

The follower warms after each step: it proves unheld keys at the current anchor, pins first, then learned keys most recently used first, with at most `WARM_IN_FLIGHT` = 8 requests in flight, merging every 64 proofs. chibi sends snap/2 requests itself, up to 16 at once to each snap/2 session (reth's session buffers 32 and drops the rest; reth's fetcher would send one at a time), each to the session with the fewest in flight, so warming takes at most half of one peer and wallet requests and BAL fetches always find room. It stops at once when a new head arrives. A key that fails stays unheld for the next pass and never blocks the head. Requests wake the follower when anything is unheld.

### Requests and misses

`Node` pins `Arc<Store>` and runs a pure step on it. `exec::execute` uses a revm `Database` whose error type cannot be constructed. A miss is recorded and reads as empty for the rest of that run only, and any recorded miss withholds the result (`Result<_, Misses>`, where `Misses` is non-empty by construction). One run can find several misses. The node proves them in one parallel batch at the pinned anchor, inserts them into the request's own copy (`Arc::make_mut`), and runs again, so round trips scale with dependency depth rather than key count. At the end `State::merge` learns every key the request read. It keeps the new values if the anchor has not moved; otherwise it drops them, except code, which is content-addressed, and the follower warms those keys at the current anchor, so the next identical call is warm. Gas estimation is reth's bisection as a pure function inside the same loop.

Every batch of keys, whether a request round, a reanchor's pins, or a warm pass, is proven grouped by account: one account request per address, whose proof carries the storage root, then one storage request per wanted slot against that root. A slot of an absent account, or of an account whose storage root is empty, is a known zero and costs no storage request. A bad account reply penalizes its sender and fails every key of that account; a bad storage reply penalizes its sender and fails only its slot.

### Concurrency

| Pair | Outcome |
|---|---|
| Follower advance and RPC read | The read keeps its pinned `Arc<Store>`. It sees one head. |
| Follower advance and a request's merge | Serialized by the mutex. A merge for an old anchor learns its keys and drops its values. |
| Warming and follower advance | Warming runs inside the follower, so its merges never straddle an anchor move. |
| Two requests missing the same key | Both fetch. The second merge re-inserts an equal value. |
| Reanchor and RPC reads | Reads use the old store until the swap. |
| Submit and follower | Both write the state database through `Disk`, always taking the disk lock before the state lock. Submit commits the outbox first and records the tx only if that commit succeeded, then broadcasts. Inclusion is found by a later step. |

The canonical `State` sits behind one `parking_lot::Mutex`, reachable only through `read` and `update`, which take synchronous closures. A closure cannot await, so no guard crosses an await. Exactly one `Follower` exists. It is not `Clone`, and it is the only code that advances or reanchors the store. helios's block channel feeds a latest-only `watch`, so a slow follower never blocks the light client.

### Persistence

The state database is `chibi.db` in the per-network data directory, a SQLite file (rusqlite with the bundled SQLite) with the rollback journal (`journal_mode = DELETE`) and `synchronous = EXTRA`, so a commit is durable when it returns; with a rollback journal SQLite only guarantees that with EXTRA, which also syncs the directory after deleting the journal. A database an earlier build left in WAL mode is switched on the node's open. `PRAGMA user_version` holds the schema version. It has four tables: `snapshot`, one row holding the display snapshot of the watched values and its block number as JSON; `outbox`, one row per pending raw transaction with the block number and hash chibi held at submission, in submission order; `peers`, up to 16 remembered snap/2 peer records in rank order; and, since schema version 2, `keys`, the learned working-set keys most recently used first (`position`, a 20-byte `address`, and a 32-byte `slot` or NULL for an account). Opening a version 1 database adds `keys` and sets version 2 in one transaction; `chibi status` reads either version. Each save commits, in one transaction on a blocking thread, only the tables whose content changed. Pins are never saved: they derive from the config at every start, so a token removed from the config stops being pinned. Learned keys are saved without block numbers, recency being row order, and at start they seed the working set in that order; rows that do not parse are skipped. They are saved when membership changed, at most once per 5 heads; when only recency changed, once the last save is over an hour old; and on clean shutdown. Re-reading a known key never writes by itself. Only peers reached through outbound sessions are remembered, because an inbound session's address is an ephemeral port. `chibi status` opens the database read-only without the directory lock, then prints the snapshot labeled with its block: for each watched address, its primary ENS name, ETH, the nonce, every pinned token (`?` until computed), and every other registry token whose balance computes from held keys and is nonzero, in registry order. The snapshot computes the name like a token balance, by running `reverse` locally over held keys without fetching. There is no name when the address has none, when the run misses a key, and when the UniversalResolver reverts: on a name that does not resolve back to the address, and with `OffchainLookup` for a name served through CCIP-read, because chibi contacts no gateway or other service. Snapshots saved before names load with none. Names are chain data anyone can set, so status omits one containing a control, line-breaking, bidirectional-override, invisible format, or private-use character instead of printing it; the zero-width joiner is allowed for emoji names. With the rollback journal a reader only takes a shared lock on the database file and creates no files, so status works in a read-only directory; WAL was dropped because its readers create `-wal` and `-shm` files. The node and status wait up to 5 s for each other's lock, which a commit every block holds for milliseconds. If a crashed node left a hot journal, status cannot roll it back read-only, so it reports that the node must start once to recover instead of reading partial data. On start, pending outbox transactions are re-broadcast. Earlier versions kept this state in `state.json`: opening a directory with no database imports that file in one transaction and deletes it after the commit, and until then `chibi status` reads it directly. A lock file lets one `chibi run` own the directory, and it holds that process's id for `chibi stop` and `chibi status`. The devp2p key and helios's checkpoint stay plain files.

### Transactions and fees

`rpc.rs` decodes EIP-2718 bytes and rejects blob transactions. `Node::submit` checks the chain id, recovers the sender, persists the outbox, records the transaction with the anchor chibi held at submission, and broadcasts it to up to 8 connected peers. While anything is pending, the follower walks back from the anchor through every block it has not searched yet, down to the oldest pending transaction's submission anchor (at most 8192 blocks, past the header window when needed), fetches and verifies those bodies against `transactions_root`, and matches hashes. On a match it verifies the block's receipts against `receipts_root` and stores the receipt, including for blocks a reanchor skipped. Pending transactions whose nonce was used are pruned only after every block since their submission anchor was searched, so a failed body fetch, a restart, or a long sleep cannot drop a mined transaction. This works with and without BALs. `eth_getTransactionCount(pending)` adds pending outbox transactions to the anchor nonce. Fees use the verified base fee and tips sampled from verified bodies, fetched on demand and cached per block.

### Block tags and unsupported methods

`latest` and `pending` resolve to the anchor. `safe` and `finalized` resolve to the finalized header for block queries. State queries at other blocks, history, and `eth_getLogs` return a JSON-RPC error that names the held head.

### Networks

`node::network` keeps every chain fact in one `NetworkSpec` per `Network` variant: the reth `ChainSpec` (which drives both the devp2p fork id and the EVM fork schedule and blob parameters), the helios network config, the default beacon API URL, boot nodes, pinned snap/2 peers, the ENS UniversalResolver, and the registry of known tokens. The UniversalResolver is `0xeEeEEEeE14D718C2B47D9923Deab1335E144EeEe` on both networks; on Sepolia it fronts ENSv2. The mainnet registry holds the 15 ERC-20s with the most transfer transactions on mainnet in the 30 days to 2026-10-07; the Sepolia registry holds the 9 of them with a Sepolia deployment. `scripts/registry-check.sh` checks every entry's code, symbol, and decimals against each chain, reading the registry through the `registry` example so there is one list. The registry labels tokens in `chibi status` and lists what the optional `tokens` config key accepts; `chibi init` never asks for tokens. `NodeConfig.watch` carries the addresses, the pinned tokens, and the configured contracts. On Sepolia, USDC is Circle's deployment, PYUSD is Paxos's, and LINK is Chainlink's. USDT, DAI, WBTC, and AAVE are the test tokens Aave's Sepolia faucet mints (owner `0xC959483DBa39aa9E78757139af0e9a2EDEb3f42D`, listed as `FAUCET` in Aave's address book); Aave's own Sepolia USDC (`0x94a9D9AC8a22534E3FaCa9F4e7F2E2cf85d5E4C8`) is a different contract from Circle's and is not in the registry. Sepolia is the default. Its beacon default is `https://beacon.sepolia.ethpandaops.io/nimbus`. That host is ethPandaOps' dugtrio load balancer, which pins each client IP to one backend of any client type; the `/nimbus` path selects a Nimbus backend, the only client there that serves fresh Gloas light-client data and bootstraps for recent checkpoints (Teku and Prysm do not serve the light-client API, Lighthouse stopped at the fork, and Lodestar refuses older bootstraps). A beacon endpoint can stall chibi but cannot make it accept a wrong header.

The light-client checkpoint follows one precedence: a user override, else the checkpoint helios saved in the data directory (a file of exactly 32 bytes), else the newest checkpoint from the community checkpoint services, logged as a warning. chibi never passes helios's shipped default, which is older than helios's 14-day limit. helios saves only checkpoints newer than the one it started from, so after a first-run sync chibi saves that checkpoint itself in helios's file format.

At start chibi dials the pinned and remembered snap/2 peers as static peers and waits, with a progress log every 30 seconds, until one snap/2 session is up. It does not give up, because discovery of these rare peers has taken from under a minute to several minutes.

### Inbound requests

The node installs an `EthRequestHandler` channel and answers every `IncomingEthRequest` variant with an empty response of the matching type, through an exhaustive match.

### Errors

Core has `VerifyError` (lying or garbled peer), `StateError` (stale proof, non-child block, unlinked ancestors, full header window), `ExecError`, and `EstimateError`. `NodeError` adds light-client and devp2p failures, disk errors (including a data directory held by another process), `Unprovable` after three attempts, `TooManyMissRounds`, `Unsupported`, and `InvalidTransaction`. `cli::rpc` holds the only mapping from errors to JSON-RPC codes.

### RPC exposure

The server binds to loopback. A `Host` header filter admits only `localhost`, `127.0.0.1`, and `[::1]` by default, which blocks DNS rebinding. Cross-origin browser requests are allowed only from `chrome-extension://` and `moz-extension://` origins by default, so an ordinary website cannot read watched balances. Both lists are configurable.

## Synthesis decision

Three structurally different designs were produced: an actor that owns all state (A), a pure value core with a thin async shell (B), and a redb ledger as the shared state (C). An independent judge scored A 30, B 28, and C 22 and recommended A.

B is the base. A and B were close, and B has the shorter call chains (5 to 6 files and one task for a miss, against 8 files and two tasks), three crates instead of four or seven, and the cheapest first end-to-end slice. A's advantages graft onto B without its actor:

- From A: reopen always reanchors, and persisted values are display-only. The core seam uses chibi reply types instead of reth snap messages. BALs are fetched with `BalRequirement::Optional` so a missing BAL falls back to reanchor instead of hanging, which fixes a judge finding against B.
- From C: the outbox is persisted before broadcast. The concurrency table. Reorg and snap-window faults in the test fixture.
- From the judge: never set helios's `checkpoint` from the shipped default.

Rejected: A's 64-BAL roll-forward window, which adds a second insertion path and carried a judge-found bug where an empty window made "unchanged since the pin" vacuously true. C's redb, which is outside the proven dependency set and holds a read transaction across awaits.

## Tradeoffs accepted

- We accept re-proving the keys of a request cut by a head move at the new anchor, instead of rolling stale facts forward (the roll-forward window was rejected above).
- We accept duplicate fetches when concurrent requests miss the same key, in exchange for no shared in-flight table.
- We accept re-proving pins at every head before Glamsterdam (tens of keys every 12 s), in exchange for one advance path.
- We accept that learned keys are cold after start and after a reanchor until warming reaches them, in exchange for reanchors that no dapp key can block.
- We accept warming inside the follower, so its merges never straddle an anchor move, at the cost of warming that pauses while the follower moves the head.
- We accept discarding BAL post-states for unheld slots, so the store stays bounded and immune to other users' traffic; a dapp re-reading such a slot pays a fetch.
- We accept a constant eviction budget of 1024 keys (pins plus learned, with pins exempt from eviction) and no config knob. Measured on Sepolia, one snap/2 peer proves about 3 keys a second, so re-warming a full set of learned keys takes about 6 minutes.
- We accept warming with half of one peer's 16 snap slots, in exchange for wallet requests and BAL fetches that always find the other half free.
- We accept opening RPC before any proof, so the first reads pay a fetch, in exchange for a wallet that never sees the network as down while chibi proves.
- We accept losing up to 5 heads of newly learned keys, and up to an hour of recency, on a crash, in exchange for not rewriting up to 1024 rows at every head.
- We accept rewriting the snapshot row at each head (a few KB of JSON), in exchange for keeping the display-only snapshot out of the schema. The outbox and peers tables are rewritten only when they change.
- We accept one mutex around the canonical value, in exchange for no actor. It guards only synchronous pure calls.
- We accept receipts only for transactions submitted through chibi.
- We accept that `eth_call` and `eth_estimateGas` with `pending` use the anchor's block environment.

## Alternatives considered

- An actor that owns the store. Every write becomes a message, and late facts need a roll-forward window. It hides the same complexity behind a larger surface and a task to supervise.
- A redb ledger as the shared state. It gives MVCC and atomic commits, but adds a dependency and a schema, and its read transactions would be held across awaits.
- revm's `WrapDatabaseAsync`. It puts awaits inside the EVM, needs fibers and a multi-thread runtime, and costs one serial round trip per miss.
- Per-entry block numbers. Every read would need a freshness check, and one run could mix entries of different ages.
- A per-token storage-layout table, or asking users for storage slot keys. Running `balanceOf` and a self-transfer and pinning the keys they read covers proxies, pause flags, fees, and nonstandard layouts without per-token code. Raw slot keys stay available as an opt-in in `config.toml` (`[[contracts]]`) for contracts outside the token registry; `chibi init` never asks for them.
- Asking users which tokens to keep. Learning keys from requests covers every token a wallet shows without a question; `tokens` remains an opt-in pin.

## Open questions and risks

- Some wallets send state queries tagged `finalized`. Should those serve the anchor instead of an error?
- Should the RPC fail closed when the anchor falls behind the light client because no snap peer is reachable?
- Will Nethermind peers serve BALs over snap/2 after the fork, and will geth and reth peers enable snap/2? Today only Nethermind does on Sepolia, and the five pinned peers are a single point of slowness.
- One failed pin proof aborts a whole reanchor, which keeps the old head published until the next step. Is that acceptable with one or two snap/2 peers?
- The first run trusts the community checkpoint services for weak subjectivity unless the user sets `checkpoint`. Should a release ship a recent checkpoint instead?
- Does MetaMask's background page send an `Origin` header to a custom RPC? If it does not, the CORS allowlist never applies to it.

## Next implementation step

After Sepolia's Glamsterdam fork on 2026-10-06 13:53:36 UTC, run `chibi run` and confirm heads log `Stepped::Extended` with BALs, then send a funded transaction from a wallet pointed at chibi and confirm its receipt.
