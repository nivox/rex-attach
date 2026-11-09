Ghostty bindings are registry dependencies, not vendored source:

- `ghostty-vt` 0.8.0+ghostty.1.4.0-main.83fb956 (Apache-2.0).
- `ghostty-vt-sys` at the matching version (see Cargo.lock).

The native binding's GHOSTTY.lock pins its Ghostty source commit and checksum.
Builds require Zig 0.17.x unless a compatible static library is supplied as
libghostty-vt-static via pkg-config. The direct ghostty-vt-sys dependency enables
that pkg-config build option; application code uses the safe ghostty-vt API.

The snapshot boundary scanner and APC parser in src/stream.rs and
src/terminal.rs are adapted from rex-rune's src/wire.rs and src/terminal.rs.
Their MIT license and attribution are retained in LICENSES/REX-RUNE-LICENSE.

The prototype uses the published complete snapshot decoder. It does not need
rex-rune's incremental next_record, image iterator, or graphics patches.

Browser and preview dependencies also come from crates.io (versions/checksums
are recorded in Cargo.lock): Ratatui and ratatui-image supply the widgets and
Kitty image protocol, fontdue rasterizes glyphs, fontdb discovers installed
fonts, ttf-parser checks fallback coverage, and image resizes pixel buffers.
No font data is vendored. The Nix dev shell points at the packaged DejaVu font;
standalone users can use installed fonts or an explicit font path.
