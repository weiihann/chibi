#!/bin/sh
# Installs chibi from this checkout and writes its config. It does not start chibi.
set -eu

repo=$(cd "$(dirname "$0")" && pwd)

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo not found. Install Rust first:" >&2
    echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
    exit 1
fi

cargo install --locked --path "$repo/crates/cli"
# cargo's bin directory may not be on PATH yet right after a fresh rustup install.
bin="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin"

echo
if [ -t 0 ]; then
    "$bin/chibi" init
else
    echo "Input is not a terminal, so \`chibi init\` was not run. chibi runs on the defaults without a config; run \`chibi init\` to choose others."
fi

echo
if ! command -v chibi >/dev/null 2>&1; then
    echo "Add $bin to your PATH to use the commands below."
fi
echo "Start chibi:      chibi node      (foreground; Ctrl-C stops it)"
echo "              or  chibi node -b   (background; \`chibi stop\` stops it)"
echo "Check on chibi:   chibi status"
