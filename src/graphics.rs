//! Bounded, owned views of Ghostty graphics. Never retain borrowed image data
//! across VT writes, and never interpret a server file path on the client.
use crate::vt::kitty::graphics::{self, DecodePng, DecodedImage, ImageFormat};
use crate::vt::{
    Terminal,
    graphics::{ImageIterator, ImagePlacement},
    render::Snapshot,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

pub mod output;
pub const IMAGE_LIMIT: u64 = 64 * 1024 * 1024;
pub const PLACEMENT_LIMIT: usize = 1024;

pub fn next_id() -> u32 {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    loop {
        let id = std::process::id()
            .wrapping_mul(2654435761)
            .wrapping_add(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        if id != 0 {
            return id;
        }
    }
}

pub fn jobs() -> Arc<tokio::sync::Semaphore> {
    static JOBS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    JOBS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
}

struct Png;
impl DecodePng for Png {
    fn decode_png(
        &mut self,
        alloc: &crate::vt::alloc::Allocator,
        data: &[u8],
    ) -> Option<DecodedImage> {
        use image::{ImageDecoder, codecs::png::PngDecoder};
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(IMAGE_LIMIT);
        let decoder = PngDecoder::with_limits(std::io::Cursor::new(data), limits).ok()?;
        let (width, height) = decoder.dimensions();
        let size = u64::from(width)
            .checked_mul(u64::from(height))?
            .checked_mul(4)?;
        if size == 0 || size > IMAGE_LIMIT {
            return None;
        }
        let pixels = image::DynamicImage::from_decoder(decoder)
            .ok()?
            .into_rgba8()
            .into_raw();
        let mut bytes = crate::vt::alloc::Bytes::new_with_alloc(alloc, pixels.len()).ok()?;
        bytes.copy_from_slice(&pixels);
        Some(DecodedImage {
            width,
            height,
            data: bytes,
        })
    }
}

pub fn install_decoder() -> Result<()> {
    graphics::set_png_decoder(Some(Box::new(Png)))?;
    Ok(())
}

pub fn configure(terminal: &mut Terminal) -> Result<()> {
    terminal
        .set_kitty_image_storage_limit(IMAGE_LIMIT)
        .context("setting image storage limit")?;
    terminal
        .set_apc_max_bytes_kitty(Some(IMAGE_LIMIT as usize))
        .context("setting Kitty APC limit")?;
    terminal.set_kitty_image_from_file_allowed(false)?;
    terminal.set_kitty_image_temp_directory(None)?;
    terminal.set_kitty_image_from_shared_mem_allowed(false)?;
    Ok(())
}

#[derive(Debug)]
pub struct Pixels {
    pub id: u32,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}
impl PartialEq for Pixels {
    fn eq(&self, other: &Self) -> bool {
        (self.id, self.generation) == (other.id, other.generation)
    }
}
impl Eq for Pixels {}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub placement: ImagePlacement,
    pub pixels: Arc<Pixels>,
}

pub struct Images {
    iterator: ImageIterator,
    cache: HashMap<(u32, u64), Arc<Pixels>>,
}
impl Images {
    pub fn new() -> Result<Self> {
        Ok(Self {
            iterator: ImageIterator::new()?,
            cache: HashMap::new(),
        })
    }
    pub fn capture(&mut self, terminal: &Terminal, snapshot: &Snapshot<'_>) -> Result<Vec<Image>> {
        let storage = terminal.kitty_graphics()?;
        if storage.generation()? == 0 {
            self.cache.clear();
            return Ok(vec![]);
        }
        let placements = self.iterator.placements(snapshot, PLACEMENT_LIMIT)?;
        let mut cache = HashMap::new();
        let mut result = vec![];
        let mut total = 0u64;
        for placement in placements.into_iter().take(PLACEMENT_LIMIT) {
            if placement.size.contains(&0) || placement.source[2..].contains(&0) {
                continue;
            }
            let key = (placement.image_id, placement.generation);
            let pixels = if let Some(p) = cache.get(&key).or_else(|| self.cache.get(&key)) {
                Arc::clone(p)
            } else {
                let Some(image) = storage.image(placement.image_id) else {
                    continue;
                };
                let (width, height) = (image.width()?, image.height()?);
                let Some(bytes) = u64::from(width)
                    .checked_mul(u64::from(height))
                    .and_then(|n| n.checked_mul(4))
                else {
                    continue;
                };
                if bytes == 0 || bytes > IMAGE_LIMIT || total + bytes > IMAGE_LIMIT {
                    continue;
                }
                let Ok(data) = image.data() else {
                    continue;
                };
                let channels = match image.format()? {
                    ImageFormat::Rgba => 4,
                    ImageFormat::Rgb => 3,
                    ImageFormat::GrayAlpha => 2,
                    ImageFormat::Gray => 1,
                    _ => continue,
                };
                ensure!(
                    data.len() as u64 == u64::from(width) * u64::from(height) * channels,
                    "invalid graphics pixel buffer"
                );
                let mut rgba = Vec::with_capacity(bytes as usize);
                for pixel in data.chunks_exact(channels as usize) {
                    match channels {
                        4 => rgba.extend_from_slice(pixel),
                        3 => rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]),
                        2 => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]),
                        _ => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255]),
                    }
                }
                Arc::new(Pixels {
                    id: key.0,
                    generation: key.1,
                    width,
                    height,
                    rgba,
                })
            };
            if !cache.contains_key(&key) {
                total += pixels.rgba.len() as u64;
            }
            if total > IMAGE_LIMIT {
                continue;
            }
            cache.insert(key, pixels.clone());
            result.push(Image { placement, pixels });
        }
        self.cache = cache;
        Ok(result)
    }
}

pub fn cell_pixels(terminal: &Terminal) -> Result<[u32; 2]> {
    // Snapshots from older servers can omit pixel metrics.
    Ok([
        (terminal.width_px()? / u32::from(terminal.cols()?).max(1)).clamp(1, 4096),
        (terminal.height_px()? / u32::from(terminal.rows()?).max(1)).clamp(1, 4096),
    ])
}

/// Sampling keeps crop, clipping, and offsets consistent between
/// software previews and native Kitty output without allocating huge images.
pub fn sample(
    image: &Image,
    cell: [u32; 2],
    target: [u32; 2],
    x: u32,
    y: u32,
) -> Option<image::Rgba<u8>> {
    let p = &image.placement;
    let origin = [
        i64::from(p.cell[0]) * i64::from(cell[0]) + i64::from(p.offset[0]),
        i64::from(p.cell[1]) * i64::from(cell[1]) + i64::from(p.offset[1]),
    ];
    let at = [
        i64::from(x) * i64::from(cell[0]) / i64::from(target[0]) - origin[0],
        i64::from(y) * i64::from(cell[1]) / i64::from(target[1]) - origin[1],
    ];
    if at[0] < 0 || at[1] < 0 || at[0] >= i64::from(p.size[0]) || at[1] >= i64::from(p.size[1]) {
        return None;
    }
    let sx = u64::from(p.source[0]) + at[0] as u64 * u64::from(p.source[2]) / u64::from(p.size[0]);
    let sy = u64::from(p.source[1]) + at[1] as u64 * u64::from(p.source[3]) / u64::from(p.size[1]);
    if sx >= u64::from(image.pixels.width) || sy >= u64::from(image.pixels.height) {
        return None;
    }
    let offset = ((sy * u64::from(image.pixels.width) + sx) * 4) as usize;
    Some(image::Rgba(
        image.pixels.rgba[offset..offset + 4].try_into().ok()?,
    ))
}

pub fn bounds(image: &Image, cell: [u32; 2], target: [u32; 2], viewport: [u32; 2]) -> [u32; 4] {
    let p = &image.placement;
    let mut bounds = [0; 4];
    for axis in 0..2 {
        let start = i64::from(p.cell[axis]) * i64::from(cell[axis]) + i64::from(p.offset[axis]);
        let scale = |v: i64| v * i64::from(target[axis]) / i64::from(cell[axis]);
        bounds[axis] = scale(start).clamp(0, i64::from(viewport[axis])) as u32;
        bounds[axis + 2] =
            scale(start + i64::from(p.size[axis])).clamp(0, i64::from(viewport[axis])) as u32;
    }
    bounds
}

/// Standalone reader for emulator comparisons; production shares text snapshots.
#[cfg(test)]
pub struct Reader {
    state: crate::vt::RenderState,
    images: Images,
}
#[cfg(test)]
impl Reader {
    pub fn new() -> Result<Self> {
        Ok(Self {
            state: crate::vt::RenderState::new()?,
            images: Images::new()?,
        })
    }
    pub fn capture(&mut self, terminal: &Terminal) -> Result<Vec<Image>> {
        if terminal.kitty_graphics()?.generation()? == 0 {
            return Ok(vec![]);
        }
        let snapshot = self.state.update(terminal)?;
        self.images.capture(terminal, &snapshot)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};

    pub fn terminal() -> Result<Terminal> {
        install_decoder()?;
        let mut t = Terminal::new(crate::vt::TerminalOptions {
            cols: 12,
            rows: 6,
            ..Default::default()
        })?;
        configure(&mut t)?;
        t.resize(12, 6, 10, 20)?;
        Ok(t)
    }
    pub fn transmit(t: &mut Terminal, id: u32, pixel: [u8; 4], placement: &str) {
        let data = STANDARD.encode(pixel.repeat(4));
        t.vt_write(
            format!("\x1b_Ga=T,f=32,s=2,v=2,i={id},q=2,{placement};{data}\x1b\\").as_bytes(),
        );
    }

    #[test]
    fn file_and_temporary_media_cannot_read_or_unlink_client_files() -> Result<()> {
        let mut t = terminal()?;
        let path = std::env::temp_dir().join(format!(
            "rex-attach-image-{}-{}",
            std::process::id(),
            next_id()
        ));
        std::fs::write(&path, [255, 0, 0, 255].repeat(4))?;
        let encoded = STANDARD.encode(path.as_os_str().as_encoded_bytes());
        for medium in ['f', 't'] {
            t.vt_write(
                format!("\x1b_Ga=T,t={medium},f=32,s=2,v=2,i=42,q=2;{encoded}\x1b\\").as_bytes(),
            );
            assert!(Reader::new()?.capture(&t)?.is_empty());
            assert_eq!(std::fs::read(&path)?, [255, 0, 0, 255].repeat(4));
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn snapshot_live_fragmentation_replacement_and_deletion_retain_images() -> Result<()> {
        let mut original = terminal()?;
        transmit(&mut original, 42, [255, 0, 0, 255], "c=2,r=2");
        let initial = crate::vt::snapshot::encode(&original)?.to_vec();
        let mut state = crate::terminal::State::new(5);
        for chunk in initial.chunks(7) {
            state.feed(chunk)?;
        }
        let mut reader = Reader::new()?;
        let first = reader.capture(state.terminal()?)?;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].pixels.rgba, [255, 0, 0, 255].repeat(4));
        assert_eq!(first[0].placement.cell, [0, 0]);
        assert_eq!(first[0].placement.size, [20, 40]);
        let same = reader.capture(state.terminal()?)?;
        assert!(Arc::ptr_eq(&first[0].pixels, &same[0].pixels));
        let live = format!(
            "\x1b[H\x1b_Ga=T,f=32,s=2,v=2,i=42,c=2,r=2,q=2;{}\x1b\\",
            STANDARD.encode([0, 255, 0, 255].repeat(4))
        );
        for byte in live.bytes() {
            state.feed(&[byte])?;
        }
        let second = reader.capture(state.terminal()?)?;
        assert_eq!(second[0].pixels.rgba, [0, 255, 0, 255].repeat(4));
        assert_ne!(first[0].pixels.generation, second[0].pixels.generation);
        state.feed(b"\x1b_Ga=d,d=I,i=42,q=2\x1b\\")?;
        assert!(reader.capture(state.terminal()?)?.is_empty());
        assert!(!state.terminal()?.is_kitty_image_from_file_allowed()?);
        assert!(!state.terminal()?.is_kitty_image_from_temp_file_allowed()?);
        assert!(!state.terminal()?.is_kitty_image_from_shared_mem_allowed()?);
        Ok(())
    }

    #[test]
    fn png_decodes_grayscale_alpha_and_rgb_and_rejects_oversized_headers() -> Result<()> {
        use image::{ExtendedColorType, ImageEncoder};
        for (pixels, color, expected) in [
            (&[128][..], ExtendedColorType::L8, [128, 128, 128, 255]),
            (&[128, 64][..], ExtendedColorType::La8, [128, 128, 128, 64]),
            (&[255, 0, 0][..], ExtendedColorType::Rgb8, [255, 0, 0, 255]),
        ] {
            let mut encoded = vec![];
            image::codecs::png::PngEncoder::new(&mut encoded).write_image(pixels, 1, 1, color)?;
            let png = STANDARD.encode(encoded);
            let mut t = terminal()?;
            t.vt_write(format!("\x1b_Ga=T,f=100,i=43,c=1,r=1,q=2;{png}\x1b\\").as_bytes());
            let frame = Reader::new()?.capture(&t)?;
            assert_eq!(frame.len(), 1);
            assert_eq!(frame[0].pixels.rgba, expected);
        }
        let mut t = terminal()?;
        t.vt_write(b"\x1b_Ga=T,f=32,s=65535,v=65535,i=1,q=2;AAAA\x1b\\");
        assert!(Reader::new()?.capture(&t)?.is_empty());
        Ok(())
    }

    #[test]
    fn unicode_relative_and_scrollback_geometry_come_from_native_iterator() -> Result<()> {
        let mut t = terminal()?;
        transmit(&mut t, 42, [255, 0, 0, 255], "U=1,c=2,r=1");
        t.vt_write("\x1b[38;2;0;0;42m\u{10eeee}\u{305}\u{305}\x1b[0m".as_bytes());
        let frame = Reader::new()?.capture(&t)?;
        assert!(!frame.is_empty());
        assert_eq!(frame[0].pixels.id, 42);
        t.vt_write(b"\x1b[2;1H");
        transmit(&mut t, 44, [0, 255, 0, 255], "p=1,c=2,r=1");
        transmit(&mut t, 45, [0, 0, 255, 255], "p=2,P=44,Q=1,H=1,V=1,c=1,r=1");
        let frame = Reader::new()?.capture(&t)?;
        let parent = frame.iter().find(|i| i.pixels.id == 44).unwrap();
        let child = frame.iter().find(|i| i.pixels.id == 45).unwrap();
        assert_eq!(
            child.placement.cell,
            [parent.placement.cell[0] + 1, parent.placement.cell[1] + 1]
        );
        t.vt_write(b"\x1b[6;1H\r\n\r\n");
        t.scroll_viewport(crate::vt::terminal::ScrollViewport::Top);
        let frame = Reader::new()?.capture(&t)?;
        assert!(frame.iter().any(|i| i.pixels.id == 44));
        Ok(())
    }
}
