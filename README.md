# devcontainer-egress-proxy

devcontainer-egress-proxy (`devp`) locks down what an AI coding agent (or any process) can access from inside a devcontainer. Two layers:

- **Proxy** — domain allowlist enforced by an internal Docker network. The container can only reach the internet through the proxy sidecar.
- **Credential broker** — git tokens served over a Unix socket via the git credential protocol. Tokens stay on the host, never exposed as env vars in the container.

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

The config is a flat domain allowlist in `devp.toml`. `devp init` detects your project language and generates a starting list.

```toml
[proxy.network]
allow = [
  "github.com",
  "api.github.com",
  "*.githubusercontent.com",
  "crates.io",
  "*.crates.io",
  "*.ubuntu.com",
]
# deny overrides allow:
deny = ["gist.github.com"]
```

Wildcards (`*.github.com`) match subdomains but not the bare domain. Deny rules always win.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│ Host                                                │
│   devp cred-server ──► ~/.devp-sockets/cred.sock    │
└─────────────────────┬───────────────────────────────┘
                      │ (socket mount, read-only)
┌─────────────────────┼───────────────────────────────┐
│ Internal network     │                               │
│                      │                               │
│  ┌──────────────┐    │    ┌──────────────────┐       │
│  │ App container │────┼───│ Proxy sidecar    │──► Internet
│  │  (isolated)   │    │   │  devp proxy      │       │
│  │  HTTP_PROXY ──┼────┘   │  DNS forwarder   │       │
│  │  DNS ─────────┼───────►│  domain allowlist │       │
│  └──────────────┘         └──────────────────┘       │
└──────────────────────────────────────────────────────┘
```

- The app container has **no external network route** — all traffic goes through the proxy sidecar
- DNS queries are filtered: denied domains get NXDOMAIN
- Git credentials flow through `devp credential` → Unix socket → `devp cred-server` → `gh auth token`

## Commands

| Command | Where it runs | Purpose |
|---------|--------------|---------|
| `devp proxy` | Proxy sidecar | HTTP/HTTPS forward proxy + DNS forwarder |
| `devp cred-server` | Host | Serves git credentials over Unix socket |
| `devp credential` | App container | Git credential helper (talks to cred-server) |
| `devp init` | Anywhere | Scaffolds `.devcontainer/` files |
| `devp check` | App container | Verifies proxy, DNS, cred-server, git config |
| `devp why-denied` | App container | Shows denied requests from the proxy log |

## Development

```bash
cargo check          # fast compile check
cargo build          # full build
cargo test           # run all tests (34 tests)
cargo clippy         # lint
```

This repo includes a `.devcontainer/` that dogfoods devp itself — it builds from source and runs with GitHub, Rust, APT, and AI domains allowed. Open it in VS Code or run:

```bash
devcontainer up --workspace-folder .
devcontainer exec --workspace-folder . .devcontainer/smoke-test.sh
```
