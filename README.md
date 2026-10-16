# chibi

chibi is a light Ethereum node that runs on your machine and serves your browser wallet's JSON-RPC. Every balance, call result and receipt it serves is proven against block headers a light client verified, using proofs from ordinary Ethereum peers, so you need no RPC provider and do not have to trust one. It also comes with a terminal wallet.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/weiihann/chibi/main/install.sh | sh
```

The script installs a prebuilt `chibi` for Linux (x86_64, arm64) or macOS (Intel, Apple silicon) into `~/.local/bin`. Set `CHIBI_INSTALL_DIR` to install somewhere else.

To build from source instead, install Rust with [rustup](https://rustup.rs) and run:

```sh
git clone https://github.com/weiihann/chibi
cd chibi
cargo install --locked --path crates/cli
```

## Quick start

```sh
chibi init     # choose the network, the addresses to watch, and the RPC host and port
chibi node     # start chibi; Ctrl-C stops it
```

When the log shows `JSON-RPC on http://127.0.0.1:8545, chain id 11155111`, add a custom network to your wallet with RPC URL `http://127.0.0.1:8545` and chain id `11155111` (Sepolia). The first start can take a few minutes while chibi finds a peer that serves state.

To send from the terminal instead, run `chibi wallet`. The first time, it helps you create a wallet, import one, or use an existing keystore. JSON-RPC keeps serving while it runs.

## Commands

| Command | What it does |
|---|---|
| `chibi init` | Write the config, asking for whatever the flags leave out |
| `chibi node` | Follow the chain and serve JSON-RPC to your wallet; `-b` runs it in the background |
| `chibi wallet` | Run the node under a terminal UI that sends ETH or tokens from your keystore |
| `chibi status` | Show whether chibi runs and the balances saved at the last verified head |
| `chibi stop` | Stop the chibi running in the background |

Run `chibi <command> --help` for every option.

## Files

- Config: `~/.config/chibi/config.toml`, or `$XDG_CONFIG_HOME/chibi/config.toml` when that is set. `--config PATH` or `CHIBI_CONFIG` picks another file.
- Data: `~/.local/share/chibi/<network>/`, or `$XDG_DATA_HOME/chibi/<network>/` when that is set, or under `data_dir` from the config. It holds the light-client checkpoint, the devp2p key, `chibi.db`, and `chibi.log` for a background chibi.
- Keystores that `chibi wallet` creates go in `keystores/` next to the config file.

## Trust model

- **Proven:** every account, balance, nonce, code, storage value and receipt chibi serves is checked against a header whose hash the light client verified. A lying peer can stall chibi but cannot make it serve a wrong value.
- **Trusted:** the Ethereum sync committee, read through a beacon API (`beacon_url`, with a public default per network). On the first run a public checkpoint service picks the light client's starting checkpoint, unless you set `checkpoint` in the config.
- **Seen by peers:** the peers that serve proofs see which accounts and contracts you query, and the network sees the transactions you send. To keep queries to your own node, list it in `trusted_peers` and set `trusted_only = true`.

## Status

chibi is early software. It targets Sepolia first. Mainnet support is in the code but untested, and `chibi wallet` does not send on mainnet.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
