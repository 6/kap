# kap

Run AI agents in devcontainers with network controls and remote access.

## Commands

```
cargo check          # fast compile check
cargo build          # full build
cargo test           # run all tests
cargo run -- --help  # CLI help
```

## Architecture

Single Rust binary with five components:

1. **Domain proxy** (:3128): HTTP/HTTPS forward proxy with domain allowlist. Docker Compose with an internal network ensures the app container has no external route except through this proxy.
2. **DNS forwarder** (:53): only resolves domains in the allowlist, returns NXDOMAIN for everything else. Prevents DNS exfiltration. DO NOT remove this thinking it's redundant with the domain proxy; DNS exfiltration doesn't use HTTP.
3. **MCP proxy** (:3129): reverse proxy for remote Streamable HTTP MCP servers. Tool-level allowlist filtering and credential isolation. Only starts when `[mcp]` is in config. Auth stored in `~/.kap/auth/` via `kap mcp add`; servers must be listed in kap.toml with `allow_tools`.
4. **CLI proxy** (:3130): proxies CLI tools (`gh`, `aws`, etc.) from the app container. Credentials stay on the sidecar; the app container gets shim scripts. Per-tool allow/deny in `[[cli.tools]]` config.
5. **Remote access daemon** (:19420): runs on the host (not in Docker). HTTP server with token-based auth for monitoring and steering devcontainers from a phone. QR code pairing, WebSocket streaming, web UI served from the binary. Start with `kap remote start`.

## Key modules

- `src/main.rs`:clap CLI dispatch
- `src/config.rs`:TOML config parsing (domain lists, MCP server configs, `[compose]` section)
- `src/proxy/mod.rs`:HTTP/HTTPS forward proxy (hyper + tokio)
- `src/proxy/dns.rs`:DNS forwarder with domain filtering (prevents DNS exfiltration, not redundant with HTTP proxy)
- `src/proxy/allowlist.rs`:wildcard domain matching, deny-overrides-allow (shared by HTTP proxy, DNS, and MCP proxy)
- `src/proxy/log.rs`:structured JSONL logging + `why-denied` reader
- `src/mcp/mod.rs`:MCP proxy HTTP listener, request routing, tool filtering
- `src/mcp/filter.rs`:tool name allowlist
- `src/mcp/client.rs`:shared MCP client (initialize + tools/list handshake)
- `src/mcp/jsonrpc.rs`:JSON-RPC 2.0 types, tools/list filtering, tools/call gating
- `src/mcp/upstream.rs`:HTTPS client to upstream MCP servers, token injection + refresh
- `src/mcp/auth.rs`:`kap mcp add` OAuth flow: metadata discovery, dynamic client registration, PKCE, browser callback
- `src/init.rs`:scaffolds `.devcontainer/` files, generates compose overlay from `[compose]` config
- `src/init_env.rs`:`kap sidecar-init` (initializeCommand); regenerates compose overlay, writes `.env`, generates shims
- `src/mcp_cmd.rs`:`kap mcp` subcommands (add, get, list, remove)
- `src/container.rs`:devcontainer lifecycle (up, down, exec, list)
- `src/status.rs`:health checks (proxy, DNS, auth mount, log path)
- `src/cli/mod.rs`:CLI proxy HTTP listener, process spawning, multi-tool routing
- `src/cli/filter.rs`:command allow/deny filtering (generic, per-tool)
- `src/cli/shim.rs`:client-side shim (runs in app container, forwards to sidecar)
- `src/check.rs`:proxy health check (for Docker healthcheck)
- `src/remote/mod.rs`:HTTP server, routing, auth middleware for remote access daemon
- `src/remote/auth.rs`:QR code pairing, token management, device lifecycle
- `src/remote/api.rs`:REST endpoint handlers (status, logs, agent sessions, cancel, message)
- `src/remote/ws.rs`:WebSocket upgrade + streaming (proxy logs, agent session events)
- `src/remote/agent.rs`:Claude Code JSONL session parser (discovery, timeline, diff)
- `src/remote/containers.rs`:Docker container discovery + exec helpers (shared with `status.rs`)
- `src/remote/web.rs`:serves the embedded PWA web UI from `static/app.html`
- `static/app.html`:terminal-themed single-page web UI (inline CSS/JS, embedded via `include_str!`)

## Testing policy

Every code change must include unit tests. Run `cargo test` before committing.

After any non-trivial change, run `cargo clippy` and `cargo fmt` to catch lint warnings and formatting drift. Fix **all** warnings before committing — CI runs clippy with `-D warnings` so any warning is a build failure.

Smoke tests in `.devcontainer/smoke-test.sh` cover end-to-end behavior across all layers (domain proxy, DNS forwarder, MCP proxy, CLI proxy). Run these in the devcontainer after any change to proxy logic, config parsing, or docker-compose templates.

**Do NOT use `docker compose up` directly** to manage devcontainers. Always use `kap up` (which wraps `devcontainer up`). The devcontainer CLI adds labels that `devcontainer exec` needs to find containers. Manual `docker compose up` strips these labels and breaks `kap exec`.

## Testing changes end-to-end

After any code change that affects the proxy, overlay, or shims:

```bash
mise run build                              # build + install to ~/.cargo/bin
kap up --reset                             # full recreate
kap exec kap status                       # verify all checks pass
kap exec .devcontainer/smoke-test.sh       # run smoke tests
```

`--reset` is required whenever `.env`, the overlay template, or shim scripts change. Without it, `kap up` reuses the existing container with stale env vars and volume mounts.

## Compose overlay

`docker-compose.kap.yml` is a **generated file** (gitignored). It is regenerated by `kap sidecar-init` on every `kap up`, so it always matches the installed kap version.

The `[compose]` section in `kap.toml` controls how the kap sidecar image is sourced:
- Default (no `[compose]` section): uses `image: ghcr.io/6/kap:latest`
- Build from source: `[compose] build = { context = "..", dockerfile = "...", target = "..." }`

DO NOT edit `docker-compose.kap.yml` directly — changes will be overwritten. Edit `kap.toml` instead.

## Security model

The domain proxy is a domain-level gate (not a request-level firewall). For HTTPS, it sees `CONNECT domain:443` but cannot inspect inside the TLS tunnel. No MITM.

The MCP proxy adds tool-level control: it inspects JSON-RPC `tools/list` and `tools/call` between the agent and remote MCP servers. Credentials (OAuth tokens, API keys) live on the proxy sidecar and never enter the app container.
