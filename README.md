# chibi

chibi is a local Ethereum JSON-RPC endpoint for a browser wallet. Point MetaMask or Rabby at it, and the balances, call results, and gas estimates the wallet shows come from state chibi proved itself, not from an RPC provider. chibi holds no keys. The wallet signs, and chibi simulates, broadcasts, and confirms.

chibi is early software. It targets Sepolia first. Mainnet support exists in the code but is untested.

## How it works

- A sync-committee light client ([helios](https://github.com/a16z/helios)) vouches for execution block hashes. chibi fetches each header over devp2p and checks its hash.
- Account and storage values come from single-key snap/2 range proofs, checked against the verified header's state root. A key chibi never fetched is never read as zero.
- chibi stores only the keys you use: your watched accounts, the storage of the tokens you picked (balances plus anything a transfer checks, such as pause flags or fees), any contracts and storage slots you list in the config, and whatever your dapps' calls touch.
- After the Glamsterdam upgrade, each block's access list (EIP-7928) keeps those keys current. Before it, chibi proves the tracked keys again at every new head.
- `eth_call` and `eth_estimateGas` run in a local EVM. A missing key is fetched with a proof and the call runs again.
- Signed transactions go straight to devp2p peers. chibi finds them in verified block bodies and checks their receipts against the block's receipts root.

The design and its tradeoffs are in [docs/DESIGN.md](docs/DESIGN.md). The decision trail for the first build is [docs/decisions.tsv](docs/decisions.tsv).

## Requirements

- Rust (the toolchain is pinned in `rust-toolchain.toml` and installed by rustup on first build).
- Outbound access to devp2p peers (TCP and UDP) and to a beacon API endpoint. The Sepolia default is `https://ethereum-sepolia-beacon-api.publicnode.com`.

## Install and set up

```sh
git clone https://github.com/weiihann/chibi
cd chibi
./install.sh
```

`install.sh` builds and installs the `chibi` binary with cargo and runs `chibi init`. It does not start chibi.

`chibi init` asks, in order:

1. The network (Sepolia by default).
2. Watch-only addresses. Paste as many as you like and press Enter on an empty line to finish.
3. Which tokens to track, as tick boxes. On Sepolia the choices are USDC, USDT, DAI, WETH, and LINK; mainnet adds WBTC. USDC is ticked by default. ETH is always tracked. Sepolia's USDC is Circle's own deployment. Sepolia's USDT and DAI are test tokens from Aave's Sepolia faucet, not tokens issued by Tether or Maker; anyone can mint them, so their balances carry no value.
4. The RPC host (an IP address or a domain your wallet uses to reach chibi, `127.0.0.1` by default) and port (`8545` by default).

It writes `config.toml` to `$XDG_CONFIG_HOME/chibi/` when `XDG_CONFIG_HOME` is set to an absolute path, otherwise to `~/.config/chibi/`, on macOS and Linux alike. In a terminal the questions are menus; elsewhere they are plain prompts. Flags skip the questions:

```sh
chibi init --network sepolia --address 0xYourAddress --token USDC --token DAI --rpc-host 127.0.0.1 --rpc-port 8545
```

`--data-dir PATH` writes a `data_dir` into the config (see below).

Every command takes `--config PATH`, before or after the subcommand, to use another config file. The `CHIBI_CONFIG` environment variable does the same when the flag is absent:

```sh
chibi --config ~/chibi-test.toml init
CHIBI_CONFIG=~/chibi-test.toml chibi run -b
chibi status --config ~/chibi-test.toml
```

## Run

```sh
chibi run        # in the foreground; Ctrl-C stops it
chibi run -b     # in the background; logs go to the data directory
chibi status     # whether it runs, and the balances saved at the last verified head
chibi stop       # stops a background chibi
```

On first start chibi needs a weak-subjectivity checkpoint for the light client. Without one in the config, it takes the newest checkpoint from public checkpoint services and logs a warning. Later starts reuse the checkpoint the light client saved.

chibi then waits for a devp2p peer that serves snap/2, logging progress every 30 seconds. On Sepolia this has taken from a few seconds to several minutes, because few peers serve it (on 2026-10-05 only Nethermind nodes did). chibi dials a short list of known snap/2 peers and the outbound snap/2 peers it reached on earlier runs first. The JSON-RPC server opens only after the first verified head.

When the log shows `JSON-RPC on http://127.0.0.1:8545, chain id 11155111`, add a custom network in your wallet with that URL and chain id.

`chibi status` reads the snapshot saved at the last verified head, so it works without a running node. Those values are labeled with their block and are not re-verified.

## Configuration

`config.toml` fields:

| Field | Default | Meaning |
|---|---|---|
| `network` | `sepolia` | `sepolia` or `mainnet` |
| `addresses` | none | Watch-only addresses to track |
| `beacon_url` | network default | Beacon API endpoint for the light client |
| `checkpoint` | none | Finalized beacon block root to start the light client from |
| `tokens` | `["USDC"]` | Tokens to track, by symbol, from the network's list |
| `rpc_host` | `127.0.0.1` | IP address or domain JSON-RPC listens on; also accepted as a `Host` header |
| `rpc_port` | `8545` | JSON-RPC port |
| `cors_origins` | `chrome-extension://*`, `moz-extension://*` | Browser origins allowed to call the RPC |
| `allowed_hosts` | `localhost`, `127.0.0.1`, `[::1]` | Accepted `Host` headers, which blocks DNS rebinding |
| `p2p_port` | `0` (OS picks) | devp2p listen port |
| `data_dir` | XDG data directory | Absolute directory that holds the per-network data directories |
| `contracts` | none | Contract accounts and storage slots to track, see below |

Data (light-client checkpoint, devp2p key, remembered peers, the state file, and `chibi.log` for a background chibi) lives in `<data root>/<network>/`. The data root is `data_dir` when set, else `$XDG_DATA_HOME/chibi` when `XDG_DATA_HOME` is set to an absolute path, else `~/.local/share/chibi`. One `chibi run` owns a network's data directory at a time, and `chibi.lock` holds its process id, so two configs on the same network need different `data_dir`s to run side by side. `chibi run -b`, `stop`, and `status` all find the directory through the config, so pass them the same `--config`.

### Tracking contracts and storage slots

Each `[[contracts]]` table tracks a contract account (balance, nonce, and code) and, optionally, raw storage slots. chibi proves them at every new head, or keeps them current from block access lists, so `eth_getBalance`, `eth_getCode`, and `eth_getStorageAt` for them answer without a fetch. `chibi status` prints each slot's value at the saved head.

```toml
[[contracts]]
address = "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"
slots = ["0x7050c9e0f4ca769c69bd3a8ef740bc37934f8e2c036e5a723fd8ee048ed3f8c3", "0x1"]

[[contracts]]
address = "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14"   # account only
```

A slot is the storage key exactly as `eth_getStorageAt` takes it: `0x` and up to 64 hex digits, so `0x1` is slot 1. It is not hashed again. For a mapping entry, derive the key with `cast index <key type> <key> <slot>`, for example `cast index address 0xYourAddress <balances slot>`. Entries with the same address are merged.

## JSON-RPC methods

Supported: `web3_clientVersion`, `net_version`, `eth_chainId`, `eth_blockNumber`, `eth_syncing`, `eth_accounts` (always empty), `eth_getBlockByNumber`, `eth_getBlockByHash`, `eth_getBalance`, `eth_getTransactionCount`, `eth_getCode`, `eth_getStorageAt`, `eth_call`, `eth_estimateGas`, `eth_gasPrice`, `eth_maxPriorityFeePerGas`, `eth_feeHistory`, `eth_sendRawTransaction`, `eth_getTransactionReceipt`, `eth_getTransactionByHash`.

Limits:

- State is served only at the verified head (`latest` and `pending`). Older blocks, `eth_getLogs`, and filters return error `-32004`.
- Block responses carry header fields and an empty transaction list.
- If every snap/2 peer disconnects, chibi keeps serving its last verified head as `latest` and logs `step failed` until a peer returns. `eth_syncing` still reports `false`.
- Receipts and transaction lookups cover only transactions sent through chibi.
- Blob transactions are rejected.

## Trust model

A peer that lies can delay chibi but cannot make it serve a wrong value, because every value is checked against a header the light client attested. The light client trusts the sync committee. The beacon API endpoint and the checkpoint services can stall chibi. On first run a checkpoint service chooses the starting checkpoint unless you set `checkpoint`, and later runs continue from the checkpoints that one leads to. Proof requests show peers which accounts and contracts you query.

## Development

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo run -p chibi-node --example live_probe   # live Sepolia: verified header and snap/2 replies
cargo run -p chibi-node --example live_node -- 0xAddress   # live Sepolia: a full node start
scripts/oracle-check.sh 0xAddress   # compare a running chibi with a public RPC at chibi's head
```

The workspace has three crates. `chibi-core` holds proof verification, the known-key store, BAL application, and local execution, with no I/O. `chibi-node` holds the light client, devp2p, the head follower, and the request-side node API. `chibi` is the binary and its JSON-RPC server.
