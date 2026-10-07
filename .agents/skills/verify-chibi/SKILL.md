---
name: verify-chibi
description: Drive the real chibi node on Sepolia through its local JSON-RPC (the surface a browser wallet uses) and its CLI, and capture proof that it serves verified state. Use to verify any chibi behaviour change, before merging, and again after Sepolia's Glamsterdam fork (2026-10-06 13:53:36 UTC).
---

# Verify chibi

chibi's user is a browser wallet pointed at `http://127.0.0.1:<port>`, plus a person running `chibi init`, `chibi run`, and `chibi status`. Verify through those surfaces only. Unit tests (`cargo test --workspace --all-features`) are not proof of behaviour.

Helpers live in `scripts/` next to this file. Paths below are relative to the repo root. Feature recipes are in [`features/README.md`](features/README.md); read the index, then the feature you are proving.

## Fork status

Before 2026-10-06 13:53:36 UTC, Sepolia has no block access lists, so every head logs `stepped=Reanchored`. After it, heads should log `stepped=Extended`. Only `follow-bal` in [head-following](features/head-following.md) needs the fork. Everything else runs today. After the fork, rerun every feature, because the EVM gas rules change at Amsterdam.

## Launch

```sh
RUN_DIR=$(.agents/skills/verify-chibi/scripts/launch.sh --port 8645 0x3d767bf85779e2b550ab5fcd7e3be39fb77a5ce6 0xe1f0ba7cf9175f61ad1fe29ee4f30249871bfc29 | tail -1)
```

- Builds `target/release/chibi`, copies it into `$RUN_DIR/bin`, writes a config (init asks no token question, so the script appends `tokens = ["USDC"]` to pin USDC) and data dir under `$RUN_DIR/home` (HOME and XDG overridden), and starts `chibi run --rpc-host 127.0.0.1 --rpc-port <port>` in the foreground of a background shell job, so `$RUN_DIR/chibi.pid` is chibi's own pid.
- Ready when `$RUN_DIR/chibi.log` contains `JSON-RPC on http://127.0.0.1:<port>`. The script waits up to `--timeout` seconds (default 900). Most of that is waiting for a snap/2 peer; the log prints `waiting for a snap/2 peer` every 30 s.
- `--network mainnet` is accepted but untested and has no pinned snap/2 peers.
- The addresses above are active Sepolia USDC holders. Use any address; at least one is required.
- Each launch is isolated by run dir and port, so several can run side by side. Never drive a chibi you did not start: the user's own instance (default port 8545, data in `~/.local/share/chibi`, or `~/Library/Application Support/chibi` from before the XDG switch) is off limits.

## Doctor

```sh
.agents/skills/verify-chibi/scripts/doctor.sh "$RUN_DIR"
```

Read-only. Exits 0 only when the pid in `$RUN_DIR/chibi.pid` runs this run's binary, holds the port, uses the scratch data dir, answers the expected `eth_chainId`, and is within 3 blocks of the public oracle's head. Run it before driving and whenever a result looks wrong. A `stale head` failure means no snap/2 peer is serving; the node keeps answering with an old block, so any comparison made then is invalid.

## Drive

- JSON-RPC: `.agents/skills/verify-chibi/scripts/rpc.sh "$RUN_DIR" <method> '<params-json>'` prints the response and appends request and response to `$RUN_DIR/evidence/rpc.log`. `cast ... --rpc-url http://127.0.0.1:<port>` exercises the same calls a wallet makes.
- Oracle comparison: `CHIBI=http://127.0.0.1:<port> scripts/oracle-check.sh <address>... | tee "$RUN_DIR/evidence/oracle-check.txt"` compares chibi with publicnode at chibi's own head and exits non-zero on any difference. The oracle is a test input only; chibi never uses it.
- CLI: run the copied binary with the run's environment, for example `env -u CHIBI_CONFIG HOME="$RUN_DIR/home" XDG_CONFIG_HOME="$RUN_DIR/home/.config" XDG_DATA_HOME="$RUN_DIR/home/.local/share" "$RUN_DIR/bin/chibi" status`.
- Node behaviour: `$RUN_DIR/chibi.log`. Stable strings are `anchored head=` (with `keys=`, the pins at start), `head verified`, `stepped=Reanchored`, `stepped=Extended`, `step failed`, `transaction included`, `session established ... snap=true`.

## Evidence

Everything goes in `$RUN_DIR/evidence/`, which cleanup keeps. Default root is `/tmp/chibi-verify/<run-id>/`; set `CHIBI_VERIFY_ROOT` to move it.

- Exercise the user path: JSON-RPC methods a wallet calls, and the CLI commands a person types. Not test fixtures, not internal APIs.
- Pair every action with the resulting state: the RPC response and the oracle's answer at the same block, or the command and its output.
- Check side effects too: the log line for the head step, the transaction found on chain by the oracle, the config file on disk, the snapshot in `chibi status`.
- Record the block number with every state comparison. chibi serves state only at its head, so an answer without a block cannot be compared.
- Report a skipped feature with the unmet precondition (no funded key, fork not active, no snap/2 peer). Never report it as verified through another path.

## Cleanup

```sh
.agents/skills/verify-chibi/scripts/cleanup.sh "$RUN_DIR"
```

Sends SIGINT to the pid this run started (never by process name), waits, then saves `chibi status` output and moves `chibi.log` into `evidence/`. It removes `home/`, `bin/`, and the pid file. Afterwards `$RUN_DIR` holds only `meta.env` and `evidence/`. Run it after every attempt, including failed ones.

## Helpers

| Script | Use |
|---|---|
| `scripts/launch.sh [--network N] [--port N] [--timeout S] ADDRESS...` | build, isolate, start, wait; last stdout line is `RUN_DIR` |
| `scripts/doctor.sh RUN_DIR` | read-only health check, exit 0 when healthy |
| `scripts/rpc.sh RUN_DIR METHOD [PARAMS_JSON]` | one JSON-RPC call, logged to evidence |
| `scripts/cleanup.sh RUN_DIR` | stop what this run started, keep evidence |
| `scripts/oracle-check.sh ADDRESS...` (repo root) | chibi vs public RPC at chibi's head; set `CHIBI` to the run's URL |
