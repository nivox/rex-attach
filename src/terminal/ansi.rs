// Original row-based ANSI renderer; also used as the benchmark baseline.
use super::*;

pub struct Renderer {
    state: RenderState,
    rows: RowIterator,
    painter: RowPainter,
    lines: Vec<String>,
    dimensions: Option<(u16, u16, u16, u16)>,
    background: Option<crate::vt::style::RgbColor>,
    cursor: Option<Option<(u16, u16)>>,
}

impl Renderer {
    pub fn new() -> Result<Self> {
        Ok(Self {
            state: RenderState::new()?,
            rows: RowIterator::new()?,
            painter: RowPainter::new()?,
            lines: Vec::new(),
            dimensions: None,
            background: None,
            cursor: None,
        })
    }

    #[cfg(test)]
    pub fn frame(&mut self, terminal: &Terminal, width: u16, height: u16) -> Result<String> {
        Ok(self.frame_images(terminal, width, height, None)?.0)
    }
    pub(super) fn frame_images(
        &mut self,
        terminal: &Terminal,
        width: u16,
        height: u16,
        images: Option<&mut crate::graphics::Images>,
    ) -> Result<(String, Vec<crate::graphics::Image>)> {
        use crate::vt::render::Dirty;
        use std::fmt::Write;
        let snapshot = self.state.update(terminal)?;
        let images = images
            .map(|cache| cache.capture(terminal, &snapshot))
            .transpose()?
            .unwrap_or_default();
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
            self.painter.paint(row, &defaults, width)?;
            if reset || self.lines[index] != self.painter.line.scratch {
                write!(body, "\x1b[{};1H{}", index + 1, self.painter.line.scratch)?;
                std::mem::swap(&mut self.lines[index], &mut self.painter.line.scratch);
            }
        }
        snapshot.clean()?;
        self.dimensions = Some(dimensions);
        self.background = Some(defaults.background);
        if body.is_empty() && self.cursor == Some(cursor) {
            return Ok((String::new(), images));
        }
        let out = finish_frame(&body, cursor)?;
        self.cursor = Some(cursor);
        Ok((out, images))
    }
}

struct RowPainter {
    cells: CellIterator,
    line: Line,
}
impl RowPainter {
    fn new() -> Result<Self> {
        Ok(Self {
            cells: CellIterator::new()?,
            line: Line::default(),
        })
    }
    fn paint(
        &mut self,
        row: &crate::vt::render::RowIteration<'_>,
        defaults: &crate::vt::render::Colors,
        width: u16,
    ) -> Result<()> {
        self.line.scratch.clear();
        self.line.active_style = None;
        let mut cells = self.cells.update(row)?;
        let mut x = 0;
        while let Some(cell) = cells.next() {
            if x >= width {
                break;
            }
            let at = x;
            x += 1;
            self.line.paint(cell, defaults, width - at)?;
        }
        Ok(())
    }
}
fn finish_frame(body: &str, cursor: Option<(u16, u16)>) -> Result<String> {
    use std::fmt::Write;
    let mut out = String::with_capacity(body.len() + 64);
    // Synchronized output is ignored by terminals that do not support it.
    // On supporting terminals it presents each batch atomically.
    out.push_str("\x1b[?2026h\x1b[?25l");
    out.push_str(body);
    out.push_str("\x1b[0m");
    if let Some((x, y)) = cursor {
        write!(out, "\x1b[{};{}H\x1b[?25h", y + 1, x + 1)?;
    }
    out.push_str("\x1b[?2026l");
    Ok(out)
}

type StyleKey = (
    crate::vt::style::RgbColor,
    crate::vt::style::RgbColor,
    bool,
    bool,
);
#[derive(Default)]
struct Line {
    scratch: String,
    text: String,
    active_style: Option<StyleKey>,
}
impl Line {
    fn paint(
        &mut self,
        cell: &crate::vt::render::CellIteration<'_>,
        defaults: &crate::vt::render::Colors,
        visible_width: u16,
    ) -> Result<()> {
        use crate::vt::screen::CellWide;
        use std::fmt::Write;
        let wide = cell.raw_cell()?.wide()?;
        if wide == CellWide::SpacerTail {
            return Ok(());
        }
        let style = cell.style()?;
        let mut fg = cell.fg_color()?.unwrap_or(defaults.foreground);
        let mut bg = cell.bg_color()?.unwrap_or(defaults.background);
        if style.inverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        let key = (fg, bg, style.bold, style.italic);
        if self.active_style != Some(key) {
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
            self.active_style = Some(key);
        }
        self.text.clear();
        cell.graphemes_utf8(&mut self.text)?;
        if self.text.starts_with("\u{10eeee}") {
            self.text.clear();
        }
        if wide == CellWide::SpacerHead || (wide == CellWide::Wide && visible_width < 2) {
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
        Ok(())
    }
}
