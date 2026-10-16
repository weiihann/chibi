#!/bin/sh
# Prints a launch.sh run's node log without colour codes, so fields such as `stepped=Extended`
# and `state_peers=` match a plain grep. chibi colours its log when it runs in the foreground.
# Usage: log.sh RUN_DIR
# Reads RUN_DIR/chibi.log while the run lives and RUN_DIR/evidence/chibi.log after cleanup.sh.
set -eu

run_dir=${1:?usage: log.sh RUN_DIR}
log=$run_dir/chibi.log
[ -f "$log" ] || log=$run_dir/evidence/chibi.log
[ -f "$log" ] || { echo "no chibi.log in $run_dir or $run_dir/evidence" >&2; exit 1; }
esc=$(printf '\033')
sed "s/${esc}\[[0-9;]*m//g" "$log"
