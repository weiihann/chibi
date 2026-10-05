#!/bin/sh
# Builds chibi, starts one isolated instance, and waits until its JSON-RPC answers.
# Usage: launch.sh [--network sepolia|mainnet] [--port N] [--timeout SECONDS] ADDRESS...
# Prints the run directory on the last line of stdout. Every other helper takes it.
set -eu

network=sepolia
port=8645
timeout=900
while [ $# -gt 0 ]; do
    case $1 in
        --network) network=$2; shift 2 ;;
        --port) port=$2; shift 2 ;;
        --timeout) timeout=$2; shift 2 ;;
        --) shift; break ;;
        -*) echo "unknown option $1" >&2; exit 2 ;;
        *) break ;;
    esac
done
[ $# -ge 1 ] || { echo "usage: $0 [--network N] [--port N] [--timeout S] ADDRESS..." >&2; exit 2; }

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
if lsof -nP -iTCP:"$port" -sTCP:LISTEN -t >/dev/null 2>&1; then
    echo "port $port is already in use; pass --port" >&2
    exit 1
fi

(cd "$repo" && cargo build --release -p chibi >&2)

run_id=$(date -u +%Y%m%dT%H%M%SZ)-$$
run_dir=${CHIBI_VERIFY_ROOT:-/tmp/chibi-verify}/$run_id
mkdir -p "$run_dir/home" "$run_dir/bin" "$run_dir/evidence"
cp "$repo/target/release/chibi" "$run_dir/bin/chibi"

cat > "$run_dir/meta.env" <<EOF
RUN_ID=$run_id
NETWORK=$network
PORT=$port
REV=$(git -C "$repo" rev-parse HEAD)
DIRTY=$(git -C "$repo" status --porcelain | wc -l | tr -d ' ')
REPO=$repo
EOF

# dirs::config_dir and dirs::data_dir follow HOME on macOS and the XDG variables on Linux.
export HOME="$run_dir/home"
export XDG_CONFIG_HOME="$run_dir/home/.config"
export XDG_DATA_HOME="$run_dir/home/.local/share"

addresses=""
for address in "$@"; do
    addresses="$addresses --address $address"
done
# shellcheck disable=SC2086
"$run_dir/bin/chibi" init --network "$network" $addresses --token USDC \
    --rpc-host 127.0.0.1 --rpc-port "$port" </dev/null >"$run_dir/evidence/init.txt"

# env and nohup exec in place, so $! is chibi's own pid.
env RUST_LOG="${CHIBI_VERIFY_LOG:-info,chibi::devp2p=debug}" \
    nohup "$run_dir/bin/chibi" run --rpc-host 127.0.0.1 --rpc-port "$port" >"$run_dir/chibi.log" 2>&1 &
echo $! >"$run_dir/chibi.pid"

waited=0
while ! grep -q "JSON-RPC on http://127.0.0.1:$port" "$run_dir/chibi.log" 2>/dev/null; do
    if ! kill -0 "$(cat "$run_dir/chibi.pid")" 2>/dev/null; then
        echo "chibi exited before serving JSON-RPC; last log lines:" >&2
        tail -20 "$run_dir/chibi.log" >&2
        exit 1
    fi
    if [ "$waited" -ge "$timeout" ]; then
        echo "no JSON-RPC after ${timeout}s; still running, inspect $run_dir/chibi.log or run cleanup.sh" >&2
        grep "waiting for a snap/2 peer" "$run_dir/chibi.log" | tail -1 >&2 || true
        exit 1
    fi
    sleep 2
    waited=$((waited + 2))
done

grep -E "anchored|JSON-RPC on" "$run_dir/chibi.log" >&2
echo "$run_dir"
