# rex-attach design proposal

Research date: 2026-11-09. This is a proposal, not a working prototype.

Implementation update: an initial Rust prototype now exists; see README.md for
its usage, checks, and limitations. It uses libc terminal handling and Rex's
terminal.write method for the first input path. The semantic event adapter,
capability fallbacks, and reconnect behavior below remain proposed work.
Session-label and block-suffix resolution plus read-only snapshot capture have
subsequently been verified against the local server using approved socket access.
An interactive smoke test in a disposable session also verifies rendering under
output backpressure, input, detach, terminal restoration, and block survival.

## Goal and interface

Attach directly to a block on the local Rex server from an interactive terminal:

```sh
rex-attach --socket /path/to/rex.sock --session SESSION --block BLOCK
```

One motivating case is operating a block after logging into a machine over SSH
when a full Rex client is unavailable. SSH is external to rex-attach and is not
part of its interface or connection handling.

The prototype requires an explicit socket and resolves session and block
arguments through `target.resolve`, accepting labels, full IDs, and hexadecimal
ID suffixes. Block resolution is scoped to the resolved session; ambiguous
matches report candidates. Later, add socket discovery compatible with Rex's
environment and platform defaults. Never silently select the focused block.

Session and block arguments are optional in interactive mode. Without them, a
Ratatui column browser lists sessions, tabs (Rex windows), and blocks, with a live
preview at the right; `--session` scopes it to one session and starts in Tabs.
Single-block tabs skip the blocks column. Arrows and hjkl navigate; Enter opens
the next column or attaches to a block/single-block tab. Floating-layer blocks
remain within their tab; detached blocks get a virtual tab.
Both arguments are required for `--capture`. Explicit session and block arguments
start attachment immediately. Detaching returns to the picker in the original
scope, restoring the selected tab/block. Escape and Ctrl-C exit from the browser.
Internally a `Window` corresponds to `session.view.windows`; the browser calls
these Tabs to match Rex.app. Native application windows are client-local UI.
`c`, `r`, and `x` create, rename, and close the focused kind of object. Modal
operations capture IDs, preserve Unicode labels, and require explicit `y` to
confirm close (Enter cancels). Creation starts shell blocks; block creation
splits the selected anchor. The modal lives above the preview image placement.

`shortcuts.browser` and `shortcuts.terminal` in JSON override default key/action
maps independently; null unbinds. The path is XDG_CONFIG_DIR/rex-attach/config.json,
falling back to XDG_CONFIG_HOME and ~/.config. Mac action identifiers are handled
locally through explicit RPCs; focus-dependent browser actions and detach use
custom client.* names. Attached actions create splits/windows or compute the next
terminal from the attached block's window, rather than assuming the server's
active window matches this client. They focus the destination, release the old
attachment, restore a fresh stream, and preserve trailing input bytes. The browser
uses short-lived session attachments around mutations and reports RPC failures.

`client.shortcuts` opens modal help (`?` in the browser, Ctrl-B then ? attached).
Help uses the effective configuration map, omits disabled bindings, and scrolls
without executing underlying actions or forwarding input. Attached help keeps
feeding the terminal state and restores it with a fresh renderer on dismissal.
The browser hides Kitty placeholder cells while help is displayed, retaining
image data so the preview can return without a forced retransmission.

The right column is split vertically in half: block information above and live
preview below. The upper pane displays label, creator/flavor, title, directory,
foreground process/PID, status/progress, and messages. A separate asynchronous
inspector subscribes to sessions without owning blocks, snapshots activity for
all terminal blocks, and follows program-status change/removal events. Periodic
resync recovers missed events; sequence cutoffs prevent buffered older events
or snapshots from reverting newer statuses. Selected-block metadata refreshes
once per second. Background updates are coalesced to 10 fps and subscriptions
are released before attachment or browser exit.

Block badges propagate to tabs and sessions with priority: blocked, error,
working, done, idle, unknown. Multiple records inside a block use the same
priority. Missing reports stay unknown; activity is never inferred from process
existence or terminal output. Status clearing removes indicators and messages.
Use emoji indicators (🙋 input, ❌ error, 🔄 working, ✅ done, 💤 idle). Unknown
or unreported status is retained internally but has no badge or visible status
row, including on parent entries with no reported descendant activity.

It displays names, short IDs, flavor, and ownership, refreshing metadata every
five seconds or on request. The preview follows a separate read-only data stream
with debounced selection changes, and never resizes, claims ownership, or sends
input. The default preview rasterizes Ghostty cell state with a local monospace
font, fits the full viewport into the pane, and displays it through
ratatui-image's Kitty protocol widget. Software rasterization and encoding run
on blocking workers at up to 2.5 fps, with cached glyphs and state deduplication.
Capability detection is bounded and uses the existing async input reader,
preserving typed keys. Text fallback runs at up to 10 fps. Preview images use
one ID per stream, are deleted on exit or selection changes, and have bounded
grid/pixel allocations. The rasterizer omits complex emoji shaping and embedded
graphics. Explicit CLI mode/font overrides support terminals where detection
or system-font discovery is unavailable.
Metadata browsing briefly attaches to sessions and detaches without claiming
blocks. Empty sessions and nonterminal blocks are displayed but not attachable.

Target Linux and macOS hosts and xterm-compatible outer terminals, including
tmux and screen. “Any terminal” means capability-based degradation: basic
colors and keys work without truecolor, Kitty keyboard, or graphics support.
`TERM=dumb` and noninteractive stdin/stdout should produce useful errors.

Use the whole alternate screen for the block, without permanent chrome.
Ctrl-B followed by d detaches; Ctrl-B followed by Ctrl-B forwards the prefix.
Configurable chord sequences buffer
fragmented input, preserve unmatched bytes and bracketed pastes, and time out
a prefix after one second. They recognize CSI-u and xterm modified keys without
changing the remote application's keyboard protocol.
Ctrl-C, Ctrl-D, and Ctrl-Z belong to the remote program. Interactive attachment
takes ownership of its selected block so its dimensions apply immediately. The
old `--take` flag is accepted as a hidden compatibility option. Preview and capture
are read-only and never claim ownership or resize.
Detach leaves the block and its process running and returns to the picker.

## Verified foundations

The public Rex API documents `session.attach`, `data.connect`, `block.event`,
`session.own_block`, and `session.disown_block`. Data tickets are single-use,
expire, and can be bound to `rex.data.terminal.v1`. They open a WebSocket at
`/api/data`. Attachment is to the session even though we display only one block.
Apply the attach snapshot before events above its sequence cutoff.

Local source inspection provides additional implementation evidence:

- `../rex-rune/vendor/rexrs/src/protocol.rs`: control is a WebSocket over the
  Unix stream at `/api/control`, subprotocol `rex.control.v1`; JSON HELLO
  negotiates an API revision range and client identity.
- `../rex-rune/src/wire.rs`: binary data HELLO, WELCOME, resume handshake,
  and Ghostty snapshot boundary handling.
- `../rex-rune/src/terminal.rs`: snapshot restoration followed by VT bytes
  and nonce-qualified Rex APC metadata for resize, theme, and process exit.
- `../rex-rune/vendor/rexrs/src/api/generated/terminal.rs`: terminal resize
  accepts columns and rows; pixel cell dimensions are optional.

These details describe that local implementation, not a promise for every Rex
release. The installed binary is preview.13. Live discovery failed because the
execution sandbox denies Unix socket connections; no live compatibility was
verified and no sessions were created or changed.

## Architecture

```text
local terminal input -> input adapter -> ordered control requests -> Rex block
                                                                    |
local terminal output <- ANSI renderer <- Ghostty state <- data WebSocket
```

Use Rust with Tokio, a Unix-stream WebSocket transport, serde_json, and
crossterm for raw mode, terminal events, and portable output commands.
Use Ratatui for the column browser and keep the direct incremental VT renderer
for the attached block. The outer terminal guard owns terminal setup across
both views; the Ratatui backend never competes for keyboard input.
Evaluate the `rexrs` control SDK and `ghostty-vt` bindings already used by
rex-rune, pinning exact revisions and retaining required licenses if reusing
code. Keep this repository independently buildable, without sibling path deps.
The prototype uses the published `ghostty-vt` 0.8.0 wrapper, pinned in Cargo.lock.
Its complete snapshot decoder supports our buffered restoration without the
local copy's incremental-decoder and graphics patches.

Keep five modules: control (RPC/events), stream (data framing and metadata),
terminal (Ghostty ownership), input (outer events to Rex), and render (cells to
outer terminal). A single state owner applies stream bytes and samples render
state; no Ghostty object is concurrently mutated by networking and drawing.
Bound queues and batch output; never discard stream bytes to catch up.

## Attach and stream lifecycle

1. Connect to the explicit Unix socket and complete the control handshake.
   Check negotiated revision, server limits, and needed methods.
2. Attach to the session; confirm the requested block supplies terminal data.
   Buffer interleaved events and honor the response sequence cutoff.
3. Take ownership of this block only, and submit the
   outer terminal's dimensions with `com.superlogical.terminal.resize`.
   Read-only mode does neither. Ownership governs size, not input authorization.
4. Request a ticket using `data.connect` for the block's terminal data source,
   explicitly binding `rex.data.terminal.v1`. Open the data WebSocket over a
   second connection to the same Unix socket and perform binary HELLO.
5. Decode the initial `GHOSTSNP` snapshot with Ghostty's decoder. Preserve
   trailing bytes after FINISH: a WebSocket message boundary is not a snapshot
   boundary. The first version can wait for FINISH; incremental READY rendering
   is a later optimization for large histories.
6. Parse the following byte stream incrementally. Extract only Rex APC records
   matching the channel nonce from WELCOME; feed remaining VT bytes to Ghostty.
   Apply resize and theme metadata in stream order. Bound snapshot and metadata
   allocations and let Ghostty validate snapshot checksums and record ordering.
7. Render the dirty viewport at a capped rate, respecting synchronized output
   with a timeout. Continue processing bytes even when skipping redraws.

## Rendering, input, and resizing

Ghostty maintains emulation state; it does not draw this TUI. Traverse its
render state and emit cursor positioning, SGR attributes, grapheme text, and
cursor visibility. The prototype retains render state and compares dirty rows,
groups adjacent cells with the same style, and emits only changed rows. It
clears on initial attachment, resize, or a changed default background, and batches
frames with synchronized output at up to 60 fps. Respect
wide-cell continuations and combining characters. Downgrade unsupported colors
and attributes and handle the bottom-right cell without accidental scrolling.
Do not forward the original output stream to stdout: it includes snapshot data
and protocol metadata and could alter the outer terminal's modes.

Decode outer keyboard events and send `block.event` key events with explicit
session/block IDs. This bypasses Rex layout key bindings, appropriate for a
single-block tool. Send paste as a paste event, not as synthetic keystrokes;
Rex handles encoding for the actual PTY. Add mouse/focus events after keyboard
support, translating coordinates to the block viewport. Local scrollback mode
can use Ghostty's history without sending wheel events to the program.

Do not also encode keys into raw PTY writes when sending semantic events.
Determine which side answers terminal queries before enabling any Ghostty
PtyWrite callbacks; duplicate device replies from the mirrored emulator would
be a bug. Never reflect outer terminal query responses into remote input.

On SIGWINCH, coalesce resize requests. Keep Ghostty at the size reported by the
server stream, rather than optimistically reflowing it. Attachment takes ownership
once and submits the outer dimensions; it does not repeatedly steal ownership if
another client subsequently takes it. Rex remembers each client's requested size
and applies the next claimant's size when we detach. The terminal size is shared,
so taking ownership changes the viewport other clients see as well.

## Failure and cleanup

For the first spike, disconnect with a useful error and restore the terminal.
For the complete prototype, retain the data resume token, byte offset, Ghostty
state, and partial metadata parser. Attempt resume; on rejection obtain a new
ticket and replace the terminal with a fresh snapshot. Count all stream bytes,
including snapshot and metadata. Resume control identity separately; reattach
and reconcile if it cannot be resumed. Do not automatically replay input whose
delivery is uncertain across a disconnect.

Use an RAII terminal guard and orderly signal handling to restore raw mode,
alternate screen, cursor, mouse, and keyboard modes. Disown/detach when possible;
never call block.close or session.destroy as cleanup. A child process exiting
can leave its final screen visible; a block being closed ends attachment.

## Prototype sequence and acceptance

1. Headless spike: control attach, ticket handshake, snapshot restore, and print
   a text capture. This establishes the Rex/Ghostty compatibility boundary.
2. Interactive: raw/alternate screen, full viewport redraw, keys, paste, resize,
   ownership handling, detach, and terminal restoration.
3. Robustness: row diffs, reconnect, scrollback, read-only mode, mouse, and
   capability negotiation. Images, clipboard integration, and Rex layouts are
   outside the initial scope.

Test fragmented snapshot/APC boundaries, corrupt snapshots, trailing VT bytes,
Unicode width, ordered input, and rejected resume with fresh restore. Integrate
against a disposable server/session: existing screen, shell, vim/less, alternate
screen, resize, detach preserving the process, connection loss, and block close.
Manually check common outer terminals and tmux. Never use a user's
working block as the write-test fixture.

The current flake supplies Rust and a C toolchain but no Zig. Once a compatible
Ghostty revision is selected, package it reproducibly with the exact Zig version
required by its source, or use a pinned binding build with all inputs fetched by
Nix. Prefer static Ghostty linkage for distribution; users should not need Zig
or Nix merely to run the binary. Validate the complete Rust/native link before
committing to Linux static binaries and macOS release targets.

## Sources

- https://www.superlogical.com/rex/docs/reference/api
- https://www.superlogical.com/rex/docs/automate/server-api
- https://github.com/ghostty-org/ghostty/blob/main/include/ghostty/vt.h
- https://github.com/ghostty-org/ghostty/blob/main/include/ghostty/vt/snapshot.h
- https://github.com/ghostty-org/ghostty/blob/main/PACKAGING-LIBGHOSTTY.md
- Local rex-rune sources listed above (read-only inspection).

Open implementation questions: live revision compatibility; snapshot decoder
compatibility with the installed server; exact key-event schema on that revision;
query-response ownership; login-server authentication for the chosen socket;
and whether upstream Rust dependencies include the local fixes we need. Start
with an already authorized per-account Rex server socket; local attachment
requires permission to access that account's socket.
