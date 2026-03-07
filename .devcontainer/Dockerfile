# Build devg binary (shared by both targets)
FROM rust:1.93-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release && cp target/release/devg /usr/local/bin/devg

# Proxy sidecar: devg binary + gh CLI.
# Runs domain proxy (:3128), DNS forwarder (:53), MCP proxy (:3129), CLI proxy (:3130).
FROM debian:bookworm-slim AS proxy
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
       -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
       > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/devg /usr/local/bin/devg
RUN mkdir -p /var/log/devg
CMD ["devg", "proxy"]

# Dev environment: full Rust toolchain for working on devg itself.
# This is the app container in the dogfood devcontainer.
FROM ubuntu:24.04 AS dev
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git zsh sudo build-essential pkg-config dnsutils \
    jq ripgrep fd-find locales openssh-client \
    && locale-gen en_US.UTF-8 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/devg /usr/local/bin/devg
# Create vscode user (matches devcontainer convention)
RUN userdel -r ubuntu 2>/dev/null; \
    groupadd -f -g 1000 vscode \
    && useradd -u 1000 -g 1000 -m -s /bin/zsh vscode \
    && echo "vscode ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers
USER vscode

# Install mise
RUN curl https://mise.jdx.dev/install.sh | sh
ENV PATH="/home/vscode/.local/share/mise/shims:/home/vscode/.local/bin:${PATH}"

# Install Rust toolchain
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    --default-toolchain stable \
    --component clippy,rustfmt
ENV PATH="/home/vscode/.cargo/bin:${PATH}"

# Install Claude Code
RUN curl -fsSL https://claude.ai/install.sh | bash
ENV PATH="/home/vscode/.local/bin:/home/vscode/.claude/bin:${PATH}"

# Shell config (prompt, mise activation, aliases, onboarding)
RUN echo 'PS1="%F{cyan}devg%f %F{blue}%~%f %# "' > ~/.zshrc \
    && echo 'eval "$(mise activate zsh)"' >> ~/.zshrc \
    && echo 'alias ccd="claude --dangerously-skip-permissions"' >> ~/.zshrc \
    && echo '{"hasCompletedOnboarding":true}' > ~/.claude.json \
    && mkdir -p ~/.claude \
    && echo '{"model":"opus","effortLevel":"high","env":{"CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY":"1","CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS":"1","DISABLE_ERROR_REPORTING":"1","DISABLE_TELEMETRY":"1"}}' > ~/.claude/settings.json

WORKDIR /home/vscode
