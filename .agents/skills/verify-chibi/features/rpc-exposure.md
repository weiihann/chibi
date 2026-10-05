# RPC exposure

chibi's JSON-RPC answers the wallet's browser extension on loopback, refuses requests whose `Host` header names another site, sends no CORS headers to ordinary websites, and reports what it cannot serve with a clear error instead of a made-up answer.

## Sub-features

- `expose-host` refuses a rebound `Host` header with HTTP 403.
- `expose-cors` sends `access-control-allow-origin` only to `chrome-extension://` and `moz-extension://` origins.
- `expose-unsupported` answers history, logs, and filters with error `-32004` naming the verified head.
- `expose-block` serves `eth_getBlockByNumber("latest")` with the verified header and an empty transaction list.

## How to get to it (user POV)

- The wallet extension calls the RPC from its background page.
- A malicious website tries to read the RPC from a browser tab.
- A dapp asks the wallet for logs or old state.

## Driving it with verify-chibi scripts

Preconditions:

- `doctor.sh "$RUN_DIR"` exits 0.
- `C=http://127.0.0.1:$PORT` and `BODY='{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'`.

- **Host filter.** Run `curl -s -o /dev/null -w '%{http_code}\n' $C -H 'Host: evil.example' -H 'content-type: application/json' -d "$BODY"`. It prints `403`. The same request with `-H "Host: 127.0.0.1:$PORT"` prints `200`.
- **CORS.** Run `curl -s -D - -o /dev/null $C -H 'Origin: https://evil.example' -H 'content-type: application/json' -d "$BODY"`. The headers contain no `access-control-allow-origin`. Repeat with `-H 'Origin: chrome-extension://nkbihfbeogaeaoehlefnkodbefgpgknn'`; the headers contain `access-control-allow-origin: chrome-extension://nkbihfbeogaeaoehlefnkodbefgpgknn`.
- **Unsupported.** Run `.agents/skills/verify-chibi/scripts/rpc.sh "$RUN_DIR" eth_getLogs '[{}]'` and `rpc.sh "$RUN_DIR" eth_getBalance '["0x3d767bf85779e2b550ab5fcd7e3be39fb77a5ce6","earliest"]'`. Both return `"code":-32004` with a message ending `(verified head is <N>)`.
- **Block.** Run `rpc.sh "$RUN_DIR" eth_getBlockByNumber '["latest",false]'`. The result's `hash` equals `cast block <number> -f hash --rpc-url $O` and `transactions` is `[]`.
- **Proof.** Keep the curl outputs (pipe them into `$RUN_DIR/evidence/exposure.txt`) and `rpc.log`. `scripts/oracle-check.sh` also asserts the Host, CORS, and `eth_getLogs` cases.

## Gotchas

- `curl` sends `Host: 127.0.0.1:<port>` by default, so a plain request passing proves nothing about the filter. Always send the rebound host explicitly.
- A preflight `OPTIONS` from a refused origin still carries `access-control-allow-methods`; only `access-control-allow-origin` decides what a browser allows.
- A custom `rpc_host` domain is added to the allowed `Host` values. Test the filter with a host that is neither the configured `rpc_host` nor in `allowed_hosts`.
