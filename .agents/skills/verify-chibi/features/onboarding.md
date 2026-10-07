# Onboarding

A new user runs `./install.sh`, which installs the `chibi` binary and runs `chibi init`. Init asks for the network, watch-only addresses, and the RPC host and port, then writes the config and says how to start. It asks no token question: chibi learns the keys the wallet reads. The user then starts chibi in the foreground or in the background, checks it, and stops it.

## Sub-features

- `onboard-install` builds and installs the binary with cargo, runs init on a terminal, and does not start chibi.
- `onboard-init-menu` asks through terminal menus: a network list, an address loop (empty line finishes, invalid input re-asks), the RPC host (IP or domain) and port with defaults. No token question, and the written config has no `tokens` key.
- `onboard-init-plain` asks the same questions as plain line prompts when stdin is not a terminal; Enter takes each default.
- `onboard-init-flags` writes the config from `--network`, `--address`, `--rpc-host`, and `--rpc-port` without prompting. There is no token flag. Overwriting a config keeps its `tokens` list, `[[contracts]]`, and `data_dir`, and says so.
- `onboard-background` runs chibi with `chibi run -b`, reports it with `chibi status`, and stops it with `chibi stop`.

## How to get to it (user POV)

- Run `./install.sh` in a terminal from the repo root.
- Run `chibi init` directly, with or without flags.
- Run `chibi run -b`, `chibi status`, and `chibi stop`.

## Driving it with verify-chibi scripts

Preconditions:

- A scratch HOME: `S=$(mktemp -d /tmp/chibi-onboard.XXXX); mkdir -p "$S/evidence"`. `E="env -u CHIBI_CONFIG HOME=$S XDG_CONFIG_HOME=$S/.config XDG_DATA_HOME=$S/.local/share"`.
- A free port for the run, for example 8747: `lsof -nP -iTCP:8747 -sTCP:LISTEN` prints nothing.

- **Install.** Run `tmux new-session -d -s chibi-onboard -x 160 -y 40 "$E RUSTUP_HOME=$HOME/.rustup CARGO_HOME=$HOME/.cargo CARGO_INSTALL_ROOT=$S/install CARGO_TARGET_DIR=$PWD/target ./install.sh 2>&1 | tee $S/evidence/install.txt; sleep 600"`. Poll `tmux capture-pane -p -t chibi-onboard` until it shows `? Network`.
- **Network.** Run `tmux send-keys -t chibi-onboard Enter`. The pane shows `? Network sepolia` and `? Watch-only address`.
- **Addresses.** Run `tmux send-keys -t chibi-onboard 'not-an-address' Enter`; the pane shows `"not-an-address" is not an address`. Clear the line with 14 `BSpace` keys, then `tmux send-keys -t chibi-onboard '0x3d767bf85779e2b550ab5fcd7e3be39fb77a5ce6' Enter` and `tmux send-keys -t chibi-onboard Enter`. The pane shows `? RPC host` next; no token question appears.
- **RPC host and port.** Run `tmux send-keys -t chibi-onboard 'bad_host!' Enter`; the pane shows `is not an IP address or host name`. Clear it with 9 `BSpace` keys, then `tmux send-keys -t chibi-onboard Enter` (host `127.0.0.1`) and `tmux send-keys -t chibi-onboard '8747' Enter`. The pane shows `wrote ... (sepolia, 1 address)` with no `pinning`, the wallet URL `http://127.0.0.1:8747` with chain id `11155111`, and `chibi run` / `chibi run -b` as next steps. No `light client` line follows: install does not start chibi.
- **Config on disk.** Run `cat "$S/.config/chibi/config.toml"`. It has `rpc_host = "127.0.0.1"`, `rpc_port = 8747`, the checksummed address, and the default `cors_origins` and `allowed_hosts`, and no `tokens` line.
- **Background run.** Run `$E "$S/install/bin/chibi" run -b`. It prints the pid, the log path under the data dir, and the RPC URL. `$E "$S/install/bin/chibi" status` prints `running (pid <N>), JSON-RPC http://127.0.0.1:8747`. A second `run -b` is refused with the same pid.
- **Serving.** Wait until `"$S/.local/share/chibi/sepolia/chibi.log"` contains `JSON-RPC on http://127.0.0.1:8747` (minutes, while it waits for a snap/2 peer). Then `CHIBI=http://127.0.0.1:8747 TOKENS="0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238 0xaA8E23Fb1079EA71e0a56F48a2aA51851D8433D0 0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14" scripts/oracle-check.sh 0x3d767bf85779e2b550ab5fcd7e3be39fb77a5ce6 | tee "$S/evidence/oracle-check.txt"` reports `0 difference(s)`.
- **Status and stop.** Run `$E "$S/install/bin/chibi" status`; it shows the running line and one line per address with ETH and the nonce, plus USDC, USDT, and WETH, which the oracle check read through `eth_call` (no token is pinned, so a token appears only after something read its keys). Run `$E "$S/install/bin/chibi" stop`; it reports the process stopped, the log ends with `shutting down on SIGTERM`, and `status` then prints `not running` above the saved snapshot.
- **Flags.** Run `$E "$S/install/bin/chibi" init --config "$S/flags.toml" --network sepolia --address 0xe1f0ba7cf9175f61ad1fe29ee4f30249871bfc29 --rpc-host localhost --rpc-port 8545 </dev/null`. Exit code 0, no prompts, and `$S/flags.toml` has `rpc_host = "localhost"` and no `tokens` line. `$E "$S/install/bin/chibi" init --help` lists no token flag.
- **Tokens survive an overwrite.** Run `echo 'tokens = ["DAI"]' >> "$S/flags.toml"`, then the flags command above again. It prints `wrote ... (sepolia, 1 address, pinning DAI)` and `kept tokens = [DAI] from the existing config`, and `$S/flags.toml` still has `tokens = ["DAI"]`.
- **Proof.** Run `tmux capture-pane -p -S - -t chibi-onboard > "$S/evidence/pane.txt"` and copy the config files and the node log into `$S/evidence/`.
- **Cleanup.** Run `$E "$S/install/bin/chibi" stop` if it still runs, `tmux kill-session -t chibi-onboard`, then remove `$S` except `$S/evidence`.

## Gotchas

- The menus need a real terminal. Driving `install.sh` or `chibi init` through a pipe gives the plain prompts instead (`Network [sepolia]:`, `RPC host, an IP address or domain [127.0.0.1]:`, and so on), which is the `onboard-init-plain` path, not `onboard-init-menu`.
- In the address prompt, a rejected line stays in the input box. Clear it with `BSpace` before typing the next address.
- The config lives under `$XDG_CONFIG_HOME/chibi/` and the data under `$XDG_DATA_HOME/chibi/` on macOS and Linux alike; with those unset, under `$HOME/.config` and `$HOME/.local/share`. `CHIBI_CONFIG` overrides the config path, so it must be unset. Override HOME and both XDG variables and unset `CHIBI_CONFIG` to isolate a run.
- Overriding HOME without `RUSTUP_HOME` and `CARGO_HOME` makes cargo look for a toolchain in the scratch directory and fail.
- `chibi status` shows the URL from the config file. A `run` started with `--rpc-host` or `--rpc-port` overrides is not reflected there.
- `chibi run -b`, `chibi stop`, and `chibi status` find the node through the config's data dir. Pass the same `--network` and `--config` (or `CHIBI_CONFIG`) to all three commands.
