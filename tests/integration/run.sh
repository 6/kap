#!/bin/bash
# Integration tests for kap devcontainers.
# Expects containers to be running (see setup.sh).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DC_DIR="$SCRIPT_DIR/project/.devcontainer"
DC="docker compose --project-name kap-integration -f $DC_DIR/docker-compose.yml -f $DC_DIR/docker-compose.kap.yml"

PASS=0
FAIL=0

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

run() {
  # Run a command in the app container
  $DC exec -T app "$@"
}

# Wait up to N seconds for a condition to become true (polls every 1s).
# Usage: wait_for 15 run getent hosts example.com
wait_for() {
  local timeout=$1; shift
  for i in $(seq 1 "$timeout"); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  return 1
}

# Wait up to N seconds for a condition to become false.
wait_for_not() {
  local timeout=$1; shift
  for i in $(seq 1 "$timeout"); do
    if ! "$@" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  return 1
}

run_kap() {
  # Run a command in the kap sidecar
  $DC exec -T kap "$@"
}

echo "=== kap integration tests ==="
echo ""

# --- Sidecar health ---
echo "--- Sidecar ---"

echo "[1] Sidecar healthy"
if run_kap kap sidecar-check --proxy >/dev/null 2>&1; then
  pass "sidecar proxy health check"
else
  fail "sidecar proxy health check"
fi

# --- Git commit signing ---
echo ""
echo "--- Git commit signing ---"

echo "[2] GIT_CONFIG_GLOBAL set in app container"
if run printenv GIT_CONFIG_GLOBAL | grep -q /opt/kap/gitconfig; then
  pass "GIT_CONFIG_GLOBAL=/opt/kap/gitconfig"
else
  fail "GIT_CONFIG_GLOBAL not set (got: $(run printenv GIT_CONFIG_GLOBAL 2>/dev/null || echo '<unset>'))"
fi

echo "[3] Gitconfig wrapper on shared volume"
if run cat /opt/kap/gitconfig | grep -q 'program = /usr/bin/ssh-keygen'; then
  pass "gitconfig wrapper contains ssh-keygen override"
else
  fail "gitconfig wrapper missing or wrong content"
fi

echo "[4] gpg.ssh.program overridden to ssh-keygen"
if run git config gpg.ssh.program | grep -q /usr/bin/ssh-keygen; then
  pass "gpg.ssh.program = /usr/bin/ssh-keygen"
else
  fail "gpg.ssh.program not overridden (got: $(run git config gpg.ssh.program 2>/dev/null || echo '<unset>'))"
fi

echo "[5] Signed commit succeeds"
# Copy the test keypair into the container. ssh-keygen finds the private key
# at the same path as the public key minus .pub (no agent needed).
APP_CONTAINER=$($DC ps -q app)
docker cp "$SCRIPT_DIR/test-sign-key" "$APP_CONTAINER:/tmp/test-sign-key"
docker cp "$SCRIPT_DIR/test-sign-key.pub" "$APP_CONTAINER:/tmp/test-sign-key.pub"
if run bash -c '
  chmod 600 /tmp/test-sign-key
  cd /tmp && rm -rf test-repo && git init test-repo && cd test-repo
  git config user.name "CI Test"
  git config user.email "ci@test.local"
  git config user.signingkey /tmp/test-sign-key.pub
  git commit --allow-empty -m "test signed commit"
' 2>&1; then
  pass "signed commit succeeded"
else
  fail "signed commit failed"
fi

# --- Domain proxy ---
echo ""
echo "--- Domain proxy ---"

echo "[6] DNS resolves allowed domain"
if run getent hosts github.com >/dev/null 2>&1; then
  pass "github.com resolves"
else
  # getent may not be installed in slim image, try dig
  if run bash -c 'apt-get update -qq && apt-get install -y -qq dnsutils >/dev/null 2>&1 && dig +short github.com | grep -q .' 2>/dev/null; then
    pass "github.com resolves (via dig)"
  else
    fail "github.com does not resolve"
  fi
fi

echo "[7] DNS blocks disallowed domain"
if ! run getent hosts evil.example.com >/dev/null 2>&1; then
  pass "evil.example.com blocked"
else
  fail "evil.example.com resolved (should be blocked)"
fi

echo "[8] HTTPS to disallowed domain denied"
# Install curl if not present (debian:bookworm-slim may not have it)
run bash -c 'command -v curl >/dev/null || (apt-get update -qq && apt-get install -y -qq curl) >/dev/null 2>&1' 2>/dev/null || true
if ! run curl -sf --max-time 5 https://example.com >/dev/null 2>&1; then
  pass "example.com HTTPS denied"
else
  fail "example.com HTTPS succeeded (should be denied)"
fi

# --- Hot-reload ---
echo ""
echo "--- Hot-reload ---"

TOML_PATH="$SCRIPT_DIR/project/.devcontainer/kap.toml"
TOML_BACKUP="$TOML_PATH.bak"
cp "$TOML_PATH" "$TOML_BACKUP"

echo "[9] Domain blocked before allowlist change"
if ! run getent hosts example.com >/dev/null 2>&1; then
  pass "example.com blocked before hot-reload"
else
  fail "example.com was not blocked (cannot test hot-reload)"
fi

echo "[10] Hot-reload: add domain to allowlist"
cat > "$TOML_PATH" <<'TOML'
ssh_signing = true

[proxy.network]
allow = ["github.com", "*.github.com", "example.com"]

[compose]
image = "kap-test"
TOML
# Sidecar polls every 2s; bind-mount propagation on Docker Desktop can be slow
if wait_for 15 run getent hosts example.com; then
  pass "example.com resolves after adding to allowlist"
else
  fail "example.com still blocked after adding to allowlist"
fi

echo "[11] Hot-reload: remove domain from allowlist"
cp "$TOML_BACKUP" "$TOML_PATH"

if wait_for_not 15 run getent hosts example.com; then
  pass "example.com blocked again after removing from allowlist"
else
  fail "example.com still resolves after removing from allowlist"
fi

# --- Post-start script ---
echo ""
echo "--- Post-start script ---"

echo "[12] Post-start script exists and is executable"
if run test -x /opt/kap/bin/kap-post-start; then
  pass "kap-post-start is executable"
else
  fail "kap-post-start missing or not executable"
fi

echo "[13] Kap binary available on shared volume"
if run test -x /opt/kap/kap; then
  pass "kap binary on shared volume"
else
  fail "kap binary missing"
fi

# --- Summary ---
echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
