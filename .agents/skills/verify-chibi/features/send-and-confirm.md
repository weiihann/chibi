# Send and confirm

A wallet signs a transaction and sends it through chibi. chibi checks it, broadcasts it to devp2p peers, finds it in a verified block, and returns a receipt checked against that block's receipts root.

## Sub-features

- `send-prepare` returns the pending nonce, fee data, and a gas estimate for the transaction.
- `send-broadcast` accepts `eth_sendRawTransaction` and returns the transaction hash.
- `send-pending` reports the transaction as pending through `eth_getTransactionByHash` before inclusion.
- `send-receipt` returns a verified receipt and the including block after inclusion.
- `send-reject` refuses blob transactions, transactions for another chain, empty input, and bytes that do not decode as a transaction.

## How to get to it (user POV)

- The wallet's send flow calls `eth_chainId`, `eth_getTransactionCount` (`pending`), `eth_feeHistory` or `eth_maxPriorityFeePerGas`, `eth_estimateGas`, then `eth_sendRawTransaction`, then polls `eth_getTransactionReceipt`.
- `cast send --rpc-url $C` drives the same flow: nonce, fees, estimate, `eth_sendRawTransaction`, then receipt polling.

## Driving it with verify-chibi scripts

Preconditions:

- `doctor.sh "$RUN_DIR"` exits 0.
- A funded Sepolia keystore: `KS` is its path and `KS_PASSWORD` its password (`cast wallet import` writes one; with a raw key, use `--private-key` in place of `--keystore "$KS" --password "$KS_PASSWORD"` below). Its address is `cast wallet address --keystore "$KS" --password "$KS_PASSWORD"`. Without one, report `send-*` as skipped for that reason. Send a few gwei at most, to its own address.
- `TO` is any address you control or a watched address.

- **Send.** Send 1 gwei through chibi and wait for the receipt. Run `cast send "$TO" --value 1gwei --keystore "$KS" --password "$KS_PASSWORD" --rpc-url $C --timeout 300 --json | tee "$RUN_DIR/evidence/send.json"`. Exit code 0; the JSON has `"status":"0x1"`, a `transactionHash`, and a `blockNumber`. This drives `send-prepare`, `send-broadcast`, and `send-receipt` in one wallet-shaped flow.
- **Node side.** Run `.agents/skills/verify-chibi/scripts/log.sh "$RUN_DIR" | grep -E 'broadcast transaction|transaction included'`. A `broadcast transaction hash=<hash> sent=<N>` line (N peers, 1 to 8; the line can appear more than once for one hash) and one `transaction included hash=<hash> block=<N> index=<i>` line name the hash and block from `send.json`. With no peer to take it there is no `broadcast transaction` line; the log warns `broadcast reached no peer; will retry` instead.
- **Receipt matches the chain.** Run `H=<transactionHash>; cast receipt $H --rpc-url $C --json > "$RUN_DIR/evidence/receipt-chibi.json"; cast receipt $H --rpc-url $O --json > "$RUN_DIR/evidence/receipt-oracle.json"`. `status`, `blockHash`, `blockNumber`, `transactionIndex`, `gasUsed`, `cumulativeGasUsed`, `from`, `to`, and the number of logs are equal in both files.
- **Lookup.** Run `.agents/skills/verify-chibi/scripts/rpc.sh "$RUN_DIR" eth_getTransactionByHash "[\"$H\"]"`. The result shows the same `blockHash`, `blockNumber`, and `transactionIndex`.
- **Pending view.** To see `send-pending`, sign without waiting: `RAW=$(cast mktx "$TO" --value 1gwei --keystore "$KS" --password "$KS_PASSWORD" --rpc-url $C)`, then `rpc.sh "$RUN_DIR" eth_sendRawTransaction "[\"$RAW\"]"` and immediately `rpc.sh "$RUN_DIR" eth_getTransactionByHash '["<returned hash>"]'`. The result has `"blockHash":null` until the next verified head includes it, and `eth_getTransactionCount(<address>, "pending")` is one higher than `latest`.
- **Rejections.** These need no funds. Sign a legacy transfer for mainnet with the well-known Anvil test key: `RAW1=$(cast mktx 0x000000000000000000000000000000000000dEaD --value 1 --chain 1 --nonce 0 --gas-limit 21000 --gas-price 1gwei --legacy --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 --rpc-url $C)`, then `rpc.sh "$RUN_DIR" eth_sendRawTransaction "[\"$RAW1\"]"`. The response is error `-32000` with `invalid transaction: transaction is for another chain`. Then send `'["0x03"]'`, `'["0x"]'`, and `'["0x1234"]'` the same way: the responses are error `-32602` with `blob transactions are not supported`, error `-32602` with `empty transaction`, and error `-32000` with `invalid transaction: malformed EIP-2718 transaction`. The count from `.agents/skills/verify-chibi/scripts/log.sh "$RUN_DIR" | grep -c 'broadcast transaction'` is the same before and after all four.
- **Proof.** Keep `send.json`, both receipt files, `rpc.log`, and the `transaction included` line.

## Gotchas

- An unfunded or underpriced transaction still returns a hash, because chibi broadcasts without a mempool. Peers drop it silently and `cast send` times out. Check the balance with `cast balance` first.
- Receipts exist only for transactions sent through this chibi instance. Looking up someone else's transaction returns `null`.
- A stale head (see `doctor.sh`) stalls inclusion: the receipt never appears until a state peer (snap) returns.
- Never write the private key into evidence. `cast send --json` output does not contain it; keep it that way when adding commands.
- Pending transactions survive a restart through the outbox in the data directory's `chibi.db`. `cleanup.sh` deletes the data directory, so finish the receipt check before cleanup.
