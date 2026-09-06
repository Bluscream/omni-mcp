#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

BINARY_PATH="$PROJECT_DIR/target/release/omni-mcp"
INSTALL_BIN_DIR="$HOME/.local/bin"
INSTALL_CONFIG_DIR="$HOME/.config/omni-mcp"

DEPLOY=false

for arg in "$@"; do
    case "$arg" in
        --deploy)
            DEPLOY=true
            ;;
        --help|-h)
            echo "Usage: $0 [--deploy]"
            echo ""
            echo "Options:"
            echo "  --deploy    Compiles release binary and installs it to $INSTALL_BIN_DIR/omni-mcp"
            echo "  --help      Show this help message"
            exit 0
            ;;
        *)
            echo "Unknown argument: $arg"
            exit 1
            ;;
    esac
done

echo "==> Building omni-mcp in release mode..."
cd "$PROJECT_DIR"
cargo clippy --quiet
cargo test --quiet
cargo build --release

if [ ! -f "$BINARY_PATH" ]; then
    echo "Error: Release binary not found at $BINARY_PATH"
    exit 1
fi

echo "✓ Build completed successfully!"

if [ "$DEPLOY" = true ]; then
    echo "==> Deploying omni-mcp to system paths..."
    
    mkdir -p "$INSTALL_BIN_DIR" "$INSTALL_CONFIG_DIR"
    
    cp "$BINARY_PATH" "$INSTALL_BIN_DIR/omni-mcp.new"
    mv -f "$INSTALL_BIN_DIR/omni-mcp.new" "$INSTALL_BIN_DIR/omni-mcp"
    chmod +x "$INSTALL_BIN_DIR/omni-mcp"
    
    if [ -f "$PROJECT_DIR/omni-mcp.toml" ] && [ ! -f "$INSTALL_CONFIG_DIR/omni-mcp.toml" ]; then
        cp "$PROJECT_DIR/omni-mcp.toml" "$INSTALL_CONFIG_DIR/omni-mcp.toml"
        echo "✓ Copied omni-mcp.toml to $INSTALL_CONFIG_DIR/omni-mcp.toml"
    fi

    echo "✓ Binary deployed to $INSTALL_BIN_DIR/omni-mcp"
    echo "✓ Testing deployed binary..."
    "$INSTALL_BIN_DIR/omni-mcp" --help >/dev/null && echo "✓ Deployed binary is working!"
fi
