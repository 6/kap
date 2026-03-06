#!/bin/bash
# Smoke test for proxy enforcement inside the devcontainer.
# Run this from the app container to verify devcontainer-guard works.
set -euo pipefail

PASS=0
FAIL=0

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

echo "=== devg smoke tests ==="
echo ""

# --- Test 1: Allowed HTTPS domain ---
echo "[1] HTTPS to allowed domain (github.com)"
if curl -sf --max-time 10 -o /dev/null https://github.com; then
  pass "curl to github.com succeeded"
else
  fail "curl to github.com failed (expected success)"
fi

# --- Test 2: Denied HTTPS domain ---
echo "[2] HTTPS to blocked domain (example.com)"
HTTP_CODE=$(curl -s --max-time 10 -o /dev/null -w '%{http_code}' https://example.com 2>/dev/null || true)
if [ "$HTTP_CODE" = "403" ] || [ "$HTTP_CODE" = "000" ]; then
  pass "curl to example.com was blocked (HTTP $HTTP_CODE)"
else
  fail "curl to example.com returned HTTP $HTTP_CODE (expected 403 or connection refused)"
fi

# --- Test 3: Proxy is reachable from app container ---
echo "[3] proxy reachable on internal network"
if curl -sf --max-time 5 -o /dev/null -x http://proxy:3128 http://github.com; then
  pass "proxy at proxy:3128 is reachable"
else
  fail "proxy at proxy:3128 is not reachable"
fi

# --- Test 4: cargo fetch works through proxy ---
echo "[4] cargo fetch through proxy"
if cargo fetch --manifest-path /workspaces/devcontainer-guard/Cargo.toml 2>&1 | tail -1; then
  pass "cargo fetch succeeded through proxy"
else
  fail "cargo fetch failed"
fi

# --- Summary ---
echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
