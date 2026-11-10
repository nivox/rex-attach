#!/usr/bin/env bash
# Exercise the release linker before spending time building all dependencies.
set -euo pipefail
probe_dir=$(mktemp -d)
trap 'rm -rf "$probe_dir"' EXIT
printf 'fn main() { println!("release linker probe"); }\n' > "$probe_dir/probe.rs"
"$CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER" -### -x c /dev/null -o "$probe_dir/clang-probe"
rustc --crate-name release_linker_probe --target "$CARGO_BUILD_TARGET" \
  -C "linker=$CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER" \
  -L "native=$SDKROOT/usr/lib" "$probe_dir/probe.rs" -o "$probe_dir/probe"
libraries=$(/usr/bin/otool -L "$probe_dir/probe")
printf '%s\n' "$libraries"
if [[ "$libraries" == */nix/store/* ]]; then
  echo 'macOS release toolchain probe depends on the Nix store' >&2
  exit 1
fi
"$probe_dir/probe"
