#!/usr/bin/env bash
#
# Build, verify and optionally install omni-mcp.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

BINARY="$PROJECT_DIR/target/release/omni-mcp"
INSTALL_BIN_DIR="${OMNI_MCP_BIN_DIR:-$HOME/.local/bin}"
INSTALL_CONFIG_DIR="${OMNI_MCP_CONFIG_DIR:-$HOME/.config/omni-mcp}"

deploy=false
skip_checks=false

usage() {
    cat <<USAGE
Usage: ${0##*/} [--deploy] [--skip-checks]

  --deploy       Install the release binary to $INSTALL_BIN_DIR/omni-mcp
                 and seed $INSTALL_CONFIG_DIR/omni-mcp.toml if absent.
  --skip-checks  Skip clippy and the test suite. Not recommended.
  --help         Show this message.

Environment:
  OMNI_MCP_BIN_DIR      Install prefix for the binary.
  OMNI_MCP_CONFIG_DIR   Install prefix for the configuration.
USAGE
}

for arg in "$@"; do
    case "$arg" in
        --deploy) deploy=true ;;
        --skip-checks) skip_checks=true ;;
        --help | -h) usage; exit 0 ;;
        *) echo "unknown argument: $arg" >&2; usage >&2; exit 2 ;;
    esac
done

cd "$PROJECT_DIR"

if [ "$skip_checks" = false ]; then
    echo "==> clippy"
    cargo clippy --all-targets --locked -- -D warnings

    echo "==> tests"
    cargo test --locked
fi

echo "==> release build"
cargo build --release --locked

if [ ! -x "$BINARY" ]; then
    echo "error: expected a release binary at $BINARY" >&2
    exit 1
fi

# A binary that cannot answer --version is not one worth installing.
"$BINARY" --version >/dev/null
echo "==> built $("$BINARY" --version)"

if [ "$deploy" = false ]; then
    echo "Run with --deploy to install."
    exit 0
fi

echo "==> installing to $INSTALL_BIN_DIR"
mkdir -p "$INSTALL_BIN_DIR" "$INSTALL_CONFIG_DIR"

# Install via a temporary name and rename, so a running process is replaced
# atomically rather than being overwritten underneath itself.
install -m 0755 "$BINARY" "$INSTALL_BIN_DIR/.omni-mcp.new"
mv -f "$INSTALL_BIN_DIR/.omni-mcp.new" "$INSTALL_BIN_DIR/omni-mcp"

if [ ! -f "$INSTALL_CONFIG_DIR/omni-mcp.toml" ]; then
    install -m 0600 "$PROJECT_DIR/omni-mcp.toml" "$INSTALL_CONFIG_DIR/omni-mcp.toml"
    echo "==> seeded $INSTALL_CONFIG_DIR/omni-mcp.toml"
else
    echo "==> kept existing $INSTALL_CONFIG_DIR/omni-mcp.toml"
fi

"$INSTALL_BIN_DIR/omni-mcp" --config "$INSTALL_CONFIG_DIR/omni-mcp.toml" check

case ":$PATH:" in
    *":$INSTALL_BIN_DIR:"*) ;;
    *) echo "note: $INSTALL_BIN_DIR is not on your PATH" ;;
esac

echo "==> done"
