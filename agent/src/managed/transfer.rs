//! Files sent to an endpoint (WT-05, P8c, docs/agent-endpoints.md). The
//! remote command is fixed (`agent --socket S receive`); what and where go
//! on standard input as a JSON header line, then the bytes. The receiver
//! writes a private partial file in the destination directory, checks its
//! size and SHA-256, and gives it its name without replacing anything.

use super::endpoints::Endpoint;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The largest file sent.
pub(crate) const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HEADER: usize = 4096;
const PARTIAL: &str = ".masil-partial-";
/// Partial files older than this are left from a receiver that died.
const STALE_PARTIAL: Duration = Duration::from_secs(60 * 60);

/// Time for `size` bytes: 30 s, and a second per MiB.
fn allowance(size: u64) -> Duration {
    Duration::from_secs(30 + size / (1024 * 1024))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A single path component a file may be named.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.starts_with(PARTIAL)
        && !name.contains('/')
        && !name.chars().any(char::is_control)
}

fn valid_run(run: &str) -> bool {
    !run.is_empty()
        && run.len() <= 128
        && run
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn c_name(name: &str) -> Result<CString, String> {
    CString::new(name).map_err(|_| "transfer_invalid: a name with NUL".to_owned())
}

/// `agent --socket S receive`: the receiving end. Prints the receipt.
pub(crate) fn receive() -> Result<i32, String> {
    let mut stdin = std::io::stdin().lock();
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stdin.read_exact(&mut byte).is_err() {
            return Err("transfer_invalid: no header".into());
        }
        if byte[0] == b'\n' {
            break;
        }
        header.push(byte[0]);
        if header.len() > MAX_HEADER {
            return Err("transfer_invalid: header over 4 KiB".into());
        }
    }
    let header: Value = serde_json::from_slice(&header)
        .map_err(|error| format!("transfer_invalid: header: {error}"))?;
    let name = header["name"].as_str().unwrap_or_default();
    if !valid_name(name) {
        return Err("transfer_invalid: the name must be one path component of 1-255 bytes".into());
    }
    let size = header["size"].as_u64().ok_or("transfer_invalid: no size")?;
    if size > MAX_BYTES {
        return Err("transfer_too_large: a file is at most 64 MiB".into());
    }
    let expected = header["sha256"]
        .as_str()
        .filter(|hash| hash.len() == 64)
        .ok_or("transfer_invalid: no sha256")?
        .to_owned();
    let attachment = header["attachment_run"].is_string();
    let directory = match (header["dir"].as_str(), header["attachment_run"].as_str()) {
        // The directory the person named, as it resolves now (`/tmp` is a
        // link on macOS); it is then opened without following links.
        (Some(dir), None) if Path::new(dir).is_absolute() => Path::new(dir)
            .canonicalize()
            .map_err(|error| format!("transfer_invalid: {dir}: {error}"))?,
        (None, Some(run)) if valid_run(run) => {
            super::private_directory(&super::state_base()?.join("masil/attachments").join(run))?
        }
        _ => return Err("transfer_invalid: give an absolute dir or an attachment run".into()),
    };
    let path = CString::new(directory.as_os_str().as_encoded_bytes())
        .map_err(|_| "transfer_invalid: a directory with NUL")?;
    // SAFETY: a valid C string; the descriptor is owned below.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "transfer_invalid: {}: {}",
            directory.display(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: just opened, owned from here.
    let directory_fd = unsafe { OwnedFd::from_raw_fd(fd) };
    clean_partials(&directory, directory_fd.as_raw_fd());
    let partial = format!("{PARTIAL}{}", super::nonce()?);
    let partial_c = c_name(&partial)?;
    // SAFETY: openat on our directory descriptor with a fresh name.
    let file_fd = unsafe {
        libc::openat(
            directory_fd.as_raw_fd(),
            partial_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if file_fd < 0 {
        return Err(format!(
            "transfer_invalid: cannot write in {}: {}",
            directory.display(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: just created, owned from here.
    let mut file = unsafe { std::fs::File::from_raw_fd(file_fd) };
    let remove_partial = {
        let dir = directory_fd.as_raw_fd();
        let partial_c = partial_c.clone();
        move || {
            // SAFETY: unlinkat on our directory descriptor.
            unsafe { libc::unlinkat(dir, partial_c.as_ptr(), 0) };
        }
    };
    // The sender may stall: past its allowance the partial goes and this
    // process ends.
    let watchdog = remove_partial.clone();
    let allowed = allowance(size);
    let committed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = committed.clone();
    std::thread::spawn(move || {
        std::thread::sleep(allowed);
        // Named already: the receipt is on its way.
        if done.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        watchdog();
        eprintln!(
            "masil-agent: error[transfer_timeout]: transfer_timeout: the file did not arrive in time"
        );
        std::process::exit(4);
    });
    let mut hasher = Sha256::new();
    let mut remaining = size;
    let mut buffer = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = buffer.len().min(remaining as usize);
        let read = match stdin.read(&mut buffer[..want]) {
            Ok(0) => {
                remove_partial();
                return Err("transfer_incomplete: the file ended early".into());
            }
            Ok(read) => read,
            Err(error) => {
                remove_partial();
                return Err(format!("transfer_incomplete: {error}"));
            }
        };
        hasher.update(&buffer[..read]);
        if let Err(error) = file.write_all(&buffer[..read]) {
            remove_partial();
            return Err(format!("transfer_invalid: {error}"));
        }
        remaining -= read as u64;
    }
    if hex(&hasher.finalize()) != expected {
        remove_partial();
        return Err("transfer_corrupt: the file arrived different from what was sent".into());
    }
    if let Err(error) = file.sync_all() {
        remove_partial();
        return Err(format!("transfer_invalid: {error}"));
    }
    drop(file);
    committed.store(true, std::sync::atomic::Ordering::SeqCst);
    let dir = directory_fd.as_raw_fd();
    let link = |name: &str| -> Result<(), std::io::Error> {
        let name_c =
            CString::new(name).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        // A link fails if the name exists: nothing is replaced.
        // SAFETY: linkat within our directory descriptor.
        let linked = unsafe { libc::linkat(dir, partial_c.as_ptr(), dir, name_c.as_ptr(), 0) };
        if linked == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    };
    let mut named = name.to_owned();
    let mut result = link(&named);
    // An agent's attachments: the same file again (a retry) is the one
    // there; another file of that name gets a name of its own.
    if attachment
        && result
            .as_ref()
            .is_err_and(|error| error.raw_os_error() == Some(libc::EEXIST))
    {
        if same_file(dir, name, &expected) {
            remove_partial();
            println!(
                "{}",
                json!({"path": directory.join(name), "size": size, "sha256": expected})
            );
            return Ok(0);
        }
        let (stem, extension) = match name.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
            _ => (name, String::new()),
        };
        for _ in 0..8 {
            named = format!("{stem}-{}{extension}", &super::nonce()?[..8]);
            result = link(&named);
            if !result
                .as_ref()
                .is_err_and(|error| error.raw_os_error() == Some(libc::EEXIST))
            {
                break;
            }
        }
    }
    remove_partial();
    if let Err(error) = result {
        return Err(if error.raw_os_error() == Some(libc::EEXIST) {
            format!("transfer_exists: {name} exists there; nothing was replaced")
        } else {
            format!("transfer_invalid: {error}")
        });
    }
    println!(
        "{}",
        json!({"path": directory.join(&named), "size": size, "sha256": expected})
    );
    Ok(0)
}

/// Whether `name` in the directory is a regular file with this SHA-256.
fn same_file(dir: i32, name: &str, expected: &str) -> bool {
    let Ok(name) = CString::new(name) else {
        return false;
    };
    // SAFETY: openat within the directory descriptor; owned below.
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return false;
    }
    // SAFETY: just opened.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let mut hasher = Sha256::new();
    let mut reader = file.take(MAX_BYTES + 1);
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(_) => return false,
        }
    }
    hex(&hasher.finalize()) == expected
}

/// A sending child's process group, killed if the send ends before the
/// child is reaped.
struct Group(Option<i32>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            // SAFETY: the leader is not reaped, so the group is still ours.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
    }
}

/// Partial files of receivers that died, older than an hour and ours.
fn clean_partials(directory: &Path, fd: i32) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| name.starts_with(PARTIAL)) else {
            continue;
        };
        let stale = entry.metadata().is_ok_and(|metadata| {
            use std::os::unix::fs::MetadataExt;
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata
                    .modified()
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > STALE_PARTIAL)
        });
        if stale && let Ok(name) = CString::new(name) {
            // SAFETY: unlinkat within the directory descriptor.
            unsafe { libc::unlinkat(fd, name.as_ptr(), 0) };
        }
    }
}

/// Where a file goes on the endpoint host.
pub(crate) enum Destination {
    /// A directory there, under a name.
    Directory { dir: String, name: String },
    /// The attachment directory of a run there, under the file's name.
    Attachment { run: String },
}

/// Sends a file of this machine to `endpoint`: checked here, then streamed
/// to its fixed `receive` command. Returns the receipt (path there, size,
/// SHA-256); a timeout or cancellation ends the process group, and the
/// receiver removes what it wrote.
pub(crate) async fn send(
    endpoint: &Endpoint,
    local: &Path,
    destination: Destination,
) -> Result<Value, String> {
    // Read and hashed off the caller's thread (the desk's runtime has one),
    // and never more than the limit, even of a file that grows meanwhile.
    let path = local.to_owned();
    let (bytes, digest) = tokio::task::spawn_blocking(move || {
        let shown = path.display().to_string();
        let file = std::fs::File::open(&path)
            .map_err(|error| format!("invalid_argument: {shown}: {error}"))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("invalid_argument: {shown}: {error}"))?;
        if !metadata.is_file() {
            return Err(format!("invalid_argument: {shown} is not a file"));
        }
        if metadata.len() > MAX_BYTES {
            return Err("transfer_too_large: a file is at most 64 MiB".to_owned());
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("invalid_argument: {shown}: {error}"))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("transfer_too_large: a file is at most 64 MiB".to_owned());
        }
        let digest = hex(&Sha256::digest(&bytes));
        Ok((bytes, digest))
    })
    .await
    .map_err(|error| error.to_string())??;
    let size = bytes.len() as u64;
    let local_name = local
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid_argument: the file name is not UTF-8")?
        .to_owned();
    let mut header = json!({"version": 1, "size": size, "sha256": digest});
    match destination {
        Destination::Directory { dir, name } => {
            header["dir"] = json!(dir);
            header["name"] = json!(name);
        }
        Destination::Attachment { run } => {
            header["attachment_run"] = json!(run);
            header["name"] = json!(local_name);
        }
    }
    if !valid_name(header["name"].as_str().unwrap_or_default()) {
        return Err("transfer_invalid: the name must be one path component of 1-255 bytes".into());
    }
    let mut command = endpoint.receive_command();
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("endpoint_unreachable: endpoint '{}': {error}", endpoint.id))?;
    // Cancelled (the desk closed) or past the deadline: the group goes
    // while its leader is not reaped.
    let mut guard = Group(child.id().map(|pid| pid as i32));
    let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("endpoint_unreachable: no pipes".into());
    };
    let mut line = serde_json::to_vec(&header).map_err(|error| error.to_string())?;
    line.push(b'\n');
    let work = async {
        let write = async {
            let sent = async {
                stdin.write_all(&line).await?;
                stdin.write_all(&bytes).await?;
                stdin.shutdown().await
            };
            let result = sent.await;
            drop(stdin);
            result
        };
        let out = async {
            let mut text = Vec::new();
            let _ = (&mut stdout).take(64 * 1024).read_to_end(&mut text).await;
            text
        };
        let err = async {
            let mut text = Vec::new();
            let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut text).await;
            text
        };
        let (written, out, err) = tokio::join!(write, out, err);
        let status = child.wait().await;
        (written, out, err, status)
    };
    let deadline = allowance(size) + Duration::from_secs(5);
    let Ok((written, out, err, status)) = tokio::time::timeout(deadline, work).await else {
        return Err("transfer_timeout: the file did not arrive in time".into());
    };
    // Reaped: its group ID is no longer ours to signal.
    guard.0 = None;
    let err = String::from_utf8_lossy(&err);
    let status = status.map_err(|error| error.to_string())?;
    if status.success() {
        return serde_json::from_slice::<Value>(&out).map_err(|_| {
            format!(
                "endpoint_unreachable: endpoint '{}' sent no receipt",
                endpoint.id
            )
        });
    }
    // An endpoint whose masil-agent predates `receive`.
    if status.code() == Some(2) && err.contains("unknown agent command") {
        return Err(format!(
            "remote_unsupported: endpoint '{}' runs a masil-agent without file transfer; update it",
            endpoint.id
        ));
    }
    // The receiver's own error, as its CLI prints it.
    let message = err
        .lines()
        .find_map(|line| {
            line.split_once("]: ")
                .map(|(_, message)| message.to_owned())
        })
        .unwrap_or_else(|| {
            let first = err.lines().next().unwrap_or("no output");
            if super::endpoints::host_key_failure(&err) {
                format!("host_key: {first}")
            } else {
                format!("endpoint_unreachable: endpoint '{}': {first}", endpoint.id)
            }
        });
    if written.is_err() && message.starts_with("endpoint_unreachable") {
        return Err(format!("{message} (sending stopped)"));
    }
    Err(message)
}
