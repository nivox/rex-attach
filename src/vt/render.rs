//! Borrowed access to one render update. Rows and cells cannot survive their
//! cursor, and snapshots prevent both native-state updates and VT writes.
use super::*;
pub use ghostty_vt::render::{Colors, CursorViewport, Dirty};

pub struct RenderState(Owned<ffi::RenderStateImpl>);
pub struct Snapshot<'a> {
    state: &'a mut RenderState,
    terminal: PhantomData<&'a Terminal>,
}
impl RenderState {
    pub fn new() -> Result<Self> {
        Ok(Self(Owned::create(
            |out| {
                // SAFETY: native creation writes one owned handle into out.
                unsafe { ffi::ghostty_render_state_new(std::ptr::null(), out) }
            },
            ffi::ghostty_render_state_free,
        )?))
    }
    pub fn update<'a>(&'a mut self, terminal: &'a Terminal) -> Result<Snapshot<'a>> {
        // SAFETY: both handles are live, and the returned borrow prevents mutation.
        check(unsafe { ffi::ghostty_render_state_update(self.0.raw(), terminal.raw()) })?;
        Ok(Snapshot {
            state: self,
            terminal: PhantomData,
        })
    }
}
impl Snapshot<'_> {
    pub(crate) fn raw(&self) -> ffi::RenderState {
        self.state.0.raw()
    }
    fn scalar<T>(&self, tag: ffi::RenderStateData::Type) -> Result<T> {
        // SAFETY: private callers select only the documented scalar type.
        unsafe { query(|out| ffi::ghostty_render_state_get(self.raw(), tag, out)) }
    }
    pub fn cols(&self) -> Result<u16> {
        self.scalar(ffi::RenderStateData::COLS)
    }
    pub fn rows(&self) -> Result<u16> {
        self.scalar(ffi::RenderStateData::ROWS)
    }
    pub fn dirty(&self) -> Result<Dirty> {
        let value: ffi::RenderStateDirty::Type = self.scalar(ffi::RenderStateData::DIRTY)?;
        value
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid native dirty state {value}"))
    }
    pub fn cursor_visible(&self) -> Result<bool> {
        self.scalar(ffi::RenderStateData::CURSOR_VISIBLE)
    }
    pub fn cursor_color(&self) -> Result<Option<style::RgbColor>> {
        if self.scalar::<bool>(ffi::RenderStateData::COLOR_CURSOR_HAS_VALUE)? {
            Ok(Some(
                self.scalar::<ffi::ColorRgb>(ffi::RenderStateData::COLOR_CURSOR)?
                    .into(),
            ))
        } else {
            Ok(None)
        }
    }
    pub fn cursor_viewport(&self) -> Result<Option<CursorViewport>> {
        if !self.scalar::<bool>(ffi::RenderStateData::CURSOR_VIEWPORT_HAS_VALUE)? {
            return Ok(None);
        }
        Ok(Some(CursorViewport {
            x: self.scalar(ffi::RenderStateData::CURSOR_VIEWPORT_X)?,
            y: self.scalar(ffi::RenderStateData::CURSOR_VIEWPORT_Y)?,
            at_wide_tail: self.scalar(ffi::RenderStateData::CURSOR_VIEWPORT_WIDE_TAIL)?,
        }))
    }
    pub fn colors(&self) -> Result<Colors> {
        let mut colors = ffi::RenderStateColors {
            size: std::mem::size_of::<ffi::RenderStateColors>(),
            ..Default::default()
        };
        // SAFETY: the sized COLORS query initializes this exact C structure.
        check(unsafe {
            ffi::ghostty_render_state_get(
                self.raw(),
                ffi::RenderStateData::COLORS,
                std::ptr::from_mut(&mut colors).cast(),
            )
        })?;
        Ok(Colors {
            background: colors.background.into(),
            foreground: colors.foreground.into(),
            cursor: colors.cursor_has_value.then_some(colors.cursor.into()),
            palette: colors.palette.map(Into::into),
        })
    }
    pub fn clean(&self) -> Result<()> {
        // SAFETY: the snapshot exclusively owns this render-state update.
        check(unsafe { ffi::ghostty_render_state_clean(self.raw()) })
    }
}

pub struct RowIterator(Owned<ffi::RenderStateRowIteratorImpl>);
pub struct RowIteration<'a> {
    raw: ffi::RenderStateRowIterator,
    borrow: PhantomData<&'a Snapshot<'a>>,
}
pub struct Rows<'a> {
    iterator: &'a mut RowIterator,
    current: RowIteration<'a>,
}
impl RowIterator {
    pub fn new() -> Result<Self> {
        Ok(Self(Owned::create(
            |out| {
                // SAFETY: native creation initializes a row-iterator handle slot.
                unsafe { ffi::ghostty_render_state_row_iterator_new(std::ptr::null(), out) }
            },
            ffi::ghostty_render_state_row_iterator_free,
        )?))
    }
    pub fn update<'a>(&'a mut self, snapshot: &'a Snapshot<'_>) -> Result<Rows<'a>> {
        let mut raw = self.0.raw();
        // SAFETY: ROW_ITERATOR fills an existing iterator; snapshot lives for Rows.
        check(unsafe {
            ffi::ghostty_render_state_get(
                snapshot.raw(),
                ffi::RenderStateData::ROW_ITERATOR,
                std::ptr::from_mut(&mut raw).cast(),
            )
        })?;
        Ok(Rows {
            current: RowIteration {
                raw,
                borrow: PhantomData,
            },
            iterator: self,
        })
    }
}
impl Rows<'_> {
    pub fn next(&mut self) -> Option<&RowIteration<'_>> {
        // SAFETY: update populated the iterator and its snapshot remains borrowed.
        unsafe { ffi::ghostty_render_state_row_iterator_next(self.iterator.0.raw()) }
            .then_some(&self.current)
    }
}
impl RowIteration<'_> {
    pub fn dirty(&self) -> Result<bool> {
        // SAFETY: DIRTY writes a bool for the currently selected row.
        unsafe {
            query(|out| {
                ffi::ghostty_render_state_row_get(self.raw, ffi::RenderStateRowData::DIRTY, out)
            })
        }
    }
}

pub struct CellIterator(Owned<ffi::RenderStateRowCellsImpl>);
pub struct CellIteration<'a> {
    raw: ffi::RenderStateRowCells,
    borrow: PhantomData<&'a RowIteration<'a>>,
}
pub struct Cells<'a> {
    iterator: &'a mut CellIterator,
    current: CellIteration<'a>,
}
impl CellIterator {
    pub fn new() -> Result<Self> {
        Ok(Self(Owned::create(
            |out| {
                // SAFETY: native creation initializes a cell-iterator handle slot.
                unsafe { ffi::ghostty_render_state_row_cells_new(std::ptr::null(), out) }
            },
            ffi::ghostty_render_state_row_cells_free,
        )?))
    }
    pub fn update<'a>(&'a mut self, row: &'a RowIteration<'_>) -> Result<Cells<'a>> {
        let mut raw = self.0.raw();
        // SAFETY: CELLS fills an existing iterator; its row stays borrowed.
        check(unsafe {
            ffi::ghostty_render_state_row_get(
                row.raw,
                ffi::RenderStateRowData::CELLS,
                std::ptr::from_mut(&mut raw).cast(),
            )
        })?;
        Ok(Cells {
            current: CellIteration {
                raw,
                borrow: PhantomData,
            },
            iterator: self,
        })
    }
}
impl Cells<'_> {
    pub fn next(&mut self) -> Option<&CellIteration<'_>> {
        // SAFETY: a row populated this iterator and remains borrowed.
        unsafe { ffi::ghostty_render_state_row_cells_next(self.iterator.0.raw()) }
            .then_some(&self.current)
    }
}
pub struct RawCell(ffi::Cell);
impl RawCell {
    pub fn wide(&self) -> Result<screen::CellWide> {
        // SAFETY: WIDE writes the native CellWide enum, not cell bitfields.
        let width: ffi::CellWide::Type =
            unsafe { query(|out| ffi::ghostty_cell_get(self.0, ffi::CellData::WIDE, out)) }?;
        width
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid native cell width {width}"))
    }
}
impl CellIteration<'_> {
    pub fn raw_cell(&self) -> Result<RawCell> {
        // SAFETY: RAW writes a GhosttyCell value.
        unsafe {
            query(|out| {
                ffi::ghostty_render_state_row_cells_get(
                    self.raw,
                    ffi::RenderStateRowCellsData::RAW,
                    out,
                )
            })
        }
        .map(RawCell)
    }
    pub fn style(&self) -> Result<style::Style> {
        // SAFETY: STYLE initializes GhosttyStyle before its public checked conversion.
        let raw: ffi::Style = unsafe {
            query(|out| {
                ffi::ghostty_render_state_row_cells_get(
                    self.raw,
                    ffi::RenderStateRowCellsData::STYLE,
                    out,
                )
            })
        }?;
        Ok(raw.try_into()?)
    }
    fn color(&self, tag: ffi::RenderStateRowCellsData::Type) -> Result<Option<style::RgbColor>> {
        let mut color = MaybeUninit::<ffi::ColorRgb>::uninit();
        // SAFETY: FG_COLOR/BG_COLOR write RGB only on success; invalid means default.
        let code = unsafe {
            ffi::ghostty_render_state_row_cells_get(self.raw, tag, color.as_mut_ptr().cast())
        };
        if code == ffi::Result::INVALID_VALUE {
            return Ok(None);
        }
        check(code)?;
        // SAFETY: success initialized the ColorRgb queried above.
        Ok(Some(unsafe { color.assume_init() }.into()))
    }
    pub fn fg_color(&self) -> Result<Option<style::RgbColor>> {
        self.color(ffi::RenderStateRowCellsData::FG_COLOR)
    }
    pub fn bg_color(&self) -> Result<Option<style::RgbColor>> {
        self.color(ffi::RenderStateRowCellsData::BG_COLOR)
    }
    pub fn graphemes_utf8(&self, text: &mut String) -> Result<()> {
        text.clear();
        text.reserve(32);
        // SAFETY: length stays zero until native output is initialized and UTF-8 checked.
        let bytes = unsafe { text.as_mut_vec() };
        loop {
            let mut buffer = ffi::Buffer {
                ptr: bytes.as_mut_ptr(),
                cap: bytes.capacity(),
                len: 0,
            };
            // SAFETY: GRAPHEMES_UTF8 respects Buffer.cap and reports required length.
            let code = unsafe {
                ffi::ghostty_render_state_row_cells_get(
                    self.raw,
                    ffi::RenderStateRowCellsData::GRAPHEMES_UTF8,
                    std::ptr::from_mut(&mut buffer).cast(),
                )
            };
            if code == ffi::Result::OUT_OF_SPACE {
                ensure!(buffer.len <= 1024 * 1024, "terminal grapheme exceeds 1 MiB");
                bytes.reserve(buffer.len);
                continue;
            }
            check(code)?;
            ensure!(
                buffer.len <= bytes.capacity(),
                "invalid native UTF-8 length"
            );
            // SAFETY: successful Buffer output initializes exactly len bytes.
            let initialized = unsafe { std::slice::from_raw_parts(bytes.as_ptr(), buffer.len) };
            std::str::from_utf8(initialized)?;
            // SAFETY: the bytes are initialized and checked as UTF-8 above.
            unsafe { bytes.set_len(buffer.len) };
            return Ok(());
        }
    }
}
