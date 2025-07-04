#!/usr/bin/env bash
##
# Set-up script for the development container & local.
set -euo pipefail

if ! command -v rustup &> /dev/null; then
  echo "Installing rustup..."
  # Install rustup and toolchains
  # shellcheck disable=SC1091
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && . "$HOME/.cargo/env"
else
  echo "rustup is already installed."
fi

if ! command -v just &> /dev/null; then
  echo "Installing just..."
  # Install just
  curl --proto '=https' --tlsv1.2 -sSf https://just.systems/install.sh | bash -s -- --to "/home/$USER/bin"
else
  echo "just is already installed."
fi

rustup install stable \
  && rustup install nightly \
  && rustup update

# Ensure cargo bin is on the path for the current session
PATH="$HOME/.cargo/bin:$PATH"

# Install act for local CI testing
if ! command -v act &> /dev/null; then
  echo "Installing act..."
  curl -s https://raw.githubusercontent.com/nektos/act/master/install.sh | bash -s -- -b "/home/$USER/bin"
else
  echo "act is already installed."
fi
