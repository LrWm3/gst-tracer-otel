#!/usr/bin/env bash
##
# Set-up script for the development container & local.
set -euo pipefail

# get this dir
DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"

# run setup-root.sh 
bash "$DIR/setup-root.sh"

# run setup-user.sh
bash "$DIR/setup-user.sh"
