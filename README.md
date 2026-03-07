# kap

> [!WARNING]
> This is experimental and may have bugs. Use at your own risk.

Run AI agents in secure capsules. Built on devcontainers with network controls and remote access.

- **Domain allowlist** - only approved domains are reachable from the container
- **MCP tool allowlist** - only approved tools are callable on remote MCP servers
- **CLI proxy** - `gh`, `aws`, etc. are proxied with per-command allowlists
- **Credential isolation** - tokens and API keys live on the sidecar and never enter the app container
- **Remote monitoring** - monitor and steer agents from your phone over local WiFi

## Quick start

```bash
cargo install kap

cd your-project
kap init
$EDITOR .devcontainer/kap.toml   # review allowed domains, MCP servers, CLI tools
kap up
```

## Domain allowlist

The config is a flat domain allowlist in `kap.toml`. `kap init` generates a starting list with safe defaults for common ecosystems (GitHub, npm, PyPI, RubyGems, crates.io, Maven, CocoaPods, Go, APT, and AI providers).

```toml
[proxy.network]
allow = [
  "github.com",
  "*.github.com",
  "crates.io",
  "*.crates.io",
  "*.ubuntu.com",
]
# deny overrides allow:
deny = ["gist.github.com"]
```

Wildcards (`*.github.com`) match subdomains but not the bare domain. Deny rules always win.

## MCP proxy

The MCP proxy sits between the agent and remote MCP servers. The agent connects to the proxy over unauthenticated HTTP on the internal network. The proxy injects credentials when forwarding to the upstream server. The app container has no access to OAuth tokens, API keys, or any other secrets.

### Registering servers

Register MCP servers on the host:

```bash
# OAuth (opens browser)
kap mcp add linear https://mcp.linear.app

# API key via headers
kap mcp add context7 https://mcp.context7.com/mcp --header "CONTEXT7_API_KEY=sk-..."

kap mcp list          # see registered servers
kap mcp get linear    # show details + tools list
```

After registering, add each server to `kap.toml` with an `allow_tools` list (see below). The agent connects to `http://172.28.0.3:3129/<name>` instead of the real server. kap handles auth and tool filtering.

### Tool allowlist

Each server needs a `[[mcp.servers]]` entry in `kap.toml` with an explicit `allow_tools` list. Same model as the domain allowlist - only what's listed is permitted:

```toml
[mcp]

# Allow all tools
[[mcp.servers]]
name = "context7"
allow_tools = ["*"]

# Allow only read/search operations
[[mcp.servers]]
name = "github"
allow_tools = ["get_*", "list_*", "search_*"]
```

Wildcards work the same as domain patterns (`get_*` matches `get_issue`, `get_user`, etc.).

## CLI proxy

The CLI proxy lets the app container run tools like `gh` or `aws` without direct access to credentials. Commands are forwarded to the sidecar which validates them against an allowlist and executes with the configured env vars.

```toml
[cli]

[[cli.tools]]
name = "gh"
allow = ["pr *", "issue *", "repo *", "search *", "auth status"]
env = ["GH_TOKEN"]

[[cli.tools]]
name = "aws"
allow = ["s3 ls *", "s3 cp *", "sts get-caller-identity"]
env = ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"]
```

`deny` overrides `allow`. Tools must be installed on the sidecar (`gh` is included by default; add others via `[compose] build`).

## Compose overlay

By default, the kap sidecar pulls from `ghcr.io/6/kap:latest`. To build from source or use a different image, add a `[compose]` section to `kap.toml`:

```toml
# Pull from a registry (default if [compose] is omitted)
[compose]
image = "ghcr.io/6/kap:latest"

# Or build from source
[compose]
build = { context = "..", dockerfile = ".devcontainer/Dockerfile", target = "proxy" }
```

The overlay is regenerated on every `kap up`. Don't edit `docker-compose.kap.yml` directly.

## Remote access

Monitor and steer AI agents running in devcontainers from your phone over local WiFi.

```bash
kap remote start    # starts HTTP daemon on :19420, shows QR code
```

Scan the QR code on your phone to open the web UI. It auto-pairs and gives you:

- **Status** -container state, proxy health, denied request count
- **Logs** -live streaming proxy events, filterable by denied-only
- **Agent** -Claude Code session timelines, tool calls, cancel button, follow-up prompts

The daemon runs on the host and uses `docker exec` to reach into both containers. All API endpoints require a bearer token issued during QR pairing. Settings for light/dark mode and font size.

```bash
kap remote pair     # show QR code again
kap remote devices  # list paired devices
kap remote revoke <id>  # revoke a device
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
│  │  proxy:3129   │    │                  │         │
│  │  gh/aws/etc ──┼───►│  CLI proxy       │──► APIs
│  │  (shims)      │    │  :3130           │         │
│  │               │    │                  │         │
│  │  (no tokens)  │    │  (credentials)   │         │
│  └──────────────┘    └──────────────────┘         │
└──────────────────────────────────────────────────┘
```

- The app container has **no external network route**. All traffic goes through the proxy sidecar.
- DNS queries only resolve allowed domains. Disallowed domains get NXDOMAIN.
- Blocked domain requests get a 403. Blocked MCP tool calls get a JSON-RPC error.
- **Credentials never enter the app container.** OAuth tokens, API keys, and GH_TOKEN live on the proxy sidecar only. The proxy injects auth when forwarding upstream.

## Security model

Network isolation is **kernel-enforced**, not proxy-based. The Docker `internal: true` network has no default gateway, so the app container has no IP route to the outside world. Unsetting `HTTP_PROXY` or making direct TCP connections doesn't bypass it. Packets have nowhere to go. The only reachable host is the proxy sidecar on the internal network.

MCP server domains are intentionally **not** in the domain allowlist. The agent can only reach them through kap's MCP proxy, which enforces tool filtering. Connecting directly would be blocked by the network.

**Known limitations:**

- **Domain fronting**: a CONNECT request to an allowed CDN domain could route to an attacker's backend via SNI/Host manipulation. kap sees the CONNECT domain, not the backend.
- **Container escape**: a kernel exploit that breaks out of the container bypasses all isolation. Not specific to kap. Running Docker inside a VM (e.g., Docker Desktop, Firecracker) adds defense-in-depth.
- **No TLS inspection**: for HTTPS, kap sees `CONNECT domain:443` but cannot inspect request paths, headers, or bodies inside the tunnel. A process with valid credentials for an allowed domain can do anything that domain permits.

## Commands

| Command | Purpose |
|---------|---------|
| `kap init` | Scaffold kap into a project |
| `kap up` | Start the devcontainer |
| `kap down` | Stop and remove the devcontainer |
| `kap exec [cmd]` | Run a command in the devcontainer (default: shell) |
| `kap list` | List running devcontainers |
| `kap status` | Check if everything is wired correctly |
| `kap why-denied` | Show denied requests from the proxy log |
| `kap mcp add <name> <url>` | Register an MCP server (OAuth or API key) |
| `kap mcp get <name>` | Show server details and tools list |
| `kap mcp list` | List registered servers |
| `kap mcp remove <name>` | Remove a registered server |
| `kap remote start` | Start the remote access daemon (shows QR code) |
| `kap remote stop` | Stop the remote access daemon |
| `kap remote status` | Show daemon status and paired devices |
| `kap remote revoke <id>` | Revoke a paired device |

## Development

```bash
cargo check          # fast compile check
cargo build          # full build
cargo test           # run all tests
cargo clippy         # lint
```

This repo includes a `.devcontainer/` that dogfoods kap itself. It builds from source, runs with GitHub/Rust/APT/AI domains allowed, and proxies [Context7](https://context7.com) as a sample MCP server. Set `CONTEXT7_API_KEY` in your environment to try it (free key from context7.com/dashboard). Open in VS Code or run:

```bash
kap up
kap exec .devcontainer/smoke-test.sh
```
