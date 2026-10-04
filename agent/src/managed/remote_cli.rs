//! CLI forwarding keeps payloads on stdin and binds actions to a server/run.
use super::{
    Agent, Manager,
    endpoints::{self, Action, Endpoint, Expected, Request},
    failure, fleet,
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
        .ok_or("target_absent: unknown or disabled endpoint")?;
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
        _ = tokio::signal::ctrl_c() => Err("outcome_unknown: agent request interrupted; inspect its receipt before retrying".into()),
        _ = terminate.recv() => Err("outcome_unknown: agent request terminated; outcome may be unknown".into()),
        _ = hangup.recv() => Err("outcome_unknown: agent connection lost; outcome may be unknown".into()),
    }
}
/// This server, when the command runs inside one: its link to the
/// endpoint, if up, carries reads.
fn local_socket() -> Option<PathBuf> {
    std::env::var("TMUX")
        .ok()
        .and_then(|value| value.rsplitn(3, ',').last().map(PathBuf::from))
        .or_else(|| std::env::var_os("MASIL_AGENT_SOCKET").map(PathBuf::from))
        .filter(|path| path.is_absolute())
}

async fn get(endpoint: &Endpoint, target: &str) -> Result<Agent, String> {
    let value = endpoint
        .call_through(
            &Request::Get {
                target: target.into(),
            },
            local_socket().as_deref(),
        )
        .await?;
    serde_json::from_value(value)
        .map_err(|e| format!("endpoint_unreachable: invalid remote agent: {e}"))
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
            let mut boot = None;
            let mut operation = None;
            let mut worktree = None;
            let mut answers = false;
            let mut extra = vec![];
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--" {
                    extra = args[i + 1..].to_vec();
                    break;
                }
                if args[i] == "--answers" && !answers {
                    answers = true;
                    i += 1;
                    continue;
                }
                let value = args
                    .get(i + 1)
                    .ok_or("usage: missing start option value")?
                    .clone();
                match args[i].as_str() {
                    "--cwd" if cwd.is_none() => cwd = Some(value),
                    "--session" if session.is_none() => session = Some(value),
                    "--split" if split.is_none() => split = Some(value),
                    "--boot" if boot.is_none() => boot = Some(value),
                    "--operation" if operation.is_none() => operation = Some(value),
                    "--worktree" if worktree.is_none() => worktree = Some(value),
                    _ => return Err("usage: invalid remote start option".into()),
                }
                i += 2;
            }
            if boot.is_some() != operation.is_some() {
                return Err("usage: --operation ID and --boot BOOT must be given together".into());
            }
            endpoint
                .call(&Request::Start {
                    name: args[0].clone(),
                    provider: args[1].clone(),
                    cwd: cwd.ok_or("usage: remote start requires an explicit --cwd")?,
                    args: extra,
                    session,
                    split,
                    boot,
                    operation_id: operation,
                    worktree,
                    answers,
                })
                .await?
        }
        "find" => {
            endpoint
                .call(&Request::Find {
                    args: args.to_vec(),
                })
                .await?
        }
        "operations" | "operation" if command == "operations" || !args.is_empty() => {
            endpoint
                .call(&Request::Operations {
                    single: command == "operation",
                    args: args.to_vec(),
                })
                .await?
        }
        "inbox" => {
            endpoint
                .call(&Request::Inbox {
                    args: args.to_vec(),
                })
                .await?
        }
        "put" if !args.is_empty() => {
            let local = PathBuf::from(&args[0]);
            let destination = match &args[1..] {
                [flag, dir] if flag == "--dir" => super::transfer::Destination::Directory {
                    dir: dir.clone(),
                    name: local
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or("invalid_argument: the file name is not UTF-8")?
                        .to_owned(),
                },
                [flag, dir, name_flag, name] if flag == "--dir" && name_flag == "--name" => {
                    super::transfer::Destination::Directory {
                        dir: dir.clone(),
                        name: name.clone(),
                    }
                }
                [flag, target] if flag == "--attachment" => {
                    super::transfer::Destination::Attachment {
                        run: get(&endpoint, target).await?.run,
                    }
                }
                _ => {
                    return Err("usage: put LOCAL_FILE (--dir REMOTE_DIR [--name NAME] | --attachment TARGET)".into());
                }
            };
            super::transfer::send(&endpoint, &local, destination).await?
        }
        "worktree" if !args.is_empty() => {
            endpoint
                .call(&Request::Worktrees {
                    args: args.to_vec(),
                })
                .await?
        }
        _ => {
            let target = args
                .first()
                .ok_or("usage: remote command requires a target")?;
            if command == "wait" && target.starts_with("--") {
                return Err("usage: an endpoint waits only for an agent's state (`wait TARGET --state S`); run the other waits on that host".into());
            }
            let agent = match get(&endpoint, target).await {
                Ok(agent) => agent,
                // A keyed retry whose target is gone still has its receipt
                // in the endpoint's store, as here.
                Err(error) if matches!(command, "interrupt" | "close") => {
                    let Some(key) = receipt_key(command, &args[1..]) else {
                        return Err(error);
                    };
                    let found = endpoint
                        .call(&Request::Operations {
                            single: true,
                            args: vec![key.clone()],
                        })
                        .await;
                    let Ok(record) = found else {
                        return Err(format!(
                            "{error}; its receipt would be `masil-agent agent --endpoint {} operation {key}`",
                            endpoint.id
                        ));
                    };
                    let stage = record
                        .get("stage")
                        .or_else(|| record.get("state"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    print(&record)?;
                    return Ok(failure::recorded_exit_code(&stage, false));
                }
                Err(error) => return Err(error),
            };
            if command == "get" && args.len() == 1 {
                print(&json!(agent))?;
                return Ok(0);
            }
            if command == "capabilities" && args.len() == 1 {
                // Older endpoints omit the contract; defaults read as unknown, not supported.
                print(&super::cli::capability_report(&agent))?;
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
                "interrupt" => Action::Interrupt {
                    operation: keyed(&agent, &args[1..])?,
                },
                "close" if args.len() == 1 => Action::Close,
                "close" => Action::CloseOperation {
                    operation: keyed(&agent, &args[1..])?
                        .ok_or("usage: close TARGET [--run RUN --operation ID]")?,
                },
                "requests" if args.len() == 1 => Action::Requests,
                "answer" if args.len() >= 3 => Action::Answer {
                    request: args[1].clone(),
                    args: args[2..].to_vec(),
                },
                "queue" => Action::Queue {
                    args: upload(&endpoint, &agent, &args[1..]).await?,
                },
                "changes" => Action::Changes {
                    args: args[1..].to_vec(),
                },
                "draft" if args.len() == 2 => Action::Draft {
                    text: args[1].clone(),
                },
                "resume" if args.len() == 3 && args[1] == "--name" => Action::Resume {
                    name: args[2].clone(),
                },
                "ack" if args.len() == 5 && args[1] == "--run" && args[3] == "--revision" => {
                    if args[2] != agent.run || args[4] != agent.revision {
                        return Err(
                            "identity_mismatch: attention changed since it was displayed".into(),
                        );
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
                _ => return Err("usage: unsupported remote command or invalid arguments".into()),
            };
            let unkeyed = matches!(action, Action::Interrupt { operation: None });
            let request = Request::Action {
                expected: Expected::from(&agent),
                action,
            };
            match endpoint.call(&request).await {
                // An endpoint older than durable remote interrupts still
                // takes the key press: the provider's measured keys, while
                // it is listed as working (it cannot check its own screen).
                Err(error) if unkeyed && error.starts_with("remote_unsupported") => {
                    // Its own evidence, judged by this side's rules: a
                    // request anywhere on the screen, or a turn these keys
                    // do not end, and nothing is sent.
                    let engine = crate::detection::Engine::load()?;
                    let evidence = serde_json::to_value(&*agent.evidence).unwrap_or_default();
                    let screen = if evidence["source"] == "run_report" {
                        &evidence["screen"]
                    } else {
                        &evidence
                    };
                    let blocked = screen["explanations"].as_array().is_some_and(|rules| {
                        rules
                            .iter()
                            .any(|rule| rule["matched"] == true && rule["state"] == "blocked")
                    });
                    let rule = screen["matched_rule"]["id"].as_str().unwrap_or_default();
                    if agent.state != "working"
                        || blocked
                        || !engine.interruptible_rule(&agent.provider, rule)
                    {
                        return Err("interrupt_not_working: the endpoint's screen shows no turn these keys end; nothing was sent".into());
                    }
                    let keys = engine.interrupt_keys(&agent.provider);
                    if keys.is_empty() {
                        return Err(format!(
                            "interrupt_unverified: masil has not measured how {} interrupts a turn; nothing was sent",
                            agent.provider
                        ));
                    }
                    let mut sent = endpoint
                        .call(&Request::Action {
                            expected: Expected::from(&agent),
                            action: Action::Keys { keys },
                        })
                        .await?;
                    sent["durable"] = json!(false);
                    print(&sent)?;
                    return Ok(0);
                }
                result => result?,
            }
        }
    };
    // A record found again has its state, not a stage.
    let stage = value
        .get("stage")
        .or_else(|| value.get("state"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    print(&value)?;
    let words: Vec<&str> = args
        .iter()
        .enumerate()
        .filter(|(at, word)| *word != "--revision" && (*at == 0 || args[at - 1] != "--revision"))
        .map(|(_, word)| word.as_str())
        .collect();
    let sent = command == "queue" && words.get(1) == Some(&"send");
    let record = matches!(
        command,
        "start" | "resume" | "prompt" | "close" | "interrupt" | "operation"
    ) || sent;
    Ok(match record {
        true => failure::recorded_exit_code(
            if stage == "pending" {
                "dispatching"
            } else {
                stage
            },
            command == "operation" && args.len() == 1,
        ),
        false => 0,
    })
}
/// `queue T attach ID --upload LOCAL`: the file goes to the agent's
/// attachment directory there first, and the item names it there.
async fn upload(
    endpoint: &Endpoint,
    agent: &Agent,
    args: &[String],
) -> Result<Vec<String>, String> {
    let Some(at) = args.iter().position(|word| word == "--upload") else {
        return Ok(args.to_vec());
    };
    let local = args
        .get(at + 1)
        .ok_or("usage: queue TARGET attach ID --upload LOCAL_FILE")?;
    if args.first().map(String::as_str) != Some("attach") {
        return Err("usage: --upload goes with queue TARGET attach ID".into());
    }
    let receipt = super::transfer::send(
        endpoint,
        std::path::Path::new(local),
        super::transfer::Destination::Attachment {
            run: agent.run.clone(),
        },
    )
    .await?;
    let remote = receipt["path"]
        .as_str()
        .ok_or("endpoint_unreachable: the receipt names no path")?
        .to_owned();
    let mut words = args.to_vec();
    words.splice(at..at + 2, [remote]);
    Ok(words)
}

/// The receipt key of a keyed interrupt or close, from its words.
fn receipt_key(command: &str, args: &[String]) -> Option<String> {
    let value = |flag: &str| {
        args.iter()
            .position(|word| word == flag)
            .and_then(|at| args.get(at + 1))
    };
    Some(format!(
        "run:{}/{command}/{}",
        value("--run")?,
        value("--operation")?
    ))
}

/// `--run RUN --operation ID` for a keyed interrupt or close: the run must
/// be the one the endpoint runs now, or the key could name another.
fn keyed(agent: &Agent, args: &[String]) -> Result<Option<String>, String> {
    let mut run = None;
    let mut id = None;
    let mut i = 0;
    while i < args.len() {
        let value = args
            .get(i + 1)
            .ok_or("usage: accepts --run RUN --operation ID")?;
        match args[i].as_str() {
            "--run" if run.is_none() => run = Some(value),
            "--operation" if id.is_none() => id = Some(value.clone()),
            _ => return Err("usage: accepts --run RUN --operation ID".into()),
        }
        i += 2;
    }
    match (run, id) {
        (None, None) => Ok(None),
        (Some(run), Some(id)) if *run == agent.run => Ok(Some(id)),
        (Some(_), Some(_)) => {
            Err("identity_mismatch: target run changed; this operation cannot be replayed".into())
        }
        _ => Err("usage: --operation ID and --run RUN must be given together".into()),
    }
}

fn operation(agent: &Agent, args: &[String]) -> Result<Option<u64>, String> {
    let mut run = None;
    let mut operation = None;
    if !args.len().is_multiple_of(2) {
        return Err("usage: missing prompt option value".into());
    }
    for pair in args.chunks_exact(2) {
        match pair[0].as_str() {
            "--run" if run.is_none() => run = Some(pair[1].as_str()),
            "--operation" if operation.is_none() => {
                operation = Some(
                    pair[1]
                        .parse()
                        .map_err(|_| "invalid_argument: invalid operation number")?,
                )
            }
            _ => return Err("usage: prompt accepts --run RUN --operation N".into()),
        }
    }
    if operation.is_some() && run.is_none() {
        return Err("usage: prompt operation requires the original --run".into());
    }
    if run.is_some_and(|r| r != agent.run) {
        return Err("identity_mismatch: prompt target run changed".into());
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
        let value = args.get(i + 1).ok_or("usage: missing wait option value")?;
        match args[i].as_str() {
            "--state" if state.is_none() => state = Some(value.as_str()),
            "--timeout" => {
                timeout = value
                    .parse::<f64>()
                    .map_err(|_| "invalid_argument: invalid wait timeout")?
            }
            _ => return Err("usage: invalid wait option".into()),
        }
        i += 2;
    }
    let desired = state
        .filter(|s| ["idle", "working", "blocked", "exited"].contains(s))
        .ok_or("invalid_argument: invalid wait state")?;
    if !timeout.is_finite() || !(0.0..=86400.0).contains(&timeout) {
        return Err("invalid_argument: invalid wait timeout".into());
    }
    let until = Instant::now() + Duration::from_secs_f64(timeout);
    let mut changed = !after_change;
    loop {
        let value = match get(endpoint, &initial.pane_id).await {
            Ok(a) if a.run == initial.run && a.boot == initial.boot => a,
            Ok(_) => {
                print(
                    &json!({"stage":"run_changed","end_reason":"run_changed","run":initial.run}),
                )?;
                return Ok(5);
            }
            Err(e) => {
                let (class, code) = failure::classify(&e);
                // The endpoint does not say whether the pane closed or only
                // the agent left it.
                let end_reason = if code == "target_absent" {
                    "target_removed"
                } else {
                    "observation_lost"
                };
                print(
                    &json!({"stage":"unavailable","end_reason":end_reason,"run":initial.run,"error":e}),
                )?;
                // No result from the endpoint is transient unless the
                // endpoint said why: an error without a code, as from an older
                // endpoint, is still only a missing result.
                return Ok(if code == "failed" {
                    4
                } else {
                    class.exit_code()
                });
            }
        };
        changed |= value.state != initial.state || value.revision != initial.revision;
        if changed && value.state == desired {
            print(
                &json!({"stage":"state_observed","end_reason":"state_observed","run":value.run,"state":value.state,"task_success":null}),
            )?;
            return Ok(0);
        }
        if value.state == "exited" || value.process == "exited" {
            print(
                &json!({"stage":"state_observed","end_reason":"run_ended","run":value.run,"state":"exited","task_success":null}),
            )?;
            return Ok(5);
        }
        if Instant::now() >= until {
            print(&json!({"stage":"timeout","end_reason":"timeout","run":initial.run}))?;
            return Ok(6);
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
        return Err("invalid_argument: invalid endpoint connection identity".into());
    }
    let endpoint = endpoints::load()?
        .into_iter()
        .find(|e| e.id == args[0] && e.enabled)
        .ok_or("target_absent: unknown or disabled endpoint")?;
    if fleet::key(&endpoint) != args[1] {
        return Err("identity_mismatch: endpoint connection changed".into());
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
        "endpoint_unreachable: could not connect native terminal: {}",
        command.exec()
    ))
}

pub(super) fn open_native(socket: &str, args: &[String]) -> Result<i32, String> {
    if args.len() != 5 {
        return Err("invalid_argument: invalid native connection identity".into());
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
        return Err("identity_mismatch: native agent changed before connection".into());
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
        .map_err(|e| format!("server_unreachable: could not attach native terminal: {e}"))?;
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
            if child.try_wait().map_err(|e| e.to_string())?.is_some() || Instant::now() >= until { return Err("server_unreachable: native connection was refused or unavailable".into()); }
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
        return Err("invalid_argument: invalid native connection lease".into());
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
        return Err("identity_mismatch: native connection identity changed".into());
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
