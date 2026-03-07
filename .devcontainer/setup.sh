#!/bin/bash
set -e

# Fix ownership on Docker volumes
sudo chown -R vscode:vscode /home/vscode/.cargo
sudo chown -R vscode:vscode /workspaces/kap/target 2>/dev/null || true

# Persistent shell history
sudo chown vscode:vscode /commandhistory
touch /commandhistory/.zsh_history
echo 'HISTFILE=/commandhistory/.zsh_history' >> ~/.zshrc

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
echo 'export GIT_CONFIG_GLOBAL=~/.gitconfig-local' >> ~/.zshenv

# mise trust for this project
mise trust 2>/dev/null || true

echo ""
echo "=== devcontainer ready ==="
