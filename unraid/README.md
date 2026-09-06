# Unraid Setup Guide for omni-mcp

This directory contains the template and instructions to deploy **`omni-mcp`** on your Unraid NAS.

## One-Click Template Installation

1. Copy [`my-omni-mcp.xml`](file:///run/media/system/Data/Projects/MCPs/omni-mcp/unraid/my-omni-mcp.xml) to your Unraid flash drive at:
   `/boot/config/plugins/dockerMan/templates-user/my-omni-mcp.xml`
2. Open the **Docker** tab in Unraid web UI and click **Add Container**.
3. Select **omni-mcp** from the template dropdown.
4. Mount your configuration file at `/mnt/user/appdata/omni-mcp/omni-mcp.toml`.
5. Click **Apply**.

---

## Connecting IDEs to Unraid `omni-mcp`

Point your AI IDE (Antigravity IDE, Claude Desktop, VSCodium) to your Unraid NAS IP:

```json
{
  "mcpServers": {
    "omni-mcp": {
      "url": "http://<UNRAID_IP>:8080/mcp"
    }
  }
}
```
