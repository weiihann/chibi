---
name: verify-chibi
description: Drive the real chibi node on Sepolia through its local JSON-RPC (the surface a browser wallet uses) and its CLI, and capture proof that it serves verified state. Use to verify any chibi behaviour change, before merging, and again after Sepolia's Glamsterdam fork (2026-10-06 13:53:36 UTC).
---

# Verify chibi

chibi's user is a browser wallet pointed at `http://127.0.0.1:<port>`, plus a person running `chibi init`, `chibi node`, `chibi wallet`, and `chibi status`. Verify through those surfaces only. Unit tests (`cargo test --workspace --all-features`) are not proof of behaviour.

Helpers live in `scripts/` next to this file. Paths below are relative to the repo root. Feature recipes are in [`features/README.md`](features/README.md); read the index, then the feature you are proving.

## Fork status

Before 2026-10-06 13:53:36 UTC, Sepolia has no block access lists, so every head logs `stepped=Reanchored`. After it, heads should log `stepped=Extended`. Only `follow-bal` in [head-following](features/head-following.md) needs the fork. Everything else runs today. After the fork, rerun every feature, because the EVM gas rules change at Amsterdam.

## Launch

```sh
RUN_DIR=$(.agents/skills/verify-chibi/scripts/launch.sh --port 8645 0x3d767bf85779e2b550ab5fcd7e3be39fb77a5ce6 0xe1f0ba7cf9175f61ad1fe29ee4f30249871bfc29 | tail -1)
```

- Builds the release binary, copies it from cargo's target directory (`target/`, or `CARGO_TARGET_DIR` when set) into `$RUN_DIR/bin`, writes a config with `chibi init` (every registry token, USDC included, is pinned for each watched address) and data dir under `$RUN_DIR/home` (HOME and XDG overridden), and starts `chibi node --rpc-host 127.0.0.1 --rpc-port <port>` in the foreground of a background shell job, so `$RUN_DIR/chibi.pid` is chibi's own pid.
- Ready when `$RUN_DIR/chibi.log` contains `JSON-RPC on http://127.0.0.1:<port>`. The script waits up to `--timeout` seconds (default 900). Most of that is waiting for a state peer (a peer that speaks snap/1 or snap/2); the log prints `waiting for a state peer (snap)` with `peers=` and `state_peers=` every 30 s.
- `--network mainnet` is accepted but untested and has no known state peers.
- The addresses above are active Sepolia USDC holders. Use any address; `launch.sh` requires at least one (`chibi init` itself does not).
- Each launch is isolated by run dir and port, so several can run side by side, but they compete for the few Sepolia state peers: on 2026-10-16, starting a second instance repeatedly cost the first its only state peer (`state_peers=0`, `step failed`, reads failing with `no connected peer serves state (snap)`) for minutes. Drive features serially in one instance where you can, and run `doctor.sh` again after starting another. Never drive a chibi you did not start: the user's own instance (default port 8545, data in `~/.local/share/chibi`, or `~/Library/Application Support/chibi` from before the XDG switch) is off limits.

## Doctor

```sh
.agents/skills/verify-chibi/scripts/doctor.sh "$RUN_DIR"
```

Read-only. Exits 0 only when the pid in `$RUN_DIR/chibi.pid` runs this run's binary, holds the port, uses the scratch data dir, answers the expected `eth_chainId`, and is within 3 blocks of the public oracle's head. Run it before driving and whenever a result looks wrong. A `stale head` failure means no state peer (snap) is serving; the node keeps answering with an old block, so any comparison made then is invalid.

## Drive

- JSON-RPC: `.agents/skills/verify-chibi/scripts/rpc.sh "$RUN_DIR" <method> '<params-json>'` prints the response and appends request and response to `$RUN_DIR/evidence/rpc.log`. `cast ... --rpc-url http://127.0.0.1:<port>` exercises the same calls a wallet makes.
- Oracle comparison: `CHIBI=http://127.0.0.1:<port> scripts/oracle-check.sh <address>... | tee "$RUN_DIR/evidence/oracle-check.txt"` compares chibi with publicnode at chibi's own head and exits non-zero on any difference. The oracle is a test input only; chibi never uses it.
- Wallet: `.agents/skills/verify-chibi/scripts/wallet-check.sh "$RUN_DIR" <address> <name> [--seconds N] [--listen-blocks] [--start-after S]` runs MetaMask's own network client (`@metamask/network-controller`, with its block-tag rewrite and 20 s block tracker) and its native-balance code (`@metamask/assets-controller` `RpcDataSource`) against the run, from the pinned packages in `wallet/`. It writes `wallet-<name>.txt` and every request on the wire to `wallet-<name>.jsonl` in evidence, and exits 0 only when every balance read succeeded and the balance loaded. The first run installs the packages with `bun install --frozen-lockfile`.
- CLI: run the copied binary with the run's environment, for example `env -u CHIBI_CONFIG HOME="$RUN_DIR/home" XDG_CONFIG_HOME="$RUN_DIR/home/.config" XDG_DATA_HOME="$RUN_DIR/home/.local/share" "$RUN_DIR/bin/chibi" status`.
- Node behaviour: `.agents/skills/verify-chibi/scripts/log.sh "$RUN_DIR"` prints `$RUN_DIR/chibi.log` (or `evidence/chibi.log` after cleanup) without the colour codes chibi writes around field names in the foreground, so grep its output, not the raw file: a raw `grep 'stepped=Extended'` finds nothing. Stable strings are `waiting for a state peer (snap)` (with `peers=` and `state_peers=`), `found a state peer` (once per process, on stdout under a small ASCII cat, before `anchored`), `anchored head=` (with `keys=`, the pinned keys plus the learned keys the last run saved), `head verified`, `stepped=Reanchored`, `stepped=Extended`, `cannot extend with BALs; reanchoring`, `step failed`, `no connected peer serves state (snap)`, `transaction included`, `broadcast transaction` (with `sent=`, debug), `session established ... snap="1"` or `snap="2"` (debug; a state peer, a `snap="1"` one only after 3 s connected; `snap="none"` otherwise), `status head=` (every 60 s once JSON-RPC opens, with `peers=`, `state_peers=`, and `pinned=` and `learned=` as held/total), `no state peer (snap) connected`, `pinned keys not held at the head for two status lines` (with `count=` and up to five `keys=`), and, without a config file, `no config at`.

## Evidence

Everything goes in `$RUN_DIR/evidence/`, which cleanup keeps. Default root is `/tmp/chibi-verify/<run-id>/`; set `CHIBI_VERIFY_ROOT` to move it.

- Exercise the user path: JSON-RPC methods a wallet calls, and the CLI commands a person types. Not test fixtures, not internal APIs.
- Pair every action with the resulting state: the RPC response and the oracle's answer at the same block, or the command and its output.
- Check side effects too: the log line for the head step, the transaction found on chain by the oracle, the config file on disk, the snapshot in `chibi status`.
- Record the block number with every state comparison. chibi serves state only at its head and the 8 blocks before it, so an answer without a block cannot be compared.
- Report a skipped feature with the unmet precondition (no funded key, fork not active, no state peer). Never report it as verified through another path.

## Cleanup

```sh
.agents/skills/verify-chibi/scripts/cleanup.sh "$RUN_DIR"
```

Sends SIGINT to the pid this run started (never by process name), waits, then saves `chibi status` output and moves `chibi.log` into `evidence/`. It removes `home/`, `bin/`, and the pid file. Afterwards `$RUN_DIR` holds only `meta.env` and `evidence/`. Run it after every attempt, including failed ones.

## Helpers

| Script | Use |
|---|---|
| `scripts/launch.sh [--network N] [--port N] [--timeout S] [--history] ADDRESS...` | build, isolate, start, wait; `--history` keeps account history; last stdout line is `RUN_DIR` |
| `scripts/doctor.sh RUN_DIR` | read-only health check, exit 0 when healthy |
| `scripts/rpc.sh RUN_DIR METHOD [PARAMS_JSON]` | one JSON-RPC call, logged to evidence |
| `scripts/log.sh RUN_DIR` | the node log without colour codes, live or after cleanup |
| `scripts/cleanup.sh RUN_DIR` | stop what this run started, keep evidence |
| `scripts/wallet-check.sh RUN_DIR ADDRESS NAME [OPTIONS]` | MetaMask's network client and balance code against the run; exit 0 when the balance loads with no failed read |
| `scripts/history-check.py RUN_DIR ADDRESS [ORACLE]` | recorded history vs the oracle's logs over every covered range; exit 0 when they match |
| `scripts/oracle-check.sh ADDRESS...` (repo root) | chibi vs public RPC at chibi's head; set `CHIBI` to the run's URL |
