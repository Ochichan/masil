//! On-demand diagnostics for the local agent, masil server, and saved configuration.

use crate::{detection::Engine, managed::endpoints, native_ui, providers};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::ffi::{CStr, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::timeout;

const VERSION: u32 = 1;
const MANAGED_PANE_LIMIT: u64 = 64;
const ENDPOINT_LIMIT: u64 = 8;
const NATIVE_RESPONSE_LIMIT: u64 = 64 * 1024;
const COMMAND_DEADLINE_SECONDS: u64 = 3;
const VERSION_DEADLINE: Duration = Duration::from_secs(2);
const PROCESS_OUTPUT_LIMIT: usize = 64 * 1024;
const SETTINGS_LIMIT: u64 = 64 * 1024;
const MAX_OVERRIDE_FILES: usize = 128;
const MAX_SNAPSHOTS: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Serialize)]
struct Check {
    id: &'static str,
    status: Status,
    summary: String,
    detail: Value,
}

#[derive(Debug, Default, Eq, PartialEq, Serialize)]
struct Summary {
    ok: usize,
    warn: usize,
    fail: usize,
    skip: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    stage: &'static str,
    version: u32,
    generated_at_ms: u64,
    summary: Summary,
    checks: Vec<Check>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bundle: Option<String>,
}

#[derive(Debug)]
struct Options {
    socket: Option<PathBuf>,
    versions: bool,
    bundle: Option<PathBuf>,
}

#[derive(Debug)]
struct ServerInfo {
    boot_id: String,
    generation: String,
    core_version: String,
    pid: u32,
    sessions: usize,
    windows: usize,
    panes: usize,
    clients: usize,
}

#[derive(Debug, PartialEq)]
struct ServerProcess {
    rss_kib: u64,
    cpu_percent: f64,
    elapsed: String,
}

pub(crate) fn run(args: &[String]) -> Result<i32, String> {
    if args == ["--help"] || args == ["help"] {
        println!("usage: masil-agent doctor [--socket MASIL_SOCKET] [--versions] [--bundle DIR]");
        return Ok(0);
    }
    let options = parse_options(args)?;
    let bundle = options.bundle.as_deref().map(prepare_bundle).transpose()?;
    let homes = home_prefixes();
    let secrets = secret_values();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let checks = runtime.block_on(diagnose(&options));
    let failed = checks.iter().any(|check| check.status == Status::Fail);
    let mut report = Report {
        stage: "diagnosed",
        version: VERSION,
        generated_at_ms: now_ms(),
        summary: count_summary(&checks),
        checks,
        bundle: bundle.as_deref().map(|path| redact_home_path(path, &homes)),
    };
    sanitize_report(&mut report, &homes, &secrets);
    let mut output = serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?;
    output.push('\n');
    if let Some(directory) = bundle {
        write_bundle(&directory, output.as_bytes())?;
    }
    print!("{output}");
    Ok(i32::from(failed))
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut socket = None;
    let mut versions = false;
    let mut bundle = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--socket" if socket.is_none() => {
                let value = args
                    .get(index + 1)
                    .ok_or("doctor --socket requires a path")?;
                if value.is_empty() || value.len() > 4096 {
                    return Err("invalid doctor socket path".into());
                }
                socket = Some(PathBuf::from(value));
                index += 2;
            }
            "--versions" if !versions => {
                versions = true;
                index += 1;
            }
            "--bundle" if bundle.is_none() => {
                let value = args
                    .get(index + 1)
                    .ok_or("doctor --bundle requires a directory")?;
                if value.is_empty() || value.len() > 4096 {
                    return Err("invalid doctor bundle directory".into());
                }
                bundle = Some(PathBuf::from(value));
                index += 2;
            }
            option => return Err(format!("unknown or repeated doctor option: {option}")),
        }
    }
    if socket.is_none() {
        socket = std::env::var("TMUX")
            .ok()
            .and_then(|value| value.rsplitn(3, ',').last().map(str::to_owned))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("MASIL_AGENT_SOCKET")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            });
    }
    Ok(Options {
        socket,
        versions,
        bundle,
    })
}

async fn diagnose(options: &Options) -> Vec<Check> {
    let mut checks = Vec::with_capacity(11);
    checks.push(binary_check());
    checks.push(platform_check());

    let (reachable, server) = server_check(options.socket.as_deref()).await;
    checks.push(reachable);
    checks.push(identity_check(options.socket.as_deref(), server.as_ref()));

    let (resources, observer) = match (options.socket.as_deref(), server.as_ref()) {
        (Some(socket), Some(server)) => {
            let context = native_ui::Context {
                socket: socket.to_owned(),
                client: None,
            };
            let server_pid = server.pid.to_string();
            let observer = observer_query(context);
            let server_ps = native_ui::run_process(
                ps_program(),
                [
                    OsString::from("-o"),
                    OsString::from("rss=,pcpu=,etime="),
                    OsString::from("-p"),
                    OsString::from(server_pid),
                ],
                None,
            );
            let user_ps = agent_processes();
            let (observer, server_ps, user_ps) = tokio::join!(observer, server_ps, user_ps);
            (
                resources_check(socket, server.pid, server_ps, user_ps),
                observer_check(observer),
            )
        }
        _ => (
            server_skip("server resources require a socket"),
            server_skip("observer status requires a socket"),
        ),
    };
    checks.push(resources);
    checks.push(limits_check(server.as_ref()));
    checks.push(providers_check(options.versions).await);
    checks.push(detection_check());
    checks.push(endpoints_check());
    checks.push(settings_check());
    checks.push(observer);
    checks.push(sessions_check());
    let recorded = match (options.socket.as_deref(), server.as_ref()) {
        (Some(socket), Some(_)) => native_ui::Context {
            socket: socket.to_owned(),
            client: None,
        }
        .tmux(
            [
                OsString::from("show-options"),
                OsString::from("-gqv"),
                OsString::from("@masil-operation-store"),
            ],
            None,
        )
        .await
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned()),
        _ => None,
    };
    checks.push(operations_check(
        options.socket.as_deref().filter(|_| server.is_some()),
        recorded,
    ));
    checks.push(coordinator_check(
        options.socket.as_deref().filter(|_| server.is_some()),
    ));
    checks.push(callbacks_check());
    checks
}

/// Integration callbacks refused in the last day because they did not come
/// from their pane's own provider process; a provider rarely shows a hook's
/// error output.
fn callbacks_check() -> Check {
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    let id = "integration.callbacks";
    let path = crate::managed::state_base()
        .ok()
        .map(|base| base.join("masil/integration-refusals.log"));
    let text = path
        .as_ref()
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_default();
    let now = now_ms();
    let recent: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["at_ms"].as_u64().is_some_and(|at| at + DAY_MS >= now))
        .collect();
    if recent.is_empty() {
        return Check {
            id,
            status: Status::Ok,
            summary: "no integration callback was refused in the last day".into(),
            detail: json!({"refused": 0}),
        };
    }
    let mut counts = serde_json::Map::new();
    for entry in &recent {
        let key = format!(
            "{}: {}",
            entry["provider"].as_str().unwrap_or("?"),
            crate::managed::failure::human(entry["reason"].as_str().unwrap_or("?"))
        );
        let count = counts.get(&key).and_then(Value::as_u64).unwrap_or(0);
        counts.insert(key, json!(count + 1));
    }
    Check {
        id,
        status: Status::Warn,
        summary: format!(
            "{} integration callback(s) were refused in the last day because they did not come from their pane's provider process; for Codex, start it with masil or with --no-daemon",
            recent.len()
        ),
        detail: json!({"refused": recent.len(), "by_reason": counts}),
    }
}

/// The coordinator for the server, read without starting, stopping or
/// replacing anything.
fn coordinator_check(socket: Option<&Path>) -> Check {
    let id = "coordinator";
    let Some(socket) = socket else {
        return Check {
            id,
            status: Status::Skip,
            summary: "the coordinator is checked for a reachable server".into(),
            detail: json!({}),
        };
    };
    let report = match crate::coordinator::status(socket) {
        Ok(report) => report,
        Err(error) => {
            return Check {
                id,
                status: Status::Warn,
                summary: format!("the coordinator could not be checked: {error}"),
                detail: json!({}),
            };
        }
    };
    let state = report["state"].as_str().unwrap_or_default().to_owned();
    let (status, summary) = match state.as_str() {
        "running" => (Status::Ok, "the coordinator is running".to_owned()),
        "not_running" => (
            Status::Ok,
            "no coordinator runs and no feature needs one".to_owned(),
        ),
        "enabled_but_not_running" => (
            Status::Warn,
            "a feature needs the coordinator but none runs; run `masil-agent agent coordinator start`. A server started with -f has no UI layer and does not restart it after a server restart".to_owned(),
        ),
        "stale" => (
            Status::Warn,
            "a coordinator of an earlier server boot or an older masil-agent answers; the next mutating command replaces it".to_owned(),
        ),
        "unresponsive" => (
            Status::Warn,
            "a coordinator holds its lock but does not answer; the next mutating command restarts it".to_owned(),
        ),
        "socket_path_too_long" => (
            Status::Warn,
            "the coordinator socket path beside the server socket is too long for a Unix socket; use a shorter socket path".to_owned(),
        ),
        other => (Status::Warn, format!("unexpected coordinator state {other}")),
    };
    Check {
        id,
        status,
        summary,
        detail: report,
    }
}

fn binary_check() -> Check {
    let result = (|| -> Result<Value, String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("locating executable: {error}"))?
            .canonicalize()
            .map_err(|error| format!("resolving executable: {error}"))?;
        let mut file =
            File::open(&executable).map_err(|error| format!("opening executable: {error}"))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|error| format!("reading executable: {error}"))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        let digest = hasher.finalize();
        let sha256 = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "executable": executable.to_string_lossy(),
            "sha256": sha256,
        }))
    })();
    match result {
        Ok(detail) => Check {
            id: "agent.binary",
            status: Status::Ok,
            summary: "masil-agent binary identified".into(),
            detail,
        },
        Err(error) => Check {
            id: "agent.binary",
            status: Status::Fail,
            summary: "could not inspect the masil-agent binary".into(),
            detail: json!({"error": error}),
        },
    }
}

fn platform_check() -> Check {
    let release = kernel_release();
    let status = if release.is_ok() {
        Status::Ok
    } else {
        Status::Warn
    };
    Check {
        id: "platform",
        status,
        summary: if status == Status::Ok {
            "platform identified".into()
        } else {
            "kernel release is unavailable".into()
        },
        detail: json!({
            "os": std::env::consts::OS,
            "architecture": std::env::consts::ARCH,
            "kernel_release": release.as_deref().ok(),
            "error": release.err(),
        }),
    }
}

fn kernel_release() -> Result<String, String> {
    let mut name = std::mem::MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: uname initializes the supplied utsname on success.
    if unsafe { libc::uname(name.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: the successful uname call initialized a NUL-terminated release field.
    let name = unsafe { name.assume_init() };
    // SAFETY: uname fields are fixed-size NUL-terminated C strings.
    let release = unsafe { CStr::from_ptr(name.release.as_ptr()) };
    release
        .to_str()
        .map(str::to_owned)
        .map_err(|_| "kernel release is not UTF-8".into())
}

async fn server_check(socket: Option<&Path>) -> (Check, Option<ServerInfo>) {
    let Some(socket) = socket else {
        return (
            Check {
                id: "server.reachable",
                status: Status::Skip,
                summary: "no masil socket was resolved".into(),
                detail: json!({"socket": Value::Null}),
            },
            None,
        );
    };
    let context = native_ui::Context {
        socket: socket.to_owned(),
        client: None,
    };
    let script = b"display-message -p 'META\t#{masil_core_boot_id}\t#{masil_pty_generation}\t#{version}\t#{pid}'\nlist-sessions -F SESSION\nlist-windows -a -F WINDOW\nlist-panes -a -F PANE\nlist-clients -F CLIENT\n".to_vec();
    let output = context
        .tmux(
            [OsString::from("source-file"), OsString::from("-")],
            Some(script),
        )
        .await;
    let info = output.and_then(|output| parse_server_output(&output.stdout));
    match info {
        Ok(info) => {
            let detail = json!({
                "core_boot_id": info.boot_id,
                "core_version": info.core_version,
                "server_pid": info.pid,
                "socket": socket.to_string_lossy(),
                "sessions": info.sessions,
                "windows": info.windows,
                "panes": info.panes,
                "clients": info.clients,
            });
            (
                Check {
                    id: "server.reachable",
                    status: Status::Ok,
                    summary: "masil server answered display-message".into(),
                    detail,
                },
                Some(info),
            )
        }
        Err(error) => (
            Check {
                id: "server.reachable",
                status: Status::Fail,
                summary: "masil server did not answer".into(),
                detail: json!({"socket": socket.to_string_lossy(), "error": error}),
            },
            None,
        ),
    }
}

fn parse_server_output(bytes: &[u8]) -> Result<ServerInfo, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "server response is not UTF-8")?;
    let mut boot_id = None;
    let mut generation = None;
    let mut core_version = None;
    let mut pid = None;
    let mut sessions = 0usize;
    let mut windows = 0usize;
    let mut panes = 0usize;
    let mut clients = 0usize;
    for line in text.lines().take(8192) {
        if let Some(value) = line.strip_prefix("META\t") {
            let fields = value.split('\t').collect::<Vec<_>>();
            if fields.len() != 4 {
                return Err("server metadata has an invalid shape".into());
            }
            boot_id = Some(fields[0].to_owned());
            generation = Some(fields[1].to_owned());
            core_version = Some(fields[2].to_owned());
            pid = Some(
                fields[3]
                    .parse::<u32>()
                    .map_err(|_| "server PID is invalid")?,
            );
        } else {
            match line {
                "SESSION" => sessions += 1,
                "WINDOW" => windows += 1,
                "PANE" => panes += 1,
                "CLIENT" => clients += 1,
                _ => {}
            }
        }
    }
    Ok(ServerInfo {
        boot_id: boot_id.ok_or("server response has no metadata")?,
        generation: generation.ok_or("server response has no PTY generation")?,
        core_version: core_version.ok_or("server response has no version")?,
        pid: pid.ok_or("server response has no PID")?,
        sessions,
        windows,
        panes,
        clients,
    })
}

fn identity_check(socket: Option<&Path>, server: Option<&ServerInfo>) -> Check {
    let Some(server) = server else {
        return Check {
            id: "server.identity",
            status: Status::Skip,
            summary: if socket.is_some() {
                "server identity requires a reachable server"
            } else {
                "server identity requires a socket"
            }
            .into(),
            detail: json!({}),
        };
    };
    let boot_valid = valid_uuid(&server.boot_id);
    let generation_valid = !server.generation.is_empty()
        && server.generation.bytes().all(|byte| byte.is_ascii_digit());
    let valid = boot_valid && generation_valid;
    Check {
        id: "server.identity",
        status: if valid { Status::Ok } else { Status::Warn },
        summary: if valid {
            "masil server identity formats are valid".into()
        } else {
            "restart the server with the current build".into()
        },
        detail: json!({
            "core_boot_id_present": !server.boot_id.is_empty(),
            "core_boot_id_valid": boot_valid,
            "pty_generation_present": !server.generation.is_empty(),
            "pty_generation_valid": generation_valid,
        }),
    }
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn resources_check(
    socket: &Path,
    pid: u32,
    server_ps: Result<native_ui::ProcessOutput, String>,
    user_ps: Result<String, String>,
) -> Check {
    let server_process = server_ps.and_then(|output| {
        let text = std::str::from_utf8(&output.stdout).map_err(|_| "ps output is not UTF-8")?;
        parse_server_ps(text)
    });
    let agents = user_ps.map(|text| parse_agent_ps(&text, &socket.to_string_lossy()));
    let status = if server_process.is_ok() && agents.is_ok() {
        Status::Ok
    } else {
        Status::Warn
    };
    let detail = json!({
        "server_pid": pid,
        "server_rss_mib": server_process.as_ref().ok().map(|value| mib(value.rss_kib)),
        "server_cpu_percent": server_process.as_ref().ok().map(|value| value.cpu_percent),
        "server_elapsed": server_process.as_ref().ok().map(|value| value.elapsed.as_str()),
        "agent_process_count": agents.as_ref().ok().map(|value| value.0),
        "agent_total_rss_mib": agents.as_ref().ok().map(|value| mib(value.1)),
        "server_ps_error": server_process.err(),
        "agent_ps_error": agents.err(),
    });
    Check {
        id: "server.resources",
        status,
        summary: if status == Status::Ok {
            "server and related agent process resources sampled".into()
        } else {
            "one or more ps samples were unavailable".into()
        },
        detail,
    }
}

fn parse_server_ps(text: &str) -> Result<ServerProcess, String> {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or("ps returned no server sample")?;
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 3 {
        return Err("ps returned an invalid server sample".into());
    }
    let rss_kib = fields[0]
        .parse::<u64>()
        .map_err(|_| "ps returned invalid server RSS")?;
    let cpu_percent = fields[1]
        .parse::<f64>()
        .map_err(|_| "ps returned invalid server CPU")?;
    if !cpu_percent.is_finite() || cpu_percent < 0.0 || !valid_elapsed(fields[2]) {
        return Err("ps returned invalid server resource values".into());
    }
    Ok(ServerProcess {
        rss_kib,
        cpu_percent,
        elapsed: fields[2].to_owned(),
    })
}

fn valid_elapsed(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b':' | b'-'))
        && value.bytes().any(|byte| byte == b':')
}

/// `rss command` lines for this user's masil-agent processes. The full
/// argument list is fetched only for those PIDs: listing every process's
/// command line can exceed the 64 KiB native output limit.
async fn agent_processes() -> Result<String, String> {
    let uid = unsafe { libc::geteuid() }.to_string();
    let names = native_ui::run_process(
        ps_program(),
        [
            OsString::from("-U"),
            OsString::from(uid),
            OsString::from("-o"),
            OsString::from("pid=,ucomm="),
        ],
        None,
    )
    .await?;
    let names = String::from_utf8(names.stdout).map_err(|_| "ps output is not UTF-8")?;
    let pids = agent_pids(&names);
    if pids.is_empty() {
        return Ok(String::new());
    }
    let output = native_ui::run_process(
        ps_program(),
        [
            OsString::from("-o"),
            OsString::from("rss=,command="),
            OsString::from("-p"),
            OsString::from(pids.join(",")),
        ],
        None,
    )
    .await?;
    String::from_utf8(output.stdout).map_err(|_| "ps output is not UTF-8".into())
}

fn agent_pids(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?;
            (fields.next()? == "masil-agent" && pid.bytes().all(|b| b.is_ascii_digit()))
                .then(|| pid.to_owned())
        })
        .take(256)
        .collect()
}

fn parse_agent_ps(text: &str, socket: &str) -> (usize, u64) {
    let mut count = 0usize;
    let mut total_rss_kib = 0u64;
    for line in text.lines().take(4096) {
        let line = line.trim_start();
        let split = line.find(char::is_whitespace).unwrap_or(line.len());
        let (rss, command) = line.split_at(split);
        let command = command.trim_start();
        let role = command
            .split_whitespace()
            .any(|word| matches!(word, "ui" | "sidebar" | "serve"));
        if !command.contains("masil-agent") || !command.contains(socket) || !role {
            continue;
        }
        let Ok(rss) = rss.parse::<u64>() else {
            continue;
        };
        count += 1;
        total_rss_kib = total_rss_kib.saturating_add(rss);
    }
    (count, total_rss_kib)
}

fn mib(kib: u64) -> f64 {
    (kib as f64 / 1024.0 * 1000.0).round() / 1000.0
}

fn limits_check(server: Option<&ServerInfo>) -> Check {
    let live_panes = server.map(|value| value.panes as u64);
    let over_limit = live_panes.is_some_and(|value| value > MANAGED_PANE_LIMIT);
    Check {
        id: "limits",
        status: if over_limit { Status::Warn } else { Status::Ok },
        summary: if over_limit {
            "live pane count exceeds the managed pane limit".into()
        } else {
            "diagnostic limits are within their configured bounds".into()
        },
        detail: json!({
            "managed_panes_per_server": MANAGED_PANE_LIMIT,
            "configured_endpoints": ENDPOINT_LIMIT,
            "native_response_bytes": NATIVE_RESPONSE_LIMIT,
            "command_deadline_seconds": COMMAND_DEADLINE_SECONDS,
            "live_panes": live_panes,
        }),
    }
}

async fn providers_check(include_versions: bool) -> Check {
    let executables: Vec<_> = providers::all()
        .iter()
        .map(|provider| resolve_provider(provider.executables))
        .collect();
    let found_count = executables.iter().filter(|path| path.is_some()).count();
    // Concurrent, so --versions takes one 2 s deadline rather than one per provider.
    let versions = futures_util::future::join_all(executables.iter().map(|executable| async {
        match executable.as_deref() {
            Some(path) if include_versions => match provider_version(path).await {
                Ok(version) => (Some(version), None),
                Err(error) => (None, Some(error)),
            },
            _ => (None, None),
        }
    }))
    .await;
    let mut entries = Vec::with_capacity(providers::all().len());
    for ((provider, executable), (version, version_error)) in
        providers::all().iter().zip(&executables).zip(versions)
    {
        entries.push(json!({
            "id": provider.id,
            "found": executable.is_some(),
            "executable": executable.as_ref().map(|path| path.to_string_lossy()),
            "version": version,
            "version_error": version_error,
        }));
    }
    Check {
        id: "providers",
        status: if found_count == 0 {
            Status::Warn
        } else {
            Status::Ok
        },
        summary: if found_count == 0 {
            "no supported provider executable was found".into()
        } else {
            format!("found {found_count} supported provider executable(s)")
        },
        detail: json!({"versions_requested": include_versions, "providers": entries}),
    }
}

fn resolve_provider(executables: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for executable in executables {
        if executable.contains('/') {
            continue;
        }
        for directory in std::env::split_paths(&path).take(256) {
            let candidate = directory.join(executable);
            let Ok(metadata) = fs::metadata(&candidate) else {
                continue;
            };
            if metadata.is_file() && metadata.mode() & 0o111 != 0 {
                return candidate.canonicalize().ok().or(Some(candidate));
            }
        }
    }
    None
}

async fn provider_version(executable: &Path) -> Result<String, String> {
    let mut child = Command::new(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("version command could not start: {error}"))?;
    let stdout = child.stdout.take().ok_or("version command has no stdout")?;
    let stderr = child.stderr.take().ok_or("version command has no stderr")?;
    let operation = async {
        tokio::join!(
            read_process_output(stdout),
            read_process_output(stderr),
            child.wait()
        )
    };
    let (stdout, stderr, status) = match timeout(VERSION_DEADLINE, operation).await {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err("version command timed out after 2 seconds".into());
        }
    };
    let stdout = stdout?;
    let stderr = stderr?;
    let status = status.map_err(|error| format!("version command wait failed: {error}"))?;
    if !status.success() {
        return Err(format!("version command exited with {status}"));
    }
    let bytes = if stdout.iter().any(|byte| !byte.is_ascii_whitespace()) {
        &stdout
    } else {
        &stderr
    };
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().next().unwrap_or_default();
    Ok(line.chars().take(200).collect())
}

async fn read_process_output<R>(reader: R) -> Result<Vec<u8>, String>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take((PROCESS_OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("reading version output: {error}"))?;
    if bytes.len() > PROCESS_OUTPUT_LIMIT {
        return Err("version output exceeds 65536 bytes".into());
    }
    Ok(bytes)
}

fn detection_check() -> Check {
    let override_files = detection_override_files();
    match Engine::load() {
        Ok(_) => Check {
            id: "detection",
            status: Status::Ok,
            summary: "detection manifests loaded".into(),
            detail: json!({"override_files": override_files}),
        },
        Err(error) => Check {
            id: "detection",
            status: Status::Fail,
            summary: "detection manifests did not load".into(),
            detail: json!({"override_files": override_files, "error": error}),
        },
    }
}

fn detection_override_files() -> Vec<String> {
    let Some(directory) = config_base().map(|base| base.join("masil/agent-detection")) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files = entries
        .take(MAX_OVERRIDE_FILES)
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.len() <= 255 && name.ends_with(".toml"))
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn endpoints_check() -> Check {
    match endpoints::load() {
        Ok(endpoints) => Check {
            id: "endpoints",
            status: Status::Ok,
            summary: "endpoint configuration loaded".into(),
            detail: json!({
                "configured": endpoints.len(),
                "enabled": endpoints.iter().filter(|endpoint| endpoint.enabled).count(),
            }),
        },
        Err(error) => Check {
            id: "endpoints",
            status: Status::Warn,
            summary: "endpoint configuration could not be read".into(),
            detail: json!({"configured": Value::Null, "enabled": Value::Null, "error": error}),
        },
    }
}

fn settings_check() -> Check {
    let Some(path) = config_base().map(|base| base.join("masil/settings.conf")) else {
        return Check {
            id: "settings",
            status: Status::Warn,
            summary: "settings path is unavailable".into(),
            detail: json!({"path": Value::Null, "exists": false, "size": Value::Null}),
        };
    };
    match inspect_settings(&path) {
        Ok((exists, size)) => Check {
            id: "settings",
            status: Status::Ok,
            summary: if exists {
                "settings file is readable".into()
            } else {
                "settings file is not present".into()
            },
            detail: json!({"path": path.to_string_lossy(), "exists": exists, "size": size}),
        },
        Err(error) => Check {
            id: "settings",
            status: Status::Warn,
            summary: "settings file could not be parsed".into(),
            detail: json!({
                "path": path.to_string_lossy(),
                "exists": fs::symlink_metadata(&path).is_ok(),
                "size": fs::symlink_metadata(&path).ok().map(|metadata| metadata.len()),
                "error": error,
            }),
        },
    }
}

fn inspect_settings(path: &Path) -> Result<(bool, Option<u64>), String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok((false, None)),
        Err(error) => return Err(format!("settings file: {error}")),
    };
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > SETTINGS_LIMIT {
        return Err("settings file is not a small regular file".into());
    }
    let mut text = String::new();
    file.take(SETTINGS_LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|_| "settings file is not UTF-8 text".to_string())?;
    Ok((true, Some(metadata.len())))
}

async fn observer_query(context: native_ui::Context) -> Result<String, String> {
    let output = context
        .tmux(
            [
                OsString::from("show-environment"),
                OsString::from("-g"),
                OsString::from("MASIL_BRIDGE_SOCKET"),
            ],
            None,
        )
        .await?;
    String::from_utf8(output.stdout).map_err(|_| "observer environment is not UTF-8".into())
}

fn observer_check(result: Result<String, String>) -> Check {
    let value = result.ok().and_then(|text| {
        text.lines()
            .find_map(|line| line.strip_prefix("MASIL_BRIDGE_SOCKET="))
            .filter(|value| !value.is_empty() && value.len() <= 4096)
            .map(PathBuf::from)
    });
    let Some(path) = value else {
        return Check {
            id: "observer",
            status: Status::Ok,
            summary: "observer bridge is not configured".into(),
            detail: json!({"configured": false, "path": Value::Null, "exists": false, "is_socket": false}),
        };
    };
    let metadata = fs::metadata(&path).ok();
    let exists = metadata.is_some();
    let is_socket = metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_socket());
    Check {
        id: "observer",
        status: if is_socket { Status::Ok } else { Status::Warn },
        summary: if is_socket {
            "observer bridge socket exists".into()
        } else {
            "observer bridge path is not a socket".into()
        },
        detail: json!({
            "configured": true,
            "path": path.to_string_lossy(),
            "exists": exists,
            "is_socket": is_socket,
        }),
    }
}

/// The durable operation store for this server: present, readable, the one
/// the server recorded, and how many operations are unresolved.
fn operations_check(socket: Option<&Path>, recorded: Option<String>) -> Check {
    let Some(socket) = socket else {
        return Check {
            id: "operations.store",
            status: Status::Skip,
            summary: "the operation store is checked for a reachable server".into(),
            detail: json!({}),
        };
    };
    match crate::managed::operation_store_status(socket) {
        Ok(None) => Check {
            id: "operations.store",
            status: Status::Ok,
            summary: "no durable management operation has been recorded for this server".into(),
            detail: json!({"exists": false}),
        },
        Ok(Some(status))
            if status["supported"] == false
                && status["min_reader"].as_i64() > status["reader_generation"].as_i64() =>
        {
            Check {
                id: "operations.store",
                status: Status::Fail,
                summary: "a newer masil-agent upgraded the operation store; use that masil-agent and restart long-running masil-agent processes such as agent ui and session autosave".into(),
                detail: status,
            }
        }
        Ok(Some(status)) if status["initialized"] != true || status["supported"] == false => {
            Check {
                id: "operations.store",
                status: Status::Warn,
                summary: "the operation store file exists but is not a usable schema; the next management command creates or refuses it".into(),
                detail: status,
            }
        }
        Ok(Some(mut status)) => {
            let instance = status["instance"].to_string();
            let replaced = recorded
                .as_deref()
                .is_some_and(|recorded| !recorded.is_empty() && recorded != instance);
            let unresolved = status["unresolved"].as_u64().unwrap_or(0);
            status["exists"] = json!(true);
            status["server_instance_matches"] = json!(!replaced);
            Check {
                id: "operations.store",
                status: if replaced {
                    Status::Fail
                } else if unresolved > 0 {
                    Status::Warn
                } else {
                    Status::Ok
                },
                summary: if replaced {
                    "the operation store differs from the one this server recorded; run `masil-agent agent operations adopt` after inspecting agents".into()
                } else if unresolved > 0 {
                    format!(
                        "{unresolved} unresolved operation(s); see `masil-agent agent operations`"
                    )
                } else {
                    "operation store is readable with no unresolved operations".into()
                },
                detail: status,
            }
        }
        Err(error) => Check {
            id: "operations.store",
            status: Status::Fail,
            summary: "the operation store could not be read".into(),
            detail: json!({"error": error}),
        },
    }
}

fn sessions_check() -> Check {
    let Some(path) = data_base().map(|base| base.join("masil/sessions")) else {
        return Check {
            id: "sessions",
            status: Status::Warn,
            summary: "session snapshot directory is unavailable".into(),
            detail: json!({"path": Value::Null, "snapshot_count": 0, "newest_snapshot_age_ms": Value::Null}),
        };
    };
    match inspect_snapshots(&path) {
        Ok((count, age)) => Check {
            id: "sessions",
            status: Status::Ok,
            summary: format!("found {count} session snapshot(s)"),
            detail: json!({
                "path": path.to_string_lossy(),
                "snapshot_count": count,
                "newest_snapshot_age_ms": age,
            }),
        },
        Err(error) => Check {
            id: "sessions",
            status: Status::Warn,
            summary: "session snapshots could not be inspected".into(),
            detail: json!({
                "path": path.to_string_lossy(),
                "snapshot_count": Value::Null,
                "newest_snapshot_age_ms": Value::Null,
                "error": error,
            }),
        },
    }
}

fn inspect_snapshots(path: &Path) -> Result<(usize, Option<u64>), String> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok((0, None)),
        Err(error) => return Err(error.to_string()),
    };
    let now = SystemTime::now();
    let mut count = 0usize;
    let mut newest = None;
    for entry in entries.take(MAX_SNAPSHOTS) {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        if !name.to_string_lossy().ends_with(".json") {
            continue;
        }
        let metadata = entry.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() {
            continue;
        }
        count += 1;
        if let Ok(modified) = metadata.modified() {
            newest = Some(newest.map_or(modified, |value: SystemTime| value.max(modified)));
        }
    }
    let age = newest.map(|modified| {
        now.duration_since(modified)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64
    });
    Ok((count, age))
}

fn config_base() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })
}

fn data_base() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/share"))
        })
}

fn ps_program() -> &'static OsStr {
    if Path::new("/bin/ps").is_file() {
        OsStr::new("/bin/ps")
    } else {
        OsStr::new("ps")
    }
}

fn server_skip(summary: &str) -> Check {
    Check {
        id: if summary.starts_with("server resources") {
            "server.resources"
        } else {
            "observer"
        },
        status: Status::Skip,
        summary: summary.into(),
        detail: json!({}),
    }
}

fn count_summary(checks: &[Check]) -> Summary {
    let mut summary = Summary::default();
    for check in checks {
        match check.status {
            Status::Ok => summary.ok += 1,
            Status::Warn => summary.warn += 1,
            Status::Fail => summary.fail += 1,
            Status::Skip => summary.skip += 1,
        }
    }
    summary
}

fn prepare_bundle(path: &Path) -> Result<PathBuf, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err("doctor bundle directory must not be a symlink".into());
            }
            if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
                return Err("doctor bundle directory must be a directory you own".into());
            }
            if fs::read_dir(path)
                .map_err(|error| format!("doctor bundle directory: {error}"))?
                .next()
                .is_some()
            {
                return Err("doctor bundle directory must be empty".into());
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
            builder
                .create(path)
                .map_err(|error| format!("creating doctor bundle directory: {error}"))?;
        }
        Err(error) => return Err(format!("doctor bundle directory: {error}")),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("securing doctor bundle directory: {error}"))?;
    Ok(path.to_owned())
}

fn write_bundle(directory: &Path, bytes: &[u8]) -> Result<(), String> {
    let path = directory.join("doctor.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| format!("creating doctor bundle: {error}"))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("writing doctor bundle: {error}"))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("securing doctor bundle: {error}"))
}

fn home_prefixes() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let mut homes = vec![home.clone()];
    if let Ok(canonical) = home.canonicalize()
        && canonical != home
    {
        homes.push(canonical);
    }
    homes.sort_by_key(|path| std::cmp::Reverse(path.as_os_str().len()));
    homes
}

fn redact_home_path(path: &Path, homes: &[PathBuf]) -> String {
    for home in homes {
        if let Ok(suffix) = path.strip_prefix(home) {
            if suffix.as_os_str().is_empty() {
                return "~".into();
            }
            return format!("~/{}", suffix.to_string_lossy());
        }
    }
    path.to_string_lossy().into_owned()
}

fn secret_values() -> Vec<String> {
    std::env::vars()
        .filter(|(name, value)| {
            let name = name.to_ascii_uppercase();
            value.len() >= 8
                && ["TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "API_KEY"]
                    .iter()
                    .any(|word| name.contains(word))
        })
        .map(|(_, value)| value)
        .take(128)
        .collect()
}

fn sanitize_report(report: &mut Report, homes: &[PathBuf], secrets: &[String]) {
    for check in &mut report.checks {
        check.summary = sanitize_text(&check.summary, homes, secrets);
        sanitize_value(&mut check.detail, homes, secrets);
    }
    if let Some(bundle) = &mut report.bundle {
        *bundle = sanitize_text(bundle, homes, secrets);
    }
}

fn sanitize_value(value: &mut Value, homes: &[PathBuf], secrets: &[String]) {
    match value {
        Value::String(text) => *text = sanitize_text(text, homes, secrets),
        Value::Array(values) => {
            for value in values {
                sanitize_value(value, homes, secrets);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                sanitize_value(value, homes, secrets);
            }
        }
        _ => {}
    }
}

fn sanitize_text(value: &str, homes: &[PathBuf], secrets: &[String]) -> String {
    let mut value = value.to_owned();
    // A root or one-letter HOME would rewrite unrelated path separators.
    for home in homes
        .iter()
        .filter_map(|path| path.to_str())
        .filter(|home| home.len() > 2)
    {
        value = value.replace(home, "~");
    }
    for secret in secrets {
        value = value.replace(secret, "[redacted]");
    }
    value
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_server_ps_output() {
        assert_eq!(
            parse_server_ps(" 20480  1.5  01:02:03\n").unwrap(),
            ServerProcess {
                rss_kib: 20_480,
                cpu_percent: 1.5,
                elapsed: "01:02:03".into(),
            }
        );
        assert!(parse_server_ps("not a sample\n").is_err());
    }

    #[test]
    fn parses_only_related_agent_processes_without_returning_commands() {
        let output = " 1024 /x/masil-agent agent --socket /tmp/core.sock ui\n\
                       2048 /x/masil-agent --socket /tmp/core.sock serve --core x\n\
                       4096 /x/masil-agent doctor --socket /tmp/core.sock\n\
                       8192 /x/masil-agent agent --socket /tmp/other.sock sidebar\n\
                       bad /x/masil-agent --socket /tmp/core.sock sidebar\n";
        assert_eq!(parse_agent_ps(output, "/tmp/core.sock"), (2, 3072));
    }

    #[test]
    fn selects_only_masil_agent_pids() {
        let output =
            "  101 zsh\n  202 masil-agent\n  303 masil\n  404 masil-agent\n bad masil-agent\n";
        assert_eq!(agent_pids(output), ["202", "404"]);
    }

    #[test]
    fn redacts_home_prefix() {
        assert_eq!(
            redact_home_path(
                Path::new("/home/example/.config/masil"),
                &[PathBuf::from("/home/example")]
            ),
            "~/.config/masil"
        );
        assert_eq!(
            redact_home_path(
                Path::new("/var/tmp/masil"),
                &[PathBuf::from("/home/example")]
            ),
            "/var/tmp/masil"
        );
    }

    #[test]
    fn counts_check_statuses() {
        let checks = [Status::Ok, Status::Warn, Status::Fail, Status::Skip]
            .into_iter()
            .map(|status| Check {
                id: "test",
                status,
                summary: String::new(),
                detail: json!({}),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            count_summary(&checks),
            Summary {
                ok: 1,
                warn: 1,
                fail: 1,
                skip: 1,
            }
        );
    }
}
