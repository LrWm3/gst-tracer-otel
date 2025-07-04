FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# Use bash as default shell
SHELL ["/bin/bash", "-lc"]

# Create vscode user (UID 1000) and set as default
ARG USERNAME=vscode
ARG USER_UID=1000
ARG USER_GID=$USER_UID

RUN groupadd --gid $USER_GID $USERNAME \
  && useradd --uid $USER_UID --gid $USER_GID -m $USERNAME \
  && chown -R $USERNAME:$USERNAME /home/$USERNAME

USER $USERNAME
WORKDIR /home/$USERNAME

RUN apt-get update && apt-get install -y docker.io && rm -rf /var/lib/apt/lists/* \
  && usermod -aG docker $USERNAME

# Install GStreamer dependencies
COPY .devcontainer/setup.sh /tmp/setup.sh

RUN chmod +x /tmp/setup.sh \
  && /tmp/setup.sh \
  && rm /tmp/setup.sh

ENV PATH="/home/$USERNAME/.cargo/bin:$PATH"
