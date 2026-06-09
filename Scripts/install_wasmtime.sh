#!/bin/bash
set -e

WASMTIME_VERSION="${WASMTIME_VERSION:-latest}"

echo "Installing wasmtime..."

# Detect OS and architecture
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$ARCH" in
    x86_64)  ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *)
        echo "Unsupported architecture: $ARCH"
        exit 1
        ;;
esac

case "$OS" in
    linux)  PLATFORM="${ARCH}-linux" ;;
    darwin) PLATFORM="${ARCH}-macos" ;;
    *)
        echo "Unsupported OS: $OS"
        exit 1
        ;;
esac

# Resolve latest version if needed
if [ "$WASMTIME_VERSION" = "latest" ]; then
    echo "Fetching latest wasmtime version..."
    WASMTIME_VERSION="$(curl -fsSL https://api.github.com/repos/bytecodealliance/wasmtime/releases/latest \
        | grep '"tag_name"' | sed 's/.*"tag_name": *"v\([^"]*\)".*/\1/')"
fi

echo "Version: v${WASMTIME_VERSION}, Platform: ${PLATFORM}"

TARBALL="wasmtime-v${WASMTIME_VERSION}-${PLATFORM}.tar.xz"
URL="https://github.com/bytecodealliance/wasmtime/releases/download/v${WASMTIME_VERSION}/${TARBALL}"
TMP_DIR="$(mktemp -d)"

trap 'rm -rf "$TMP_DIR"' EXIT

echo "Downloading $URL ..."
curl -fsSL "$URL" -o "$TMP_DIR/$TARBALL"

echo "Extracting..."
tar -xf "$TMP_DIR/$TARBALL" -C "$TMP_DIR"

INSTALL_DIR="${INSTALL_DIR:-$HOME/.wasmtime/bin}"
mkdir -p "$INSTALL_DIR"

BINARY_DIR="$TMP_DIR/wasmtime-v${WASMTIME_VERSION}-${PLATFORM}"
cp "$BINARY_DIR/wasmtime" "$INSTALL_DIR/wasmtime"
chmod +x "$INSTALL_DIR/wasmtime"

echo "wasmtime installed to $INSTALL_DIR/wasmtime"

# Add to PATH if not already present
SHELL_RC=""
case "$SHELL" in
    */zsh)  SHELL_RC="$HOME/.zshrc" ;;
    */fish) SHELL_RC="$HOME/.config/fish/config.fish" ;;
    *)      SHELL_RC="$HOME/.bashrc" ;;
esac

if ! echo "$PATH" | grep -q "$INSTALL_DIR"; then
    echo "" >> "$SHELL_RC"
    echo "# wasmtime" >> "$SHELL_RC"
    echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$SHELL_RC"
    echo "Added $INSTALL_DIR to PATH in $SHELL_RC"
    echo "Run: source $SHELL_RC  (or open a new shell)"
fi

echo ""
echo "Verify installation:"
"$INSTALL_DIR/wasmtime" --version
