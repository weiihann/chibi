#!/bin/sh
# Read-only health check for an instance started by launch.sh.
# Usage: doctor.sh RUN_DIR
# Exits 0 only when the instance is ours, serving the expected chain, and at the chain head.
set -u

run_dir=${1:?usage: doctor.sh RUN_DIR}
# shellcheck disable=SC1091
. "$run_dir/meta.env"
case $NETWORK in
    sepolia) expect_chain=0xaa36a7; oracle=${ORACLE:-https://ethereum-sepolia-rpc.publicnode.com} ;;
    mainnet) expect_chain=0x1; oracle=${ORACLE:-https://ethereum-rpc.publicnode.com} ;;
    *) echo "FAIL unknown network $NETWORK"; exit 1 ;;
esac
rpc="http://127.0.0.1:$PORT"
failures=0
ok() { echo "OK    $1"; }
fail() { echo "FAIL  $1"; failures=$((failures + 1)); }
warn() { echo "WARN  $1"; }

pid=$(cat "$run_dir/chibi.pid" 2>/dev/null || echo "")
if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && ps -p "$pid" -o command= | grep -q "$run_dir/bin/chibi"; then
    ok "process $pid runs $run_dir/bin/chibi"
else
    fail "no chibi process from this run (pid '${pid}')"
fi

listener=$(lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null | head -1)
[ "$listener" = "$pid" ] && ok "port $PORT is held by pid $pid" || fail "port $PORT is held by '${listener:-nobody}', not $pid"

log=$("$(dirname "$0")/log.sh" "$run_dir")
printf '%s\n' "$log" | grep -q "data_dir=$run_dir/home" && ok "data dir is inside $run_dir/home" \
    || fail "log does not show a data dir under $run_dir/home"

head_rev=$(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)
[ "$head_rev" = "$REV" ] && ok "binary built from HEAD $REV (dirty files at build: $DIRTY)" \
    || warn "binary built from $REV, repo HEAD is now $head_rev"

call() {
    curl -s -m 20 "$rpc" -H 'content-type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":${2:-[]}}"
}
chain=$(call eth_chainId | sed -n 's/.*"result":"\([^"]*\)".*/\1/p')
[ "$chain" = "$expect_chain" ] && ok "eth_chainId $chain" || fail "eth_chainId '${chain}' expected $expect_chain"

ours=$(call eth_blockNumber | sed -n 's/.*"result":"\([^"]*\)".*/\1/p')
theirs=$(curl -s -m 20 "$oracle" -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' | sed -n 's/.*"result":"\([^"]*\)".*/\1/p')
if [ -n "$ours" ] && [ -n "$theirs" ]; then
    lag=$(( $(printf '%d' "$theirs") - $(printf '%d' "$ours") ))
    if [ "$lag" -le 3 ]; then
        ok "head $(printf '%d' "$ours") is $lag block(s) behind the oracle"
    else
        fail "head $(printf '%d' "$ours") is $lag blocks behind the oracle (stale head)"
        printf '%s\n' "$log" | grep "step failed" | tail -2
    fi
else
    fail "could not read eth_blockNumber (chibi '${ours}', oracle '${theirs}')"
fi

last=$(printf '%s\n' "$log" | grep -o 'stepped=[A-Za-z]*' | tail -1)
echo "INFO  last head step: ${last:-none yet (anchored only)}"
[ "$failures" -eq 0 ]
