# Terminal renderer comparison

Measured on 2026-11-09 on an Intel N100 (four cores), Linux x86_64, with an
optimized Rust release build, Ratatui 0.30.2, and the same optimized Ghostty VT
library for both backends.

Ratatui reduced output volume but increased local rendering latency and CPU use
for both workloads. The existing ANSI renderer remains the default. Select the
alternative with `rex-attach --socket /path/to/rex.sock --renderer ratatui`.

## Workloads and measurements

Both workloads start from a filled, styled 120×40 screen:

- **Sparse:** overwrite one cell at row 20, column 60 each frame. The surrounding
  row has fifteen color runs, so rewriting the row also reproduces those runs.
- **Heavy:** overwrite all 4,800 cells each frame, with fifteen eight-character
  color runs per row. Every character changes between frames.

VT input sequences are generated before timing. Each backend receives identical
input, warms up for 100 frames, renders 10,000 updates without pacing, then renders
300 updates at 60 Hz over five seconds. Three final runs per backend were executed
serially, in order ANSI/Ratatui, Ratatui/ANSI, ANSI/Ratatui. No builds or tests ran
alongside those measurements. Tables show medians of the three run-level values;
the reported p95 is the median of the three run-level p95 values.

**Render latency** measures `Renderer::frame`, from Ghostty render-state extraction
through the completed ANSI output string. **CPU use** is measured benchmark-thread
CPU time (`CLOCK_THREAD_CPUTIME_ID`) divided by elapsed wall time, as a percentage
of one core. It includes VT ingestion, rendering, output disposal, benchmark
instrumentation, and pacing-call overhead; it excludes input generation and setup.

Output is generated in memory and discarded. This isolates renderer cost but does
**not** measure terminal writes, terminal emulator parsing/painting, Rex transport,
the application's 16 ms redraw timer, or input-to-visible-display latency. Fewer
bytes could improve terminal-side performance, which this benchmark cannot quantify.
CPU frequency scaling and normal OS scheduling remain enabled; paced and unpaced
latencies should therefore be compared within their own tables.

## Results at 60 updates per second

| Workload | Backend | Median render | p95 render | CPU, one core | Bytes/frame |
| --- | --- | ---: | ---: | ---: | ---: |
| Sparse | Existing ANSI | 0.084 ms | 0.116 ms | 0.75% | 711 |
| Sparse | Ratatui | 0.108 ms | 0.153 ms | 0.88% | 94 |
| Heavy | Existing ANSI | 1.494 ms | 1.959 ms | 11.38% | 24,312 |
| Heavy | Ratatui | 2.673 ms | 4.181 ms | 18.63% | 23,127 |

For sparse changes, Ratatui's median render latency was 29% higher and CPU use 18%
higher, while output fell 87%. For heavy changes, median render latency was 79%
higher and CPU use 64% higher, while output fell only 5%. Neither renderer exceeded
the frame budget at the measured p95, but these workloads do not establish a worst
case or an end-to-end frame-rate guarantee.

## Unpaced renderer cost

| Workload | Backend | Median render | p95 render | CPU per update, including VT ingestion |
| --- | --- | ---: | ---: | ---: |
| Sparse | Existing ANSI | 10.829 µs | 15.099 µs | 11.632 µs |
| Sparse | Ratatui | 12.869 µs | 15.596 µs | 13.645 µs |
| Heavy | Existing ANSI | 393.256 µs | 473.023 µs | 502.576 µs |
| Heavy | Ratatui | 660.820 µs | 819.463 µs | 789.377 µs |

Both unpaced loops saturate approximately one core; CPU time per update is the
useful comparison in this mode.

## Implementation and reproduction

The baseline ran before replacing the renderer. The original renderer is preserved
in `src/terminal/ansi.rs`. The alternative in `src/terminal/ratatui_backend.rs`
retains Ghostty's VT emulation, dirty-row tracking, and authoritative cell widths.
Ratatui `Buffer::diff_iter` compares cells within each dirty row, and
`CrosstermBackend` emits styles, cursor commands, and changed cells. It uses buffers
directly to avoid terminal queries and competing input readers. Both paths preserve
synchronized output and leave the existing TTY byte-input code in place.

An initial version compared the entire screen each frame. Restricting Ratatui
diffing to dirty rows reduced unpaced sparse median render latency from 61.842 µs
to 12.869 µs. The final comparison above uses this improvement.

Run each backend separately, without concurrent builds or other benchmarks:

```sh
REX_BENCH_BACKEND=legacy cargo test --release renderer_comparison -- --ignored --nocapture --test-threads=1
REX_BENCH_BACKEND=ratatui cargo test --release renderer_comparison -- --ignored --nocapture --test-threads=1
```

Repeat three times, alternating order. `legacy` selects the original ANSI renderer;
`ratatui` selects the alternative. The normal application flag uses `ansi`, rather
than `legacy`, for the original renderer.

For this workspace's offline release build, the existing optimized Ghostty archive
was exposed through a temporary pkg-config file using `-L... -lghostty-vt`. Both
backends linked the same archive. No Ghostty source or dependency version changed.

Raw measurement logs are not included in this repository. The tables above
record the summarized results; use the commands above to collect fresh samples.

## Validation

The test suite includes emulator-based comparisons against the original renderer
for wide glyphs, combining characters, styles, theme changes, cursor state,
padding, and source resizing. See [DESIGN.md](../DESIGN.md#validation) for the
current required tests and all-target Clippy check.
