# devcontainer-egress-proxy

Egress proxy + credential isolation for devcontainers. Binary name: `devp`.

## Commands

```
cargo check          # fast compile check
cargo build          # full build
cargo test           # run all tests
cargo run -- --help  # CLI help
```

## Architecture

Single Rust binary, three roles:

- **`devp proxy`** — HTTP/HTTPS forward proxy + DNS forwarder (runs in proxy sidecar container)
- **`devp cred-server`** — git credential server over Unix socket (runs on host)
- **`devp credential`** — git credential helper client (runs in app container)

Docker Compose with an internal network ensures the app container has no external route except through the proxy sidecar.

## Key modules

- `src/main.rs` — clap CLI dispatch
- `src/config.rs` — TOML config parsing + profile resolution
- `src/profiles.rs` — built-in ecosystem domain profiles (ruby, node, python, rust, go, apt, github, ai)
- `src/proxy/mod.rs` — HTTP/HTTPS forward proxy (hyper + tokio)
- `src/proxy/allowlist.rs` — wildcard domain matching, deny-overrides-allow
- `src/proxy/dns.rs` — UDP DNS forwarder with domain filtering
- `src/proxy/log.rs` — structured JSONL logging + `why-denied` reader
- `src/cred_server.rs` — host-side Unix socket credential server
- `src/credential.rs` — container-side git credential helper
- `src/init.rs` — scaffolds `.devcontainer/` files, detects project language
- `src/check.rs` — health checks (proxy, DNS, cred-server, git config)

## Security model

The proxy is a domain-level gate (not a request-level firewall). For HTTPS, it sees `CONNECT domain:443` but cannot inspect inside the TLS tunnel. No MITM.

Two complementary layers:
- **Proxy**: controls where the agent can talk (domain allowlist enforced by Docker internal network)
- **Credential broker**: controls what the agent can do (tokens only served via git credential protocol, never exposed as env vars)

Subdomain-level granularity works: `api.github.com` vs `gist.github.com` are independently allowed/denied.
