use crate::stream::SnapshotBuffer;
use anyhow::{Result, ensure};
use ghostty_vt::{
    RenderState, Terminal,
    render::{CellIterator, RowIterator},
};
use serde_json::Value;

pub struct State {
    pub terminal: Option<Terminal>,
    snapshot: SnapshotBuffer,
    parser: ApcParser,
    held_since: Option<std::time::Instant>,
}
impl State {
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

// Text capture remains stateless; interactive output reuses Renderer below.
pub fn frame(terminal: &mut Terminal, width: u16, height: u16, ansi: bool) -> Result<String> {
    if ansi {
        return Renderer::new()?.frame(terminal, width, height);
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
            cell.graphemes_utf8(&mut text)?;
            if text.is_empty() || cell.style()?.invisible {
                out.push(' ');
            } else {
                out.extend(text.chars().filter(|c| !c.is_control()));
            }
        }
        out.push('\n');
        y += 1;
    }
    Ok(out)
}

pub struct Renderer {
    state: RenderState,
    rows: RowIterator,
    cells: CellIterator,
    lines: Vec<String>,
    dimensions: Option<(u16, u16, u16, u16)>,
    background: Option<ghostty_vt::style::RgbColor>,
    cursor: Option<Option<(u16, u16)>>,
    scratch: String,
    text: String,
}

impl Renderer {
    pub fn new() -> Result<Self> {
        Ok(Self {
            state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            lines: Vec::new(),
            dimensions: None,
            background: None,
            cursor: None,
            scratch: String::new(),
            text: String::new(),
        })
    }

    pub fn frame(&mut self, terminal: &Terminal, width: u16, height: u16) -> Result<String> {
        use ghostty_vt::{render::Dirty, screen::CellWide};
        use std::fmt::Write;
        let snapshot = self.state.update(terminal)?;
        let dimensions = (width, height, snapshot.cols()?, snapshot.rows()?);
        let defaults = snapshot.colors()?;
        // A background change must also repaint padding outside the block.
        let reset =
            self.dimensions != Some(dimensions) || self.background != Some(defaults.background);
        let all = reset || snapshot.dirty()? == Dirty::Full;
        let cursor = if snapshot.cursor_visible()? {
            snapshot
                .cursor_viewport()?
                .filter(|p| p.x < width && p.y < height)
                .map(|p| (p.x, p.y))
        } else {
            None
        };
        let mut body = String::new();
        if reset {
            self.lines.clear();
            write!(
                body,
                "\x1b[0;38;2;{};{};{};48;2;{};{};{}m\x1b[2J",
                defaults.foreground.r,
                defaults.foreground.g,
                defaults.foreground.b,
                defaults.background.r,
                defaults.background.g,
                defaults.background.b
            )?;
        }
        self.lines
            .resize_with(usize::from(snapshot.rows()?.min(height)), String::new);
        let mut rows = self.rows.update(&snapshot)?;
        let mut y = 0;
        while let Some(row) = rows.next() {
            if y >= height {
                break;
            }
            let index = usize::from(y);
            y += 1;
            if !all && !row.dirty()? {
                continue;
            }
            self.scratch.clear();
            let mut active_style = None;
            let mut cells = self.cells.update(row)?;
            let mut x = 0;
            while let Some(cell) = cells.next() {
                if x >= width {
                    break;
                }
                let at = x;
                x += 1;
                let wide = cell.raw_cell()?.wide()?;
                if wide == CellWide::SpacerTail {
                    continue;
                }
                let style = cell.style()?;
                let mut fg = cell.fg_color()?.unwrap_or(defaults.foreground);
                let mut bg = cell.bg_color()?.unwrap_or(defaults.background);
                if style.inverse {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let key = (fg, bg, style.bold, style.italic);
                if active_style != Some(key) {
                    write!(
                        self.scratch,
                        "\x1b[0;38;2;{};{};{};48;2;{};{};{}m",
                        fg.r, fg.g, fg.b, bg.r, bg.g, bg.b
                    )?;
                    if style.bold {
                        self.scratch.push_str("\x1b[1m");
                    }
                    if style.italic {
                        self.scratch.push_str("\x1b[3m");
                    }
                    active_style = Some(key);
                }
                self.text.clear();
                cell.graphemes_utf8(&mut self.text)?;
                if wide == CellWide::SpacerHead || (wide == CellWide::Wide && at + 1 >= width) {
                    self.scratch.push(' ');
                } else if self.text.is_empty() || style.invisible {
                    self.scratch.push(' ');
                    if wide == CellWide::Wide {
                        self.scratch.push(' ');
                    }
                } else {
                    self.scratch
                        .extend(self.text.chars().filter(|c| !c.is_control()));
                }
            }
            if reset || self.lines[index] != self.scratch {
                write!(body, "\x1b[{};1H{}", index + 1, self.scratch)?;
                std::mem::swap(&mut self.lines[index], &mut self.scratch);
            }
        }
        snapshot.clean()?;
        self.dimensions = Some(dimensions);
        self.background = Some(defaults.background);
        if body.is_empty() && self.cursor == Some(cursor) {
            return Ok(String::new());
        }
        let mut out = String::with_capacity(body.len() + 64);
        // Synchronized output is ignored by terminals that do not support it.
        // On supporting terminals it presents each batch atomically.
        out.push_str("\x1b[?2026h\x1b[?25l");
        out.push_str(&body);
        out.push_str("\x1b[0m");
        if let Some((x, y)) = cursor {
            write!(out, "\x1b[{};{}H\x1b[?25h", y + 1, x + 1)?;
        }
        out.push_str("\x1b[?2026l");
        self.cursor = Some(cursor);
        Ok(out)
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
    use super::*;
    use ghostty_vt::{TerminalOptions, snapshot};

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
    fn single_cell_updates_only_emit_one_row_and_resize_invalidates_cache() -> Result<()> {
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
        assert!(delta.contains("\x1b[20;1H"));
        assert!(!delta.contains("\x1b[2J"));
        source.vt_write(b"\x1b[10;10H");
        let cursor = renderer.frame(&source, 120, 40)?;
        assert!(cursor.len() < 64);
        assert!(!cursor.contains("38;2"));
        assert!(renderer.frame(&source, 80, 24)?.contains("\x1b[2J"));
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
        let mut renderer = Renderer::new()?;
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
