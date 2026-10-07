# chibi verification map

This directory is the maintained source for verifying chibi's user-facing behaviour. Read this index, then drive the matching feature file.

## Baseline preconditions

- A run started with `.agents/skills/verify-chibi/scripts/launch.sh`, with `RUN_DIR` and its `PORT` (from `$RUN_DIR/meta.env`) known.
- `doctor.sh "$RUN_DIR"` exits 0, including the head-freshness check.
- `cast`, `curl`, `lsof`, and `tmux` are on `PATH`, and `bun` for `wallet-check.sh`.
- Outbound access to Sepolia devp2p peers, `https://beacon.sepolia.ethpandaops.io/nimbus` (the light client's beacon API), and `https://ethereum-sepolia-rpc.publicnode.com` (the oracle).
- Never drive the user's own chibi (default port 8545).

## Driving conventions

- `C=http://127.0.0.1:$PORT` is chibi; `O=https://ethereum-sepolia-rpc.publicnode.com` is the oracle.
- Pin every state comparison to chibi's head: read `cast block-number --rpc-url $C`, then query both sides with `--block <that number>`. chibi serves state at its head and the 8 blocks before it and answers `-32004` for older blocks; when the head moves between the two calls, read it again.
- Sepolia USDC is `0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238` (6 decimals).
- Send every artifact into `$RUN_DIR/evidence/`.

## Proof and skip reporting

- Capture the action and the resulting state, and say which block it was at.
- RPC proof is chibi's response next to the oracle's at the same block.
- CLI proof is the command, its output, and its exit code.
- Node proof is the matching `chibi.log` line.
- Name the feature ID and entry point with each artifact.
- Report an unreachable path with the command tried and the unmet precondition: fork not active, no funded key, or a stale head from `doctor.sh`.

## Feature entry contract

Each feature file has an H1 title and one paragraph describing the user-visible behaviour, then exactly four H2 sections in this order: `Sub-features`, `How to get to it (user POV)`, `Driving it with verify-chibi scripts` (starting with `Preconditions:`), and `Gotchas`.

## Features

- [Verified state reads](./state-reads.md) covers balances, nonces, token balances, code, storage, `eth_call`, and gas estimates against the oracle. Runs before the fork.
- [Head following](./head-following.md) covers reanchoring every block before the fork, applying block access lists after it, staleness, and `chibi status`.
- [Send and confirm](./send-and-confirm.md) covers sending a signed transaction through chibi and getting a verified receipt. Needs a funded Sepolia key.
- [Onboarding](./onboarding.md) covers `install.sh`, interactive and flag-driven `chibi init`, and the first `chibi run`.
- [RPC exposure](./rpc-exposure.md) covers the Host filter, the CORS allowlist, unsupported methods, and block responses.
