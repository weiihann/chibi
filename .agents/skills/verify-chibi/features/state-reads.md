# Verified state reads

A wallet reads balances, nonces, token balances, contract code, and storage through chibi, runs `eth_call` and `eth_estimateGas`, and gets the same answers a full node gives at the same block, for watched addresses and for addresses chibi has never seen.

## Sub-features

- `read-account` returns ETH balance and nonce for watched and unwatched addresses.
- `read-token` returns an ERC-20 balance through `eth_call(balanceOf)`, including through the USDC proxy.
- `read-code-storage` returns contract code and a raw storage slot.
- `read-estimate` estimates gas for an ETH transfer and a token transfer.
- `read-miss` fetches a key chibi never held, with a proof, instead of answering zero.

## How to get to it (user POV)

- The wallet's balance and token views call `eth_getBalance`, `eth_getTransactionCount`, and `eth_call` on `http://127.0.0.1:$PORT`.
- The wallet's send screen calls `eth_estimateGas`.
- Dapps call `eth_getCode`, `eth_getStorageAt`, and `eth_call` through the wallet.

## Driving it with verify-chibi scripts

Preconditions:

- `doctor.sh "$RUN_DIR"` exits 0.
- `C=http://127.0.0.1:$PORT` from `$RUN_DIR/meta.env`.

- **Bulk comparison.** Compare watched and unwatched addresses in one go. Run `CHIBI=$C scripts/oracle-check.sh 0x3d767bf85779e2b550ab5fcd7e3be39fb77a5ce6 0xe1f0ba7cf9175f61ad1fe29ee4f30249871bfc29 0x662d80b69803cd79e3489d805c605cb3adecf4f1 | tee "$RUN_DIR/evidence/oracle-check.txt"`. Every line reads `MATCH` with a block number, the last line reads `0 difference(s)`, and the exit code is 0. This covers `read-account`, `read-token`, `read-estimate`, and `read-miss` (the third address is not watched).
- **Code.** Read the USDC proxy code at chibi's head. Run `B=$(cast block-number --rpc-url $C); cast code 0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238 --block $B --rpc-url $C | shasum` and the same with `--rpc-url $O`. Both digests are equal.
- **Storage.** Read the USDC implementation slot. Run `cast storage 0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238 0x7050c9e0f4ca769c69bd3a8ef740bc37934f8e2c036e5a723fd8ee048ed3f8c3 --block $B --rpc-url $C` and the same with `--rpc-url $O`. Both return the same 32-byte word (an address in the low 20 bytes).
- **Raw call.** Read a token balance the way a wallet does. Run `.agents/skills/verify-chibi/scripts/rpc.sh "$RUN_DIR" eth_call '[{"to":"0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238","data":"0x70a082310000000000000000000000003d767bf85779e2b550ab5fcd7e3be39fb77a5ce6"},"latest"]'`. The result is a 32-byte word equal to `cast call 0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238 'balanceOf(address)(uint256)' 0x3d767bf85779e2b550ab5fcd7e3be39fb77a5ce6 --block <head> --rpc-url $O` at the same head.
- **Proof.** Keep `oracle-check.txt` and `rpc.log`. Note the block numbers they name.

## Gotchas

- A `stale head` from `doctor.sh` makes every comparison meaningless, because chibi keeps answering at its old block. Fix peers first or relaunch.
- `SKIP ... chibi head kept moving` in `oracle-check.sh` usually means chibi's request failed (no snap/2 peer for a miss), not that the head moved. Check `chibi.log` for `step failed` or run `doctor.sh`.
- `eth_gasPrice` differs from the oracle by design (different tip oracle); the script prints it as `INFO`, not a comparison.
- After the fork, gas estimates change (EIP-8037: an ETH transfer that creates an account costs 204600). Compare against the oracle at the same block, never against numbers recorded before the fork.
- JSON-RPC opens before anything is proven. Right after start, and after a reanchor, reads fetch their keys on demand, so the first reads are slower; every key read is then kept (up to 1024, least recently used dropped) and re-proved at each head.
