{
  description = "Rust development environment for rex-attach";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
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
        });
    };
}
