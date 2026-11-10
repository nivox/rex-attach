use crate::stream::SnapshotBuffer;
use anyhow::{Result, ensure};
use ghostty_vt::{
    RenderState, Terminal,
    render::{CellIterator, RowIterator},
};
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
            Some(t) => t.mode(ghostty_vt::terminal::Mode::SYNC_OUTPUT)?,
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
                        terminal.resize(cols as u16, rows as u16, 0, 0)?;
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
    use ghostty_vt::{
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
    row: &ghostty_vt::render::RowIteration<'_>,
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
        if cell.raw_cell()?.wide()? == ghostty_vt::screen::CellWide::SpacerTail {
            continue;
        }
        text.clear();
        cell.graphemes_utf8(text)?;
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

pub enum Renderer {
    Ansi(ansi::Renderer),
    Ratatui(ratatui_backend::Renderer),
}

impl Renderer {
    pub fn new(backend: Backend) -> Result<Self> {
        match backend {
            Backend::Ansi => Ok(Self::Ansi(ansi::Renderer::new()?)),
            Backend::Ratatui => Ok(Self::Ratatui(ratatui_backend::Renderer::new()?)),
        }
    }

    pub fn frame(&mut self, terminal: &Terminal, width: u16, height: u16) -> Result<String> {
        match self {
            Self::Ansi(renderer) => renderer.frame(terminal, width, height),
            Self::Ratatui(renderer) => renderer.frame(terminal, width, height),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Output(Vec<u8>),
    Metadata(Value),
}

/// Retains partial escape prefixes across frames and reconnects. Only a Rex
/// APC with this channel's nonce is interpreted as transport metadata.
pub struct ApcParser {
    prefix: Vec<u8>,
    pending: Vec<u8>,
}

impl ApcParser {
    pub fn new(nonce: u64) -> Self {
        Self {
            prefix: format!("\x1b_rex1;{nonce};").into_bytes(),
            pending: Vec::new(),
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Part>> {
        let mut parts = Vec::new();
        let mut output = Vec::new();
        for &byte in bytes {
            self.pending.push(byte);
            loop {
                if self.pending.starts_with(&self.prefix) {
                    ensure!(self.pending.len() <= 1024 * 1024, "Rex APC exceeds 1 MiB");
                    if self.pending.ends_with(b"\x1b\\") {
                        if !output.is_empty() {
                            parts.push(Part::Output(std::mem::take(&mut output)));
                        }
                        let json = &self.pending[self.prefix.len()..self.pending.len() - 2];
                        parts.push(Part::Metadata(serde_json::from_slice(json)?));
                        self.pending.clear();
                    }
                    break;
                }
                if self.prefix.starts_with(&self.pending) {
                    break;
                }
                output.push(self.pending.remove(0));
            }
        }
        if !output.is_empty() {
            parts.push(Part::Output(output));
        }
        Ok(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::ratatui_backend::Renderer;
    use super::*;
    use ghostty_vt::{TerminalOptions, snapshot};

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
            terminal.scroll_viewport(ghostty_vt::terminal::ScrollViewport::Bottom);
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
        original.scroll_viewport(ghostty_vt::terminal::ScrollViewport::Top);
        restored.scroll_viewport(ghostty_vt::terminal::ScrollViewport::Top);
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
        for byte in encoded.as_ref() {
            state.feed(&[*byte])?;
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
