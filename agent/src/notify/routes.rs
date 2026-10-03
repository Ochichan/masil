//! How a batch reaches the user: tmux clients, the OS, a command. Each
//! route is tried once; its outcome is recorded, never retried.

use super::Batch;
use crate::native_ui::Context;
use std::ffi::OsString;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A command route's time, output included.
const HOOK_TIME: Duration = Duration::from_secs(5);
/// Output read from each of the hook's stdout and stderr; then the pipe is
/// closed, and a hook that writes on gets EPIPE.
const HOOK_OUTPUT: u64 = 64 * 1024;
const OS_TIME: Duration = Duration::from_secs(5);
/// How long a tmux message stays.
const MESSAGE_MS: &str = "3000";

/// Text for `display-message -l`: no control characters, `#` doubled so
/// styles are not read from it, and no trailing `;` for tmux to take as a
/// command separator.
pub(crate) fn tmux_text(text: &str) -> String {
    let mut clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .replace('#', "##");
    if clean.ends_with(';') {
        clean.push(' ');
    }
    clean
}

/// Every attached client of this server gets the message (and a bell if
/// asked), without freezing its panes.
pub(crate) async fn tmux(
    native: &Context,
    batch: &Batch,
    detail: bool,
    bell: bool,
) -> Result<(), String> {
    let command =
        |args: &[&str]| native.tmux(args.iter().map(OsString::from).collect::<Vec<_>>(), None);
    let listing = command(&[
        "list-clients",
        "-F",
        "#{client_name}\t#{client_tty}\t#{client_control_mode}",
    ])
    .await?;
    let clients = String::from_utf8_lossy(&listing.stdout);
    let lines = batch.lines(detail).join(" | ");
    let text = tmux_text(&lines);
    // A control-mode client gets `%message` as it is, never drawn as a
    // format: no `##` for it.
    let plain = tmux_text(&lines).replace("##", "#");
    let mut sent = 0;
    for client in clients.lines() {
        let mut fields = client.split('\t');
        let (Some(name), Some(tty), Some(control)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let message = if control == "1" { &plain } else { &text };
        if command(&[
            "display-message",
            "-C",
            "-d",
            MESSAGE_MS,
            "-c",
            name,
            "-l",
            message,
        ])
        .await
        .is_ok()
        {
            sent += 1;
        }
        if bell && control != "1" {
            ring(tty);
        }
    }
    if clients.trim().is_empty() {
        return Err("no client attached".into());
    }
    if sent == 0 {
        return Err("no client took the message".into());
    }
    Ok(())
}

/// A bell on a client's terminal: only a terminal device this user owns,
/// never blocking.
fn ring(tty: &str) {
    let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(tty)
    else {
        return;
    };
    let Ok(metadata) = file.metadata() else {
        return;
    };
    use std::os::fd::AsRawFd;
    // SAFETY: geteuid has no preconditions; isatty takes an open descriptor.
    let ours =
        metadata.uid() == unsafe { libc::geteuid() } && metadata.file_type().is_char_device();
    if ours && unsafe { libc::isatty(file.as_raw_fd()) } == 1 {
        use std::io::Write;
        let _ = (&file).write_all(b"\x07");
    }
}

fn environment(command: &mut tokio::process::Command, env: &[(String, String)]) {
    command.env_clear();
    for (name, value) in env {
        command.env(name, value);
    }
}

/// The OS's own notification: osascript on macOS, notify-send on Linux
/// when installed. Text goes as arguments, never into a script.
pub(crate) async fn os(
    batch: &Batch,
    detail: bool,
    env: &[(String, String)],
) -> Result<(), String> {
    for line in batch.lines(detail) {
        let mut command = if cfg!(target_os = "macos") {
            let mut command = tokio::process::Command::new("/usr/bin/osascript");
            command.args([
                "-e",
                "on run argv",
                "-e",
                "display notification (item 1 of argv) with title (item 2 of argv)",
                "-e",
                "end run",
                "--",
                &line,
                "masil",
            ]);
            command
        } else {
            let mut command = tokio::process::Command::new("notify-send");
            command.args(["-a", "masil", "--", "masil", &line]);
            command
        };
        environment(&mut command, env);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| format!("os notification: {error}"))?;
        match tokio::time::timeout(OS_TIME, child.wait()).await {
            Ok(Ok(status)) if status.success() => {}
            Ok(Ok(status)) => return Err(format!("os notification exited with {status}")),
            Ok(Err(error)) => return Err(error.to_string()),
            Err(_) => return Err("os notification did not finish in 5 s".into()),
        }
    }
    Ok(())
}

/// The user's command: the batch as JSON on standard input, in its own
/// process group, 5 s for everything including its output, which is read
/// and dropped; then SIGTERM, and SIGKILL 2 s later.
pub(crate) async fn hook(
    argv: &[String],
    batch: &Batch,
    detail: bool,
    env: &[(String, String)],
) -> Result<(), String> {
    let (program, args) = argv.split_first().ok_or("no hook command")?;
    let input = serde_json::to_vec(&batch.hook_json(detail)).map_err(|error| error.to_string())?;
    let mut command = tokio::process::Command::new(program);
    command.args(args);
    environment(&mut command, env);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().map_err(|error| format!("hook: {error}"))?;
    let group = child.id().map(|pid| pid as i32);
    // A coordinator that ends mid-hook takes the hook's group with it.
    let mut guard = Group(group);
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let run = async {
        let write = async {
            if let Some(mut pipe) = stdin.take() {
                let _ = pipe.write_all(&input).await;
            }
        };
        let out = async {
            if let Some(pipe) = stdout.take() {
                let _ = tokio::io::copy(&mut pipe.take(HOOK_OUTPUT), &mut tokio::io::sink()).await;
            }
        };
        let err = async {
            if let Some(pipe) = stderr.take() {
                let _ = tokio::io::copy(&mut pipe.take(HOOK_OUTPUT), &mut tokio::io::sink()).await;
            }
        };
        tokio::join!(write, out, err);
        child.wait().await
    };
    let result = tokio::time::timeout(HOOK_TIME, run).await;
    if result.is_ok() {
        // Reaped: the group ID may belong to someone else from here on.
        guard.0 = None;
    }
    match result {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(format!("hook exited with {status}")),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => {
            drop((stdout, stderr));
            if let Some(group) = group {
                stop_group(group).await;
            }
            guard.0 = None;
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            Err("hook did not finish in 5 s and was stopped".into())
        }
    }
}

/// A hook's process group, killed if its future is dropped before the hook
/// is reaped.
struct Group(Option<i32>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            // SAFETY: the leader is not reaped, so the group ID is the hook's.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
    }
}

async fn stop_group(group: i32) {
    // SAFETY: signals to the hook's own group.
    unsafe { libc::killpg(group, libc::SIGTERM) };
    tokio::time::sleep(Duration::from_secs(2)).await;
    // SAFETY: as above; a group already gone is fine.
    unsafe { libc::killpg(group, libc::SIGKILL) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_text_cannot_style_or_end_a_command() {
        assert_eq!(tmux_text("a#[fg=red]b"), "a##[fg=red]b");
        assert_eq!(tmux_text("line\nnext\x1b[2J"), "line next [2J");
        assert_eq!(tmux_text("ends;"), "ends; ");
    }
}
