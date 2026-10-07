#!/bin/sh
# Runs MetaMask's own network client and native-balance code against a launch.sh instance.
# Usage: wallet-check.sh RUN_DIR ADDRESS NAME [check.ts options]
# check.ts options: --seconds N, --listen-blocks, --start-after S, --surface assets|account-tracker.
# Writes RUN_DIR/evidence/wallet-NAME.txt and wallet-NAME.jsonl (every request on the wire).
# Exits 0 when every balance read succeeded and the balance loaded, 1 otherwise.
set -eu

run_dir=${1:?usage: wallet-check.sh RUN_DIR ADDRESS NAME [check.ts options]}
address=${2:?usage: wallet-check.sh RUN_DIR ADDRESS NAME [check.ts options]}
name=${3:?usage: wallet-check.sh RUN_DIR ADDRESS NAME [check.ts options]}
shift 3
# shellcheck disable=SC1091
. "$run_dir/meta.env"

wallet=$(cd "$(dirname "$0")/../wallet" && pwd)
[ -d "$wallet/node_modules" ] || (cd "$wallet" && bun install --frozen-lockfile >&2)

cd "$wallet"
bun run check.ts --rpc "http://127.0.0.1:$PORT" --address "$address" \
    --out "$run_dir/evidence/wallet-$name.jsonl" "$@" >"$run_dir/evidence/wallet-$name.txt" 2>&1
