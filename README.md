# chibi

chibi is a local Ethereum JSON-RPC endpoint for a browser wallet. Point MetaMask or Rabby at it, and the balances, call results, and gas estimates the wallet shows come from state chibi proved itself, not from an RPC provider. chibi holds no keys. The wallet signs, and chibi simulates, broadcasts, and confirms.

chibi is early software. It targets Sepolia first. Mainnet support exists in the code but is untested.

## How it works

- A sync-committee light client ([helios](https://github.com/a16z/helios)) vouches for execution block hashes. chibi fetches each header over devp2p and checks its hash.
- Account and storage values come from single-key snap/2 range proofs, checked against the verified header's state root. A key chibi never fetched is never read as zero. Only state peers, the devp2p peers that speak snap/2, can serve these proofs (see [Peers and keys](#peers-and-keys)).
- chibi keeps only the state your wallet and dapps read (see [What chibi keeps](#what-chibi-keeps)).
- After the Glamsterdam upgrade, each block's access list (EIP-7928) keeps those keys current. Before it, chibi proves the pinned keys again at every new head and re-proves the learned keys in the background.
- `eth_call` and `eth_estimateGas` run in a local EVM. A missing key is fetched with a proof and the call runs again.
- Signed transactions go straight to devp2p peers. chibi finds them in verified block bodies and checks their receipts against the block's receipts root.

The design and its tradeoffs are in [docs/DESIGN.md](docs/DESIGN.md). The decision trail for the first build is [docs/decisions.tsv](docs/decisions.tsv).

## What chibi keeps

chibi remembers the state your wallet and dapps read, as accounts and storage slots. You don't pick tokens. Watched addresses, plus any `[[contracts]]` in the config, are pinned: chibi proves them at start, keeps them current, and never drops them. For every watched address chibi also pins every token in the network's [token list](#token-list): it runs `balanceOf` and a self-transfer for each and pins the keys they read, so the balances are proven at start, before the wallet asks. A pinned key no state peer can prove at a new head never holds the head back; chibi moves on and keeps retrying it. Everything else a request reads is learned and kept current at every head. Once pinned and learned keys together pass 1024, the least recently used learned key is dropped. Pinned keys never count against that: a pinned set larger than 1024 is kept whole, and then nothing else is learned.

Learned keys survive restarts: chibi saves them in `chibi.db`, most recently used first, and after a restart proves them again in that order in the background. Pinned keys are not saved; they come from the config and the token list at every start.

JSON-RPC opens as soon as the head is verified. Pinned and learned keys are proven in the background, and anything not yet proven is fetched with a proof on first use.

## Peers and keys

A peer is any devp2p node chibi is connected to. A state peer is a peer that speaks snap/2, the protocol that serves the account and storage proofs chibi needs. Most Sepolia nodes do not, so chibi can have many peers and no state peer; without one, the head and state requests stall.

Pinned keys come from the config: each watched address's account and its token balance slots, the keys ENS lookups read, and the slots of `[[contracts]]`. chibi proves them at start, keeps them current at every head, and never evicts them. Learned keys are the accounts and slots your wallet's requests read. chibi keeps them current too, but the 1024 budget counts pinned and learned keys together: while the total is over 1024, the least recently used learned key is evicted. Pinned keys are never evicted, so a large pinned set leaves less room for learned keys.

A key is held when it is proven at the current head. The status line counts each group as held/total, such as `pinned=3/3 learned=41/42`: 41 of the 42 learned keys are proven at the head, and one is not, because its proof failed, it is waiting for a retry, or it is not yet warmed.

## Requirements

- Rust (the toolchain is pinned in `rust-toolchain.toml` and installed by rustup on first build).
- Outbound access to devp2p peers (TCP and UDP) and to a beacon API endpoint. The Sepolia default is `https://beacon.sepolia.ethpandaops.io/nimbus`.

## Install and set up

1. Install: `./install.sh` builds and installs the `chibi` binary with cargo and, on a terminal, runs `chibi init`.

   ```sh
   git clone https://github.com/weiihann/chibi
   cd chibi
   ./install.sh
   ```

2. `chibi init` asks for the network (Sepolia by default), the watch-only addresses to track (paste as many as you like and press Enter on an empty line to finish; none is fine), and the RPC host (an IP address or a domain your wallet uses to reach chibi, `127.0.0.1` by default) and port (`8545` by default), then writes the config. Every question has a default, so pressing Enter through them is fine.
3. `chibi run` starts chibi in the foreground (`chibi run -b` in the background).
4. When the log shows `JSON-RPC on http://127.0.0.1:8545, chain id 11155111`, add a custom network in your wallet with that URL and chain id.

`chibi run` also works with no config: it uses Sepolia, JSON-RPC on `127.0.0.1:8545`, no pinned keys, and the default data directory, and learns the accounts and storage slots your wallet reads (see [What chibi keeps](#what-chibi-keeps)). `chibi run -b`, `status`, and `stop` work without one too; `chibi run` logs that it found no config and where `chibi init` would write one.

`chibi init` writes `config.toml` to `$XDG_CONFIG_HOME/chibi/` when `XDG_CONFIG_HOME` is set to an absolute path, otherwise to `~/.config/chibi/`, on macOS and Linux alike. In a terminal the questions are menus; elsewhere they are plain prompts. Flags skip the questions:

```sh
chibi init --network sepolia --address 0xYourAddress --rpc-host 127.0.0.1 --rpc-port 8545
```

`--beacon-url URL` and `--data-dir PATH` write `beacon_url` and `data_dir` into the config, and the peer flags write their keys (see [Configuration](#configuration) and [Peers](#peers)). When `chibi init` overwrites an existing config, it keeps that config's `[[contracts]]` and, unless `--data-dir` is given, its `data_dir`, and says so.

Writing a config resets the chosen network's saved state, since the old snapshot, pending transactions, and learned keys belong to the old config: once the config is written, `chibi init` deletes them from that network's `chibi.db` and prints what it deleted. If the config cannot be written, nothing is deleted. It keeps the remembered state peers, so the next start finds one fast, along with the light-client checkpoint and the devp2p key. It refuses, writing nothing, while a chibi runs on that data directory; stop it first. While init writes, it holds that data directory, so a chibi started meanwhile refuses to start. Without a database there is nothing to reset and it says nothing.

A config file that exists must parse: `chibi run` refuses a malformed one rather than falling back to the defaults. A config with a `tokens` line, from before every token was kept for watched addresses, fails with that reason; delete the line or rerun `chibi init`.

Every command takes `--config PATH`, before or after the subcommand, to use another config file. The `CHIBI_CONFIG` environment variable does the same when the flag is absent; an empty `CHIBI_CONFIG` counts as unset. A config named either way must exist: every command but `chibi init` fails, naming the path, rather than falling back to the defaults:

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

chibi then waits for a state peer (snap/2), logging `waiting for a state peer (snap/2)` with the peer and state peer counts every 30 seconds. On Sepolia this has taken from a few seconds to several minutes, because few peers serve snap/2 (on 2026-10-05 only Nethermind nodes did). chibi dials a short list of known state peers and the outbound state peers it reached on earlier runs first. When the first one connects, chibi prints a small cat and `found a state peer` to standard output (with `run -b`, into `chibi.log`). The JSON-RPC server opens only after the first verified head.

When the log shows `JSON-RPC on http://127.0.0.1:8545, chain id 11155111`, add a custom network in your wallet with that URL and chain id (with a config, the URL uses its `rpc_host` and `rpc_port`).

Once serving, chibi logs a `status` line every minute, such as `status head=9372110 peers=12 state_peers=2 pinned=3/3 learned=41/42`. It gives the verified head, the connected peers and state peers, and how many pinned and learned keys the head holds out of how many there are (see [Peers and keys](#peers-and-keys)). After a reanchor, learned keys are proven again in the background, so `learned` can briefly lag. A pinned key no state peer can prove at a new head never holds the head back: chibi moves to the head without it and keeps retrying it, so `pinned` can lag too. The warning `no state peer (snap/2) connected` repeats while no state peer is connected, and another names up to five pinned keys the head has lacked for two status lines in a row; a key still warming right after start is not reported.

`chibi status` reads the snapshot saved at the last verified head, so it works without a running node. For each watched address it shows the address's primary ENS name, ETH, the nonce, and every listed token whose balance chibi can compute from the keys it holds and is not zero, in list order. Every listed token is pinned for each watched address, so its balance shows once the first head is proven. Those values are labeled with their block and are not re-verified.

The ENS name comes from the chain's ENS UniversalResolver, run by chibi itself over proven state, and is shown only when the name also resolves back to the address. A name served offchain through CCIP-read is not shown, because chibi contacts no gateway or other service. A name containing control or other invisible characters is not printed.

### Token list

The token list is the 15 ERC-20s with the most transfers on mainnet in the 30 days to 2026-10-07. Mainnet has all 15: USDT, USDC, WETH, WBTC, UNI, LINK, QNT, DAI, PYUSD, ONDO, AAVE, PEPE, SHIB, 1INCH, and CRV. Sepolia has the 9 with a Sepolia deployment: USDT, USDC, WETH, WBTC, UNI, LINK, DAI, PYUSD, and AAVE. Sepolia's USDC, PYUSD, and LINK are their issuers' own deployments. Sepolia's USDT, DAI, WBTC, and AAVE are test tokens from Aave's Sepolia faucet, not tokens issued by Tether, Maker, BitGo, or Aave; anyone can mint them, so their balances carry no value.

## Configuration

`config.toml` fields:

| Field | Default | Meaning |
|---|---|---|
| `network` | `sepolia` | `sepolia` or `mainnet` |
| `addresses` | none | Watch-only addresses, pinned with their balance of every listed token |
| `beacon_url` | Sepolia `https://beacon.sepolia.ethpandaops.io/nimbus`, mainnet `https://ethereum-beacon-api.publicnode.com` | Beacon (consensus) API the light client reads sync-committee updates from |
| `checkpoint` | none | Finalized beacon block root to start the light client from |
| `rpc_host` | `127.0.0.1` | IP address or domain JSON-RPC listens on; also accepted as a `Host` header |
| `rpc_port` | `8545` | JSON-RPC port |
| `cors_origins` | `chrome-extension://*`, `moz-extension://*` | Browser origins allowed to call the RPC |
| `allowed_hosts` | `localhost`, `127.0.0.1`, `[::1]` | Accepted `Host` headers, which blocks DNS rebinding |
| `p2p_port` | `0` (OS picks) | devp2p listen port |
| `data_dir` | XDG data directory | Absolute directory that holds the per-network data directories |
| `contracts` | none | Contract accounts and storage slots to pin, see below |

The peer keys are under [Peers](#peers).

`chibi run --beacon-url URL --data-dir PATH` overrides those keys for one run, as `--network`, `--rpc-host`, `--rpc-port`, and the peer flags do; `chibi init` writes the same flags into the config. A relative `--data-dir` is taken from the working directory. `chibi status` and `chibi stop` take `--data-dir` too, to find a chibi started with it.

Data (light-client checkpoint, devp2p key, `chibi.db` with the saved snapshot, pending transactions, remembered state peers, and learned keys, and `chibi.log` for a background chibi) lives in `<data root>/<network>/`. The data root is `data_dir` when set, else `$XDG_DATA_HOME/chibi` when `XDG_DATA_HOME` is set to an absolute path, else `~/.local/share/chibi`. A `state.json` left by an earlier chibi is imported into `chibi.db` on the next `chibi run` and then deleted. One `chibi run` owns a network's data directory at a time, and `chibi.lock` holds its process id, so two configs on the same network need different `data_dir`s to run side by side. `chibi run -b`, `stop`, and `status` all find the directory through the config, so pass them the same `--config` and `--data-dir`.

### Peers

chibi reads state from state peers: devp2p peers that speak snap/2 and so can serve proofs. By default it finds peers through discovery, starting from the network's boot nodes, and also dials the network's built-in state peers (Sepolia has five; mainnet has none) and the state peers it reached on earlier runs. These keys change that. Each has a flag: `chibi init` writes it into the config, and `chibi run` uses it for that run only.

| Field | Flag | Default | Meaning |
|---|---|---|---|
| `trusted_peers` | `--trusted-peer ENODE`, repeatable | none | `enode://` URLs chibi always dials and keeps connected, besides the built-in state peers |
| `trusted_only` | `--trusted-only` | `false` | Connect to `trusted_peers` only: no built-in, remembered, discovered, or inbound peer. Needs at least one trusted peer |
| `boot_nodes` | `--boot-node ENODE`, repeatable | the network's | Discovery boot nodes, replacing the network's |
| `max_outbound_peers` | `--max-outbound-peers N` | `100` | Most outbound peer sessions, at least 1 |
| `max_inbound_peers` | `--max-inbound-peers N` | `30` | Most inbound peer sessions |
| `discovery_port` | `--discovery-port PORT` | `p2p_port` | UDP discovery port, when it must differ from `p2p_port` |
| `disable_discovery` | `--disable-discovery` | `false` | Run no discovery; dial only the trusted and built-in state peers. Refused when that leaves nothing to dial |

A malformed enode fails the config naming its entry, such as `trusted_peers[1]`. Run flags replace the configured list rather than adding to it.

For reliability and privacy, run your own geth or Nethermind node with snap serving enabled and put its enode in `trusted_peers`. chibi then always has a state peer, without waiting for discovery, and with `trusted_only = true` it talks to no other peer, so only your node sees which accounts you query. chibi still checks every proof against the light client, so a trusted peer is trusted to stay connected, not to tell the truth. With `trusted_only`, chibi waits while your node is down.

```toml
trusted_peers = ["enode://<128 hex digits>@192.168.1.10:30303"]
trusted_only = true
```

### Pinning contracts and storage slots

Each `[[contracts]]` table pins a contract account (balance, nonce, and code hash) and, optionally, raw storage slots. chibi keeps them proven at every new head, from block access lists or by proving them again, so `eth_getBalance`, `eth_getTransactionCount`, and `eth_getStorageAt` for them answer without a fetch once proven. The contract's code is fetched once, on first use, and kept from then on. `chibi status` prints each slot's value at the saved head.

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
- If every state peer disconnects, chibi keeps serving its last verified head as `latest` and logs `step failed` (`no connected peer serves state (snap/2)`) until one returns. `eth_syncing` still reports `false`.
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
