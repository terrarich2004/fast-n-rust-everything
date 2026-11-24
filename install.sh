#!/bin/sh
set -eu

REPO="${FNR_REPO:-terrarich2004/fast-n-rust-everything}"
INSTALL_DIR="${FNR_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${FNR_VERSION:-}"

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
  cat <<EOF
Install fast-n-rust (fnr) prebuilt binaries on Linux or macOS.

Environment variables:
  FNR_VERSION      Release tag (e.g. v0.2.3). Defaults to latest GitHub release.
  FNR_INSTALL_DIR  Destination directory (default: \$HOME/.local/bin).
  FNR_REPO         GitHub owner/repo (default: terrarich2004/fast-n-rust-everything).
EOF
  exit 0
fi

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)  OS_PART="unknown-linux-gnu" ;;
  Darwin) OS_PART="apple-darwin" ;;
  *)
    echo "Unsupported OS: $OS. Install via Cargo instead:" >&2
    echo "  cargo install --git https://github.com/$REPO fast-n-rust" >&2
    exit 1
    ;;
esac

case "$ARCH" in
  x86_64|amd64)  ARCH_PART="x86_64" ;;
  aarch64|arm64) ARCH_PART="aarch64" ;;
  *)
    echo "Unsupported architecture: $ARCH. Install via Cargo instead:" >&2
    echo "  cargo install --git https://github.com/$REPO fast-n-rust" >&2
    exit 1
    ;;
esac

TARGET="${ARCH_PART}-${OS_PART}"

if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required. Install via Cargo instead:" >&2
  echo "  cargo install --git https://github.com/$REPO fast-n-rust" >&2
  exit 1
fi

if [ -z "$VERSION" ]; then
  LATEST_URL="$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" 2>/dev/null || true)"
  case "$LATEST_URL" in
    */tag/*) VERSION="${LATEST_URL##*/tag/}" ;;
    *) VERSION="v0.2.3" ;;
  esac
fi

ASSET="fast-n-rust-${VERSION}-${TARGET}.tar.gz"
BASE_URL="https://github.com/$REPO/releases/download/${VERSION}"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

echo "Downloading fast-n-rust ${VERSION} (${TARGET})..."
if ! curl -fsSL "${BASE_URL}/${ASSET}" -o "${TMP_DIR}/${ASSET}"; then
  echo "" >&2
  echo "Prebuilt binary ${ASSET} was not found at ${BASE_URL}." >&2
  echo "You can install directly from source with Rust/Cargo:" >&2
  echo "  cargo install --git https://github.com/$REPO fast-n-rust" >&2
  exit 1
fi

if curl -fsSL "${BASE_URL}/checksums.txt" -o "${TMP_DIR}/checksums.txt" 2>/dev/null; then
  EXPECTED="$(awk -v f="$ASSET" '$2 == f || $2 == "*"f || $2 ~ "/"f"$" { print $1; exit }' "${TMP_DIR}/checksums.txt")"
  if [ -n "$EXPECTED" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
      ACTUAL="$(sha256sum "${TMP_DIR}/${ASSET}" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
      ACTUAL="$(shasum -a 256 "${TMP_DIR}/${ASSET}" | awk '{print $1}')"
    else
      ACTUAL=""
    fi
    if [ -n "$ACTUAL" ] && [ "$ACTUAL" != "$EXPECTED" ]; then
      echo "SHA-256 verification failed for ${ASSET}!" >&2
      exit 1
    fi
  fi
fi

tar -xzf "${TMP_DIR}/${ASSET}" -C "$TMP_DIR"
mkdir -p "$INSTALL_DIR"
install -m 0755 "${TMP_DIR}/fast-n-rust" "${INSTALL_DIR}/fast-n-rust"
if [ -f "${TMP_DIR}/fnr" ]; then
  install -m 0755 "${TMP_DIR}/fnr" "${INSTALL_DIR}/fnr"
else
  ln -sf fast-n-rust "${INSTALL_DIR}/fnr" || cp -f "${INSTALL_DIR}/fast-n-rust" "${INSTALL_DIR}/fnr"
fi

echo "Installed fast-n-rust and fnr to ${INSTALL_DIR}"
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    UPDATED_RC=""
    if [ "${FNR_NO_MODIFY_PATH:-0}" != "1" ]; then
      RC_FILE=""
      case "${SHELL:-}" in
        */zsh) RC_FILE="$HOME/.zshrc" ;;
        */bash)
          if [ -f "$HOME/.bashrc" ]; then
            RC_FILE="$HOME/.bashrc"
          else
            RC_FILE="$HOME/.profile"
          fi
          ;;
        *)
          if [ "$OS" = "Darwin" ]; then
            RC_FILE="$HOME/.zshrc"
          elif [ -f "$HOME/.bashrc" ]; then
            RC_FILE="$HOME/.bashrc"
          else
            RC_FILE="$HOME/.profile"
          fi
          ;;
      esac
      if [ -n "$RC_FILE" ]; then
        if [ ! -f "$RC_FILE" ] || (! grep -Fq "$INSTALL_DIR" "$RC_FILE" && ! grep -Fq "# >>> fast-n-rust >>>" "$RC_FILE"); then
          mkdir -p "$(dirname "$RC_FILE")"
          printf '\n# >>> fast-n-rust >>>\nexport PATH="%s:$PATH"\n# <<< fast-n-rust <<<\n' "$INSTALL_DIR" >> "$RC_FILE"
          UPDATED_RC="$RC_FILE"
        fi
      fi
    fi
    if [ -n "$UPDATED_RC" ]; then
      echo "Added ${INSTALL_DIR} to ${UPDATED_RC}."
    fi
    echo "Run this in your current terminal (or open a new one):"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\" && fnr setup"
    ;;
esac
"${INSTALL_DIR}/fnr" --version || true
