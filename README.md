# omni-mcp

> **High-performance consolidated Model Context Protocol (MCP) router and native module suite in Rust.**

`omni-mcp` replaces heavy NodeJS/Python/Podman process chains with a single, ultra-fast, memory-safe Rust daemon that executes native tools in **< 1ms** using **~15 MB RAM**.

---

## Features

- **Native Rust Modules**: Instant execution for `diff`, `common` (regex/count), `resx`, `everything`, `eval`.
- **Zero Container Lockups**: Eliminates process storms and kernel OverlayFS lockups.
- **Graceful Fault Tolerance**: Unavailable proxy endpoints never crash the daemon or IDE pipe.
- **`omni_status` Diagnostic Tool**: Real-time diagnostic feedback provided directly to LLM agents.
- **Declarative Proxy & Sidecar Config**: Add external HTTP/SSE MCPs or local scripts in `omni-mcp.toml` without writing code.
- **Docker & Unraid Support**: Single-stage & multi-stage Docker builds, plus Unraid CA templates.

---

## Quick Start

### Running Locally
```bash
cargo run --release
```

### IDE Configuration

Add to your `mcpServers` settings in Antigravity IDE or Claude Desktop:

```json
{
  "mcpServers": {
    "omni-mcp": {
      "command": "/path/to/target/release/omni-mcp",
      "args": ["--stdio"],
      "env": {
        "OMNI_MCP_CONFIG": "/path/to/omni-mcp.toml"
      }
    }
  }
}
```

---

## Docker & Unraid Deployment

See [docker/Dockerfile](file:///run/media/system/Data/Projects/MCPs/omni-mcp/docker/Dockerfile) and [unraid/README.md](file:///run/media/system/Data/Projects/MCPs/omni-mcp/unraid/README.md).

---

## License

[UNLICENSE](file:///run/media/system/Data/Projects/MCPs/omni-mcp/LICENSE) (Public Domain)
