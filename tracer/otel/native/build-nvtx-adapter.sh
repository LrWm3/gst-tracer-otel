#!/bin/sh
set -eu
# Linux, 64 bit. This adapter needs neither CUDA headers nor a GPU.
output=${1:-target/libmars-nvtx.so}
mkdir -p "$(dirname "$output")"
cc -std=c11 -O2 -Wall -Wextra -Werror -fPIC -shared \
    "$(dirname "$0")/nvtx_adapter.c" -o "$output"
