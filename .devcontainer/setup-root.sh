#!/usr/bin/env bash
### Set-up script for the development container & local.
set -euo pipefail

if ! command -v apt-get &> /dev/null; then
  echo "apt-get is not installed. Skipping GStreamer installation."
else
  if ! command -v gst-launch-1.0 &> /dev/null; then
    echo "Installing GStreamer dependencies..."

    # 1) Base dependencies + symbolizer
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
      ubuntu-dbgsym-keyring \
    && rm -rf /var/lib/apt/lists/*

    # 2) Symlink the symbolizer so ASan can find it
    ln -s "$(which llvm-symbolizer-14)" /usr/bin/llvm-symbolizer || true

    # 3) Enable the ddebs (debug-symbol) repository
    echo "deb http://ddebs.ubuntu.com $(lsb_release -cs) main restricted universe multiverse" \
      | sudo tee /etc/apt/sources.list.d/ddebs.list
    echo "deb http://ddebs.ubuntu.com $(lsb_release -cs)-updates main restricted universe multiverse" \
      | sudo tee -a /etc/apt/sources.list.d/ddebs.list
    echo "deb http://ddebs.ubuntu.com $(lsb_release -cs)-proposed main restricted universe multiverse" \
      | sudo tee -a /etc/apt/sources.list.d/ddebs.list

    # 4) Install the GStreamer debug-symbol packages
    apt-get update && apt-get install -y --no-install-recommends \
      libgstreamer1.0-0-dbgsym \
      gstreamer1.0-plugins-base-dbgsym \
      gstreamer1.0-plugins-good-dbgsym \
      gstreamer1.0-plugins-bad-dbgsym \
      gstreamer1.0-plugins-ugly-dbgsym \
      gstreamer1.0-libav-dbg \
    && rm -rf /var/lib/apt/lists/*

  else
    echo "GStreamer is already installed."
  fi
fi
