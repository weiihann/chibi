#!/bin/sh
# Checks chibi's compiled-in token registry against each network's chain, using public RPCs
# as a test oracle only: every token has code, and its symbol() and decimals() match.
# Usage: scripts/registry-check.sh
# Env: SEPOLIA_RPC, MAINNET_RPC (default publicnode). Requires foundry's `cast`.
set -u

SEPOLIA_RPC=${SEPOLIA_RPC:-https://ethereum-sepolia-rpc.publicnode.com}
MAINNET_RPC=${MAINNET_RPC:-https://ethereum-rpc.publicnode.com}
failures=0

registry=$(cargo run -q -p chibi-node --example registry) || exit 2

while IFS="$(printf '\t')" read -r network symbol address decimals; do
    case $network in
        sepolia) rpc=$SEPOLIA_RPC ;;
        mainnet) rpc=$MAINNET_RPC ;;
        *) echo "FAIL  $network $symbol: unknown network"; failures=$((failures + 1)); continue ;;
    esac
    code=$(cast codesize "$address" --rpc-url "$rpc" 2>/dev/null || echo 0)
    onchain_symbol=$(cast call "$address" 'symbol()(string)' --rpc-url "$rpc" 2>/dev/null | tr -d '"')
    onchain_decimals=$(cast call "$address" 'decimals()(uint8)' --rpc-url "$rpc" 2>/dev/null)
    if [ "$code" -gt 0 ] && [ "$onchain_symbol" = "$symbol" ] && [ "$onchain_decimals" = "$decimals" ]; then
        echo "OK    $network $symbol $address decimals=$decimals code=${code}B"
    else
        echo "FAIL  $network $symbol $address: code=${code}B symbol=$onchain_symbol decimals=$onchain_decimals (registry $symbol, $decimals)"
        failures=$((failures + 1))
    fi
done <<EOF
$registry
EOF

echo "$failures failure(s)"
[ "$failures" -eq 0 ]
