//! `masil-agent open-link`: opens one hyperlink a client clicked, once. The UI
//! layer calls it from the mouse-up binding. Only http and https links open,
//! never on a remote (SSH) client, and the same (client, link) pair opens at
//! most once while repeated requests keep arriving within twice the click
//! timeout (2 x 300 ms, KEYC_CLICK_TIMEOUT in the core). The binding decides whether the client is remote and
//! passes `--remote 1|0`.

use crate::native_ui::require_client_name;
use crate::ui::settings::server::Server;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_URI: usize = 2048;
/// The core's double-click interval (KEYC_CLICK_TIMEOUT); it is not an option.
const CLICK_TIMEOUT_MS: u64 = 300;
/// The sliding dedup window: twice the click timeout.
const WINDOW_MS: u64 = 2 * CLICK_TIMEOUT_MS;
/// Remembered keys older than this are dropped, and a window never exceeds it.
const KEEP_MS: u64 = 5000;
const MAX_STATE: u64 = 64 * 1024;
const MAX_ENTRIES: usize = 256;
const LOCK_NAME: &str = ".masil-open-link.lock";
/// The format that reads the UI language of the client's server.
const LANGUAGE_FORMAT: &str = "#{@masil-lang}";

fn usage() -> String {
    "usage: masil-agent open-link --socket PATH --client CLIENT --remote 1|0 --uri URI".into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    Requested,
    Duplicate,
    UnsupportedScheme,
    RemoteClient,
    Failed,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Duplicate => "duplicate",
            Self::UnsupportedScheme => "unsupported_scheme",
            Self::RemoteClient => "remote_client",
            Self::Failed => "failed",
        }
    }
}

pub(crate) fn run(args: &[String]) -> Result<i32, String> {
    let mut socket = None;
    let mut client = None;
    let mut uri = None;
    let mut remote = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let slot = match arg.as_str() {
            "--socket" | "-S" => &mut socket,
            "--client" => &mut client,
            "--uri" => &mut uri,
            "--remote" => &mut remote,
            _ => return Err(usage()),
        };
        if slot.is_some() {
            return Err(usage());
        }
        *slot = Some(args.next().ok_or_else(usage)?.clone());
    }
    let (Some(socket), Some(client), Some(uri), Some(remote)) = (socket, client, uri, remote)
    else {
        return Err(usage());
    };
    let remote = match remote.as_str() {
        "1" => true,
        "0" => false,
        _ => return Err(usage()),
    };
    require_client_name(&client)?;
    let outcome = open_link(&socket, &client, remote, &uri);
    println!("{}", json!({"result": outcome.name(), "uri": uri}));
    Ok(0)
}

fn open_link(socket: &str, client: &str, remote: bool, uri: &str) -> Outcome {
    if !supported_uri(uri) {
        return Outcome::UnsupportedScheme;
    }
    let Ok(server) = Server::locate(Some(socket)) else {
        return Outcome::Failed;
    };
    if remote {
        let Ok(language) = server.run(&["display-message", "-c", client, "-p", LANGUAGE_FORMAT])
        else {
            return Outcome::Failed;
        };
        let text = remote_message(language.trim() == "ko", uri);
        return match server.run(&["display-message", "-l", "-c", client, "-d", "0", &text]) {
            Ok(_) => Outcome::RemoteClient,
            Err(_) => Outcome::Failed,
        };
    }
    // A click from a client that has gone must not open anything.
    // display-message -p succeeds for an unknown client, but the format then
    // expands without a client, so the name comes back empty.
    match server.run(&["display-message", "-c", client, "-p", "#{client_name}"]) {
        Ok(name) if name.trim() == client => {}
        _ => return Outcome::Failed,
    }
    let Some(directory) = Path::new(socket).parent() else {
        return Outcome::Failed;
    };
    // The state file can be shared between sockets, so the key names the socket.
    let scoped = format!("{socket}\0{client}");
    match first_open(directory, &key(&scoped, uri), now_ms(), WINDOW_MS) {
        Ok(true) => {}
        Ok(false) => return Outcome::Duplicate,
        Err(_) => return Outcome::Failed,
    }
    match launch(uri) {
        Ok(()) => Outcome::Requested,
        Err(_) => Outcome::Failed,
    }
}

/// An http or https link of bounded length with a host, made only of ASCII
/// printable bytes (no space, control or non-ASCII characters).
fn supported_uri(uri: &str) -> bool {
    if uri.is_empty() || uri.len() > MAX_URI || !uri.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return false;
    }
    let Some((scheme, rest)) = uri.split_once(':') else {
        return false;
    };
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
        return false;
    }
    let Some(rest) = rest.strip_prefix("//") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    // A bracketed IPv6 host keeps its colons; otherwise the port follows a colon.
    let host = if host_port.starts_with('[') {
        host_port.find(']').map_or("", |end| &host_port[1..end])
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    !host.is_empty()
}

/// The status message for a remote client, sent with `display-message -l` so
/// `%` stays literal. The status line still reads `#` as a style marker, so
/// `#` is doubled, and `;` is encoded so a trailing one cannot end the
/// command.
fn remote_message(korean: bool, uri: &str) -> String {
    let shown = uri.replace('#', "##").replace(';', "%3B");
    if korean {
        format!("원격 client라 링크를 열지 않았습니다: {shown}")
    } else {
        format!("Link not opened on this remote client: {shown}")
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn key(client: &str, uri: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(client.as_bytes());
    hash.update([0]);
    hash.update(uri.as_bytes());
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Records `key` at `now` and returns true, unless the same key was recorded
/// less than `window_ms` earlier. A duplicate refreshes the recorded time, so
/// the window slides while requests keep arriving. The flock held on the state file
/// covers the check and the record, so concurrent calls agree on one winner.
fn first_open(directory: &Path, key: &str, now: u64, window_ms: u64) -> Result<bool, String> {
    let mut file = state_file(directory)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!(
            "open-link lock: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut text = String::new();
    (&mut file)
        .take(MAX_STATE)
        .read_to_string(&mut text)
        .map_err(|error| format!("open-link state: {error}"))?;
    let window = window_ms.min(KEEP_MS);
    let mut entries: Vec<(u64, &str)> = text
        .lines()
        .filter_map(|line| {
            let (time, key) = line.split_once(' ')?;
            Some((time.parse::<u64>().ok()?, key))
        })
        // An entry from the future means the clock stepped back.
        .filter(|(time, _)| *time <= now && now - *time < KEEP_MS)
        .collect();
    let duplicate = entries
        .iter()
        .any(|(time, known)| *known == key && now - *time < window);
    entries.retain(|(_, known)| *known != key);
    entries.push((now, key));
    let skip = entries.len().saturating_sub(MAX_ENTRIES);
    let mut output = String::new();
    for (time, known) in &entries[skip..] {
        output.push_str(&format!("{time} {known}\n"));
    }
    file.set_len(0)
        .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|()| file.write_all(output.as_bytes()))
        .map_err(|error| format!("open-link state: {error}"))?;
    // The lock ends when the file closes.
    Ok(!duplicate)
}

/// True when `path` is a directory we own that only we can use. With
/// `follow` false a symlink does not count.
fn is_private_dir(path: &Path, follow: bool, uid: u32) -> bool {
    let metadata = if follow {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    };
    metadata.is_ok_and(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o077 == 0)
}

/// A private directory at `path`, created 0700 when missing. A symlink, a
/// foreign owner or loose permissions are refused.
fn ensure_private_dir(path: &Path, uid: u32) -> Result<(), String> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("open-link state directory: {error}")),
    }
    if is_private_dir(path, false, uid) {
        Ok(())
    } else {
        Err("open-link state directory is not a private owner directory".into())
    }
}

/// The directory for the state file when the socket's directory is not
/// private: under `runtime` when it is private, else `temp/masil-<uid>`.
fn fallback_directory(runtime: Option<&Path>, temp: &Path, uid: u32) -> Result<PathBuf, String> {
    if let Some(runtime) = runtime.filter(|p| p.is_absolute() && is_private_dir(p, true, uid)) {
        let path = runtime.join("masil");
        ensure_private_dir(&path, uid)?;
        return Ok(path);
    }
    let path = temp.join(format!("masil-{uid}"));
    ensure_private_dir(&path, uid)?;
    Ok(path)
}

/// The state file in the socket's directory when that is private, else in a
/// private fallback directory.
fn state_file(directory: &Path) -> Result<File, String> {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let directory = if is_private_dir(directory, true, uid) {
        directory.to_path_buf()
    } else {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        fallback_directory(runtime.as_deref(), &std::env::temp_dir(), uid)?
    };
    let path: PathBuf = directory.join(LOCK_NAME);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|error| format!("open-link state: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("open-link state: {error}"))?;
    if !metadata.file_type().is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err("open-link state is not a private owner file".into());
    }
    Ok(file)
}

/// Starts the system opener without a shell or terminal and does not wait for
/// the browser.
fn launch(uri: &str) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = Command::new(program)
        .arg(uri)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    // The opener normally outlives this call. Reap it only if it is already
    // gone, so a finished child is not left as a zombie; this process exits
    // right after, which hands any other child to init.
    let _ = child.try_wait();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn private_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("masil-open-link-{name}-{}", now_ms()));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn only_http_and_https_links_with_a_host_are_supported() {
        for good in [
            "http://example.com",
            "https://example.com/a?b=c&d=e#frag",
            "HTTPS://Example.com:8443/x",
            "https://user@example.com/",
            "http://[::1]:8080/",
            "https://example.com/a%20b;c",
        ] {
            assert!(supported_uri(good), "{good}");
        }
        for bad in [
            "",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://example.com",
            "custom-scheme://example.com",
            "http:example.com",
            "http://",
            "http:///path",
            "http://:80/",
            "https://@/",
            "example.com",
            "https://exa mple.com",
            "https://example.com/\tx",
            "https://example.com/\u{7}",
            "https://example.com/\u{a0}x",
            "https://example.com/\u{202e}gpj.exe",
            "https://exam\u{200b}ple.com/",
            "https://example.com/\u{e9}",
        ] {
            assert!(!supported_uri(bad), "{bad:?}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URI));
        assert!(!supported_uri(&long));
        let fits = format!("https://example.com/{}", "a".repeat(MAX_URI - 20));
        assert!(supported_uri(&fits));
    }

    #[test]
    fn remote_message_keeps_the_link_literal_in_a_format_and_a_command() {
        assert_eq!(
            remote_message(false, "https://a.test/#x;"),
            "Link not opened on this remote client: https://a.test/##x%3B"
        );
        assert!(remote_message(true, "https://a.test/").starts_with("원격 client라"));
    }

    #[test]
    fn the_window_slides_while_duplicates_arrive() {
        let dir = private_dir("window");
        let a = key("client", "https://a.test/");
        let b = key("client", "https://b.test/");
        assert!(first_open(&dir, &a, 10_000, 600).unwrap());
        assert!(!first_open(&dir, &a, 10_599, 600).unwrap());
        assert!(first_open(&dir, &b, 10_600, 600).unwrap());
        assert!(!first_open(&dir, &b, 10_700, 600).unwrap());
        // Each duplicate refreshes the time, so the window runs from the last request.
        assert!(!first_open(&dir, &a, 11_100, 600).unwrap());
        assert!(!first_open(&dir, &a, 11_699, 600).unwrap());
        assert!(first_open(&dir, &a, 12_300, 600).unwrap());
        assert!(first_open(&dir, &b, 12_300, 600).unwrap());
        assert_ne!(key("c1", "u"), key("c2", "u"));
        assert_ne!(key("c", "1u"), key("c1", "u"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_and_future_keys_are_dropped_and_a_window_is_capped() {
        let dir = private_dir("prune");
        let a = key("client", "https://a.test/");
        assert!(first_open(&dir, &a, 1_000, 600).unwrap());
        assert!(
            first_open(
                &dir,
                &key("client", "https://b.test/"),
                1_000 + KEEP_MS,
                600
            )
            .unwrap()
        );
        let text = std::fs::read_to_string(dir.join(LOCK_NAME)).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        // A window longer than KEEP_MS cannot outlive the pruning.
        assert!(first_open(&dir, &a, 20_000, 60_000).unwrap());
        assert!(first_open(&dir, &a, 20_000 + KEEP_MS, 60_000).unwrap());
        // The clock stepped back: the recorded time is in the future.
        assert!(first_open(&dir, &a, 30_000, 600).unwrap());
        assert!(first_open(&dir, &a, 25_000, 600).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_state_file_must_be_private_and_not_a_link() {
        let dir = private_dir("private");
        let state = dir.join(LOCK_NAME);
        std::os::unix::fs::symlink("/etc/hosts", &state).unwrap();
        assert!(first_open(&dir, "k", 1, 300).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_loose_socket_directory_uses_a_private_fallback() {
        let uid = unsafe { libc::geteuid() };
        let base = private_dir("fallback");
        let loose = base.join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!is_private_dir(&loose, true, uid));
        // No runtime directory: temp/masil-<uid>.
        let path = fallback_directory(None, &base, uid).unwrap();
        assert_eq!(path, base.join(format!("masil-{uid}")));
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        // A private runtime directory is preferred; a loose one is not.
        let runtime = base.join("run");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            fallback_directory(Some(&runtime), &base, uid).unwrap(),
            runtime.join("masil")
        );
        assert!(is_private_dir(&runtime.join("masil"), false, uid));
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            fallback_directory(Some(&runtime), &base, uid).unwrap(),
            path
        );
        // A symlink or a loose directory at the fallback path is refused.
        std::fs::remove_dir(&path).unwrap();
        std::os::unix::fs::symlink(&loose, &path).unwrap();
        assert!(fallback_directory(None, &base, uid).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(fallback_directory(None, &base, uid).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn concurrent_calls_agree_on_one_winner() {
        let dir = private_dir("race");
        let k = key("client", "https://a.test/");
        let winners = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| first_open(&dir, &k, 50_000, 300).unwrap()))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|won| *won)
                .count()
        });
        assert_eq!(winners, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn usage_errors_name_the_command() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(run(&args(&[])).is_err());
        assert!(run(&args(&["--socket", "/s", "--client", "c"])).is_err());
        // --remote is required and must be 1 or 0.
        for remote in [
            &[][..],
            &["--remote", "2"],
            &["--remote", "true"],
            &["--remote", ""],
        ] {
            let mut list = vec![
                "--socket",
                "/s",
                "--client",
                "c",
                "--uri",
                "https://a.test/",
            ];
            list.extend_from_slice(remote);
            assert!(run(&args(&list)).is_err(), "{remote:?}");
        }
        assert!(
            run(&args(&[
                "--socket", "/s", "--socket", "/t", "--client", "c", "--uri", "u"
            ]))
            .is_err()
        );
        assert!(run(&args(&["--bogus", "x"])).is_err());
        assert!(
            run(&args(&[
                "-S",
                "/s",
                "--client",
                "bad name",
                "--remote",
                "0",
                "--uri",
                "https://a.test/"
            ]))
            .is_err()
        );
    }
}
