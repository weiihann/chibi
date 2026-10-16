#!/bin/sh
# Installs the chibi binary from a GitHub release. It does not run chibi.
#
#   curl -fsSL https://raw.githubusercontent.com/weiihann/chibi/main/install.sh | sh
#
# CHIBI_VERSION      release tag to install, such as v0.1.0 (default: the latest release)
# CHIBI_INSTALL_DIR  where to put chibi (default: $HOME/.local/bin)
# CHIBI_RELEASE_URL  where to download the release files from; accepts file:// URLs
set -eu

repo_url="https://github.com/weiihann/chibi"
from_source="cargo install --locked --git $repo_url chibi"

err() {
    echo "error: $*" >&2
    exit 1
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Linux)
            case "$arch" in
                x86_64 | amd64) echo x86_64-unknown-linux-gnu ;;
                aarch64 | arm64) echo aarch64-unknown-linux-gnu ;;
                *) unsupported ;;
            esac
            ;;
        Darwin)
            case "$arch" in
                x86_64)
                    # A shell under Rosetta reports x86_64 on Apple silicon.
                    if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
                        echo aarch64-apple-darwin
                    else
                        echo x86_64-apple-darwin
                    fi
                    ;;
                arm64 | aarch64) echo aarch64-apple-darwin ;;
                *) unsupported ;;
            esac
            ;;
        *) unsupported ;;
    esac
}

unsupported() {
    {
        echo "error: there is no prebuilt chibi for $os $arch."
        echo "Build it from source instead (needs Rust, https://rustup.rs):"
        echo "  $from_source"
    } >&2
    exit 1
}

download() {
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https,file' --proto-redir '=https' --tlsv1.2 -fsSL -o "$2" "$1"
    elif [ "$scheme" = file ]; then
        err "file:// URLs need curl, which is not installed"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --https-only --max-redirect=5 -O "$2" "$1"
    else
        err "this script needs curl or wget"
    fi
}

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    err "this script needs sha256sum or shasum to check the download"
fi

target=$(detect_target)

if [ -n "${CHIBI_RELEASE_URL:-}" ]; then
    base_url=${CHIBI_RELEASE_URL%/}
elif [ -n "${CHIBI_VERSION:-}" ]; then
    base_url="$repo_url/releases/download/$CHIBI_VERSION"
else
    base_url="$repo_url/releases/latest/download"
fi
case "$base_url" in
https://*) scheme="https" ;;
file://*) scheme="file" ;;
*) err "release URL must start with https:// (or file:// for testing): $base_url" ;;
esac
install_dir=${CHIBI_INSTALL_DIR:-$HOME/.local/bin}
asset="chibi-$target.tar.gz"

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t chibi)
stage=
trap 'rm -rf "$tmp"; [ -z "$stage" ] || rm -f "$stage"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

echo "Downloading $base_url/$asset"
download "$base_url/$asset" "$tmp/$asset" || err "could not download $base_url/$asset"
download "$base_url/$asset.sha256" "$tmp/$asset.sha256" ||
    err "could not download $base_url/$asset.sha256"

malformed() { err "checksum file for $asset is malformed"; }
[ "$(wc -l <"$tmp/$asset.sha256")" -le 1 ] || malformed
record=$(cat "$tmp/$asset.sha256")
expected=${record%% *}
case "${record#"$expected"}" in
"  $asset" | " *$asset") ;;
*) malformed ;;
esac
[ "${#expected}" -eq 64 ] || malformed
case "$expected" in *[!0-9a-f]*) malformed ;; esac
actual=$(sha256 "$tmp/$asset")
if [ "$expected" != "$actual" ]; then
    {
        echo "error: checksum mismatch for $asset"
        echo "  expected $expected"
        echo "  got      $actual"
        echo "Nothing was installed."
    } >&2
    exit 1
fi
echo "Checksum OK ($actual)"

mkdir -p "$tmp/unpack"
tar -xzf "$tmp/$asset" -C "$tmp/unpack"
[ -f "$tmp/unpack/chibi" ] || err "$asset does not contain chibi"

mkdir -p "$install_dir"
# Stage then rename, so a running chibi is never overwritten in place.
stage=$(mktemp "$install_dir/.chibi.XXXXXX")
cp "$tmp/unpack/chibi" "$stage"
chmod 0755 "$stage"
mv -f "$stage" "$install_dir/chibi"
stage=

echo
echo "Installed $("$install_dir/chibi" --version) to $install_dir/chibi"

case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *)
        echo
        echo "$install_dir is not on your PATH. Add it, for example:"
        echo "  export PATH=\"$install_dir:\$PATH\""
        ;;
esac

echo
echo "Next steps:"
echo "  chibi init      write the config (optional; chibi runs on the defaults without one)"
echo "  chibi node      follow the chain and serve JSON-RPC to your browser wallet"
echo "  chibi wallet    run the node under a terminal UI that sends from your keystore"
