# rex-attach

A Unix terminal client for existing terminal blocks on a local Rex server.
Browse sessions, tabs, and blocks with a live preview, or attach directly to a
block and use it from your terminal. This also works over SSH when run on the
machine hosting Rex.

## Build

With [Nix](https://nixos.org/):

```sh
nix develop
cargo build --release --locked
```

The binary is `target/release/rex-attach`; put it on your `PATH` for the examples
below. Without Nix, install Rust 1.96+, a C toolchain, pkg-config, and Zig 0.17.x,
then run the same Cargo command. The first native build downloads pinned Ghostty
sources and dependencies, so it needs network access. See
[THIRD_PARTY.md](THIRD_PARTY.md) for native dependency details.

## Use

Start a Rex server, then connect using automatic socket discovery:

```sh
rex-attach
```

To choose a particular server, supply a socket accessible to your account:

```sh
rex-attach --socket /path/to/rex.sock
```

The browser shows **Sessions → Tabs → Blocks**. Use arrows or `hjkl` to navigate
and Enter to select or attach. Tabs with one block skip the blocks column.
Press `?` for shortcuts, `p` to toggle information and preview, or `q`/Esc to exit.
Use `c`, `r`, and `x` to create, rename, or close the focused entry; closing
requires confirmation.

To browse one session or attach directly:

```sh
rex-attach --socket /path/to/rex.sock --session work
rex-attach --socket /path/to/rex.sock --session work --block shell
```

Targets accept labels, full IDs, or hexadecimal ID suffixes (4–32 digits).
Blocks are resolved within the selected session; ambiguous matches report
candidates.

While attached, press **Ctrl-B**, release it, then:

| Key | Action |
| --- | --- |
| `d` | Detach to the browser, leaving the block running |
| `Ctrl-B` | Send a literal Ctrl-B |
| `?` | Show configured shortcuts |
| `"` / `%` | Split below / right and attach the new block |
| `o` | Cycle blocks in the tab |
| `c` | Create a tab |
| `n` / `p` | Next / previous tab |

**Attachment takes ownership and resizes the block to your terminal.** Other
clients see that shared size. Browser previews and text captures are read-only;
they leave ownership, size, and focus unchanged. To print the initial screen:

```sh
rex-attach --socket /path/to/rex.sock --session work --block shell --capture
```

Mouse clicks select browser rows; double-click chooses a row, and the wheel
navigates the column under the pointer or scrolls shortcut help. Attached programs
receive mouse input when they request it. Otherwise, the wheel scrolls local
terminal history; typing returns to the live viewport. Alternate-screen programs
receive arrow-key scrolling only when they enable alternate-scroll mode. Use your
outer terminal's mouse-selection override (commonly Shift) for text selection.

This is an early client: scrollback search, embedded terminal images,
and automatic reconnect are unavailable. Keyboard protocols are forwarded rather
than translated. Interactive mode requires a terminal (`TERM=dumb` is unsupported).
Run `rex-attach --help` for all options.

## Configuration

No config file is required. At startup, the client looks for
`rex-attach/config.json` under `$XDG_CONFIG_DIR`, then `$XDG_CONFIG_HOME`, then
`~/.config`, using the first nonempty base directory.

### Server socket

The server is selected in this order:

1. `--socket PATH` uses that Unix socket directly.
2. A nonempty `REX_SERVER` uses an absolute socket path or a `unix:///path`
   address. Percent escapes in Unix addresses are decoded. Rex sets this variable
   inside its terminals. Host labels and network addresses are unsupported;
   override them with `--socket PATH`.
3. Otherwise, the client probes these paths, skipping duplicate paths and symlink
   aliases:

   | Source | Socket path |
   | --- | --- |
   | Nonempty `REX_RUNTIME_DIR` | `$REX_RUNTIME_DIR/server.sock` |
   | Nonempty `XDG_RUNTIME_DIR` | `$XDG_RUNTIME_DIR/rex/server.sock` |
   | Linux user runtime fallback | `/run/user/<effective-uid>/rex/server.sock` |
   | State fallback | `$XDG_STATE_HOME/rex/run/server.sock`, or `~/.local/state/rex/run/server.sock` when `XDG_STATE_HOME` is unset or empty |
   | Shared server | `/run/rex/server.sock` |

Automatic discovery checks every candidate with a Rex control handshake, allowing
up to two seconds per candidate. Exactly one server must respond successfully;
if none or several do, the error asks you to specify `--socket PATH`. Failed
explicit selections (`--socket` or `REX_SERVER`) report an error without falling
back to another server. Discovery does not attach to sessions, claim ownership,
or resize blocks.

The same paths apply on NixOS: the Rex executable may live in the Nix store, but
personal sockets normally live under `/run/user/<uid>/rex`, and the shared service
defaults to `/run/rex/server.sock`. Custom service socket paths need `--socket`
or `REX_SERVER`. The Rex CLI is not required or invoked, and rex-attach does not
launch a server itself. Connecting to a socket managed by systemd can activate
its service.

### Shortcuts and previews

[config.example.json](config.example.json) lists the defaults. Override individual
keys in `shortcuts.browser` or `shortcuts.terminal`; use `null` to disable a
binding. `>` separates successive chords and `+` joins modifiers. For example:

```json
{
  "preview_layout": "row",
  "shortcuts": {
    "terminal": {
      "ctrl+b>d": null,
      "ctrl+a>d": "client.detach"
    }
  }
}
```

Previews default to Kitty graphics when supported, with a text fallback. Use
`--preview text` for text, or `--preview kitty` to force graphics; image previews
require Kitty Unicode placements and an installed monospace font. Select a font
with `--preview-font /path/font.ttf` or `REX_ATTACH_PREVIEW_FONT`.
The default `row` layout places information and preview below navigation.
Set `preview_layout` to `column` for a pane on the right.
`--preview-layout row|column` overrides the configured layout.

For implementation and validation details, see [DESIGN.md](DESIGN.md).
