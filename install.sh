```sh
#!/bin/sh
set -eu

REPO="hellolio/latent"
INSTALL_DIR="${LATENT_INSTALL_DIR:-$HOME/.local/bin}"
BASE_URL="https://github.com/${REPO}/releases/latest/download"

fail() {
    echo "Error: $*" >&2
    exit 1
}

# Detect operating system.
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Darwin)
        case "$ARCH" in
            arm64|aarch64) PLATFORM="macos-aarch64" ;;
            *) fail "Unsupported macOS architecture: $ARCH" ;;
        esac
        ARCHIVE_EXT="tar.gz"
        ;;
    Linux)
        case "$ARCH" in
            x86_64|amd64) PLATFORM="linux-x86_64" ;;
            *) fail "Unsupported Linux architecture: $ARCH" ;;
        esac
        ARCHIVE_EXT="tar.gz"
        ;;
    *)
        fail "Unsupported operating system: $OS"
        ;;
esac

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT HUP INT TERM

ARCHIVE="latent-${PLATFORM}.${ARCHIVE_EXT}"
URL="${BASE_URL}/${ARCHIVE}"

echo "Downloading latent for ${PLATFORM}..."
curl -fsSL --retry 3 "$URL" -o "${TMP_DIR}/${ARCHIVE}" \
    || fail "Download failed: $URL"

tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "$TMP_DIR" \
    || fail "Failed to extract archive"

[ -f "${TMP_DIR}/latent" ] || fail "Binary not found in archive"

mkdir -p "$INSTALL_DIR"
install -m 755 "${TMP_DIR}/latent" "${INSTALL_DIR}/latent"

echo
echo "latent installed to ${INSTALL_DIR}/latent"

case ":${PATH}:" in
    *":${INSTALL_DIR}:"*)
        echo "Run 'latent' to get started."
        ;;
    *)
        echo
        echo "Add the install directory to your PATH:"
        echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
        echo
        echo "For zsh, add that line to ~/.zshrc, then run:"
        echo "  source ~/.zshrc"
        ;;
esac
```