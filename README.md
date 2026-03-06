# devcontainer-guard

> [!WARNING]
> This is experimental and may have bugs. Use at your own risk.

devcontainer-guard (`devg`) controls what a devcontainer can reach and what tools an AI agent can use. It runs as a proxy sidecar with two layers:

1. **Domain proxy** -- allowlist of domains the container can talk to
2. **MCP proxy** -- allowlist of MCP tools an agent can call, with credential isolation

For HTTPS, the domain proxy sees `CONNECT domain:443` but doesn't inspect inside the TLS tunnel (no MITM). The MCP proxy inspects Streamable HTTP JSON-RPC to filter tools.

## Quick start

```bash
cargo install --path .

# Scaffold devcontainer files into your project
devg init --project-dir /path/to/your/project

# Review and adjust the config
$EDITOR /path/to/your/project/.devcontainer/devg.toml

# Open in VS Code or start with the CLI
devcontainer up --workspace-folder /path/to/your/project
```

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

The agent connects to `http://proxy:3129/github` instead of the real server. devg forwards requests with the configured auth and filters `tools/list` and `tools/call`.

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
│  │  MCP servers ─┼───►│  MCP proxy       │──► MCP servers
│  │  via http://  │    │  :3129           │         │
│  │  proxy:3129   │    │  (tool filter +  │         │
│  │               │    │   credential     │         │
│  │  (no tokens)  │    │   injection)     │         │
│  └──────────────┘    └──────────────────┘         │
└──────────────────────────────────────────────────┘
```

- The app container has **no external network route**. All traffic goes through the proxy sidecar.
- Blocked domain requests get a 403. Blocked MCP tool calls get a JSON-RPC error.
- Credentials never enter the app container.

## Commands

| Command | Where it runs | Purpose |
|---------|--------------|---------|
| `devg proxy` | Proxy sidecar | Domain proxy + MCP proxy (if `[mcp]` in config) |
| `devg auth <name> --upstream <url>` | Host | OAuth 2.1 setup for an MCP server |
| `devg init` | Anywhere | Scaffolds `.devcontainer/` files (3 files) |
| `devg check` | Proxy sidecar | Proxy health check (used by Docker healthcheck) |
| `devg why-denied` | App container | Shows denied requests from the proxy log |

## Development

```bash
cargo check          # fast compile check
cargo build          # full build
cargo test           # run all tests
cargo clippy         # lint
```

This repo includes a `.devcontainer/` that dogfoods devg itself. It builds from source and runs with GitHub, Rust, APT, and AI domains allowed. Open it in VS Code or run:

```bash
devcontainer up --workspace-folder .
devcontainer exec --workspace-folder . .devcontainer/smoke-test.sh
```
