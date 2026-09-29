//! CLI forwarding keeps payloads on stdin and binds actions to a server/run.
use super::{
    Agent, Manager,
    endpoints::{self, Action, Endpoint, Expected, Request},
    fleet,
};
use serde_json::{Value, json};
use std::{
    io::Read,
    os::unix::process::CommandExt,
    path::PathBuf,
    time::{Duration, Instant},
};

pub(super) fn run(id: &str, command: &str, args: &[String]) -> Result<i32, String> {
    let endpoint = endpoints::load()?
        .into_iter()
        .find(|e| e.id == id && e.enabled)
        .ok_or("unknown or disabled endpoint")?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(cancellable(execute(endpoint, command, args)))
}

pub(super) async fn cancellable<F>(operation: F) -> Result<i32, String>
where
    F: std::future::Future<Output = Result<i32, String>>,
{
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(|e| e.to_string())?;
    tokio::select! {
        result = operation => result,
        _ = tokio::signal::ctrl_c() => Err("agent request interrupted; inspect its receipt before retrying".into()),
        _ = terminate.recv() => Err("agent request terminated; outcome may be unknown".into()),
        _ = hangup.recv() => Err("agent connection lost; outcome may be unknown".into()),
    }
}
async fn get(endpoint: &Endpoint, target: &str) -> Result<Agent, String> {
    let value = endpoint
        .call(&Request::Get {
            target: target.into(),
        })
        .await?;
    serde_json::from_value(value).map_err(|e| format!("invalid remote agent: {e}"))
}
async fn execute(endpoint: Endpoint, command: &str, args: &[String]) -> Result<i32, String> {
    let value = match command {
        "list" if args.is_empty() || args == ["--json"] => endpoint.call(&Request::List).await?,
        "view" => {
            endpoint
                .call(&Request::View {
                    args: args.to_vec(),
                })
                .await?
        }
        "save" if args.len() == 1 => {
            endpoint
                .call(&Request::Save {
                    path: args[0].clone(),
                })
                .await?
        }
        "restore" if args.len() == 1 || (args.len() == 2 && args[1] == "--allow-fresh") => {
            endpoint
                .call(&Request::Restore {
                    path: args[0].clone(),
                    allow_fresh: args.len() == 2,
                })
                .await?
        }
        "start" if args.len() >= 2 => {
            let mut cwd = None;
            let mut session = None;
            let mut split = None;
            let mut extra = vec![];
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--" {
                    extra = args[i + 1..].to_vec();
                    break;
                }
                let value = args.get(i + 1).ok_or("missing start option value")?.clone();
                match args[i].as_str() {
                    "--cwd" if cwd.is_none() => cwd = Some(value),
                    "--session" if session.is_none() => session = Some(value),
                    "--split" if split.is_none() => split = Some(value),
                    _ => return Err("invalid remote start option".into()),
                }
                i += 2;
            }
            endpoint
                .call(&Request::Start {
                    name: args[0].clone(),
                    provider: args[1].clone(),
                    cwd: cwd.ok_or("remote start requires an explicit --cwd")?,
                    args: extra,
                    session,
                    split,
                })
                .await?
        }
        _ => {
            let target = args.first().ok_or("remote command requires a target")?;
            let agent = get(&endpoint, target).await?;
            if command == "get" && args.len() == 1 {
                print(&json!(agent))?;
                return Ok(0);
            }
            if command == "explain" && args.len() == 1 {
                print(
                    &json!({"pane_id":agent.pane_id,"provider":agent.provider,"state":agent.state,"evidence":agent.evidence}),
                )?;
                return Ok(0);
            }
            if command == "focus" && args.len() == 1 {
                return connect(&endpoint, &agent);
            }
            if command == "wait" {
                return wait(&endpoint, &agent, &args[1..]).await;
            }
            let action = match command {
                "read" if args.len() == 1 || (args.len() == 2 && args[1] == "--history") => {
                    Action::Read {
                        history: args.len() == 2,
                    }
                }
                "rename" | "attach" if args.len() == 2 => Action::Rename {
                    name: args[1].clone(),
                },
                "send-keys" if args.len() >= 2 => Action::Keys {
                    keys: args[1..].to_vec(),
                },
                "interrupt" if args.len() == 1 => Action::Keys {
                    keys: vec!["C-c".into()],
                },
                "close" if args.len() == 1 => Action::Close,
                "draft" if args.len() == 2 => Action::Draft {
                    text: args[1].clone(),
                },
                "resume" if args.len() == 3 && args[1] == "--name" => Action::Resume {
                    name: args[2].clone(),
                },
                "ack" if args.len() == 5 && args[1] == "--run" && args[3] == "--revision" => {
                    if args[2] != agent.run || args[4] != agent.revision {
                        return Err("attention changed since it was displayed".into());
                    }
                    Action::Ack
                }
                "prompt" if args.len() >= 2 => Action::Prompt {
                    text: args[1].clone(),
                    operation: operation(&agent, &args[2..])?,
                },
                "prompt-receipt" => Action::Receipt {
                    operation: operation(&agent, &args[1..])?,
                },
                _ => return Err("unsupported remote command or invalid arguments".into()),
            };
            endpoint
                .call(&Request::Action {
                    expected: Expected::from(&agent),
                    action,
                })
                .await?
        }
    };
    print(&value)?;
    Ok(0)
}
fn operation(agent: &Agent, args: &[String]) -> Result<Option<u64>, String> {
    let mut run = None;
    let mut operation = None;
    if !args.len().is_multiple_of(2) {
        return Err("missing prompt option value".into());
    }
    for pair in args.chunks_exact(2) {
        match pair[0].as_str() {
            "--run" if run.is_none() => run = Some(pair[1].as_str()),
            "--operation" if operation.is_none() => {
                operation = Some(pair[1].parse().map_err(|_| "invalid operation number")?)
            }
            _ => return Err("prompt accepts --run RUN --operation N".into()),
        }
    }
    if operation.is_some() && run.is_none() {
        return Err("prompt operation requires the original --run".into());
    }
    if run.is_some_and(|r| r != agent.run) {
        return Err("prompt target run changed".into());
    }
    Ok(operation)
}
async fn wait(endpoint: &Endpoint, initial: &Agent, args: &[String]) -> Result<i32, String> {
    let mut state = None;
    let mut timeout = 30.0;
    let mut after_change = false;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--after-change" && !after_change {
            after_change = true;
            i += 1;
            continue;
        }
        let value = args.get(i + 1).ok_or("missing wait option value")?;
        match args[i].as_str() {
            "--state" if state.is_none() => state = Some(value.as_str()),
            "--timeout" => timeout = value.parse::<f64>().map_err(|_| "invalid wait timeout")?,
            _ => return Err("invalid wait option".into()),
        }
        i += 2;
    }
    let desired = state
        .filter(|s| ["idle", "working", "blocked", "exited"].contains(s))
        .ok_or("invalid wait state")?;
    if !timeout.is_finite() || !(0.0..=86400.0).contains(&timeout) {
        return Err("invalid wait timeout".into());
    }
    let until = Instant::now() + Duration::from_secs_f64(timeout);
    let mut changed = !after_change;
    loop {
        let value = match get(endpoint, &initial.pane_id).await {
            Ok(a) if a.run == initial.run && a.boot == initial.boot => a,
            Ok(_) => {
                print(&json!({"stage":"run_changed","run":initial.run}))?;
                return Ok(3);
            }
            Err(e) => {
                print(&json!({"stage":"unavailable","run":initial.run,"error":e}))?;
                return Ok(3);
            }
        };
        changed |= value.state != initial.state || value.revision != initial.revision;
        if changed && value.state == desired {
            print(
                &json!({"stage":"state_observed","run":value.run,"state":value.state,"task_success":null}),
            )?;
            return Ok(0);
        }
        if Instant::now() >= until {
            print(&json!({"stage":"timeout","run":initial.run}))?;
            return Ok(4);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
pub(super) async fn serve(manager: &Manager) -> Result<i32, String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(262145)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let envelope = endpoints::serve(manager, &bytes).await;
    print(&envelope)?;
    Ok(0)
}
pub(super) fn endpoint_connect(args: &[String]) -> Result<i32, String> {
    if args.len() != 7 {
        return Err("invalid endpoint connection identity".into());
    }
    let endpoint = endpoints::load()?
        .into_iter()
        .find(|e| e.id == args[0] && e.enabled)
        .ok_or("unknown or disabled endpoint")?;
    if fleet::key(&endpoint) != args[1] {
        return Err("endpoint connection changed".into());
    }
    exec_connection(&endpoint, &args[2..])
}
fn connect(endpoint: &Endpoint, agent: &Agent) -> Result<i32, String> {
    exec_connection(
        endpoint,
        &[
            agent.pane_id.clone(),
            agent.boot.clone(),
            agent.generation.clone(),
            agent.run.clone(),
            super::nonce()?,
        ],
    )
}
fn exec_connection(endpoint: &Endpoint, identity: &[String]) -> Result<i32, String> {
    let mut argv = vec![
        endpoint.binary.clone(),
        "agent".into(),
        "--socket".into(),
        endpoint.socket.clone(),
        "open-native".into(),
    ];
    argv.extend_from_slice(identity);
    let mut command = if let Some(host) = &endpoint.host {
        let mut command = std::process::Command::new("ssh");
        command.args([
            "-tt",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=3",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
            "--",
            host,
        ]);
        command.arg(argv.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" "));
        command
    } else {
        let mut command = std::process::Command::new(&argv[0]);
        command.args(&argv[1..]);
        command
    };
    command.env_remove("TMUX").env_remove("TMUX_PANE");
    Err(format!(
        "could not connect native terminal: {}",
        command.exec()
    ))
}
pub(super) fn open_native(socket: &str, args: &[String]) -> Result<i32, String> {
    if args.len() != 5 {
        return Err("invalid native connection identity".into());
    }
    let lease = &args[4];
    validate_lease(lease)?;
    let manager = Manager::new(PathBuf::from(socket), None)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let agent = runtime.block_on(manager.get(&args[0]))?;
    if agent.boot != args[1]
        || agent.generation != args[2]
        || agent.run != args[3]
        || agent.process != "running"
    {
        return Err("native agent changed before connection".into());
    }
    let guard = super::and(&[
        super::identity_guard(&agent),
        "#{==:#{pane_dead},0}".into(),
        "#{==:#{session_attached},0}".into(),
        "#{==:#{window_active_clients},0}".into(),
    ]);
    let attach = format!(
        "attach-session -E -t {}",
        quote(&format!("{}.{}", agent.window_id, agent.pane_id))
    );
    let mut child = std::process::Command::new(crate::native_ui::native_executable()?)
        .args([
            "-N",
            "-S",
            socket,
            "if-shell",
            "-F",
            "-t",
            &agent.pane_id,
            &guard,
            &attach,
            "display-message 'Agent changed or its terminal is shared; connection refused'",
        ])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .spawn()
        .map_err(|e| format!("could not attach native terminal: {e}"))?;
    let pid = child.id().to_string();
    let option = format!("@masil-agent-lease-{lease}");
    let registered = runtime.block_on(async {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let clients = manager.command(&["list-clients","-F","#{q:client_pid}\t#{q:client_tty}\t#{q:pane_id}"]).await?;
            if let Some(row) = super::records(&clients)?.into_iter().find(|r| r.len() == 3 && r[0] == pid && r[2] == agent.pane_id) {
                let value = super::encode(&json!({"pid":pid,"tty":row[1],"pane":agent.pane_id,"boot":agent.boot,"run":agent.run}))?;
                manager.command(&["set-option","-g",&option,&value]).await?;
                return Ok::<(),String>(());
            }
            if child.try_wait().map_err(|e| e.to_string())?.is_some() || Instant::now() >= until { return Err("native connection was refused or unavailable".into()); }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    if let Err(error) = registered {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let status = child.wait().map_err(|e| e.to_string());
    let _ = runtime.block_on(manager.command(&["set-option", "-gu", &option]));
    Ok(status?.code().unwrap_or(1))
}
fn validate_lease(lease: &str) -> Result<(), String> {
    if lease.len() != 32 || !lease.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid native connection lease".into());
    }
    Ok(())
}
pub(super) async fn connection_status(
    manager: &Manager,
    agent: &Agent,
    lease: &str,
) -> Result<Value, String> {
    validate_lease(lease)?;
    let option = format!("@masil-agent-lease-{lease}");
    let raw = manager.command(&["show-options", "-gqv", &option]).await?;
    let value: Value =
        super::decode(raw.trim()).ok_or("native connection is starting or no longer exists")?;
    if value["boot"] != agent.boot || value["run"] != agent.run || value["pane"] != agent.pane_id {
        return Err("native connection identity changed".into());
    }
    let clients = manager
        .command(&[
            "list-clients",
            "-F",
            "#{q:client_pid}\t#{q:client_tty}\t#{q:pane_id}",
        ])
        .await?;
    if !super::records(&clients)?.iter().any(|r| {
        r.len() == 3 && value["pid"] == r[0] && value["tty"] == r[1] && r[2] == agent.pane_id
    }) {
        return Err(
            "connection no longer shows this agent; return to it in the native connection window"
                .into(),
        );
    }
    Ok(json!({"stage":"connection_verified"}))
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn print(value: &Value) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?
    );
    Ok(())
}
