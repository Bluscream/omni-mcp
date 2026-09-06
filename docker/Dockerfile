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

# `eval_code` is disabled by default, so the interpreters it would drive are not
# installed by default either. Enable them only if you intend to turn on
# tools.allow_code_execution:
#   podman build --build-arg INCLUDE_INTERPRETERS=true ...
ARG INCLUDE_INTERPRETERS=false

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && if [ "$INCLUDE_INTERPRETERS" = "true" ]; then \
           apt-get install -y --no-install-recommends python3 nodejs; \
       fi \
    && rm -rf /var/lib/apt/lists/*

# Never run the gateway as root: its tools touch the filesystem. A regular (not
# --system) account, because a system account must have a uid below SYS_UID_MAX.
RUN useradd --create-home --uid 10001 --shell /usr/sbin/nologin omni

# No --chown, which is what we want: COPY defaults to root:root and preserves
# the source mode, so `omni` can read and execute these but never rewrite its
# own binary or configuration.
COPY --from=builder /src/target/release/omni-mcp /usr/local/bin/omni-mcp
COPY omni-mcp.toml /etc/omni-mcp/omni-mcp.toml

USER omni
WORKDIR /home/omni

ENV OMNI_MCP_CONFIG=/etc/omni-mcp/omni-mcp.toml \
    RUST_LOG=info

EXPOSE 8080

# No HEALTHCHECK here: it is a Docker-format extension that OCI images (podman's
# default) ignore with a warning. docker-compose.yml defines it instead, which
# works for both runtimes.

ENTRYPOINT ["/usr/local/bin/omni-mcp"]
# HTTP mode requires server.auth_token; set OMNI_MCP_TOKEN in the environment.
CMD ["serve", "--bind", "0.0.0.0:8080"]
