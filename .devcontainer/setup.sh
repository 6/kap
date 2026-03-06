#!/bin/bash
set -e

# Fix ownership on Docker volumes
sudo chown -R vscode:vscode /home/vscode/.cargo
sudo chown -R vscode:vscode /workspaces/devcontainer-egress-proxy/target 2>/dev/null || true

# Configure git to use devp credential helper
devp credential --install

# Persistent shell history
sudo chown vscode:vscode /commandhistory
touch /commandhistory/.zsh_history
echo 'HISTFILE=/commandhistory/.zsh_history' >> ~/.zshrc

# Verify the proxy setup works
echo ""
echo "=== Running devp check ==="
devp check || true

echo ""
echo "=== devcontainer ready ==="
echo "Run 'cargo build' to build devp from source"
echo "Run 'cargo test' to run the test suite"
echo "Run '.devcontainer/smoke-test.sh' to verify proxy enforcement"
