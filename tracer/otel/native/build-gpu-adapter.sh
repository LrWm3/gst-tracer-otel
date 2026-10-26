#!/bin/sh
set -eu
# CUDA_ROOT points at a CUDA 11.4 installation. No installation is performed.
cuda_root=${CUDA_ROOT:-/usr/local/cuda-11.4}
cuda_target=${CUDA_TARGET:-aarch64-linux}
output=${1:-target/libmars-cupti.so}
mkdir -p "$(dirname "$output")"
cc -std=c11 -O2 -Wall -Wextra -Werror -fPIC -shared \
    -I"$cuda_root/targets/$cuda_target/include" \
    "$(dirname "$0")/gpu_cupti.c" \
    -L"$cuda_root/targets/$cuda_target/lib" -Wl,-rpath,"$cuda_root/targets/$cuda_target/lib" \
    -lcupti -o "$output"
