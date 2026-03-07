#!/bin/bash
set -e

# Fix ownership on Docker volumes
sudo chown -R vscode:vscode /home/vscode/.cargo
sudo chown -R vscode:vscode /workspaces/devcontainer-guard/target 2>/dev/null || true

# Persistent shell history
sudo chown vscode:vscode /commandhistory
touch /commandhistory/.zsh_history
echo 'HISTFILE=/commandhistory/.zsh_history' >> ~/.zshrc

# Install mise and persist to zsh
if ! command -v mise &>/dev/null; then
  curl https://mise.jdx.dev/install.sh | sh
fi
export PATH="$HOME/.local/share/mise/shims:$HOME/.local/bin:$PATH"
echo 'export PATH="$HOME/.local/share/mise/shims:$HOME/.local/bin:$PATH"' >> ~/.zshenv
echo '[ -x ~/.local/bin/mise ] && eval "$(~/.local/bin/mise activate zsh)"' >> ~/.zshrc
mise trust 2>/dev/null || true
mise install 2>/dev/null || true

# Install Claude Code
if ! command -v claude &>/dev/null; then
  echo "Installing Claude Code..."
  curl -fsSL https://claude.ai/install.sh | bash
fi
echo 'export PATH="$HOME/.local/bin:$HOME/.claude/bin:$PATH"' >> ~/.zshenv

# Claude Code onboarding + alias
echo '{"hasCompletedOnboarding":true}' > ~/.claude.json
echo 'alias ccd="claude --dangerously-skip-permissions"' >> ~/.zshrc

# Git signing fix: host mounts op-ssh-sign which doesn't exist in Linux
SIGNING_KEY=$(git config -f ~/.gitconfig user.signingkey 2>/dev/null || true)
if [ -n "$SIGNING_KEY" ]; then
  echo "$SIGNING_KEY" > ~/.ssh-signing-key.pub
fi
cat > ~/.gitconfig-local <<'GITCFG'
[include]
    path = ~/.gitconfig
[gpg "ssh"]
    program = /usr/bin/ssh-keygen
GITCFG
if [ -f ~/.ssh-signing-key.pub ]; then
  git config -f ~/.gitconfig-local user.signingkey ~/.ssh-signing-key.pub
fi
export GIT_CONFIG_GLOBAL=~/.gitconfig-local
echo 'export GIT_CONFIG_GLOBAL=~/.gitconfig-local' >> ~/.zshenv

echo ""
echo "=== devcontainer ready ==="
echo "Run 'cargo build' to build devg from source"
echo "Run 'cargo test' to run the test suite"
echo "Run '.devcontainer/smoke-test.sh' to verify proxy enforcement"
