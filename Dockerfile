FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# Use bash as default shell
SHELL ["/bin/bash", "-lc"]

ARG USERNAME=ubuntu
ARG USER_UID=1000
ARG USER_GID=$USER_UID

RUN apt-get update && apt-get install -y docker.io && rm -rf /var/lib/apt/lists/*

# Install dependencies which require root privileges
COPY .devcontainer/setup-root.sh /tmp/setup-root.sh
RUN /tmp/setup-root.sh

USER $USERNAME
WORKDIR /home/$USERNAME
ENV HOME=/home/$USERNAME
ENV USER=$USERNAME

# Install Rust and other user-level dependencies
COPY .devcontainer/setup-user.sh /tmp/setup-user.sh
RUN /tmp/setup-user.sh

ENV PATH="/home/$USERNAME/.cargo/bin:$PATH"
