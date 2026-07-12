#!/usr/bin/env sh
# nrg install script (roadmap 3.1) — downloads a prebuilt binary from a GitHub Release
# (published by .github/workflows/release.yml) and installs it onto PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/inou/nrgize-rs/main/scripts/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --version v0.2.0
#   curl -fsSL .../install.sh | sh -s -- --bin-dir /usr/local/bin
#
# POSIX sh only (no bashisms) so it runs correctly under `sh` regardless of the caller's
# default shell — this script is meant to be piped into `sh`, not necessarily `bash`.
set -eu

REPO="inou/nrgize-rs"
BIN_DIR="${NRG_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${NRG_VERSION:-latest}"
print_target_only=0

usage() {
    cat <<'EOF'
Usage: install.sh [--version vX.Y.Z] [--bin-dir DIR] [--print-target]

  --version vX.Y.Z   Install a specific release instead of the latest.
  --bin-dir DIR       Install directory (default: $HOME/.local/bin).
  --print-target      Print the resolved target triple and exit (no network access).

Env vars: NRG_VERSION, NRG_INSTALL_DIR (defaults for the flags above).
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || { echo "install.sh: --version needs an argument" >&2; exit 1; }
            VERSION="$2"; shift 2 ;;
        --bin-dir)
            [ $# -ge 2 ] || { echo "install.sh: --bin-dir needs an argument" >&2; exit 1; }
            BIN_DIR="$2"; shift 2 ;;
        --print-target) print_target_only=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install.sh: unknown argument: $1" >&2; usage >&2; exit 1 ;;
    esac
done

# Validate before touching the network — a version string flows straight into a download URL,
# so reject anything that doesn't look like a release tag rather than handing curl a surprise.
case "$VERSION" in
    latest) ;;
    v[0-9]*) ;;
    *) echo "install.sh: --version must look like vX.Y.Z (got: $VERSION)" >&2; exit 1 ;;
esac

# NRG_TEST_UNAME_S / NRG_TEST_UNAME_M let tests exercise every OS/arch branch below without
# needing to run on that actual hardware.
uname_s="${NRG_TEST_UNAME_S:-$(uname -s)}"
uname_m="${NRG_TEST_UNAME_M:-$(uname -m)}"

case "$uname_s" in
    Darwin) os_part="apple-darwin" ;;
    Linux) os_part="unknown-linux-gnu" ;;
    *) echo "install.sh: unsupported OS: $uname_s (nrg ships prebuilt binaries for Linux and macOS only — see docs/getting-started.md for building from source)" >&2; exit 1 ;;
esac

case "$uname_m" in
    x86_64|amd64) arch_part="x86_64" ;;
    arm64|aarch64) arch_part="aarch64" ;;
    *) echo "install.sh: unsupported architecture: $uname_m (nrg ships prebuilt binaries for x86_64 and arm64 only — see docs/getting-started.md for building from source)" >&2; exit 1 ;;
esac

target="${arch_part}-${os_part}"

if [ "$print_target_only" -eq 1 ]; then
    echo "$target"
    exit 0
fi

if [ -n "${NRG_TEST_BASE_URL:-}" ]; then
    # Test-only override so the download → verify → install pipeline is exercisable against a
    # local HTTP server instead of a real GitHub Release — see tests/install_script.rs.
    asset_base="$NRG_TEST_BASE_URL"
elif [ "$VERSION" = "latest" ]; then
    asset_base="https://github.com/$REPO/releases/latest/download"
else
    asset_base="https://github.com/$REPO/releases/download/$VERSION"
fi

archive="nrg-${target}.tar.gz"
url="$asset_base/$archive"
checksum_url="$url.sha256"

verify_checksum() {
    # Both tools accept the same "<hash>  <filename>" format, so either can verify a checksum
    # file produced by the other — this just widens which minimal environments this works on.
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c "$1"
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$1"
    else
        echo "install.sh: neither shasum nor sha256sum found; cannot verify download integrity" >&2
        exit 1
    fi
}

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

echo "Downloading $url ..." >&2
curl -fsSL "$url" -o "$tmpdir/$archive"
curl -fsSL "$checksum_url" -o "$tmpdir/$archive.sha256"

( cd "$tmpdir" && verify_checksum "$archive.sha256" ) \
    || { echo "install.sh: checksum verification failed for $archive — refusing to install" >&2; exit 1; }

tar xzf "$tmpdir/$archive" -C "$tmpdir"
mkdir -p "$BIN_DIR"
cp "$tmpdir/nrg" "$BIN_DIR/nrg"
chmod +x "$BIN_DIR/nrg"

echo "Installed nrg ($VERSION, $target) to $BIN_DIR/nrg" >&2
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "note: $BIN_DIR is not on your PATH — add it, e.g.: export PATH=\"$BIN_DIR:\$PATH\"" >&2 ;;
esac
