use crate::{domain::Target, protocol::Control, stream, terminal};
use anyhow::{Context, Result};
use ratatui_image::{
    picker::{
        ProtocolType,
        cap_parser::{Parser, QueryStdioOptions, Response},
    },
    protocol::{Protocol, kitty::Kitty},
};
use std::time::Duration;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::{oneshot, watch};

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum Mode {
    #[default]
    Auto,
    Kitty,
    Text,
}

#[derive(Clone, Default)]
pub struct Options {
    graphics: Option<Graphics>,
    raster: Option<Arc<Mutex<crate::raster::Rasterizer>>>,
    image_jobs: Option<Arc<tokio::sync::Semaphore>>,
}
impl Options {
    pub async fn detect(
        mode: Mode,
        font: Option<&Path>,
        input: &mut crate::tty::Input,
    ) -> Result<Self> {
        let graphics = match mode {
            Mode::Text => return Ok(Self::default()),
            Mode::Kitty => probe(input).await?.unwrap_or(Graphics {
                font: ratatui_image::FontSize::new(10, 20),
                tmux: std::env::var_os("TMUX").is_some(),
                compress: false,
            }),
            // These implementations answer a basic Kitty query but do not
            // support the Unicode placements used by ratatui-image.
            Mode::Auto
                if ["WEZTERM_EXECUTABLE", "KONSOLE_VERSION"]
                    .iter()
                    .any(|key| std::env::var(key).is_ok_and(|s| !s.is_empty())) =>
            {
                return Ok(Self::default());
            }
            Mode::Auto => match probe(input).await? {
                Some(g) => g,
                None => return Ok(Self::default()),
            },
        };
        let env_font = std::env::var_os("REX_ATTACH_PREVIEW_FONT").map(std::path::PathBuf::from);
        let raster = match crate::raster::Rasterizer::load(font.or(env_font.as_deref())) {
            Ok(raster) => raster,
            Err(_) if matches!(mode, Mode::Auto) && font.is_none() => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        Ok(Self {
            graphics: Some(graphics),
            raster: Some(Arc::new(Mutex::new(raster))),
            image_jobs: Some(Arc::new(tokio::sync::Semaphore::new(1))),
        })
    }
    pub fn images(&self) -> bool {
        self.graphics.is_some()
    }
    fn encode(
        &self,
        grid: &crate::raster::Grid,
        width: u16,
        height: u16,
        id: u32,
    ) -> Result<RenderedImage> {
        let graphics = self.graphics.as_ref().context("image picker unavailable")?;
        let image = self
            .raster
            .as_ref()
            .context("preview font unavailable")?
            .lock()
            .map_err(|_| anyhow::anyhow!("preview rasterizer failed"))?
            .render(grid);
        let font = graphics.font;
        let image = image.resize(
            u32::from(width.max(1)) * u32::from(font.width),
            u32::from(height.max(1)) * u32::from(font.height),
            image::imageops::FilterType::Triangle,
        );
        let size = (
            image.width().div_ceil(u32::from(font.width)) as u16,
            image.height().div_ceil(u32::from(font.height)) as u16,
        )
            .into();
        let tmux = graphics.tmux;
        let protocol = Protocol::Kitty(Kitty::new(image, size, id, tmux, graphics.compress)?);
        Ok(RenderedImage { protocol, id, tmux })
    }
}

#[derive(Clone)]
struct Graphics {
    font: ratatui_image::FontSize,
    tmux: bool,
    compress: bool,
}

// Probe with our existing async tty reader. A timed-out probe must not leave a
// background stdio reader competing with the browser for keyboard input.
#[derive(Default)]
struct Probe {
    sequence: Vec<u8>,
    keys: Vec<u8>,
    kitty: bool,
    compress: bool,
    font: Option<(u16, u16)>,
    done: bool,
}
impl Probe {
    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.byte(byte);
        }
    }
    fn byte(&mut self, byte: u8) {
        if self.sequence.is_empty() {
            if byte == 0x1b {
                self.sequence.push(byte);
            } else {
                self.keys.push(byte);
            }
            return;
        }
        self.sequence.push(byte);
        let csi = self.sequence.starts_with(b"\x1b[");
        let apc = self.sequence.starts_with(b"\x1b_G");
        let finished = (csi && self.sequence.len() > 2 && (0x40..=0x7e).contains(&byte))
            || (apc && self.sequence.ends_with(b"\x1b\\"));
        if finished {
            if apc || (csi && b"ntc".contains(&byte)) {
                self.reply();
            } else {
                self.keys.extend_from_slice(&self.sequence);
            }
            self.sequence.clear();
        } else if (self.sequence.len() == 2 && byte != b'[' && byte != b'_')
            || self.sequence.len() > 4096
        {
            self.keys.append(&mut self.sequence);
        }
    }
    fn reply(&mut self) {
        let mut parser = Parser::default();
        for ch in String::from_utf8_lossy(&self.sequence).chars() {
            for response in parser.push(ch) {
                match response {
                    Response::Kitty => self.kitty = true,
                    Response::KittyCompression => self.compress = true,
                    Response::CellSize(Some((w, h))) if w > 0 && h > 0 => self.font = Some((w, h)),
                    Response::Status => self.done = true,
                    _ => {}
                }
            }
        }
    }
}
async fn probe(input: &mut crate::tty::Input) -> Result<Option<Graphics>> {
    use std::io::Write;
    let tmux = std::env::var_os("TMUX").is_some();
    let query = Parser::query(
        tmux,
        QueryStdioOptions {
            kitty_compression: true,
            blacklist_protocols: vec![ProtocolType::Sixel],
            ..Default::default()
        },
    );
    std::io::stdout().write_all(query.as_bytes())?;
    std::io::stdout().flush()?;
    let mut probe = Probe::default();
    let mut bytes = [0u8; 1024];
    let timeout = tokio::time::sleep(Duration::from_millis(500));
    tokio::pin!(timeout);
    while !probe.done {
        tokio::select! {
            _=&mut timeout=>break,
            result=input.read(&mut bytes)=> {let n=result?;if n==0{break;}probe.feed(&bytes[..n]);},
        }
    }
    if !probe.sequence.starts_with(b"\x1b_G") {
        probe.keys.append(&mut probe.sequence);
    }
    input.unread(&probe.keys);
    Ok(probe.kitty.then(|| Graphics {
        font: probe.font.unwrap_or((10, 20)).into(),
        tmux,
        compress: probe.compress,
    }))
}

pub struct RenderedImage {
    pub protocol: Protocol,
    pub id: u32,
    pub tmux: bool,
}
impl RenderedImage {
    pub fn delete(&self, out: &mut impl std::io::Write) -> std::io::Result<()> {
        delete_image(out, self.id, self.tmux)
    }
}
pub fn delete_image(out: &mut impl std::io::Write, id: u32, tmux: bool) -> std::io::Result<()> {
    let (start, esc, end) = ratatui_image::picker::cap_parser::Parser::tmux_start_escape_end(tmux);
    write!(out, "{start}{esc}_Ga=d,d=I,i={id},q=2;{esc}\\{end}")
}
#[derive(Default)]
pub struct Frame {
    pub text: String,
    pub image: Option<Arc<RenderedImage>>,
}
impl Frame {
    fn text(text: String) -> Self {
        Self { text, image: None }
    }
}

pub struct Preview {
    pub frame: watch::Receiver<Frame>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Preview {
    pub fn start(
        socket: &str,
        target: Option<Target>,
        width: u16,
        height: u16,
        options: Options,
    ) -> Self {
        let (tx, frame) = watch::channel(Frame::text(if target.is_some() {
            "Loading preview…".into()
        } else {
            "Select a terminal block to preview.".into()
        }));
        let (stop, mut cancelled) = oneshot::channel();
        let socket = socket.to_owned();
        let task = tokio::task::spawn_local(async move {
            let Some(target) = target else {
                let _ = cancelled.await;
                return;
            };
            // Avoid opening streams for every intermediate row during navigation.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(120)) => {},
                _ = &mut cancelled => return,
            }
            if let Err(error) = worker(
                &socket,
                &target,
                (width, height),
                options,
                &tx,
                &mut cancelled,
            )
            .await
            {
                tx.send_replace(Frame::text(format!("Preview unavailable: {error:#}")));
            }
        });
        Self {
            frame,
            stop: Some(stop),
            task: Some(task),
        }
    }

    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        // The local task handles cancellation and releases its attachment.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

async fn worker(
    socket: &str,
    target: &Target,
    size: (u16, u16),
    options: Options,
    tx: &watch::Sender<Frame>,
    cancelled: &mut oneshot::Receiver<()>,
) -> Result<()> {
    let mut control = tokio::select! {
        result = Control::connect(socket) => result?,
        _ = &mut *cancelled => return Ok(()),
    };
    // Cancellation abandons this dedicated connection, never a reused RPC stream.
    tokio::select! {
        result = crate::session::with_session(&mut control, &target.session, async |control, _| {
            render(control, socket, target, size, options, tx).await
        }) => result,
        _ = &mut *cancelled => Ok(()),
    }
}

async fn render(
    control: &mut Control,
    socket: &str,
    target: &Target,
    size: (u16, u16),
    options: Options,
    tx: &watch::Sender<Frame>,
) -> Result<()> {
    let (width, height) = size;
    let (mut data, mut state) = crate::session::open_terminal(control, socket, target).await?;
    let mut tick = tokio::time::interval(Duration::from_millis(if options.images() {
        400
    } else {
        100
    }));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = false;
    let deadline = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(deadline);
    let mut last_grid = None;
    static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    let id = (std::process::id().wrapping_mul(2654435761)
        ^ NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    .max(1);
    loop {
        tokio::select! {
            bytes = stream::next_binary(&mut data) => { state.feed(&bytes?)?; dirty = true; },
            _ = &mut deadline, if !state.is_ready() => anyhow::bail!("snapshot restoration timed out"),
            _ = tick.tick(), if dirty && state.is_ready() => {
                if state.render_held(std::time::Instant::now())? { continue; }
                let text = terminal::frame(state.terminal_mut()?,width,height,false)?;
                if options.images() && width>0 && height>0 {
                    let grid=crate::raster::Grid::capture(state.terminal()?)?;
                    if last_grid.as_ref()!=Some(&grid) {
                        // A cancelled spawn_blocking job keeps its permit until
                        // encoding finishes. Selection changes cannot queue more.
                        let Some(permit) = options.image_jobs.as_ref().and_then(|jobs| jobs.clone().try_acquire_owned().ok()) else { continue; };
                        last_grid=Some(grid.clone());
                        let encode_options=options.clone();
                        let image=tokio::task::spawn_blocking(move|| { let _permit = permit; encode_options.encode(&grid,width,height,id) }).await??;
                        tx.send_replace(Frame {text,image:Some(Arc::new(image))});
                    }
                } else if tx.borrow().text != text { tx.send_replace(Frame::text(text)); }
                dirty = false;
            }
            value = control.next_event() => { value?; },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn probe_handles_fragmented_replies_and_preserves_typed_keys() {
        let data = b"j\x1b[B\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;OK\x1b\\\x1b[6;20;10t\x1b[0nl";
        for split in 0..=data.len() {
            let mut probe = Probe::default();
            probe.feed(&data[..split]);
            probe.feed(&data[split..]);
            assert!(probe.kitty && probe.compress && probe.done);
            assert_eq!(probe.font, Some((10, 20)));
            assert_eq!(probe.keys, b"j\x1b[Bl");
        }
        let mut probe = Probe::default();
        probe.feed(b"\x1b_Gi=31;ENOTSUP\x1b\\\x1b[0n");
        assert!(!probe.kitty);
        assert!(probe.keys.is_empty());
        assert!(probe.done);
    }
    #[test]
    fn kitty_widget_transmits_pixels_once_and_deletes_only_its_image() -> Result<()> {
        use ratatui::{
            buffer::Buffer,
            layout::{Rect, Size},
            widgets::Widget,
        };
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            4,
            2,
            image::Rgba([255, 0, 0, 255]),
        ));
        let protocol = Protocol::Kitty(Kitty::new(image, Size::new(2, 1), 1234, false, false)?);
        let area = Rect::new(3, 2, 2, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 5));
        ratatui_image::Image::new(&protocol).render(area, &mut buffer);
        let encoded = buffer[(3, 2)].symbol();
        assert!(encoded.contains("i=1234,a=T,U=1,f=32"));
        let payload = encoded
            .split(';')
            .nth(1)
            .unwrap()
            .split("\x1b\\")
            .next()
            .unwrap();
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(payload)?;
        assert_eq!(bytes.len(), 4 * 2 * 4);
        assert_eq!(&bytes[..4], &[255, 0, 0, 255]);
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        let mut second = Buffer::empty(Rect::new(0, 0, 10, 5));
        ratatui_image::Image::new(&protocol).render(area, &mut second);
        assert!(!second[(3, 2)].symbol().contains("a=T"));
        let mut deleted = Vec::new();
        delete_image(&mut deleted, 1234, false)?;
        assert_eq!(deleted, b"\x1b_Ga=d,d=I,i=1234,q=2;\x1b\\");
        let mut tmux = Vec::new();
        delete_image(&mut tmux, 1234, true)?;
        assert!(tmux.starts_with(b"\x1bPtmux;\x1b\x1b_G"));
        Ok(())
    }
}
