FROM rust:1.85-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release && cp target/release/devg /usr/local/bin/devg

# Proxy sidecar: minimal image, just the binary
FROM debian:bookworm-slim AS proxy
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/devg /usr/local/bin/devg
RUN mkdir -p /var/log/devg
CMD ["devg", "proxy"]

# Dev environment: full toolchain for working on devg itself
FROM ubuntu:24.04 AS dev
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git zsh sudo build-essential pkg-config dnsutils \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/devg /usr/local/bin/devg
RUN userdel -r ubuntu 2>/dev/null; \
    groupadd -f -g 1000 vscode \
    && useradd -u 1000 -g 1000 -m -s /bin/zsh vscode \
    && echo "vscode ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers
USER vscode
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    --default-toolchain stable \
    --component clippy,rustfmt
ENV PATH="/home/vscode/.cargo/bin:${PATH}"
WORKDIR /home/vscode
