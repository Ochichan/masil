//! Own terminal modes and bound writes even when the host stops consuming output.
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    fs::{File, OpenOptions},
    io::{self, IsTerminal, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    time::{Duration, Instant},
};

const ENTER: &[u8] =
    b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h";
const LEAVE: &[u8] =
    b"\x1b[?2004l\x1b[?1004l\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?25h\x1b[?1049l";

pub(super) struct TtyWriter(File);
impl Write for TtyWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_bounded(bytes, Duration::from_secs(2))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl TtyWriter {
    fn write_bounded(&mut self, bytes: &[u8], budget: Duration) -> io::Result<usize> {
        let end = Instant::now() + budget;
        loop {
            match self.0.write(bytes) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = end.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "terminal output stalled",
                        ));
                    }
                    let mut fd = libc::pollfd {
                        fd: self.0.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    // This descriptor belongs to the UI, opened independently of
                    // stdout, so its nonblocking flag cannot leak into the shell.
                    let ready =
                        unsafe { libc::poll(&mut fd, 1, remaining.as_millis().min(100) as i32) };
                    if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
                value => return value,
            }
            if Instant::now() >= end {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "terminal output stalled",
                ));
            }
        }
    }
}

struct Guard {
    writer: TtyWriter,
    raw: bool,
}
impl Drop for Guard {
    fn drop(&mut self) {
        if self.raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        // A single bounded restoration path covers partial setup, draw errors,
        // unwind, normal close, SIGINT, SIGTERM and SIGHUP.
        let end = Instant::now() + Duration::from_millis(200);
        let mut remaining = LEAVE;
        while !remaining.is_empty() && Instant::now() < end {
            match self
                .writer
                .write_bounded(remaining, end.saturating_duration_since(Instant::now()))
            {
                Ok(0) | Err(_) => break,
                Ok(n) => remaining = &remaining[n..],
            }
        }
    }
}

pub(super) struct Session {
    terminal: Option<Terminal<CrosstermBackend<TtyWriter>>>,
    _guard: Guard,
}
impl Session {
    pub fn open() -> Result<Self, String> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err("ui needs a terminal on stdin and stdout".into());
        }
        let mut name = [0i8; 1024];
        let result = unsafe { libc::ttyname_r(libc::STDOUT_FILENO, name.as_mut_ptr(), name.len()) };
        if result != 0 {
            return Err("cannot identify the UI terminal".into());
        }
        let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
            .to_str()
            .map_err(|_| "invalid terminal path")?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(name)
            .map_err(|e| e.to_string())?;
        let copy = file.try_clone().map_err(|e| e.to_string())?;
        let mut guard = Guard {
            writer: TtyWriter(file),
            raw: false,
        };
        crossterm::terminal::enable_raw_mode().map_err(|e| e.to_string())?;
        guard.raw = true;
        guard.writer.write_all(ENTER).map_err(|e| e.to_string())?;
        let terminal =
            Terminal::new(CrosstermBackend::new(TtyWriter(copy))).map_err(|e| e.to_string())?;
        Ok(Self {
            terminal: Some(terminal),
            _guard: guard,
        })
    }
    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<TtyWriter>> {
        self.terminal.as_mut().expect("active terminal")
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        drop(self.terminal.take());
    }
}
