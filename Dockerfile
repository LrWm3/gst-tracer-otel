FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

SHELL ["/bin/bash", "-lc"]

ARG USERNAME=ubuntu
ARG USER_UID=1000
ARG USER_GID=$USER_UID

# Create a non-root user
RUN groupadd --gid $USER_GID $USERNAME \
    && useradd --uid $USER_UID --gid $USER_GID -m $USERNAME

# Install git and docker then clean up
RUN apt-get update \
    && apt-get install -y git docker.io \
    && rm -rf /var/lib/apt/lists/* \
    && usermod -aG docker $USERNAME

# Install dependencies requiring root
COPY .devcontainer/setup-root.sh /tmp/setup-root.sh
RUN bash /tmp/setup-root.sh

# Switch to the non-root user
USER $USERNAME
WORKDIR /home/$USERNAME
ENV HOME=/home/$USERNAME
ENV USER=$USERNAME
ENV PATH="/home/$USERNAME/.cargo/bin:/home/$USERNAME/bin:$PATH"

# Install user-level dependencies and nightly component
COPY .devcontainer/setup-user.sh /tmp/setup-user.sh
RUN GITHUB_ACTIONS=true bash /tmp/setup-user.sh \
    && rustup component add llvm-tools-preview --toolchain nightly

