# Releasing rex-attach

The [release workflow](.github/workflows/release.yml) builds each platform on a
native GitHub runner using `nix develop .#release`. The release shell pins Rust
1.96.0 and selects the native target, including musl on Linux:

```sh
nix develop .#release --command cargo build --release --locked
```

On macOS, the release shell uses Apple's linker and SDK from the active Xcode
installation (selected with `xcode-select`). This keeps the release binary's
runtime libraries independent of the Nix store. The release toolchain disables
rust-overlay's propagated Nix `libiconv` dependency and searches the Apple SDK's
libraries explicitly, so Rust's standard library links against system `libiconv`.
The compiler wrapper also selects Apple's `ld` explicitly and clears inherited
library search variables. CI first compiles and runs a small Rust probe, prints
the selected linker, and checks its libraries for Nix store paths. It repeats the
library check on the final binary before packaging.

The binary is written to `target/<target>/release/rex-attach`. From a clean,
committed working tree, use the release script:

```sh
nix develop --command python3 scripts/release.py --dry-run
nix develop --command python3 scripts/release.py --push
```

The script keeps `Cargo.toml` and the package entry in `Cargo.lock` in sync,
creates a release commit and annotated `v<version>` tag, then commits the next
development version with a `-dev` suffix. The default release strips `-dev` from
the current version; the next version bumps the minor (`0.1.0` → `0.2.0-dev`).
An initial plain version such as `0.1.0` is also accepted. Use `--version X.Y.Z`
and `--next X.Y.Z` to override these defaults, and `--yes` to skip confirmation.
Without `--push`, commits and tags stay local. With `--push`, the branch and tag
are pushed atomically to `origin`; the tag triggers GitHub Actions.

All three builds must succeed before the workflow creates a release and uploads
archives and checksums. Tags with a prerelease suffix create a GitHub prerelease.
A manual **Run workflow** builds downloadable Actions artifacts; when run on a
tag it also publishes the release.

CI and release builds check both locked Ghostty crate versions and the Nix
package's source revision during evaluation. If the binding changes, update the
pins and hashes in `nix/package.nix` together; a stale version fails before
compilation.
