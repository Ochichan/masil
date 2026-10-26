//! Answering OpenCode's and Kilo's permission and question requests from
//! masil (docs/agent-answers.md). Only an OpenCode or Kilo started with
//! `agent start --answers` has a channel: masil starts it with a local port
//! and a fresh server password, finds that port from the pane process's own
//! listening sockets, and checks that the server refuses requests without
//! the password before using it.
//!
//! A same-user agent can reach the same port (OpenCode's tools inherit the
//! password), so this is a convenience with accident checks, not a security
//! boundary; see the docs.

use super::operations::{Admission, NewOperation, Record, Store};
use super::{Agent, Manager};
use crate::observation::now_ms;
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

/// What differs between the servers `--answers` supports. The flag and the
/// OpenCode channel name stay as they are: launch records and run evidence
/// already carry them.
pub(crate) struct AnswerProfile {
    /// The provider's id.
    pub(crate) id: &'static str,
    /// The provider's name in messages.
    pub(crate) label: &'static str,
    /// The flag `agent start --answers` passes to `exec-managed`.
    pub(super) exec_flag: &'static str,
    /// Recorded in run evidence when a start asked for the channel.
    pub(crate) channel: &'static str,
    password_env: &'static str,
    username_env: &'static str,
    /// The user the server's basic authentication expects.
    username: &'static str,
    /// The release range checked live; a new minor is checked before
    /// widening.
    minimum: (u64, u64, u64),
    below: (u64, u64),
    /// Options of the TUI that take a value.
    valued: &'static [&'static str],
}

const PROFILES: &[AnswerProfile] = &[
    AnswerProfile {
        id: "opencode",
        label: "OpenCode",
        exec_flag: "--opencode-answers",
        channel: "opencode_port",
        password_env: "OPENCODE_SERVER_PASSWORD",
        username_env: "OPENCODE_SERVER_USERNAME",
        username: "opencode",
        minimum: (1, 18, 34),
        below: (1, 20),
        valued: &[
            "-m",
            "--model",
            "-s",
            "--session",
            "--prompt",
            "--agent",
            "--log-level",
        ],
    },
    AnswerProfile {
        id: "kilo",
        label: "Kilo",
        exec_flag: "--kilo-answers",
        channel: "kilo_port",
        password_env: "KILO_SERVER_PASSWORD",
        username_env: "KILO_SERVER_USERNAME",
        username: "kilo",
        minimum: (7, 8, 17),
        below: (7, 9),
        valued: &[
            "-m",
            "--model",
            "-s",
            "--session",
            "--prompt",
            "--agent",
            "--log-level",
            "--worktree",
            "--replay-limit",
        ],
    },
];

/// The profile of a provider that can have an answer channel.
pub(crate) fn profile(provider: &str) -> Option<&'static AnswerProfile> {
    PROFILES.iter().find(|profile| profile.id == provider)
}

/// The profile whose `exec-managed` flag this is.
pub(super) fn profile_for_flag(flag: &str) -> Option<&'static AnswerProfile> {
    PROFILES.iter().find(|profile| profile.exec_flag == flag)
}

/// Whether `channel` is a profile's: what a row's `channel` holds when the
/// run has one.
pub(crate) fn is_channel(channel: &str) -> bool {
    PROFILES.iter().any(|profile| profile.channel == channel)
}

const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const LIST_LIMIT: usize = 4 << 20;
const FIELD_LIMIT: usize = 2048;
/// Routes each API group guards; all must refuse a request without the
/// password.
const GUARDED: &[&str] = &["/permission", "/question", "/session"];
/// Options that would change where or how the server listens.
const NETWORK: &[&str] = &[
    "--port",
    "--hostname",
    "--mdns",
    "--no-mdns",
    "--mdns-domain",
    "--cors",
    "--mini",
];

/// Refuses user arguments with which `--answers` cannot safely add its
/// listening options: positional words (subcommands), `--`, and network
/// options. Unknown options followed by a bare word count as positional.
pub(super) fn check_start_args(profile: &AnswerProfile, args: &[String]) -> Result<(), String> {
    let refuse = |why: &str| {
        Err(format!(
            "invalid_argument: --answers needs the {} TUI without {why}",
            profile.label
        ))
    };
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let name = arg.split('=').next().unwrap_or(arg);
        if arg == "--" {
            return refuse("`--`");
        }
        if !arg.starts_with('-') {
            return refuse("subcommands or positional arguments");
        }
        // `--no-port` and the like are network options too.
        let positive = name.strip_prefix("--no-").map(|rest| format!("--{rest}"));
        if NETWORK.contains(&name)
            || positive.is_some_and(|positive| NETWORK.contains(&positive.as_str()))
        {
            return refuse("its own network options");
        }
        // The TUI does not take an option as another option's value, so a
        // word starting with `-` is checked as an option of its own.
        let valued = profile.valued.contains(&name)
            && !arg.contains('=')
            && args
                .get(index + 1)
                .is_some_and(|value| !value.starts_with('-'));
        index += if valued { 2 } else { 1 };
    }
    Ok(())
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let word = text
        .split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = word.split('.');
    let mut next = || parts.next()?.parse::<u64>().ok();
    Some((next()?, next()?, next()?))
}

fn supported(profile: &AnswerProfile, version: (u64, u64, u64)) -> bool {
    version >= profile.minimum && (version.0, version.1) < profile.below
}

/// The exit status and first LIST_LIMIT bytes of output of a command that
/// ends within `limit`; None when it cannot start or runs longer.
fn output_within(command: &mut std::process::Command, limit: Duration) -> Option<(bool, Vec<u8>)> {
    use std::io::Read;
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    // Read while waiting, so a full pipe does not stall the child. A
    // process the child left behind may hold the pipe open: the read is
    // bounded by the same deadline and otherwise left to end on its own.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout
            .take(LIST_LIMIT as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(read);
    });
    let deadline = std::time::Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                let output = receiver.recv_timeout(left).ok()?.ok()?;
                return Some((status.success(), output));
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// `program --version`, within five seconds; None when it fails or is not a
/// plain release number.
fn version(program: &str) -> Option<(u64, u64, u64)> {
    let (success, output) = output_within(
        std::process::Command::new(program).arg("--version"),
        Duration::from_secs(5),
    )?;
    if !success {
        return None;
    }
    let text = String::from_utf8_lossy(&output);
    // A development build has a suffix; only plain releases qualify.
    let version = parse_version(&text)?;
    (text.split_whitespace().last()? == format!("{}.{}.{}", version.0, version.1, version.2))
        .then_some(version)
}

fn secret() -> Option<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// In `exec-managed`, just before the provider starts, in the pane's own
/// directory and environment: adds the listening options and a fresh
/// password together, or nothing. Returns the environment to set.
pub(super) async fn prepare_exec(
    profile: &'static AnswerProfile,
    native: &crate::native_ui::Context,
    socket: &Path,
    run: &str,
    argv: &mut Vec<String>,
) -> Vec<(&'static str, String)> {
    let program = argv[0].clone();
    let Some(version) = tokio::task::spawn_blocking(move || version(&program))
        .await
        .ok()
        .flatten()
    else {
        return Vec::new();
    };
    if !supported(profile, version) {
        return Vec::new();
    }
    // Never empty: an empty password turns the server's authentication off.
    let Some(secret) = secret() else {
        return Vec::new();
    };
    // Best-effort: without the stored copy the channel is unavailable, but
    // the port still has a password nobody knows.
    if let Ok(output) = native
        .tmux(
            ["show-options", "-gqv", super::STORE_OPTION]
                .iter()
                .map(std::ffi::OsString::from),
            None,
        )
        .await
    {
        let instance = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let socket = socket.to_owned();
        let run = run.to_owned();
        let value = secret.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let socket = socket.canonicalize().map_err(|error| error.to_string())?;
            let mut store = Store::open_existing(&socket, &instance)?
                .ok_or("the operation store is missing or replaced")?;
            store.put_secret(&run, profile.id, &value, now_ms())
            // The connection closes here, before the provider starts.
        })
        .await;
    }
    argv.splice(
        1..1,
        ["--port", "0", "--hostname", "127.0.0.1"].map(str::to_owned),
    );
    let mut environment = vec![
        (profile.password_env, secret),
        (profile.username_env, profile.username.into()),
    ];
    match profile.id {
        // For the exported plugin's shell.env hook: it gives the agent's
        // shell the password the user had, not this one.
        "opencode" => environment.extend([
            ("MASIL_OPENCODE_ANSWERS", "1".into()),
            (
                "MASIL_OPENCODE_ORIGINAL_PASSWORD",
                std::env::var(profile.password_env).unwrap_or_default(),
            ),
        ]),
        // Required: with a `kilo daemon` running, Kilo 7.8.17 attaches the
        // TUI to the daemon and opens no listener of its own, ignoring the
        // port and password masil set. Unset, the pane would have no server.
        "kilo" => environment.push(("KILO_NO_DAEMON", "1".into())),
        _ => {}
    }
    environment
}

/// 127.0.0.1 ports `pid` listens on.
fn listening(pid: i32) -> Vec<u16> {
    #[cfg(target_os = "macos")]
    {
        // lsof exits 1 when nothing matches; its output still counts.
        output_within(
            std::process::Command::new("/usr/sbin/lsof").args([
                "-a",
                "-p",
                &pid.to_string(),
                "-iTCP",
                "-sTCP:LISTEN",
                "-nP",
                "-Fn",
            ]),
            Duration::from_secs(3),
        )
        .map_or_else(Vec::new, |(_, output)| {
            parse_lsof(&String::from_utf8_lossy(&output))
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let inodes = socket_inodes(pid);
        std::fs::read_to_string(format!("/proc/{pid}/net/tcp"))
            .map(|text| parse_proc_tcp(&text, &inodes))
            .unwrap_or_default()
    }
}

/// As `listening`, but starts no process: `lsof` costs one, and a reader that
/// looks often would spend its budget on it. libproc on macOS, /proc elsewhere
/// (where `listening` already reads /proc).
fn listening_quietly(pid: i32) -> Vec<u16> {
    #[cfg(target_os = "macos")]
    {
        libproc_listening(pid)
    }
    #[cfg(not(target_os = "macos"))]
    {
        listening(pid)
    }
}

/// `struct socket_fdinfo` from the macOS SDK's sys/proc_info.h: libc has
/// `proc_pidinfo`, `proc_pidfdinfo`, `proc_fdinfo` and `PROC_PIDLISTFDS` but
/// not this struct or `PROC_PIDFDSOCKETINFO`, and its `socket_info` ends in a
/// union of larger structs that nothing here needs. The call fills a buffer
/// of this size, and the fields used are read at their offsets in it (taken
/// from the SDK with `offsetof`). Any other size back is an unknown layout
/// and counts as no listener.
const SOCKET_FDINFO_SIZE: usize = 792;
#[cfg(target_os = "macos")]
const PROC_PIDFDSOCKETINFO: i32 = 3;
/// `psi.soi_protocol`, `psi.soi_family` and `psi.soi_kind` (24 + 156, 160, 232).
const SOI_PROTOCOL: usize = 180;
const SOI_FAMILY: usize = 184;
const SOI_KIND: usize = 256;
/// In `psi.soi_proto.pri_tcp` (at 264): `tcpsi_ini.insi_lport` (+4, an int
/// holding the port in network byte order), `insi_vflag` (+24), the IPv4
/// address in the last four bytes of `insi_laddr` (+48+12) and `tcpsi_state`
/// (+80).
const TCP_LPORT: usize = 268;
const TCP_VFLAG: usize = 288;
const TCP_LADDR4: usize = 324;
const TCP_STATE: usize = 344;
/// `SOCKINFO_TCP`, `INI_IPV4` and `TSI_S_LISTEN`.
const SOCKINFO_TCP: i32 = 2;
const INI_IPV4: u8 = 1;
const TSI_S_LISTEN: i32 = 1;

fn read_i32(info: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_ne_bytes(info.get(at..at + 4)?.try_into().ok()?))
}

/// The port of a `socket_fdinfo` that is a TCP socket listening on
/// 127.0.0.1 (the `parse_lsof` filter); None for any other socket.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_listener(info: &[u8]) -> Option<u16> {
    let listening = info.len() == SOCKET_FDINFO_SIZE
        && read_i32(info, SOI_FAMILY)? == libc::AF_INET
        && read_i32(info, SOI_PROTOCOL)? == libc::IPPROTO_TCP
        && read_i32(info, SOI_KIND)? == SOCKINFO_TCP
        && info[TCP_VFLAG] & INI_IPV4 != 0
        && read_i32(info, TCP_STATE)? == TSI_S_LISTEN
        && info[TCP_LADDR4..TCP_LADDR4 + 4] == [127, 0, 0, 1];
    if !listening {
        return None;
    }
    Some(u16::from_be(read_i32(info, TCP_LPORT)? as u16))
}

/// The ports `pid` listens on, from its descriptors, without a process.
#[cfg(target_os = "macos")]
fn libproc_listening(pid: i32) -> Vec<u16> {
    use std::mem::size_of;
    let entry = size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer of size 0 only asks how much room the list needs.
    let wanted =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if wanted <= 0 {
        return Vec::new();
    }
    // Room for descriptors opened since the size was asked.
    let capacity = wanted as usize / entry + 16;
    let mut fds = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        capacity
    ];
    // SAFETY: the buffer holds `capacity` entries; the call writes at most
    // the byte count it is given.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            (capacity * entry) as libc::c_int,
        )
    };
    if written <= 0 {
        return Vec::new();
    }
    let count = (written as usize / entry).min(capacity);
    let mut info = vec![0u8; SOCKET_FDINFO_SIZE];
    let mut ports = Vec::new();
    for fd in fds[..count]
        .iter()
        .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32)
    {
        // SAFETY: `info` holds SOCKET_FDINFO_SIZE bytes, the size passed.
        let got = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd.proc_fd,
                PROC_PIDFDSOCKETINFO,
                info.as_mut_ptr().cast(),
                SOCKET_FDINFO_SIZE as libc::c_int,
            )
        };
        if usize::try_from(got) == Ok(SOCKET_FDINFO_SIZE)
            && let Some(port) = parse_listener(&info)
        {
            ports.push(port);
        }
    }
    ports
}

/// `lsof -Fn` lines name sockets as `n127.0.0.1:PORT` (macOS; tested on
/// every system).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_lsof(text: &str) -> Vec<u16> {
    text.lines()
        .filter_map(|line| line.strip_prefix("n127.0.0.1:"))
        .filter_map(|port| port.parse().ok())
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn socket_inodes(pid: i32) -> std::collections::HashSet<String> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .into_iter()
        .flatten()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter_map(|target| {
            target
                .to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')
                .map(str::to_owned)
        })
        .collect()
}

/// Listening (state 0A) sockets on 127.0.0.1 owned by one of `inodes`.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn parse_proc_tcp(text: &str, inodes: &std::collections::HashSet<String>) -> Vec<u16> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (address, port) = fields.get(1)?.split_once(':')?;
            (address == "0100007F" && *fields.get(3)? == "0A" && inodes.contains(*fields.get(9)?))
                .then(|| u16::from_str_radix(port, 16).ok())
                .flatten()
        })
        .collect()
}

/// The processes whose listening sockets may be the pane's server: the pane
/// process, and when it is a node or bun launcher, its direct children
/// identified as the provider `provider` in its foreground group (Kilo's
/// launcher runs the compiled `.kilo` as such a child).
fn candidates(pid: i32, provider: &str) -> Vec<i32> {
    let mut candidates = vec![pid];
    let launcher = crate::process::argv(pid)
        .and_then(|argv| argv.first().cloned())
        .is_some_and(|first| {
            let name = Path::new(&first)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            matches!(name.as_str(), "node" | "nodejs" | "bun")
        });
    if launcher {
        let group = crate::process::info(pid).map(|info| info.group);
        candidates.extend(crate::process::children(pid).into_iter().filter(|child| {
            crate::process::info(*child).map(|info| info.group) == group
                && crate::process::argv(*child)
                    .and_then(|argv| crate::providers::identify_process(&argv))
                    .is_some_and(|found| found.id == provider)
        }));
    }
    candidates
}

pub(super) struct Endpoint {
    profile: &'static AnswerProfile,
    client: Client,
    port: u16,
    secret: String,
}

/// Whether `port` is one of the ports the pane's processes listen on.
fn still_listening(ports: &[u16], port: u16) -> bool {
    ports.contains(&port)
}

impl Endpoint {
    /// Whether the processes `discover` would try for the pane (`pid`) still
    /// listen on this endpoint's port. A server that closed its port, or one
    /// that restarted on another, may have left the port to a process that
    /// is not the pane's, and the password goes only to the pane's own
    /// server. Starts no process, unlike `discover`.
    pub(super) async fn still_served_by(&self, pid: i32) -> bool {
        let (port, provider) = (self.port, self.profile.id);
        tokio::task::spawn_blocking(move || {
            pid > 1
                && still_listening(
                    &candidates(pid, provider)
                        .into_iter()
                        .flat_map(listening_quietly)
                        .collect::<Vec<_>>(),
                    port,
                )
        })
        .await
        .unwrap_or(false)
    }

    /// The provider this endpoint serves.
    pub(super) fn profile(&self) -> &'static AnswerProfile {
        self.profile
    }

    /// Ends the session's current turn: a positive answer that it stopped.
    pub(super) async fn abort(&self, session: &str) -> Result<(), String> {
        match self
            .send(
                Method::POST,
                &format!("/session/{session}/abort"),
                None,
                true,
            )
            .await
        {
            Ok((status, _)) if status.is_success() => Ok(()),
            // A server error may have come after the abort took.
            Ok((status, _)) if status.is_server_error() => Err(format!(
                "outcome_unknown: {} answered {status} to the abort",
                self.profile.label
            )),
            Ok((status, _)) => Err(format!(
                "not_applied: {} answered {status} to the abort",
                self.profile.label
            )),
            Err(error) => Err(format!("outcome_unknown: the abort got no answer: {error}")),
        }
    }

    /// `GET /event`: the server's event stream, open until either side
    /// ends it. Only connecting has a time limit.
    pub(super) async fn events(&self) -> Result<reqwest::Response, String> {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|error| error.to_string())?;
        let response = client
            .get(format!("http://127.0.0.1:{}/event", self.port))
            .basic_auth(self.profile.username, Some(&self.secret))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|error| format!("answer_unavailable: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "answer_unavailable: {} answered {} for /event",
                self.profile.label,
                response.status()
            ));
        }
        Ok(response)
    }

    /// A JSON page and the cursor OpenCode gives for the next older one.
    pub(super) async fn get_page(&self, path: &str) -> Result<(Value, Option<String>), String> {
        let response = self
            .client
            .get(format!("http://127.0.0.1:{}{path}", self.port))
            .basic_auth(self.profile.username, Some(&self.secret))
            .send()
            .await
            .map_err(|error| format!("answer_unavailable: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "answer_unavailable: {} answered {} for {path}",
                self.profile.label,
                response.status()
            ));
        }
        let cursor = response
            .headers()
            .get("x-next-cursor")
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= 512 && value.bytes().all(|b| b.is_ascii_graphic()))
            .map(str::to_owned);
        let (_, body) = read_limited(response).await?;
        let value = serde_json::from_slice(&body).map_err(|_| {
            format!(
                "answer_unavailable: {} sent no JSON for {path}",
                self.profile.label
            )
        })?;
        Ok((value, cursor))
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        authenticated: bool,
    ) -> Result<(StatusCode, Vec<u8>), String> {
        call(
            &self.client,
            self.port,
            method,
            path,
            body,
            authenticated.then_some((self.profile.username, &self.secret)),
        )
        .await
    }

    pub(super) async fn get_json(&self, path: &str) -> Result<Value, String> {
        let (status, body) = self.send(Method::GET, path, None, true).await?;
        if !status.is_success() {
            return Err(format!(
                "answer_unavailable: {} answered {status} for {path}",
                self.profile.label
            ));
        }
        serde_json::from_slice(&body).map_err(|_| {
            format!(
                "answer_unavailable: {} sent no JSON for {path}",
                self.profile.label
            )
        })
    }
}

async fn call(
    client: &Client,
    port: u16,
    method: Method,
    path: &str,
    body: Option<Value>,
    login: Option<(&str, &String)>,
) -> Result<(StatusCode, Vec<u8>), String> {
    let mut request = client.request(method, format!("http://127.0.0.1:{port}{path}"));
    if let Some((user, secret)) = login {
        request = request.basic_auth(user, Some(secret));
    }
    if let Some(body) = body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("answer_unavailable: {error}"))?;
    read_limited(response).await
}

/// The status and body, refusing a body over LIST_LIMIT.
async fn read_limited(mut response: reqwest::Response) -> Result<(StatusCode, Vec<u8>), String> {
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("answer_unavailable: {error}"))?
    {
        if body.len() + chunk.len() > LIST_LIMIT {
            return Err("answer_unavailable: the server's answer is too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, body))
}

fn client() -> Result<Client, String> {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(HTTP_TIMEOUT)
        // A local connection is cheap to open; an idle one kept for later
        // would outlive a quiet observer's stream.
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|error| error.to_string())
}

/// The server of the process group `pid`: exactly one local listener of its
/// own processes that refuses every guarded route without the password and
/// is healthy with it. The password goes only to a port that asked for it.
pub(super) async fn discover(
    profile: &'static AnswerProfile,
    secret: String,
    pid: i32,
) -> Result<Endpoint, String> {
    let label = profile.label;
    if pid <= 1 {
        return Err(format!(
            "answer_unavailable: the pane's {label} is not running"
        ));
    }
    let ports = tokio::task::spawn_blocking(move || {
        candidates(pid, profile.id)
            .into_iter()
            .flat_map(listening)
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|error| error.to_string())?;
    let client = client()?;
    let mut usable = Vec::new();
    let mut unsafe_port = false;
    for port in ports {
        let mut refuses = true;
        let mut answered = false;
        for path in GUARDED {
            match call(&client, port, Method::GET, path, None, None).await {
                Ok((status, _)) if status == StatusCode::UNAUTHORIZED => {}
                // A server that served the route took the request.
                Ok((status, _)) if status.is_success() => answered = true,
                // Another status or no answer proves nothing either way:
                // the port is not usable now.
                _ => refuses = false,
            }
        }
        if answered {
            unsafe_port = true;
            continue;
        }
        if !refuses {
            continue;
        }
        let healthy = call(
            &client,
            port,
            Method::GET,
            "/global/health",
            None,
            Some((profile.username, &secret)),
        )
        .await
        .is_ok_and(|(status, body)| {
            status.is_success()
                && serde_json::from_slice::<Value>(&body)
                    .is_ok_and(|value| value["healthy"] == true)
        });
        if healthy {
            usable.push(port);
        }
    }
    match usable.as_slice() {
        [port] => Ok(Endpoint {
            profile,
            client,
            port: *port,
            secret,
        }),
        [] if unsafe_port => Err(format!(
            "answer_unsafe: the pane's {label} server answered without its password; masil will not use it"
        )),
        [] => Err(format!(
            "answer_unavailable: the pane's {label} server is not listening yet"
        )),
        _ => Err(format!(
            "answer_unavailable: more than one {label} server belongs to this pane"
        )),
    }
}

/// Whether the agent asked for a channel at start.
pub(super) fn requested(agent: &Agent) -> bool {
    profile(&agent.provider).is_some_and(|profile| {
        agent
            .run_evidence
            .as_ref()
            .and_then(|evidence| evidence.answer_channel.as_deref())
            == Some(profile.channel)
    })
}

/// Whether an inventory row is a pane of an `--answers` run, read from the
/// row's own registration and run evidence. A watch that serves only these
/// runs picks their panes with it before it captures anything.
pub(super) fn requested_in(fields: &[String]) -> bool {
    let Some(metadata) = super::valid_metadata(fields) else {
        return false;
    };
    profile(&metadata.provider).is_some_and(|profile| {
        super::decode::<super::RunEvidence>(&fields[16]).is_some_and(|evidence| {
            evidence.run == metadata.run
                && evidence.answer_channel.as_deref() == Some(profile.channel)
        })
    })
}

impl Manager {
    /// The run's server and bound session, for ending its turn
    /// (`POST /session/ID/abort`). None when the run has no answer channel
    /// or no bound session, or the server cannot be reached; the interrupt
    /// keys are used then.
    pub(super) async fn abort_channel(&self, agent: &Agent) -> Option<(Endpoint, String)> {
        if !requested(agent) {
            return None;
        }
        let session = agent.session_id.as_deref().filter(|session| {
            !session.is_empty()
                && session.len() <= 128
                && session
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })?;
        let endpoint = self.answer_endpoint(agent).await.ok()?;
        Some((endpoint, session.to_owned()))
    }

    /// The run's server password; secrets of runs that ended are dropped
    /// on the way.
    async fn answer_secret(
        &self,
        profile: &AnswerProfile,
        run: &str,
    ) -> Result<Option<String>, String> {
        let (live, _) = self.live_runs().await?;
        let mut store = self.operation_store().await?;
        let run = run.to_owned();
        let provider = profile.id;
        tokio::task::spawn_blocking(move || {
            store.prune_secrets(&live, now_ms())?;
            let secret = store.secret(&run, provider)?;
            Ok::<_, String>(secret)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    /// The pane's server: exactly one local listener of its own processes
    /// that answers with the password and refuses without it.
    async fn answer_endpoint(&self, agent: &Agent) -> Result<Endpoint, String> {
        let profile = profile(&agent.provider)
            .ok_or("answer_channel_none: this provider has no answer channel")?;
        let secret = self.answer_secret(profile, &agent.run).await?.ok_or(
            "answer_unavailable: no server password was recorded for this run; start it again with --answers",
        )?;
        // A dead pane's recorded group may belong to another process now.
        if agent.process != "running" || agent.foreground_group <= 1 {
            return Err(format!(
                "answer_unavailable: the pane's {} is not running",
                profile.label
            ));
        }
        discover(profile, secret, agent.foreground_group).await
    }

    /// Records what the start's check found: the coordinator observes only
    /// a ready channel. A ready one also marks the server so that autosave
    /// brings the coordinator back.
    pub(super) async fn record_answer_channel(
        &self,
        pane: &str,
        state: &'static str,
    ) -> Result<(), String> {
        let agent = self.get(pane).await?;
        let mut evidence = agent
            .run_evidence
            .clone()
            .filter(|evidence| evidence.run == agent.run)
            .ok_or("identity_mismatch: the run has no evidence")?;
        if evidence.answer_channel_state.as_deref() != Some(state) {
            evidence.answer_channel_state = Some(state.into());
            let outcome = self
                .guarded_input_outcome(
                    &agent,
                    vec![Self::option(
                        &agent,
                        super::EVIDENCE,
                        super::encode(&evidence)?,
                    )],
                    None,
                )
                .await?;
            if matches!(outcome, super::Guarded::Rejected) {
                return Err("identity_mismatch: the pane changed".into());
            }
        }
        if state == "ready" {
            self.command(&["set-option", "-gq", "@masil-answers", "on"])
                .await?;
        }
        Ok(())
    }

    /// Drops the secrets of runs not in `live` (after a short grace).
    pub(super) async fn prune_answer_secrets(
        &self,
        live: &std::collections::HashSet<String>,
    ) -> Result<(), String> {
        let socket = self.native.socket.clone();
        let live = live.clone();
        // A store that was never created holds no secret.
        tokio::task::spawn_blocking(move || {
            let Some(base) = super::operations::state_base().ok() else {
                return Ok(());
            };
            if !super::operations::Store::enabled_features(&base, &socket)?
                .iter()
                .any(|feature| feature == "answers")
            {
                return Ok(());
            }
            let mut store = super::operations::Store::open(&socket)?;
            store.prune_secrets(&live, now_ms()).map(drop)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    /// The run's server password, without pruning (the coordinator prunes
    /// with its ended-run sweep).
    pub(super) async fn run_secret(
        &self,
        profile: &AnswerProfile,
        run: &str,
    ) -> Result<Option<String>, String> {
        let provider = profile.id;
        // The coordinator's own connection opens only while the inbox is on.
        if self.is_resident() {
            let run = run.to_owned();
            if let Some(secret) = self
                .with_resident_store(move |store| store.secret(&run, provider))
                .await?
            {
                return Ok(secret);
            }
        }
        let run = run.to_owned();
        let store = self.operation_store().await?;
        tokio::task::spawn_blocking(move || store.secret(&run, provider))
            .await
            .map_err(|error| error.to_string())?
    }

    /// Checks a fresh `--answers` start: the server must refuse requests
    /// without its password. One that does not is stopped.
    pub(super) async fn verify_answers(&self, agent: &Agent) -> Result<&'static str, String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            // The pane is read again each time: right after the start it can
            // still run exec-managed, which then becomes the provider.
            let current = self
                .get(&agent.pane_id)
                .await
                .ok()
                .filter(|current| current.run == agent.run);
            let result = match &current {
                Some(current) => self.answer_endpoint(current).await,
                None => Err("answer_unavailable: the pane runs another program".into()),
            };
            match result {
                Ok(_) => return Ok("ready"),
                Err(error) if error.starts_with("answer_unsafe") => {
                    // answer_endpoint only probes a running pane's group above 1.
                    let group = current.map_or(0, |current| current.foreground_group);
                    if group > 1 {
                        // SAFETY: kill on the pane's provider process group.
                        unsafe { libc::kill(-group, libc::SIGTERM) };
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        unsafe { libc::kill(-group, libc::SIGKILL) };
                    }
                    let label =
                        profile(&agent.provider).map_or("provider", |profile| profile.label);
                    return Err(format!("{error}; the {label} process was stopped"));
                }
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(_) => return Ok("unavailable"),
            }
        }
    }

    /// `agent requests TARGET`: the pending requests of the pane's server.
    pub(crate) async fn requests(&self, agent: &Agent) -> Result<Value, String> {
        let Some(profile) = profile(&agent.provider).filter(|_| requested(agent)) else {
            return Ok(no_channel(agent));
        };
        let endpoint = self.answer_endpoint(agent).await?;
        let pending = pending(&endpoint).await?;
        let root = agent.session_id.as_deref();
        let requests = pending
            .iter()
            .map(|request| public_request(request, root))
            .collect::<Vec<_>>();
        Ok(json!({"channel": profile.channel, "requests": requests}))
    }

    /// `agent answer TARGET REQUEST ...`, once per request key.
    pub(crate) async fn answer(
        &self,
        agent: &Agent,
        request: &str,
        reply: Reply,
    ) -> Result<Value, String> {
        refuse_agent_caller()?;
        let Some(profile) = profile(&agent.provider).filter(|_| requested(agent)) else {
            return Err(format!(
                "answer_channel_none: {}",
                no_channel(agent)["reason"]
                    .as_str()
                    .unwrap_or("no answer channel")
            ));
        };
        let endpoint = self.answer_endpoint(agent).await?;
        let (path, body) = reply.post(request);
        let body_text = body.to_string();
        let digest = super::prompt::sha256(&format!("{path}\n{body_text}"));
        let namespace = format!("run:{}", agent.run);
        let mut store = self.operation_store().await?;
        let earlier = store.get(&super::operations::key(&namespace, "answer", request))?;
        let before = pending(&endpoint).await?;
        let Some(found) = before.iter().find(|item| item["id"] == request) else {
            // The server drops a request once answered, so a repeat is told
            // what happened to it from the record.
            return match earlier {
                // An attempt that did not take leaves the request to whoever
                // answered it since.
                Some(record)
                    if record.digest == digest
                        && matches!(
                            record.state.as_str(),
                            "rejected_before_effect" | "not_applied"
                        ) =>
                {
                    Err(format!("expired: request {request} is no longer pending"))
                }
                Some(record)
                    if record.digest == digest
                        && record.state != super::operations::DISPATCHING =>
                {
                    recorded(&record)
                }
                Some(record) if record.state != super::operations::DISPATCHING => Err(format!(
                    "expired: request {request} is no longer pending; an earlier, different answer ended as {}",
                    record.state
                )),
                _ => Err(format!("expired: request {request} is not pending")),
            };
        };
        reply.check(found)?;
        // An unknown outcome whose request is still pending did not take,
        // once the server has had as long as a request may take to apply it.
        if let Some(record) = earlier.filter(|record| record.state == super::operations::UNKNOWN) {
            if now_ms() < record.updated_ms + HTTP_TIMEOUT.as_millis() as u64 {
                return Err(format!(
                    "outcome_unknown: the last answer to request {request} may still apply; try again in a few seconds"
                ));
            }
            store.settle_unknown(&record, "not_applied", "reconcile", None, now_ms())?;
        }
        let mut reconciled = false;
        loop {
            let new = NewOperation {
                namespace: &namespace,
                action: "answer",
                id: request,
                explicit: false,
                target: &agent.pane_id,
                run: Some(&agent.run),
                boot: &agent.boot,
                digest: &digest,
                payload_bytes: body_text.len() as u64,
            };
            let ticket = match store.admit(&new, json!({"path": path}), now_ms())? {
                Admission::Dispatch(ticket) => ticket,
                Admission::Recorded(record) => return recorded(&record),
                Admission::NeedsReconcile(record) if !reconciled => {
                    // Still pending: the earlier attempt did not take.
                    let still = pending(&endpoint)
                        .await?
                        .iter()
                        .any(|item| item["id"] == request);
                    let state = if still {
                        "not_applied"
                    } else {
                        super::operations::UNKNOWN
                    };
                    store.resolve(&record, state, "reconcile", None, now_ms())?;
                    reconciled = true;
                    continue;
                }
                Admission::NeedsReconcile(_) => {
                    return Err(
                        "outcome_unknown: an earlier answer is unresolved; see `agent operations`"
                            .into(),
                    );
                }
            };
            // `always` also allows the session's other waiting permissions
            // that its patterns cover, judged with rules masil cannot see:
            // with any of them waiting now, it is not sent.
            let mut checked = None;
            if matches!(reply, Reply::Always) {
                let now = pending(&endpoint).await;
                let others: Result<bool, String> = match &now {
                    Ok(now) => Ok(now.iter().any(|item| {
                        item["_kind"] == "permission"
                            && item["sessionID"] == found["sessionID"]
                            && item["id"] != request
                    })),
                    Err(error) => Err(error.clone()),
                };
                checked = now.ok();
                if !matches!(others, Ok(false)) {
                    store.finish(&ticket, "not_applied", "masil", None, now_ms())?;
                    return Err(match others {
                        Ok(_) => "answer_refused: another permission of this session waits; answer it first, or answer in the pane".into(),
                        Err(error) => error,
                    });
                }
            }
            let sent = endpoint
                .send(Method::POST, &path, Some(body.clone()), true)
                .await;
            let (state, result) = match sent {
                Ok((status, _)) if status.is_success() => ("native_accepted", Ok(())),
                Ok((status, body)) if status == StatusCode::NOT_FOUND && not_found(&body) => (
                    "expired",
                    Err(format!("expired: request {request} is no longer pending")),
                ),
                Ok((status, _)) if status.is_client_error() => (
                    "rejected_before_effect",
                    Err(format!(
                        "rejected_before_effect: {} refused the answer ({status})",
                        profile.label
                    )),
                ),
                Ok((status, _)) => (
                    super::operations::UNKNOWN,
                    Err(format!(
                        "outcome_unknown: {} answered {status}; check the pane",
                        profile.label
                    )),
                ),
                Err(error) => (
                    super::operations::UNKNOWN,
                    Err(format!("outcome_unknown: {error}; check the pane")),
                ),
            };
            store.finish(
                &ticket,
                state,
                profile.id,
                Some(&json!({"path": path})),
                now_ms(),
            )?;
            result?;
            // The server rejects the session's other permissions with a
            // rejected one. `always` is sent only with none waiting at the
            // check just before it, so a request that arrived after the
            // check and was allowed with it is not seen. Null when the list
            // cannot be read again.
            let also = if matches!(reply, Reply::Reject { .. } | Reply::Always) {
                let base = checked.as_ref().unwrap_or(&before);
                pending(&endpoint).await.ok().map(|after| {
                    base.iter()
                        .filter(|item| {
                            item["_kind"] == "permission"
                                && item["sessionID"] == found["sessionID"]
                                && item["id"] != request
                                && !after.iter().any(|other| other["id"] == item["id"])
                        })
                        .filter_map(|item| item["id"].as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
            } else {
                Some(Vec::new())
            };
            let mut effects = vec![super::inbox::Effect::Resolve {
                source: format!("hook:{}", profile.id),
                source_ref: format!("request:{request}"),
                resolution: "answered",
            }];
            effects.extend(
                also.iter()
                    .flatten()
                    .map(|id| super::inbox::Effect::Resolve {
                        source: format!("hook:{}", profile.id),
                        source_ref: format!("request:{id}"),
                        resolution: "superseded",
                    }),
            );
            self.inbox_apply(effects).await;
            return Ok(json!({
                "stage": "native_accepted",
                "operation": ticket.key,
                "request": request,
                "also_answered": also,
                "provider_accepted": true,
            }));
        }
    }
}

/// What a request answer posts.
pub(crate) enum Reply {
    Once,
    /// Allow this and, while the provider's instance lives, what its
    /// `always` patterns cover. Sent only when no other permission of the
    /// session waits (what it would also allow cannot be known).
    Always,
    Reject {
        message: Option<String>,
    },
    Answers(Vec<String>),
    RejectQuestion,
}

impl Reply {
    /// The words `agent answer TARGET REQUEST` takes for this reply, for an
    /// endpoint to parse as its own CLI does.
    pub(crate) fn words(&self) -> Vec<String> {
        let mut words: Vec<String> = match self {
            Self::Once => vec!["--choice".into(), "once".into()],
            Self::Always => vec!["--choice".into(), "always".into()],
            Self::Reject { .. } => vec!["--choice".into(), "reject".into()],
            Self::Answers(answers) => answers
                .iter()
                .flat_map(|answer| ["--answer".to_owned(), answer.clone()])
                .collect(),
            Self::RejectQuestion => vec!["--reject".into()],
        };
        if let Self::Reject {
            message: Some(message),
        } = self
        {
            words.extend(["--message".into(), message.clone()]);
        }
        words
    }
}

impl Reply {
    /// The route and body for request `id`; they depend on the reply alone.
    fn post(&self, id: &str) -> (String, Value) {
        match self {
            Self::Once => (format!("/permission/{id}/reply"), json!({"reply": "once"})),
            Self::Always => (
                format!("/permission/{id}/reply"),
                json!({"reply": "always"}),
            ),
            Self::Reject { message } => {
                let mut body = json!({"reply": "reject"});
                if let Some(message) = message {
                    body["message"] = json!(message);
                }
                (format!("/permission/{id}/reply"), body)
            }
            Self::RejectQuestion => (format!("/question/{id}/reject"), json!({})),
            Self::Answers(answers) => {
                let answers = answers
                    .iter()
                    .map(|answer| vec![answer.clone()])
                    .collect::<Vec<_>>();
                (format!("/question/{id}/reply"), json!({"answers": answers}))
            }
        }
    }

    /// Whether this reply fits the pending request's kind and choices.
    fn check(&self, request: &Value) -> Result<(), String> {
        let kind = request["_kind"].as_str().unwrap_or_default();
        match (self, kind) {
            (Self::Once | Self::Always | Self::Reject { .. }, "permission") => Ok(()),
            (Self::RejectQuestion, "question") => Ok(()),
            (Self::Answers(answers), "question") => {
                let questions = request["questions"].as_array().cloned().unwrap_or_default();
                if answers.len() != questions.len() {
                    return Err(format!(
                        "invalid_argument: this request has {} question(s); give one --answer each",
                        questions.len()
                    ));
                }
                for (answer, question) in answers.iter().zip(&questions) {
                    if question["multiple"] == true {
                        return Err("invalid_argument: a question with several choices is answered in the pane".into());
                    }
                    let labels = question["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|option| option["label"].as_str())
                        .collect::<Vec<_>>();
                    if question["custom"] == false && !labels.contains(&answer.as_str()) {
                        return Err(format!(
                            "invalid_argument: {answer:?} is not one of the choices {labels:?}"
                        ));
                    }
                }
                Ok(())
            }
            (_, "permission") => {
                Err("invalid_argument: a permission takes --choice once|always|reject".into())
            }
            _ => Err(
                "invalid_argument: a question takes --answer TEXT per question, or --reject".into(),
            ),
        }
    }

    #[cfg(test)]
    fn request(&self, request: &Value) -> Result<(String, Value), String> {
        self.check(request)?;
        Ok(self.post(request["id"].as_str().unwrap_or_default()))
    }
}

/// Pending permissions and questions, each tagged with its `_kind`.
async fn pending(endpoint: &Endpoint) -> Result<Vec<Value>, String> {
    let mut all = Vec::new();
    for (path, kind) in [("/permission", "permission"), ("/question", "question")] {
        let value = endpoint.get_json(path).await?;
        for mut item in value.as_array().cloned().unwrap_or_default() {
            if item["id"].as_str().is_none() {
                continue;
            }
            item["_kind"] = json!(kind);
            all.push(item);
        }
    }
    Ok(all)
}

fn not_found(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body).is_ok_and(|value| {
        matches!(
            value["_tag"].as_str().or_else(|| value["name"].as_str()),
            Some("PermissionNotFoundError" | "QuestionNotFoundError")
        )
    })
}

/// The fields a person needs to decide, each string cut to FIELD_LIMIT.
fn public_request(request: &Value, root: Option<&str>) -> Value {
    let session = request["sessionID"].as_str();
    let mut value = json!({
        "id": request["id"],
        "kind": request["_kind"],
        "session_id": session,
    });
    if let Some(root) = root {
        value["root"] = json!(session == Some(root));
    }
    for field in [
        "permission",
        "patterns",
        "always",
        "metadata",
        "questions",
        "tool",
    ] {
        if let Some(field_value) = request.get(field) {
            value[field] = clip(field_value);
        }
    }
    value
}

fn clip(value: &Value) -> Value {
    match value {
        Value::String(text) if text.len() > FIELD_LIMIT => {
            let mut end = FIELD_LIMIT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            Value::String(format!("{}…", &text[..end]))
        }
        Value::Array(items) => Value::Array(items.iter().map(clip).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), clip(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn no_channel(agent: &Agent) -> Value {
    let reason = match agent.provider.as_str() {
        "claude" => "Claude asks in its own window; masil shows the request and focuses the pane",
        "codex" => {
            "Codex runs without its shared daemon under masil, so masil cannot answer it; answer in the pane"
        }
        "opencode" => "this OpenCode was not started with --answers; answer in the pane",
        "kilo" => "this Kilo was not started with --answers; answer in the pane",
        _ => "masil cannot answer this provider; answer in the pane",
    };
    json!({"channel": "none", "reason": reason, "requests": []})
}

/// A replayed answer reports what was recorded.
fn recorded(record: &Record) -> Result<Value, String> {
    match record.state.as_str() {
        state @ ("native_accepted" | "user_confirmed_delivered") => Ok(json!({
            "stage": state,
            "operation": record.operation_key,
            "request": record.id,
            "provider_accepted": true,
            "replayed": true,
        })),
        "expired" => Err(format!(
            "expired: request {} is no longer pending",
            record.id
        )),
        state => Err(format!(
            "{state}: an earlier answer to request {} ended as {state}",
            record.id
        )),
    }
}

/// An agent answering its own requests through masil is refused: no
/// ancestor of this command may be a provider with a terminal. Agents can
/// still go around this (it is an accident check), and an ancestry that
/// cannot be read is refused.
fn refuse_agent_caller() -> Result<(), String> {
    match agent_ancestor() {
        Ok(false) => Ok(()),
        Ok(true) => Err(
            "answer_refused: this command runs inside an agent; answer from a terminal or the management window"
                .into(),
        ),
        Err(Ancestry::Unreadable) => Err(
            "answer_refused: this command's ancestry could not be read; answer from a terminal or the management window"
                .into(),
        ),
        Err(Ancestry::TooDeep) => {
            Err("answer_refused: this command's ancestry is too deep to check".into())
        }
    }
}

/// Why an ancestry could not be checked.
pub(super) enum Ancestry {
    Unreadable,
    TooDeep,
}

/// Whether an ancestor of this command is a provider with a terminal: an
/// agent running masil-agent itself.
pub(super) fn agent_ancestor() -> Result<bool, Ancestry> {
    let mut pid = std::process::id() as i32;
    for _ in 0..64 {
        let parent = match crate::process::info(pid) {
            Some(info) => {
                if pid != std::process::id() as i32
                    && info.tty
                    && crate::process::argv(pid)
                        .and_then(|argv| crate::providers::identify_process(&argv))
                        .is_some()
                {
                    return Ok(true);
                }
                info.parent
            }
            // Another user's process, such as login(1) between a terminal
            // and its shell: not an agent of this user, but its parent is.
            None => crate::process::other_users_parent(pid).ok_or(Ancestry::Unreadable)?,
        };
        if parent <= 1 {
            return Ok(false);
        }
        pid = parent;
    }
    Err(Ancestry::TooDeep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn only_a_registered_opencode_run_with_the_channel_is_an_answers_pane() {
        use super::super::{Metadata, RunEvidence, encode};
        let boot = "00000000-0000-0000-0000-000000000000";
        let row = |provider: &str, run: &str, channel: Option<&str>, evidence_run: &str| {
            let mut row = vec![String::new(); 20];
            row[0] = "%3".into();
            row[5] = boot.into();
            row[6] = "1".into();
            row[13] = "7".into();
            row[11] = encode(&Metadata {
                name: "oc".into(),
                provider: provider.into(),
                boot: boot.into(),
                generation: "1".into(),
                run: run.into(),
                foreground_group: 7,
                ..Metadata::default()
            })
            .unwrap();
            row[16] = encode(&RunEvidence {
                run: evidence_run.into(),
                answer_channel: channel.map(str::to_owned),
                ..RunEvidence::default()
            })
            .unwrap();
            row
        };
        assert!(requested_in(&row(
            "opencode",
            "r1",
            Some("opencode_port"),
            "r1"
        )));
        // No channel asked, or evidence of another run.
        assert!(!requested_in(&row("opencode", "r1", None, "r1")));
        assert!(!requested_in(&row(
            "opencode",
            "r1",
            Some("opencode_port"),
            "r0"
        )));
        // The channel is the provider's own: Kilo's is its own name, and one
        // provider's channel on another's row counts for nothing.
        assert!(requested_in(&row("kilo", "r1", Some("kilo_port"), "r1")));
        assert!(!requested_in(&row(
            "kilo",
            "r1",
            Some("opencode_port"),
            "r1"
        )));
        assert!(!requested_in(&row(
            "opencode",
            "r1",
            Some("kilo_port"),
            "r1"
        )));
        assert!(!requested_in(&row("kilo", "r1", None, "r1")));
        assert!(!requested_in(&row(
            "codex",
            "r1",
            Some("opencode_port"),
            "r1"
        )));
        assert!(!requested_in(&row("codex", "r1", Some("kilo_port"), "r1")));
        // No registration, or no evidence at all.
        let mut bare = row("opencode", "r1", Some("opencode_port"), "r1");
        bare[11].clear();
        assert!(!requested_in(&bare));
        let mut plain = row("opencode", "r1", Some("opencode_port"), "r1");
        plain[16].clear();
        assert!(!requested_in(&plain));
    }

    fn opencode() -> &'static AnswerProfile {
        profile("opencode").unwrap()
    }

    fn kilo() -> &'static AnswerProfile {
        profile("kilo").unwrap()
    }

    #[test]
    fn answers_take_the_tui_with_its_own_options_only() {
        assert!(check_start_args(opencode(), &args(&[])).is_ok());
        assert!(
            check_start_args(
                opencode(),
                &args(&["--model", "anthropic/x", "-c", "--log-level=DEBUG"])
            )
            .is_ok()
        );
        for refused in [
            &["serve"][..],
            &["--log-level", "DEBUG", "serve"],
            &["--"],
            &["--port", "4096"],
            &["--port=4096"],
            &["--hostname=0.0.0.0"],
            &["--mdns"],
            &["--mini"],
            &["--unknown", "word"],
            // An option where a value would go is an option of its own.
            &["--model", "--port=4096"],
            &["--model", "--hostname=0.0.0.0"],
            &["--agent", "--mdns"],
            &["--no-port"],
            &["--no-hostname"],
        ] {
            for profile in [opencode(), kilo()] {
                assert!(
                    check_start_args(profile, &args(refused)).is_err(),
                    "{} {refused:?}",
                    profile.id
                );
            }
        }
        // Kilo's TUI takes a value after these; for OpenCode the word is a
        // positional one.
        for option in ["--worktree", "--replay-limit"] {
            assert!(check_start_args(kilo(), &args(&[option, "x"])).is_ok());
            assert!(check_start_args(kilo(), &args(&["--model", "m", option, "x"])).is_ok());
            assert!(check_start_args(opencode(), &args(&[option, "x"])).is_err());
        }
        let refused = check_start_args(kilo(), &args(&["serve"])).unwrap_err();
        assert!(refused.contains("the Kilo TUI"), "{refused}");
    }

    #[test]
    fn each_provider_has_its_own_profile() {
        assert_eq!(
            profile_for_flag("--opencode-answers").map(|profile| profile.id),
            Some("opencode")
        );
        assert_eq!(
            profile_for_flag("--kilo-answers").map(|profile| profile.id),
            Some("kilo")
        );
        assert!(profile_for_flag("--answers").is_none());
        assert!(profile("codex").is_none());
        // Run evidence and launch records already carry the OpenCode names.
        assert_eq!(opencode().exec_flag, "--opencode-answers");
        assert_eq!(opencode().channel, "opencode_port");
        assert_eq!(kilo().channel, "kilo_port");
        assert!(is_channel("opencode_port"));
        assert!(is_channel("kilo_port"));
        assert!(!is_channel("none"));
        assert!(!is_channel(""));
        assert_ne!(opencode().password_env, kilo().password_env);
        assert_eq!(kilo().username, "kilo");
    }

    #[test]
    fn only_the_checked_release_line_qualifies() {
        assert_eq!(parse_version("1.18.34\n"), Some((1, 18, 34)));
        assert_eq!(parse_version("opencode 1.18.40"), Some((1, 18, 40)));
        assert_eq!(parse_version("7.8.17\n"), Some((7, 8, 17)));
        for version in [(1, 18, 34), (1, 18, 99), (1, 19, 4)] {
            assert!(supported(opencode(), version), "{version:?}");
        }
        for version in [(1, 18, 33), (1, 20, 0), (2, 0, 0), (7, 8, 17)] {
            assert!(!supported(opencode(), version), "{version:?}");
        }
        for version in [(7, 8, 17), (7, 8, 99)] {
            assert!(supported(kilo(), version), "{version:?}");
        }
        for version in [(7, 8, 16), (7, 9, 0), (1, 19, 4)] {
            assert!(!supported(kilo(), version), "{version:?}");
        }
        assert_eq!(parse_version("dev"), None);
    }

    #[test]
    fn listeners_are_read_from_lsof_and_proc() {
        assert_eq!(
            parse_lsof("p123\nf7\nn127.0.0.1:4096\nf9\nn*:5000\nn127.0.0.1:61000\n"),
            vec![4096, 61000]
        );
        let inodes = std::collections::HashSet::from(["777".to_owned()]);
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1000 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 777 1\n   1: 0100007F:1001 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 888 1\n   2: 00000000:1002 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 777 1\n";
        assert_eq!(parse_proc_tcp(table, &inodes), vec![4096]);
    }

    /// A `socket_fdinfo` as libproc fills it, with the fields `parse_listener` reads.
    fn socket_info(listening: bool, address: [u8; 4], port: u16) -> Vec<u8> {
        let mut info = vec![0u8; SOCKET_FDINFO_SIZE];
        let mut put =
            |at: usize, value: i32| info[at..at + 4].copy_from_slice(&value.to_ne_bytes());
        put(SOI_FAMILY, libc::AF_INET);
        put(SOI_PROTOCOL, libc::IPPROTO_TCP);
        put(SOI_KIND, SOCKINFO_TCP);
        put(TCP_LPORT, i32::from(port.to_be()));
        put(TCP_STATE, if listening { TSI_S_LISTEN } else { 4 });
        info[TCP_VFLAG] = INI_IPV4;
        info[TCP_LADDR4..TCP_LADDR4 + 4].copy_from_slice(&address);
        info
    }

    #[test]
    fn only_a_tcp_socket_listening_on_loopback_has_a_port() {
        assert_eq!(
            parse_listener(&socket_info(true, [127, 0, 0, 1], 4096)),
            Some(4096)
        );
        // The port is in network byte order in an int: both bytes count.
        assert_eq!(
            parse_listener(&socket_info(true, [127, 0, 0, 1], 61000)),
            Some(61000)
        );
        assert_eq!(
            parse_listener(&socket_info(true, [127, 0, 0, 1], 258)),
            Some(258)
        );
        // Connected, not listening.
        assert_eq!(
            parse_listener(&socket_info(false, [127, 0, 0, 1], 4096)),
            None
        );
        // Not loopback, as `parse_lsof` leaves `*:PORT` out.
        assert_eq!(parse_listener(&socket_info(true, [0, 0, 0, 0], 4096)), None);
        assert_eq!(
            parse_listener(&socket_info(true, [10, 0, 0, 5], 4096)),
            None
        );
        for (at, value) in [(SOI_FAMILY, 30), (SOI_PROTOCOL, 17), (SOI_KIND, 1)] {
            let mut info = socket_info(true, [127, 0, 0, 1], 4096);
            info[at..at + 4].copy_from_slice(&i32::to_ne_bytes(value));
            assert_eq!(parse_listener(&info), None, "{at}");
        }
        let mut info = socket_info(true, [127, 0, 0, 1], 4096);
        info[TCP_VFLAG] = 2;
        assert_eq!(parse_listener(&info), None);
        // A size other than the layout this was written for.
        let mut info = socket_info(true, [127, 0, 0, 1], 4096);
        info.pop();
        assert_eq!(parse_listener(&info), None);
        assert_eq!(parse_listener(&[]), None);
    }

    #[test]
    fn the_kept_port_is_reused_only_while_the_pane_still_listens_on_it() {
        assert!(still_listening(&[4096, 61000], 61000));
        assert!(!still_listening(&[4096], 61000));
        assert!(!still_listening(&[], 61000));
    }

    #[tokio::test]
    async fn the_listeners_of_a_process_are_read_without_starting_one() {
        let own = std::process::id() as i32;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let wildcard = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let other = wildcard.local_addr().unwrap().port();
        let _connected = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let ports = listening_quietly(own);
        assert!(ports.contains(&port), "{ports:?} lacks {port}");
        // Neither a socket bound to every address nor a connected one.
        assert!(!ports.contains(&other), "{ports:?}");
        let endpoint = Endpoint {
            profile: opencode(),
            client: client().unwrap(),
            port,
            secret: "secret".into(),
        };
        assert!(endpoint.still_served_by(own).await);
        // A process that is not the pane's, or no process, does not serve it.
        assert!(!endpoint.still_served_by(1).await);
        assert!(!endpoint.still_served_by(0).await);
        drop(listener);
        assert!(!listening_quietly(own).contains(&port));
        assert!(!endpoint.still_served_by(own).await);
    }

    #[test]
    fn replies_match_the_request_kind_and_choices() {
        let permission = json!({"id": "per_1", "_kind": "permission"});
        assert_eq!(
            Reply::Once.request(&permission).unwrap().0,
            "/permission/per_1/reply"
        );
        assert!(
            Reply::Answers(vec!["x".into()])
                .request(&permission)
                .is_err()
        );
        let question = json!({"id": "que_1", "_kind": "question", "questions": [
            {"question": "Pick", "options": [{"label": "a"}, {"label": "b"}], "custom": false}
        ]});
        let (path, body) = Reply::Answers(vec!["b".into()]).request(&question).unwrap();
        assert_eq!(
            (path.as_str(), body),
            ("/question/que_1/reply", json!({"answers": [["b"]]}))
        );
        assert!(Reply::Answers(vec!["c".into()]).request(&question).is_err());
        assert!(Reply::Answers(vec![]).request(&question).is_err());
        let several = json!({"id": "que_2", "_kind": "question", "questions": [{"question": "P", "multiple": true}]});
        assert!(Reply::Answers(vec!["a".into()]).request(&several).is_err());
        assert!(not_found(br#"{"_tag":"PermissionNotFoundError"}"#));
        assert!(!not_found(br#"{"message":"route not found"}"#));
    }
}
