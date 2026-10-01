//! Read-only facts about other processes of this user: argv, parent and
//! process group. Each returns None when the process is gone or unreadable.

#[cfg(not(target_os = "macos"))]
use std::fs;

const MAX_ARGS: usize = 256;

/// The exact argv of a process.
#[cfg(target_os = "macos")]
pub(crate) fn argv(pid: i32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let mut argmax: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: the buffers match the sizes passed to sysctl.
    let ok = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut argmax as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok != 0 || argmax <= 0 {
        return None;
    }
    let mut buffer = vec![0u8; argmax as usize];
    let mut size = buffer.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: as above; the kernel writes at most size bytes.
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
    parse_procargs(&buffer[..size])
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

#[cfg(not(target_os = "macos"))]
pub(crate) fn argv(pid: i32) -> Option<Vec<String>> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let mut argv = Vec::new();
    for part in bytes.split(|&b| b == 0) {
        if !part.is_empty() {
            argv.push(String::from_utf8(part.to_vec()).ok()?);
        }
    }
    (!argv.is_empty() && argv.len() <= MAX_ARGS).then_some(argv)
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
    })
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
