//! Answering OpenCode's permission and question requests from masil
//! (docs/agent-answers.md). Only an OpenCode started with `agent start
//! --answers` has a channel: masil starts it with a local port and a fresh
//! server password, finds that port from the pane process's own listening
//! sockets, and checks that the server refuses requests without the
//! password before using it.
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

/// The flag `agent start --answers` passes to `exec-managed`.
pub(super) const EXEC_FLAG: &str = "--opencode-answers";
/// Recorded in run evidence when a start asked for the channel.
pub(crate) const CHANNEL: &str = "opencode_port";
const PROVIDER: &str = "opencode";
/// The release range checked live; a new minor is checked before widening.
const MINIMUM: (u64, u64, u64) = (1, 18, 34);
const BELOW: (u64, u64) = (1, 19);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const LIST_LIMIT: usize = 4 << 20;
const FIELD_LIMIT: usize = 2048;
/// Routes each API group guards; all must refuse a request without the
/// password.
const GUARDED: &[&str] = &["/permission", "/question", "/session"];
/// Options of OpenCode's TUI that take a value.
const VALUED: &[&str] = &[
    "-m",
    "--model",
    "-s",
    "--session",
    "--prompt",
    "--agent",
    "--log-level",
];
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
pub(super) fn check_start_args(args: &[String]) -> Result<(), String> {
    let refuse = |why: &str| {
        Err(format!(
            "invalid_argument: --answers needs the OpenCode TUI without {why}"
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
        // OpenCode does not take an option as another option's value, so a
        // word starting with `-` is checked as an option of its own.
        let valued = VALUED.contains(&name)
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

fn supported(version: (u64, u64, u64)) -> bool {
    version >= MINIMUM && (version.0, version.1) < BELOW
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
    if !supported(version) {
        return Vec::new();
    }
    // Never empty: an empty password turns OpenCode's authentication off.
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
            store.put_secret(&run, PROVIDER, &value, now_ms())
            // The connection closes here, before the provider starts.
        })
        .await;
    }
    argv.splice(
        1..1,
        ["--port", "0", "--hostname", "127.0.0.1"].map(str::to_owned),
    );
    let original = std::env::var("OPENCODE_SERVER_PASSWORD").unwrap_or_default();
    vec![
        ("OPENCODE_SERVER_PASSWORD", secret),
        ("OPENCODE_SERVER_USERNAME", "opencode".into()),
        // For the exported plugin's shell.env hook: it gives the agent's
        // shell the password the user had, not this one.
        ("MASIL_OPENCODE_ANSWERS", "1".into()),
        ("MASIL_OPENCODE_ORIGINAL_PASSWORD", original),
    ]
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

/// `lsof -Fn` lines name sockets as `n127.0.0.1:PORT`.
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

/// The processes whose listening sockets may be the pane's OpenCode server:
/// the pane process, and when it is a node or bun launcher, its direct
/// children identified as OpenCode in its foreground group.
fn candidates(pid: i32) -> Vec<i32> {
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
                    .is_some_and(|provider| provider.id == PROVIDER)
        }));
    }
    candidates
}

pub(super) struct Endpoint {
    client: Client,
    port: u16,
    secret: String,
}

impl Endpoint {
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
            .basic_auth("opencode", Some(&self.secret))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|error| format!("answer_unavailable: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "answer_unavailable: OpenCode answered {} for /event",
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
            .basic_auth("opencode", Some(&self.secret))
            .send()
            .await
            .map_err(|error| format!("answer_unavailable: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "answer_unavailable: OpenCode answered {} for {path}",
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
        let value = serde_json::from_slice(&body)
            .map_err(|_| format!("answer_unavailable: OpenCode sent no JSON for {path}"))?;
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
            authenticated.then_some(&self.secret),
        )
        .await
    }

    pub(super) async fn get_json(&self, path: &str) -> Result<Value, String> {
        let (status, body) = self.send(Method::GET, path, None, true).await?;
        if !status.is_success() {
            return Err(format!(
                "answer_unavailable: OpenCode answered {status} for {path}"
            ));
        }
        serde_json::from_slice(&body)
            .map_err(|_| format!("answer_unavailable: OpenCode sent no JSON for {path}"))
    }
}

async fn call(
    client: &Client,
    port: u16,
    method: Method,
    path: &str,
    body: Option<Value>,
    secret: Option<&String>,
) -> Result<(StatusCode, Vec<u8>), String> {
    let mut request = client.request(method, format!("http://127.0.0.1:{port}{path}"));
    if let Some(secret) = secret {
        request = request.basic_auth("opencode", Some(secret));
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
            return Err("answer_unavailable: OpenCode's answer is too large".into());
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

/// The OpenCode server of the process group `pid`: exactly one local
/// listener of its own processes that refuses every guarded route without
/// the password and is healthy with it. The password goes only to a port
/// that asked for it.
pub(super) async fn discover(secret: String, pid: i32) -> Result<Endpoint, String> {
    if pid <= 1 {
        return Err("answer_unavailable: the pane's OpenCode is not running".into());
    }
    let ports = tokio::task::spawn_blocking(move || {
        candidates(pid)
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
            Some(&secret),
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
            client,
            port: *port,
            secret,
        }),
        [] if unsafe_port => Err(
            "answer_unsafe: the pane's OpenCode server answered without its password; masil will not use it"
                .into(),
        ),
        [] => Err("answer_unavailable: the pane's OpenCode server is not listening yet".into()),
        _ => Err("answer_unavailable: more than one OpenCode server belongs to this pane".into()),
    }
}

/// Whether the agent asked for a channel at start.
pub(super) fn requested(agent: &Agent) -> bool {
    agent.provider == PROVIDER
        && agent
            .run_evidence
            .as_ref()
            .and_then(|evidence| evidence.answer_channel.as_deref())
            == Some(CHANNEL)
}

impl Manager {
    /// The run's server password; secrets of runs that ended are dropped
    /// on the way.
    async fn answer_secret(&self, run: &str) -> Result<Option<String>, String> {
        let (live, _) = self.live_runs().await?;
        let mut store = self.operation_store().await?;
        let run = run.to_owned();
        tokio::task::spawn_blocking(move || {
            store.prune_secrets(&live, now_ms())?;
            let secret = store.secret(&run, PROVIDER)?;
            Ok::<_, String>(secret)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    /// The pane's OpenCode server: exactly one local listener of its own
    /// processes that answers with the password and refuses without it.
    async fn answer_endpoint(&self, agent: &Agent) -> Result<Endpoint, String> {
        let secret = self.answer_secret(&agent.run).await?.ok_or(
            "answer_unavailable: no server password was recorded for this run; start it again with --answers",
        )?;
        // A dead pane's recorded group may belong to another process now.
        if agent.process != "running" || agent.foreground_group <= 1 {
            return Err("answer_unavailable: the pane's OpenCode is not running".into());
        }
        discover(secret, agent.foreground_group).await
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
    pub(super) async fn run_secret(&self, run: &str) -> Result<Option<String>, String> {
        // The coordinator's own connection opens only while the inbox is on.
        if self.is_resident() {
            let run = run.to_owned();
            if let Some(secret) = self
                .with_resident_store(move |store| store.secret(&run, PROVIDER))
                .await?
            {
                return Ok(secret);
            }
        }
        let run = run.to_owned();
        let store = self.operation_store().await?;
        tokio::task::spawn_blocking(move || store.secret(&run, PROVIDER))
            .await
            .map_err(|error| error.to_string())?
    }

    /// Checks a fresh `--answers` start: the server must refuse requests
    /// without its password. One that does not is stopped.
    pub(super) async fn verify_answers(&self, agent: &Agent) -> Result<&'static str, String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            // The pane is read again each time: right after the start it can
            // still run exec-managed, which then becomes OpenCode.
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
                    return Err(format!("{error}; the OpenCode process was stopped"));
                }
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(_) => return Ok("unavailable"),
            }
        }
    }

    /// `agent requests TARGET`: the pending requests of the pane's OpenCode.
    pub(crate) async fn requests(&self, agent: &Agent) -> Result<Value, String> {
        if !requested(agent) {
            return Ok(no_channel(agent));
        }
        let endpoint = self.answer_endpoint(agent).await?;
        let pending = pending(&endpoint).await?;
        let root = agent.session_id.as_deref();
        let requests = pending
            .iter()
            .map(|request| public_request(request, root))
            .collect::<Vec<_>>();
        Ok(json!({"channel": CHANNEL, "requests": requests}))
    }

    /// `agent answer TARGET REQUEST ...`, once per request key.
    pub(crate) async fn answer(
        &self,
        agent: &Agent,
        request: &str,
        reply: Reply,
    ) -> Result<Value, String> {
        refuse_agent_caller()?;
        if !requested(agent) {
            return Err(format!(
                "answer_channel_none: {}",
                no_channel(agent)["reason"]
                    .as_str()
                    .unwrap_or("no answer channel")
            ));
        }
        let endpoint = self.answer_endpoint(agent).await?;
        let (path, body) = reply.post(request);
        let body_text = body.to_string();
        let digest = super::prompt::sha256(&format!("{path}\n{body_text}"));
        let namespace = format!("run:{}", agent.run);
        let mut store = self.operation_store().await?;
        let earlier = store.get(&super::operations::key(&namespace, "answer", request))?;
        let before = pending(&endpoint).await?;
        let Some(found) = before.iter().find(|item| item["id"] == request) else {
            // OpenCode drops a request once answered, so a repeat is told
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
        // once OpenCode has had as long as a request may take to apply it.
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
                        "rejected_before_effect: OpenCode refused the answer ({status})"
                    )),
                ),
                Ok((status, _)) => (
                    super::operations::UNKNOWN,
                    Err(format!(
                        "outcome_unknown: OpenCode answered {status}; check the pane"
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
                "opencode",
                Some(&json!({"path": path})),
                now_ms(),
            )?;
            result?;
            // OpenCode rejects the session's other permissions with a
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
                source: format!("hook:{PROVIDER}"),
                source_ref: format!("request:{request}"),
                resolution: "answered",
            }];
            effects.extend(
                also.iter()
                    .flatten()
                    .map(|id| super::inbox::Effect::Resolve {
                        source: format!("hook:{PROVIDER}"),
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
    /// Allow this and, while the OpenCode instance lives, what its
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
        PROVIDER => "this OpenCode was not started with --answers; answer in the pane",
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
                    return Err(
                        "answer_refused: this command runs inside an agent; answer from a terminal or the management window"
                            .into(),
                    );
                }
                info.parent
            }
            // Another user's process, such as login(1) between a terminal
            // and its shell: not an agent of this user, but its parent is.
            None => crate::process::other_users_parent(pid).ok_or(
                "answer_refused: this command's ancestry could not be read; answer from a terminal or the management window",
            )?,
        };
        if parent <= 1 {
            return Ok(());
        }
        pid = parent;
    }
    Err("answer_refused: this command's ancestry is too deep to check".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn answers_take_the_tui_with_its_own_options_only() {
        assert!(check_start_args(&args(&[])).is_ok());
        assert!(
            check_start_args(&args(&[
                "--model",
                "anthropic/x",
                "-c",
                "--log-level=DEBUG"
            ]))
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
            assert!(check_start_args(&args(refused)).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn only_the_checked_release_line_qualifies() {
        assert_eq!(parse_version("1.18.34\n"), Some((1, 18, 34)));
        assert_eq!(parse_version("opencode 1.18.40"), Some((1, 18, 40)));
        assert!(supported((1, 18, 34)));
        assert!(supported((1, 18, 99)));
        assert!(!supported((1, 18, 33)));
        assert!(!supported((1, 19, 0)));
        assert!(!supported((2, 0, 0)));
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
