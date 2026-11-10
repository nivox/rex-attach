use crate::stream::SnapshotBuffer;
use crate::vt::{
    RenderState, Terminal,
    render::{CellIterator, RowIterator},
};
use anyhow::{Result, ensure};
use serde_json::Value;

pub struct State {
    terminal: Option<Terminal>,
    snapshot: SnapshotBuffer,
    parser: ApcParser,
    held_since: Option<std::time::Instant>,
}
impl State {
    pub fn is_ready(&self) -> bool {
        self.terminal.is_some()
    }
    pub fn terminal(&self) -> Result<&Terminal> {
        self.terminal
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("terminal snapshot is not ready"))
    }
    pub fn terminal_mut(&mut self) -> Result<&mut Terminal> {
        self.terminal
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("terminal snapshot is not ready"))
    }
    pub fn new(nonce: u64) -> Self {
        Self {
            terminal: None,
            snapshot: SnapshotBuffer::default(),
            parser: ApcParser::new(nonce),
            held_since: None,
        }
    }
    pub fn render_held(&mut self, now: std::time::Instant) -> Result<bool> {
        let held = match &self.terminal {
            Some(t) => t.mode(crate::vt::terminal::Mode::SYNC_OUTPUT)?,
            None => false,
        };
        if !held {
            self.held_since = None;
            return Ok(false);
        }
        // Avoid presenting partial application redraws, but don't let a program
        // that forgets to finish a synchronized update freeze the display.
        let since = *self.held_since.get_or_insert(now);
        Ok(now.duration_since(since) < std::time::Duration::from_secs(1))
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        let tail;
        let bytes = if self.terminal.is_none() {
            let Some((terminal, rest)) = self.snapshot.push(bytes)? else {
                return Ok(());
            };
            self.terminal = Some(terminal);
            tail = rest;
            &tail
        } else {
            bytes
        };
        let terminal = self.terminal.as_mut().unwrap();
        for part in self.parser.feed(bytes)? {
            match part {
                Part::Output(bytes) => terminal.vt_write(&bytes),
                Part::Metadata(value) => {
                    if value["type"] == "resize" {
                        let cols = value["cols"]
                            .as_u64()
                            .filter(|n| (1..=65535).contains(n))
                            .ok_or_else(|| anyhow::anyhow!("invalid columns"))?;
                        let rows = value["rows"]
                            .as_u64()
                            .filter(|n| (1..=65535).contains(n))
                            .ok_or_else(|| anyhow::anyhow!("invalid rows"))?;
                        let metric = |name: &str| {
                            value[name].as_u64().filter(|n| *n <= 4096).unwrap_or(0) as u32
                        };
                        terminal.resize(
                            cols as u16,
                            rows as u16,
                            metric("cell_width_px"),
                            metric("cell_height_px"),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Local wheel scrolling never writes to Rex. Semantic mouse events are encoded
/// by Rex against the authoritative remote modes, including legacy byte formats.
pub fn route_input(
    terminal: &mut Terminal,
    events: Vec<crate::shortcuts::Event>,
    size: (u16, u16),
) -> Result<Vec<crate::shortcuts::Event>> {
    use crate::shortcuts::Event;
    use crate::vt::{
        screen::Screen,
        terminal::{Mode, ScrollViewport},
    };
    let mut routed = Vec::new();
    for event in events {
        if let Event::Mouse(mouse) = event {
            if mouse.x >= size.0.min(terminal.cols()?) || mouse.y >= size.1.min(terminal.rows()?) {
                continue;
            }
            if terminal.is_mouse_tracking()? {
                if terminal.is_viewport_active()? {
                    routed.push(Event::Mouse(mouse));
                }
            } else if let Some((_, dy)) = mouse.wheel() {
                if terminal.active_screen()? == Screen::Primary {
                    terminal.scroll_viewport(ScrollViewport::Delta(dy as isize * 3));
                } else if terminal.mode(Mode::ALT_SCROLL)? && dy != 0 {
                    let arrow = match (terminal.mode(Mode::DECCKM)?, dy < 0) {
                        (true, true) => b"\x1bOA",
                        (true, false) => b"\x1bOB",
                        (false, true) => b"\x1b[A",
                        (false, false) => b"\x1b[B",
                    };
                    routed.push(Event::Raw(arrow.to_vec()));
                }
            }
        } else {
            terminal.scroll_viewport(ScrollViewport::Bottom);
            routed.push(event);
        }
    }
    Ok(routed)
}

// Text capture remains stateless; interactive output reuses Renderer below.
pub fn frame(terminal: &mut Terminal, width: u16, height: u16, ansi: bool) -> Result<String> {
    if ansi {
        return Renderer::new(Backend::Ansi)?.frame(terminal, width, height);
    }
    let mut state = RenderState::new()?;
    let snapshot = state.update(terminal)?;
    let mut rows = RowIterator::new()?;
    let mut cells = CellIterator::new()?;
    let mut text = String::new();
    let mut out = String::new();
    let mut rows = rows.update(&snapshot)?;
    let mut y = 0;
    while let Some(row) = rows.next() {
        if y >= height {
            break;
        }
        text_row(row, &mut cells, &mut text, &mut out, width)?;
        out.push('\n');
        y += 1;
    }
    Ok(out)
}

fn text_row(
    row: &crate::vt::render::RowIteration<'_>,
    cells: &mut CellIterator,
    text: &mut String,
    out: &mut String,
    width: u16,
) -> Result<()> {
    let mut cells = cells.update(row)?;
    let mut x = 0;
    while let Some(cell) = cells.next() {
        if x >= width {
            break;
        }
        x += 1;
        if cell.raw_cell()?.wide()? == crate::vt::screen::CellWide::SpacerTail {
            continue;
        }
        text.clear();
        cell.graphemes_utf8(text)?;
        if text.starts_with("\u{10eeee}") {
            text.clear();
        }
        if text.is_empty() || cell.style()?.invisible {
            out.push(' ');
        } else {
            out.extend(text.chars().filter(|c| !c.is_control()));
        }
    }
    Ok(())
}

mod ansi;
mod ratatui_backend;

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum Backend {
    #[default]
    Ansi,
    Ratatui,
}

enum TextRenderer {
    Ansi(ansi::Renderer),
    Ratatui(ratatui_backend::Renderer),
}

pub struct Renderer {
    text: TextRenderer,
    graphics: Option<crate::graphics::output::Output>,
    images: Option<crate::graphics::Images>,
}

impl Renderer {
    pub fn new(backend: Backend) -> Result<Self> {
        let text = match backend {
            Backend::Ansi => TextRenderer::Ansi(ansi::Renderer::new()?),
            Backend::Ratatui => TextRenderer::Ratatui(ratatui_backend::Renderer::new()?),
        };
        Ok(Self {
            text,
            graphics: None,
            images: None,
        })
    }

    pub fn with_graphics(mut self, cell: Option<[u32; 2]>) -> Result<Self> {
        self.graphics = cell.map(crate::graphics::output::Output::new).transpose()?;
        self.images = cell.map(|_| crate::graphics::Images::new()).transpose()?;
        Ok(self)
    }
    pub fn images_pending(&self) -> bool {
        self.graphics.as_ref().is_some_and(|g| g.pending())
    }
    pub fn clear_images(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(graphics) = &mut self.graphics {
            let mut out = std::io::stdout().lock();
            out.write_all(graphics.clear().as_bytes())?;
            out.flush()?;
        }
        Ok(())
    }

    pub fn frame(&mut self, terminal: &Terminal, width: u16, height: u16) -> Result<String> {
        let (mut text, images) = match &mut self.text {
            TextRenderer::Ansi(renderer) => {
                renderer.frame_images(terminal, width, height, self.images.as_mut())?
            }
            TextRenderer::Ratatui(renderer) => {
                renderer.frame_images(terminal, width, height, self.images.as_mut())?
            }
        };
        if let Some(graphics) = &mut self.graphics {
            text.push_str(&graphics.frame(terminal, (width, height), images)?);
        }
        Ok(text)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Output(Vec<u8>),
    Metadata(Value),
}

/// Incremental recognizer for the nonce-qualified Rex metadata marker. A
/// failed marker probe is emitted unchanged; JSON is bounded before decoding.
pub struct ApcParser {
    signature: Vec<u8>,
    probe: Vec<u8>,
    json: Option<Vec<u8>>,
    escaped: bool,
}
impl ApcParser {
    pub fn new(nonce: u64) -> Self {
        Self {
            signature: format!("\x1b_rex1;{nonce};").into_bytes(),
            probe: vec![],
            json: None,
            escaped: false,
        }
    }
    pub fn feed(&mut self, input: &[u8]) -> Result<Vec<Part>> {
        let mut result = vec![];
        let mut plain = vec![];
        for &byte in input {
            if let Some(json) = self.json.as_mut() {
                if self.escaped && byte == b'\\' {
                    json.pop(); // The ESC introducing ST is not part of JSON.
                    let metadata = serde_json::from_slice(json)?;
                    if !plain.is_empty() {
                        result.push(Part::Output(std::mem::take(&mut plain)));
                    }
                    result.push(Part::Metadata(metadata));
                    self.json = None;
                    self.escaped = false;
                } else {
                    json.push(byte);
                    self.escaped = byte == 0x1b;
                    ensure!(
                        json.len() + self.signature.len() + 2 <= 1024 * 1024,
                        "Rex APC exceeds 1 MiB"
                    );
                }
                continue;
            }
            self.probe.push(byte);
            if self.probe == self.signature {
                self.probe.clear();
                self.json = Some(vec![]);
            } else if !self.signature.starts_with(&self.probe) {
                // ESC occurs only at the start of the marker, so at most this
                // final byte can begin another probe after a mismatch.
                if byte == 0x1b {
                    self.probe.pop();
                    plain.append(&mut self.probe);
                    self.probe.push(byte);
                } else {
                    plain.append(&mut self.probe);
                }
            }
        }
        if !plain.is_empty() {
            result.push(Part::Output(plain));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::ratatui_backend::Renderer;
    use super::*;
    use crate::vt::{TerminalOptions, snapshot};

    #[tokio::test]
    async fn embedded_images_and_text_share_dirty_tracking_on_both_backends() -> Result<()> {
        for backend in [Backend::Ansi, Backend::Ratatui] {
            let mut source = crate::graphics::tests::terminal()?;
            let mut display = crate::graphics::tests::terminal()?;
            crate::graphics::tests::transmit(&mut source, 42, [255, 0, 0, 255], "c=2,r=2");
            let mut renderer = super::Renderer::new(backend)?.with_graphics(Some([10, 20]))?;
            for marker in ["BEFORE", "AFTER"] {
                source.vt_write(format!("\x1b[1;5H{marker}").as_bytes());
                for _ in 0..300 {
                    display.vt_write(renderer.frame(&source, 12, 6)?.as_bytes());
                    if !renderer.images_pending() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                }
                assert!(frame(&mut display, 12, 6, false)?.contains(marker));
                assert_eq!(crate::graphics::Reader::new()?.capture(&display)?.len(), 1);
            }
            let clear = renderer.graphics.as_mut().unwrap().clear();
            display.vt_write(clear.as_bytes());
        }
        Ok(())
    }

    #[test]
    fn mouse_routing_respects_tracking_history_and_alternate_scroll() -> Result<()> {
        use crate::{mouse::Mouse, shortcuts::Event};
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 120,
            rows: 4,
            ..Default::default()
        })?;
        for n in 0..20 {
            terminal.vt_write(format!("line {n}\r\n").as_bytes());
        }
        let wheel = || Event::Mouse(Mouse::parse(b"\x1b[<64;110;2M").unwrap());
        assert!(route_input(&mut terminal, vec![wheel()], (120, 4))?.is_empty());
        assert!(!terminal.is_viewport_active()?);
        for backend in [Backend::Ansi, Backend::Ratatui] {
            let mut renderer = super::Renderer::new(backend)?;
            let mut display = Terminal::new(TerminalOptions {
                cols: 120,
                rows: 4,
                ..Default::default()
            })?;
            display.vt_write(renderer.frame(&terminal, 120, 4)?.as_bytes());
            assert_eq!(
                frame(&mut display, 120, 4, false)?,
                frame(&mut terminal, 120, 4, false)?
            );
            terminal.scroll_viewport(crate::vt::terminal::ScrollViewport::Bottom);
            display.vt_write(renderer.frame(&terminal, 120, 4)?.as_bytes());
            assert_eq!(
                frame(&mut display, 120, 4, false)?,
                frame(&mut terminal, 120, 4, false)?
            );
            route_input(&mut terminal, vec![wheel()], (120, 4))?;
        }
        route_input(&mut terminal, vec![Event::Raw(b"a".to_vec())], (120, 4))?;
        assert!(terminal.is_viewport_active()?);
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        assert!(matches!(
            route_input(&mut terminal, vec![wheel()], (120, 4))?.as_slice(),
            [Event::Mouse(_)]
        ));
        assert!(route_input(&mut terminal, vec![wheel()], (80, 4))?.is_empty());
        terminal.vt_write(b"\x1b[?1000l\x1b[?1049h\x1b[?1007l");
        assert!(route_input(&mut terminal, vec![wheel()], (120, 4))?.is_empty());
        terminal.vt_write(b"\x1b[?1007h\x1b[?1h");
        assert_eq!(
            route_input(&mut terminal, vec![wheel()], (120, 4))?,
            vec![Event::Raw(b"\x1bOA".to_vec())]
        );
        Ok(())
    }

    #[test]
    fn incremental_output_matches_full_redraw_through_terminal_emulation() -> Result<()> {
        let options = || TerminalOptions {
            cols: 20,
            rows: 4,
            ..Default::default()
        };
        let mut source = Terminal::new(options())?;
        let mut display = Terminal::new(options())?;
        display.vt_write(b"\x1b[?7l"); // UI disables outer autowrap.
        let mut renderer = Renderer::new()?;
        let updates: &[&[u8]] = &[
            b"hello \x1b[31mred\x1b[0m\r\n",
            "界 e\u{301} text".as_bytes(),
            b"\x1b[3;5H",
            b"\x1b[2;1H\x1b[2K",
            b"\x1b[1;1H\x1b[32mh\x1b[0m",
            b"\x1b[?1049hALT SCREEN",
            b"\x1b[?1049l",
            b"\x1b[4;1Hlast\r\nnext",
            b"\x1b[?25l",
        ];
        for update in updates {
            source.vt_write(update);
            let delta = renderer.frame(&source, 20, 4)?;
            display.vt_write(delta.as_bytes());
            assert_eq!(
                frame(&mut source, 20, 4, true)?,
                frame(&mut display, 20, 4, true)?,
                "update {update:?}"
            );
            assert!(
                renderer.frame(&source, 20, 4)?.is_empty(),
                "unchanged screen should emit no bytes"
            );
        }
        Ok(())
    }

    #[test]
    fn single_cell_updates_only_emit_changed_cell_and_resize_invalidates_cache() -> Result<()> {
        let mut source = Terminal::new(TerminalOptions {
            cols: 120,
            rows: 40,
            ..Default::default()
        })?;
        let mut renderer = Renderer::new()?;
        let first = renderer.frame(&source, 120, 40)?;
        assert!(first.len() < 10_000);
        source.vt_write(b"\x1b[20;60Hx");
        let delta = renderer.frame(&source, 120, 40)?;
        assert!(delta.len() < 256, "{} bytes", delta.len());
        assert!(delta.contains("\x1b[20;60H"));
        assert!(!delta.contains("\x1b[2J"));
        source.vt_write(b"\x1b[10;10H");
        let cursor = renderer.frame(&source, 120, 40)?;
        assert!(cursor.len() < 64);
        assert!(!cursor.contains("38;2"));
        assert!(renderer.frame(&source, 80, 24)?.contains("\x1b[2J"));
        Ok(())
    }

    #[test]
    fn ratatui_output_matches_original_renderer_for_wide_cells_and_theme_changes() -> Result<()> {
        let options = || TerminalOptions {
            cols: 12,
            rows: 3,
            ..Default::default()
        };
        let mut source = Terminal::new(options())?;
        let mut display = Terminal::new(options())?;
        source.vt_write(b"\x1b[?7l");
        display.vt_write(b"\x1b[?7l");
        let mut renderer = Renderer::new()?;
        let updates: &[&[u8]] = &[
            "界😀 e\u{301}".as_bytes(),
            b"\x1b[1;1Habcd", // Replace both halves of wide glyphs.
            "\x1b[1;2H界".as_bytes(),
            "\x1b[2;1H\x1b[8m界\x1b[0m".as_bytes(),
            b"\x1b[2;1H\x1b[1;3;7;31;44mstyle\x1b[0m",
            "\x1b[3;12H界".as_bytes(), // Clipped wide glyph at the right edge.
            b"\x1b]11;rgb:12/34/56\x1b\\",
            b"\x1b]10;rgb:fe/dc/ba\x1b\\",
            b"\x1b[2J\x1b[H\x1b[?25l",
        ];
        for update in updates {
            source.vt_write(update);
            display.vt_write(renderer.frame(&source, 12, 3)?.as_bytes());
            let expected = ansi::Renderer::new()?.frame(&source, 12, 3)?;
            let actual = ansi::Renderer::new()?.frame(&display, 12, 3)?;
            // The outer terminal keeps its own theme. Compare visible cell colors
            // and cursor state, excluding the oracle's initial clear/default SGR.
            assert_eq!(
                expected.split_once("\x1b[1;1H").unwrap().1,
                actual.split_once("\x1b[1;1H").unwrap().1,
                "update {update:?}"
            );
            assert!(renderer.frame(&source, 12, 3)?.is_empty());
        }
        Ok(())
    }

    #[test]
    fn ratatui_clears_padding_after_source_resize_and_background_change() -> Result<()> {
        let options = || TerminalOptions {
            cols: 20,
            rows: 4,
            ..Default::default()
        };
        let mut source = Terminal::new(options())?;
        let mut expected = Terminal::new(options())?;
        let mut actual = Terminal::new(options())?;
        expected.vt_write(b"\x1b[?7l");
        actual.vt_write(b"\x1b[?7l");
        let mut legacy = ansi::Renderer::new()?;
        let mut renderer = Renderer::new()?;
        for (cols, rows, update) in [
            (20, 4, &b"\x1b[4;1Hbottom row"[..]),
            (8, 2, &b"\x1b[2J\x1b[Hsmall"[..]),
            (8, 2, &b"\x1b]11;rgb:12/34/56\x1b\\"[..]),
            (20, 4, &b"\x1b[4;1Hlarge again"[..]),
        ] {
            source.resize(cols, rows, 0, 0)?;
            source.vt_write(update);
            expected.vt_write(legacy.frame(&source, 20, 4)?.as_bytes());
            actual.vt_write(renderer.frame(&source, 20, 4)?.as_bytes());
            assert_eq!(
                ansi::Renderer::new()?.frame(&expected, 20, 4)?,
                ansi::Renderer::new()?.frame(&actual, 20, 4)?,
                "source {cols}x{rows}, update {update:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn synchronized_application_updates_are_held_with_a_timeout() -> Result<()> {
        let mut state = State::new(1);
        let terminal = Terminal::new(TerminalOptions::default())?;
        state.feed(&snapshot::encode(&terminal)?)?;
        let now = std::time::Instant::now();
        state.feed(b"\x1b[?2026hpartial")?;
        assert!(state.render_held(now)?);
        assert!(!state.render_held(now + std::time::Duration::from_secs(1))?);
        state.feed(b" complete\x1b[?2026l")?;
        assert!(!state.render_held(now + std::time::Duration::from_secs(1))?);
        Ok(())
    }

    #[test]
    #[ignore = "manual renderer measurement"]
    fn render_profile() -> Result<()> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 120,
            rows: 40,
            ..Default::default()
        })?;
        for row in 1..=40 {
            terminal.vt_write(format!("\x1b[{row};1H{}", "x".repeat(119)).as_bytes());
        }
        let mut renderer = ansi::Renderer::new()?;
        let first = renderer.frame(&terminal, 120, 40)?.len();
        let start = std::time::Instant::now();
        let mut total = 0;
        for n in 0..100 {
            terminal.vt_write(format!("\x1b[20;60H{}", n % 10).as_bytes());
            total += renderer.frame(&terminal, 120, 40)?.len();
        }
        eprintln!(
            "first={first} bytes; 100 single-cell updates={total} bytes in {:?}",
            start.elapsed()
        );
        Ok(())
    }

    #[test]
    fn complete_decoder_preserves_history_and_parser_continuation() -> Result<()> {
        let mut original = Terminal::new(TerminalOptions {
            cols: 20,
            rows: 3,
            ..Default::default()
        })?;
        original.set_continuation_max_bytes(4096)?;
        for n in 0..100 {
            original.vt_write(format!("line {n}\r\n").as_bytes());
        }
        original.vt_write(b"\x1b[31");
        let mut bytes = snapshot::encode(&original)?.to_vec();
        bytes.extend_from_slice(b"mRED");
        let mut restored = State::new(42);
        restored.feed(&bytes)?;
        original.vt_write(b"mRED");
        let restored = restored.terminal.as_mut().unwrap();
        assert_eq!(
            frame(restored, 20, 3, true)?,
            frame(&mut original, 20, 3, true)?
        );
        // Older rows must survive full decode as well as the visible screen.
        original.scroll_viewport(crate::vt::terminal::ScrollViewport::Top);
        restored.scroll_viewport(crate::vt::terminal::ScrollViewport::Top);
        let history = frame(restored, 20, 3, false)?;
        assert!(history.contains("line 0"));
        assert_eq!(history, frame(&mut original, 20, 3, false)?);
        Ok(())
    }

    #[test]
    fn snapshot_fragmentation_tail_unicode_and_resize() -> Result<()> {
        let mut original = Terminal::new(TerminalOptions {
            cols: 20,
            rows: 3,
            ..Default::default()
        })?;
        original.vt_write("界 café e\u{301}\r\nsecond".as_bytes());
        let encoded = snapshot::encode(&original)?;
        let mut state = State::new(42);
        for &byte in &encoded {
            state.feed(&[byte])?;
        }
        state.feed(b"\r\nlast\x1b_rex1;42;{\"type\":\"resize\",\"cols\":25,\"rows\":4}\x1b\\")?;
        let text = frame(state.terminal.as_mut().unwrap(), 25, 4, false)?;
        assert!(text.contains("界 café e\u{301}"), "{text:?}");
        assert!(text.contains("second"));
        assert!(text.contains("last"));
        Ok(())
    }

    #[test]
    fn snapshot_and_vt_can_share_a_message() -> Result<()> {
        let original = Terminal::new(TerminalOptions {
            cols: 10,
            rows: 2,
            ..Default::default()
        })?;
        let mut bytes = snapshot::encode(&original)?.to_vec();
        bytes.extend_from_slice(b"tail");
        let mut state = State::new(1);
        state.feed(&bytes)?;
        assert!(frame(state.terminal.as_mut().unwrap(), 10, 2, false)?.starts_with("tail"));
        Ok(())
    }

    #[test]
    fn corrupted_snapshot_is_rejected() -> Result<()> {
        let original = Terminal::new(TerminalOptions::default())?;
        let mut bytes = snapshot::encode(&original)?.to_vec();
        bytes[16] ^= 1;
        let mut state = State::new(1);
        assert!(state.feed(&bytes).is_err());
        assert!(state.terminal.is_none());
        Ok(())
    }

    #[test]
    fn metadata_every_split_and_wrong_nonce() -> Result<()> {
        let bytes = b"before\x1b_rex1;42;{\"type\":\"resize\"}\x1b\\after";
        for split in 0..=bytes.len() {
            let mut parser = ApcParser::new(42);
            let mut parts = parser.feed(&bytes[..split])?;
            parts.extend(parser.feed(&bytes[split..])?);
            let output: Vec<u8> = parts
                .iter()
                .filter_map(|p| {
                    if let Part::Output(b) = p {
                        Some(b.as_slice())
                    } else {
                        None
                    }
                })
                .flatten()
                .copied()
                .collect();
            assert_eq!(output, b"beforeafter");
            assert_eq!(
                parts
                    .iter()
                    .filter(|p| matches!(p, Part::Metadata(_)))
                    .count(),
                1
            );
        }
        let mut parser = ApcParser::new(99);
        assert_eq!(parser.feed(bytes)?, vec![Part::Output(bytes.to_vec())]);
        Ok(())
    }
}

#[cfg(test)]
mod benchmark;
