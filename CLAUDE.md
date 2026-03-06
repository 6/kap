# devcontainer-egress-proxy

Egress proxy for devcontainers. Binary name: `devp`.

## Commands

```
cargo check          # fast compile check
cargo build          # full build
cargo test           # run all tests
cargo run -- --help  # CLI help
```

## Architecture

Single Rust binary. Docker Compose with an internal network ensures the app container has no external route except through the proxy sidecar.

## Key modules

- `src/main.rs` — clap CLI dispatch
- `src/config.rs` — TOML config parsing (allow/deny domain lists)
- `src/proxy/mod.rs` — HTTP/HTTPS forward proxy (hyper + tokio)
- `src/proxy/allowlist.rs` — wildcard domain matching, deny-overrides-allow
- `src/proxy/log.rs` — structured JSONL logging + `why-denied` reader
- `src/init.rs` — scaffolds `.devcontainer/` files (3 files: devp.toml, docker-compose.yml, devcontainer.json)
- `src/check.rs` — proxy health check (for Docker healthcheck)

## Security model

The proxy is a domain-level gate (not a request-level firewall). For HTTPS, it sees `CONNECT domain:443` but cannot inspect inside the TLS tunnel. No MITM.

Subdomain-level granularity works: `api.github.com` vs `gist.github.com` are independently allowed/denied.
