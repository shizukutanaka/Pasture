#!/usr/bin/env sh
# Minimal bootstrap: build pasture and place it on your PATH.
# Usage:  sh install.sh        (override target dir with PASTURE_BIN_DIR=...)
set -eu

echo "Installing pasture..."

if ! command -v cargo >/dev/null 2>&1; then
  echo "Rust/cargo not found. Install it from https://rustup.rs then re-run." >&2
  exit 1
fi

cargo build --release

DEST="${PASTURE_BIN_DIR:-$HOME/.local/bin}"
mkdir -p "$DEST"
cp "target/release/pasture" "$DEST/pasture"
echo "Installed: $DEST/pasture"

case ":$PATH:" in
  *":$DEST:"*) : ;;
  *) echo "Add this to your shell profile:  export PATH=\"$DEST:\$PATH\"" ;;
esac

echo ""
echo "Next step:  pasture up    (downloads the model if needed, then starts the proxy)"
