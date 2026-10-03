//! Run tokens (RM-05, docs/agent-integration.md): each launch gets a random
//! token in a private file, and the pane's environment names the file, never
//! the token. A provider hook shows it; the pane's run evidence keeps only
//! its SHA-256. This is in addition to the process-tree check and prevents
//! accidents only: any process of the same user can read the file.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The variable a launched pane carries.
pub(crate) const VARIABLE: &str = "MASIL_AGENT_TOKEN_FILE";
/// Token files older than this are removed at the next launch, for runs
/// whose end nothing saw.
const KEEP: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// A run not listed yet may be starting: its token stays this long.
const STARTING: Duration = Duration::from_secs(120);

/// One directory per server: the state directory is the user's.
fn directory(socket: &Path) -> Result<PathBuf, String> {
    let digest = hex(&Sha256::digest(socket.as_os_str().as_encoded_bytes())[..8]);
    super::private_directory(&super::state_base()?.join("masil/tokens").join(digest))
}

fn valid(run: &str) -> bool {
    !run.is_empty()
        && run.len() <= 128
        && run
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A new token for `run`: the file it is in and its SHA-256.
pub(crate) fn create(socket: &Path, run: &str) -> Result<(PathBuf, String), String> {
    if !valid(run) {
        return Err("invalid run for a token".into());
    }
    let directory = directory(socket)?;
    prune(&directory, |_| true, KEEP);
    let mut token = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut token))
        .map_err(|error| format!("run token: {error}"))?;
    let token = hex(&token);
    let path = directory.join(run);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| format!("run token: {error}"))?;
    file.write_all(token.as_bytes())
        .map_err(|error| format!("run token: {error}"))?;
    Ok((path, hex(&Sha256::digest(token.as_bytes()))))
}

/// The run ended: its token goes.
pub(crate) fn remove(socket: &Path, run: &str) {
    if valid(run)
        && let Ok(directory) = directory(socket)
    {
        let _ = std::fs::remove_file(directory.join(run));
    }
}

/// Removes the tokens of runs that no longer run on this server.
pub(crate) fn prune_ended(socket: &Path, live: &std::collections::HashSet<String>) {
    if let Ok(directory) = directory(socket) {
        prune(&directory, |run| !live.contains(run), STARTING);
    }
}

fn prune(directory: &Path, ended: impl Fn(&str) -> bool, older: Duration) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let old = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > older);
        if old && name.to_str().is_some_and(&ended) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The token this process's environment names, checked against the hash
/// the run recorded.
pub(crate) fn check(socket: &Path, expected: &str) -> Result<(), String> {
    let refused = |why: &str| Err(format!("identity_mismatch: run token {why}"));
    let Some(path) = std::env::var_os(VARIABLE).map(PathBuf::from) else {
        return refused("missing from the hook's environment");
    };
    // The file the launch named; the hook's own state directory may differ
    // from the launcher's, so only the file is checked.
    let _ = socket;
    let Ok(mut file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&path)
    else {
        return refused("file is gone");
    };
    let owner = file.metadata().is_ok_and(|metadata| {
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
    });
    if !owner {
        return refused("file is not private");
    }
    let mut token = Vec::new();
    if (&mut file).take(128).read_to_end(&mut token).is_err() {
        return refused("file cannot be read");
    }
    if hex(&Sha256::digest(&token)) != expected {
        return refused("does not match this run");
    }
    // In use: the prune of month-old tokens leaves it.
    let now = std::time::SystemTime::now();
    let _ = file.set_times(
        std::fs::FileTimes::new()
            .set_modified(now)
            .set_accessed(now),
    );
    Ok(())
}
