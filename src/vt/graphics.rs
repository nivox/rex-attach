//! Image access through documented native handles. Geometry is read in bulk
//! from the same render update as text; no private Rust layout is inspected.
use super::*;
pub use ghostty_vt::kitty::graphics::{DecodePng, DecodedImage, ImageFormat, set_png_decoder};

pub struct Graphics<'a> {
    raw: NonNull<ffi::KittyGraphicsImpl>,
    owner: PhantomData<&'a Terminal>,
}
pub struct Image<'a> {
    raw: NonNull<ffi::KittyGraphicsImageImpl>,
    owner: PhantomData<&'a Terminal>,
}
impl Terminal {
    pub fn kitty_graphics(&self) -> Result<Graphics<'_>> {
        // SAFETY: KITTY_GRAPHICS writes a borrowed native storage pointer.
        let raw: ffi::KittyGraphics = unsafe {
            query(|out| {
                ffi::ghostty_terminal_get(self.raw(), ffi::TerminalData::KITTY_GRAPHICS, out)
            })
        }?;
        Ok(Graphics {
            raw: NonNull::new(raw).ok_or_else(|| anyhow::anyhow!("missing image storage"))?,
            owner: PhantomData,
        })
    }
}
impl<'a> Graphics<'a> {
    pub fn generation(&self) -> Result<u64> {
        // SAFETY: GENERATION writes a uint64_t; the terminal remains borrowed.
        unsafe {
            query(|out| {
                ffi::ghostty_kitty_graphics_get(
                    self.raw.as_ptr(),
                    ffi::KittyGraphicsData::GENERATION,
                    out,
                )
            })
        }
    }
    pub fn image(&self, id: u32) -> Option<Image<'a>> {
        // SAFETY: lookup returns terminal-owned memory, protected by the borrow.
        let raw = unsafe { ffi::ghostty_kitty_graphics_image(self.raw.as_ptr(), id) };
        NonNull::new(raw.cast_mut()).map(|raw| Image {
            raw,
            owner: PhantomData,
        })
    }
}
impl Image<'_> {
    fn scalar<T>(&self, tag: ffi::KittyGraphicsImageData::Type) -> Result<T> {
        // SAFETY: private callers pair each tag with its documented output type.
        unsafe { query(|out| ffi::ghostty_kitty_graphics_image_get(self.raw.as_ptr(), tag, out)) }
    }
    pub fn width(&self) -> Result<u32> {
        self.scalar(ffi::KittyGraphicsImageData::WIDTH)
    }
    pub fn height(&self) -> Result<u32> {
        self.scalar(ffi::KittyGraphicsImageData::HEIGHT)
    }
    pub fn format(&self) -> Result<ImageFormat> {
        let value: ffi::KittyImageFormat::Type =
            self.scalar(ffi::KittyGraphicsImageData::FORMAT)?;
        value
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid native image format {value}"))
    }
    pub fn data(&self) -> Result<&[u8]> {
        let length: usize = self.scalar(ffi::KittyGraphicsImageData::DATA_LEN)?;
        if length == 0 {
            return Ok(&[]);
        }
        let pointer: *const u8 = self.scalar(ffi::KittyGraphicsImageData::DATA_PTR)?;
        ensure!(
            !pointer.is_null() && length <= isize::MAX as usize,
            "invalid image buffer"
        );
        // SAFETY: native storage owns this initialized buffer; its terminal
        // cannot be mutated while the borrowed Image and returned slice exist.
        Ok(unsafe { std::slice::from_raw_parts(pointer, length) })
    }
}

/// An owned description of the native image operation to reproduce in a view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImagePlacement {
    pub image_id: u32,
    pub generation: u64,
    pub layer: i32,
    pub z: i32,
    pub cell: [i32; 2],
    pub offset: [u32; 2],
    pub size: [u32; 2],
    pub source: [u32; 4],
}
pub struct ImageIterator(Owned<ffi::RenderStateImageIteratorImpl>);
impl ImageIterator {
    pub fn new() -> Result<Self> {
        Ok(Self(Owned::create(
            |out| {
                // SAFETY: native creation initializes one owned iterator handle.
                unsafe { ffi::ghostty_render_state_image_iterator_new(std::ptr::null(), out) }
            },
            ffi::ghostty_render_state_image_iterator_free,
        )?))
    }
    pub fn placements(
        &mut self,
        snapshot: &render::Snapshot<'_>,
        limit: usize,
    ) -> Result<Vec<ImagePlacement>> {
        let mut iterator = self.0.raw();
        // SAFETY: IMAGE_ITERATOR fills an existing iterator from a live update.
        check(unsafe {
            ffi::ghostty_render_state_get(
                snapshot.raw(),
                ffi::RenderStateData::IMAGE_ITERATOR,
                std::ptr::from_mut(&mut iterator).cast(),
            )
        })?;
        let mut placements = Vec::new();
        while placements.len() < limit {
            // SAFETY: the snapshot is unchanged for the entire iteration.
            if !unsafe { ffi::ghostty_render_state_image_iterator_next(iterator) } {
                break;
            }
            let mut protocol: ffi::ImageProtocol::Type = 0;
            let mut placement = ImagePlacement::default();
            use ffi::RenderStateImageData as D;
            let keys = [
                D::PROTOCOL,
                D::IMAGE_ID,
                D::IMAGE_GENERATION,
                D::LAYER,
                D::Z,
                D::VIEWPORT_COL,
                D::VIEWPORT_ROW,
                D::CELL_OFFSET_X,
                D::CELL_OFFSET_Y,
                D::PIXEL_WIDTH,
                D::PIXEL_HEIGHT,
                D::SOURCE_X,
                D::SOURCE_Y,
                D::SOURCE_WIDTH,
                D::SOURCE_HEIGHT,
            ];
            let mut outputs = [
                std::ptr::from_mut(&mut protocol).cast::<c_void>(),
                std::ptr::from_mut(&mut placement.image_id).cast(),
                std::ptr::from_mut(&mut placement.generation).cast(),
                std::ptr::from_mut(&mut placement.layer).cast(),
                std::ptr::from_mut(&mut placement.z).cast(),
                std::ptr::from_mut(&mut placement.cell[0]).cast(),
                std::ptr::from_mut(&mut placement.cell[1]).cast(),
                std::ptr::from_mut(&mut placement.offset[0]).cast(),
                std::ptr::from_mut(&mut placement.offset[1]).cast(),
                std::ptr::from_mut(&mut placement.size[0]).cast(),
                std::ptr::from_mut(&mut placement.size[1]).cast(),
                std::ptr::from_mut(&mut placement.source[0]).cast(),
                std::ptr::from_mut(&mut placement.source[1]).cast(),
                std::ptr::from_mut(&mut placement.source[2]).cast(),
                std::ptr::from_mut(&mut placement.source[3]).cast(),
            ];
            // SAFETY: keys and outputs have identical lengths; every pointer
            // addresses its documented scalar type and remains live for this call.
            check(unsafe {
                ffi::ghostty_render_state_image_get_multi(
                    iterator,
                    keys.len(),
                    keys.as_ptr(),
                    outputs.as_mut_ptr(),
                    std::ptr::null_mut(),
                )
            })?;
            if protocol != ffi::ImageProtocol::KITTY {
                continue;
            }
            ensure!(
                (0..=2).contains(&placement.layer),
                "invalid native image layer"
            );
            placements.push(placement);
        }
        placements.sort_by_key(|p| (p.layer, p.z, p.image_id));
        Ok(placements)
    }
}
