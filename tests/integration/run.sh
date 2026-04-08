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
  # Set up allowedSignersFile so --show-signature can verify
  echo "ci@test.local $(cat /tmp/test-sign-key.pub)" > /tmp/allowed-signers
  git config gpg.ssh.allowedSignersFile /tmp/allowed-signers
  git commit --allow-empty -m "test signed commit"
' 2>&1; then
  pass "signed commit succeeded"
else
  fail "signed commit failed"
fi

echo "[6] Signature verification works (allowedSignersFile)"
if run bash -c '
  cd /tmp/test-repo
  git log --show-signature -1 2>&1
' 2>&1 | grep -q "Good"; then
  pass "signature verified via allowedSignersFile"
else
  fail "signature verification failed (allowedSignersFile missing or wrong)"
fi

# --- Domain proxy ---
echo ""
echo "--- Domain proxy ---"

echo "[7] DNS resolves allowed domain"
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

echo "[8] DNS blocks disallowed domain"
if ! run getent hosts evil.example.com >/dev/null 2>&1; then
  pass "evil.example.com blocked"
else
  fail "evil.example.com resolved (should be blocked)"
fi

echo "[9] HTTPS to disallowed domain denied"
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

echo "[10] Domain blocked before allowlist change"
if ! run getent hosts example.com >/dev/null 2>&1; then
  pass "example.com blocked before hot-reload"
else
  fail "example.com was not blocked (cannot test hot-reload)"
fi

echo "[11] Hot-reload: add domain to allowlist"
cat > "$TOML_PATH" <<'TOML'
ssh_signing = true

[proxy.network]
allow = ["github.com", "*.github.com", "example.com"]

[compose]
image = "kap-test"

[cli]
[[cli.tools]]
name = "curl"
mode = "proxy"
allow = ["*"]

[[cli.tools]]
name = "hostname"
mode = "proxy"
allow = ["*"]

[[cli.tools]]
name = "env"
mode = "direct"
env = ["TEST_SECRET"]
TOML
# Sidecar polls every 2s; bind-mount propagation on Docker Desktop can be slow
if wait_for 15 run getent hosts example.com; then
  pass "example.com resolves after adding to allowlist"
else
  fail "example.com still blocked after adding to allowlist"
fi

echo "[12] Hot-reload: remove domain from allowlist"
cp "$TOML_BACKUP" "$TOML_PATH"

if wait_for_not 15 run getent hosts example.com; then
  pass "example.com blocked again after removing from allowlist"
else
  fail "example.com still resolves after removing from allowlist"
fi

# --- CLI proxy ---
echo ""
echo "--- CLI proxy ---"

echo "[13] Shims created for configured tools"
if run test -x /opt/kap/bin/curl && run test -x /opt/kap/bin/hostname && run test -x /opt/kap/bin/env; then
  pass "shims exist for curl, hostname, env"
else
  fail "one or more shims missing"
fi

echo "[14] mode=proxy: hostname runs on sidecar"
APP_HOSTNAME=$(run hostname 2>/dev/null || echo "")
PROXY_HOSTNAME=$(run /opt/kap/bin/hostname 2>/dev/null || echo "")
if [ -n "$PROXY_HOSTNAME" ] && [ "$PROXY_HOSTNAME" != "$APP_HOSTNAME" ]; then
  pass "proxy hostname ($PROXY_HOSTNAME) differs from app ($APP_HOSTNAME)"
else
  fail "proxy hostname ($PROXY_HOSTNAME) same as app ($APP_HOSTNAME) or empty"
fi

echo "[15] mode=proxy: curl fetches through sidecar"
# The sidecar has direct internet access (no proxy needed). curl via shim
# runs on the sidecar, so it can reach github.com without going through
# the domain proxy.
if run /opt/kap/bin/curl -sf --max-time 10 -o /dev/null https://github.com 2>/dev/null; then
  pass "curl via proxy mode reached github.com"
else
  fail "curl via proxy mode failed"
fi

echo "[16] mode=direct: env var passed from sidecar to app"
# The sidecar has TEST_SECRET in its environment (from .env).
# Direct mode returns it to the shim, which exec's the real `env` binary.
DIRECT_OUTPUT=$(run /opt/kap/bin/env 2>/dev/null || echo "")
if echo "$DIRECT_OUTPUT" | grep -q "TEST_SECRET=s3cret-from-sidecar"; then
  pass "TEST_SECRET passed from sidecar to app via direct mode"
else
  fail "TEST_SECRET not found in direct mode output"
fi

# --- Post-start script ---
echo ""
echo "--- Post-start script ---"

echo "[17] Post-start script exists and is executable"
if run test -x /opt/kap/bin/kap-post-start; then
  pass "kap-post-start is executable"
else
  fail "kap-post-start missing or not executable"
fi

echo "[18] Kap binary available on shared volume"
if run test -x /opt/kap/kap; then
  pass "kap binary on shared volume"
else
  fail "kap binary missing"
fi

# --- Subnet drift ---
echo ""
echo "--- Subnet drift ---"

echo "[19] Overlay subnet matches actual Docker network after sidecar-init"
# Simulate the drift scenario: sidecar-init regenerates the overlay while
# Docker networks already exist with specific subnets. The overlay must
# use the actual network subnets, not stale/recomputed ones.
SANDBOX_NET="kap-integration_kap_sandbox"
ACTUAL_SUBNET=$(docker network inspect "$SANDBOX_NET" --format '{{range .IPAM.Config}}{{.Subnet}}{{end}}' 2>/dev/null || echo "")
if [ -z "$ACTUAL_SUBNET" ]; then
  fail "could not inspect $SANDBOX_NET"
else
  # Re-run sidecar-init (regenerates the overlay)
  (cd "$SCRIPT_DIR/project" && kap sidecar-init) >/dev/null 2>&1
  # Read the sandbox subnet from the freshly generated overlay
  OVERLAY_SUBNET=$(grep -A2 'kap_sandbox:' "$DC_DIR/docker-compose.kap.yml" | grep 'subnet:' | head -1 | awk '{print $NF}')
  if [ "$OVERLAY_SUBNET" = "$ACTUAL_SUBNET" ]; then
    pass "overlay subnet ($OVERLAY_SUBNET) matches Docker network"
  else
    fail "overlay subnet ($OVERLAY_SUBNET) != Docker network ($ACTUAL_SUBNET)"
  fi
fi

# --- Summary ---
echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
