# Head following

chibi moves to every new head the light client attests, so the wallet's `latest` is the chain tip. Since Glamsterdam it applies each block's access list, updating only keys it holds. It reanchors instead (proving the pinned keys again, swapping, then warming the learned keys requests read) when a gap has no access lists, is over 32 blocks, or does not link to its anchor, and when applying the access lists fails. The pinned keys are the watched addresses, `[[contracts]]`, every registry token's keys for each watched address, and the ENS reverse-lookup keys seeded for them. Every step needs a state peer: a devp2p session on snap/2 at once, or on snap/1 once it has stayed connected 3 s. `chibi status` shows the last verified head without a running node.

## Sub-features

- `follow-reanchor` moves to a new head by proving the pinned keys again and then warming learned keys (every head before the fork; after it, when the gap lacks access lists, is over 32 blocks, or does not link, or when an access list fails to apply, logged as `cannot extend with BALs; reanchoring`).
- `follow-bal` moves to the next head by applying its block access list. Needs the Sepolia Glamsterdam fork (2026-10-06 13:53:36 UTC).
- `follow-fresh` keeps `eth_blockNumber` within a few blocks of the chain tip.
- `follow-status` prints the saved snapshot with its block number from `chibi status`.
- `follow-state-peers` counts state peers: `session established ... snap="2"` counts at once, `snap="1"` only after 3 s connected, and `snap="none"` never. The `status head=` line every 60 s carries `peers=` and `state_peers=`, and `no state peer (snap) connected` warns when none is left; heads stall until one returns.

## How to get to it (user POV)

- The wallet polls `eth_blockNumber` and `eth_getBlockByNumber("latest")` and refreshes balances.
- A person runs `chibi status` in a terminal, with or without a running node.

## Driving it with verify-chibi scripts

Preconditions:

- `doctor.sh "$RUN_DIR"` exits 0.
- For `follow-bal`, the current time is after 2026-10-06 13:53:36 UTC and `cast block latest -f timestamp --rpc-url $O` is at least `1791294816`.

- **Watch heads.** Let the node run about two minutes. Run `.agents/skills/verify-chibi/scripts/log.sh "$RUN_DIR" | grep 'head verified' | tail -10`. Block numbers increase by one per line about every 12 s, a stall now and then aside (see **State peers**). Before the fork each line ends in `stepped=Reanchored(...)`; after it, `stepped=Extended(...)`.
- **BAL path.** After the fork, confirm the access-list path ran. Run `.agents/skills/verify-chibi/scripts/log.sh "$RUN_DIR" | grep -c 'stepped=Extended'` and `.agents/skills/verify-chibi/scripts/log.sh "$RUN_DIR" | grep 'cannot extend with BALs'`. The count is at least 1, and every `Reanchored` line is explained by a `cannot extend with BALs` line just before it or by a gap (the head jumped more than 32 blocks). If the count stays 0 while heads advance, chibi is falling back to reanchoring because no peer served a block access list; record that as a `follow-bal` failure.
- **Values follow the chain.** Compare at two heads at least a minute apart. Run `CHIBI=$C scripts/oracle-check.sh 0x3d767bf85779e2b550ab5fcd7e3be39fb77a5ce6 | tee "$RUN_DIR/evidence/follow-1.txt"`, wait 60 s, then run it again into `follow-2.txt`. Both report `0 difference(s)` at different block numbers.
- **Freshness.** Run `.agents/skills/verify-chibi/scripts/doctor.sh "$RUN_DIR" | tee "$RUN_DIR/evidence/doctor.txt"`. The head line reads `OK` with a lag of 3 blocks or fewer.
- **State peers.** Run `.agents/skills/verify-chibi/scripts/log.sh "$RUN_DIR" | grep -o 'session established.*snap="[^"]*"' | grep -o 'snap="[^"]*"' | sort | uniq -c` (the `session established` lines are debug output, which `launch.sh` turns on) and `.agents/skills/verify-chibi/scripts/log.sh "$RUN_DIR" | grep -E 'status head=|no state peer \(snap\) connected' | tail -4`. Each `status head=` line has `state_peers=` at most `peers=`; with `state_peers=0` the warning follows on the next line and `head verified` lines pause until a state peer returns.
- **Status.** With the node running, run `env -u CHIBI_CONFIG HOME="$RUN_DIR/home" XDG_CONFIG_HOME="$RUN_DIR/home/.config" XDG_DATA_HOME="$RUN_DIR/home/.local/share" "$RUN_DIR/bin/chibi" status | tee "$RUN_DIR/evidence/status.txt"`. It prints `running (pid <N>), JSON-RPC http://127.0.0.1:<port>`, then `sepolia head <N> (saved <age> ago, not re-verified)` and one line per watched address with its primary ENS name right after the address when it has one, ETH, and every registry token the address holds a nonzero balance of (every registry token is pinned for each watched address, so they show once the first step, which runs as soon as the node serves rather than at the next head, has seeded and proven them; zero balances are omitted), then a `contracts` section when the config tracks any. It prints no nonce. Check each name against the ENS UniversalResolver at chibi's head, the same `reverse` call chibi proves: `B=$(cast block-number --rpc-url $C); cast call 0xeEeEEEeE14D718C2B47D9923Deab1335E144EeEe 'reverse(bytes,uint256)(string,address,address)' <address> 60 --block $B --rpc-url $O | head -1 | tee -a "$RUN_DIR/evidence/ens-oracle.txt"`: a name it prints is shown by chibi, and an address it returns `""` for, or that reverts (a name served through a CCIP-read gateway), shows none. `cast lookup-address` is not a check: it goes through the old reverse registrar and reports `ENS resolver not found` on Sepolia even for named addresses. `cleanup.sh` saves the same view after the node stops as `status-after-stop.txt`.
- **Proof.** Keep the `head verified` lines, both oracle checks, `doctor.txt`, `ens-oracle.txt`, and the status files.

## Gotchas

- `step failed ... no connected peer serves state (snap)` followed by no new `head verified` lines is the stale-head state: chibi keeps serving its last head and `eth_syncing` still answers `false`. It is a real behaviour to report, not a harness fault.
- helios attests a head about 4 s into each slot, and under Gloas one slot late, so chibi's head trails the explorer by a block or two.
- `chibi status` values are a display snapshot from disk. They prove persistence, not verification.
- After the fork, an `Extended` step makes no proof requests for keys chibi holds; only unheld keys are warmed. Compare the values with `oracle-check.sh` anyway; that is the only check that the access list was applied correctly.
- Before the fork, learned keys only partly re-warm between heads, so a repeated `eth_call` may still fetch. That is expected.
- Learned keys survive a restart in `chibi.db`, so a second `chibi node` on the same data directory logs a larger `anchored ... keys=` than the first and warms those keys in the background, most recently used first.
