{
  description = "rex-attach package and Rust development environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.rust-overlay = {
    url = "github:oxalica/rust-overlay";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      # The pinned Nixpkgs no longer supports x86_64-darwin.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          rex-attach = pkgs.callPackage ./nix/package.nix { };
        in
        {
          default = rex-attach;
          inherit rex-attach;
        });

      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          # Release builds use the minimum supported Rust version and include
          # the native musl standard library for portable Linux binaries.
          releaseTarget = if pkgs.stdenv.hostPlatform.isLinux then
            "${pkgs.stdenv.hostPlatform.parsed.cpu.name}-unknown-linux-musl"
          else
            pkgs.stdenv.hostPlatform.rust.rustcTarget;
          releaseRust = (pkgs.rust-bin.stable."1.96.0".minimal.override {
            targets = [ releaseTarget ];
          }).overrideAttrs (_: pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin {
            # rust-overlay propagates Nix libiconv for Darwin's Rust std.
            # Portable releases resolve iconv from the host Apple SDK instead.
            depsTargetTargetPropagated = [ ];
          });
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              python3
              rustc
              rustfmt
              clippy
              rust-analyzer
              pkg-config
              jq
              stdenv.cc
              zig_0_17
              dejavu_fonts
            ];

            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            REX_ATTACH_PREVIEW_FONT = "${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSansMono.ttf";
          };

          release = pkgs.mkShell {
            packages = [ releaseRust pkgs.pkg-config pkgs.zig_0_17 pkgs.jq ];
            CARGO_BUILD_TARGET = releaseTarget;
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = "-C target-feature=+crt-static -C linker=rust-lld";
            CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = "-C target-feature=+crt-static -C linker=rust-lld";
            # Portable macOS releases must use the host Apple SDK and tools.
            # Nix's compiler wrapper links libraries from the Nix store.
            shellHook = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
              export SDKROOT="$(/usr/bin/env -u SDKROOT /usr/bin/xcrun --sdk macosx --show-sdk-path)"
              export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/clang
              export CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS="-L native=$SDKROOT/usr/lib"
              export CC_aarch64_apple_darwin=/usr/bin/clang
              export CXX_aarch64_apple_darwin=/usr/bin/clang++
              export AR_aarch64_apple_darwin=/usr/bin/ar
            '';
          };
        });
    };
}
