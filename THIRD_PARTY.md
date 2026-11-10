Ghostty dependencies come from crates.io; no Ghostty source or wrapper is vendored:

- `ghostty-vt` 0.8.0+ghostty.1.4.0-main.83fb956 (Apache-2.0).
- `ghostty-vt-sys` at the matching version (see Cargo.lock).

The native binding's GHOSTTY.lock pins its Ghostty source commit and checksum.
Builds require Zig 0.17.x unless a compatible static library is supplied as
libghostty-vt-static via pkg-config. The direct ghostty-vt-sys dependency enables
that pkg-config build option.

`src/vt.rs` and `src/vt/` are original, application-specific adapters over the
published native C API. They own the terminal/render handles needed by this
client and expose borrowed render cursors and owned image geometry. Public types,
style conversions and PNG callback registration use the registry Rust crate.
The stream envelope scanner, metadata recognizer and image fixtures are also
implemented locally; no sibling checkout supplies source or assets.

Browser and preview dependencies also come from crates.io (versions/checksums
are recorded in Cargo.lock): Ratatui and ratatui-image supply the widgets and
Kitty image protocol, fontdue rasterizes glyphs, fontdb discovers installed
fonts, ttf-parser checks fallback coverage, and image resizes pixel buffers.
No font data is vendored. The Nix dev shell points at the packaged DejaVu font;
standalone users can use installed fonts or an explicit font path.
