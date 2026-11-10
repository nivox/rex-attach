use super::*;
use ::ratatui::{
    backend::{Backend, CrosstermBackend},
    buffer::{Buffer, Cell, CellDiffOption},
    layout::Rect,
    style::{Color, Modifier},
};

/// Ghostty owns emulation and dirty tracking; Ratatui owns cell diffing and ANSI drawing.
/// Use buffers directly so drawing never queries or reads the controlling terminal.
pub struct Renderer {
    state: RenderState,
    rows: RowIterator,
    cells: CellIterator,
    buffers: Buffers,
    dimensions: Option<(u16, u16, u16, u16)>,
    colors: Option<(ghostty_vt::style::RgbColor, ghostty_vt::style::RgbColor)>,
    cursor: Option<Option<(u16, u16)>>,
    text: String,
}

impl Renderer {
    pub fn new() -> Result<Self> {
        // These colors reproduce the remote screen, including blank-cell backgrounds.
        // NO_COLOR must not strip terminal state from the mirrored application.
        ::ratatui::crossterm::style::force_color_output(true);
        Ok(Self {
            state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            buffers: Buffers::default(),
            dimensions: None,
            colors: None,
            cursor: None,
            text: String::new(),
        })
    }

    pub fn frame(&mut self, terminal: &Terminal, width: u16, height: u16) -> Result<String> {
        use ghostty_vt::render::Dirty;
        let snapshot = self.state.update(terminal)?;
        let dimensions = (width, height, snapshot.cols()?, snapshot.rows()?);
        let defaults = snapshot.colors()?;
        let rgb = |c: ghostty_vt::style::RgbColor| Color::Rgb(c.r, c.g, c.b);
        let colors = (defaults.foreground, defaults.background);
        let reset = self.dimensions != Some(dimensions) || self.colors != Some(colors);
        let all = reset || snapshot.dirty()? == Dirty::Full;
        if reset {
            let mut blank = Cell::default();
            blank.fg = rgb(defaults.foreground);
            blank.bg = rgb(defaults.background);
            self.buffers.previous = (0..height)
                .map(|y| Buffer::empty(Rect::new(0, y, width, 1)))
                .collect();
            self.buffers.current = (0..height)
                .map(|y| Buffer::filled(Rect::new(0, y, width, 1), blank.clone()))
                .collect();
        }
        let cursor = if snapshot.cursor_visible()? {
            snapshot
                .cursor_viewport()?
                .filter(|p| p.x < width && p.y < height)
                .map(|p| (p.x, p.y))
        } else {
            None
        };
        self.buffers.dirty_rows.clear();
        if reset {
            // Also paint padding outside the source screen when dimensions/theme change.
            self.buffers.dirty_rows.extend(0..usize::from(height));
        }
        let mut rows = self.rows.update(&snapshot)?;
        let mut y = 0;
        while let Some(row) = rows.next() {
            if y >= height {
                break;
            }
            let at_y = y;
            y += 1;
            if !all && !row.dirty()? {
                continue;
            }
            if !reset {
                self.buffers.dirty_rows.push(usize::from(at_y));
            }
            let mut cells = self.cells.update(row)?;
            let mut x = 0;
            while let Some(cell) = cells.next() {
                if x >= width {
                    break;
                }
                let at_x = x;
                x += 1;
                let target = &mut self.buffers.current[usize::from(at_y)][(at_x, at_y)];
                paint_cell(cell, target, &mut self.text, &defaults, width - at_x)?;
            }
        }
        let output = self
            .buffers
            .draw_changes(reset, cursor, self.cursor != Some(cursor))?;
        // Keep the two buffers equal without copying clean rows on sparse updates.
        for &y in &self.buffers.dirty_rows {
            self.buffers.previous[y].clone_from(&self.buffers.current[y]);
        }
        snapshot.clean()?;
        self.dimensions = Some(dimensions);
        self.colors = Some(colors);
        self.cursor = Some(cursor);
        Ok(String::from_utf8(output)?)
    }
}

fn paint_cell(
    cell: &ghostty_vt::render::CellIteration<'_>,
    target: &mut Cell,
    text: &mut String,
    defaults: &ghostty_vt::render::Colors,
    visible_width: u16,
) -> Result<()> {
    use ghostty_vt::screen::CellWide;
    let rgb = |c: ghostty_vt::style::RgbColor| Color::Rgb(c.r, c.g, c.b);
    let wide = cell.raw_cell()?.wide()?;
    let style = cell.style()?;
    let mut fg = cell.fg_color()?.unwrap_or(defaults.foreground);
    let mut bg = cell.bg_color()?.unwrap_or(defaults.background);
    if style.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    target.reset();
    target.fg = rgb(fg);
    target.bg = rgb(bg);
    if style.bold {
        target.modifier.insert(Modifier::BOLD);
    }
    if style.italic {
        target.modifier.insert(Modifier::ITALIC);
    }
    text.clear();
    if wide != CellWide::SpacerTail {
        cell.graphemes_utf8(text)?;
    }
    text.retain(|c| !c.is_control());
    if wide == CellWide::SpacerTail
        || wide == CellWide::SpacerHead
        || (wide == CellWide::Wide && visible_width < 2)
        || text.is_empty()
        || style.invisible
    {
        target.set_symbol(" ");
    } else {
        target.set_symbol(text);
        // Respect Ghostty's width rather than recomputing Unicode width.
        target.set_diff_option(CellDiffOption::ForcedWidth(
            std::num::NonZeroU16::new(if wide == CellWide::Wide { 2 } else { 1 }).unwrap(),
        ));
    }
    Ok(())
}

#[derive(Default)]
struct Buffers {
    previous: Vec<Buffer>,
    current: Vec<Buffer>,
    dirty_rows: Vec<usize>,
}
impl Buffers {
    fn draw_changes(
        &self,
        reset: bool,
        cursor: Option<(u16, u16)>,
        cursor_changed: bool,
    ) -> Result<Vec<u8>> {
        use std::io::Write;
        // Ratatui performs cell diffing within Ghostty's dirty rows, avoiding a
        // second full-screen scan for each keystroke.
        let mut changes = self
            .dirty_rows
            .iter()
            .flat_map(|&y| self.previous[y].diff_iter(&self.current[y]))
            .peekable();
        let changed = changes.peek().is_some();
        let mut output = Vec::new();
        if changed || reset || cursor_changed {
            let mut backend = CrosstermBackend::new(&mut output);
            backend.write_all(b"\x1b[?2026h")?;
            backend.hide_cursor()?;
            if reset {
                backend.clear()?;
            }
            if changed {
                backend.draw(changes)?;
            }
            if let Some(position) = cursor {
                backend.set_cursor_position(position)?;
                backend.show_cursor()?;
            }
            backend.write_all(b"\x1b[?2026l")?;
        }
        Ok(output)
    }
}
