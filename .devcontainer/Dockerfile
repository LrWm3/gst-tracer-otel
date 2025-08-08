FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# Use bash as default shell
SHELL ["/bin/bash", "-lc"]

ENV GITHUB_ACTIONS=true

RUN apt-get update && apt-get install -y openssl libssl-dev docker.io && rm -rf /var/lib/apt/lists/*

# Install dependencies which require root privileges
COPY .devcontainer/setup-root.sh /tmp/setup-root.sh
RUN /tmp/setup-root.sh

# Install Rust and other user-level dependencies
COPY .devcontainer/setup-user.sh /tmp/setup-user.sh
RUN /tmp/setup-user.sh

ENV PATH="/root/.cargo/bin:/root/.local/bin:$PATH"
