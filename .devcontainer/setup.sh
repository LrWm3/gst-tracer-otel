#!/usr/bin/env bash
##
# Set-up script for the development container & local.

if ! command -v apt-get &> /dev/null; then
  echo "apt-get is not installed. Skipping gstreamer installation."
else
  if ! command -v gst-launch-1.0 &> /dev/null; then
    echo "Installing GStreamer dependencies..."
      
    apt-get update && apt-get install -y --no-install-recommends \
        gdb \
        libgstreamer1.0-dev \
        libgstreamer-plugins-base1.0-dev \
        gstreamer1.0-tools \
        gstreamer1.0-plugins-base \
        curl \
        ca-certificates \
      && rm -rf /var/lib/apt/lists/*
    else
      echo "GStreamer is already installed."
    fi
fi

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
  curl -fsSL https://raw.githubusercontent.com/casey/just/master/install.sh | bash -s -- -b /usr/local/bin
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
  curl -s https://raw.githubusercontent.com/nektos/act/master/install.sh | bash -s -- -b /usr/local/bin
else
  echo "act is already installed."
fi
