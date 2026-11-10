# Design

This document describes the current implementation. User setup and basic usage
live in [README.md](README.md); review guidelines live in
[CODE_QUALITY.md](CODE_QUALITY.md).

## Scope and compatibility

rex-attach mirrors one terminal block from a local Rex server into a Unix TTY.
SSH is external: run the client on the machine with access to the server socket.
The Unix socket is selected by `--socket`, then nonempty `REX_SERVER`, then
automatic probing of standard local paths. There is no remote transport layer.
Interactive mode requires terminal stdin/stdout and rejects `TERM=dumb`.

The control handshake accepts API revisions 10–11. The client uses its own small
JSON RPC implementation, not the Rex SDK, and relies on the terminal methods and
Ghostty snapshot format supported by those server versions. Revision negotiation
does not establish compatibility with every server or outer terminal. There is no
general color, attribute, or keyboard capability translation layer.

Target resolution uses `target.resolve` for labels, full IDs, and hexadecimal
suffixes. Blocks are scoped to the resolved session; ambiguity reports candidates
rather than choosing a block. Explicit session and block arguments attach
immediately; both are required for capture. Otherwise the browser supplies the
target. An optional session scope persists across detach and browser mutations.

## Components and state ownership

The executable runs Tokio's current-thread runtime and a `LocalSet`. Each
terminal state has one owner; network processing and drawing do not concurrently
mutate Ghostty. CPU-heavy image work runs separately on blocking workers.

| Component | Responsibility |
| --- | --- |
| `app.rs` | CLI, browser/attachment loop, interactive input and rendering |
| `discovery.rs` | Explicit socket selection, Unix endpoint decoding, bounded local socket discovery |
| `domain.rs`, `wire.rs` | Shared identities and typed decoding at protocol boundaries |
| `protocol.rs` | Unix WebSocket connections, revision negotiation, ordered RPCs and events |
| `session.rs` | Session attachment/cleanup, terminal stream setup, resize and RPC/output coordination |
| `stream.rs` | Binary data handshake and snapshot boundary detection |
| `terminal.rs`, `terminal/` | Ghostty state, APC parsing, ANSI and Ratatui renderers, renderer benchmarks |
| `tty.rs` | Raw mode, alternate screen, terminal dimensions, nonblocking input |
| `browser/model.rs`, `state.rs`, `dialog.rs` | Browser tree, selection, pure transitions and modal editing |
| `browser/controller.rs`, `data.rs`, `view.rs` | Effects, asynchronous metadata/mutations, Ratatui presentation |
| `action.rs`, `actions.rs`, `shortcuts.rs`, `help.rs` | Validated local actions, RPC implementations, key matching and modal help |
| `activity.rs` | Status subscriptions, metadata inspection and activity aggregation |
| `preview.rs`, `raster.rs` | Read-only preview streams, capability probing and software rasterization |
| `task.rs` | Cancellable local workers that own their resources |
| `text.rs` | Display text sanitization |

Browser state and dialog editing return effects. The controller executes view
changes or hands RPC work to the data worker; transitions contain no protocol or
TTY operations. Selection is restored by identity rather than stale list indices.
Production items precede test-only modules.

Socket discovery runs once before entering terminal mode. Explicit selections
never fall back; `REX_SERVER` accepts absolute paths or percent-encoded Unix
addresses, not host labels or network endpoints. Without an explicit selection,
the client probes the runtime override, XDG runtime directory, Linux effective
UID runtime directory, XDG/home state fallback, and shared server socket (see
[README.md](README.md#server-socket)). Paths and symlink aliases are deduplicated.
Each probe has a two-second deadline covering the WebSocket and Rex handshakes;
cancelled or rejected probes drop their connections. No session RPCs are sent.
Exactly one successful connection is retained for application startup; zero or
multiple successes require `--socket`. The resolved path is reused by subsequent
workers. The Rex CLI is never invoked and no server is launched by the client,
though connecting to a systemd socket may activate its service.

## Connections, ordering and cancellation

Control uses a WebSocket over a Unix stream at `/api/control`, with subprotocol
`rex.control.v1`. Each `Control` has one reader and one outstanding RPC. A call
reads until its matching response; idle event reads happen only outside calls.
Connection setup, control HELLO, and RPC responses have ten-second timeouts.

The activity subscriber opts into buffering events received during RPCs. Its
queue retains at most 1,024 events, each at most 16 KiB; periodic snapshots recover
from dropped events. Response sequence numbers and per-block cutoffs prevent
older buffered status events or snapshots from replacing newer state.

The main attachment, browser data worker, activity inspector, and preview use
separate control connections. The browser data worker permits one operation at a
time, with capacity-one request/result channels. Refresh requests do not accumulate
while busy; a mutation attempted while busy returns its dialog for later editing.
Mutations execute once, followed by a metadata reload.

`session::with_session` attaches, runs an operation, and awaits `session.detach`
on success or operation failure, preserving both operation and cleanup errors.
Cancellation can interrupt an uncertain RPC, so connection-owning workers drop
their dedicated connection instead of reusing it. Browser exit/attachment awaits
worker shutdown. Normal completion detaches explicitly; cancellation relies on
connection closure to release subscriptions and ownership. Input and mutations
are never automatically replayed.

While an attached input/action RPC is pending, `while_streaming` continues to
consume and draw data output. Even an output error does not abandon that RPC:
it waits for completion before reporting the error, preserving control ordering.

## Attachment and stream lifecycle

1. Resolve the target and attach to its session. Interactive attachment focuses
   a placed block, activating its tab; detached blocks have no tab to focus.
   Capture skips focus changes.
2. Request `data.connect` for data reference `terminal`, bound to
   `rex.data.terminal.v1`. Open a separate WebSocket at `/api/data` on the same
   socket and send binary HELLO with the decoded 16-byte ticket. Validate WELCOME
   and its nonzero channel nonce; the data handshake has an eight-second timeout.
3. Buffer the initial `GHOSTSNP` snapshot through its FINISH record, then use the
   published Ghostty decoder to validate and restore it. WebSocket message
   boundaries are independent of snapshot boundaries; trailing bytes are retained
   and parsed immediately. Snapshot buffering is limited to 256 MiB and initial
   restoration has a 30-second timeout.
4. Capture prints the restored viewport as plain text and releases the session.
   Interactive mode calls `session.own_block` with mode `take` after restoration,
   then submits the outer terminal's columns/rows through
   `com.superlogical.terminal.resize`.
5. Feed live VT bytes into Ghostty. The incremental parser extracts only Rex APC
   records matching the channel nonce, preserves partial prefixes across messages,
   and limits a metadata record to 1 MiB. Resize metadata is applied in stream
   order. Live theme metadata and process-exit notices are currently ignored.

The restored snapshot and ordered stream are authoritative. A requested resize
does not optimistically reflow the local emulator; the server's resize record
does. SIGWINCH submits new dimensions. Ownership is taken once per attachment,
without repeatedly reclaiming it from another client. Rex shares the block's size
across viewers and restores the next claimant's requested size on release.
The hidden `--take` option remains a compatibility alias for the default behavior.

Preview and capture never focus, claim ownership, resize, or write input. Cleanup
releases attachment/ownership without closing a user's block or session. Explicit
confirmed management actions are the only close path. There is no data resume or
automatic reconnect: attachment failures become browser notices when possible,
and unrecoverable control/browser errors exit through terminal cleanup.

## Terminal rendering and input

Ghostty supplies emulation state, cells, widths, colors and cursor state. The
client renders that state rather than copying the original stream to stdout,
which would expose snapshot framing and transport metadata to the outer terminal.
Ghostty query replies are not sent back to Rex.

The default ANSI renderer retains Ghostty render state, inspects dirty rows,
compares rendered row strings, and emits changed rows with style runs. The
alternative `--renderer ratatui` retains dirty-row tracking and compares Ratatui
buffers within those rows, using the Crossterm backend to emit changed cells.
Both preserve wide-cell continuations and grapheme text, invalidate caches on
source/output dimensions or default-background changes, and repaint padding.
They batch frames with synchronized output and draw on a 16 ms timer. Remote
synchronized updates hold rendering until completion, with a one-second timeout.
The browser uses Ratatui's incremental drawing and synchronized frame output.

ANSI remains the default because the recorded comparison found lower CPU use and
render latency, despite Ratatui emitting fewer bytes. See the
[renderer comparison](benchmarks/renderer-comparison.md) for measurements,
workloads, limitations and reproduction commands. Renderer changes must preserve
dirty-row processing, cell widths, invalidation and output bounds; compare against
the second-emulator tests and serial release benchmarks.

One outer `tty::Guard` owns termios, raw mode, the alternate screen and autowrap
across browser and attachment. Drop ends synchronized output, resets styles,
restores wrap/cursor/screen and saved termios. Input opens `/dev/tty` independently
with `O_NONBLOCK` and Tokio readiness; it does not set nonblocking mode on the
shared stdin/stdout file description. Ratatui does not run a competing input
reader or terminal setup. Browser/help rendering avoids cursor-position queries.

Keyboard input is a byte stream, not semantic `block.event` keyboard events. Unbound input
and bracketed paste pass through `com.superlogical.terminal.write` as UTF-8;
partial UTF-8 characters wait for completion and invalid UTF-8 fails. There is no
application cursor/keypad or terminal keyboard protocol translation. Ctrl-C,
Ctrl-D and Ctrl-Z typed in raw mode go to the remote program. External SIGINT and
SIGTERM request exit through the normal cleanup path.

Mouse reports are decoded before shortcut matching. The outer terminal uses SGR
cell reports, with press/release, drag, or all-motion tracking according to the
active UI and remote modes. Scoped TTY guards disable mouse reporting on exit;
attached help temporarily uses press/release tracking and restores the remote
tracking level on close. There is still only one TTY input reader.

Application mouse and wheel events use ordered `block.event` RPCs, after flushing
preceding text input. Rex encodes events using its authoritative terminal modes,
so legacy binary reports do not pass through the UTF-8 text writer. Coordinates
outside the displayed remote grid are ignored. Without application tracking,
vertical wheel events scroll Ghostty's local primary-screen history, or generate
cursor keys if the remote alternate screen has alternate-scroll mode enabled.
Keyboard input restores the live viewport. Mouse events received for a screen
are discarded rather than replayed across attachment/navigation transitions.
Successful presses are tracked per attachment; held buttons are released on the
old target before entering help, navigating, detaching, or exiting normally.

Browser clicks use the rectangles and list offsets captured by the last draw;
single clicks select, double clicks choose, and wheel events navigate the pointed
column. Dialogs absorb mouse input; help handles wheel scrolling locally. Mouse
transitions return the same effects as keyboard transitions. The input decoder
accepts SGR and legacy X10 cell reports; extra buttons beyond the standard three
buttons and wheel are ignored. Pixel reports and a local text-selection UI are
not implemented; use the outer terminal's selection override.

Shortcut matching preserves fragmented escapes, bracketed-paste isolation and
unmatched bytes. It buffers chord sequences with a one-second prefix timeout
(and an 80 ms partial-token timeout), recognizing CSI-u and xterm modifyOtherKeys
without globally enabling an extended keyboard mode. Modified punctuation has
portable literal aliases. Detach/navigation/help preserve trailing bytes in the
TTY unread buffer. Terminal navigation computes destinations from this client's
attached block's tab, wraps, skips tabs without terminals, and restores their
focused terminal or first terminal. A switch restores a fresh stream.

Help displays effective configured bindings and suspends actions/input forwarding.
Attached help continues consuming live output and creates a fresh renderer on
close. Browser help hides Kitty placements while retaining image data for reuse.

## Browser model and activity

The browser maps Rex `session.view.windows` to visible tabs; native application
windows add no level. Tiled and floating blocks stay under their tab; detached
blocks form a virtual tab. Single-block tabs skip the blocks column. Empty
sessions and nonterminal blocks remain visible but cannot be attached.
Metadata reloads every five seconds or on explicit refresh. Detach restores the
selected tab/block and original scope; preview visibility persists for the client
lifetime.

Custom labels take precedence over terminal titles. Default labels are inferred
from ID-based names and creator/flavor names because Rex exposes no customized
flag; a custom label identical to a default is indistinguishable. Default tabs
use their focused block's title; default blocks use their terminal title, falling
back to stored labels. Rename dialogs and block details use stored labels.

Create/rename/close dialogs capture target identities and edit Unicode labels.
Blank creation labels request defaults; Ctrl-U clears rename text. Only `y`
confirms closing; Enter cancels. A new session has one tab/shell, a new tab one
shell, and block creation splits the anchor. Single-block tabs create another tab
from the tab column. Scoped session creation reports the new ID without changing
scope. Action names are validated locally and translated into RPCs rather than
invoking native application actions.

The information pane includes label/flavor, title, directory, foreground
process/PID and reported status/progress/messages. Directory prefers terminal pwd,
then foreground/child process cwd. Missing fields and inspection errors remain
visible. In column layout, details occupy one equal-width navigation column,
with information above preview. The default row layout allocates half the body to
navigation and half to side-by-side information/preview. Hiding details reclaims
that space.

The independent inspector subscribes to sessions, snapshots all terminal blocks'
program statuses, follows change/removal events, and resyncs every five seconds.
Selected title/pwd/process queries run once per second; other titles refresh on
initial discovery and five-second resync. Watch updates are coalesced to 10 fps.
Status comes from programs reporting OSC 7501, never inferred from process
existence or output. Priority within blocks and up through tabs/sessions is
needs-input (🙋), error (❌), working (🔄), done (✅), idle (💤), unknown.
Unknown has no visible badge/status row. Clearing reports removes stale messages
and indicators. Inspection never owns blocks or sends input.

## Preview and bounded image work

Each selection uses a separate read-only snapshot/live stream. Selection changes
are debounced for 120 ms. The previous worker is stopped on target/size changes;
images are deleted by their own ID on replacement, hiding or browser exit.

Auto mode probes Kitty support, compression and cell dimensions through the same
async TTY reader, with a 500 ms deadline that preserves unrelated typed keys.
Known incompatible Unicode-placement backends (WezTerm and Konsole) use text.
Text fallback also covers absent graphics or unavailable automatically selected
fonts. Explicit font errors surface; forced Kitty mode can use assumed cell
size when probing fails. The protocol supports tmux passthrough, but images still
require Unicode placements through the entire terminal/multiplexer chain.

Text previews crop to the pane and update at up to 10 fps. Image previews sample
Ghostty cells, colors, basic styles and cursor at up to 2.5 fps, deduplicate equal
grids, and fit the captured viewport into the pane with its aspect ratio intact.
They use `ratatui-image` Kitty Unicode placements, reusing one image ID per stream
and compression when advertised. RGB colors are required for image addressing,
so image rendering preserves colors even under `NO_COLOR`.

The rasterizer uses an explicit CLI font, `REX_ATTACH_PREVIEW_FONT`, or a discovered
monospace font, with glyph fallback. It bounds capture to 240×120 cells, renders
9×18-pixel cells and clears its glyph cache above 4,096 entries. Rasterization,
fallback lookup, resizing and encoding run on blocking workers behind one shared
semaphore permit. A cancelled job retains that permit until it finishes, so rapid
selection changes cannot accumulate image jobs. This is software rendering with
no display server/GPU; complex emoji shaping and embedded images are unavailable.

## Configuration and build decisions

Configuration loads once, before terminal setup. The first nonempty base of
`XDG_CONFIG_DIR`, `XDG_CONFIG_HOME`, or `$HOME/.config` supplies
`rex-attach/config.json`. Missing files use defaults. Browser and terminal maps
merge individual overrides independently; null disables a key. Invalid chords or
unsupported actions fail early. `preview_layout` defaults to row and the CLI
overrides it. [config.example.json](config.example.json) records the default map;
`action.rs` defines accepted actions and their contexts.

The repository builds independently of sibling checkouts. Rust 1.96+ and Zig
0.17.x are required for the default native build; `flake.nix` supplies the Rust/C
and Zig toolchains, pkg-config, Python and DejaVu Sans Mono for previews/tests.
Published `ghostty-vt` and `ghostty-vt-sys` 0.8.0 dependencies are pinned in
Cargo.lock. The native binding downloads checksum-verified Ghostty sources;
offline builds need those sources and Zig dependencies or a compatible static
library through pkg-config. No Ghostty source or fonts are vendored. The complete
snapshot decoder suffices because restoration buffers through FINISH; local
incremental-decoder or graphics patches are unnecessary. Dependency provenance
and adapted-code attribution live in [THIRD_PARTY.md](THIRD_PARTY.md).

## Validation

Run the required checks in the Nix shell:

```sh
nix develop
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked rasterized_preview -- --ignored
```

CI runs these same checks. Tests exercise protocol/stream fragmentation and
corruption, snapshot tails, Unicode and resize, ordered RPC/output handling,
cleanup/cancellation, pure browser/modal transitions, selection restoration,
shortcut/paste handling, status ordering and propagation, renderer comparisons
against another emulator, Kitty transfers/deletion and bounded worker work.
The explicit-font raster test uses the shell's font or `REX_ATTACH_PREVIEW_FONT`.
Renderer release benchmarks are separate from normal tests.

For ownership, protocol ordering, navigation or graphics changes, run relevant
smoke scenarios when a local Rex server and CLI are available. Scripts create and
clean up disposable sessions; never use a working user block as a write fixture.

```sh
python3 scripts/live-smoke.py --socket /path/to/rex.sock
python3 scripts/live-smoke.py --socket /path/to/rex.sock --mode all --multi-block-tab
python3 scripts/live-smoke.py --socket /path/to/rex.sock --mode session --multi-block-tab --preview-mode auto --kitty-terminal
python3 scripts/ownership-smoke.py --socket /path/to/rex.sock
python3 scripts/shortcuts-smoke.py --socket /path/to/rex.sock
python3 scripts/mouse-smoke.py --socket /path/to/rex.sock
python3 scripts/activity-smoke.py --socket /path/to/rex.sock
```

Live smoke covers input/output under backpressure, detach/block survival,
navigation, preview size/ownership invariants, and terminal restoration.
`--kitty-terminal` simulates a receiver in a PTY and checks transferred pixels,
updates and deletion; it is not a real emulator GUI test. `--preview-font` selects
an explicit font and `--image-out /tmp/preview.png` saves decoded pixels. Ownership
smoke exercises simultaneous clients and resize/release; shortcut smoke covers
management, navigation, config overrides and modal help; mouse smoke covers both
renderers, browser activation, SGR/legacy/fragmented reports, held-button cleanup,
help isolation, local history, resize and exit cleanup; activity smoke covers
metadata, status changes/clearing and parent aggregation. These checks provide
bounded evidence, not a guarantee of arbitrary terminal compatibility.

## Unimplemented capabilities

Automatic reconnect/data resume, semantic keyboard/paste events, focus
forwarding, scrollback search, embedded terminal graphics and clipboard
integration remain outside the implementation. There is no guarantee of color
fallback or translation between differing keyboard protocols. Live theme and
process-exit metadata presentation are also unimplemented.
