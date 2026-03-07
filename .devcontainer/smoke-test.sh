#!/bin/bash
# Smoke test for kap enforcement.
# Run this from the app container to verify all three layers work.
#
# Pre-flight (run on host before starting the devcontainer):
#   gh auth status          # ensure GitHub CLI is authenticated
#   kap sidecar-init       # generates .env with GH_TOKEN + API keys
set -euo pipefail

PROXY_IP="172.28.0.3"
PASS=0
FAIL=0
SKIP=0

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }
skip() { echo "  SKIP: $1"; SKIP=$((SKIP + 1)); }

echo "=== kap smoke tests ==="
echo ""

# --- Domain proxy tests ---
echo "--- Domain proxy ---"

echo "[1] HTTPS to allowed domain (github.com)"
if curl -sf --max-time 10 -o /dev/null https://github.com; then
  pass "curl to github.com succeeded"
else
  fail "curl to github.com failed (expected success)"
fi

echo "[2] HTTPS to blocked domain (example.com)"
HTTP_CODE=$(curl -s --max-time 10 -o /dev/null -w '%{http_code}' https://example.com 2>/dev/null || true)
if [ "$HTTP_CODE" = "403" ] || [ "$HTTP_CODE" = "000" ]; then
  pass "curl to example.com was blocked (HTTP $HTTP_CODE)"
else
  fail "curl to example.com returned HTTP $HTTP_CODE (expected 403 or connection refused)"
fi

echo "[3] proxy reachable on internal network"
if curl -sf --max-time 5 -o /dev/null -x "http://$PROXY_IP:3128" http://github.com; then
  pass "proxy at $PROXY_IP:3128 is reachable"
else
  fail "proxy at $PROXY_IP:3128 is not reachable"
fi

# --- DNS forwarder tests ---
echo ""
echo "--- DNS forwarder ---"

echo "[4] DNS resolves allowed domain (github.com)"
if dig +short +time=5 github.com | grep -q .; then
  pass "github.com resolved"
else
  fail "github.com did not resolve (expected A record)"
fi

echo "[5] DNS blocks disallowed domain (evil.example.com)"
RESULT=$(dig +short +time=5 evil.example.com 2>/dev/null || true)
if [ -z "$RESULT" ]; then
  pass "evil.example.com returned empty (NXDOMAIN)"
else
  fail "evil.example.com resolved to $RESULT (expected NXDOMAIN)"
fi

echo "[6] DNS resolves wildcard subdomain (api.github.com)"
if dig +short +time=5 api.github.com | grep -q .; then
  pass "api.github.com resolved (matches *.github.com)"
else
  fail "api.github.com did not resolve"
fi

# --- MCP proxy tests ---
echo ""
echo "--- MCP proxy ---"

echo "[7] MCP proxy endpoint reachable"
MCP_STATUS=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 --noproxy '*' \
  -X POST "http://$PROXY_IP:3129/nonexistent" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' 2>/dev/null || true)
if [ "$MCP_STATUS" = "404" ]; then
  pass "MCP proxy returned 404 for unknown server (endpoint is up)"
else
  fail "MCP proxy returned $MCP_STATUS (expected 404)"
fi

echo "[8] auth dir mounted in sidecar"
# Check from inside the app container that the sidecar has the auth dir
AUTH_CHECK=$(curl -s --max-time 5 --noproxy '*' \
  -X POST "http://$PROXY_IP:3129/linear" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' 2>/dev/null || true)
if echo "$AUTH_CHECK" | grep -q '"tools"'; then
  pass "auth dir mounted (linear server loaded)"
elif echo "$AUTH_CHECK" | grep -q 'unknown MCP server'; then
  fail "auth dir not mounted in sidecar (add ~/.kap/auth:/etc/kap/auth to compose volumes)"
else
  skip "cannot determine auth mount status"
fi

# --- Real MCP test (Context7, requires CONTEXT7_API_KEY on the proxy) ---
echo ""
echo "--- MCP end-to-end (Context7) ---"

echo "[9] tools/list through MCP proxy to Context7 (optional)"
# The proxy sidecar has CONTEXT7_API_KEY. If the server is configured,
# we can call tools/list and verify we get real tools back.
MCP_RESP=$(curl -s --max-time 10 --noproxy '*' \
  -X POST "http://$PROXY_IP:3129/context7" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' 2>/dev/null || true)

if echo "$MCP_RESP" | grep -q '"tools"'; then
  TOOL_COUNT=$(echo "$MCP_RESP" | grep -o '"name"' | wc -l | tr -d ' ')
  pass "Context7 returned $TOOL_COUNT tools via MCP proxy"
elif echo "$MCP_RESP" | grep -q '"error"'; then
  # Server responded but no tools (auth issue, etc.)
  skip "Context7 returned error (CONTEXT7_API_KEY may not be set)"
elif echo "$MCP_RESP" | grep -q 'unknown MCP server'; then
  skip "context7 not configured in kap.toml"
else
  skip "Context7 not reachable (CONTEXT7_API_KEY may not be set)"
fi

# --- Cargo fetch (end-to-end: DNS + proxy + TLS) ---
echo ""
echo "--- End-to-end ---"

echo "[10] cargo fetch through proxy"
if cargo fetch --manifest-path /workspaces/kap/Cargo.toml 2>&1 | tail -1; then
  pass "cargo fetch succeeded (DNS + proxy + TLS all working)"
else
  fail "cargo fetch failed"
fi

# --- Summary ---
echo ""
echo "=== Results: $PASS passed, $FAIL failed, $SKIP skipped ==="
if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
