#!/bin/bash
# Clean up integration test containers and fixtures.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DC_DIR="$SCRIPT_DIR/project/.devcontainer"

if [ -f "$DC_DIR/docker-compose.yml" ] && [ -f "$DC_DIR/docker-compose.kap.yml" ]; then
  docker compose --project-name kap-integration \
    -f "$DC_DIR/docker-compose.yml" \
    -f "$DC_DIR/docker-compose.kap.yml" \
    down -v --remove-orphans 2>/dev/null || true
fi

rm -rf "$SCRIPT_DIR/project"
rm -f "$SCRIPT_DIR/test-sign-key" "$SCRIPT_DIR/test-sign-key.pub" "$SCRIPT_DIR/test-gitconfig"
