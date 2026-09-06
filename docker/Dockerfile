# ==========================================
# Stage 1: Build binary
# ==========================================
FROM rust:1.80-slim-bookworm AS builder

WORKDIR /usr/src/omni-mcp
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

# ==========================================
# Stage 2: Runtime Image
# ==========================================
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    python3 \
    nodejs \
    bash \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /usr/src/omni-mcp/target/release/omni-mcp /app/omni-mcp
COPY omni-mcp.toml /app/omni-mcp.toml

ENV OMNI_MCP_CONFIG=/app/omni-mcp.toml
EXPOSE 8080

ENTRYPOINT ["/app/omni-mcp"]
