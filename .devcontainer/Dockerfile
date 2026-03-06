# Build devg binary (shared by both targets)
FROM rust:1.93-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release && cp target/release/devg /usr/local/bin/devg

# Proxy sidecar: minimal image with just the devg binary.
# Runs domain proxy (:3128), DNS forwarder (:53), and MCP proxy (:3129).
FROM debian:bookworm-slim AS proxy
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/devg /usr/local/bin/devg
RUN mkdir -p /var/log/devg
CMD ["devg", "proxy"]

# Dev environment: full Rust toolchain for working on devg itself.
# This is the app container in the dogfood devcontainer.
FROM ubuntu:24.04 AS dev
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git zsh sudo build-essential pkg-config dnsutils \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/devg /usr/local/bin/devg
# Create vscode user (matches devcontainer convention)
RUN userdel -r ubuntu 2>/dev/null; \
    groupadd -f -g 1000 vscode \
    && useradd -u 1000 -g 1000 -m -s /bin/zsh vscode \
    && echo "vscode ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers
USER vscode
# Install Rust toolchain for development
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    --default-toolchain stable \
    --component clippy,rustfmt
ENV PATH="/home/vscode/.cargo/bin:${PATH}"
WORKDIR /home/vscode
