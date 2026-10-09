# Terminal UI

`chibi run --tui` runs the node in-process under a terminal UI while JSON-RPC keeps serving the wallet. Before the node starts it asks for the password of the user's keystore (`--keystore` or the config's `keystore`) and watches the decrypted address. A dozing cat shows the wait for a state peer with live peer counts, perks up at `found a state peer!`, and then a transfer form sends ETH or a registry token the address holds: validated per field, reviewed, signed, broadcast through chibi, and tracked to a verified receipt. Sending works only while a state peer is connected and only on Sepolia.

## Sub-features

- `tui-flags` `--tui` takes every `run` flag; `--tui` without a keystore fails naming `--keystore PATH` and the `keystore` config key; `--tui` with `-b` and `--logs` without `--tui` are refused by the parser; `chibi init --keystore PATH` writes `keystore`; a relative `keystore` in the config fails as not absolute.
- `tui-unlock` a masked password prompt; a wrong password re-asks with `wrong password, try again`; a missing or unreadable keystore exits before anything starts with `cannot read the keystore <path>`; Esc quits before the node starts.
- `tui-waiting` the dozing cat above `looking for a state peer · peers N · state peers M · <elapsed>`, updated at least once a second; then `found a state peer!` until anchored.
- `tui-form` `from` (checksummed address and ETH balance), `token` (ETH and held registry tokens, left/right), `amount` (decimal or `max`), `to`, and `fee` (max fee, gas, and gwei, refreshed each head; a revert shows its reason).
- `tui-validate` per-field messages that keep the input: `refusing to send to the zero address`, `the checksum does not match; check the address, or type it all lowercase`, `expected 0x followed by 40 hex digits`, `<SYMBOL> has at most <N> decimals`, `more than your balance of ...`, `the amount plus the max fee ... is more than your balance of ...`, `waiting for the fee estimate`.
- `tui-send` Review shows `sepolia (chain id 11155111)`, the full recipient, exact amount, and max fee; `y` signs an EIP-1559 transaction and submits it; the screen goes from the hash and `pending` to `included in a block; verifying the receipt...` to `sent! the receipt is verified with status 1` with a happy cat; `still pending after N blocks` after 10 heads.
- `tui-paused` with no state peer connected, a sleepy cat and `waiting for a state peer`; Enter does nothing; the form returns unchanged when one connects.
- `tui-mainnet` on mainnet the form shows balances and Review says `sending on mainnet is not enabled yet`; `y` signs nothing.
- `tui-keys` `l` toggles the log pane (the last log lines; nothing is printed to the terminal), `q` or Ctrl-C quits and stops the node as SIGINT does; with a transfer pending the first `q` warns that chibi's outbox rebroadcasts it on the next run.

## How to get to it (user POV)

- Import a key once with `cast wallet import <name> --interactive`, then `chibi init --keystore ~/.foundry/keystores/<name>` or pass `--keystore` to `chibi run --tui`.
- Run `chibi run --tui` (add `--logs` to start with the log pane open), type the password, wait for the cat, then fill the form and press Enter, then `y`.
- Point the wallet at the same URL as with `chibi run`; it keeps working while the TUI runs.

## Driving it with verify-chibi scripts

Preconditions:

- `launch.sh` does not apply: it starts a plain `chibi run`. Drive the TUI in tmux from a scratch HOME instead: `S=$(mktemp -d /tmp/chibi-tui.XXXX); mkdir -p "$S/evidence" "$S/ks"`, `E="env -u CHIBI_CONFIG HOME=$S XDG_CONFIG_HOME=$S/.config XDG_DATA_HOME=$S/.local/share"`, and `B=$PWD/target/release/chibi` after `cargo build --release -p chibi`.
- A free scratch port, for example 8750: `lsof -nP -iTCP:8750 -sTCP:LISTEN` prints nothing. Never use 8545 or 8546, and never the user's keystore.
- A scratch keystore: `cast wallet new "$S/ks" --unsafe-password hunter2` (unfunded; enough for every sub-feature but `tui-send`). For `tui-send`, a funded Sepolia key in `CHIBI_VERIFY_KEY` imported as `cast wallet import funded --private-key "$CHIBI_VERIFY_KEY" --unsafe-password hunter2 --keystore-dir "$S/ks"`; without it report `tui-send` as skipped for that reason. `KS` is the keystore file in `$S/ks`.
- `tmux`, `cast`, and outbound Sepolia access as in the [baseline](./README.md#baseline-preconditions).

- **Flags.** Run `$E $B run --tui --rpc-port 8750`; it exits non-zero with ``--tui needs a keystore: pass --keystore PATH, or set keystore in the config (`chibi init --keystore PATH` writes it)``. `$E $B run --tui --rpc-port 8750 --keystore "$S/ks/missing"` exits non-zero with `cannot read the keystore $S/ks/missing` and opens no port. `$E $B run --tui -b` and `$E $B run --logs` exit 2 with a usage error. `$E $B init --network sepolia --rpc-host 127.0.0.1 --rpc-port 8750 --keystore "$KS" </dev/null` writes `keystore = "<KS>"` into `$S/.config/chibi/config.toml`, so later runs need no `--keystore`. A config with `keystore = "rel.json"` fails with `keystore rel.json is not an absolute path`.
- **Unlock.** Run `tmux new-session -d -s chibi-tui -x 120 -y 40 "$E $B run --tui --rpc-port 8750 --logs; echo exit=\$?; sleep 600"`. `tmux capture-pane -p -t chibi-tui` shows `unlock your keystore to send from it` and `password`. Run `tmux send-keys -t chibi-tui 'wrong' Enter`; within a few seconds the pane shows `wrong password, try again`. Run `tmux send-keys -t chibi-tui 'hunter2' Enter`.
- **Waiting.** Capture the pane twice a few seconds apart: the cat (`( -.- )` or `( o.- )`) sits above `looking for a state peer · peers N · state peers 0 · <elapsed>`, and the elapsed time moves. The log pane shows `waiting for a state peer (snap/2)` and the `starting` line with `watched=1`, the keystore's address.
  The counts and elapsed time start once the light client has synced; until then the line reads `peers 0 · state peers 0 · 0s`.
- **Serving.** When a state peer connects the pane shows `found a state peer!`, then the form with `from <checksummed address>` and its ETH balance. In another shell, `cast chain-id --rpc-url http://127.0.0.1:8750` prints `11155111` and `cast balance <address> --rpc-url http://127.0.0.1:8750` matches the form's balance at that head.
- **Validate.** On the form, type `1`, press Down, type `0x0000000000000000000000000000000000000000`, press Enter: the pane shows `refusing to send to the zero address` and both inputs stay. Clear the recipient with `BSpace` and type `0x7f02C3E3c98b133055B8B348B2Ac625669Ed295d` (last letter case flipped), Enter: `the checksum does not match; check the address, or type it all lowercase`. With the unfunded keystore and a valid recipient, Enter shows `more than your balance of 0.00 ETH`.
- **Send.** With the funded keystore, set amount `0.000000001` and `to` an address you control, wait for the `fee` row to show `... ETH (<gas> gas at up to <gwei> gwei)`, press Enter. Review shows `sepolia (chain id 11155111)`, the checksummed recipient, `0.000000001 ETH`, and the max fee. Press `y`. The pane shows `hash 0x...`, then `pending`, then `sent! the receipt is verified with status 1` under the happy cat `( ^.^ )`. Then `cast receipt <hash> --rpc-url https://ethereum-sepolia-rpc.publicnode.com` shows `status 1` in the same block, and `cast receipt <hash> --rpc-url http://127.0.0.1:8750` agrees (chibi serves receipts for transactions it submitted).
- **Quit.** Press `q`; with a transfer still pending the pane first shows `a transfer is still pending; chibi's outbox rebroadcasts it on the next run. press q again to quit`, and a second `q` quits. The pane then shows `exit=0` on a normal screen (raw mode and the alternate screen restored), and `lsof -nP -iTCP:8750 -sTCP:LISTEN` prints nothing.
- **Mainnet.** Optional and slow (mainnet has no known state peers): run the TUI with `--network mainnet`; once the form opens, a valid transfer's Review says `sending on mainnet is not enabled yet` and `y` changes nothing.
- **Proof.** Before quitting each stage, `tmux capture-pane -p -t chibi-tui > "$S/evidence/<stage>.txt"`, with the log pane open (`l`) where the log matters. Keep the `cast` outputs next to them. Never capture the password or the private key.
- **Cleanup.** `tmux kill-session -t chibi-tui`, then remove `$S` except `$S/evidence`.

## Gotchas

- There is no `chibi.log`: under `--tui` the log goes to an in-memory buffer of the last 1000 lines, shown only in the log pane, and it is gone when the TUI exits. Capture the pane with the log pane open for node proof.
- Decrypting a `cast` keystore takes a second or two (scrypt); the prompt shows `unlocking...` meanwhile.
- `Paused` appears only when every state peer drops after serving, which cannot be forced reliably on Sepolia; report `tui-paused` as observed or as not driven, never as verified from unit tests.
- The fee row and `max` for ETH need a finished estimate; Enter before it shows `waiting for the fee estimate`.
- The `to` field accepts only hex digits and `x`, and `amount` only digits, `.`, and the letters of `max`, so `q` and `l` always act as keys.
- A transfer left pending stays in the scratch data directory's outbox; a later `chibi run` on the same data directory broadcasts it again.
