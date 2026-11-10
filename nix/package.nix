{
  lib,
  rustPlatform,
  fetchzip,
  runCommand,
  zig_0_17,
  pkg-config,
  dejavu_fonts,
}:
let
  manifest = builtins.fromTOML (builtins.readFile ../Cargo.toml);
  lock = builtins.fromTOML (builtins.readFile ../Cargo.lock);
  ghosttyVersion = "0.8.0+ghostty.1.4.0-main.83fb956";
  ghosttyRevision = "83fb95624b0439ee660c2679b43b38c8b63769cd";
  ghosttyMatchesLock = lib.all (name:
    let
      packages = builtins.filter (package: package.name == name) lock.package;
    in
    builtins.length packages == 1 && (builtins.head packages).version == ghosttyVersion
  ) [ "ghostty-vt" "ghostty-vt-sys" ];

  # Keep these in sync with ghostty-vt-sys's GHOSTTY.lock in Cargo.lock's
  # pinned crate. Normal Cargo builds fetch these during the build script;
  # Nix must fetch them separately before entering the build sandbox.
  ghosttySource = fetchzip {
    url = "https://tip.files.ghostty.org/${ghosttyRevision}/libghostty-vt-source.tar.gz";
    hash = "sha256-uRtcTrlsOhUl1lBOwn+I2lc+dP9wDs0G8hxqr4T/5Vk=";
  };

  ghosttyDeps = runCommand "rex-attach-ghostty-zig-deps" {
    nativeBuildInputs = [ zig_0_17 ];
    outputHashMode = "recursive";
    outputHash = "sha256-u3EQYE+w857UFrn+LYV760vlvgFmaO82TnGKaOyBkRE=";
  } ''
    export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
    cp -R ${ghosttySource} source
    chmod -R u+w source
    cd source
    # Fetch the library's dependency graph for both OS families. Avoid the
    # terminal application's unrelated GUI, font and theme dependencies.
    # --help evaluates lazy dependencies without compiling the library.
    for target in x86_64-linux-gnu aarch64-macos; do
      zig build --help -Demit-lib-vt -Dtarget="$target" \
        -Doptimize=ReleaseFast -Dsimd=true -Dcpu=baseline \
        -Demit-xcframework=false -Demit-themes=false > /dev/null
    done
    mv "$ZIG_GLOBAL_CACHE_DIR/p" "$out"
  '';

  # Zig 0.17 caches archives, but --system expects unpacked package directories.
  ghosttySystem = runCommand "rex-attach-ghostty-zig-system" { } ''
    mkdir -p "$out"
    for archive in ${ghosttyDeps}/*.tar.gz; do
      tar -xzf "$archive" -C "$out"
    done
  '';
in
assert lib.assertMsg (
  ghosttyMatchesLock
  && lib.hasSuffix ".${builtins.substring 0 7 ghosttyRevision}" ghosttyVersion
) "nix/package.nix: Ghostty version/revision does not match Cargo.lock; update the Ghostty pin and source/dependency hashes together";
rustPlatform.buildRustPackage {
  pname = manifest.package.name;
  inherit (manifest.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config zig_0_17 ];
  dontUseZigConfigure = true;
  dontUseZigBuild = true;
  dontUseZigCheck = true;
  dontUseZigInstall = true;

  GHOSTTY_SOURCE_DIR = ghosttySource;
  GHOSTTY_ZIG_SYSTEM_DIR = ghosttySystem;
  REX_ATTACH_PREVIEW_FONT = "${dejavu_fonts}/share/fonts/truetype/DejaVuSansMono.ttf";
  preBuild = ''
    export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
  '';

  meta = {
    description = "Unix terminal client for existing terminal blocks on a local Rex server";
    mainProgram = "rex-attach";
    platforms = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
  };
}
