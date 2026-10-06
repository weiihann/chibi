# Head following

chibi moves to every new head the light client attests, so the wallet's `latest` is the chain tip. Before Glamsterdam it proves every tracked key again at each head. After Glamsterdam it applies each block's access list instead. `chibi status` shows the last verified head without a running node.

## Sub-features

- `follow-reanchor` moves to each new head by proving tracked keys again (every head before the fork; after the fork, whenever a block access list is unavailable).
- `follow-bal` moves to the next head by applying its block access list. Needs the Sepolia Glamsterdam fork (2026-10-06 13:53:36 UTC).
- `follow-fresh` keeps `eth_blockNumber` within a few blocks of the chain tip.
- `follow-status` prints the saved snapshot with its block number from `chibi status`.

## How to get to it (user POV)

- The wallet polls `eth_blockNumber` and `eth_getBlockByNumber("latest")` and refreshes balances.
- A person runs `chibi status` in a terminal, with or without a running node.

## Driving it with verify-chibi scripts

Preconditions:

- `doctor.sh "$RUN_DIR"` exits 0.
- For `follow-bal`, the current time is after 2026-10-06 13:53:36 UTC and `cast block latest -f timestamp --rpc-url $O` is at least `1791294816`.

- **Watch heads.** Let the node run about two minutes. Run `grep 'head verified' "$RUN_DIR/chibi.log" | tail -10`. Block numbers increase by one per line about every 12 s. Before the fork each line ends in `stepped=Reanchored(...)`; after it, `stepped=Extended(...)`.
- **BAL path.** After the fork, confirm the access-list path ran. Run `grep -c 'stepped=Extended' "$RUN_DIR/chibi.log"`. The count is at least 1. If it stays 0 while heads advance, chibi is falling back to reanchoring because no peer served a block access list; record that as a `follow-bal` failure.
- **Values follow the chain.** Compare at two heads at least a minute apart. Run `CHIBI=$C scripts/oracle-check.sh 0x3d767bf85779e2b550ab5fcd7e3be39fb77a5ce6 | tee "$RUN_DIR/evidence/follow-1.txt"`, wait 60 s, then run it again into `follow-2.txt`. Both report `0 difference(s)` at different block numbers.
- **Freshness.** Run `.agents/skills/verify-chibi/scripts/doctor.sh "$RUN_DIR" | tee "$RUN_DIR/evidence/doctor.txt"`. The head line reads `OK` with a lag of 3 blocks or fewer.
- **Status.** With the node running, run `env -u CHIBI_CONFIG HOME="$RUN_DIR/home" XDG_CONFIG_HOME="$RUN_DIR/home/.config" XDG_DATA_HOME="$RUN_DIR/home/.local/share" "$RUN_DIR/bin/chibi" status | tee "$RUN_DIR/evidence/status.txt"`. It prints `sepolia head <N> (saved <age> ago, not re-verified)` and one line per watched address with ETH and USDC. `cleanup.sh` saves the same view after the node stops as `status-after-stop.txt`.
- **Proof.** Keep the `head verified` lines, both oracle checks, `doctor.txt`, and the status files.

## Gotchas

- `step failed ... no connected peer serves snap/2` followed by no new `head verified` lines is the stale-head state: chibi keeps serving its last head and `eth_syncing` still answers `false`. It is a real behaviour to report, not a harness fault.
- helios attests a head about 4 s into each slot, and under Gloas one slot late, so chibi's head trails the explorer by a block or two.
- `chibi status` values are a display snapshot from disk. They prove persistence, not verification.
- After the fork, an `Extended` step makes no proof requests. Compare the values with `oracle-check.sh` anyway; that is the only check that the access list was applied correctly.
