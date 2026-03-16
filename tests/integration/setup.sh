#!/bin/bash
# Set up a minimal devcontainer for integration testing.
# Run from the repo root after building the kap binary and sidecar image.
#
# Usage:
#   cargo build --release
#   docker build --target proxy -t kap-test -f .devcontainer/Dockerfile .
#   tests/integration/setup.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PROJECT_DIR="$SCRIPT_DIR/project"
DC_DIR="$PROJECT_DIR/.devcontainer"

# Clean previous run
rm -rf "$PROJECT_DIR"
mkdir -p "$DC_DIR"

# --- 1. Fake SSH signing setup (simulates 1Password on macOS) ---
rm -f "$SCRIPT_DIR/test-sign-key" "$SCRIPT_DIR/test-sign-key.pub"
ssh-keygen -t ed25519 -f "$SCRIPT_DIR/test-sign-key" -N "" -C "ci@test" -q

# Create a SANDBOXED gitconfig (never touch ~/.gitconfig).
# This simulates a developer with 1Password commit signing configured.
# The compose overlay mounts this into the app container.
cat > "$SCRIPT_DIR/test-gitconfig" <<GITCFG
[user]
    name = CI Test
    email = ci@test.local
    signingkey = $(cat "$SCRIPT_DIR/test-sign-key.pub")
[gpg]
    format = ssh
[gpg "ssh"]
    program = /nonexistent/op-ssh-sign
[commit]
    gpgsign = true
[init]
    defaultBranch = main
GITCFG

# --- 2. Create minimal project fixtures ---

cat > "$DC_DIR/kap.toml" <<'TOML'
ssh_signing = true

[proxy.network]
allow = ["github.com", "*.github.com"]

[compose]
image = "kap-test"
TOML

# Build a minimal app image with git and test tools pre-installed
docker build -t kap-test-app -f - . <<'DOCKERFILE'
FROM debian:bookworm-slim
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
    git openssh-client curl dnsutils ca-certificates \
    && rm -rf /var/lib/apt/lists/*
CMD ["sleep", "infinity"]
DOCKERFILE

cat > "$DC_DIR/docker-compose.yml" <<YAML
services:
  app:
    image: kap-test-app
    command: sleep infinity
    volumes:
      - ..:/workspace
      # Mount the test gitconfig as ~/.gitconfig (simulates devcontainer CLI copy)
      - $SCRIPT_DIR/test-gitconfig:/root/.gitconfig:ro
YAML

cat > "$DC_DIR/devcontainer.json" <<'JSON'
{
  "name": "kap-integration-test",
  "service": "app",
  "workspaceFolder": "/workspace",
  "dockerComposeFile": ["docker-compose.yml", "docker-compose.kap.yml"],
  "remoteEnv": {
    "PATH": "/opt/kap/bin:${containerEnv:PATH}"
  },
  "postStartCommand": {
    "kap-setup": "/opt/kap/bin/kap-post-start"
  }
}
JSON

# --- 3. Generate overlay and .env ---
# kap sidecar-init reads kap.toml and generates docker-compose.kap.yml + .env
(cd "$PROJECT_DIR" && kap sidecar-init)

# --- 4. Start containers ---
DC="docker compose --project-name kap-integration -f $DC_DIR/docker-compose.yml -f $DC_DIR/docker-compose.kap.yml"
# kap-test and kap-test-app are locally-built images, don't try to pull them
$DC up -d --wait --wait-timeout 30 --pull never

echo ""
echo "=== Integration test environment ready ==="
echo "  Project: $PROJECT_DIR"
echo "  Compose: $DC"
