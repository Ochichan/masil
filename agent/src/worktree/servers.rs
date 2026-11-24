//! Asking masil servers which panes and runs they have, for leases and
//! removal. A server that is not there is not an error; one that does not
//! answer is.

use crate::managed::{PANE_RUNS, PaneRun, pane_runs};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// How long one server may take to list its panes.
const ANSWER: Duration = Duration::from_secs(2);
const OUTPUT_LIMIT: u64 = 4 << 20;

pub(super) enum Server {
    /// Nothing listens on the socket.
    Absent,
    /// It did not answer, or answered something unreadable.
    Unknown(String),
    Panes(Vec<PaneRun>),
}

/// `realpath(TMUX_TMPDIR or /tmp)/masil-<uid>`, where masil puts default
/// and `-L` sockets.
pub(crate) fn socket_dir() -> Option<PathBuf> {
    let base = std::env::var_os("TMUX_TMPDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    Some(base.canonicalize().ok()?.join(format!("masil-{uid}")))
}

/// Sockets in `dir`: socket files whose names do not start with `.`.
pub(crate) fn sockets_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sockets: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter(|entry| {
            std::fs::symlink_metadata(entry.path())
                .is_ok_and(|metadata| metadata.file_type().is_socket())
        })
        .map(|entry| entry.path())
        .collect();
    sockets.sort();
    sockets
}

/// Whether a server listens on `socket`. Refused or missing means no.
pub(crate) fn listening(socket: &Path) -> Result<bool, String> {
    match std::os::unix::net::UnixStream::connect(socket) {
        Ok(_) => Ok(true),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ECONNREFUSED | libc::ENOENT | libc::ENOTDIR)
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error.to_string()),
    }
}

pub(super) fn query(socket: &Path) -> Server {
    match listening(socket) {
        Ok(false) => return Server::Absent,
        Ok(true) => {}
        Err(error) => return Server::Unknown(error),
    }
    match list(socket) {
        Ok(output) => match pane_runs(&output) {
            Ok(panes) => Server::Panes(panes),
            Err(error) => Server::Unknown(error),
        },
        Err(error) => Server::Unknown(error),
    }
}

fn list(socket: &Path) -> Result<String, String> {
    let binary = crate::native_ui::native_executable()?;
    let child = Command::new(binary)
        .arg("-u")
        .arg("-S")
        .arg(socket)
        .args(["list-panes", "-a", "-F", PANE_RUNS])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run masil: {error}"))?;
    let captured = crate::process::capture_child(child, Some(ANSWER), OUTPUT_LIMIT as usize)
        .map_err(|error| match error {
            crate::process::CaptureError::Timeout => "it did not answer within 2 s".to_owned(),
            crate::process::CaptureError::Io(error) => {
                format!("cannot read the pane list: {error}")
            }
        })?;
    // A cut-off list would read as "no pane there".
    if captured.stdout_truncated || captured.stdout.len() as u64 >= OUTPUT_LIMIT {
        return Err("output too large".to_owned());
    }
    if !captured.status.success() {
        return Err(format!("list-panes failed ({})", captured.status));
    }
    // A pane path that is not UTF-8 must not hide the others.
    Ok(crate::process::into_string_lossy(captured.stdout))
}

/// Whether `path`, or one of its parents, is the directory `identity`.
/// Compared by device and inode, not by spelling.
pub(super) fn inside(path: &Path, identity: (u64, u64)) -> bool {
    path.ancestors().any(|dir| {
        std::fs::metadata(dir).is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == identity)
    })
}
