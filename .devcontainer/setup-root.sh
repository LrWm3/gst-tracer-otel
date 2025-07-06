#!/usr/bin/env bash
##
# Set-up script for the development container & local.
set -euo pipefail

if ! command -v apt-get &> /dev/null; then
  echo "apt-get is not installed. Skipping gstreamer installation."
else
  if ! command -v gst-launch-1.0 &> /dev/null; then
    echo "Installing GStreamer dependencies..."
      
    apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        gdb \
        libgstreamer1.0-dev \
        libgstreamer-plugins-base1.0-dev \
        gstreamer1.0-tools \
        gstreamer1.0-plugins-base \
        llvm-14 llvm-14-tools clang \
        curl \
        ca-certificates \
      && rm -rf /var/lib/apt/lists/*
    ln -s "$(which llvm-symbolizer-14)" /usr/bin/llvm-symbolizer || true
    else
      echo "GStreamer is already installed."
    fi
fi
