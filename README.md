# rex-attach

Minimal Unix terminal client for one existing terminal block on a local Rex server.

```sh
nix develop
cargo run -- --socket /path/to/rex.sock
```

With no session or block arguments, a column browser shows **Sessions → Tabs →
Blocks**, followed by a live preview. The browser uses Ratatui and its Crossterm
backend for layout, widgets, styling, scrolling, and incremental rendering.
Tabs correspond to Rex windows and
include their tiled and floating blocks. The blocks column is hidden when a tab
has exactly one block. Detached blocks appear under a separate entry in the tabs
column. The footer shows the selected block's short ID, flavor, and ownership.

The right column is split into equal vertical halves. The upper half shows the
selected block's label, type/flavor, terminal title, working directory,
foreground process/PID, and program-reported status with progress and messages.
The lower half contains the text or Kitty preview. Directory uses the terminal's
reported location, falling back to process directories when none was reported.
Unavailable fields and metadata errors are shown explicitly.

Colored indicators appear on blocks and propagate to their tabs and sessions:

| Indicator | Meaning |
|---|---|
| 🙋 | Needs input (permission, question, or authentication) |
| ❌ | Error |
| 🔄 | Working |
| ✅ | Done |
| 💤 | Idle |

The table order is the aggregation priority: each parent displays the highest
reported state among its descendants. Multiple status records within a block
use the same priority, with their messages shown in the information pane.
Unknown or unreported status has no badge or information-pane
status row. Known states also retain color styling.
Programs must report activity using OSC 7501; process existence or output alone
does not imply working or idle. Clearing reports removes stale indicators and
messages. Metadata/status inspection never takes ownership or sends input.

Status snapshots are read for all terminal blocks, followed by live change and
removal events; a five-second resync also recovers missed events. Title,
directory, and process queries run only for the selected block, once per second.
The inspector runs asynchronously, coalesces UI updates to at most 10 fps, and
releases its session subscriptions when attaching or exiting the browser.

Use Up/Down or j/k to select entries, Left/Right or h/l to move between columns.
Enter opens the next column, or attaches when the selection is a block or a
single-block tab. Escape or Ctrl-C exits the browser. Empty sessions and
nonterminal blocks remain visible but cannot be attached. Metadata refreshes
every five seconds; press Ctrl-R to refresh immediately.

To show only one session's blocks, or attach directly:

```sh
rex-attach --socket /path/to/rex.sock --session work
rex-attach --socket /path/to/rex.sock --session work --block shell
```

`Ctrl-B d` returns to the browser with the selected tab
and block preserved. If
`--session` was supplied, the browser stays scoped to that session and starts in
the tabs column. Direct attachment also returns to this browser on detach.

Browser management shortcuts act on the focused column:

| Key | Action |
| --- | --- |
| `c` | Create a session, tab, or block; enter its label, or leave blank for the default |
| `r` | Rename the selected entry; `Ctrl-U` clears the current label |
| `x` | Confirm closing the selected entry and its contents; `y` closes, `n`/Enter/Esc cancels |
| `Ctrl-R` | Refresh metadata |
| `?` | Show configured browser and terminal shortcuts |
| `q` / Esc / Ctrl-C | Exit the browser |

A new session includes one tab and shell. A new tab includes one shell. A new
block splits the selected block side by side. Single-block tabs skip the blocks
column, so `c` there creates another tab; use an attached split shortcut to add
a block. When scoped with `--session`, creating another session leaves the browser
in its original scope and reports the new session ID.

While attached, press `Ctrl-B`, release it, then the second chord:

| Shortcut | Action | Name |
| --- | --- | --- |
| `Ctrl-B > Ctrl-Shift-'` | Split above/below, attach the new lower block | `pane.split.down` |
| `Ctrl-B > Ctrl-Shift-5` | Split side by side, attach the new right block | `pane.split.right` |
| `Ctrl-B > o` | Attach the next terminal block in this tab, wrapping | `pane.focus_next` |
| `Ctrl-B > c` | Create and attach a new tab | `client.tab.new` |
| `Ctrl-B > n` / `p` | Attach the next/previous tab, wrapping | `client.tab.next` / `client.tab.previous` |
| `Ctrl-B > d` | Return to the browser | `client.detach` |
| `Ctrl-B > Ctrl-B` | Send a literal Ctrl-B | `client.send_prefix` |
| `Ctrl-B > ?` | Show browser and terminal shortcuts | `client.shortcuts` |

Help lists browser and terminal bindings in separate sections, including overrides
and excluding disabled keys. Scroll with arrows or j/k, PgUp/PgDn, or g/G. Esc, q, Enter, or ? closes
help and restores the previous view. Management shortcuts and terminal input
are suspended while help is open; live terminal output continues to update.
`client.shortcuts` can be rebound in either configuration section.

Modified punctuation needs a terminal that reports it through CSI-u or xterm
modifyOtherKeys. The program recognizes those encodings but does not enable an
extended keyboard mode globally. Portable aliases are `Ctrl-B > "` for
above/below and `Ctrl-B > %` for side by side. Unbound terminal input is forwarded
unchanged; an incomplete shortcut prefix is forwarded after one second. Tab
navigation restores its focused terminal, or the first terminal if its focused
block is another type, and skips tabs without terminals.

Shortcuts load at startup from `$XDG_CONFIG_DIR/rex-attach/config.json`. If that
variable is unset, `$XDG_CONFIG_HOME` is also supported, then `~/.config` is used.
No file is required: built-in defaults apply. See [config.example.json](config.example.json)
for the defaults. File entries override individual defaults; use `null` to disable
a binding. For example:

```json
{
  "shortcuts": {
    "browser": {
      "r": null,
      "z": "client.rename"
    },
    "terminal": {
      "ctrl+b>d": null,
      "ctrl+a>d": "client.detach"
    }
  }
}
```

Use `>` for successive chords, `+` for modifiers (`ctrl`, `alt`, `shift`), named
keys `up/down/left/right/enter/esc/tab/space`, or a literal character. Invalid keys
or unsupported actions produce an error before terminal setup. Action names
follow [Rex's Mac actions](https://www.superlogical.com/rex/docs/reference/mac-actions):
`session.new/rename/close`, `client.tab.new/rename/close`, and
`pane.split`, `pane.split.right`, `pane.split.down`, `pane.rename`, and
`pane.close`. These explicit management actions can
also be bound in the browser. Focus-dependent `client.create` and `client.rename`,
`client.browser.up/down/left/right/choose/exit`, `client.detach`,
and `client.send_prefix` are rex-attach's own actions.
`client.close` follows the focused column.
Actions run locally through server APIs rather than invoking native app actions.
Server windows are modeled internally as `Window`; the visible label “Tabs”
matches Rex.app. Native client windows are not another level in this browser.
The info/preview pane shares the width equally with each visible navigation
column. The footer shows the configured detach and attached-help shortcuts.

Tabs and blocks show custom labels. For default labels, they show the terminal
title (the focused block's title for a tab), falling back to the stored label
when no title is available. Defaults are inferred from the entry's ID-based
label or, for blocks, its flavor's short name or display name; Rex does not expose whether a
label was customized. A custom label identical to a default is indistinguishable.
The information pane and rename dialog always show the stored block label.
Interactive attachment focuses the selected block and activates its tab, so the
tab's title follows that block. Detached blocks have no tab to focus. Captures
and browser previews leave focus unchanged.

The preview is read-only: a separate data connection restores the selected
block's screen. By default, `--preview auto` probes Kitty graphics support and
cell dimensions with a bounded async query. When supported, Ghostty's current
terminal cells, colors, basic styles, and cursor are rasterized to an image and
displayed with `ratatui-image` using Kitty Unicode placements. The whole viewport
is fitted into the preview pane, preserving its aspect ratio. Images update at
up to 2.5 fps, skip identical state, reuse an image ID, and use compression when
the terminal advertises it. Rasterization, glyph fallback, resizing, and encoding
run off the UI thread. Selection changes are debounced.

`--preview text` uses the cropped text preview at up to 10 fps; auto mode also
falls back to text when graphics support or a font is unavailable. `--preview
kitty` forces Kitty output when detection cannot establish support. Images
require the Unicode-placeholder part of the Kitty protocol, including through
any multiplexer. Auto mode falls back to text for known incompatible backends.

Image previews use an installed monospace font. Use `--preview-font /path/font.ttf`
or `REX_ATTACH_PREVIEW_FONT` to select one; the Nix shell supplies DejaVu Sans Mono.
This is a cell-based software rasterizer, not Ghostty's GUI renderer: complex
emoji shaping and embedded terminal images are not reproduced. Large terminal
grids are bounded to 240×120 cells. No display server or GPU is needed.

Preview connections never claim ownership, resize, or send input. They detach
on selection changes or browser exit, and transmitted image data is deleted
when leaving the preview. Metadata browsing briefly attaches to each session
and then detaches.

Session and block arguments accept labels, full IDs, or hexadecimal ID suffixes
(4–32 digits). Blocks are resolved only within the selected session. Ambiguous
matches report candidates instead of selecting one. For example:

```sh
target/debug/rex-attach --socket /run/user/1000/rex/server.sock --session ripple-views --block 68585c
```

The socket must be accessible to your account. The
prototype negotiates Rex API revision 10–11, restores a Ghostty snapshot, and
renders live terminal cells. `Ctrl-B d` returns to the picker;
`Ctrl-B Ctrl-B` sends a literal Ctrl-B. Interactive attachment takes
ownership of the selected block and resizes it to this terminal's full screen,
even when another client is attached. Local window resizes update the block too.
Rex shares one terminal size across clients, so other viewers see this resize.
Detaching releases ownership and the next claimant's requested size takes effect.
The old `--take` flag is accepted for compatibility; it is now the default.
Browser previews and `--capture` never claim ownership or resize blocks.

For a read-only initial screen capture without raw terminal mode:

```sh
cargo run -- --socket /path/to/rex.sock --session SESSION_ID --block BLOCK_ID --capture
```

Build requirements: Rust 1.96+ and Zig 0.17.x (included in the flake). The pinned
native binding downloads checksum-verified Ghostty sources on its first build;
building offline requires providing those sources and their Zig dependencies,
or a compatible static library through pkg-config. Both Rust Ghostty bindings
come from crates.io and are pinned in Cargo.lock; no Ghostty code is vendored.
See THIRD_PARTY.md for dependency and attribution details.

Validation:

```sh
cargo test
cargo clippy -- -D warnings
```

For a live interactive check (requires Python 3 and the Rex CLI):

```sh
python3 scripts/live-smoke.py --socket /path/to/rex.sock
python3 scripts/live-smoke.py --socket /path/to/rex.sock --mode all --exit-key ctrl-c
python3 scripts/live-smoke.py --socket /path/to/rex.sock --mode session --exit-key esc
python3 scripts/live-smoke.py --socket /path/to/rex.sock --mode all --multi-block-tab
python3 scripts/live-smoke.py --socket /path/to/rex.sock --mode session --multi-block-tab --preview-mode auto --kitty-terminal
python3 scripts/activity-smoke.py --socket /path/to/rex.sock
```

Each run creates and cleans up a disposable session and checks selection, large
redraws, input, detach back to the browser, browser exit, restored terminal settings,
and that the block survives detachment. Browser modes also check live preview
updates without changes to the terminal size or block ownership. The split-tab
case verifies session → tab → block navigation and selection restoration.
`--kitty-terminal` simulates a Kitty receiver in the PTY: it answers queries,
decodes transferred RGBA pixels, verifies colors and live updates, and checks
image deletion. This is a protocol integration test, not a real Kitty window.
Use `--preview-font` if the test host has no monospace font. An optional
`--image-out /tmp/preview.png` saves received pixels for inspection.
The activity smoke test uses controlled Python programs in a disposable session
to verify details, status changes/messages/progress, propagation from unselected
blocks, parent priority, clearing reports, and terminal restoration.

This is an approach-validation prototype: incremental row redraws, truecolor output,
basic attributes, no images/mouse/scrollback UI/reconnect. Input uses Rex's
terminal.write method and forwards UTF-8 plus escape sequences from the outer
terminal. Ordinary typing and control keys should work; application cursor/keypad
modes and differing terminal keyboard protocols are not translated yet. Paste
uses the outer terminal's ordinary input behavior. Invalid UTF-8 is rejected.
Do not assume arbitrary terminal compatibility yet.

Snapshot state is authoritative for rendering. Live resize metadata is applied
in stream order. Live theme metadata and process-exit notices are not presented
yet. Terminal queries from the mirrored Ghostty state are not sent back to Rex.
Disconnects exit with an error and restore the local terminal; uncertain input
is never retried. For the initial interactive check, use a disposable block.

The renderer retains Ghostty render state, skips unchanged rows, and emits color
changes once per style run. It batches frames with synchronized output and caps
redraws at 60 fps. The picker also skips unchanged output. Applications using
synchronized output are held until completion, with a one-second timeout.

A local debug-build measurement on a filled 120×40 screen reduced the initial
frame from 158,704 to 6,428 bytes. One hundred single-cell updates fell from
15,870,300 to 19,900 output bytes, and renderer time from about 544 ms to 7 ms.
These are renderer measurements, not end-to-end latency guarantees. Reproduce:

```sh
cargo test render_profile -- --ignored --nocapture
```

Automatic attach sizing was verified with two simultaneous clients: the second
attachment takes ownership and applies its screen size, SIGWINCH updates it, and
detaching restores the first client's ownership and requested dimensions.
Capture and previews remain read-only. Repeat with:

```sh
python3 scripts/ownership-smoke.py --socket /path/to/rex.sock
```

The management/shortcut smoke test creates disposable sessions and exercises
browser/terminal help with configured overrides and live terminal restoration;
browser session/tab/block create, rename, cancelled and confirmed close; both
modified split chords; terminal block/tab cycling and wrapping; config overrides;
and terminal restoration:

```sh
python3 scripts/shortcuts-smoke.py --socket /path/to/rex.sock
```

Current validation: thirty tests cover shortcut overrides, fragmented key
sequences, paste isolation, modal editing and confirmation, window navigation, and hidden unreported status, emoji
badges, the split information/preview
layout, activity priority and propagation, clearing and event/snapshot ordering,
Ratatui rendering, Kitty transfer and
targeted deletion, fragmented capability replies and keyboard preservation,
terminal grid capture, column navigation, single-block tab
shortcuts, tiled/floating/detached grouping, selection preservation, incremental rendering against a second
terminal emulator, output bounds and cache invalidation, synchronized updates,
picker metadata and scope, split arrow
sequences and Escape, independent nonblocking terminal input
with output backpressure, target resolution, snapshot/metadata fragmentation, snapshot
corruption, Unicode and resize, history and parser continuation, and an in-memory WebSocket flow through attach,
ticket, snapshot plus live output, input RPC, and detach. Build and Clippy were
checked using the matching native library cached by rex-rune through pkg-config.
Live read-only capture was verified with a session label and short block ID.
Live interactive smoke tests also passed for direct startup, all-session picking,
and scoped browsing, including single-block and split tabs, live previews,
unchanged preview size/ownership, output backpressure, typed input, detach back
to the selected tab/block, Escape/Ctrl-C exit, and restoration of terminal settings.
Nix shell activation is denied in the default sandbox, so
the cached dev-shell toolchain was used; the updated flake needs validation
outside the sandbox.

The font-dependent raster test was also run explicitly. To repeat it:

```sh
REX_ATTACH_PREVIEW_FONT=/path/font.ttf cargo test rasterized_preview -- --ignored
```
