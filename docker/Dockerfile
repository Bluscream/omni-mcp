# syntax=docker/dockerfile:1

# ------------------------------------------------------------------------------
# Build
# ------------------------------------------------------------------------------
FROM rust:1-slim-bookworm AS builder

WORKDIR /src

# Compile dependencies against a stub first so that editing application source
# does not invalidate the dependency layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY tests ./tests
# Touch so cargo rebuilds the crate itself rather than reusing the stub.
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# ------------------------------------------------------------------------------
# Runtime
# ------------------------------------------------------------------------------
FROM debian:bookworm-slim

# `eval_code` is disabled by default; these interpreters are only useful if the
# operator enables it. Drop this layer for a smaller, more locked-down image.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        python3 \
        nodejs \
    && rm -rf /var/lib/apt/lists/*

# Never run the gateway as root: its tools touch the filesystem.
RUN useradd --system --create-home --uid 10001 omni
USER omni
WORKDIR /home/omni

COPY --from=builder --chown=omni:omni /src/target/release/omni-mcp /usr/local/bin/omni-mcp
COPY --chown=omni:omni omni-mcp.toml /etc/omni-mcp/omni-mcp.toml

ENV OMNI_MCP_CONFIG=/etc/omni-mcp/omni-mcp.toml \
    RUST_LOG=info

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/omni-mcp", "--config", "/etc/omni-mcp/omni-mcp.toml", "check"]

ENTRYPOINT ["/usr/local/bin/omni-mcp"]
# HTTP mode requires server.auth_token; set OMNI_MCP_TOKEN in the environment.
CMD ["serve", "--bind", "0.0.0.0:8080"]
