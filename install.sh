#!/bin/sh
# Installs chibi from this checkout, writes its config on first use, and starts it.
set -eu

repo=$(cd "$(dirname "$0")" && pwd)

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo not found. Install Rust first:" >&2
    echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
    exit 1
fi

cargo install --locked --path "$repo/crates/cli"
# cargo's bin directory may not be on PATH yet right after a fresh rustup install.
chibi="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin/chibi"

if [ -t 0 ]; then
    "$chibi" init
fi

exec "$chibi" run
