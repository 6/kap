# devcontainer-guard

> [!WARNING]
> This is experimental and may have bugs. Use at your own risk.

devcontainer-guard (`devg`) controls what a devcontainer can reach and what tools an AI agent can use. It runs as a proxy sidecar with three layers:

1. **Domain proxy**: allowlist of domains the container can talk to
2. **DNS forwarder**: only resolves allowed domains, blocks DNS exfiltration
3. **MCP proxy**: allowlist of MCP tools an agent can call, with credential isolation

For HTTPS, the domain proxy sees `CONNECT domain:443` but doesn't inspect inside the TLS tunnel (no MITM). The MCP proxy inspects Streamable HTTP JSON-RPC to filter tools.

## Quick start

```bash
cargo install devcontainer-guard --version 0.0.1-pre1

cd path/to/your/project
devg init

# Review and adjust allowed domains
$EDITOR .devcontainer/devg.toml

devcontainer up --workspace-folder .
```

`devg init` detects whether you have an existing devcontainer or not:

- **Existing project**: creates `devg.toml` + `docker-compose.devg.yml` (overlay) and updates your `devcontainer.json` to include them. Your existing compose file stays untouched.
- **New project**: creates everything from scratch.

## Domain allowlist

The config is a flat domain allowlist in `devg.toml`. `devg init` generates a starting list with safe defaults for common ecosystems (GitHub, npm, PyPI, RubyGems, crates.io, Maven, CocoaPods, Go, APT, and AI providers).

```toml
[proxy.network]
allow = [
  "github.com",
  "*.github.com",
  "*.githubusercontent.com",
  "crates.io",
  "*.crates.io",
  "*.ubuntu.com",
]
# deny overrides allow:
deny = ["gist.github.com"]
```

Wildcards (`*.github.com`) match subdomains but not the bare domain. Deny rules always win.

## MCP proxy

The MCP proxy sits between the agent and remote MCP servers. It controls which tools the agent can call and keeps credentials (OAuth tokens, API keys) out of the app container.

```toml
[mcp]

[[mcp.servers]]
name = "github"
upstream = "https://mcp.github.com"
token_env = "GH_TOKEN"
allow_tools = ["get_pull_request", "list_issues", "search_code"]
deny_tools = ["create_repository", "delete_repository"]
```

The agent connects to `http://172.28.0.3:3129/github` instead of the real server. devg forwards requests with the configured auth and filters `tools/list` and `tools/call`.

Three auth modes:

```toml
# 1. Bearer token from env var
token_env = "GH_TOKEN"

# 2. Custom headers (${VAR} expanded from env)
headers = { "X-Api-Key" = "${MY_KEY}" }

# 3. OAuth 2.1 (run once on host, tokens stored in ~/.devg/auth/)
# devg auth github --upstream https://mcp.github.com
```

## Architecture

```
┌──────────────────────────────────────────────────┐
│ Internal network                                  │
│                                                   │
│  ┌──────────────┐    ┌──────────────────┐         │
│  │ App container │    │ Proxy sidecar    │         │
│  │  (isolated)   │    │                  │         │
│  │  HTTP_PROXY ──┼───►│  domain proxy    │──► Internet
│  │               │    │  :3128           │         │
│  │  DNS ─────────┼───►│  DNS forwarder   │──► Upstream DNS
│  │               │    │  :53             │         │
│  │  MCP servers ─┼───►│  MCP proxy       │──► MCP servers
│  │  via http://  │    │  :3129           │         │
│  │  proxy:3129   │    │  (tool filter +  │         │
│  │               │    │   credential     │         │
│  │  (no tokens)  │    │   injection)     │         │
│  └──────────────┘    └──────────────────┘         │
└──────────────────────────────────────────────────┘
```

- The app container has **no external network route**. All traffic goes through the proxy sidecar.
- DNS queries only resolve allowed domains. Disallowed domains get NXDOMAIN.
- Blocked domain requests get a 403. Blocked MCP tool calls get a JSON-RPC error.
- Credentials never enter the app container.

## Security model

Network isolation is **kernel-enforced**, not proxy-based. The Docker `internal: true` network has no default gateway, so the app container has no IP route to the outside world. Unsetting `HTTP_PROXY` or making direct TCP connections doesn't bypass it. Packets have nowhere to go. The only reachable host is the proxy sidecar on the internal network.

MCP server domains are intentionally **not** in the domain allowlist. The agent can only reach them through devg's MCP proxy, which enforces tool filtering. Connecting directly would be blocked by the network.

**Known limitations:**

- **Domain fronting**: a CONNECT request to an allowed CDN domain could route to an attacker's backend via SNI/Host manipulation. devg sees the CONNECT domain, not the backend.
- **Container escape**: a kernel exploit that breaks out of the container bypasses all isolation. Not specific to devg. Running Docker inside a VM (e.g., Docker Desktop, Firecracker) adds defense-in-depth.
- **No TLS inspection**: for HTTPS, devg sees `CONNECT domain:443` but cannot inspect request paths, headers, or bodies inside the tunnel. A process with valid credentials for an allowed domain can do anything that domain permits.

## Commands

| Command | Where it runs | Purpose |
|---------|--------------|---------|
| `devg init` | Host | Scaffold devg into a project |
| `devg init-env` | Host | Generate .env with credentials (runs as initializeCommand) |
| `devg status` | Host | Check if everything is wired correctly |
| `devg why-denied` | Host | Show denied requests from the proxy log |
| `devg auth <name> --upstream <url>` | Host | OAuth 2.1 setup for an MCP server |
| `devg proxy` | Proxy sidecar | Domain proxy + DNS forwarder + MCP proxy (internal) |
| `devg check` | Proxy sidecar | Health check (internal, used by Docker healthcheck) |

## Development

```bash
cargo check          # fast compile check
cargo build          # full build
cargo test           # run all tests
cargo clippy         # lint
```

This repo includes a `.devcontainer/` that dogfoods devg itself. It builds from source, runs with GitHub/Rust/APT/AI domains allowed, and proxies [Context7](https://context7.com) as a sample MCP server. Set `CONTEXT7_API_KEY` in your environment to try it (free key from context7.com/dashboard). Open in VS Code or run:

```bash
devcontainer up --workspace-folder .
devcontainer exec --workspace-folder . .devcontainer/smoke-test.sh
```
