use super::{Agent, Manager, commands, failure, validate_args};
use crate::providers;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(crate) fn run(args: &[String]) -> Result<i32, String> {
    match run_inner(args) {
        Err(error) if is_integration_hook(args) => {
            eprintln!("masil-agent: {error}");
            Ok(0)
        }
        result => result,
    }
}

fn is_integration_hook(args: &[String]) -> bool {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if matches!(argument.as_str(), "--socket" | "--client" | "--endpoint") {
            index += 2;
        } else {
            break;
        }
    }
    matches!(
        args.get(index..),
        Some([command, action, ..]) if command == "integration" && action == "hook"
    )
}

fn run_inner(args: &[String]) -> Result<i32, String> {
    if args.is_empty() || args == ["--help"] || args == ["help"] {
        println!("{}", commands::help());
        return Ok(0);
    }
    let mut index = 0;
    let mut socket = None;
    let mut client = None;
    let mut endpoint = None;
    while index < args.len() {
        match args[index].as_str() {
            "--socket" if socket.is_none() => {
                socket = Some(value(args, index)?.to_owned());
                index += 2;
            }
            "--client" if client.is_none() => {
                client = Some(value(args, index)?.to_owned());
                index += 2;
            }
            "--endpoint" if endpoint.is_none() => {
                endpoint = Some(value(args, index)?.to_owned());
                index += 2;
            }
            _ => break,
        }
    }
    let command = args.get(index).ok_or("usage: missing agent command")?;
    let rest = &args[index + 1..];
    if ["help", "--help", "-h"].contains(&command.as_str()) && rest.is_empty() {
        println!("{}", commands::help());
        return Ok(0);
    }
    commands::verb(command).ok_or_else(|| {
        format!("usage: unknown agent command '{command}'; see masil-agent agent --help")
    })?;
    if command == "providers" && rest.is_empty() {
        print(
            &json!({"providers":providers::all().iter().map(|p|json!({"id":p.id,"command":p.command,"aliases":p.aliases,"resume":providers::resume(p.id,"example").is_ok(),"integration":super::integration::target_capability(p.id)})).collect::<Vec<_>>()}),
        )?;
        return Ok(0);
    }
    if command == "capabilities" && rest.len() == 2 && rest[0] == "--provider" {
        let engine = crate::detection::Engine::load()?;
        print(&super::provider_contract(&engine, &rest[1])?)?;
        return Ok(0);
    }
    if command == "endpoints" || command == "endpoint" {
        print(&super::endpoints::configure(rest)?)?;
        return Ok(0);
    }
    if command == "endpoint-connect" {
        return super::remote_cli::endpoint_connect(rest);
    }
    if let Some(endpoint) = endpoint {
        if socket.is_some() || client.is_some() {
            return Err("usage: --endpoint cannot be combined with --socket or --client".into());
        }
        if !commands::supports_remote(command) {
            return Err(format!(
                "usage: {command} is not available with --endpoint; see masil-agent agent --help"
            ));
        }
        return super::remote_cli::run(&endpoint, command, rest);
    }
    let socket = socket
        .or_else(|| {
            std::env::var("TMUX")
                .ok()
                .and_then(|v| v.rsplitn(3, ',').last().map(str::to_owned))
        })
        .or_else(|| std::env::var("MASIL_AGENT_SOCKET").ok())
        .ok_or("usage: specify --socket PATH or run this command inside masil")?;
    if command == "ui" || command == "sidebar" {
        let socket = PathBuf::from(&socket)
            .canonicalize()
            .map_err(|e| format!("server_unreachable: native socket: {e}"))?;
        let socket = socket
            .to_str()
            .ok_or("invalid_argument: native socket path is not UTF-8")?;
        let mut args = rest.to_vec();
        if let Some(client) = client {
            args.extend(["--client".into(), client]);
        }
        return crate::ui::run_managed(socket, &args, command == "sidebar");
    }
    if command == "coordinator" {
        let socket = PathBuf::from(&socket);
        let report = match rest {
            [action] if action == "status" => crate::coordinator::status(&socket)?,
            [action] if action == "start" => crate::coordinator::ensure(&socket)?,
            [action] if action == "stop" => crate::coordinator::stop(&socket)?,
            _ => return Err("usage: masil-agent agent coordinator status|start|stop".into()),
        };
        print(&report)?;
        return Ok(0);
    }
    if command == "exec-managed" {
        return launch_registered(&socket, rest);
    }
    if command == "open-native" {
        return super::remote_cli::open_native(&socket, rest);
    }
    let manager = Manager::new(PathBuf::from(socket), client)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(super::remote_cli::cancellable(execute(
            manager, command, rest,
        )))
}

/// Keep native startup callbacks behind registration, then replace this wrapper
/// with the original provider process. No second provider or terminal parser.
fn launch_registered(socket: &str, args: &[String]) -> Result<i32, String> {
    use std::os::unix::process::CommandExt;
    validate_args(args)?;
    let program = args
        .first()
        .ok_or("usage: managed launcher requires an executable")?;
    let pane = std::env::var("TMUX_PANE")
        .map_err(|_| "invalid_argument: managed launcher needs TMUX_PANE")?;
    crate::pane_id(&pane).map_err(|error| format!("unknown_target_syntax: {error}"))?;
    let run = std::env::var("MASIL_AGENT_RUN")
        .map_err(|_| "invalid_argument: managed launcher needs a run identity")?;
    let native = crate::native_ui::Context {
        socket: socket.into(),
        client: None,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let output = native
                        .tmux(
                            ["show-options", "-pqv", "-t", &pane, super::META]
                                .iter()
                                .map(std::ffi::OsString::from),
                            None,
                        )
                        .await
                        .map_err(super::server_unreachable)?;
                    let encoded = String::from_utf8(output.stdout)
                        .map_err(|_| "outcome_unknown: invalid launch registration")?;
                    if let Some(meta) = super::decode::<super::Metadata>(encoded.trim()) {
                        if meta.run != run
                            || meta.argv != args
                            || meta.foreground_group != std::process::id() as i32
                        {
                            return Err("identity_mismatch: managed launch registration does not match this process"
                                .to_string());
                        }
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .map_err(|_| "wait_timeout: managed launch registration timed out".to_string())?
        })?;
    let error = std::process::Command::new(program).args(&args[1..]).exec();
    Err(format!("provider could not start: {error}"))
}

/// A client operation key needs the namespace the client observed.
fn client_key<'a>(
    pin: Option<&'a str>,
    operation: Option<&'a str>,
    flag: &str,
) -> Result<Option<super::durable::ClientKey<'a>>, String> {
    match (pin, operation) {
        (Some(pin), Some(id)) => Ok(Some(super::durable::ClientKey { pin, id })),
        (None, None) => Ok(None),
        _ => Err(format!(
            "usage: --operation ID and {flag} must be given together"
        )),
    }
}

fn run_operation(args: &[String]) -> Result<(Option<&str>, Option<&str>), String> {
    let mut run = None;
    let mut operation = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--run" if run.is_none() => run = Some(value(args, i)?),
            "--operation" if operation.is_none() => operation = Some(value(args, i)?),
            _ => return Err("usage: accepts --run RUN --operation ID".into()),
        }
        i += 2;
    }
    Ok((run, operation))
}

/// A keyed retry whose target is gone still has a durable receipt.
fn keyed_error(error: String, action: &str, key: Option<super::durable::ClientKey<'_>>) -> String {
    match key {
        Some(key) => format!(
            "{error}; its receipt is `masil-agent agent operation run:{}/{action}/{}`",
            key.pin, key.id
        ),
        None => error,
    }
}

pub(super) fn capability_report(agent: &Agent) -> serde_json::Value {
    json!({"pane_id":agent.pane_id,"run":agent.run,"provider":agent.provider,"process":agent.process,"state":agent.state,"binding":agent.binding,"capabilities":agent.capabilities})
}

fn value(args: &[String], index: usize) -> Result<&str, String> {
    args.get(index + 1)
        .map(String::as_str)
        .ok_or_else(|| format!("usage: missing value for {}", args[index]))
}

fn print<T: Serialize + ?Sized>(value: &T) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?
    );
    Ok(())
}

/// Prints a durable result and returns the exit status of the stage it
/// shows, so the status always agrees with the printed stage.
async fn print_record(_manager: &Manager, value: &Value, query: bool) -> Result<i32, String> {
    let stage = value
        .get("stage")
        .or_else(|| value.get("state"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    print(value)?;
    Ok(stage.map_or(0, |stage| failure::recorded_exit_code(&stage, query)))
}

async fn execute(mut manager: Manager, command: &str, args: &[String]) -> Result<i32, String> {
    match command {
        "rpc" if args.is_empty() => return super::remote_cli::serve(&manager).await,
        "list" if args == ["--all"] => {
            let fleet = super::fleet::Fleet::new(manager)?;
            let deadline = Instant::now() + Duration::from_secs(9);
            loop {
                let snapshot = fleet.poll().await?;
                if !snapshot
                    .endpoints
                    .iter()
                    .any(|e| e.error.as_deref() == Some("connecting"))
                    || Instant::now() >= deadline
                {
                    print(
                        &json!({"agents":snapshot.agents,"partial":snapshot.endpoints.iter().any(|e| !e.connected),"endpoints":snapshot.endpoints}),
                    )?;
                    return Ok(0);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        "view" => print(&manager.view(args).await?)?,
        "save" if args.len() == 1 => print(&manager.save(std::path::Path::new(&args[0])).await?)?,
        "restore" if args.len() == 1 || (args.len() == 2 && args[1] == "--allow-fresh") => {
            print(
                &manager
                    .restore(std::path::Path::new(&args[0]), args.len() == 2)
                    .await?,
            )?;
        }
        "integration" => {
            if let Some(value) = super::integration::run(&manager, args).await? {
                print(&value)?;
            }
        }
        "list" if args.is_empty() || args == ["--json"] => {
            #[derive(Serialize)]
            struct AgentList {
                agents: Vec<Agent>,
            }

            print(&AgentList {
                agents: manager.list_view().await?,
            })?
        }
        "capabilities" if args.len() == 1 => {
            let agent = manager.get(&args[0]).await?;
            print(&capability_report(&agent))?;
        }
        "get" | "explain" if args.len() == 1 => {
            let agent = manager.get(&args[0]).await?;
            print(&if command == "get" {
                json!(agent)
            } else {
                json!({"pane_id":agent.pane_id,"provider":agent.provider,"state":agent.state,"evidence":agent.evidence})
            })?;
        }
        "read" if args.len() == 1 || (args.len() == 2 && args[1] == "--history") => {
            let agent = manager.get(&args[0]).await?;
            print(
                &json!({"pane_id":agent.pane_id,"run":agent.run,"source":if args.len()==2 {"recent"}else{"visible"},"text":manager.read(&agent,args.len()==2).await?}),
            )?;
        }
        "start" if args.len() >= 2 => {
            let mut cwd = None;
            let mut split = None;
            let mut session = None;
            let mut boot = None;
            let mut operation = None;
            let mut extra = Vec::new();
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--cwd" if cwd.is_none() => cwd = Some(PathBuf::from(value(args, i)?)),
                    "--split" if split.is_none() => split = Some(value(args, i)?),
                    "--session" if session.is_none() => session = Some(value(args, i)?),
                    "--boot" if boot.is_none() => boot = Some(value(args, i)?),
                    "--operation" if operation.is_none() => operation = Some(value(args, i)?),
                    "--" => {
                        extra = args[i + 1..].to_vec();
                        break;
                    }
                    other => {
                        return Err(format!("usage: unknown or repeated start option: {other}"));
                    }
                }
                i += 2;
            }
            let outcome = manager
                .start_with_operation(
                    &args[0],
                    &args[1],
                    &cwd.unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?),
                    &extra,
                    session,
                    split,
                    client_key(boot, operation, "--boot BOOT")?,
                )
                .await?;
            // A new agent needs watching: start a coordinator that should be
            // running and is not, without waiting for it (D1).
            let socket = manager.native.socket.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let enabled = super::state_base()
                    .and_then(|base| super::coordinator_features(&base, &socket))
                    .is_ok_and(|features| !features.is_empty());
                if enabled {
                    crate::coordinator::spawn_detached(&socket);
                    // A running one looks at the new pane now.
                    crate::coordinator::poke(&socket);
                }
            })
            .await;
            return print_record(&manager, &outcome, false).await;
        }
        "rename" | "attach" if args.len() == 2 => {
            let agent = manager.get(&args[0]).await?;
            manager.rename(&agent, &args[1]).await?;
            print(&json!({"stage":"registered","pane_id":agent.pane_id,"name":args[1]}))?;
        }
        "focus" if args.len() == 1 => {
            let agent = manager.get(&args[0]).await?;
            manager.focus(&agent).await?;
            print(&json!({"stage":"selected","pane_id":agent.pane_id,"run":agent.run}))?;
        }
        "interrupt" | "close" if !args.is_empty() => {
            let (run, operation) = run_operation(&args[1..])?;
            let key = client_key(run, operation, "--run RUN")?;
            let agent = match manager.get(&args[0]).await {
                Ok(agent) => agent,
                Err(error) => return Err(keyed_error(error, command, key)),
            };
            if key.is_some_and(|key| key.pin != agent.run) {
                return Err(keyed_error(
                    format!(
                        "identity_mismatch: {command} target run changed; this operation cannot be replayed"
                    ),
                    command,
                    key,
                ));
            }
            let outcome = if command == "interrupt" {
                manager.interrupt(&agent, key).await?
            } else {
                manager.close_with_operation(&agent, key).await?
            };
            return print_record(&manager, &outcome, false).await;
        }
        "find" => print(&manager.find(args).await?)?,
        "operations" => print(&manager.operations_command(false, args).await?)?,
        "inbox" => print(&super::inbox::command(&manager, args).await?)?,
        "operation" if !args.is_empty() => {
            let record = manager.operations_command(true, args).await?;
            return print_record(&manager, &record, args.len() == 1).await;
        }
        "ack" if args.len() == 5 && args[1] == "--run" && args[3] == "--revision" => {
            let agent = manager.get(&args[0]).await?;
            if agent.run != args[2] || agent.revision != args[4] {
                return Err("identity_mismatch: attention changed since it was displayed".into());
            }
            manager.acknowledge(&agent).await?;
            print(&json!({"stage":"seen","revision":agent.revision}))?;
        }
        "send-keys" if args.len() >= 2 => {
            let agent = manager.get(&args[0]).await?;
            print(&manager.keys(&agent, &args[1..]).await?)?;
        }
        "draft" if args.len() == 2 => {
            let agent = manager.get(&args[0]).await?;
            print(&manager.draft(&agent, &args[1]).await?)?;
        }
        "prompt" if args.len() >= 2 => {
            let agent = manager.get(&args[0]).await?;
            let mut operation = None;
            let mut run = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--run" if run.is_none() => run = Some(value(args, i)?),
                    "--operation" if operation.is_none() => {
                        operation = Some(
                            value(args, i)?
                                .parse::<u64>()
                                .map_err(|_| "invalid_argument: invalid prompt operation")?,
                        );
                    }
                    _ => return Err("usage: prompt accepts --run RUN --operation N".into()),
                }
                i += 2;
            }
            if operation.is_some() && run.is_none() {
                return Err(
                    "usage: prompt operation requires --run from the original agent observation"
                        .into(),
                );
            }
            if run.is_some_and(|run| run != agent.run) {
                return Err("identity_mismatch: prompt target run changed; this operation cannot be replayed".into());
            }
            let outcome = manager
                .prompt_with_operation(&agent, &args[1], operation)
                .await?;
            return print_record(&manager, &outcome, false).await;
        }
        "prompt-receipt" if !args.is_empty() => {
            let agent = manager.get(&args[0]).await?;
            let mut operation = None;
            let mut run = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--run" if run.is_none() => run = Some(value(args, i)?),
                    "--operation" if operation.is_none() => {
                        operation = Some(
                            value(args, i)?
                                .parse::<u64>()
                                .map_err(|_| "invalid_argument: invalid prompt operation")?,
                        );
                    }
                    _ => return Err("usage: prompt-receipt accepts --run RUN --operation N".into()),
                }
                i += 2;
            }
            if operation.is_some() && run.is_none() {
                return Err(
                    "usage: prompt operation requires --run from the original agent observation"
                        .into(),
                );
            }
            if run.is_some_and(|run| run != agent.run) {
                return Err(
                    "identity_mismatch: prompt target run changed; its receipt is unavailable"
                        .into(),
                );
            }
            print(&manager.prompt_receipt(&agent, operation).await?)?;
        }
        "resume" if args.len() >= 3 => {
            let agent = manager.get(&args[0]).await?;
            let mut name = None;
            let mut split = None;
            let mut boot = None;
            let mut operation = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--name" if name.is_none() => name = Some(value(args, i)?),
                    "--split" if split.is_none() => split = Some(value(args, i)?),
                    "--boot" if boot.is_none() => boot = Some(value(args, i)?),
                    "--operation" if operation.is_none() => operation = Some(value(args, i)?),
                    _ => {
                        return Err(
                            "usage: resume accepts --name NAME, --split %N and --boot BOOT --operation ID"
                                .into(),
                        );
                    }
                }
                i += 2;
            }
            let mut outcome = manager
                .start_with_operation(
                    name.ok_or("usage: resume requires a new --name")?,
                    &agent.provider,
                    std::path::Path::new(&agent.cwd),
                    &[],
                    Some(manager.resume_session(&agent)?),
                    split,
                    client_key(boot, operation, "--boot BOOT")?,
                )
                .await?;
            // The new run has only a request; the source run's evidence stays separate.
            outcome["source_binding"] = json!(agent.binding);
            return print_record(&manager, &outcome, false).await;
        }
        "wait" if args.len() >= 3 => return wait(&manager, args).await,
        "report" => {
            let mut pane = None;
            let mut run = None;
            let mut sequence = None;
            let mut state = None;
            let mut session = None;
            let mut i = 0;
            while i < args.len() {
                match args[i].as_str() {
                    "--pane" if pane.is_none() => pane = Some(value(args, i)?),
                    "--run" if run.is_none() => run = Some(value(args, i)?),
                    "--sequence" if sequence.is_none() => {
                        sequence = Some(
                            value(args, i)?
                                .parse::<u64>()
                                .map_err(|_| "invalid_argument: invalid sequence")?,
                        )
                    }
                    "--state" if state.is_none() => state = Some(value(args, i)?),
                    "--session" if session.is_none() => session = Some(value(args, i)?),
                    _ => return Err("usage: invalid report options".into()),
                }
                i += 2;
            }
            let result = manager
                .report(
                    pane.ok_or("usage: report requires --pane")?,
                    run.ok_or("usage: report requires --run")?,
                    sequence.ok_or("usage: report requires --sequence")?,
                    state.ok_or("usage: report requires --state")?,
                    session,
                )
                .await?;
            print(
                &json!({"stage":"report_recorded","binding":result.binding.map(|b| b.label()),"state_applied":result.state_applied}),
            )?;
        }
        "reload" if args.is_empty() => {
            manager.reload()?;
            let socket = manager.native.socket.clone();
            let _ = tokio::task::spawn_blocking(move || crate::coordinator::reload(&socket)).await;
            print(
                &json!({"stage":"manifests_validated","note":"new commands and views load current overrides"}),
            )?;
        }
        _ => {
            return Err(format!(
                "usage: invalid agent command or arguments; see masil-agent agent --help\n{}",
                commands::help()
            ));
        }
    }
    Ok(0)
}

async fn wait(manager: &Manager, args: &[String]) -> Result<i32, String> {
    let mut state = None;
    let mut timeout = 30.0;
    let mut after = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--state" if state.is_none() => state = Some(value(args, i)?),
            "--timeout" => {
                timeout = value(args, i)?
                    .parse::<f64>()
                    .map_err(|_| "invalid_argument: invalid timeout")?
            }
            "--after-change" if !after => {
                after = true;
                i += 1;
                continue;
            }
            _ => return Err("usage: invalid wait options".into()),
        }
        i += 2;
    }
    if !(0.05..=300.0).contains(&timeout) {
        return Err("invalid_argument: timeout must be 0.05–300 seconds".into());
    }
    let state = state.ok_or("usage: wait requires --state")?;
    validate_args(&[state.into()])?;
    if !["idle", "working", "blocked", "exited", "unknown"].contains(&state) {
        return Err("invalid_argument: unsupported wait state".into());
    }
    let initial = manager.get(&args[0]).await?;
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    let mut changed = false;
    loop {
        let agent = match manager.get(&initial.pane_id).await {
            Ok(agent) => agent,
            Err(error) => {
                let absent = failure::classify(&error).1 == "target_absent";
                let reason = error.strip_prefix("target_absent: ").unwrap_or(&error);
                print(
                    &json!({"outcome":if absent{"target_removed"}else{"observation_lost"},"reason":reason,"pane_id":initial.pane_id,"run":initial.run}),
                )?;
                return Ok(if absent { 5 } else { 6 });
            }
        };
        if agent.boot != initial.boot
            || agent.generation != initial.generation
            || agent.run != initial.run
        {
            print(&json!({"outcome":"run_changed","pane_id":initial.pane_id,"run":initial.run}))?;
            return Ok(5);
        }
        changed |= agent.state != initial.state;
        if agent.state == state && (!after || changed) {
            print(
                &json!({"outcome":"state_observed","state":state,"pane_id":agent.pane_id,"run":agent.run,"task_success":null}),
            )?;
            return Ok(0);
        }
        if agent.state == "exited" {
            print(&json!({"outcome":"process_exited","pane_id":agent.pane_id,"run":agent.run}))?;
            return Ok(5);
        }
        if Instant::now() >= deadline {
            print(&json!({"outcome":"timeout","pane_id":agent.pane_id,"run":agent.run}))?;
            return Ok(6);
        }
        tokio::time::sleep(
            Duration::from_millis(200).min(deadline.saturating_duration_since(Instant::now())),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalog_verb_reaches_a_dispatcher_before_unknown_command_rejection() {
        for verb in commands::VERBS {
            let args = vec![
                "--socket".to_owned(),
                "/dev/null".to_owned(),
                verb.name.to_owned(),
                "__deliberately_invalid__".to_owned(),
            ];
            let error = run(&args).expect_err(verb.name);
            assert!(
                !error.starts_with("usage: unknown agent command"),
                "{} was rejected before its dispatcher: {error}",
                verb.name
            );
        }
    }
}
