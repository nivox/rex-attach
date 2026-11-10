#!/bin/sh
# Keep compiler subprocesses and implicit search paths outside the Nix toolchain.
set -eu
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
unset LIBRARY_PATH CPATH C_INCLUDE_PATH CPLUS_INCLUDE_PATH OBJC_INCLUDE_PATH
export SDKROOT="$(/usr/bin/xcrun --sdk macosx --show-sdk-path)"
linker=$(/usr/bin/xcrun --sdk macosx --find ld)
exec /usr/bin/clang -fuse-ld="$linker" -isysroot "$SDKROOT" "$@"
