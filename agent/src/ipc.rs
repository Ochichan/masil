use serde_json::Value;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream as StdUnixStream};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub(crate) struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.dev() == self.dev
            && metadata.ino() == self.ino
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn private_parent(path: &Path, what: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| format!("{what} socket requires an absolute parent directory"))?;
    let metadata =
        fs::metadata(parent).map_err(|_| format!("{what} socket parent is unavailable"))?;
    if !path.is_absolute()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(format!(
            "{what} socket must be in a private owner directory"
        ));
    }
    Ok(())
}

pub(crate) fn bind_private_socket(
    path: &Path,
    what: &str,
) -> Result<(UnixListener, SocketGuard), String> {
    private_parent(path, what)?;
    match fs::symlink_metadata(path) {
        Ok(_) => return Err(format!("refusing existing {what} socket path")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(format!("cannot inspect {what} socket path")),
    }
    let listener = UnixListener::bind(path).map_err(|_| format!("cannot bind {what} socket"))?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return Err(format!("cannot inspect bound {what} socket")),
    };
    let guard = SocketGuard {
        path: path.to_owned(),
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    if !metadata.file_type().is_socket() {
        return Err(format!("bound {what} path is not a Unix socket"));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| format!("cannot make {what} socket private"))?;
    listener
        .set_nonblocking(true)
        .map_err(|_| format!("cannot make {what} socket nonblocking"))?;
    Ok((listener, guard))
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) fn peer_credentials(
    stream: &StdUnixStream,
) -> Option<(libc::uid_t, Option<libc::pid_t>)> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return None;
    }

    let mut pid: libc::pid_t = 0;
    let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let pid = (unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut length,
        )
    } == 0)
        .then_some(pid);
    Some((uid, pid))
}

#[cfg(target_os = "linux")]
pub(crate) fn peer_credentials(
    stream: &StdUnixStream,
) -> Option<(libc::uid_t, Option<libc::pid_t>)> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
    {
        return None;
    }
    Some((credentials.uid, Some(credentials.pid)))
}

#[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
pub(crate) fn peer_credentials(
    stream: &StdUnixStream,
) -> Option<(libc::uid_t, Option<libc::pid_t>)> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    (unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0)
        .then_some((uid, None))
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "linux"
)))]
pub(crate) fn peer_credentials(
    _stream: &StdUnixStream,
) -> Option<(libc::uid_t, Option<libc::pid_t>)> {
    None
}

pub(crate) fn peer_is_owner(stream: &StdUnixStream) -> bool {
    peer_credentials(stream).is_some_and(|(uid, _)| uid == unsafe { libc::geteuid() })
}

pub(crate) async fn read_frame(
    stream: &mut UnixStream,
    max_request: usize,
) -> Result<Vec<u8>, String> {
    let mut header = [0; 4];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|_| "incomplete request frame")?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > max_request {
        return Err("request exceeds frame limit".into());
    }
    let mut body = vec![0; length];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|_| "incomplete request frame")?;
    Ok(body)
}

pub(crate) async fn write_frame(
    stream: &mut UnixStream,
    value: &Value,
    max_response: usize,
) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|_| "cannot encode response")?;
    if body.is_empty() || body.len() > max_response {
        return Err("response exceeds frame limit".into());
    }
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .await
        .map_err(|_| "cannot write response")?;
    stream
        .write_all(&body)
        .await
        .map_err(|_| "cannot write response".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn frame_round_trip() {
        let (mut writer, mut reader) = UnixStream::pair().expect("create Unix stream pair");
        let value = json!({"kind": "status", "v": 1});

        write_frame(&mut writer, &value, 1024)
            .await
            .expect("write frame");
        let body = read_frame(&mut reader, 1024).await.expect("read frame");

        assert_eq!(body, serde_json::to_vec(&value).expect("encode value"));
    }

    #[tokio::test]
    async fn zero_length_frame_is_rejected() {
        let (mut writer, mut reader) = UnixStream::pair().expect("create Unix stream pair");
        writer
            .write_all(&0_u32.to_be_bytes())
            .await
            .expect("write frame header");

        let error = read_frame(&mut reader, 1024)
            .await
            .expect_err("zero-length frame must fail");

        assert_eq!(error, "request exceeds frame limit");
    }

    #[tokio::test]
    async fn oversized_frames_are_rejected() {
        let (mut writer, mut reader) = UnixStream::pair().expect("create Unix stream pair");
        writer
            .write_all(&2_u32.to_be_bytes())
            .await
            .expect("write frame header");

        let error = read_frame(&mut reader, 1)
            .await
            .expect_err("oversized request frame must fail");
        assert_eq!(error, "request exceeds frame limit");

        let (mut writer, _reader) = UnixStream::pair().expect("create Unix stream pair");
        let error = write_frame(&mut writer, &json!("oversized"), 1)
            .await
            .expect_err("oversized response frame must fail");
        assert_eq!(error, "response exceeds frame limit");
    }

    #[test]
    fn peer_credentials_returns_local_process_credentials() {
        let (stream, _peer) = StdUnixStream::pair().expect("create Unix stream pair");
        let credentials = peer_credentials(&stream).expect("read peer credentials");

        assert_eq!(credentials.0, unsafe { libc::geteuid() });

        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert_eq!(
            credentials.1,
            Some(
                std::process::id()
                    .try_into()
                    .expect("process ID fits pid_t")
            )
        );
    }
}
