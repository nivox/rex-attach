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
no display server/GPU; complex emoji shaping is unavailable.

## Embedded terminal graphics

Static Kitty images are retained from the initial snapshot and live stream. The
native decoder budget is set before restoration (64 MiB per screen). PNG decoding
uses allocation limits before parsing, converts supported PNG colors to RGBA, and
returns memory through Ghostty's allocator. File, temporary-file and shared-memory
loading remain disabled in the client. The tested Rex server normalizes file
transmissions into inline pixels for its viewers; other servers must do likewise.

The two text backends extract image geometry from their own render snapshot,
before cleaning it. A second render-state update would consume dirty rows and
hide text updates. The resolved native iterator handles ordinary, Unicode and
relative placements. At most 1,024 placements and 64 MiB of owned RGBA pixels are
captured per view. Pixel copies are shared and cached by image ID and generation;
geometry and preview equality also track image-only changes. Placeholder glyphs
are suppressed in text output. The preview rasterizer composites images around
backgrounds, text and cursor, respecting source crops, offsets and clipping.

Attached graphics are independent of preview fonts and enabled by `--images`
(`auto`, `kitty`, `off`). One startup probe supplies both paths and keeps the
existing single TTY input reader. Native attachment placement is disabled under
tmux; the existing Unicode-based browser preview path still supports passthrough.
Unsupported terminals keep text output. `--images kitty` uses 10x20 cells when
probing fails. Interactive resize includes detected cell pixel sizes when known;
stream resize metadata preserves server pixel metrics. Previews never resize Rex.

A shared output layer maps resolved placements to client-owned Kitty IDs for both
renderers. Cropping, resampling, background masks and base64 preparation run on a
blocking worker behind the same single-permit semaphore as previews. Prepared
pixels are capped at 64 MiB per frame. A cancelled worker retains its permit;
obsolete results are discarded. Geometry-only moves reuse uploaded pixel variants.
Uploads are chunked at 4 KiB of base64 and drained at up to 256 KiB per frame plus
small control sequences, while input and RPC ordering remain unchanged. Pending
image work keeps the streaming RPC helper drawing even without more remote output.
No local input/query callbacks are installed; Rex alone answers the program. Old
placements remain visible until replacement uploads finish. All output uses quiet
Kitty commands, and text cursor/style state is preserved around placement output.

Source z-order is mapped to native under-text/above-text order. Below-background
images mask non-default cell backgrounds before transmission because the text
renderers use explicit RGB backgrounds. Deletion and help/navigation/exit cleanup
address only allocated client image IDs, including interrupted uploads. Returning
from help creates a fresh renderer and reuploads the current image state.

Animation frame scheduling, Sixel/iTerm2 input, and native attachment graphics
through multiplexers remain unimplemented. The decoder retains static frames;
animation would additionally require the native animation clock and redraw deadlines.

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
The pinned `ghostty-vt` and `ghostty-vt-sys` 0.8.0 dependencies come from crates.io.
The native binding downloads checksum-verified Ghostty sources; offline builds
need those sources and Zig dependencies or a compatible static library through
pkg-config. No Ghostty wrapper, native source, or fonts are vendored.

### Native adapter and upstream replacement

`vt.rs` and `vt/` provide an original adapter over the published
`ghostty-vt-sys` C bindings. The native libghostty-vt API already supports the
operations we need, but the pinned higher-level `ghostty-vt` Rust crate does not
expose all of them. Its terminal and render handles are private, so extending it
locally would otherwise require a fork or assumptions about private Rust layouts.
The adapter owns terminal/render/snapshot handles and provides lifetime-bound
row/cell cursors and bulk image geometry queries. Native
handles remain on their creating thread. No query/input callbacks are installed:
Rex answers programs, and local emulation cannot accumulate or replay replies.
The complete native snapshot decoder validates the independently framed snapshot
and applies the image budget before restoring pixels. Public Rust types and the
PNG callback hook still come from the registry crate. Dependency provenance is
recorded in [THIRD_PARTY.md](THIRD_PARTY.md).

The gaps that require this adapter are:

| Surface | Required behavior missing or incorrectly represented in the pinned Rust wrapper |
| --- | --- |
| Resolved render images | Iterate ordinary, Unicode-placeholder and relative placements from the same render update as text, including crop, viewport coordinates, pixel offsets, dimensions, layer, z-order and image generation. Storage placement enumeration alone cannot supply equivalent resolved geometry. |
| Snapshot restoration | Set image-storage and continuation limits before decoding; restore the complete snapshot transactionally, including image records and parser continuation, and verify completion and the consumed byte boundary. |
| Graphics media controls | Disable file, temporary-file and shared-memory input with the correct native types. In particular, the temporary-file setting is an optional directory string, which the pinned wrapper models as a boolean. |

Replace the adapter with built-in support in the libghostty-vt Rust crate
(currently packaged as `ghostty-vt`) when a published version satisfies these
conditions:

- Its public APIs cover the gaps above and the terminal, history, mode, resize,
  row/cell and image access used by this client. Ownership and borrowing must
  prevent cursors or image slices from surviving invalidating mutations, and
  handles must retain the native threading constraints. A supported public
  native-handle API can justify a smaller extension while complete wrappers land.
- It preserves one render update for both text and images, dirty-row rendering,
  cell widths, image-generation cache invalidation and bounded image work.
  Snapshot limits apply before allocation, local media remain disabled, and
  query handling can leave Rex as the sole responder without accumulating replies.
- Its native ABI and snapshot format remain compatible with the supported Rex
  protocol revisions. The pinned registry dependency builds independently of
  sibling checkouts, without vendoring or copied source/assets, through the
  supported Zig build or compatible pkg-config library.
- The migration passes the Nix checks below, emulator comparisons and explicit-font
  raster tests. When a local server is available, run disposable graphics,
  mouse, ownership, navigation and preview smoke scenarios with both renderers.
  Compare serial release benchmarks against the adapter for sparse/heavy text,
  static images and image replacement; review median/p95 time, CPU and output
  bytes, and investigate regressions before removing the corresponding adapter.

Reevaluate these conditions when upgrading the crate. Remove adapter pieces as
their upstream replacements pass the same checks, and remove `vt/` entirely once
all required surfaces are covered. Unused features such as animation are not a
prerequisite for replacing the current static-image implementation.

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
Run these commands separately in the Nix shell, without concurrent benchmarks:

```sh
REX_BENCH_BACKEND=legacy REX_BENCH_IMAGES=1 cargo test --release --locked renderer_comparison -- --ignored --nocapture --test-threads=1
REX_BENCH_BACKEND=ratatui REX_BENCH_IMAGES=1 cargo test --release --locked renderer_comparison -- --ignored --nocapture --test-threads=1
cargo test --release --locked graphics_comparison -- --ignored --nocapture --test-threads=1
```

For a text-only baseline, run the first two commands against an exported baseline
checkout without `REX_BENCH_IMAGES`. Compare the same workloads and toolchains,
repeat runs and alternate backend order. The graphics workload measures text
updates beside an unchanged image and image replacement, including worker
preparation and output generation. Output is collected in memory, so it excludes
real terminal upload and rendering costs. Keep dated measurements as local
artifacts and summarize relevant comparisons in the commit or PR description.

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
python3 scripts/graphics-smoke.py --socket /path/to/rex.sock --probe-file-media
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
help isolation, local history, resize and exit cleanup; graphics smoke verifies
embedded previews, snapshot/live pixels, compression, file-media normalization,
scrolling and graphics lifecycle on both renderers; activity smoke covers
metadata, status changes/clearing and parent aggregation. These checks provide
bounded evidence, not a guarantee of arbitrary terminal compatibility.

## Unimplemented capabilities

Automatic reconnect/data resume, semantic keyboard/paste events, focus
forwarding, scrollback search, animated images, non-Kitty image protocols and clipboard
integration remain outside the implementation. There is no guarantee of color
fallback or translation between differing keyboard protocols. Live theme and
process-exit metadata presentation are also unimplemented.
