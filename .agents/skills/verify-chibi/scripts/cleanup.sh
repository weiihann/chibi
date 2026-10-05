#!/bin/sh
# Stops the instance a launch.sh run started and removes its scratch state. Keeps evidence.
# Usage: cleanup.sh RUN_DIR
# Afterwards RUN_DIR holds only meta.env and evidence/ (including the node log).
set -eu

run_dir=${1:?usage: cleanup.sh RUN_DIR}
[ -f "$run_dir/meta.env" ] || { echo "$run_dir is not a launch.sh run directory" >&2; exit 1; }

pid=$(cat "$run_dir/chibi.pid" 2>/dev/null || echo "")
if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && ps -p "$pid" -o command= | grep -q "$run_dir/bin/chibi"; then
    kill -INT "$pid"
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        kill -0 "$pid" 2>/dev/null || break
        sleep 1
    done
    kill -0 "$pid" 2>/dev/null && kill -KILL "$pid"
fi

if [ -d "$run_dir/home" ]; then
    # chibi status reads the saved snapshot without a running node; keep that last view.
    env HOME="$run_dir/home" XDG_CONFIG_HOME="$run_dir/home/.config" \
        XDG_DATA_HOME="$run_dir/home/.local/share" \
        "$run_dir/bin/chibi" status >"$run_dir/evidence/status-after-stop.txt" 2>&1 || true
fi
[ -f "$run_dir/chibi.log" ] && mv "$run_dir/chibi.log" "$run_dir/evidence/chibi.log"
rm -rf "$run_dir/home" "$run_dir/bin" "$run_dir/chibi.pid"
echo "evidence kept in $run_dir/evidence"
