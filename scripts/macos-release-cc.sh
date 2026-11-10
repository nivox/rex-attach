#!/bin/sh
# Keep compiler subprocesses and implicit search paths outside the Nix toolchain.
set -eu
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
unset LIBRARY_PATH CPATH C_INCLUDE_PATH CPLUS_INCLUDE_PATH OBJC_INCLUDE_PATH
unset SDKROOT DEVELOPER_DIR TOOLCHAINS
# Cargo's Nix wrapper supplies its own dynamic library paths. Apple tools must
# resolve their own libraries without inheriting those paths.
unset DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH DYLD_INSERT_LIBRARIES
DEVELOPER_DIR=$(/usr/bin/xcode-select --print-path)
export DEVELOPER_DIR
SDKROOT=$(/usr/bin/xcrun --no-cache --sdk macosx --show-sdk-path)
export SDKROOT
linker=$(/usr/bin/xcrun --no-cache --sdk macosx --find ld)
case "$DEVELOPER_DIR:$SDKROOT:$linker" in
  */nix/store/*) echo 'Apple release tools must use the host Xcode installation' >&2; exit 1 ;;
esac
test -x "$linker"
exec /usr/bin/clang -fuse-ld="$linker" -isysroot "$SDKROOT" "$@"
