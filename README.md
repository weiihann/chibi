# chibi

chibi is a local Ethereum JSON-RPC endpoint for a browser wallet. Point MetaMask or Rabby at it, and the balances, call results, and gas estimates the wallet shows come from state chibi proved itself, not from an RPC provider. chibi holds no keys. The wallet signs, and chibi simulates, broadcasts, and confirms.

chibi is early software. It targets Sepolia first. Mainnet support exists in the code but is untested.

## How it works

- A sync-committee light client ([helios](https://github.com/a16z/helios)) vouches for execution block hashes. chibi fetches each header over devp2p and checks its hash.
- Account and storage values come from single-key snap/2 range proofs, checked against the verified header's state root. A key chibi never fetched is never read as zero.
- chibi stores only the keys you use: your watched accounts, the USDC storage your balances need, and whatever your dapps' calls touch.
- After the Glamsterdam upgrade, each block's access list (EIP-7928) keeps those keys current. Before it, chibi proves the tracked keys again at every new head.
- `eth_call` and `eth_estimateGas` run in a local EVM. A missing key is fetched with a proof and the call runs again.
- Signed transactions go straight to devp2p peers. chibi finds them in verified block bodies and checks their receipts against the block's receipts root.

The design and its tradeoffs are in [docs/DESIGN.md](docs/DESIGN.md). The decision trail for the first build is [docs/decisions.tsv](docs/decisions.tsv).

## Requirements

- Rust (the toolchain is pinned in `rust-toolchain.toml` and installed by rustup on first build).
- Outbound access to devp2p peers (TCP and UDP) and to a beacon API endpoint. The Sepolia default is `https://ethereum-sepolia-beacon-api.publicnode.com`.

## Install and set up

```sh
git clone <this repository> chibi
cd chibi
./install.sh
```

`install.sh` builds and installs the `chibi` binary with cargo, runs `chibi init`, and then starts `chibi run`.

`chibi init` asks for the network (Sepolia by default) and for watch-only addresses. Paste as many as you like and leave the line blank to finish. It writes `config.toml` in your OS config directory (`~/Library/Application Support/chibi/` on macOS, `~/.config/chibi/` on Linux). You can pass everything as flags instead:

```sh
chibi init --network sepolia --address 0xYourAddress --address 0xAnotherAddress
```

## Run

```sh
chibi run
```

On first start chibi needs a weak-subjectivity checkpoint for the light client. Without one in the config, it takes the newest checkpoint from public checkpoint services and logs a warning. Later starts reuse the checkpoint the light client saved.

chibi then waits for a devp2p peer that serves snap/2. On Sepolia this has taken from about one to several minutes, because few peers serve it. chibi remembers the snap/2 peers it reached, so later starts connect faster.

When the log shows `JSON-RPC on http://127.0.0.1:8545, chain id 11155111`, add a custom network in your wallet with that URL and chain id.

`chibi status` prints the balances saved at the last verified head without starting a node. Those values are labeled with their block and are not re-verified.

## Configuration

`config.toml` fields:

| Field | Default | Meaning |
|---|---|---|
| `network` | `sepolia` | `sepolia` or `mainnet` |
| `addresses` | none | Watch-only addresses to track |
| `beacon_url` | network default | Beacon API endpoint for the light client |
| `checkpoint` | none | Finalized beacon block root to start the light client from |
| `rpc_addr` | `127.0.0.1:8545` | Where JSON-RPC listens |
| `cors_origins` | `chrome-extension://*`, `moz-extension://*` | Browser origins allowed to call the RPC |
| `allowed_hosts` | `localhost`, `127.0.0.1`, `[::1]` | Accepted `Host` headers, which blocks DNS rebinding |
| `p2p_port` | `0` (OS picks) | devp2p listen port |

Data (light-client checkpoint, devp2p key, remembered peers, and the state file) lives in the OS data directory under `chibi/<network>/`. One `chibi run` owns that directory at a time.

## JSON-RPC methods

Supported: `web3_clientVersion`, `net_version`, `eth_chainId`, `eth_blockNumber`, `eth_syncing`, `eth_accounts` (always empty), `eth_getBlockByNumber`, `eth_getBlockByHash`, `eth_getBalance`, `eth_getTransactionCount`, `eth_getCode`, `eth_getStorageAt`, `eth_call`, `eth_estimateGas`, `eth_gasPrice`, `eth_maxPriorityFeePerGas`, `eth_feeHistory`, `eth_sendRawTransaction`, `eth_getTransactionReceipt`, `eth_getTransactionByHash`.

Limits:

- State is served only at the verified head (`latest` and `pending`). Older blocks, `eth_getLogs`, and filters return error `-32004`.
- Block responses carry header fields and an empty transaction list.
- Receipts and transaction lookups cover only transactions sent through chibi.
- Blob transactions are rejected.

## Trust model

A peer that lies can delay chibi but cannot make it serve a wrong value, because every value is checked against a header the light client attested. The light client trusts the sync committee. The beacon API endpoint and the checkpoint services can stall chibi, and on first run a checkpoint service chooses the starting checkpoint unless you set `checkpoint`. Proof requests show peers which accounts and contracts you query.

## Development

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo run -p chibi-node --example live_probe   # live Sepolia: verified header and snap/2 replies
cargo run -p chibi-node --example live_node -- 0xAddress   # live Sepolia: a full node start
```

The workspace has three crates. `chibi-core` holds proof verification, the known-key store, BAL application, and local execution, with no I/O. `chibi-node` holds the light client, devp2p, the head follower, and the request-side node API. `chibi` is the binary and its JSON-RPC server.
