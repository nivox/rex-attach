//! Run in release mode, alone: cargo test --release renderer_comparison -- --ignored --nocapture --test-threads=1
use super::*;
use crate::vt::TerminalOptions;
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
        let images = std::env::var_os("REX_BENCH_IMAGES").is_some();
        let mut renderer = super::Renderer::new(if backend == "legacy" {
            Backend::Ansi
        } else {
            Backend::Ratatui
        })?
        .with_graphics(images.then_some([10, 20]))?;
        let mut render = |terminal: &Terminal| renderer.frame(terminal, 120, 40);
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
                    "backend": backend, "workload": if heavy { "heavy" } else { "sparse" }, "images_enabled":images,
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

#[tokio::test]
#[ignore = "manual serial release-mode graphics CPU, latency and output measurement"]
async fn graphics_comparison() -> Result<()> {
    for backend in [Backend::Ansi, Backend::Ratatui] {
        for replacing in [false, true] {
            let mut source = crate::graphics::tests::terminal()?;
            source.resize(120, 40, 10, 20)?;
            crate::graphics::tests::transmit(&mut source, 42, [255, 0, 0, 255], "c=20,r=10,C=1");
            let mut renderer = super::Renderer::new(backend)?.with_graphics(Some([10, 20]))?;
            while {
                black_box(renderer.frame(&source, 120, 40)?);
                renderer.images_pending()
            } {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let process_cpu = || {
                let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
                // SAFETY: clock_gettime initializes timespec on success.
                assert_eq!(
                    unsafe {
                        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, time.as_mut_ptr())
                    },
                    0
                );
                let time = unsafe { time.assume_init() };
                time.tv_sec as f64 + time.tv_nsec as f64 * 1e-9
            };
            let start_cpu = process_cpu();
            let mut latency = vec![];
            let mut bytes = 0;
            for n in 0..40 {
                let start = Instant::now();
                if replacing {
                    source.vt_write(b"\x1b[H");
                    crate::graphics::tests::transmit(
                        &mut source,
                        42,
                        [n, 255 - n, 0, 255],
                        "c=20,r=10,C=1",
                    );
                }
                source.vt_write(format!("\x1b[30;1H{n:02}").as_bytes());
                loop {
                    bytes += black_box(renderer.frame(&source, 120, 40)?).len();
                    if !renderer.images_pending() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                latency.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            let cpu = process_cpu() - start_cpu;
            latency.sort_by(f64::total_cmp);
            println!(
                "{}",
                serde_json::json!({"backend":format!("{backend:?}"),"workload":if replacing {"image-replacement"}else{"text-with-image"},"updates":40,
                "complete_median_ms":latency[20],"complete_p95_ms":latency[38],"process_cpu_ms_per_update":cpu*1000.0/40.0,"bytes_per_update":bytes as f64/40.0})
            );
            renderer.graphics.as_mut().unwrap().clear();
        }
    }
    Ok(())
}
