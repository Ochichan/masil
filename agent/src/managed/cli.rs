use super::{Agent, Manager, validate_args};
use crate::providers;
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const HELP: &str = "Native agents (no observer configuration needed):
  masil-agent agent [--socket MASIL_SOCKET] [--client CLIENT] providers
  masil-agent agent endpoints list|add|remove|enable|disable ...
  masil-agent agent --endpoint ID COMMAND ...
  masil-agent agent [--socket MASIL_SOCKET] list --all
  masil-agent agent [--socket MASIL_SOCKET] list|get TARGET|explain TARGET
  masil-agent agent [--socket MASIL_SOCKET] find QUERY [--limit N]
  masil-agent agent [--socket MASIL_SOCKET] capabilities TARGET
  masil-agent agent capabilities --provider PROVIDER
  masil-agent agent [--socket MASIL_SOCKET] read TARGET [--history]
  masil-agent agent [--socket MASIL_SOCKET] start NAME PROVIDER --cwd DIR [--split %N] [--session ID] [--boot BOOT --operation ID] [-- ARGS...]
  masil-agent agent [--socket MASIL_SOCKET] attach TARGET NAME
  masil-agent agent [--socket MASIL_SOCKET] rename TARGET NAME
  masil-agent agent [--socket MASIL_SOCKET] focus TARGET
  masil-agent agent [--socket MASIL_SOCKET] send-keys TARGET KEY...
  masil-agent agent [--socket MASIL_SOCKET] draft TARGET TEXT
  masil-agent agent [--socket MASIL_SOCKET] prompt TARGET TEXT [--run RUN --operation N]
  masil-agent agent [--socket MASIL_SOCKET] prompt-receipt TARGET [--run RUN] [--operation N]
  masil-agent agent [--socket MASIL_SOCKET] interrupt|close TARGET [--run RUN --operation ID]
  masil-agent agent [--socket MASIL_SOCKET] wait TARGET --state idle|working|blocked|exited [--timeout SECONDS] [--after-change]
  masil-agent agent [--socket MASIL_SOCKET] resume TARGET --name NAME [--split %N] [--boot BOOT --operation ID]
  masil-agent agent [--socket MASIL_SOCKET] operations [list] [--all] [--limit N] | status | reconcile | adopt
  masil-agent agent [--socket MASIL_SOCKET] operation KEY | operation resolve KEY --as delivered|not-delivered
  masil-agent agent [--socket MASIL_SOCKET] ack TARGET --run RUN --revision REVISION
  masil-agent agent [--socket MASIL_SOCKET] save FILE
  masil-agent agent [--socket MASIL_SOCKET] restore FILE [--allow-fresh]
  masil-agent agent [--socket MASIL_SOCKET] view get|clear|set [--provider ID] [--state STATE] [--workspace NAME] [--sort priority|name|provider|workspace]
  masil-agent agent [--socket MASIL_SOCKET] integration status [PROVIDER]
  masil-agent agent [--socket MASIL_SOCKET] integration export PROVIDER [--directory ABSOLUTE_PATH]
  masil-agent agent [--socket MASIL_SOCKET] reload
  masil-agent agent [--socket MASIL_SOCKET] report --pane %N --run RUN --sequence N --state STATE [--session ID]
  masil-agent agent [--socket MASIL_SOCKET] ui|sidebar [--lang en|ko] [--theme dark|light|terminal]

Socket defaults to the current TMUX server. Start creates a new window or split.
Draft prepares a tmux buffer. Prompt checks idle/foreground identity, pastes, then sends Enter.
Prompt receipts report delivery, not provider acceptance. Reuse --run RUN --operation N for retries.
Start, prompt, interrupt and close are recorded in a durable operation store before any effect;
a retry with the same key returns the recorded result and an unknown outcome is never resent.
Raw send-keys is explicit keyboard delivery, never provider acceptance.
Wait reports an observed state, never task success. A session binding is verified only
when a provider callback carrying this run's identity reported it; requested resume refs stay unverified.";

pub(crate) fn run(args: &[String]) -> Result<i32, String> {
    if args.is_empty() || args == ["--help"] || args == ["help"] {
        println!("{HELP}");
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
    let command = args.get(index).ok_or("missing agent command")?;
    let rest = &args[index + 1..];
    if ["help", "--help", "-h"].contains(&command.as_str()) && rest.is_empty() {
        println!("{HELP}");
        return Ok(0);
    }
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
            return Err("--endpoint cannot be combined with --socket or --client".into());
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
        .ok_or("specify --socket PATH or run this command inside masil")?;
    if command == "ui" || command == "sidebar" {
        let socket = PathBuf::from(&socket)
            .canonicalize()
            .map_err(|e| format!("native socket: {e}"))?;
        let socket = socket.to_str().ok_or("native socket path is not UTF-8")?;
        let mut args = rest.to_vec();
        if let Some(client) = client {
            args.extend(["--client".into(), client]);
        }
        return crate::ui::run_managed(socket, &args, command == "sidebar");
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
        .ok_or("managed launcher requires an executable")?;
    let pane = std::env::var("TMUX_PANE").map_err(|_| "managed launcher needs TMUX_PANE")?;
    crate::pane_id(&pane)?;
    let run =
        std::env::var("MASIL_AGENT_RUN").map_err(|_| "managed launcher needs a run identity")?;
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
                        .await?;
                    let encoded = String::from_utf8(output.stdout)
                        .map_err(|_| "invalid launch registration")?;
                    if let Some(meta) = super::decode::<super::Metadata>(encoded.trim()) {
                        if meta.run != run
                            || meta.argv != args
                            || meta.foreground_group != std::process::id() as i32
                        {
                            return Err("managed launch registration does not match this process"
                                .to_string());
                        }
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .map_err(|_| "managed launch registration timed out".to_string())?
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
        _ => Err(format!("--operation ID and {flag} must be given together")),
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
            _ => return Err("accepts --run RUN --operation ID".into()),
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
        .ok_or_else(|| format!("missing value for {}", args[index]))
}

fn print<T: Serialize + ?Sized>(value: &T) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?
    );
    Ok(())
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
        "integration" => print(&super::integration::run(&manager, args).await?)?,
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
                    other => return Err(format!("unknown or repeated start option: {other}")),
                }
                i += 2;
            }
            print(
                &manager
                    .start_with_operation(
                        &args[0],
                        &args[1],
                        &cwd.unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?),
                        &extra,
                        session,
                        split,
                        client_key(boot, operation, "--boot BOOT")?,
                    )
                    .await?,
            )?;
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
                    format!("{command} target run changed; this operation cannot be replayed"),
                    command,
                    key,
                ));
            }
            print(&if command == "interrupt" {
                manager.interrupt(&agent, key).await?
            } else {
                manager.close_with_operation(&agent, key).await?
            })?;
        }
        "find" => print(&manager.find(args).await?)?,
        "operations" => print(&manager.operations_command(false, args).await?)?,
        "operation" if !args.is_empty() => print(&manager.operations_command(true, args).await?)?,
        "ack" if args.len() == 5 && args[1] == "--run" && args[3] == "--revision" => {
            let agent = manager.get(&args[0]).await?;
            if agent.run != args[2] || agent.revision != args[4] {
                return Err("attention changed since it was displayed".into());
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
                                .map_err(|_| "invalid prompt operation")?,
                        );
                    }
                    _ => return Err("prompt accepts --run RUN --operation N".into()),
                }
                i += 2;
            }
            if operation.is_some() && run.is_none() {
                return Err(
                    "prompt operation requires --run from the original agent observation".into(),
                );
            }
            if run.is_some_and(|run| run != agent.run) {
                return Err("prompt target run changed; this operation cannot be replayed".into());
            }
            print(
                &manager
                    .prompt_with_operation(&agent, &args[1], operation)
                    .await?,
            )?;
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
                                .map_err(|_| "invalid prompt operation")?,
                        );
                    }
                    _ => return Err("prompt-receipt accepts --run RUN --operation N".into()),
                }
                i += 2;
            }
            if operation.is_some() && run.is_none() {
                return Err(
                    "prompt operation requires --run from the original agent observation".into(),
                );
            }
            if run.is_some_and(|run| run != agent.run) {
                return Err("prompt target run changed; its receipt is unavailable".into());
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
                            "resume accepts --name NAME, --split %N and --boot BOOT --operation ID"
                                .into(),
                        );
                    }
                }
                i += 2;
            }
            let mut outcome = manager
                .start_with_operation(
                    name.ok_or("resume requires a new --name")?,
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
            print(&outcome)?;
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
                                .map_err(|_| "invalid sequence")?,
                        )
                    }
                    "--state" if state.is_none() => state = Some(value(args, i)?),
                    "--session" if session.is_none() => session = Some(value(args, i)?),
                    _ => return Err("invalid report options".into()),
                }
                i += 2;
            }
            let result = manager
                .report(
                    pane.ok_or("report requires --pane")?,
                    run.ok_or("report requires --run")?,
                    sequence.ok_or("report requires --sequence")?,
                    state.ok_or("report requires --state")?,
                    session,
                )
                .await?;
            print(
                &json!({"stage":"report_recorded","binding":result.binding.map(|b| b.label()),"state_applied":result.state_applied}),
            )?;
        }
        "reload" if args.is_empty() => {
            manager.reload()?;
            print(
                &json!({"stage":"manifests_validated","note":"new commands and views load current overrides"}),
            )?;
        }
        _ => {
            return Err(format!(
                "invalid agent command or arguments; see masil-agent agent --help\n{HELP}"
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
                    .map_err(|_| "invalid timeout")?
            }
            "--after-change" if !after => {
                after = true;
                i += 1;
                continue;
            }
            _ => return Err("invalid wait options".into()),
        }
        i += 2;
    }
    if !(0.05..=300.0).contains(&timeout) {
        return Err("timeout must be 0.05–300 seconds".into());
    }
    let state = state.ok_or("wait requires --state")?;
    validate_args(&[state.into()])?;
    if !["idle", "working", "blocked", "exited", "unknown"].contains(&state) {
        return Err("unsupported wait state".into());
    }
    let initial = manager.get(&args[0]).await?;
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    let mut changed = false;
    loop {
        let agent = match manager.get(&initial.pane_id).await {
            Ok(agent) => agent,
            Err(error) => {
                print(
                    &json!({"outcome":if error=="agent target not found"{"target_removed"}else{"observation_lost"},"reason":error,"pane_id":initial.pane_id,"run":initial.run}),
                )?;
                return Ok(3);
            }
        };
        if agent.boot != initial.boot
            || agent.generation != initial.generation
            || agent.run != initial.run
        {
            print(&json!({"outcome":"run_changed","pane_id":initial.pane_id,"run":initial.run}))?;
            return Ok(3);
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
            return Ok(3);
        }
        if Instant::now() >= deadline {
            print(&json!({"outcome":"timeout","pane_id":agent.pane_id,"run":agent.run}))?;
            return Ok(4);
        }
        tokio::time::sleep(
            Duration::from_millis(200).min(deadline.saturating_duration_since(Instant::now())),
        )
        .await;
    }
}
