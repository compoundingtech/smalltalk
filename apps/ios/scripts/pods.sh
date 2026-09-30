#!/bin/sh
set -eu

export LANG=en_US.UTF-8
export LC_ALL=en_US.UTF-8
cd "$(dirname "$0")/../ios"
exec pod install "$@"
