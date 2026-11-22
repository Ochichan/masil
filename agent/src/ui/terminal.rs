//! Own terminal modes and bound writes even when the host stops consuming output.
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    fs::{File, OpenOptions},
    io::{self, IsTerminal, Write},
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, RawFd},
        unix::fs::OpenOptionsExt,
    },
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const ENTER: &[u8] =
    b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h";
const LEAVE: &[u8] =
    b"\x1b[?2004l\x1b[?1004l\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?25h\x1b[?1049l";
/// DEC Mode 2026 Synchronized Output markers (BSU / ESU). Supporting terminals
/// buffer the frame until ESU; terminals that do not support it ignore CSI ?2026.
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";

struct TtyIdentity {
    device: libc::dev_t,
    inode: libc::ino_t,
    special_device: libc::dev_t,
    session: libc::pid_t,
    #[cfg(target_os = "linux")]
    terminal_device: libc::c_uint,
}

fn tty_identity(fd: RawFd, description: &str) -> Result<TtyIdentity, String> {
    if unsafe { libc::fcntl(fd, libc::F_GETFL) } < 0 {
        return Err(format!(
            "cannot inspect {description}: {}",
            io::Error::last_os_error()
        ));
    }
    let mut status = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, status.as_mut_ptr()) } != 0 {
        return Err(format!(
            "cannot identify {description}: {}",
            io::Error::last_os_error()
        ));
    }
    let status = unsafe { status.assume_init() };
    if status.st_mode & libc::S_IFMT != libc::S_IFCHR {
        return Err(format!("{description} is not a character terminal"));
    }
    let session = unsafe { libc::tcgetsid(fd) };
    if session <= 0 {
        return Err(format!(
            "cannot identify the session for {description}: {}",
            io::Error::last_os_error()
        ));
    }
    #[cfg(target_os = "linux")]
    let terminal_device = {
        let mut device = 0;
        if unsafe { libc::ioctl(fd, libc::TIOCGDEV, &mut device) } != 0 {
            return Err(format!(
                "cannot identify the device for {description}: {}",
                io::Error::last_os_error()
            ));
        }
        device
    };
    Ok(TtyIdentity {
        device: status.st_dev,
        inode: status.st_ino,
        special_device: status.st_rdev,
        session,
        #[cfg(target_os = "linux")]
        terminal_device,
    })
}

fn same_terminal(left: &TtyIdentity, right: &TtyIdentity) -> bool {
    if left.session != right.session {
        return false;
    }
    let same_node = left.device == right.device
        && left.inode == right.inode
        && left.special_device == right.special_device;
    #[cfg(target_os = "linux")]
    {
        same_node || left.terminal_device == right.terminal_device
    }
    #[cfg(not(target_os = "linux"))]
    {
        // A session can have only one controlling terminal. This also handles
        // /dev/tty, whose node identity may differ from the underlying tty.
        same_node || left.session == right.session
    }
}

fn open_terminal(path: &str) -> Result<File, String> {
    let open = |path| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
    };
    match open(path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => open("/dev/tty")
            .map_err(|fallback| format!("cannot open the controlling terminal: {fallback}")),
        Err(error) => Err(format!("cannot open the UI terminal: {error}")),
    }
}

/// Bytes held before a write to the terminal is forced.
const PENDING_LIMIT: usize = 64 * 1024;

/// Writes to the terminal. `write` collects a frame's many small pieces and
/// `flush` sends them in bounded writes, so a repaint is a few system calls.
pub(super) struct TtyWriter {
    file: File,
    pending: Vec<u8>,
    /// Reusable flush buffer swapped with `pending` before writing so a
    /// stalled terminal can never see the same frame twice while avoiding
    /// per-frame heap allocation.
    flushing: Vec<u8>,
    sync_frames: bool,
    in_sync_frame: bool,
}
impl Write for TtyWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.sync_frames && !self.in_sync_frame {
            self.in_sync_frame = true;
            self.pending.extend_from_slice(SYNC_BEGIN);
        }
        if self.pending.len() + bytes.len() > PENDING_LIMIT {
            self.flush_pending()?;
        }
        self.pending.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.in_sync_frame {
            self.in_sync_frame = false;
            self.pending.extend_from_slice(SYNC_END);
        }
        self.flush_pending()
    }
}
impl TtyWriter {
    fn new(file: File) -> Self {
        Self {
            file,
            pending: Vec::new(),
            flushing: Vec::new(),
            sync_frames: false,
            in_sync_frame: false,
        }
    }

    fn new_framed(file: File) -> Self {
        Self {
            sync_frames: true,
            ..Self::new(file)
        }
    }

    fn flush_pending(&mut self) -> io::Result<()> {
        // Swapped first: a stalled terminal must not see this frame again.
        self.flushing.clear();
        std::mem::swap(&mut self.pending, &mut self.flushing);
        if self.flushing.is_empty() {
            return Ok(());
        }
        let result = (|| {
            let mut offset = 0;
            while offset < self.flushing.len() {
                let written = write_bounded(
                    &mut self.file,
                    &self.flushing[offset..],
                    Duration::from_secs(2),
                )?;
                if written == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                offset += written;
            }
            Ok(())
        })();
        self.flushing.clear();
        if self.flushing.capacity() > PENDING_LIMIT {
            self.flushing.shrink_to(PENDING_LIMIT);
        }
        result
    }

    /// One write straight to the terminal, past anything pending.
    fn write_bounded(&mut self, bytes: &[u8], budget: Duration) -> io::Result<usize> {
        write_bounded(&mut self.file, bytes, budget)
    }
}

fn write_bounded(file: &mut File, bytes: &[u8], budget: Duration) -> io::Result<usize> {
    let end = Instant::now() + budget;
    loop {
        match file.write(bytes) {
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
                    fd: file.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // This descriptor belongs to the UI, opened independently of
                // stdout, so its nonblocking flag cannot leak into the shell.
                let ready =
                    unsafe { libc::poll(&mut fd, 1, remaining.as_millis().min(100) as i32) };
                if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
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

/// Leaves the alternate screen, shows the cursor and turns mouse, focus and
/// bracketed-paste reporting off, within a bounded time. Returns whether
/// the whole sequence was written.
fn leave(writer: &mut TtyWriter) -> bool {
    let end = Instant::now() + Duration::from_millis(200);
    let mut remaining = LEAVE;
    while !remaining.is_empty() && Instant::now() < end {
        match writer.write_bounded(remaining, end.saturating_duration_since(Instant::now())) {
            Ok(0) | Err(_) => break,
            Ok(n) => remaining = &remaining[n..],
        }
    }
    remaining.is_empty()
}

struct Guard {
    writer: TtyWriter,
    raw: bool,
    /// Set by the panic hook. The desk loops read it to close.
    panicked: Arc<AtomicBool>,
    /// Set once the whole leave sequence has been written, by this guard or
    /// by the panic hook. Until then the other one tries again.
    left: Arc<AtomicBool>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        if self.left.load(Ordering::SeqCst) {
            return;
        }
        if self.raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        // A single bounded restoration path covers partial setup, draw errors,
        // unwind, normal close, SIGINT, SIGTERM and SIGHUP. It also runs
        // after a panic hook whose write stalled.
        if leave(&mut self.writer) {
            self.left.store(true, Ordering::SeqCst);
        }
    }
}

/// What the panic hook does: it records the panic and gives the terminal
/// back unless that is done already. `left` is set only after a whole
/// write, so the guard restores again when this write stalls.
fn give_back(writer: &Mutex<TtyWriter>, panicked: &AtomicBool, left: &AtomicBool) {
    panicked.store(true, Ordering::SeqCst);
    if left.load(Ordering::SeqCst) {
        return;
    }
    let _ = crossterm::terminal::disable_raw_mode();
    if let Ok(mut writer) = writer.try_lock()
        && leave(&mut writer)
    {
        left.store(true, Ordering::SeqCst);
    }
}

/// Gives the terminal back before a panic message is printed. Without this
/// the text goes to the alternate screen and is gone when the guard leaves
/// it. The hook stays installed and writes nothing once `left` is set.
fn restore_on_panic(file: File, panicked: Arc<AtomicBool>, left: Arc<AtomicBool>) {
    let writer = Mutex::new(TtyWriter::new(file));
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        give_back(&writer, &panicked, &left);
        previous(info);
    }));
}

pub(super) struct Session {
    terminal: Option<Terminal<CrosstermBackend<TtyWriter>>>,
    guard: Guard,
}
impl Session {
    pub fn open() -> Result<Self, String> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err("ui needs a terminal on stdin and stdout".into());
        }
        // c_char is i8 on some targets and u8 on others (Linux aarch64).
        let mut name: [libc::c_char; 1024] = [0; 1024];
        let result = unsafe { libc::ttyname_r(libc::STDOUT_FILENO, name.as_mut_ptr(), name.len()) };
        if result != 0 {
            return Err("cannot identify the UI terminal".into());
        }
        let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
            .to_str()
            .map_err(|_| "invalid terminal path")?;
        let stdin = tty_identity(libc::STDIN_FILENO, "UI stdin terminal")?;
        let stdout = tty_identity(libc::STDOUT_FILENO, "UI stdout terminal")?;
        if !same_terminal(&stdin, &stdout) {
            return Err("ui stdin and stdout refer to different terminals".into());
        }
        let process_session = unsafe { libc::getsid(0) };
        if process_session <= 0 {
            return Err(format!(
                "cannot identify the UI process session: {}",
                io::Error::last_os_error()
            ));
        }
        if stdin.session != process_session || stdout.session != process_session {
            return Err("ui stdin and stdout are not the controlling terminal".into());
        }
        let file = open_terminal(name)?;
        let opened = tty_identity(file.as_raw_fd(), "opened UI terminal")?;
        if !same_terminal(&stdout, &opened) || opened.session != process_session {
            return Err("opened UI terminal does not match stdin and stdout".into());
        }
        let copy = file.try_clone().map_err(|e| e.to_string())?;
        let for_panic = file.try_clone().map_err(|e| e.to_string())?;
        let mut guard = Guard {
            writer: TtyWriter::new(file),
            raw: false,
            panicked: Arc::new(AtomicBool::new(false)),
            left: Arc::new(AtomicBool::new(false)),
        };
        crossterm::terminal::enable_raw_mode().map_err(|e| e.to_string())?;
        guard.raw = true;
        guard.writer.write_all(ENTER).map_err(|e| e.to_string())?;
        guard.writer.flush().map_err(|e| e.to_string())?;
        restore_on_panic(for_panic, guard.panicked.clone(), guard.left.clone());
        let terminal = Terminal::new(CrosstermBackend::new(TtyWriter::new_framed(copy)))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            terminal: Some(terminal),
            guard,
        })
    }
    pub fn draw(&mut self, f: impl FnOnce(&mut ratatui::Frame)) -> io::Result<()> {
        self.terminal
            .as_mut()
            .expect("active terminal")
            .draw(f)
            .map(|_| ())
    }
    /// Whether the panic hook ran. A panic in a background task does not
    /// unwind the desk, which must then stop drawing and close.
    pub fn panicked(&self) -> bool {
        self.guard.panicked.load(Ordering::SeqCst)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        drop(self.terminal.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private file standing in for the terminal, and its path.
    fn scratch(name: &str) -> (File, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("masil-ui-terminal-{}-{name}", std::process::id()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        (file, path)
    }

    #[test]
    fn writes_reach_the_terminal_on_flush() {
        let (file, path) = scratch("flush");
        let mut writer = TtyWriter::new(file);
        for piece in ["\x1b[1;1H", "a", "\x1b[1;2H", "b"] {
            writer.write_all(piece.as_bytes()).unwrap();
        }
        assert!(std::fs::read(&path).unwrap().is_empty());
        writer.flush().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"\x1b[1;1Ha\x1b[1;2Hb");
        // Nothing is sent twice, and more than the limit is not held back.
        writer.flush().unwrap();
        let large = vec![b'x'; PENDING_LIMIT];
        writer.write_all(&large).unwrap();
        writer.write_all(b"y").unwrap();
        assert_eq!(std::fs::read(&path).unwrap().len(), 14 + PENDING_LIMIT);
        writer.flush().unwrap();
        assert_eq!(std::fs::read(&path).unwrap().len(), 15 + PENDING_LIMIT);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_terminal_is_given_back_once() {
        let (file, path) = scratch("guard");
        let left = Arc::new(AtomicBool::new(false));
        let guard = |left: &Arc<AtomicBool>| Guard {
            writer: TtyWriter::new(file.try_clone().unwrap()),
            raw: false,
            panicked: Arc::new(AtomicBool::new(false)),
            left: left.clone(),
        };
        drop(guard(&left));
        assert_eq!(std::fs::read(&path).unwrap(), LEAVE);
        assert!(left.load(Ordering::SeqCst));
        // The terminal was given back already: the guard writes nothing more.
        drop(guard(&left));
        assert_eq!(std::fs::read(&path).unwrap(), LEAVE);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_guard_restores_after_a_panic_hook_that_could_not_write() {
        let (file, path) = scratch("hook");
        let (panicked, left) = (AtomicBool::new(false), Arc::new(AtomicBool::new(false)));
        // A descriptor that refuses writes stands in for a stalled terminal.
        let closed = Mutex::new(TtyWriter::new(File::open(&path).unwrap()));
        give_back(&closed, &panicked, &left);
        assert!(panicked.load(Ordering::SeqCst));
        assert!(!left.load(Ordering::SeqCst));
        assert!(std::fs::read(&path).unwrap().is_empty());
        drop(Guard {
            writer: TtyWriter::new(file.try_clone().unwrap()),
            raw: false,
            panicked: Arc::new(AtomicBool::new(false)),
            left: left.clone(),
        });
        assert_eq!(std::fs::read(&path).unwrap(), LEAVE);
        assert!(left.load(Ordering::SeqCst));
        // A hook that can write does it once and leaves nothing to the guard.
        let (panicked, left) = (AtomicBool::new(false), AtomicBool::new(false));
        let open = Mutex::new(TtyWriter::new(file));
        give_back(&open, &panicked, &left);
        give_back(&open, &panicked, &left);
        assert!(panicked.load(Ordering::SeqCst) && left.load(Ordering::SeqCst));
        assert_eq!(std::fs::read(&path).unwrap(), [LEAVE, LEAVE].concat());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sync_frame_wraps_output_in_dec_2026_and_clears_buffer_on_write_error() {
        let (file, path) = scratch("sync");
        let mut writer = TtyWriter::new_framed(file);
        writer.write_all(b"\x1b[1;1Hhi").unwrap();
        writer.flush().unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\x1b[?2026h\x1b[1;1Hhi\x1b[?2026l"
        );
        // A second empty flush writes nothing stale from the reused buffer.
        writer.flush().unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\x1b[?2026h\x1b[1;1Hhi\x1b[?2026l"
        );
        // A read-only descriptor errors on flush, and pending is already empty
        // so a subsequent write does not replay the failed frame.
        let mut readonly = TtyWriter::new_framed(File::open(&path).unwrap());
        readonly.write_all(b"stale").unwrap();
        assert!(readonly.flush().is_err());
        assert!(readonly.pending.is_empty());
        assert!(readonly.flushing.is_empty());
        let _ = std::fs::remove_file(path);
    }
}
