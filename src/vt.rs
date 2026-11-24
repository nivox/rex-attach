//! The application's native Ghostty surface. Own handles directly rather than
//! depending on private layouts or maintaining a fork of the Rust wrapper.
use anyhow::{Result, bail, ensure};
use ghostty_vt_sys as ffi;
use std::{ffi::c_void, marker::PhantomData, mem::MaybeUninit, ptr::NonNull, rc::Rc};

#[cfg(test)]
pub use ghostty_vt::TerminalOptions;
pub use ghostty_vt::{alloc, style};
pub mod graphics;
pub mod render;
pub mod snapshot;
pub use render::RenderState;
pub mod terminal {
    pub use ghostty_vt::terminal::{Mode, ScrollViewport};
}
pub mod screen {
    pub use ghostty_vt::screen::{CellWide, Screen};
}
pub mod kitty {
    pub mod graphics {
        pub use super::super::graphics::*;
    }
}

fn check(code: ffi::Result::Type) -> Result<()> {
    if code == ffi::Result::SUCCESS {
        Ok(())
    } else {
        bail!("Ghostty operation failed ({code})")
    }
}

/// Each handle is owned once, uses the native default allocator, and stays on
/// its creating thread. Borrowed cursors never own or free their source data.
struct Owned<T> {
    ptr: NonNull<T>,
    free: unsafe extern "C" fn(*mut T),
    local: PhantomData<Rc<()>>,
}
impl<T> Owned<T> {
    fn create(
        create: impl FnOnce(*mut *mut T) -> ffi::Result::Type,
        free: unsafe extern "C" fn(*mut T),
    ) -> Result<Self> {
        let mut raw = std::ptr::null_mut();
        check(create(&mut raw))?;
        Ok(Self {
            ptr: NonNull::new(raw)
                .ok_or_else(|| anyhow::anyhow!("Ghostty returned a null handle"))?,
            free,
            local: PhantomData,
        })
    }
    fn raw(&self) -> *mut T {
        self.ptr.as_ptr()
    }
}
impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner and no cursor can outlive its borrow.
        unsafe { (self.free)(self.raw()) };
    }
}

/// # Safety
/// The caller must pair the native data tag with its documented output type.
unsafe fn query<T>(read: impl FnOnce(*mut c_void) -> ffi::Result::Type) -> Result<T> {
    let mut value = MaybeUninit::<T>::uninit();
    check(read(value.as_mut_ptr().cast()))?;
    // SAFETY: the typed tag contract requires initialization on success.
    Ok(unsafe { value.assume_init() })
}

pub struct Terminal {
    // The native handle must drop before the queue its userdata points at.
    handle: Owned<ffi::TerminalImpl>,
    events: Option<Box<std::cell::RefCell<Vec<Event>>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardLocation {
    Standard,
    Selection,
    Primary,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardWrite {
    pub location: ClipboardLocation,
    pub text: Vec<u8>,
}
impl ClipboardWrite {
    /// The OSC 52 sequence that makes the outer terminal perform this write.
    pub fn osc52(&self) -> String {
        use base64::Engine;
        let target = match self.location {
            ClipboardLocation::Standard => 'c',
            ClipboardLocation::Selection => 's',
            ClipboardLocation::Primary => 'p',
        };
        let data = base64::engine::general_purpose::STANDARD.encode(&self.text);
        format!("\x1b]52;{target};{data}\x1b\\")
    }
}
const CLIPBOARD_LIMIT: usize = 1024 * 1024;
const HYPERLINK_LIMIT: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Clipboard(ClipboardWrite),
    Bell,
    TitleChanged,
    PwdChanged,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Forward {
    pub clipboard: bool,
    pub title: bool,
}
macro_rules! terminal_values {
    ($($name:ident:$ty:ty=>$tag:ident),*$(,)?)=>{$(
        pub fn $name(&self)->Result<$ty> {
            // SAFETY: each pair below matches GhosttyTerminalData's C contract.
            unsafe{query(|out|ffi::ghostty_terminal_get(self.raw(),ffi::TerminalData::$tag,out))}
        }
    )*};
}
impl Terminal {
    #[cfg(test)]
    pub fn new(options: TerminalOptions) -> Result<Self> {
        let result = Self {
            handle: Owned::create(
                |out| {
                    // SAFETY: out is an initialized handle slot; dimensions are scalar values.
                    unsafe {
                        ffi::ghostty_terminal_new(std::ptr::null(), out, options.cols, options.rows)
                    }
                },
                ffi::ghostty_terminal_free,
            )?,
            events: None,
        };
        result.set(
            ffi::TerminalOption::SCROLLBACK_MAX_BYTES,
            &options.max_scrollback,
        )?;
        let mode = ffi::TerminalModeConfig {
            mode: terminal::Mode::GRAPHEME_CLUSTER.0,
            value: true,
        };
        result.set(ffi::TerminalOption::MODE_DEFAULT, &mode)?;
        // No input/query callbacks are installed: Rex alone answers the program.
        // Event forwarding is opt-in through enable_events.
        Ok(result)
    }
    pub(crate) fn raw(&self) -> ffi::Terminal {
        self.handle.raw()
    }
    fn install(&self, tag: ffi::TerminalOption::Type, ptr: *const c_void) -> Result<()> {
        // SAFETY: callers pass the documented callback/userdata pointer for tag.
        check(unsafe { ffi::ghostty_terminal_set(self.raw(), tag, ptr) })
    }
    /// Queue decoded terminal events (bell, pwd, optional title/clipboard) for the caller.
    pub fn enable_events(&mut self, forward: Forward) -> Result<()> {
        if self.events.is_some() {
            return Ok(());
        }
        let queue: Box<std::cell::RefCell<Vec<Event>>> = Box::default();
        let userdata = std::ptr::from_ref(&*queue).cast::<c_void>();
        // SAFETY: the boxed queue has a stable address and outlives the native
        // handle (field order); each option receives its documented callback type.
        self.install(ffi::TerminalOption::USERDATA, userdata)?;
        let bell: ffi::TerminalBellFn = Some(on_bell);
        self.install(
            ffi::TerminalOption::BELL,
            bell.map_or(std::ptr::null(), |f| f as *const c_void),
        )?;
        let pwd: ffi::TerminalPwdChangedFn = Some(on_pwd_changed);
        self.install(
            ffi::TerminalOption::PWD_CHANGED,
            pwd.map_or(std::ptr::null(), |f| f as *const c_void),
        )?;
        if forward.title {
            let title: ffi::TerminalTitleChangedFn = Some(on_title_changed);
            self.install(
                ffi::TerminalOption::TITLE_CHANGED,
                title.map_or(std::ptr::null(), |f| f as *const c_void),
            )?;
        }
        if forward.clipboard {
            let clipboard: ffi::TerminalClipboardWriteFn = Some(clipboard_write);
            self.install(
                ffi::TerminalOption::CLIPBOARD_WRITE,
                clipboard.map_or(std::ptr::null(), |f| f as *const c_void),
            )?;
            self.set(
                ffi::TerminalOption::CLIPBOARD_WRITE_MAX_BYTES,
                &CLIPBOARD_LIMIT,
            )?;
        }
        self.events = Some(queue);
        if forward.title && !self.title()?.is_empty() {
            self.events
                .as_ref()
                .unwrap()
                .borrow_mut()
                .push(Event::TitleChanged);
        }
        if !self.pwd()?.is_empty() {
            self.events
                .as_ref()
                .unwrap()
                .borrow_mut()
                .push(Event::PwdChanged);
        }
        Ok(())
    }
    pub fn take_events(&mut self) -> Vec<Event> {
        self.events
            .as_ref()
            .map(|queue| std::mem::take(&mut *queue.borrow_mut()))
            .unwrap_or_default()
    }
    fn string(&self, tag: ffi::TerminalData::Type) -> Result<String> {
        // SAFETY: TITLE/PWD write a borrowed GhosttyString valid until the next mutation.
        let s: ffi::String =
            unsafe { query(|out| ffi::ghostty_terminal_get(self.raw(), tag, out)) }?;
        if s.ptr.is_null() || s.len == 0 {
            return Ok(String::new());
        }
        // SAFETY: non-null GhosttyString points to len valid bytes.
        let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }
    pub fn title(&self) -> Result<String> {
        self.string(ffi::TerminalData::TITLE)
    }
    pub fn pwd(&self) -> Result<String> {
        self.string(ffi::TerminalData::PWD)
    }
    pub fn hyperlink_uri(&self, x: u16, y: u16) -> Result<Option<String>> {
        let point = ffi::Point {
            tag: ffi::PointTag::VIEWPORT,
            value: ffi::PointValue {
                coordinate: ffi::PointCoordinate { x, y: u32::from(y) },
            },
        };
        let mut grid_ref = ffi::GridRef {
            size: size_of::<ffi::GridRef>(),
            node: std::ptr::null_mut(),
            x: 0,
            y: 0,
        };
        // SAFETY: grid_ref is sized and valid for this terminal call.
        if unsafe { ffi::ghostty_terminal_grid_ref(self.raw(), point, &mut grid_ref) }
            != ffi::Result::SUCCESS
        {
            return Ok(None);
        }
        let mut len = 0usize;
        // SAFETY: sizing pass with null buffer writes required length into len.
        let code = unsafe {
            ffi::ghostty_grid_ref_hyperlink_uri(&grid_ref, std::ptr::null_mut(), 0, &mut len)
        };
        if code == ffi::Result::SUCCESS && len == 0 {
            return Ok(None);
        }
        if (code != ffi::Result::OUT_OF_SPACE && code != ffi::Result::SUCCESS)
            || len == 0
            || len > HYPERLINK_LIMIT
        {
            return Ok(None);
        }
        let mut buf = vec![0u8; len];
        // SAFETY: buf has len bytes of capacity for the URI.
        let code = unsafe {
            ffi::ghostty_grid_ref_hyperlink_uri(&grid_ref, buf.as_mut_ptr(), buf.len(), &mut len)
        };
        if code != ffi::Result::SUCCESS || len == 0 || len > buf.len() {
            return Ok(None);
        }
        buf.truncate(len);
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    }
    fn set<T>(&self, tag: ffi::TerminalOption::Type, value: &T) -> Result<()> {
        // SAFETY: private callers provide the C type documented for each tag.
        check(unsafe {
            ffi::ghostty_terminal_set(self.raw(), tag, std::ptr::from_ref(value).cast())
        })
    }
    pub fn vt_write(&mut self, bytes: &[u8]) {
        // SAFETY: the input slice is valid for the synchronous native call.
        unsafe { ffi::ghostty_terminal_vt_write(self.raw(), bytes.as_ptr(), bytes.len()) };
    }
    pub fn set_theme(
        &mut self,
        foreground: style::RgbColor,
        background: style::RgbColor,
        cursor: Option<style::RgbColor>,
        palette: style::Palette,
    ) -> Result<()> {
        for (tag, color) in [
            (ffi::TerminalOption::COLOR_FOREGROUND, Some(foreground)),
            (ffi::TerminalOption::COLOR_BACKGROUND, Some(background)),
            (ffi::TerminalOption::COLOR_CURSOR, cursor),
        ] {
            let color = color.map(ffi::ColorRgb::from);
            let value = color.as_ref().map_or(std::ptr::null(), std::ptr::from_ref);
            // SAFETY: these setters copy a ColorRgb or clear the default on null.
            check(unsafe { ffi::ghostty_terminal_set(self.raw(), tag, value.cast()) })?;
        }
        let palette: [ffi::ColorRgb; 256] = palette.into();
        self.set(ffi::TerminalOption::COLOR_PALETTE, &palette)
    }
    pub fn resize(&mut self, cols: u16, rows: u16, width: u32, height: u32) -> Result<()> {
        // SAFETY: uniquely borrowed live terminal, scalar geometry.
        check(unsafe { ffi::ghostty_terminal_resize(self.raw(), cols, rows, width, height) })
    }
    pub fn mode(&self, mode: terminal::Mode) -> Result<bool> {
        let mut value = ffi::TerminalModeConfig {
            mode: mode.0,
            value: false,
        };
        // SAFETY: MODE is the documented in/out TerminalModeConfig query.
        check(unsafe {
            ffi::ghostty_terminal_get(
                self.raw(),
                ffi::TerminalData::MODE,
                std::ptr::from_mut(&mut value).cast(),
            )
        })?;
        Ok(value.value)
    }
    pub fn scroll_viewport(&mut self, scroll: terminal::ScrollViewport) {
        // SAFETY: the public upstream conversion constructs the C tagged union.
        unsafe { ffi::ghostty_terminal_scroll_viewport(self.raw(), scroll.into()) };
    }
    pub fn active_screen(&self) -> Result<screen::Screen> {
        // SAFETY: ACTIVE_SCREEN writes a TerminalScreen enum value.
        let value: ffi::TerminalScreen::Type = unsafe {
            query(|out| {
                ffi::ghostty_terminal_get(self.raw(), ffi::TerminalData::ACTIVE_SCREEN, out)
            })
        }?;
        value
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid native screen {value}"))
    }
    terminal_values! {
        cols:u16=>COLS, rows:u16=>ROWS,width_px:u32=>WIDTH_PX,height_px:u32=>HEIGHT_PX,
        is_mouse_tracking:bool=>MOUSE_TRACKING,is_viewport_active:bool=>VIEWPORT_ACTIVE,
    }
    #[cfg(test)]
    terminal_values! {
        is_kitty_image_from_file_allowed:bool=>KITTY_IMAGE_MEDIUM_FILE,
        is_kitty_image_from_shared_mem_allowed:bool=>KITTY_IMAGE_MEDIUM_SHARED_MEM,
    }
    #[cfg(test)]
    pub fn is_kitty_image_from_temp_file_allowed(&self) -> Result<bool> {
        // SAFETY: the native temporary-file setting is a string, not a boolean.
        let directory: ffi::String = unsafe {
            query(|out| {
                ffi::ghostty_terminal_get(
                    self.raw(),
                    ffi::TerminalData::KITTY_IMAGE_MEDIUM_TEMP_FILE,
                    out,
                )
            })
        }?;
        Ok(directory.len != 0)
    }
    pub fn set_kitty_image_storage_limit(&mut self, limit: u64) -> Result<&mut Self> {
        self.set(ffi::TerminalOption::KITTY_IMAGE_STORAGE_LIMIT, &limit)?;
        Ok(self)
    }
    pub fn set_apc_max_bytes_kitty(&mut self, limit: Option<usize>) -> Result<&mut Self> {
        // SAFETY: this optional setting takes a size_t pointer or null.
        check(unsafe {
            ffi::ghostty_terminal_set(
                self.raw(),
                ffi::TerminalOption::APC_MAX_BYTES_KITTY,
                limit
                    .as_ref()
                    .map_or(std::ptr::null(), |n| std::ptr::from_ref(n).cast()),
            )
        })?;
        Ok(self)
    }
    pub fn set_kitty_image_from_file_allowed(&mut self, allowed: bool) -> Result<&mut Self> {
        self.set(ffi::TerminalOption::KITTY_IMAGE_MEDIUM_FILE, &allowed)?;
        Ok(self)
    }
    pub fn set_kitty_image_from_shared_mem_allowed(&mut self, allowed: bool) -> Result<&mut Self> {
        self.set(ffi::TerminalOption::KITTY_IMAGE_MEDIUM_SHARED_MEM, &allowed)?;
        Ok(self)
    }
    pub fn set_kitty_image_temp_directory(&mut self, directory: Option<&str>) -> Result<&mut Self> {
        ensure!(
            directory.is_none(),
            "client-side temporary image files are disabled"
        );
        // SAFETY: null is the documented way to disable the temporary medium.
        check(unsafe {
            ffi::ghostty_terminal_set(
                self.raw(),
                ffi::TerminalOption::KITTY_IMAGE_MEDIUM_TEMP_FILE,
                std::ptr::null(),
            )
        })?;
        Ok(self)
    }
    #[cfg(test)]
    pub fn set_continuation_max_bytes(&mut self, limit: usize) -> Result<&mut Self> {
        self.set(ffi::TerminalOption::CONTINUATION_MAX_BYTES, &limit)?;
        Ok(self)
    }
}

unsafe fn push_event(userdata: *mut c_void, event: Event) -> bool {
    if userdata.is_null() {
        return false;
    }
    // SAFETY: userdata is the boxed queue installed by enable_events.
    let queue = unsafe { &*userdata.cast::<std::cell::RefCell<Vec<Event>>>() };
    let Ok(mut queue) = queue.try_borrow_mut() else {
        return false;
    };
    queue.push(event);
    true
}

unsafe extern "C" fn on_bell(_terminal: ffi::Terminal, userdata: *mut c_void) {
    // SAFETY: userdata is the boxed queue installed by enable_events.
    let _ = unsafe { push_event(userdata, Event::Bell) };
}

unsafe extern "C" fn on_title_changed(_terminal: ffi::Terminal, userdata: *mut c_void) {
    // SAFETY: userdata is the boxed queue installed by enable_events.
    let _ = unsafe { push_event(userdata, Event::TitleChanged) };
}

unsafe extern "C" fn on_pwd_changed(_terminal: ffi::Terminal, userdata: *mut c_void) {
    // SAFETY: userdata is the boxed queue installed by enable_events.
    let _ = unsafe { push_event(userdata, Event::PwdChanged) };
}

unsafe extern "C" fn clipboard_write(
    _terminal: ffi::Terminal,
    userdata: *mut c_void,
    write: *const ffi::ClipboardWrite,
) {
    if write.is_null() || userdata.is_null() {
        return;
    }
    // SAFETY: the native call provides a valid request for this callback only.
    let raw = unsafe { &*write };
    if raw.size
        < std::mem::offset_of!(ffi::ClipboardWrite, reply) + size_of::<ffi::ClipboardWriteReplyFn>()
    {
        return;
    }
    let bytes = |s: ffi::String| -> &'static [u8] {
        if s.ptr.is_null() || s.len == 0 {
            &[]
        } else {
            // SAFETY: strings are borrowed and valid for the callback duration,
            // and never escape it (data is copied below).
            unsafe { std::slice::from_raw_parts(s.ptr, s.len) }
        }
    };
    let location = match raw.location {
        ffi::ClipboardLocation::STANDARD => Some(ClipboardLocation::Standard),
        ffi::ClipboardLocation::SELECTION => Some(ClipboardLocation::Selection),
        ffi::ClipboardLocation::PRIMARY => Some(ClipboardLocation::Primary),
        _ => None,
    };
    let contents: &[ffi::ClipboardContent] = if raw.contents.is_null() {
        &[]
    } else {
        // SAFETY: the contents array has contents_len valid entries.
        unsafe { std::slice::from_raw_parts(raw.contents, raw.contents_len) }
    };
    let text = contents
        .iter()
        .find(|c| bytes(c.mime).starts_with(b"text/plain"))
        .map(|c| bytes(c.data))
        .filter(|data| data.len() <= CLIPBOARD_LIMIT);
    let mut result = ffi::ClipboardWriteResult::UNSUPPORTED;
    if let (Some(location), Some(text)) = (location, text) {
        if unsafe {
            push_event(
                userdata,
                Event::Clipboard(ClipboardWrite {
                    location,
                    text: text.to_vec(),
                }),
            )
        } {
            result = ffi::ClipboardWriteResult::SUCCESS;
        } else {
            result = ffi::ClipboardWriteResult::BUSY;
        }
    }
    let reply = ffi::ClipboardWriteReply {
        size: size_of::<ffi::ClipboardWriteReply>(),
        result,
        remember: false,
    };
    if let Some(reply_fn) = raw.reply {
        // SAFETY: replying inside the callback with the request is the contract.
        unsafe { reply_fn(write, &raw const reply) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal() -> Terminal {
        Terminal::new(TerminalOptions {
            cols: 20,
            rows: 4,
            max_scrollback: 0,
        })
        .unwrap()
    }

    #[test]
    fn osc52_is_queued_only_when_enabled() {
        let mut off = terminal();
        off.vt_write(b"\x1b]52;c;aGVsbG8=\x1b\\");
        assert!(off.take_events().is_empty());

        let mut on = terminal();
        on.enable_events(Forward {
            clipboard: true,
            title: true,
        })
        .unwrap();
        on.vt_write(b"\x1b]52;c;aGVsbG8=\x1b\\\x1b]52;p;aGk=\x07");
        let writes: Vec<_> = on
            .take_events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Clipboard(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].location, ClipboardLocation::Standard);
        assert_eq!(writes[0].text, b"hello");
        assert_eq!(writes[0].osc52(), "\x1b]52;c;aGVsbG8=\x1b\\");
        assert_eq!(writes[1].location, ClipboardLocation::Primary);
        assert!(on.take_events().is_empty());
    }

    #[test]
    fn bell_title_pwd_and_seeding() {
        let mut term = terminal();
        term.vt_write(b"\x1b]2;pre-title\x07");
        term.enable_events(Forward {
            clipboard: false,
            title: true,
        })
        .unwrap();
        assert!(term.take_events().contains(&Event::TitleChanged));
        assert_eq!(term.title().unwrap(), "pre-title");

        term.vt_write(b"\x1b]2;hello\x07\x07\x1b]7;file://host/tmp\x1b\\");
        let events = term.take_events();
        assert!(events.contains(&Event::TitleChanged));
        assert!(events.contains(&Event::Bell));
        assert!(events.contains(&Event::PwdChanged));
        assert_eq!(term.title().unwrap(), "hello");
        assert!(term.pwd().unwrap().contains("/tmp"));

        let mut no_title = terminal();
        no_title
            .enable_events(Forward {
                clipboard: false,
                title: false,
            })
            .unwrap();
        no_title.vt_write(b"\x1b]2;ignored\x07");
        assert!(!no_title.take_events().contains(&Event::TitleChanged));
    }

    #[test]
    fn hyperlink_uri_lookup() {
        let mut term = terminal();
        term.vt_write(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ plain");
        assert_eq!(
            term.hyperlink_uri(0, 0).unwrap().as_deref(),
            Some("https://example.com")
        );
        assert_eq!(term.hyperlink_uri(5, 0).unwrap(), None);
    }
}
