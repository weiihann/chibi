# Account history

With history on, chibi keeps a permanent record of what happened to the watched accounts: sends, ETH moves (EIP-7708 logs), token, NFT and ERC-1155 transfers, approvals, delegations and withdrawals. Every row comes from a block chibi verified. Forward from the moment it starts, chibi records each extended block; in the background it walks back to the Glamsterdam fork and fills in older blocks whose header bloom may name an account. `chibi status` and `h` in `chibi wallet` show the history.

## Sub-features

- `history-optin`: `chibi init --history` writes `history = true`; without it chibi fetches and writes nothing for history.
- `history-forward`: each extended block is recorded; a send through chibi shows as a send and its ETH move.
- `history-backfill`: settled blocks below the forward start gain logs coverage down to the fork, and older activity appears.
- `history-oracle`: over every covered range, the logs naming the account match the oracle exactly.
- `history-restart`: a restart keeps the history, reconciles it, and fills the stretch it missed once finality passes it.
- `history-status`: `chibi status` prints the coverage line and the newest activities under each watched address.
- `history-wallet`: `h` in `chibi wallet` opens the same history in a pane.

## How to get to it (user POV)

- `chibi init` asks "Keep a transaction history for watched accounts?"; `--history` answers yes.
- `chibi status` prints, under each watched address, a line such as `history since block 11,941,326 · gaps: none · sends from nonce 36 complete` and the five newest activities.
- In `chibi wallet`, `h` opens the history pane wherever `l` opens the logs; `h` or Esc closes it.

## Driving it with verify-chibi scripts

Preconditions:

- A run started with `launch.sh --history ADDRESS...`; `doctor.sh "$RUN_DIR"` exits 0.
- For `history-forward`, a funded Sepolia key, as in [send and confirm](./send-and-confirm.md).
- The oracle for logs is Tenderly's public gateway (the default in `history-check.py`): publicnode refuses `eth_getLogs` without a contract address.

- **Opt-in.** `grep history "$RUN_DIR/home/.config/chibi/config.toml"` prints `history = true`, and `ls -l` on the run's `chibi.db` shows `-rw-------`.
- **Forward.** Send 1 gwei to a fresh address (`cast wallet new` prints one) through chibi, as in send and confirm. The receipt has one log, the EIP-7708 transfer. Then run `chibi status` with the run's environment: the two newest lines are `-0.000000001 ETH to <address>` and `sent to <address> (nonce N, fee ...)` at the receipt's block. A send to the account itself emits no EIP-7708 log, and shows its value in the send line instead.
- **Backfill.** Nothing walks back until finality passes the forward start, about 13 minutes after launch. Then `log.sh "$RUN_DIR" | grep 'older history added'` prints lines with `blocks=` and `reached=`, and `reached` falls each head. On 2026-10-21 one step covered 85 to 201 blocks.
- **Oracle.** Run `python3 .agents/skills/verify-chibi/scripts/history-check.py "$RUN_DIR" ADDRESS > "$RUN_DIR/evidence/history-check.txt"; echo $?`. It prints one line per covered range, such as `blocks 11941567-11962598: oracle 4 logs, chibi 4, missing 0, extra 0`, and exits 0 only when nothing is missing or extra. Check the exit code of the script itself, not of a pipe.
- **Restart.** Stop the node with SIGINT on the pid in `chibi.pid` (not `cleanup.sh`, which deletes the data) and wait until `lsof -nP -iTCP:PORT -sTCP:LISTEN` prints nothing; copying a new binary over a running one fails with `Text file busy`. After a minute, start `bin/chibi node` again with the run's environment, appending to `chibi.log` under a marker line of its own. `log.sh` prints `history reconciled tip=<last recorded block>`, and `history_coverage` (read-only, `sqlite3 "file:...chibi.db?mode=ro"`) shows a gap from that tip to the new anchor. Once finality passes the anchor, the logs coverage fills the gap and `history_settled` moves past it; the BAL coverage keeps the gap.
- **Wallet.** Stop the node, then start `chibi wallet --rpc-port PORT` with the run's environment in tmux. With no keystore configured, the setup menu opens with the `>` on `just watch balances, without a wallet`; press Enter and wait for the balances. Press `h` and capture the pane: it shows the address, the coverage line and the activities. In the `to` field of the send form `h` types instead, as `l` does.

## Gotchas

- Coverage claims only what the committed filters can prove: logs naming the account as emitter or topic 1-3, and the account's own BAL changes for forward blocks. Withdrawals and fee credits are complete only where BAL coverage is.
- Backward blocks never get BAL coverage, so a restart's downtime stays a gap there for good; the coverage line reports logs coverage.
- A self-send emits no EIP-7708 log, so the oracle has nothing to compare for it.
- `chibi status` reads history read-only beside a running node; a long write can make it wait up to the 5 s busy timeout.
- The walk back covers about 10 blocks a second on Sepolia, so reaching the fork from today takes hours; it never delays the head by more than the 2 s history budget.
- A re-anchor (`cannot extend with BALs; reanchoring` in the log) leaves its anchor block unrecorded, and the walk waits there until finality passes it before filling anything older. On 2026-10-21 that held a three-block restart gap open for 17 minutes, until `older history added blocks=4` filled both gaps in one step.
