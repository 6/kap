# devcontainer-guard

Network and MCP access control for devcontainers. Binary name: `devg`.

## Commands

```
cargo check          # fast compile check
cargo build          # full build
cargo test           # run all tests
cargo run -- --help  # CLI help
```

## Architecture

Single Rust binary with three enforcement layers:

1. **Domain proxy** (:3128): HTTP/HTTPS forward proxy with domain allowlist. Docker Compose with an internal network ensures the app container has no external route except through this proxy.
2. **DNS forwarder** (:53): only resolves domains in the allowlist, returns NXDOMAIN for everything else. Prevents DNS exfiltration. DO NOT remove this thinking it's redundant with the domain proxy; DNS exfiltration doesn't use HTTP.
3. **MCP proxy** (:3129): reverse proxy for remote Streamable HTTP MCP servers. Provides tool-level filtering and credential isolation. Only starts when `[mcp]` is in config.

## Key modules

- `src/main.rs`:clap CLI dispatch
- `src/config.rs`:TOML config parsing (domain lists, MCP server configs)
- `src/proxy/mod.rs`:HTTP/HTTPS forward proxy (hyper + tokio)
- `src/proxy/dns.rs`:DNS forwarder with domain filtering (prevents DNS exfiltration, not redundant with HTTP proxy)
- `src/proxy/allowlist.rs`:wildcard domain matching, deny-overrides-allow (shared by HTTP proxy, DNS, and MCP proxy)
- `src/proxy/log.rs`:structured JSONL logging + `why-denied` reader
- `src/mcp/mod.rs`:MCP proxy HTTP listener, request routing, tool filtering
- `src/mcp/filter.rs`:tool name allow/deny (reuses allowlist pattern)
- `src/mcp/jsonrpc.rs`:JSON-RPC 2.0 types, tools/list filtering, tools/call gating
- `src/mcp/upstream.rs`:HTTPS client to upstream MCP servers, token injection + refresh
- `src/mcp/auth.rs`:`devg auth` command: OAuth 2.1 (metadata discovery, dynamic client registration, PKCE, browser callback)
- `src/init.rs`:scaffolds `.devcontainer/` files (3 files: devg.toml, docker-compose.yml, devcontainer.json)
- `src/check.rs`:proxy health check (for Docker healthcheck)

## Security model

The domain proxy is a domain-level gate (not a request-level firewall). For HTTPS, it sees `CONNECT domain:443` but cannot inspect inside the TLS tunnel. No MITM.

The MCP proxy adds tool-level control: it inspects JSON-RPC `tools/list` and `tools/call` between the agent and remote MCP servers. Credentials (OAuth tokens, API keys) live on the proxy sidecar and never enter the app container.
