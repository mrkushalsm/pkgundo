#!/usr/bin/env sh
# One-shot installer for pkgundo: builds from source (no prebuilt-binary
# releases exist yet) and wires it into the system.
#
# Usage:
#   curl -fsSL <url-to-this-file> | sh
#
# This script deliberately stays "dumb" — it only fetches the source,
# builds it, and drops the binary in place. Every actual system change
# (installing the daemon's systemd unit, enabling it, installing the
# package-manager hooks) lives in `pkgundo setup` itself, in Rust, where
# it's covered by the same VM regression tests as everything else pkgundo
# does. This script just gets that binary onto the machine and calls it.
#
# Override REPO_URL to point at wherever this repo actually lives once
# it's published — this is a placeholder until then:
#   REPO_URL=https://github.com/you/pkgundo.git curl -fsSL ... | sh

set -eu

REPO_URL="${PKGUNDO_REPO_URL:-https://example.invalid/pkgundo.git}"
INSTALL_DIR="${PKGUNDO_INSTALL_DIR:-/usr/local/bin}"

log() { printf '==> %s\n' "$1"; }
die() { printf 'error: %s\n' "$1" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1; }

need git || die "git is required but not found. Install it and re-run."
need curl || die "curl is required but not found. Install it and re-run."

if [ "$REPO_URL" = "https://example.invalid/pkgundo.git" ]; then
    die "REPO_URL isn't set to a real repository yet. Set \$PKGUNDO_REPO_URL and re-run: PKGUNDO_REPO_URL=https://github.com/you/pkgundo.git sh install.sh"
fi

if ! need cargo; then
    log "No Rust toolchain found — installing one via rustup (non-interactive)."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

log "Fetching pkgundo source ($REPO_URL) ..."
git clone --depth 1 "$REPO_URL" "$WORKDIR/pkgundo"

log "Building pkgundo (release mode) — this can take a few minutes ..."
(cd "$WORKDIR/pkgundo" && cargo build --release --quiet)

log "Installing binary to $INSTALL_DIR/pkgundo (requires sudo) ..."
sudo install -m 755 "$WORKDIR/pkgundo/target/release/pkgundo" "$INSTALL_DIR/pkgundo"

log "Running 'pkgundo setup' (installs+starts the daemon, installs package-manager hooks) ..."
sudo "$INSTALL_DIR/pkgundo" setup

log "Done. 'pkgundo track <app>' to start watching something."
log "To undo everything: sudo pkgundo setup --remove && sudo rm $INSTALL_DIR/pkgundo"
