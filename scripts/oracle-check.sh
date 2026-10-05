#!/bin/sh
# Compares a running chibi against a public RPC used only as a test oracle.
# Usage: scripts/oracle-check.sh ADDRESS [ADDRESS...]
# Env: CHIBI (default http://127.0.0.1:8545), ORACLE (default publicnode Sepolia),
#      TOKEN (default Sepolia USDC). Requires foundry's `cast` and curl.
set -u

CHIBI=${CHIBI:-http://127.0.0.1:8545}
ORACLE=${ORACLE:-https://ethereum-sepolia-rpc.publicnode.com}
TOKEN=${TOKEN:-0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238}
failures=0

[ $# -ge 1 ] || { echo "usage: $0 ADDRESS [ADDRESS...]" >&2; exit 2; }

report() {
    if [ "$2" = "$3" ]; then
        echo "MATCH $1: $2"
    else
        echo "DIFF  $1: chibi=$2 oracle=$3"
        failures=$((failures + 1))
    fi
}

# chibi serves state only at its verified head, so each comparison pins that block and
# retries when the head moves between the two calls.
at_head() {
    label=$1
    shift
    for _ in 1 2 3; do
        block=$(cast block-number --rpc-url "$CHIBI") || break
        mine=$("$@" --rpc-url "$CHIBI" --block "$block" 2>/dev/null) || continue
        theirs=$("$@" --rpc-url "$ORACLE" --block "$block")
        report "$label @$block" "$mine" "$theirs"
        return
    done
    echo "SKIP  $label: chibi head kept moving"
}

report "chain id" "$(cast chain-id --rpc-url "$CHIBI")" "$(cast chain-id --rpc-url "$ORACLE")"
block=$(cast block-number --rpc-url "$CHIBI")
report "header hash @$block" "$(cast block "$block" -f hash --rpc-url "$CHIBI")" \
    "$(cast block "$block" -f hash --rpc-url "$ORACLE")"

for address in "$@"; do
    at_head "$address balance" cast balance "$address"
    at_head "$address nonce" cast nonce "$address"
    at_head "$address token balance" cast call "$TOKEN" 'balanceOf(address)(uint256)' "$address"
done

from=$1
at_head "estimate ETH transfer" cast estimate "$from" --value 1 --from "$from"
at_head "estimate token transfer" cast estimate "$TOKEN" 'transfer(address,uint256)' "$from" 1 --from "$from"
echo "INFO  gas price: chibi=$(cast gas-price --rpc-url "$CHIBI") oracle=$(cast gas-price --rpc-url "$ORACLE")"

body='{"jsonrpc":"2.0","id":1,"method":"eth_getLogs","params":[{}]}'
code=$(curl -s "$CHIBI" -H 'content-type: application/json' -d "$body" | sed -n 's/.*"code":\(-*[0-9]*\).*/\1/p')
report "eth_getLogs error code" "$code" "-32004"

body='{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'
status=$(curl -s -o /dev/null -w '%{http_code}' "$CHIBI" -H 'Host: evil.example' -H 'content-type: application/json' -d "$body")
report "rebound Host header status" "$status" "403"
allowed=$(curl -s -D - -o /dev/null "$CHIBI" -H 'Origin: https://evil.example' -H 'content-type: application/json' -d "$body" | grep -ci 'access-control-allow-origin')
report "CORS headers for a website origin" "$allowed" "0"

echo "$failures difference(s)"
[ "$failures" -eq 0 ]
