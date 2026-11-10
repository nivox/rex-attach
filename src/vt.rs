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

pub struct Terminal(Owned<ffi::TerminalImpl>);
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
        let result = Self(Owned::create(
            |out| {
                // SAFETY: out is an initialized handle slot; dimensions are scalar values.
                unsafe {
                    ffi::ghostty_terminal_new(std::ptr::null(), out, options.cols, options.rows)
                }
            },
            ffi::ghostty_terminal_free,
        )?);
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
        Ok(result)
    }
    pub(crate) fn raw(&self) -> ffi::Terminal {
        self.0.raw()
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
