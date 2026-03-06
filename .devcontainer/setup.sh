#!/bin/bash
set -e

# Fix ownership on Docker volumes
sudo chown -R vscode:vscode /home/vscode/.cargo
sudo chown -R vscode:vscode /workspaces/devcontainer-guard/target 2>/dev/null || true

# Persistent shell history
sudo chown vscode:vscode /commandhistory
touch /commandhistory/.zsh_history
echo 'HISTFILE=/commandhistory/.zsh_history' >> ~/.zshrc

# Install Claude Code
if ! command -v claude &>/dev/null; then
  echo "Installing Claude Code..."
  curl -fsSL https://claude.ai/install.sh | bash
  echo 'export PATH="$HOME/.claude/bin:$PATH"' >> ~/.zshrc
  echo 'export PATH="$HOME/.claude/bin:$PATH"' >> ~/.bashrc
fi

echo ""
echo "=== devcontainer ready ==="
echo "Run 'cargo build' to build devg from source"
echo "Run 'cargo test' to run the test suite"
echo "Run '.devcontainer/smoke-test.sh' to verify proxy enforcement"
