# Working on rex-attach

Read `CODE_QUALITY.md` when evaluating new changes. In the Nix shell, run
`cargo fmt --all -- --check`, `cargo test --locked`, and
`cargo clippy --locked --all-targets -- -D warnings`. Run the explicit-font raster
test with `cargo test --locked rasterized_preview -- --ignored`.
Keep production items before test-only modules.

Preserve these boundaries:

- Browser state/dialog editing returns effects; the controller and data worker
  execute them. Keep protocol and TTY operations out of pure transitions.
- Each control connection has one reader and ordered RPCs. Cancellation of a
  connection-owning worker drops that connection; never replay uncertain input.
- Preview and capture never claim ownership or resize. Cleanup releases sessions
  and ownership without closing users' blocks.
- Preserve dirty-row rendering, cell widths, cache invalidation, and bounded image
  work. Use the emulator comparisons and release benchmarks for renderer changes.

For changes to ownership, protocol ordering, UI navigation, or graphics, run the
relevant disposable-session smoke scenarios when a local Rex server is available.
