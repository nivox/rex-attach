//! Client-owned Kitty image IDs and bounded asynchronous pixel preparation.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    collections::{HashSet, VecDeque},
    fmt::Write as _,
    io::Write as _,
};
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum Mode {
    #[default]
    Auto,
    Kitty,
    Off,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Scene {
    images: Vec<Image>,
    cell: [u32; 2],
    target: [u32; 2],
    viewport: [u32; 2],
    mask: Vec<bool>,
    cols: u16,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    generation: u64,
    source: [u32; 4],
    size: [u32; 2],
    cell: [u32; 2],
    target: [u32; 2],
    clip: [i64; 4],
    mask: Vec<bool>,
    mask_origin: [u32; 2],
}
struct Upload {
    id: u32,
    commands: Vec<String>,
}
struct Prepared {
    scene: Scene,
    cache: HashMap<Key, Arc<Upload>>,
    placements: Vec<(u32, [u32; 4], i32)>,
}

pub struct Output {
    cell: [u32; 2],
    displayed: Option<Scene>,
    cache: HashMap<Key, Arc<Upload>>,
    owned: HashSet<u32>,
    pending: VecDeque<String>,
    job: Option<oneshot::Receiver<Result<Prepared>>>,
    waiting: bool,
    after_upload: Option<HashSet<u32>>,
}
impl Output {
    pub fn new(cell: [u32; 2]) -> Result<Self> {
        ensure!(
            cell.iter().all(|n| (1..=4096).contains(n)),
            "invalid output cell size"
        );
        Ok(Self {
            cell,
            displayed: None,
            cache: HashMap::new(),
            owned: HashSet::new(),
            pending: VecDeque::new(),
            job: None,
            waiting: false,
            after_upload: None,
        })
    }
    pub fn pending(&self) -> bool {
        self.waiting || self.job.is_some() || !self.pending.is_empty()
    }
    pub fn frame(
        &mut self,
        terminal: &Terminal,
        size: (u16, u16),
        images: Vec<Image>,
    ) -> Result<String> {
        if images.is_empty() {
            return Ok(self.clear());
        }
        let target =
            crate::tty::cell_pixels().map_or(self.cell, |(w, h)| [u32::from(w), u32::from(h)]);
        let scene = Scene {
            mask: background_mask(terminal, &images)?,
            images,
            cell: cell_pixels(terminal)?,
            target,
            viewport: [
                u32::from(size.0.min(terminal.cols()?)) * target[0],
                u32::from(size.1.min(terminal.rows()?)) * target[1],
            ],
            cols: terminal.cols()?,
        };
        if scene.images.is_empty() {
            return Ok(self.clear());
        }
        if let Some(job) = self.job.as_mut() {
            match job.try_recv() {
                Ok(result) => {
                    self.job = None;
                    let prepared = result?;
                    if prepared.scene == scene {
                        self.apply(prepared);
                    }
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.job = None;
                    anyhow::bail!("image preparation worker stopped");
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
        }
        self.waiting = self.displayed.as_ref() != Some(&scene);
        if self.job.is_none()
            && self.pending.is_empty()
            && self.displayed.as_ref() != Some(&scene)
            && let Ok(permit) = jobs().try_acquire_owned()
        {
            let (tx, rx) = oneshot::channel();
            let cache = self.cache.clone();
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let _ = tx.send(prepare(scene, cache));
            });
            self.job = Some(rx);
        }
        let mut out = String::new();
        // At most 256 KiB is written per frame. Upload chunks are never
        // interleaved with another graphics command before the final chunk.
        while self
            .pending
            .front()
            .is_some_and(|command| out.len() + command.len() <= 256 * 1024)
        {
            out.push_str(&self.pending.pop_front().unwrap());
        }
        if self.pending.is_empty()
            && let Some(keep) = self.after_upload.take()
        {
            self.owned = keep;
        }
        if !out.is_empty() {
            out = format!("\x1b[?2026h\x1b7{out}\x1b8\x1b[?2026l");
        }
        Ok(out)
    }
    fn apply(&mut self, prepared: Prepared) {
        let keep: HashSet<_> = prepared.cache.values().map(|u| u.id).collect();
        for upload in prepared.cache.values() {
            if !self.owned.contains(&upload.id) {
                self.pending.extend(upload.commands.iter().cloned());
            }
        }
        // Keep old placements visible until all new uploads are complete.
        for id in &self.owned {
            self.pending.push_back(format!(
                "\x1b_Ga=d,d={},i={id},q=2\x1b\\",
                if keep.contains(id) { "i" } else { "I" }
            ));
        }
        self.owned.extend(keep.iter().copied());
        self.after_upload = Some(keep);
        for (index, (id, at, z)) in prepared.placements.into_iter().enumerate() {
            let [w, h] = prepared.scene.target;
            self.pending.push_back(format!(
                "\x1b[{};{}H\x1b_Ga=p,i={id},p={},X={},Y={},z={z},C=1,q=2\x1b\\",
                at[1] / h + 1,
                at[0] / w + 1,
                index + 1,
                at[0] % w,
                at[1] % h
            ));
        }
        self.cache = prepared.cache;
        self.displayed = Some(prepared.scene);
    }
    pub fn clear(&mut self) -> String {
        self.pending.clear();
        self.job = None;
        self.cache.clear();
        self.displayed = None;
        self.waiting = false;
        self.after_upload = None;
        let mut out = String::new();
        for id in self.owned.drain() {
            write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\").unwrap();
        }
        out
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        let clear = self.clear();
        if !clear.is_empty() {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(clear.as_bytes());
            let _ = out.flush();
        }
    }
}

fn background_mask(terminal: &Terminal, images: &[Image]) -> Result<Vec<bool>> {
    if !images.iter().any(|i| i.placement.layer == 0) {
        return Ok(vec![]);
    }
    let mut render = crate::vt::RenderState::new()?;
    let snapshot = render.update(terminal)?;
    let default = snapshot.colors()?.background;
    let mut rows = crate::vt::render::RowIterator::new()?;
    let mut cells = crate::vt::render::CellIterator::new()?;
    let mut result = vec![];
    let mut rows = rows.update(&snapshot)?;
    while let Some(row) = rows.next() {
        let mut cells = cells.update(row)?;
        while let Some(cell) = cells.next() {
            let style = cell.style()?;
            let bg = if style.inverse {
                cell.fg_color()?.unwrap_or(snapshot.colors()?.foreground)
            } else {
                cell.bg_color()?.unwrap_or(default)
            };
            result.push(bg != default);
            if result.len() >= 1024 * 512 {
                return Ok(result);
            }
        }
    }
    Ok(result)
}

fn prepare(scene: Scene, old: HashMap<Key, Arc<Upload>>) -> Result<Prepared> {
    let mut cache = HashMap::new();
    let mut placements = vec![];
    let mut budget = IMAGE_LIMIT;
    for image in &scene.images {
        let at = bounds(image, scene.cell, scene.target, scene.viewport);
        let (width, height) = (at[2] - at[0], at[3] - at[1]);
        let bytes = u64::from(width) * u64::from(height) * 4;
        if bytes == 0 || bytes > budget {
            continue;
        }
        budget -= bytes;
        let p = &image.placement;
        let origin = [
            i64::from(p.cell[0]) * i64::from(scene.cell[0]) + i64::from(p.offset[0]),
            i64::from(p.cell[1]) * i64::from(scene.cell[1]) + i64::from(p.offset[1]),
        ];
        let key = Key {
            generation: p.generation,
            source: p.source,
            size: p.size,
            cell: scene.cell,
            target: scene.target,
            clip: [
                i64::from(at[0]) * i64::from(scene.cell[0])
                    - origin[0] * i64::from(scene.target[0]),
                i64::from(at[1]) * i64::from(scene.cell[1])
                    - origin[1] * i64::from(scene.target[1]),
                i64::from(width),
                i64::from(height),
            ],
            mask: if p.layer == 0 {
                scene.mask.clone()
            } else {
                vec![]
            },
            mask_origin: if p.layer == 0 { [at[0], at[1]] } else { [0; 2] },
        };
        let upload = if let Some(upload) = cache.get(&key).or_else(|| old.get(&key)) {
            Arc::clone(upload)
        } else {
            let mut rgba = Vec::with_capacity(bytes as usize);
            for y in at[1]..at[3] {
                for x in at[0]..at[2] {
                    let mut pixel = sample(image, scene.cell, scene.target, x, y)
                        .unwrap_or(image::Rgba([0; 4]));
                    if p.layer == 0 {
                        let index = (y / scene.target[1]) as usize * usize::from(scene.cols)
                            + (x / scene.target[0]) as usize;
                        if scene.mask.get(index).copied().unwrap_or(true) {
                            pixel.0[3] = 0;
                        }
                    }
                    rgba.extend_from_slice(&pixel.0);
                }
            }
            Arc::new(upload(width, height, &rgba))
        };
        // The native text renderer uses explicit RGB backgrounds. Mask layer 0
        // against non-default backgrounds, then place it under glyphs like layer 1.
        let z = if p.layer < 2 {
            -(scene.images.len() as i32) + placements.len() as i32
        } else {
            placements.len() as i32
        };
        placements.push((upload.id, at, z));
        cache.insert(key, upload);
    }
    Ok(Prepared {
        scene,
        cache,
        placements,
    })
}

fn upload(width: u32, height: u32, pixels: &[u8]) -> Upload {
    let id = super::next_id();
    let encoded = STANDARD.encode(pixels);
    let mut commands = vec![];
    for (n, chunk) in encoded.as_bytes().chunks(4096).enumerate() {
        let more = usize::from((n + 1) * 4096 < encoded.len());
        let header = if n == 0 {
            format!("a=t,t=d,f=32,s={width},v={height},i={id},q=2,")
        } else {
            String::new()
        };
        commands.push(format!(
            "\x1b_G{header}m={more};{}\x1b\\",
            std::str::from_utf8(chunk).unwrap()
        ));
    }
    Upload { id, commands }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::tests::{terminal, transmit};

    async fn draw(
        output: &mut Output,
        source: &Terminal,
        display: &mut Terminal,
    ) -> Result<String> {
        let mut bytes = String::new();
        for _ in 0..300 {
            let frame = output.frame(
                source,
                (12, 6),
                crate::graphics::Reader::new()?.capture(source)?,
            )?;
            assert!(frame.len() < 261 * 1024, "unbounded image output frame");
            display.vt_write(frame.as_bytes());
            bytes.push_str(&frame);
            if !output.pending() {
                return Ok(bytes);
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        anyhow::bail!("image worker did not finish")
    }

    #[tokio::test]
    async fn output_reuses_uploads_moves_clips_replaces_and_cleans_only_owned_ids() -> Result<()> {
        let mut source = terminal()?;
        let mut display = terminal()?;
        transmit(&mut display, 7, [1, 2, 3, 255], "c=1,r=1");
        transmit(&mut source, 42, [255, 0, 0, 255], "c=2,r=2");
        let mut output = Output::new([10, 20])?;
        let first = draw(&mut output, &source, &mut display).await?;
        assert!(first.contains("a=t,") && first.contains("a=p,"));
        let images = Reader::new()?.capture(&display)?;
        let image = images.iter().find(|i| i.pixels.id != 7).unwrap();
        assert_eq!([image.pixels.width, image.pixels.height], [20, 40]);
        assert_eq!(&image.pixels.rgba[..4], &[255, 0, 0, 255]);
        assert!(draw(&mut output, &source, &mut display).await?.is_empty());
        source.vt_write(b"\x1b[2;4H\x1b_Ga=p,i=42,p=1,c=2,r=2,C=1,q=2\x1b\\");
        let moved = draw(&mut output, &source, &mut display).await?;
        assert!(
            !moved.contains("a=t,"),
            "moving the same pixels must not retransmit"
        );
        assert!(moved.contains("a=p,"));
        source.vt_write(b"\x1b_Ga=d,d=I,i=42,q=2\x1b\\");
        draw(&mut output, &source, &mut display).await?;
        let images = Reader::new()?.capture(&display)?;
        assert!(images.iter().all(|i| i.pixels.id == 7));
        assert!(output.clear().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn partial_uploads_keep_old_placements_and_cleanup_aborts_every_owned_image() -> Result<()>
    {
        let mut source = terminal()?;
        let mut display = terminal()?;
        source.resize(100, 60, 10, 20)?;
        display.resize(100, 60, 10, 20)?;
        transmit(&mut source, 42, [255, 0, 0, 255], "c=2,r=2,C=1");
        let mut output = Output::new([10, 20])?;
        draw(&mut output, &source, &mut display).await?;
        source.vt_write(b"\x1b[H");
        transmit(&mut source, 42, [0, 255, 0, 255], "c=40,r=20,C=1");
        let mut partial = false;
        for _ in 0..300 {
            let images = crate::graphics::Reader::new()?.capture(&source)?;
            let frame = output.frame(&source, (100, 60), images)?;
            assert!(frame.len() <= 256 * 1024 + 64);
            display.vt_write(frame.as_bytes());
            if frame.contains("a=t,") {
                assert!(
                    !frame.contains("a=p,"),
                    "large upload must be spread across frames"
                );
                assert!(output.pending());
                assert!(
                    output.owned.len() >= 2,
                    "cleanup must still own the old visible image"
                );
                assert_eq!(crate::graphics::Reader::new()?.capture(&display)?.len(), 1);
                partial = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert!(partial, "worker did not prepare large image");
        display.vt_write(output.clear().as_bytes());
        assert!(
            crate::graphics::Reader::new()?
                .capture(&display)?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn clipping_and_layer_zero_background_mask_are_prepared_before_upload() -> Result<()> {
        let mut source = terminal()?;
        transmit(&mut source, 42, [255, 0, 0, 255], "c=2,r=2,z=-2000000000");
        source.vt_write(b"\x1b[1;1H\x1b[44m \x1b[0m");
        let images = Reader::new()?.capture(&source)?;
        let scene = Scene {
            mask: background_mask(&source, &images)?,
            images,
            cell: [10, 20],
            target: [10, 20],
            viewport: [15, 25],
            cols: 12,
        };
        let prepared = prepare(scene, HashMap::new())?;
        assert_eq!(prepared.placements[0].1, [0, 0, 15, 25]);
        let upload = prepared.cache.values().next().unwrap();
        let mut receiver = terminal()?;
        for c in &upload.commands {
            receiver.vt_write(c.as_bytes());
        }
        let image = receiver.kitty_graphics()?.image(upload.id).unwrap();
        assert_eq!([image.width()?, image.height()?], [15, 25]);
        assert_eq!(image.data()?[3], 0);
        assert_eq!(image.data()?[(10 * 4) + 3], 255);
        Ok(())
    }
}
