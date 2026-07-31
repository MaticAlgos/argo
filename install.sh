#!/usr/bin/env bash
# Build Argo from source and install it to ~/.local/bin.
#
# Works both from a cloned checkout (`./install.sh`) and directly from GitHub:
#   curl -fsSL https://raw.githubusercontent.com/MaticAlgos/argo/main/install.sh | bash
set -euo pipefail

REPOSITORY="MaticAlgos/argo"
REF="${ARGO_INSTALL_REF:-main}"
INSTALL_DIR="${ARGO_INSTALL_DIR:-${ARGO_PREFIX:-$HOME/.local/bin}}"
TEMP_DIR=""

cleanup() {
  if [[ -n "$TEMP_DIR" && -d "$TEMP_DIR" ]]; then
    rm -rf "$TEMP_DIR"
  fi
}
trap cleanup EXIT INT TERM

fail() {
  echo "error: $*" >&2
  exit 1
}

command -v cargo >/dev/null 2>&1 || \
  fail "cargo not found. Install Rust 1.82 or newer from https://rustup.rs"
command -v rustc >/dev/null 2>&1 || \
  fail "rustc not found. Install Rust 1.82 or newer from https://rustup.rs"

# BASH_SOURCE names a real file when this script runs from a checkout. When the
# script arrives through stdin, download a clean GitHub source archive instead.
SCRIPT_SOURCE="${BASH_SOURCE[0]:-}"
SOURCE_DIR=""
if [[ -n "$SCRIPT_SOURCE" && -f "$SCRIPT_SOURCE" ]]; then
  CANDIDATE="$(cd -- "$(dirname -- "$SCRIPT_SOURCE")" && pwd)"
  if [[ -f "$CANDIDATE/Cargo.toml" && -d "$CANDIDATE/crates" ]]; then
    SOURCE_DIR="$CANDIDATE"
  fi
fi

if [[ -z "$SOURCE_DIR" ]]; then
  command -v curl >/dev/null 2>&1 || fail "curl is required for one-shot installation"
  command -v tar >/dev/null 2>&1 || fail "tar is required for one-shot installation"

  TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/argo-install.XXXXXX")"
  ARCHIVE="$TEMP_DIR/argo.tar.gz"
  URL="https://codeload.github.com/$REPOSITORY/tar.gz/$REF"
  echo "downloading Argo source ($REF)..."
  CURL_AUTH=()
  if [[ -n "${GITHUB_TOKEN:-}" ]]; then
    CURL_AUTH+=(--header "Authorization: Bearer $GITHUB_TOKEN")
  fi
  curl --fail --silent --show-error --location --retry 3 \
    "${CURL_AUTH[@]}" --output "$ARCHIVE" "$URL"
  tar -xzf "$ARCHIVE" -C "$TEMP_DIR"
  MANIFEST="$(find "$TEMP_DIR" -mindepth 2 -maxdepth 2 -name Cargo.toml -print -quit)"
  [[ -n "$MANIFEST" ]] || fail "downloaded archive did not contain an Argo workspace"
  SOURCE_DIR="$(dirname -- "$MANIFEST")"
fi

echo "building Argo (release)..."
cargo build --manifest-path "$SOURCE_DIR/Cargo.toml" --release --locked

mkdir -p "$INSTALL_DIR"

# Stop an older daemon before replacing its client binary. Failure is harmless
# when no daemon is running or this is the first installation.
if [[ -x "$INSTALL_DIR/argo" ]]; then
  "$INSTALL_DIR/argo" stop >/dev/null 2>&1 || true
fi

# Replacing a Mach-O binary in place can invalidate its signature, so remove the
# destination before copying and ad-hoc sign the completed file on macOS.
rm -f "$INSTALL_DIR/argo"
install -m 755 "$SOURCE_DIR/target/release/argo" "$INSTALL_DIR/argo"
if [[ "$(uname -s)" == "Darwin" ]] && command -v codesign >/dev/null 2>&1; then
  codesign --force --sign - "$INSTALL_DIR/argo" >/dev/null 2>&1 || true
fi

echo "installed $("$INSTALL_DIR/argo" --version) -> $INSTALL_DIR/argo"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo
    echo "note: $INSTALL_DIR is not on your PATH. Add it:"
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac

echo
echo "next: cd your-project && argo"
