# Onboarding

A new user runs `./install.sh`, which installs the `chibi` binary, asks for the network and watch-only addresses through `chibi init`, writes the config, and starts `chibi run`.

## Sub-features

- `onboard-install` builds and installs the binary with cargo.
- `onboard-init-prompt` asks for the network (Enter keeps `sepolia`) and addresses (blank line finishes), re-prompts on invalid input, and writes the config.
- `onboard-init-flags` writes the same config from `--network` and repeated `--address` flags without prompting.
- `onboard-first-run` starts the node after init, which saves a light-client checkpoint for later runs.

## How to get to it (user POV)

- Run `./install.sh` in a terminal from the repo root.
- Run `chibi init` or `chibi init --network sepolia --address 0x...` directly.

## Driving it with verify-chibi scripts

Preconditions:

- Nothing listens on port 8545: `lsof -nP -iTCP:8545 -sTCP:LISTEN` prints nothing. `install.sh` always ends in `chibi run` on the configured address, which `init` sets to `127.0.0.1:8545`.
- A scratch directory: `S=$(mktemp -d /tmp/chibi-onboard.XXXX); mkdir -p "$S/evidence"`.

- **Interactive install.** Start `install.sh` in a tmux pane with an isolated HOME and install root, reusing the real toolchain and build cache. Run `tmux new-session -d -s chibi-onboard -x 200 -y 50 "env HOME=$S XDG_CONFIG_HOME=$S/.config XDG_DATA_HOME=$S/.local/share RUSTUP_HOME=$HOME/.rustup CARGO_HOME=$HOME/.cargo CARGO_INSTALL_ROOT=$S/install CARGO_TARGET_DIR=$PWD/target ./install.sh 2>&1 | tee $S/evidence/install.txt"`. Poll `tmux capture-pane -p -t chibi-onboard` until it shows `Network [sepolia]:`.
- **Answer prompts.** Run `tmux send-keys -t chibi-onboard 'goerli' Enter`. The pane shows `unknown network "goerli"; expected sepolia or mainnet` and asks again. Run `tmux send-keys -t chibi-onboard '' Enter`, then `tmux send-keys -t chibi-onboard 'not-an-address' Enter` (re-prompted), then `tmux send-keys -t chibi-onboard '0xe1f0ba7cf9175f61ad1fe29ee4f30249871bfc29' Enter` and `tmux send-keys -t chibi-onboard '' Enter`. The pane shows `wrote .../chibi/config.toml (sepolia, 1 address, tracking USDC)` and the next steps naming chain id `11155111`.
- **Config on disk.** Run `cat "$S/Library/Application Support/chibi/config.toml"` on macOS (`$S/.config/chibi/config.toml` on Linux). It has `network = "sepolia"`, the address in checksum form, `rpc_addr = "127.0.0.1:8545"`, and the default `cors_origins` and `allowed_hosts`.
- **First run.** Keep polling the pane. It shows `light client synced`, then `waiting for a snap/2 peer`, then `JSON-RPC on http://127.0.0.1:8545, chain id 11155111`. Then `ls "$S/Library/Application Support/chibi/sepolia/"` lists `checkpoint` (32 bytes), `devp2p.key`, `chibi.lock`, and `state.json`.
- **Flags.** Run `env HOME=$S/flags XDG_CONFIG_HOME=$S/flags/.config "$S/install/bin/chibi" init --network sepolia --address 0xe1f0ba7cf9175f61ad1fe29ee4f30249871bfc29 --address 0x3d767bf85779e2b550ab5fcd7e3be39fb77a5ce6`. Exit code 0 and output `wrote ... (sepolia, 2 addresses, tracking USDC)` with no prompts.
- **Proof.** Run `tmux capture-pane -p -S - -t chibi-onboard > "$S/evidence/pane.txt"` and copy the config files into `$S/evidence/`.
- **Cleanup.** Run `tmux kill-session -t chibi-onboard`, then remove `$S` except `$S/evidence`.

## Gotchas

- `install.sh` runs `chibi init` only when stdin is a terminal. Piping input into `install.sh` skips init and starts `chibi run` with whatever config exists, so drive it in tmux.
- On macOS the config lives under `$HOME/Library/Application Support/chibi/`, not `~/.config`. Overriding HOME isolates it; on Linux the XDG variables do.
- Answering `n` to the overwrite prompt exits 0 and keeps the old config; `install.sh` then runs the old config.
- Overriding HOME without `RUSTUP_HOME` and `CARGO_HOME` makes cargo look for a toolchain in the scratch directory and fail.
- The first run trusts public checkpoint services and logs a warning. Later runs in the same data dir start from the saved checkpoint and do not warn.
