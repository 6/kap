# devcontainer-egress-proxy

> [!WARNING]
> This is experimental and may have bugs. Use at your own risk.

devcontainer-egress-proxy (`devp`) locks down what an AI coding agent (or any process) can access from inside a devcontainer. It's a proxy sidecar with a domain allowlist — the container can only reach the internet through the proxy.

For HTTPS, the proxy sees `CONNECT domain:443` but doesn't inspect inside the TLS tunnel (no MITM). Granularity is at the domain level: `api.github.com` and `gist.github.com` can be independently allowed or denied.

## Quick start

```bash
cargo install --path .

# Scaffold devcontainer files into your project
devp init --project-dir /path/to/your/project

# Review and adjust the config
$EDITOR /path/to/your/project/.devcontainer/devp.toml

# Open in VS Code or start with the CLI
devcontainer up --workspace-folder /path/to/your/project
```

## Configuration

The config is a flat domain allowlist in `devp.toml`. `devp init` generates a starting list with safe defaults for common ecosystems (GitHub, npm, PyPI, RubyGems, crates.io, Maven, CocoaPods, Go, APT, and AI providers).

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

Wildcards (`*.github.com`) match subdomains but not the bare domain — no need to list both `*.anthropic.com` and `api.anthropic.com`. Deny rules always win.

## Architecture

```
┌──────────────────────────────────────────────────┐
│ Internal network                                  │
│                                                   │
│  ┌──────────────┐    ┌──────────────────┐         │
│  │ App container │────│ Proxy sidecar    │──► Internet
│  │  (isolated)   │    │  devp proxy      │         │
│  │  HTTP_PROXY ──┼───►│  domain allowlist │         │
│  └──────────────┘    └──────────────────┘         │
└──────────────────────────────────────────────────┘
```

- The app container has **no external network route** — all traffic goes through the proxy sidecar
- Blocked requests get a 403 with a clear error message naming the denied domain

## Commands

| Command | Where it runs | Purpose |
|---------|--------------|---------|
| `devp proxy` | Proxy sidecar | HTTP/HTTPS forward proxy with domain allowlist |
| `devp init` | Anywhere | Scaffolds `.devcontainer/` files (3 files) |
| `devp check` | Proxy sidecar | Proxy health check (used by Docker healthcheck) |
| `devp why-denied` | App container | Shows denied requests from the proxy log |

## Development

```bash
cargo check          # fast compile check
cargo build          # full build
cargo test           # run all tests
cargo clippy         # lint
```

This repo includes a `.devcontainer/` that dogfoods devp itself — it builds from source and runs with GitHub, Rust, APT, and AI domains allowed. Open it in VS Code or run:

```bash
devcontainer up --workspace-folder .
devcontainer exec --workspace-folder . .devcontainer/smoke-test.sh
```
