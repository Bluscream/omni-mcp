# Running omni-mcp on Unraid

## Before you start

This container exposes tools that read and, if you enable it, write files. It is
not a public service. Keep it on a trusted network, give it a token, and mount
only the paths it needs.

Generate a token:

```bash
head -c 32 /dev/urandom | base64
```

The container refuses to start in HTTP mode without one.

## Install

1. Copy `my-omni-mcp.xml` to your Unraid flash drive at
   `/boot/config/plugins/dockerMan/templates-user/my-omni-mcp.xml`.
2. **Docker → Add Container**, then pick **omni-mcp** from the template list.
3. Put your `omni-mcp.toml` at `/mnt/user/appdata/omni-mcp/omni-mcp.toml`.
4. Paste the token into **Auth Token**.
5. **Apply**.

Check it came up:

```bash
curl -s http://<UNRAID_IP>:8080/health
```

## Connect an IDE

```json
{
  "mcpServers": {
    "omni-mcp": {
      "url": "http://<UNRAID_IP>:8080/mcp",
      "headers": { "Authorization": "Bearer <YOUR_TOKEN>" }
    }
  }
}
```

## Enabling the filesystem tools

By default nothing can be modified. To allow it, mount a workspace and confine
the tools to it in `omni-mcp.toml`:

```toml
[tools]
allow_file_mutation = true
allowed_roots = ["/workspace"]
```

Leave `allow_code_execution` off unless you specifically need `eval_code`, and
understand that it runs arbitrary commands inside the container.

## Troubleshooting

Call the `omni_status` tool — it reports every backend's health and the reason
for any discovery failure. For startup problems, check the container log; all
diagnostics go to stderr.
