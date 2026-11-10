use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;

pub struct Input(
    tokio::io::unix::AsyncFd<File>,
    std::collections::VecDeque<u8>,
);

impl Input {
    pub fn open() -> io::Result<Self> {
        let file = open_input(std::path::Path::new("/dev/tty"))?;
        // SAFETY: this registration owns the independently opened nonblocking
        // descriptor, and all reads go through readiness checks below.
        let fd = unsafe { tokio::io::unix::AsyncFd::register(file) }.map_err(io::Error::other)?;
        Ok(Self(fd, Default::default()))
    }

    pub fn unread(&mut self, bytes: &[u8]) {
        self.1.extend(bytes);
    }

    pub async fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if !self.1.is_empty() {
            let n = bytes.len().min(self.1.len());
            for byte in &mut bytes[..n] {
                *byte = self.1.pop_front().unwrap();
            }
            return Ok(n);
        }
        loop {
            let mut ready = self.0.readable_mut().await?;
            match ready.try_io(|inner| std::io::Read::read(inner.get_mut(), bytes)) {
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Ok(result) => return result,
                Err(_) => continue,
            }
        }
    }
}

fn open_input(path: &std::path::Path) -> io::Result<File> {
    // Opening, rather than dup'ing stdin, creates a separate file description.
    // Setting O_NONBLOCK on stdin can also make stdout nonblocking when the
    // shell connected both descriptors to the same terminal file description.
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
}
pub fn size() -> io::Result<(u16, u16)> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: ioctl initializes a winsize on success.
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, size.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let size = unsafe { size.assume_init() };
    Ok((size.ws_col.max(1), size.ws_row.max(1)))
}
pub struct Guard {
    termios: libc::termios,
}
impl Guard {
    pub fn enter() -> io::Result<Self> {
        let mut termios = std::mem::MaybeUninit::uninit();
        // SAFETY: tcgetattr initializes termios on success; subsequent calls use valid values.
        if unsafe { libc::tcgetattr(0, termios.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let termios = unsafe { termios.assume_init() };
        let guard = Self { termios };
        let mut raw = termios;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } < 0 {
            return Err(io::Error::last_os_error());
        }
        io::stdout().write_all(b"\x1b[?1049h\x1b[?7l")?;
        io::stdout().flush()?;
        Ok(guard)
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = io::stdout().write_all(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2026l\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1049l");
        let _ = io::stdout().flush();
        // SAFETY: restore the saved terminal attributes.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.termios);
        }
    }
}

/// The outer TTY reports SGR cells; Rex owns the remote application's encoding.
pub struct MouseGuard(Option<u16>);
impl MouseGuard {
    pub fn enter(motion: bool) -> io::Result<Self> {
        let mut guard = Self(None);
        guard.set(motion)?;
        Ok(guard)
    }
    pub fn set(&mut self, motion: bool) -> io::Result<()> {
        self.set_mode(if motion { 1003 } else { 1000 })
    }
    pub fn track(&mut self, terminal: &ghostty_vt::Terminal) -> anyhow::Result<()> {
        use ghostty_vt::terminal::Mode;
        self.set_mode(if terminal.mode(Mode::ANY_MOUSE)? {
            1003
        } else if terminal.mode(Mode::BUTTON_MOUSE)? {
            1002
        } else {
            1000
        })?;
        Ok(())
    }
    pub fn resume(&mut self, terminal: &ghostty_vt::Terminal) -> anyhow::Result<()> {
        self.0 = None;
        self.track(terminal)
    }
    fn set_mode(&mut self, mode: u16) -> io::Result<()> {
        if self.0 == Some(mode) {
            return Ok(());
        }
        let mut out = io::stdout().lock();
        out.write_all(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h")?;
        write!(out, "\x1b[?{mode}h")?;
        out.flush()?;
        self.0 = Some(mode);
        Ok(())
    }
}
impl Drop for MouseGuard {
    fn drop(&mut self) {
        let _ = io::stdout().write_all(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l");
        let _ = io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::time::{Duration, Instant};

    #[test]
    fn nonblocking_input_does_not_affect_output_under_backpressure() -> io::Result<()> {
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: valid output pointers; null parameters request default PTY settings.
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty returned two uniquely owned descriptors.
        let mut master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let mut name = [0u8; 1024];
        // SAFETY: the name buffer is writable for its length.
        let error =
            unsafe { libc::ttyname_r(slave.as_raw_fd(), name.as_mut_ptr().cast(), name.len()) };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        let name = std::ffi::CStr::from_bytes_until_nul(&name)
            .unwrap()
            .to_str()
            .unwrap();
        let input = open_input(std::path::Path::new(name))?;
        // dup intentionally models shells sharing stdin/stdout file descriptions.
        let mut output = slave.try_clone()?;
        let flags = |file: &File| unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags(&input) & libc::O_NONBLOCK, 0);
        assert_eq!(flags(&slave) & libc::O_NONBLOCK, 0);
        assert_eq!(flags(&output) & libc::O_NONBLOCK, 0);

        // Write substantially more than the PTY output buffer can hold while
        // the receiver drains it. A nonblocking output would fail with EAGAIN.
        const BYTES: usize = 256 * 1024;
        let writer = std::thread::spawn(move || output.write_all(&vec![b'x'; BYTES]));
        if unsafe {
            libc::fcntl(
                master.as_raw_fd(),
                libc::F_SETFL,
                flags(&master) | libc::O_NONBLOCK,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut received = 0;
        let mut buffer = [0u8; 4096];
        while received < BYTES {
            assert!(
                Instant::now() < deadline,
                "PTY output stalled after {received} bytes"
            );
            match master.read(&mut buffer) {
                Ok(0) => panic!("PTY output ended early"),
                Ok(n) => received += n,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(error) => return Err(error),
            }
        }
        writer.join().unwrap()?;
        Ok(())
    }
}
