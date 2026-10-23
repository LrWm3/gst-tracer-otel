# Native ARM validation on an integrated memory nvidia device; also builds on x86-64.
# The host needs Docker, but its installed GStreamer/Rust are not modified.
FROM rust:1.89-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-tools gstreamer1.0-plugins-base \
    python3 pkg-config binutils util-linux procps qemu-user \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_TOOLCHAIN=1.89.0
ENV RUSTFLAGS="-C force-frame-pointers=yes"
ENV CARGO_BUILD_JOBS=2
WORKDIR /work
