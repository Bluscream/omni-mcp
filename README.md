# omni-mcp

One process that serves native Rust tools, supervises local MCP servers, and
proxies remote ones — behind a single stdio or HTTP endpoint.

The point is **containment**. Running a dozen MCP servers as independent,
container-wrapped processes is what caused the incident this project was written
to prevent: parallel `podman inspect` calls hit OverlayFS lock contention, put
more than 50 processes into uninterruptible sleep, and drove load average past
68. So every path here that can create work is bounded — sidecar spawns are
serialised, concurrent calls are capped, every call has a deadline, and no
request ever fans out to a backend that does not own the tool being called.

---

## Quick start

```bash
cargo build --release
```

Point your IDE at the binary:

```json
{
  "mcpServers": {
    "omni-mcp": {
      "command": "/path/to/omni-mcp/target/release/omni-mcp",
      "args": ["stdio"],
      "env": { "OMNI_MCP_CONFIG": "/path/to/omni-mcp.toml" }
    }
  }
}
```

Check what a configuration resolves to before wiring it up:

```bash
omni-mcp --config omni-mcp.toml check
```

List every tool the gateway would advertise:

```bash
omni-mcp tools
```

---

## Tools

| Tool | What it does |
| --- | --- |
| `omni_status` | Which tools exist and how every backend is doing. Start here when something is missing. |
| `diff_text`, `diff_json` | Unified diffs. `diff_json` compares structurally, so key order and formatting are ignored. |
| `regex_match` | All matches with byte offsets and capture groups. |
| `count_stats` | Lines, words, Unicode characters, bytes. |
| `grep_search` | Content search across a tree, gitignore-aware and binary-skipping. Optional in-place replace. |
| `hex_view`, `hex_patch` | Hex dump of a byte range; pattern or offset patching with a backup. |
| `read_resx`, `write_resx_entry` | .NET `.resx` resources, structurally correct. |
| `everything_search` | Filename search via a Voidtools Everything HTTP server. |
| `eval_code` | Runs a short script. **Off by default** — see below. |

Every tool also accepts an optional `timeout` (seconds), clamped to
`limits.max_tool_timeout`.

---

## Security

Two capabilities are disabled unless you turn them on, because this gateway can
be reached over HTTP and runs as your user:

```toml
[tools]
allow_code_execution = true   # eval_code
allow_file_mutation  = true   # grep_search --apply, hex_patch, write_resx_entry
allowed_roots = ["/home/you/projects"]   # confine the filesystem tools
```

`omni-mcp serve` refuses to start without `server.auth_token` unless you pass
`--allow-unauthenticated`. CORS is closed unless `server.allowed_origins` lists
origins explicitly.

Secrets never belong in the config file. `${VAR}` and `${VAR:-default}` are
expanded from the environment when string values are read:

```toml
[[proxies]]
name = "homeassistant"
url = "${HOME_ASSISTANT_URL:-http://homeassistant.local:8123/api/mcp}"
bearer = "${HOME_ASSISTANT_TOKEN}"
prefix = "ha_"
```

A referenced variable that is not set is a startup error, not a silently empty
credential.

---

## Backends

Three kinds, all routed through one explicit tool-name table:

- **native** — Rust code in this binary. Microseconds, no process.
- **sidecar** — a child process speaking MCP over stdio. Spawned lazily on first
  use, serialised by `limits.max_concurrent_spawns`, killed on drop, restarted
  if it dies.
- **proxy** — a remote MCP server over HTTP, plain JSON-RPC or SSE-framed.

Two backends exposing the same tool name would collide, so give one a `prefix`
(`ha_`) and both stay reachable.

Run sidecars natively rather than through `distrobox enter` or `podman run`. The
per-call container wrapper is what produced the original lock contention.

---

## Configuration reference

See the annotated [omni-mcp.toml](omni-mcp.toml). Every key has a default;
an empty file is a valid configuration.

Durations are written as `"30s"`, `"1500ms"`, `"2m"` or `"1h"`.

---

## Development

```bash
cargo test                       # unit + end-to-end
cargo clippy --all-targets       # pedantic, warning-free
./scripts/build.sh --deploy      # release build, install to ~/.local/bin
```

Tests cover the protocol surface, each tool's edge cases, and a real
spawn-and-speak-JSON-RPC session against the built binary.

---

## License

[Unlicense](LICENSE) (public domain).
