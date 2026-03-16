# kap

Run AI agents in secure capsules. Built on devcontainers with network controls and remote access.

## Commands

```
cargo check          # fast compile check
cargo build          # full build
cargo test           # run all tests
cargo run -- --help  # CLI help
mise run build       # build, install, and push to running sidecars
```

After any changes to CLI commands or flags, run `mise run build` to install the host binary, build the Linux sidecar binary via Docker, and deploy to all running sidecar containers.

## Architecture

Single Rust binary with five components:

1. **Domain proxy** (:3128): HTTP/HTTPS forward proxy with domain allowlist. Docker Compose with an internal network ensures the app container has no external route except through this proxy.
2. **DNS forwarder** (:53): only resolves domains in the allowlist, returns NXDOMAIN for everything else. Prevents DNS exfiltration. DO NOT remove this thinking it's redundant with the domain proxy; DNS exfiltration doesn't use HTTP.
3. **MCP proxy** (:3129): reverse proxy for remote Streamable HTTP MCP servers. Tool-level allow/deny filtering and credential isolation. Only starts when `[mcp]` is in config. Auth stored in `~/.kap/auth/` via `kap mcp add`; servers must be listed in kap.toml with `allow`.
4. **CLI proxy** (:3130): proxies CLI tools (`gh`, `aws`, etc.) from the app container. Two modes per tool: `mode = "proxy"` (default) runs the command on the sidecar, returning stdout/stderr; `mode = "direct"` returns credentials to the shim, which exec's the real binary locally (needed for commands that write files, e.g. `gh run download`). Shims live on the shared `kap-bin` volume at `/opt/kap/bin/`, managed by the sidecar at runtime. Per-tool allow/deny in `[[cli.tools]]` config (proxy mode only).
5. **Remote access daemon** (:19420): runs on the host (not in Docker). HTTP server with token-based auth for monitoring and steering devcontainers from a phone. QR code pairing, WebSocket streaming, web UI served from the binary. Start with `kap remote start`.

## Library crate

kap is both a CLI binary (`src/main.rs`) and a Rust library (`src/lib.rs`), so other Rust projects can depend on it without shelling out.

**Public API:** Only `config` and `container` are public. All other modules are `#[doc(hidden)]` — they must remain `pub` (the binary crate needs them), but are not part of the stable API for downstream consumers.

**Rules for library code (everything in `src/` except `main.rs`):**
- **Never call `std::process::exit()`** — return `Result`/`anyhow::bail!` instead. Library code must not kill the host process. A test in `lib.rs` enforces this.
- **Don't make new modules `pub`** without `#[doc(hidden)]` unless they're intentionally part of the external API. Default to `#[doc(hidden)] pub mod`.

## Key modules

- `src/main.rs`:clap CLI dispatch
- `src/config.rs`:TOML config parsing (domain lists, MCP server configs, `ssh_agent`, `[compose]` section). Merges global `~/.kap/kap.toml` with project config on load.
- `src/proxy/mod.rs`:HTTP/HTTPS forward proxy (hyper + tokio)
- `src/proxy/dns.rs`:DNS forwarder with domain filtering (prevents DNS exfiltration, not redundant with HTTP proxy)
- `src/proxy/allowlist.rs`:wildcard domain matching, deny-overrides-allow (shared by HTTP proxy, DNS, and MCP proxy)
- `src/proxy/sni.rs`:TLS ClientHello SNI extraction + CONNECT domain validation (blocks SNI-mismatch attacks)
- `src/proxy/log.rs`:structured JSONL logging + `why-denied` reader
- `src/mcp/mod.rs`:MCP proxy HTTP listener, request routing, tool filtering
- `src/mcp/filter.rs`:tool name allowlist
- `src/mcp/client.rs`:shared MCP client (initialize + tools/list handshake)
- `src/mcp/jsonrpc.rs`:JSON-RPC 2.0 types, tools/list filtering, tools/call gating
- `src/mcp/upstream.rs`:HTTPS client to upstream MCP servers, token injection + refresh
- `src/mcp/auth.rs`:`kap mcp add` OAuth flow: metadata discovery, dynamic client registration, PKCE, browser callback
- `src/reload.rs`:hot-reload config watcher (polls kap.toml, swaps shared state), `Shared<T>` type, CLI shim writer
- `src/init.rs`:scaffolds `.devcontainer/` files, generates compose overlay from `[compose]` config, JSONC parser for devcontainer.json
- `src/init_env.rs`:`kap sidecar-init` (initializeCommand); regenerates compose overlay and `.env`
- `src/mcp_cmd.rs`:`kap mcp` subcommands (add, get, list, remove)
- `src/container.rs`:devcontainer lifecycle (up, down, exec, list)
- `src/connect_proxy.rs`:TCP-over-HTTP-CONNECT bridge for SSH ProxyCommand (runs in app container, used by `GIT_SSH_COMMAND`)
- `src/status.rs`:health checks (proxy, DNS, auth mount, log path), validates devcontainer.json on `kap up`/`kap exec`
- `src/cli/mod.rs`:CLI proxy HTTP listener, proxy/direct mode dispatch, multi-tool routing
- `src/cli/filter.rs`:command allow/deny filtering (generic, per-tool, proxy mode only)
- `src/cli/shim.rs`:`kap sidecar-cli-shim` (runs in app container). Proxy mode: outputs sidecar response. Direct mode: decodes env vars from sidecar, finds real binary, exec's it.
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

After any non-trivial change, run `cargo clippy` and `cargo fmt` to catch lint warnings and formatting drift. Fix **all** warnings before committing. CI runs clippy with `-D warnings` so any warning is a build failure.

Smoke tests in `.devcontainer/smoke-test.sh` cover end-to-end behavior across all layers (domain proxy, DNS forwarder, MCP proxy, CLI proxy). Run these in the devcontainer after any change to proxy logic, config parsing, or docker-compose templates.

**Do NOT use `docker compose up` directly** to manage devcontainers. Always use `kap up` (which wraps `devcontainer up`). The devcontainer CLI adds labels that `devcontainer exec` needs to find containers. Manual `docker compose up` strips these labels and breaks `kap exec`.

## Testing changes end-to-end

After any code change that affects the proxy, overlay, or shims:

```bash
mise run build                              # build + install + push to sidecars
kap up --reset                             # full recreate
kap exec kap status                       # verify all checks pass
kap exec .devcontainer/smoke-test.sh       # run smoke tests
```

`--reset` is required when: the overlay template changes (`src/init.rs` `generate_overlay()`), the post-start script changes (`src/reload.rs`), `[compose]` (image/build), `ssh_agent`, or network/subnet changes. Config changes to `kap.toml` (allowlists, CLI tool modes, MCP tool filters) are hot-reloaded by the sidecar every 2 seconds — no restart needed. `--reset` also clears proxy logs. Binary-only changes (proxy logic, CLI shim, connect proxy) only need `mise run build` — no reset.

## Compose overlay

`docker-compose.kap.yml` is a **generated file** (gitignored). It is regenerated by `kap sidecar-init` on every `kap up`, so it always matches the installed kap version.

The `[compose]` section in `kap.toml` controls how the kap sidecar image is sourced:
- Default (no `[compose]` section): uses `image: ghcr.io/6/kap:latest`
- Build from source: `[compose] build = { context = "..", dockerfile = "...", target = "..." }`

DO NOT edit `docker-compose.kap.yml` directly. Changes will be overwritten. Edit `kap.toml` instead.

## CLI shims

CLI tool shims live on the shared `kap-bin` volume at `/opt/kap/bin/`, NOT as Docker Compose configs. The sidecar writes shim scripts on startup and updates them on config reload. PATH is set two ways: `remoteEnv.PATH` in devcontainer.json (for `devcontainer exec`) and `/etc/profile.d/kap-path.sh` (for interactive login shells like `kap exec zsh`). Both must be present — `remoteEnv` alone doesn't survive shell startup files that reset PATH.

Each shim calls `kap sidecar-cli-shim <tool> <args>`, which POSTs to the sidecar. The sidecar decides per-request whether to proxy or direct based on the current (hot-reloaded) config:
- **Proxy mode** (`mode = "proxy"`, default): sidecar executes the command, returns stdout/stderr/exit code. Credentials never enter the app container.
- **Direct mode** (`mode = "direct"`): sidecar returns env vars (e.g. `GH_TOKEN`), shim exec's the real binary locally. Needed for commands that write files (e.g. `gh run download`). The tool must be installed in the app container.

## Hot-reload

The sidecar polls `kap.toml` every 2 seconds using content hashing (not mtime, which is unreliable with Docker Desktop macOS bind mounts). On change, it atomically swaps:
- Domain allowlist (HTTP proxy + DNS forwarder)
- CLI tool configs (modes, filters, env vars) + shim scripts on the shared volume
- MCP tool filters (allow/deny)

Uses `Arc<RwLock<Arc<T>>>` for shared state — readers clone an inner Arc (~1ns), the reloader briefly holds a write lock to swap.

## SSH agent forwarding and git-over-SSH

Controlled by `ssh_agent = true` (default) in `kap.toml`. When enabled and an SSH agent is detected, the overlay adds:
- `SSH_AUTH_SOCK` volume mount + env var (for key-based auth)
- `GIT_SSH_COMMAND` env var with a ProxyCommand that routes SSH through the HTTP proxy via `kap sidecar-connect-proxy`

The connect proxy (`src/connect_proxy.rs`) bridges SSH through an HTTP CONNECT tunnel. It runs in the app container as an SSH ProxyCommand. The domain proxy's CONNECT handler uses `tokio::select!` to handle both client-speaks-first (TLS/SNI validation) and server-speaks-first (SSH banner forwarding) protocols.

SSH agent socket detection logic is in `detect_ssh_auth_sock()`:
- **macOS**: mounts Docker Desktop's `/run/host-services/ssh-auth.sock` (avoids VM socket-sharing issues with direct bind mounts). Requires the host SSH agent to be visible to Docker Desktop (e.g. via a LaunchAgent).
- **Linux**: bind-mounts `$SSH_AUTH_SOCK` directly (Docker runs natively, no VM boundary).
- **Disabled / no agent**: mount and `GIT_SSH_COMMAND` are both omitted, no error.

### SSH proxy pitfalls (learned the hard way)

- **CONNECT tunnel deadlock**: the proxy must NOT wait for client data before forwarding server data. SSH has the server speak first (banner). Use `tokio::select!` to race reads from both sides.
- **Upgraded connection buffering**: after `write_all` on a hyper `Upgraded` connection, call `flush()` explicitly — data may sit in an internal buffer and never reach the client.
- **BufReader data loss**: when using `BufReader` to parse HTTP headers, it reads ahead in chunks. After draining headers, call `.buffer().to_vec()` to capture any extra data before `.into_inner()` (which discards the buffer).
- **Socket shutdown kills both sides**: `TcpStream::try_clone()` + `shutdown(Write)` sends EOF to the proxy because `shutdown` operates on the socket, not the fd. Use `libc::dup()` for independent close semantics.
- **Stdout buffering in pipes**: Rust's `Stdout` uses full buffering when connected to a pipe (SSH ProxyCommand). Write directly to fd 1 via `File::from_raw_fd(1)` and use a manual read/write loop instead of `std::io::copy`.
- **Shims mask install checks**: the post-start script's `command -v gh` finds the kap shim and skips installing the real binary. Filter `/opt/kap` out of PATH before install checks.

## Security model

The domain proxy is a domain-level gate (not a request-level firewall). For HTTPS, it sees `CONNECT domain:443` and validates that the TLS SNI in the ClientHello matches the CONNECT target (blocking SNI-mismatch routing attacks), but cannot inspect inside the encrypted TLS tunnel. No MITM.

The MCP proxy adds tool-level control: it inspects JSON-RPC `tools/list` and `tools/call` between the agent and remote MCP servers. Credentials (OAuth tokens, API keys) live on the proxy sidecar and never enter the app container.

The CLI proxy in `mode = "proxy"` keeps credentials on the sidecar. In `mode = "direct"`, credentials are sent to the app container at exec time — the domain proxy still controls what the container can reach, limiting blast radius.
