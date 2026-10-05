#!/bin/sh
# Sends one JSON-RPC request to a launch.sh instance and records it as evidence.
# Usage: rpc.sh RUN_DIR METHOD [PARAMS_JSON]
# Prints the response body. Appends request and response to RUN_DIR/evidence/rpc.log.
set -eu

run_dir=${1:?usage: rpc.sh RUN_DIR METHOD [PARAMS_JSON]}
method=${2:?usage: rpc.sh RUN_DIR METHOD [PARAMS_JSON]}
params=${3:-[]}
# shellcheck disable=SC1091
. "$run_dir/meta.env"

body="{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}"
response=$(curl -s -m 60 "http://127.0.0.1:$PORT" -H 'content-type: application/json' -d "$body")
{
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) >> $body"
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) << $response"
} >>"$run_dir/evidence/rpc.log"
echo "$response"
