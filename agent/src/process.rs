//! Read-only facts about other processes of this user: argv, environment,
//! name, parent and process group. Each returns None when the process is gone or
//! unreadable.

#[cfg(not(target_os = "macos"))]
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

const MAX_ARGS: usize = 256;
/// Largest pid list buffer the macOS listing calls grow to.
#[cfg(target_os = "macos")]
const MAX_PIDS: usize = 1_048_576;

/// Output captured from a child process on the calling thread via `poll(2)`.
#[derive(Debug)]
pub(crate) struct CapturedOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stdout_truncated: bool,
    #[allow(dead_code)]
    pub(crate) stderr_truncated: bool,
}

#[derive(Debug)]
pub(crate) enum CaptureError {
    Timeout,
    Io(std::io::Error),
}

/// Converts UTF-8 bytes without copying when valid, falling back to lossy conversion.
pub(crate) fn into_string_lossy(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

fn set_nonblocking(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: fcntl(F_GETFL/F_SETFL) on a valid pipe descriptor owned by this process.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Maximum bytes drained from one pipe per `poll(2)` turn so a continuous
/// producer on one stream cannot starve the other stream or deadline checks.
const DRAIN_BUDGET: usize = 64 * 1024;

fn kill_and_wait(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn drain_nonblocking<R: Read>(
    pipe: &mut R,
    dst: &mut Vec<u8>,
    limit: usize,
    truncated: &mut bool,
    budget: usize,
) -> bool {
    let mut chunk = [0u8; 4096];
    let mut read_this_turn = 0usize;
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => return true,
            Ok(count) => {
                let remaining = limit.saturating_sub(dst.len());
                let keep = count.min(remaining);
                dst.extend_from_slice(&chunk[..keep]);
                if count > keep {
                    *truncated = true;
                }
                read_this_turn = read_this_turn.saturating_add(count);
                if read_this_turn >= budget {
                    return false;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return false,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return true,
        }
    }
}

/// Drains a spawned child's piped stdout/stderr and waits for exit on the
/// calling thread using `poll(2)`, avoiding extra reader threads and fixed
/// sleep polling while pipes remain open. Kills and reaps the child if
/// initialization fails or `timeout` elapses.
pub(crate) fn capture_child(
    mut child: Child,
    timeout: Option<Duration>,
    limit: usize,
) -> Result<CapturedOutput, CaptureError> {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    if let Some(ref pipe) = stdout
        && let Err(error) = set_nonblocking(pipe.as_raw_fd())
    {
        kill_and_wait(&mut child);
        return Err(CaptureError::Io(error));
    }
    if let Some(ref pipe) = stderr
        && let Err(error) = set_nonblocking(pipe.as_raw_fd())
    {
        kill_and_wait(&mut child);
        return Err(CaptureError::Io(error));
    }
    let deadline = timeout.map(|limit| Instant::now() + limit);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut stdout_truncated = false;
    let mut stderr_truncated = false;

    while stdout.is_some() || stderr.is_some() {
        let poll_timeout_ms: libc::c_int = match deadline {
            Some(deadline) => {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    kill_and_wait(&mut child);
                    return Err(CaptureError::Timeout);
                };
                left.as_millis().clamp(1, i32::MAX as u128) as libc::c_int
            }
            None => -1,
        };

        let mut fds = [libc::pollfd {
            fd: -1,
            events: 0,
            revents: 0,
        }; 2];
        let mut nfds: libc::nfds_t = 0;
        let mut out_idx = None;
        let mut err_idx = None;
        if let Some(ref pipe) = stdout {
            out_idx = Some(nfds as usize);
            fds[nfds as usize] = libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            nfds += 1;
        }
        if let Some(ref pipe) = stderr {
            err_idx = Some(nfds as usize);
            fds[nfds as usize] = libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            nfds += 1;
        }

        // SAFETY: `fds` contains `nfds` initialized entries pointing to live pipes.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), nfds, poll_timeout_ms) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            kill_and_wait(&mut child);
            return Err(CaptureError::Io(error));
        }
        if ready == 0 {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                kill_and_wait(&mut child);
                return Err(CaptureError::Timeout);
            }
            continue;
        }
        if let Some(idx) = out_idx
            && fds[idx].revents != 0
            && let Some(ref mut pipe) = stdout
            && drain_nonblocking(pipe, &mut out, limit, &mut stdout_truncated, DRAIN_BUDGET)
        {
            stdout = None;
        }
        if let Some(idx) = err_idx
            && fds[idx].revents != 0
            && let Some(ref mut pipe) = stderr
            && drain_nonblocking(pipe, &mut err, limit, &mut stderr_truncated, DRAIN_BUDGET)
        {
            stderr = None;
        }
    }

    let status = match deadline {
        None => child.wait().map_err(CaptureError::Io)?,
        Some(deadline) => {
            let mut step = Duration::from_millis(1);
            loop {
                if let Some(status) = child.try_wait().map_err(CaptureError::Io)? {
                    break status;
                }
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    kill_and_wait(&mut child);
                    return Err(CaptureError::Timeout);
                };
                std::thread::sleep(step.min(left));
                step = (step * 2).min(Duration::from_millis(10));
            }
        }
    };

    Ok(CapturedOutput {
        status,
        stdout: out,
        stderr: err,
        stdout_truncated,
        stderr_truncated,
    })
}

/// The exact argv of a process.
#[cfg(target_os = "macos")]
pub(crate) fn argv(pid: i32) -> Option<Vec<String>> {
    parse_procargs(&procargs(pid)?)
}

/// Cached kernel `KERN_ARGMAX` value (constant for the lifetime of the boot).
#[cfg(target_os = "macos")]
fn argmax() -> Option<usize> {
    static ARGMAX: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *ARGMAX.get_or_init(|| {
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let mut value: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        // SAFETY: the buffers match the sizes passed to sysctl.
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&mut value as *mut libc::c_int).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (ok == 0 && value > 0).then_some(value as usize)
    })
}

/// The kernel's argument area of a process: argc, the executable path,
/// argv and the environment.
#[cfg(target_os = "macos")]
fn procargs(pid: i32) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; argmax()?];
    let mut size = buffer.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: the buffer matches `size`; the kernel writes at most size bytes.
    let ok = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok != 0 || size < 4 {
        return None;
    }
    buffer.truncate(size);
    Some(buffer)
}

#[cfg(target_os = "macos")]
fn parse_procargs(buffer: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?);
    if argc <= 0 || argc as usize > MAX_ARGS {
        return None;
    }
    let rest = &buffer[4..];
    // The executable path, then NUL padding, then argc arguments.
    let path_end = rest.iter().position(|&b| b == 0)?;
    let mut position = path_end;
    while rest.get(position) == Some(&0) {
        position += 1;
    }
    let mut argv = Vec::new();
    for part in rest[position..].split(|&b| b == 0).take(argc as usize) {
        argv.push(String::from_utf8(part.to_vec()).ok()?);
    }
    (argv.len() == argc as usize).then_some(argv)
}

/// Every process ID on this machine; most others' processes are not
/// readable by the calls above.
#[cfg(target_os = "macos")]
pub(crate) fn all() -> Vec<i32> {
    // A full buffer may be truncated, so retry with a larger one.
    let mut size = 8192;
    loop {
        let mut buffer = vec![0 as libc::pid_t; size];
        let bytes = (buffer.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer holds `bytes` bytes; the call writes at most that.
        let written = unsafe { libc::proc_listallpids(buffer.as_mut_ptr().cast(), bytes) };
        if written <= 0 {
            return Vec::new();
        }
        if written as usize >= buffer.len() && size < MAX_PIDS {
            size *= 2;
            continue;
        }
        buffer.truncate((written as usize).min(buffer.len()));
        return buffer.into_iter().filter(|pid| *pid > 0).collect();
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn all() -> Vec<i32> {
    fs::read_dir("/proc")
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// A process's environment, as `NAME=value` strings. macOS shows none for
/// Apple's own platform binaries (an empty list).
#[cfg(target_os = "macos")]
pub(crate) fn environment(pid: i32) -> Option<Vec<String>> {
    parse_environment(&procargs(pid)?)
}

/// The strings after argc arguments and any NUL padding, up to the next
/// empty one.
#[cfg(target_os = "macos")]
fn parse_environment(buffer: &[u8]) -> Option<Vec<String>> {
    let argc = usize::try_from(i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?)).ok()?;
    let rest = &buffer[4..];
    let path_end = rest.iter().position(|&b| b == 0)?;
    let mut position = path_end;
    while rest.get(position) == Some(&0) {
        position += 1;
    }
    Some(
        rest[position..]
            .split(|&b| b == 0)
            .skip(argc)
            .skip_while(|part| part.is_empty())
            .take_while(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect(),
    )
}

/// A process's short command name, readable for other users' processes too.
#[cfg(target_os = "macos")]
pub(crate) fn name(pid: i32) -> Option<String> {
    // SAFETY: a zeroed proc_bsdshortinfo is valid; proc_pidinfo writes at
    // most its size.
    let mut short: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            (&mut short as *mut libc::proc_bsdshortinfo).cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    let bytes: Vec<u8> = short
        .pbsi_comm
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8(bytes).ok()
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn name(pid: i32) -> Option<String> {
    Some(
        fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()?
            .trim_end()
            .to_owned(),
    )
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn environment(pid: i32) -> Option<Vec<String>> {
    let bytes = fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(
        bytes
            .split(|&b| b == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect(),
    )
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn argv(pid: i32) -> Option<Vec<String>> {
    parse_cmdline(&fs::read(format!("/proc/{pid}/cmdline")).ok()?)
}

/// Splits /proc/PID/cmdline. Interior empty arguments stay. All trailing NULs
/// are dropped, because a process that rewrote its title in place leaves NUL
/// padding; trailing empty arguments cannot be told apart from that padding.
#[cfg(any(test, not(target_os = "macos")))]
fn parse_cmdline(bytes: &[u8]) -> Option<Vec<String>> {
    let end = bytes.iter().rposition(|&b| b != 0).map_or(0, |at| at + 1);
    let bytes = &bytes[..end];
    if bytes.is_empty() {
        return None;
    }
    let mut argv = Vec::new();
    for part in bytes.split(|&b| b == 0) {
        argv.push(String::from_utf8(part.to_vec()).ok()?);
    }
    (argv.len() <= MAX_ARGS).then_some(argv)
}

/// A process's parent, group and start time, read in one call so the three
/// belong to the same process even if its ID is reused right after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Info {
    pub(crate) parent: i32,
    pub(crate) group: i32,
    /// Start time in microseconds (macOS) or clock ticks since boot (Linux);
    /// only compared between processes of the same machine.
    pub(crate) started: u64,
    /// Whether the process has a controlling terminal.
    pub(crate) tty: bool,
}

#[cfg(target_os = "macos")]
pub(crate) fn info(pid: i32) -> Option<Info> {
    // SAFETY: a zeroed proc_bsdinfo is valid; proc_pidinfo writes at most
    // its size.
    let mut bsd: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut bsd as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (written == size).then(|| Info {
        parent: bsd.pbi_ppid as i32,
        group: bsd.pbi_pgid as i32,
        started: bsd.pbi_start_tvsec * 1_000_000 + bsd.pbi_start_tvusec,
        // NODEV (all bits set) when there is no controlling terminal.
        tty: bsd.e_tdev != u32::MAX && bsd.e_tdev != 0,
    })
}

/// The parent of `pid` when another user owns it; macOS gives only short
/// information on such a process. None for this user's processes.
#[cfg(target_os = "macos")]
pub(crate) fn other_users_parent(pid: i32) -> Option<i32> {
    // SAFETY: a zeroed proc_bsdshortinfo is valid; proc_pidinfo writes at
    // most its size.
    let mut short: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            (&mut short as *mut libc::proc_bsdshortinfo).cast(),
            size,
        )
    };
    // SAFETY: geteuid has no preconditions.
    (written == size && short.pbsi_uid != unsafe { libc::geteuid() })
        .then_some(short.pbsi_ppid as i32)
}

/// Linux shows every process's parent, so this is never needed there.
#[cfg(not(target_os = "macos"))]
pub(crate) fn other_users_parent(_pid: i32) -> Option<i32> {
    None
}

/// The user ID a process runs as.
#[cfg(target_os = "macos")]
pub(crate) fn owner(pid: i32) -> Option<u32> {
    // SAFETY: as in other_users_parent.
    let mut short: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            (&mut short as *mut libc::proc_bsdshortinfo).cast(),
            size,
        )
    };
    (written == size).then_some(short.pbsi_uid)
}

/// The user ID a process runs as: the owner of its /proc directory.
#[cfg(not(target_os = "macos"))]
pub(crate) fn owner(pid: i32) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(format!("/proc/{pid}"))
        .ok()
        .map(|metadata| metadata.uid())
}

/// The direct children of `pid`.
#[cfg(target_os = "macos")]
pub(crate) fn children(pid: i32) -> Vec<i32> {
    // A full buffer may be truncated, so retry with a larger one.
    let mut size = 256;
    loop {
        let mut buffer = vec![0 as libc::pid_t; size];
        let bytes = (buffer.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer holds `bytes` bytes; the call writes at most that.
        let written = unsafe { libc::proc_listchildpids(pid, buffer.as_mut_ptr().cast(), bytes) };
        if written <= 0 {
            return Vec::new();
        }
        if written as usize >= buffer.len() && size < MAX_PIDS {
            size *= 2;
            continue;
        }
        // The return value is a count of PIDs on macOS.
        buffer.truncate((written as usize).min(buffer.len()));
        return buffer.into_iter().filter(|child| *child > 0).collect();
    }
}

/// The direct children of `pid`, from every process's parent field.
#[cfg(not(target_os = "macos"))]
pub(crate) fn children(pid: i32) -> Vec<i32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|child| info(*child).is_some_and(|info| info.parent == pid))
        .collect()
}

/// Fields 4, 5 and 22 of /proc/PID/stat. The command name in field 2 may
/// contain spaces and parentheses, so fields are counted after its last
/// closing parenthesis.
#[cfg(not(target_os = "macos"))]
pub(crate) fn info(pid: i32) -> Option<Info> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    Some(Info {
        parent: fields.get(1)?.parse().ok()?,
        group: fields.get(2)?.parse().ok()?,
        started: fields.get(19)?.parse().ok()?,
        tty: fields.get(4)?.parse::<i64>().ok()? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmdline_keeps_empty_arguments() {
        assert_eq!(
            parse_cmdline(b"tool\0\0x\0"),
            Some(vec!["tool".to_string(), String::new(), "x".to_string()])
        );
        assert_eq!(parse_cmdline(b"tool\0"), Some(vec!["tool".to_string()]));
        assert_eq!(parse_cmdline(b""), None);
        for size in [60, 300] {
            let mut padded = b"claude".to_vec();
            padded.resize(size, 0);
            assert_eq!(parse_cmdline(&padded), Some(vec!["claude".to_string()]));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn procargs_layout_is_parsed() {
        let mut buffer = 2i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/usr/bin/vim\0\0\0vim\0a b\0HOME=/x\0");
        assert_eq!(
            parse_procargs(&buffer),
            Some(vec!["vim".to_string(), "a b".to_string()])
        );
    }

    /// A process that is not one of Apple's platform binaries, whose
    /// environment macOS does show: this test binary, waiting.
    #[test]
    #[ignore]
    fn waits_for_another_test() {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }

    #[test]
    fn another_process_environment_is_read() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "process::tests::waits_for_another_test",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .env("MASIL_PROCESS_TEST", "seen")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        // After its exec, not the fork's copy of this process.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let found = environment(pid);
        let listed = all().contains(&pid);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(listed);
        assert!(name(std::process::id() as i32).is_some());
        assert!(
            found.as_ref().is_some_and(|entries| entries
                .iter()
                .any(|entry| entry == "MASIL_PROCESS_TEST=seen")),
            "{found:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn another_users_process_still_has_a_parent() {
        // launchd is root's; a user process gets no full information on it.
        if unsafe { libc::geteuid() } != 0 {
            assert!(info(1).is_none());
            assert_eq!(other_users_parent(1), Some(0));
        }
        assert_eq!(other_users_parent(std::process::id() as i32), None);
    }

    #[test]
    fn a_process_owner_is_its_user() {
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        assert_eq!(owner(std::process::id() as i32), Some(me));
        assert_eq!(owner(-1), None);
    }

    #[test]
    fn a_spawned_child_is_listed() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let listed = children(std::process::id() as i32);
        let _ = child.kill();
        let _ = child.wait();
        assert!(listed.contains(&pid), "{pid} not in {listed:?}");
    }

    #[test]
    fn this_process_has_argv_a_parent_a_group_and_a_start() {
        let pid = std::process::id() as i32;
        assert!(argv(pid).is_some_and(|argv| !argv.is_empty()));
        let me = info(pid).unwrap();
        // SAFETY: getppid and getpgrp have no preconditions.
        assert_eq!(me.parent, unsafe { libc::getppid() });
        assert_eq!(me.group, unsafe { libc::getpgrp() });
        let parent = info(me.parent).unwrap();
        assert!(parent.started <= me.started);
        assert_eq!(info(-1), None);
    }

    #[test]
    fn capture_child_drains_stdout_and_stderr_and_enforces_limit() {
        let child = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "i=0; while [ $i -lt 200 ]; do printf 'out-line-%04d\\n' $i; printf 'err-line-%04d\\n' $i >&2; i=$((i+1)); done",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let captured = capture_child(child, Some(Duration::from_secs(3)), 256).unwrap();
        assert!(captured.status.success());
        assert_eq!(captured.stdout.len(), 256);
        assert_eq!(captured.stderr.len(), 256);
        assert!(captured.stdout_truncated);
        assert!(captured.stderr_truncated);
        assert!(captured.stdout.starts_with(b"out-line-0000\n"));
        assert!(captured.stderr.starts_with(b"err-line-0000\n"));
    }

    #[test]
    fn capture_child_kills_timed_out_process_promptly() {
        let started = Instant::now();
        let child = std::process::Command::new("sleep")
            .arg("5")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let result = capture_child(child, Some(Duration::from_millis(80)), 1024);
        assert!(matches!(result, Err(CaptureError::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn drain_nonblocking_yields_after_turn_budget_on_infinite_stream() {
        struct EndlessReader {
            calls: usize,
        }
        impl Read for EndlessReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.calls += 1;
                buf.fill(b'a');
                Ok(buf.len())
            }
        }
        let mut reader = EndlessReader { calls: 0 };
        let mut dst = Vec::new();
        let mut truncated = false;
        let eof = drain_nonblocking(&mut reader, &mut dst, 1024, &mut truncated, 8192);
        assert!(!eof);
        assert_eq!(dst.len(), 1024);
        assert!(truncated);
        assert_eq!(reader.calls, 2);
    }

    #[test]
    fn capture_child_times_out_under_continuous_stdout_flood() {
        let started = Instant::now();
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "while true; do printf '%04096d' 0; done"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let result = capture_child(child, Some(Duration::from_millis(100)), 256);
        assert!(matches!(result, Err(CaptureError::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
