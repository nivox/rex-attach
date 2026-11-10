//! Run in release mode, alone: cargo test --release renderer_comparison -- --ignored --nocapture --test-threads=1
use super::ratatui_backend::Renderer;
use super::*;
use ghostty_vt::TerminalOptions;
use std::hint::black_box;
use std::time::{Duration, Instant};

fn cpu_seconds() -> f64 {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime initializes the valid output pointer on success.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) },
        0
    );
    let time = unsafe { time.assume_init() };
    time.tv_sec as f64 + time.tv_nsec as f64 * 1e-9
}

fn update(heavy: bool, n: usize) -> Vec<u8> {
    use std::fmt::Write;
    let mut output = String::new();
    if heavy {
        for row in 1..=40 {
            write!(output, "\x1b[{row};1H").unwrap();
            for run in 0..15 {
                write!(
                    output,
                    "\x1b[{}m{}",
                    31 + run % 6,
                    char::from(b'a' + ((n + row + run) % 26) as u8)
                        .to_string()
                        .repeat(8)
                )
                .unwrap();
            }
        }
    } else {
        write!(output, "\x1b[20;60H\x1b[0m{}", n % 10).unwrap();
    }
    output.into_bytes()
}

#[test]
#[ignore = "manual release-mode CPU and latency benchmark (includes 10 seconds of paced updates)"]
fn renderer_comparison() -> Result<()> {
    let backend = std::env::var("REX_BENCH_BACKEND").unwrap_or_else(|_| "ratatui".into());
    assert!(matches!(backend.as_str(), "legacy" | "ratatui"));
    for heavy in [false, true] {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 120,
            rows: 40,
            ..Default::default()
        })?;
        terminal.vt_write(b"\x1b[?7l");
        terminal.vt_write(&update(true, 0));
        let mut current = Renderer::new()?;
        let mut legacy = super::ansi::Renderer::new()?;
        let mut render = |terminal: &Terminal| {
            if backend == "legacy" {
                legacy.frame(terminal, 120, 40)
            } else {
                current.frame(terminal, 120, 40)
            }
        };
        black_box(render(&terminal)?);
        let updates: Vec<_> = (0..260).map(|n| update(heavy, n)).collect();
        for n in 0..100 {
            terminal.vt_write(&updates[n % updates.len()]);
            black_box(render(&terminal)?);
        }
        for paced in [false, true] {
            let frames = if paced { 300 } else { 10_000 };
            let mut latency = Vec::with_capacity(frames);
            let mut bytes = 0;
            let cpu_start = cpu_seconds();
            let start = Instant::now();
            for n in 0..frames {
                if paced {
                    let deadline = start + Duration::from_secs_f64((n + 1) as f64 / 60.0);
                    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                }
                terminal.vt_write(&updates[n % updates.len()]);
                let render_start = Instant::now();
                let output = black_box(render(&terminal)?);
                latency.push(render_start.elapsed().as_secs_f64() * 1e6);
                bytes += output.len();
                black_box(output);
            }
            let wall = start.elapsed().as_secs_f64();
            let cpu = cpu_seconds() - cpu_start;
            latency.sort_by(f64::total_cmp);
            println!(
                "{}",
                serde_json::json!({
                    "backend": backend, "workload": if heavy { "heavy" } else { "sparse" },
                    "paced": paced, "frames": frames, "wall_seconds": wall,
                    "cpu_seconds": cpu, "cpu_percent_one_core": cpu / wall * 100.0,
                    "cpu_us_per_update": cpu / frames as f64 * 1e6,
                    "render_median_us": latency[frames / 2], "render_p95_us": latency[frames * 95 / 100],
                    "bytes_per_frame": bytes as f64 / frames as f64
                })
            );
        }
    }
    Ok(())
}
