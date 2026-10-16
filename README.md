# chibi

chibi is a local Ethereum JSON-RPC endpoint for a browser wallet. Point MetaMask or Rabby at it, and the balances, call results, and gas estimates the wallet shows come from state chibi proved itself, not from an RPC provider. Over JSON-RPC chibi holds no keys. The wallet signs, and chibi simulates, broadcasts, and confirms. The one exception is the optional terminal UI, [`chibi wallet`](#send-from-the-terminal-chibi-wallet), which unlocks your keystore, or helps you create or import one, and sends from it.

chibi is early software. It targets Sepolia first. Mainnet support exists in the code but is untested.

## How it works

- A sync-committee light client ([helios](https://github.com/a16z/helios)) vouches for execution block hashes. chibi fetches each header over devp2p and checks its hash.
- Account and storage values come from single-key snap range proofs, checked against the verified header's state root. A key chibi never fetched is never read as zero. Only state peers, the devp2p peers that speak snap/1 or snap/2, can serve these proofs (see [Peers and keys](#peers-and-keys)).
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

A peer is any devp2p node chibi is connected to. A state peer is a peer that speaks snap/1 or snap/2, the protocols that serve the account and storage proofs chibi needs. Many Sepolia nodes speak neither, so chibi can have many peers and no state peer; without one, the head and state requests stall. Block access lists come from snap/2 peers or eth/71 peers, never over snap/1.

Pinned keys come from the config: each watched address's account and the balance keys of every token in the [token list](#token-list), the keys ENS lookups read, and the slots of `[[contracts]]`. chibi proves them at start, keeps them current, and never evicts them; one that no state peer can prove at a new head stays unheld and is retried, without holding the head back. Learned keys are the accounts and slots your wallet's requests read. chibi keeps them current too, but the 1024 budget counts pinned and learned keys together: while the total is over 1024, the least recently used learned key is evicted. Pinned keys are never evicted, so a large pinned set leaves less room for learned keys.

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
3. `chibi node` starts chibi in the foreground (`chibi node -b` in the background).
4. When the log shows `JSON-RPC on http://127.0.0.1:8545, chain id 11155111`, add a custom network in your wallet with that URL and chain id.

`chibi node` also works with no config: it uses Sepolia, JSON-RPC on `127.0.0.1:8545`, no pinned keys, and the default data directory, and learns the accounts and storage slots your wallet reads (see [What chibi keeps](#what-chibi-keeps)). `chibi node -b`, `status`, and `stop` work without one too; `chibi node` logs that it found no config and where `chibi init` would write one.

`chibi init` writes `config.toml` to `$XDG_CONFIG_HOME/chibi/` when `XDG_CONFIG_HOME` is set to an absolute path, otherwise to `~/.config/chibi/`, on macOS and Linux alike. In a terminal the questions are menus; elsewhere they are plain prompts. Flags skip the questions:

```sh
chibi init --network sepolia --address 0xYourAddress --rpc-host 127.0.0.1 --rpc-port 8545
```

`--beacon-url URL`, `--data-dir PATH`, and `--keystore PATH` write `beacon_url`, `data_dir`, and `keystore` into the config, and the peer flags write their keys (see [Configuration](#configuration) and [Peers](#peers)). When `chibi init` overwrites an existing config, it keeps that config's `[[contracts]]` and, unless `--data-dir` is given, its `data_dir`, and says so.

Writing a config resets the chosen network's saved state, since the old snapshot, pending transactions, and learned keys belong to the old config: once the config is written, `chibi init` deletes them from that network's `chibi.db` and prints what it deleted. If the config cannot be written, nothing is deleted. It keeps the remembered state peers, so the next start finds one fast, along with the light-client checkpoint and the devp2p key. It refuses, writing nothing, while a chibi runs on that data directory; stop it first. While init writes, it holds that data directory, so a chibi started meanwhile refuses to start. Without a database there is nothing to reset and it says nothing.

A config file that exists must parse: `chibi node` refuses a malformed one rather than falling back to the defaults. A config with a `tokens` line, from before every token was kept for watched addresses, fails with that reason; delete the line or rerun `chibi init`.

Every command takes `--config PATH`, before or after the subcommand, to use another config file. The `CHIBI_CONFIG` environment variable does the same when the flag is absent; an empty `CHIBI_CONFIG` counts as unset. A config named either way must exist: every command but `chibi init` fails, naming the path, rather than falling back to the defaults:

```sh
chibi --config ~/chibi-test.toml init
CHIBI_CONFIG=~/chibi-test.toml chibi node -b
chibi status --config ~/chibi-test.toml
```

## Run

```sh
chibi node       # in the foreground; Ctrl-C stops it
chibi node -b    # in the background; logs go to the data directory
chibi wallet     # with a terminal UI that sends from your keystore, see below
chibi status     # whether it runs, and the balances saved at the last verified head
chibi stop       # stops a background chibi
```

On first start chibi needs a weak-subjectivity checkpoint for the light client. Without one in the config, it takes the newest checkpoint from public checkpoint services and logs a warning. Later starts reuse the checkpoint the light client saved.

chibi then waits for a state peer (snap), logging `waiting for a state peer (snap)` with the peer and state peer counts every 30 seconds. On Sepolia this took from a few seconds to several minutes while chibi spoke only snap/2, which only Nethermind nodes served (2026-10-05). On 2026-10-16, 10 of 17 Sepolia peers were state peers: 6 Nethermind nodes announcing snap/1 and snap/2, and 4 Geth nodes announcing snap/1 only. chibi dials a short list of known state peers and the outbound state peers it reached on earlier runs first. When the first one connects, chibi prints a small cat and `found a state peer` to standard output (with `node -b`, into `chibi.log`). The JSON-RPC server opens only after the first verified head.

When the log shows `JSON-RPC on http://127.0.0.1:8545, chain id 11155111`, add a custom network in your wallet with that URL and chain id (with a config, the URL uses its `rpc_host` and `rpc_port`).

Once serving, chibi logs a `status` line every minute, such as `status head=9372110 peers=12 state_peers=2 pinned=3/3 learned=41/42`. It gives the verified head, the connected peers and state peers, and how many pinned and learned keys the head holds out of how many there are (see [Peers and keys](#peers-and-keys)). After a reanchor, learned keys are proven again in the background, so `learned` can briefly lag. A pinned key no state peer can prove at a new head never holds the head back: chibi moves to the head without it and keeps retrying it, so `pinned` can lag too. The warning `no state peer (snap) connected` repeats while no state peer is connected, and another names up to five pinned keys the head has lacked for two status lines in a row; a key still warming right after start is not reported.

`chibi status` reads the snapshot saved at the last verified head, so it works without a running node. For each watched address it shows the address's primary ENS name, ETH, the nonce, and every listed token whose balance chibi can compute from the keys it holds and is not zero, in list order. Every listed token is pinned for each watched address, so its balance shows once the first head is proven. Those values are labeled with their block and are not re-verified.

The ENS name comes from the chain's ENS UniversalResolver, run by chibi itself over proven state, and is shown only when the name also resolves back to the address. A name served offchain through CCIP-read is not shown, because chibi contacts no gateway or other service. A name containing control or other invisible characters is not printed.

### Token list

The token list is the 15 ERC-20s with the most transfers on mainnet in the 30 days to 2026-10-07. Mainnet has all 15: USDT, USDC, WETH, WBTC, UNI, LINK, QNT, DAI, PYUSD, ONDO, AAVE, PEPE, SHIB, 1INCH, and CRV. Sepolia has the 9 with a Sepolia deployment: USDT, USDC, WETH, WBTC, UNI, LINK, DAI, PYUSD, and AAVE. Sepolia's USDC, PYUSD, and LINK are their issuers' own deployments. Sepolia's USDT, DAI, WBTC, and AAVE are test tokens from Aave's Sepolia faucet, not tokens issued by Tether, Maker, BitGo, or Aave; anyone can mint them, so their balances carry no value.

## Send from the terminal (`chibi wallet`)

`chibi wallet` runs chibi in a terminal UI that sends ETH or a token from the [token list](#token-list) from your own keystore, while JSON-RPC keeps serving your wallet as `chibi node` does. It is the only mode in which chibi holds a key: the keystore is decrypted into memory when you type its password, and the key is never logged, and never written except encrypted into a keystore by the setup below.

The first time you run `chibi wallet` without a keystore (no `--keystore`, no `keystore` in the config), it walks you through setting one up. Up and Down choose, Enter selects, Esc goes back, and Esc on the menu quits:

- **Create a new wallet.** chibi shows a new 12-word recovery phrase, numbered. Write it down: anyone with these words controls the wallet, and chibi will not show them again. Then type the two words it asks for, by position, to show you have them. The phrase is standard BIP-39 English and its first account (`m/44'/60'/0'/0/0`), so it also imports into MetaMask or any other wallet as the same address.
- **Import a private key or recovery phrase.** Type or paste 64 hex digits (with or without `0x`) for a private key, or a 12 or 24 word recovery phrase, whose first account is used. The input is masked.
- **Use an existing keystore file.** Type its path (absolute, or starting with `~/`), then its password.
- **Just watch balances, without a wallet.** No key at all: chibi shows the balances of the addresses it watches and sends nothing. The menu starts on this choice when the run already watches an address (`addresses` in the config). With none, it asks for `the address to watch`: `0x` and 40 hex digits, checksummed or all lowercase, not the zero address (names are not taken here, since nothing resolves before chibi starts). That address is added to `addresses` in the config file in use, keeping everything else in it, or a config with the defaults is written if there is none, so later runs start on watching it. If the config cannot be written, chibi watches it for this run anyway and says to add it to `addresses` in that file. Esc goes back to the menu.

Creating and importing then ask for a password twice (at least 8 characters; up and down move between the two fields) and encrypt the key with it into `keystores/<address>.json` next to the config file in use: `~/.config/chibi/keystores/` by default (`$XDG_CONFIG_HOME/chibi/keystores/` when that is set), or beside the file `--config` or `CHIBI_CONFIG` names. The directory is readable only by you (0700) and the file too (0600). A keystore already there for the same address is never overwritten: chibi says so and asks for that keystore's password instead. chibi then records the keystore as `keystore` in that config file, keeping everything else in it, or writes a config with the defaults and `keystore` if there is none, so later runs go straight to the password. If the keystore is saved but the config cannot be updated, for example because it is read-only, chibi says where the keystore is and to start with `--keystore <path>` next time, and carries on. The new key stays unlocked, so the UI goes on to wait for a state peer without asking for the password again.

You can also bring a keystore made elsewhere, for example with Foundry's `cast`, which asks for the private key and a password:

```sh
cast wallet import chibi --interactive          # writes ~/.foundry/keystores/chibi
chibi init --keystore ~/.foundry/keystores/chibi   # or pass --keystore to chibi wallet
chibi wallet
```

The keystore is an encrypted JSON keystore (Web3 Secret Storage, as `cast wallet import` or geth writes it). `chibi wallet` takes every `chibi node` flag but `-b` (`--config`, `--network`, `--data-dir`, `--rpc-host`, `--rpc-port`, the peer flags), plus:

| Flag | Meaning |
|---|---|
| `--keystore PATH` | The keystore to send from, instead of the config's `keystore`. Without either, the UI opens the setup above |
| `--logs` | Start with the log pane open |

`chibi wallet` cannot run in the background: it has no `-b`. A keystore that is missing or unreadable stops chibi before anything starts, naming the path. A wrong password asks again. Tab on the password prompt leaves without unlocking and watches balances instead, as **Just watch balances** above; the typed password is wiped.

Watching without a wallet shows the same waiting cats, then a list titled `chibi · <network> · watching` of every watched address in config order: its checksummed address, its primary ENS name if it has one that prints as itself, its ETH balance, and each listed token it holds, with `more tokens appear once their balances are proven` until they are. There is no form, fee estimate, name resolution, or sending. A list taller than the box scrolls one address at a time with up and down, with `↑ N more` and `↓ N more` counting the addresses out of view and `up/down scroll` among the keys; `l` opens the log pane and `q` quits. Without a state peer the sleepy cat stands in for the list until one returns.

Once unlocked, your address is watched for that run, so its ETH and every listed token are proven while chibi waits for a state peer. A dozing cat shows the wait, `looking for a state peer · peers N · state peers M · 1m20s`, and perks up at `found a state peer!` for at least a second and a half. Then the transfer form opens:

- `from`: your checksummed address and its ETH balance.
- `token`: ETH or a listed token you hold. On this row Enter opens a list, `choose a token`, of each one's symbol, name, and balance, starting on the current one: type to filter by symbol or name, up and down move, Enter picks and moves on to `amount`, Esc closes it unchanged. While the list is open every typed key goes to its filter, `q` and `l` included. Left and right on the row still cycle through the tokens. Tokens join once their balances are proven, which starts as soon as chibi is serving, without waiting for a new head; until then the row and the list say `more tokens appear once their balances are proven`.
- `amount`: a decimal amount, or `max` for the whole token balance, or the ETH balance less the max fee. For `max` ETH the fee is estimated for the exact amount sent, since a recipient contract can spend more gas for a nonzero value, and never with a zero value, which a recipient may refuse; if the fee keeps moving that amount, chibi asks for an exact amount instead.
- `to`: the recipient, `0x` and 40 hex digits, or an ENS name such as `name.eth`. Mixed case must carry a valid checksum; all lowercase skips the check. The zero address is refused, and sending a token to its own contract warns. A name is resolved through ENS's UniversalResolver against state chibi proves, like any call: under the row it shows `resolving...`, then `→` and the address it resolves to, or why it does not: `has no address`, `is not registered`, or `is served offchain, which chibi does not follow` (chibi never fetches a name from an offchain gateway). Names are taken in ASCII only (letters, digits, hyphens, and a leading underscore), lowercased; for a name with other characters, paste its address. The fee and Review wait for the name to resolve, and what is signed is the address Review shows. While this row has focus every key you type goes into it, `q` and `l` included; Ctrl-C quits.
- `fee`: the estimated gas and the max fee, refreshed at each head. A transfer that would revert shows the reason and cannot be sent.

Enter on any other row checks every field and shows each problem beside its field, keeping what you typed. Review then shows the network and chain id, the full recipient (for a name, the name and under it its full address), the exact amount and token, and the max fee; `y` signs and sends, `n` or Esc goes back. The transfer is an EIP-1559 transaction with the pending nonce, the suggested tip, and a max fee of twice the next block's base fee plus the tip, broadcast through chibi like a wallet's. The UI follows it from pending to included to a verified receipt and shows success for status 1 and failure for status 0. After 10 blocks still pending it says so; chibi never raises the fee on its own. If another transaction takes its nonce, for example a replacement your wallet sent through chibi's JSON-RPC, the UI says `this transfer was replaced or dropped: its nonce was used by another transaction` and stops following it.

Sending works only while a state peer is connected: without one, a sleepy cat says `waiting for a state peer`, Review and sending are disabled, and the form comes back as it was when one returns. chibi checks again just before signing and refuses with the same message if the last state peer has gone. Sending is Sepolia only for now: on mainnet the form shows your balances, and Review says `sending on mainnet is not enabled yet`.

The cats are pixel art drawn with half blocks in 24-bit colour, animated four times a second: a cat dozing under a drifting z while chibi waits, hopping between twinkling stars at `found a state peer!`, nodding off while paused, and bouncing under a rising heart when a transfer succeeds. When the form has at least 70 columns (a terminal at least 78 wide) a cat sits beside the form, twice its size where that fits and its own size otherwise, only where it fits without covering it, and blinks now and then; it steps away while the token list is open. Pixel cats need a terminal that sets `COLORTERM` to `truecolor` or `24bit`; without that, or with `NO_COLOR` set, the TUI shows plain ASCII cats instead.

The UI sits in the middle of the terminal as a rounded box titled `chibi · <network> · <address>`, or `chibi · <network> · <name> · <address>` once your address's primary ENS name is proven, which grows with the terminal up to 120 columns by 36 rows (12 rows more with the log pane open) and keeps at least two blank columns and one blank row around it. On a terminal smaller than 44 columns by 14 rows it fills the screen without a border instead. The text stays the terminal's font size, which a program cannot change portably, but the cats grow: the waiting, found, paused, and receipt screens center their cat and lines in the box, and the pixel cat doubles once the box has room for twice its size (a 120x40 terminal does, 80x24 does not). A form row too wide for the box, such as `from` with an 18-decimal balance at 80x24, continues on the next line under its values without splitting an address, a balance, or a hint, and the keys at the bottom take as many rows as they need.

Keys: arrows (or Tab) move between fields, `l` opens and closes the log pane (the last lines of chibi's log, which goes there instead of the terminal), `q` or Ctrl-C quits and stops chibi as Ctrl-C does in `chibi node` (on the `to` row and in the token list `q` and `l` type, so only Ctrl-C quits there). Quitting with a transfer still pending warns first; the transfer stays in chibi's outbox, which broadcasts it again on the next run. Passwords are at most 256 bytes, and a pasted secret at most 256; their buffers are wiped once used and when you quit.

## Configuration

`config.toml` fields:

| Field | Default | Meaning |
|---|---|---|
| `network` | `sepolia` | `sepolia` or `mainnet` |
| `addresses` | none | Watch-only addresses, pinned with their balance of every listed token; the TUI's watch prompt adds to it |
| `beacon_url` | Sepolia `https://beacon.sepolia.ethpandaops.io/nimbus`, mainnet `https://ethereum-beacon-api.publicnode.com` | Beacon (consensus) API the light client reads sync-committee updates from |
| `checkpoint` | none | Finalized beacon block root to start the light client from |
| `rpc_host` | `127.0.0.1` | IP address or domain JSON-RPC listens on; also accepted as a `Host` header |
| `rpc_port` | `8545` | JSON-RPC port |
| `cors_origins` | `chrome-extension://*`, `moz-extension://*` | Browser origins allowed to call the RPC |
| `allowed_hosts` | `localhost`, `127.0.0.1`, `[::1]` | Accepted `Host` headers, which blocks DNS rebinding |
| `p2p_port` | `0` (OS picks) | devp2p listen port |
| `data_dir` | XDG data directory | Absolute directory that holds the per-network data directories |
| `keystore` | none | Absolute path of the encrypted JSON keystore `chibi wallet` sends from; the TUI's setup writes it |
| `contracts` | none | Contract accounts and storage slots to pin, see below |

The peer keys are under [Peers](#peers).

`chibi node --beacon-url URL --data-dir PATH` overrides those keys for one run, as `--network`, `--rpc-host`, `--rpc-port`, and the peer flags do (`chibi wallet` takes them too); `chibi init` writes the same flags into the config. A relative `--data-dir` is taken from the working directory. `chibi status` and `chibi stop` take `--data-dir` too, to find a chibi started with it.

Data (light-client checkpoint, devp2p key, `chibi.db` with the saved snapshot, pending transactions, remembered state peers, and learned keys, and `chibi.log` for a background chibi) lives in `<data root>/<network>/`. The data root is `data_dir` when set, else `$XDG_DATA_HOME/chibi` when `XDG_DATA_HOME` is set to an absolute path, else `~/.local/share/chibi`. A `state.json` left by an earlier chibi is imported into `chibi.db` on the next `chibi node` or `chibi wallet` and then deleted. One `chibi node` or `chibi wallet` owns a network's data directory at a time, and `chibi.lock` holds its process id, so two configs on the same network need different `data_dir`s to run side by side. `chibi node -b`, `stop`, and `status` all find the directory through the config, so pass them the same `--config` and `--data-dir`.

### Peers

chibi reads state from state peers: devp2p peers that speak snap/1 or snap/2 and so can serve proofs. By default it finds peers through discovery, starting from the network's boot nodes, and also dials the network's built-in state peers (Sepolia has four; mainnet has none) and the state peers it reached on earlier runs. These keys change that. Each has a flag: `chibi init` writes it into the config, and `chibi node` and `chibi wallet` use it for that run only.

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

- State is served only at the verified head (`latest` and `pending`) and the 8 blocks before it, by number or hash. Older blocks, `eth_getLogs`, and filters return error `-32004`.
- Block responses carry header fields and an empty transaction list.
- If every state peer disconnects, chibi keeps serving its last verified head as `latest` and logs `step failed` (`no connected peer serves state (snap)`) until one returns. `eth_syncing` still reports `false`.
- Receipts and transaction lookups cover only transactions sent through chibi.
- Blob transactions are rejected.

## Trust model

A peer that lies can delay chibi but cannot make it serve a wrong value, because every value is checked against a header the light client attested. The light client trusts the sync committee. The beacon API endpoint and the checkpoint services can stall chibi. On first run a checkpoint service chooses the starting checkpoint unless you set `checkpoint`, and later runs continue from the checkpoints that one leads to. Proof requests show peers which accounts and contracts you query.

## Development

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo run -p chibi-node --example live_probe   # live Sepolia: verified header and snap replies
cargo run -p chibi-node --example live_node -- 0xAddress   # live Sepolia: a full node start
scripts/oracle-check.sh 0xAddress   # compare a running chibi with a public RPC at chibi's head
```

The workspace has three crates. `chibi-core` holds proof verification, the known-key store, BAL application, and local execution, with no I/O. `chibi-node` holds the light client, devp2p, the head follower, and the request-side node API. `chibi` is the binary, its JSON-RPC server, and its terminal UI.
