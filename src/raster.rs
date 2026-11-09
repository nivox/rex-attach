//! Rasterize the mirrored terminal grid, without a GUI or a remote resize.
use anyhow::{Context, Result, ensure};
use fontdue::{Font, FontSettings, Metrics};
use ghostty_vt::{
    RenderState, Terminal,
    render::{CellIterator, RowIterator},
    screen::CellWide,
    style::{RgbColor, Underline},
};
use image::{DynamicImage, Rgba, RgbaImage};
use std::{collections::HashMap, path::Path};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Cell {
    text: String,
    fg: [u8; 3],
    bg: [u8; 3],
    wide: bool,
    tail: bool,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grid {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
    cursor: Option<(u16, u16)>,
    cursor_color: [u8; 3],
}
fn rgb(c: RgbColor) -> [u8; 3] {
    [c.r, c.g, c.b]
}
impl Grid {
    pub fn capture(terminal: &Terminal) -> Result<Self> {
        let mut state = RenderState::new()?;
        let snapshot = state.update(terminal)?;
        // Bound pathological terminal sizes before allocating pixels or cells.
        let cols = snapshot.cols()?.min(240);
        let rows = snapshot.rows()?.min(120);
        ensure!(cols > 0 && rows > 0, "empty terminal grid");
        let colors = snapshot.colors()?;
        let mut grid = Self {
            cols,
            rows,
            cells: Vec::with_capacity(usize::from(cols) * usize::from(rows)),
            cursor: None,
            cursor_color: rgb(snapshot.cursor_color()?.unwrap_or(colors.foreground)),
        };
        if snapshot.cursor_visible()? {
            grid.cursor = snapshot.cursor_viewport()?.map(|p| (p.x, p.y));
        }
        let mut row_iterator = RowIterator::new()?;
        let mut cell_iterator = CellIterator::new()?;
        let mut lines = row_iterator.update(&snapshot)?;
        let mut y = 0;
        while let Some(line) = lines.next() {
            if y >= rows {
                break;
            }
            y += 1;
            let mut cells = cell_iterator.update(line)?;
            let mut x = 0;
            while let Some(cell) = cells.next() {
                if x >= cols {
                    break;
                }
                x += 1;
                let style = cell.style()?;
                let wide = cell.raw_cell()?.wide()?;
                let mut fg = rgb(cell.fg_color()?.unwrap_or(colors.foreground));
                let mut bg = rgb(cell.bg_color()?.unwrap_or(colors.background));
                if style.inverse {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if style.faint {
                    for c in &mut fg {
                        *c /= 2;
                    }
                }
                let mut text = String::new();
                if !style.invisible && wide != CellWide::SpacerTail && wide != CellWide::SpacerHead
                {
                    cell.graphemes_utf8(&mut text)?;
                }
                grid.cells.push(Cell {
                    text,
                    fg,
                    bg,
                    wide: wide == CellWide::Wide,
                    tail: wide == CellWide::SpacerTail,
                    bold: style.bold,
                    italic: style.italic,
                    underline: style.underline != Underline::None,
                    strike: style.strikethrough,
                });
            }
        }
        Ok(grid)
    }
}
struct Glyph {
    metrics: Metrics,
    bitmap: Vec<u8>,
}
pub struct Rasterizer {
    fonts: Vec<Font>,
    glyphs: HashMap<char, Glyph>,
    baseline: i32,
    database: fontdb::Database,
    scanned: bool,
}
impl Rasterizer {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut db = fontdb::Database::new();
        let mut fonts = Vec::new();
        if let Some(path) = path {
            fonts.push(
                Font::from_bytes(
                    std::fs::read(path)
                        .with_context(|| format!("reading preview font {}", path.display()))?,
                    FontSettings::default(),
                )
                .map_err(anyhow::Error::msg)?,
            );
        }
        let mut scanned = false;
        if fonts.is_empty() {
            db.load_system_fonts();
            scanned = true;
            let id = db
                .query(&fontdb::Query {
                    families: &[fontdb::Family::Monospace],
                    ..Default::default()
                })
                .or_else(|| db.faces().find(|f| f.monospaced).map(|f| f.id))
                .context("no monospace font found; supply --preview-font /path/to/font.ttf")?;
            fonts.push(
                db.with_face_data(id, |data, index| {
                    Font::from_bytes(
                        data,
                        FontSettings {
                            collection_index: index,
                            ..Default::default()
                        },
                    )
                })
                .context("reading system preview font")?
                .map_err(anyhow::Error::msg)?,
            );
        }
        let baseline = fonts[0]
            .horizontal_line_metrics(14.0)
            .map_or(13, |m| m.ascent.ceil() as i32)
            .min(14);
        Ok(Self {
            fonts,
            glyphs: HashMap::new(),
            baseline,
            database: db,
            scanned,
        })
    }
    fn load_fallback(&mut self, ch: char) {
        // Large CJK fonts can take seconds to parse. Load them only when the
        // actual viewport needs a missing glyph, on the raster worker thread.
        if !self.scanned {
            self.database.load_system_fonts();
            self.scanned = true;
        }
        for face in self.database.faces() {
            let supported = self
                .database
                .with_face_data(face.id, |data, index| {
                    ttf_parser::Face::parse(data, index)
                        .is_ok_and(|face| face.glyph_index(ch).is_some())
                })
                .unwrap_or(false);
            if supported
                && let Some(Ok(font)) = self.database.with_face_data(face.id, |data, index| {
                    Font::from_bytes(
                        data,
                        FontSettings {
                            collection_index: index,
                            ..Default::default()
                        },
                    )
                })
            {
                self.fonts.push(font);
                return;
            }
        }
    }
    fn glyph(&mut self, ch: char) -> &Glyph {
        // Keep the cache bounded even if a program cycles through all Unicode.
        if self.glyphs.len() > 4096 {
            self.glyphs.clear();
        }
        if !self.glyphs.contains_key(&ch)
            && self
                .fonts
                .iter()
                .all(|font| font.lookup_glyph_index(ch) == 0)
        {
            self.load_fallback(ch);
        }
        self.glyphs.entry(ch).or_insert_with(|| {
            let font = self
                .fonts
                .iter()
                .find(|f| f.lookup_glyph_index(ch) != 0)
                .unwrap_or(&self.fonts[0]);
            let (metrics, bitmap) = font.rasterize(ch, 14.0);
            Glyph { metrics, bitmap }
        })
    }
    pub fn render(&mut self, grid: &Grid) -> DynamicImage {
        const CW: u32 = 9;
        const CH: u32 = 18;
        let mut image = RgbaImage::new(u32::from(grid.cols) * CW, u32::from(grid.rows) * CH);
        for (index, cell) in grid.cells.iter().enumerate() {
            let x = index as u32 % u32::from(grid.cols) * CW;
            let y = index as u32 / u32::from(grid.cols) * CH;
            for py in y..y + CH {
                for px in x..x + CW {
                    image.put_pixel(px, py, Rgba([cell.bg[0], cell.bg[1], cell.bg[2], 255]));
                }
            }
        }
        for (index, cell) in grid.cells.iter().enumerate() {
            if cell.tail {
                continue;
            }
            let x = index as u32 % u32::from(grid.cols) * CW;
            let y = index as u32 / u32::from(grid.cols) * CH;
            let cell_width = if cell.wide { 2 * CW } else { CW };
            let baseline = self.baseline;
            for ch in cell
                .text
                .chars()
                .filter(|c| !c.is_control() && !c.is_whitespace())
            {
                let glyph = self.glyph(ch);
                for gy in 0..glyph.metrics.height {
                    for gx in 0..glyph.metrics.width {
                        let alpha = u16::from(glyph.bitmap[gy * glyph.metrics.width + gx]);
                        if alpha == 0 {
                            continue;
                        }
                        let skew = if cell.italic {
                            (glyph.metrics.height - gy) as i32 / 5
                        } else {
                            0
                        };
                        let px = x as i32 + glyph.metrics.xmin + gx as i32 + skew;
                        let py =
                            y as i32 + baseline - glyph.metrics.height as i32 - glyph.metrics.ymin
                                + gy as i32;
                        for bold in 0..=u32::from(cell.bold) {
                            let px = px + bold as i32;
                            if px < x as i32
                                || px >= (x + cell_width).min(image.width()) as i32
                                || py < y as i32
                                || py >= (y + CH) as i32
                            {
                                continue;
                            }
                            let pixel = image.get_pixel_mut(px as u32, py as u32);
                            for channel in 0..3 {
                                pixel[channel] = ((u16::from(cell.fg[channel]) * alpha
                                    + u16::from(pixel[channel]) * (255 - alpha))
                                    / 255) as u8;
                            }
                        }
                    }
                }
            }
            for (enabled, offset) in [(cell.underline, 16), (cell.strike, 9)] {
                if enabled {
                    for px in x..(x + cell_width).min(image.width()) {
                        image.put_pixel(
                            px,
                            y + offset,
                            Rgba([cell.fg[0], cell.fg[1], cell.fg[2], 255]),
                        );
                    }
                }
            }
        }
        if let Some((cx, cy)) = grid.cursor
            && cx < grid.cols
            && cy < grid.rows
        {
            let color = Rgba([
                grid.cursor_color[0],
                grid.cursor_color[1],
                grid.cursor_color[2],
                255,
            ]);
            for dx in 0..CW {
                image.put_pixel(u32::from(cx) * CW + dx, u32::from(cy) * CH + CH - 2, color);
            }
        }
        DynamicImage::ImageRgba8(image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostty_vt::TerminalOptions;
    #[test]
    fn grid_captures_full_viewport_styles_unicode_and_cursor() -> Result<()> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 12,
            rows: 4,
            ..Default::default()
        })?;
        terminal.vt_write("\x1b[1;3;4;38;2;10;20;30;48;2;40;50;60mA界\x1b[0m".as_bytes());
        let grid = Grid::capture(&terminal)?;
        assert_eq!((grid.cols, grid.rows), (12, 4));
        assert_eq!(grid.cells.len(), 48);
        assert_eq!(grid.cells[0].fg, [10, 20, 30]);
        assert_eq!(grid.cells[0].bg, [40, 50, 60]);
        assert!(grid.cells[0].bold && grid.cells[0].italic && grid.cells[0].underline);
        assert_eq!(grid.cells[1].text, "界");
        assert!(grid.cells[1].wide && grid.cells[2].tail);
        assert_eq!(grid.cursor, Some((3, 0)));
        terminal.vt_write(b"\x1b[?25l");
        assert!(Grid::capture(&terminal)?.cursor.is_none());
        Ok(())
    }
    #[test]
    #[ignore = "requires REX_ATTACH_PREVIEW_FONT or an installed monospace font"]
    fn rasterized_preview_contains_background_glyphs_and_cursor() -> Result<()> {
        let font = std::env::var_os("REX_ATTACH_PREVIEW_FONT").map(std::path::PathBuf::from);
        let mut raster = Rasterizer::load(font.as_deref())?;
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 60,
            rows: 12,
            ..Default::default()
        })?;
        terminal.vt_write(b"\x1b[48;2;20;24;32m\x1b[2J\x1b[H\x1b[38;2;80;220;160mrex-attach image preview\r\n\x1b[0;48;2;20;24;32m\r\n");
        terminal.vt_write("Regular text  café e\u{301}\r\n\x1b[1mBold\x1b[0m  \x1b[3mItalic\x1b[0m  \x1b[4mUnderline\x1b[0m\r\n\r\n\x1b[31mRed \x1b[32mGreen \x1b[34mBlue\x1b[0m\r\n\r\n$ ".as_bytes());
        let grid = Grid::capture(&terminal)?;
        let image = raster.render(&grid).to_rgba8();
        assert_eq!(image.dimensions(), (540, 216));
        assert_eq!(image.get_pixel(539, 215).0[3], 255);
        assert!(
            image
                .rows()
                .take(18)
                .flatten()
                .any(|p| p.0[..3] != [20, 24, 32])
        );
        let (x, y) = grid.cursor.unwrap();
        assert_eq!(
            &image.get_pixel(u32::from(x) * 9, u32::from(y) * 18 + 16).0[..3],
            &grid.cursor_color
        );
        if let Some(path) = std::env::var_os("REX_ATTACH_RASTER_TEST_PNG") {
            image.save(path)?;
        }
        Ok(())
    }
}
