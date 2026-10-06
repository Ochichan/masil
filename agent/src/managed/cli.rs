use super::{Agent, Manager, commands, failure, validate_args};
use crate::{bridge, providers};
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
    // Links are this server's (P8b); the rest edits endpoints.json.
    let link_command = matches!(
        rest.first().map(String::as_str),
        Some("connect" | "disconnect" | "status")
    );
    if (command == "endpoints" || command == "endpoint") && !link_command {
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
        .or_else(|| std::env::var("MASIL_AGENT_SOCKET").ok());
    // Its own signals: SIGINT stops a dictation rather than dropping it.
    // Stopping and looking need no server.
    if command == "dictate" {
        return super::dictation::run(socket.map(PathBuf::from), client, rest);
    }
    let socket = socket.ok_or("usage: specify --socket PATH or run this command inside masil")?;
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
    // Before a Manager: the stream says when no server runs, and waits.
    if command == "rpc" && rest == ["--stream", "--no-start"] {
        return super::stream::serve(&socket);
    }
    // A file from another server needs no server here.
    if command == "receive" && rest.is_empty() {
        return super::transfer::receive();
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
    // `agent start --answers`: the listening options and password are
    // decided here, in the pane, after registration.
    let (answers, args) = match args.split_first() {
        Some((flag, rest)) if flag == super::answer::EXEC_FLAG => (true, rest),
        _ => (false, args),
    };
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
    let mut argv = args.to_vec();
    // The directory the start checked; None from a launch without it.
    let checked = match std::env::var(super::LAUNCH_CWD) {
        Ok(value) => Some(enter_launch_directory(&value)),
        Err(_) => None,
    };
    let verdict = match &checked {
        Some(Err(reason)) => format!("{run} cwd_rejected {reason}"),
        _ => format!("{run} ok"),
    };
    let environment = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(async {
            // A rejection keeps the pane until the start has read why.
            if let Some(Err(reason)) = &checked {
                native
                    .tmux(
                        [
                            "set-option", "-p", "-t", &pane, "remain-on-exit", "on", ";",
                            "set-option", "-p", "-t", &pane, super::LAUNCH_VERDICT, &verdict,
                        ]
                        .iter()
                        .map(std::ffi::OsString::from),
                        None,
                    )
                    .await
                    .map_err(super::server_unreachable)?;
                return Err(format!("cwd_rejected: {reason}; the agent was not started"));
            }
            let mut first = true;
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    // The verdict goes with the first read of the registration.
                    let mut read: Vec<&str> = Vec::new();
                    if first && checked.is_some() {
                        read.extend(["set-option", "-p", "-t", &pane, super::LAUNCH_VERDICT, &verdict, ";"]);
                    }
                    first = false;
                    read.extend(["show-options", "-pqv", "-t", &pane, super::META]);
                    let output = native
                        .tmux(read.iter().map(std::ffi::OsString::from), None)
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
            .map_err(|_| "wait_timeout: managed launch registration timed out".to_string())??;
            if !answers {
                return Ok(Vec::new());
            }
            Ok::<_, String>(
                super::answer::prepare_exec(&native, std::path::Path::new(socket), &run, &mut argv)
                    .await,
            )
        })?;
    let mut command = std::process::Command::new(program);
    command
        .args(&argv[1..])
        .envs(environment)
        .env_remove(super::LAUNCH_CWD);
    if let Some(Ok(path)) = &checked {
        command.env("PWD", path);
    }
    let error = command.exec();
    Err(format!("provider could not start: {error}"))
}

/// Opens the directory the start checked and moves into it, if it is still
/// that directory (`<dev>:<ino>:<path>`). tmux starts a pane in the home
/// directory when its directory is gone; the provider never runs there.
fn enter_launch_directory(value: &str) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let mut parts = value.splitn(3, ':');
    let (Some(dev), Some(ino), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
        return Err("the start's directory record is unreadable".into());
    };
    let (Ok(dev), Ok(ino)) = (dev.parse::<u64>(), ino.parse::<u64>()) else {
        return Err("the start's directory record is unreadable".into());
    };
    // chdir needs only search permission, as tmux's own does; the identity
    // of "." is then the directory this process is in.
    std::env::set_current_dir(path)
        .map_err(|error| format!("{path} cannot be entered: {error}"))?;
    let metadata = std::fs::metadata(".").map_err(|error| format!("{path}: {error}"))?;
    if (metadata.dev(), metadata.ino()) != (dev, ino) {
        return Err(format!(
            "{path} is not the directory the start checked (replaced or recreated)"
        ));
    }
    Ok(path.to_owned())
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

/// Starts an agent as `agent start` does, here or for an endpoint's RPC:
/// verifies an `--answers` channel and starts a coordinator that should be
/// running and is not, without waiting for it (D1).
#[allow(clippy::too_many_arguments)]
pub(super) async fn launch(
    manager: &Manager,
    name: &str,
    provider: &str,
    cwd: &std::path::Path,
    extra: &[String],
    session: Option<&str>,
    split: Option<&str>,
    key: Option<super::durable::ClientKey<'_>>,
    answers: bool,
) -> Result<Value, String> {
    let mut outcome = manager
        .start_with_operation(name, provider, cwd, extra, session, split, key, answers)
        .await?;
    // The channel is only used once the server refuses requests without its
    // password; one that does not is stopped.
    if answers && let Some(pane) = outcome["pane_id"].as_str() {
        let agent = manager.get(pane).await?;
        let channel = manager.verify_answers(&agent).await?;
        // Without the record the coordinator does not observe the run;
        // answering still works.
        let _ = manager.record_answer_channel(pane, channel).await;
        outcome["answer_channel"] = json!(channel);
    }
    let socket = manager.native.socket.clone();
    let problem = tokio::task::spawn_blocking(move || {
        let enabled = super::state_base()
            .and_then(|base| super::coordinator_features(&base, &socket))
            .is_ok_and(|features| !features.is_empty());
        if !enabled {
            return None;
        }
        crate::coordinator::spawn_detached(&socket);
        // A running one looks at the new pane now.
        crate::coordinator::poke(&socket);
        crate::coordinator::listen_problem(&socket)
    })
    .await
    .ok()
    .flatten();
    // An `--answers` run is only observed by a coordinator.
    if let Some(problem) = problem.filter(|_| answers) {
        outcome["warning"] = json!(format!("coordinator_unavailable: {problem}"));
    }
    Ok(outcome)
}

/// The reply of `answer TARGET REQUEST ...`, from the words after REQUEST.
pub(super) fn answer_reply(args: &[String]) -> Result<super::answer::Reply, String> {
    let mut choice = None;
    let mut message = None;
    let mut answers = Vec::new();
    let mut reject = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--choice" if choice.is_none() => choice = Some(value(args, i)?),
            "--message" if message.is_none() => message = Some(value(args, i)?.to_owned()),
            "--answer" => answers.push(value(args, i)?.to_owned()),
            "--reject" if !reject => {
                reject = true;
                i += 1;
                continue;
            }
            other => {
                return Err(format!("usage: unknown or repeated answer option: {other}"));
            }
        }
        i += 2;
    }
    Ok(match (choice, answers.is_empty(), reject) {
        (Some("once"), true, false) => super::answer::Reply::Once,
        (Some("always"), true, false) if message.is_none() => super::answer::Reply::Always,
        (Some("reject"), true, false) => super::answer::Reply::Reject { message },
        (None, false, false) if message.is_none() => super::answer::Reply::Answers(answers),
        (None, true, true) if message.is_none() => super::answer::Reply::RejectQuestion,
        (Some(other), _, _) if !["once", "always", "reject"].contains(&other) => {
            return Err("invalid_argument: --choice is once, always or reject".into());
        }
        _ => {
            return Err("usage: answer TARGET REQUEST --choice once|always|reject [--message TEXT] | --answer TEXT... | --reject".into());
        }
    })
}

fn queue_usage() -> String {
    "usage: queue TARGET [show ID | add TEXT [--attach PATH]... | add --from ID | edit ID TEXT | attach ID PATH | detach ID N | move ID POSITION | remove ID | send [ID]] [--revision N] | queue --held [remove ID]".to_owned()
}

/// `--revision N` may come anywhere after the target.
fn queue_revision(args: &[String]) -> Result<(Option<i64>, Vec<&str>), String> {
    let mut revision = None;
    let mut words = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--revision" {
            let value = value(args, index)?;
            revision = Some(value.parse::<i64>().map_err(|_| queue_usage())?);
            index += 2;
        } else {
            words.push(args[index].as_str());
            index += 1;
        }
    }
    Ok((revision, words))
}

/// `agent queue ...` (queue.rs).
async fn queue_command(manager: &Manager, args: &[String]) -> Result<i32, String> {
    let (revision, words) = queue_revision(args)?;
    let number = |text: &str| text.parse::<i64>().map_err(|_| queue_usage());
    if words.first() == Some(&"--held") {
        return match words[1..] {
            [] => print(&manager.queue_held().await?).map(|()| 0),
            ["remove", id] => {
                print(&manager.queue_remove(number(id)?, revision).await?).map(|()| 0)
            }
            _ => Err(queue_usage()),
        };
    }
    // The target is the first word that is not `--revision N`.
    let Some(at) = (0..args.len())
        .find(|&at| args[at] != "--revision" && (at == 0 || args[at - 1] != "--revision"))
    else {
        return Err(queue_usage());
    };
    let agent = manager.get(&args[at]).await?;
    let here = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut rest = args.to_vec();
    rest.remove(at);
    let (value, record) = queue_words(manager, &agent, &rest, Some(&here)).await?;
    if record {
        return print_record(manager, &value, false).await;
    }
    print(&value).map(|()| 0)
}

/// One queue command for `agent`, the words after the target. Returns the
/// value and whether it is a prompt record (its stage sets the exit code).
/// Without `base` (an endpoint's RPC), attachment paths must be absolute.
pub(super) async fn queue_words(
    manager: &Manager,
    agent: &Agent,
    args: &[String],
    base: Option<&std::path::Path>,
) -> Result<(Value, bool), String> {
    let usage = queue_usage;
    let (revision, words) = queue_revision(args)?;
    let number = |text: &str| text.parse::<i64>().map_err(|_| usage());
    let absolute = |path: &str| {
        if base.is_none() && !std::path::Path::new(path).is_absolute() {
            Err(format!(
                "invalid_argument: {path} must be an absolute path on the endpoint host"
            ))
        } else {
            Ok(())
        }
    };
    let here = base.unwrap_or(std::path::Path::new("/"));
    let agent = agent.clone();
    let value = match words[..] {
        [] => manager.queue_view(&agent).await?,
        ["show", id] => manager.queue_show(number(id)?).await?,
        ["add", "--from", id] => manager.queue_add_from(&agent, number(id)?).await?,
        ["add", ref rest @ ..] if !rest.is_empty() => {
            let mut body = None;
            let mut paths = Vec::new();
            let mut index = 0;
            while index < rest.len() {
                match rest[index] {
                    "--attach" => {
                        let path = rest.get(index + 1).ok_or_else(usage)?;
                        absolute(path)?;
                        paths.push(path.to_string());
                        index += 2;
                    }
                    text if body.is_none() => {
                        body = Some(text);
                        index += 1;
                    }
                    _ => return Err(usage()),
                }
            }
            manager
                .queue_add(&agent, body.ok_or_else(usage)?, &paths, here)
                .await?
        }
        ["edit", id, text] => {
            manager
                .queue_edit(&agent, number(id)?, text, revision)
                .await?
        }
        ["attach", id, path] => {
            absolute(path)?;
            manager
                .queue_attach(&agent, number(id)?, path, revision, here)
                .await?
        }
        ["detach", id, at] => {
            let at = at.parse::<usize>().map_err(|_| usage())?;
            manager
                .queue_detach(&agent, number(id)?, at, revision)
                .await?
        }
        ["move", id, position] => {
            manager
                .queue_move(&agent, number(id)?, number(position)?, revision)
                .await?
        }
        ["remove", id] => manager.queue_remove(number(id)?, revision).await?,
        ["send"] => return Ok((manager.queue_send(&agent, None, revision).await?, true)),
        ["send", id] => {
            let outcome = manager
                .queue_send(&agent, Some(number(id)?), revision)
                .await?;
            return Ok((outcome, true));
        }
        _ => return Err(usage()),
    };
    Ok((value, false))
}

/// `restore FILE [--allow-fresh] [--again] [--wait SECONDS]`.
fn restore_options(args: &[String]) -> Result<super::store::RestoreOptions, String> {
    // An extension API call is cut off after a minute: no wait unless asked,
    // and never one that could outlast the call.
    let api = std::env::var_os("MASIL_API_CALL").is_some();
    let limit = if api { 20.0 } else { 120.0 };
    let mut options = super::store::RestoreOptions {
        allow_fresh: false,
        again: false,
        wait: Duration::from_secs(if api { 0 } else { 30 }),
    };
    let mut seen = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let option = args[i].as_str();
        if seen.contains(&option) {
            return Err("usage: repeated restore option".into());
        }
        seen.push(option);
        match option {
            "--allow-fresh" => options.allow_fresh = true,
            "--again" => options.again = true,
            "--wait" => {
                let seconds = value(args, i)?
                    .parse::<f64>()
                    .ok()
                    .filter(|seconds| (0.0..=limit).contains(seconds))
                    .ok_or(format!("invalid_argument: --wait takes 0–{limit} seconds"))?;
                options.wait = Duration::from_secs_f64(seconds);
                i += 1;
            }
            _ => {
                return Err(
                    "usage: restore FILE [--allow-fresh] [--again] [--wait SECONDS]".into(),
                );
            }
        }
        i += 1;
    }
    Ok(options)
}

fn value(args: &[String], index: usize) -> Result<&str, String> {
    args.get(index + 1)
        .map(String::as_str)
        .ok_or_else(|| format!("usage: missing value for {}", args[index]))
}

/// `changes TARGET [--file PATH] [--handoff REVIEWER]`: the agent's working
/// tree changes, one file's diff, or a review request queued for another
/// agent (never sent by itself).
async fn changes_command(manager: &Manager, args: &[String]) -> Result<i32, String> {
    let agent = manager.get(&args[0]).await?;
    print(&changes_words(manager, &agent, &args[1..], false).await?)?;
    Ok(0)
}

/// `changes` for `agent`, the words after the target. An endpoint's RPC
/// (`remote`) only reads: no `--handoff`.
pub(super) async fn changes_words(
    manager: &Manager,
    agent: &Agent,
    args: &[String],
    remote: bool,
) -> Result<Value, String> {
    let mut file = None;
    let mut handoff = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--file" if file.is_none() => file = Some(value(args, i)?.to_owned()),
            "--handoff" if handoff.is_none() => handoff = Some(value(args, i)?.to_owned()),
            other => {
                return Err(format!(
                    "usage: unknown or repeated changes option: {other}"
                ));
            }
        }
        i += 2;
    }
    if file.is_some() && handoff.is_some() {
        return Err("usage: --file and --handoff do not go together".into());
    }
    if remote && handoff.is_some() {
        return Err("remote_unsupported: --handoff is not offered for an endpoint's agent".into());
    }
    let op = match (file, handoff) {
        (Some(path), _) => super::changes::ChangesOp::Diff { path },
        (None, Some(reviewer)) => super::changes::ChangesOp::Handoff { reviewer },
        (None, None) => super::changes::ChangesOp::List,
    };
    manager.changes_op(agent, op).await
}

/// `restore ID PATH... [--confirm TOKEN]`.
fn restore_op(args: &[String]) -> Result<super::changes::ChangesOp, String> {
    let usage = "usage: checkpoint TARGET restore ID PATH... [--confirm TOKEN]";
    let (id, rest) = args.split_first().ok_or(usage)?;
    let id = id
        .parse::<u64>()
        .map_err(|_| format!("invalid_argument: {id} is not a checkpoint number"))?;
    let mut paths = Vec::new();
    let mut token = None;
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--confirm" && token.is_none() {
            token = Some(value(rest, i)?.to_owned());
            i += 2;
        } else {
            paths.push(rest[i].clone());
            i += 1;
        }
    }
    if paths.is_empty() {
        return Err(usage.into());
    }
    Ok(super::changes::ChangesOp::Restore {
        id,
        paths,
        token,
        run: None,
    })
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
        "restore" if !args.is_empty() => {
            let options = restore_options(&args[1..])?;
            print(
                &manager
                    .restore(std::path::Path::new(&args[0]), options)
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
                json!({
                    "pane_id":agent.pane_id,
                    "provider":agent.provider,
                    "state":agent.state,
                    "evidence":agent.evidence,
                    "action":manager.action_path_report().await?,
                })
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
            let mut answers = false;
            let mut worktree = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--worktree" if worktree.is_none() => worktree = Some(value(args, i)?),
                    "--answers" if !answers => {
                        answers = true;
                        i += 1;
                        continue;
                    }
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
            let here = std::env::current_dir().map_err(|e| e.to_string())?;
            // With a worktree, the directory is that worktree, or --cwd
            // inside it; the start then leases it like any start there.
            let cwd = match worktree {
                Some(spec) => crate::worktree::start_dir(spec, &here, cwd.as_deref())?,
                None => cwd.unwrap_or(here),
            };
            let outcome = launch(
                &manager,
                &args[0],
                &args[1],
                &cwd,
                &extra,
                session,
                split,
                client_key(boot, operation, "--boot BOOT")?,
                answers,
            )
            .await?;
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
        "interrupt" if !args.is_empty() => {
            let mut asked = super::durable::InterruptRequest::default();
            let mut confirm = 5;
            let mut rest = Vec::new();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--any-state" => {
                        asked.any_state = true;
                        i += 1;
                    }
                    "--key" => {
                        let key = value(args, i)?;
                        if !matches!(key, "Escape" | "C-c" | "C-d" | "C-g" | "q") {
                            return Err("usage: --key is Escape, C-c, C-d, C-g or q".into());
                        }
                        let keys = asked.keys.get_or_insert_with(Vec::new);
                        if keys.len() == 4 {
                            return Err("usage: at most 4 --key".into());
                        }
                        keys.push(key.to_owned());
                        i += 2;
                    }
                    "--confirm-seconds" => {
                        confirm = value(args, i)?
                            .parse::<u64>()
                            .ok()
                            .filter(|seconds| *seconds <= 60)
                            .ok_or("usage: --confirm-seconds is 0-60")?;
                        i += 2;
                    }
                    // A flag's value is never read as a flag.
                    "--run" | "--operation" => {
                        rest.push(args[i].clone());
                        rest.push(value(args, i)?.to_owned());
                        i += 2;
                    }
                    _ => {
                        rest.push(args[i].clone());
                        i += 1;
                    }
                }
            }
            // What a person may override, neither an agent nor the API may.
            if (asked.any_state || asked.keys.is_some())
                && (std::env::var_os("MASIL_API_CALL").is_some()
                    || !matches!(super::answer::agent_ancestor(), Ok(false)))
            {
                return Err("interrupt_refused: --any-state and --key are a person's; this command came through the extension API, runs inside an agent, or its ancestry could not be read".into());
            }
            let (run, operation) = run_operation(&rest)?;
            let key = client_key(run, operation, "--run RUN")?;
            let agent = match manager.get(&args[0]).await {
                Ok(agent) => agent,
                Err(error) => return Err(keyed_error(error, command, key)),
            };
            if key.is_some_and(|key| key.pin != agent.run) {
                return Err(keyed_error(
                    "identity_mismatch: interrupt target run changed; this operation cannot be replayed".into(),
                    command,
                    key,
                ));
            }
            let asked_at = crate::observation::now_ms();
            let outcome = manager.interrupt_with(&agent, key, &asked).await?;
            let outcome = manager
                .confirm_interrupt(&agent, outcome, confirm, asked_at)
                .await;
            return print_record(&manager, &outcome, false).await;
        }
        "close" if !args.is_empty() => {
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
            let outcome = {
                match manager.close_with_operation(&agent, key).await {
                    // A report written meanwhile (a hook, the coordinator)
                    // changes what the guard compares. Once more, for the
                    // same run only, when masil numbers the operation.
                    Err(error) if key.is_none() && error.starts_with("rejected_before_effect") => {
                        let again = manager.get(&args[0]).await?;
                        if again.run != agent.run
                            || again.generation != agent.generation
                            || again.state != agent.state
                        {
                            return Err(error);
                        }
                        manager.close_with_operation(&again, None).await?
                    }
                    result => result?,
                }
            };
            return print_record(&manager, &outcome, false).await;
        }
        "find" => print(&manager.find(args).await?)?,
        "endpoints" | "endpoint" => print(&super::links::command(&manager, args).await?)?,
        "worktree" if !args.is_empty() => {
            let (verb, rest) = (args[0].clone(), args[1..].to_vec());
            let value = tokio::task::spawn_blocking(move || match verb.as_str() {
                "list" => crate::worktree::listing(&rest),
                "jobs" => crate::worktree::job_listing(&rest),
                _ => {
                    Err("usage: agent worktree list | jobs; see masil-agent worktree --help".into())
                }
            })
            .await
            .map_err(|error| error.to_string())??;
            print(&value)?;
        }
        "operations" => print(&manager.operations_command(false, args).await?)?,
        "inbox" => print(&super::inbox::command(&manager, args).await?)?,
        "notify" => print(&super::notifications::command(&manager, args).await?)?,
        "schedule" => print(&super::schedules::command(&manager, args).await?)?,
        "requests" if args.len() == 1 => {
            let agent = manager.get(&args[0]).await?;
            print(&manager.requests(&agent).await?)?;
        }
        "queue" if !args.is_empty() => {
            return queue_command(&manager, args).await;
        }
        "changes" if !args.is_empty() => {
            return changes_command(&manager, args).await;
        }
        "checkpoint" if args.len() >= 2 => {
            let agent = manager.get(&args[0]).await?;
            let op = match args[1].as_str() {
                "restore" => restore_op(&args[2..])?,
                _ => match crate::checkpoint::Command::parse(&args[1..])? {
                    crate::checkpoint::Command::List => super::changes::ChangesOp::Checkpoints,
                    crate::checkpoint::Command::Make { reason } => {
                        super::changes::ChangesOp::Make { reason }
                    }
                    crate::checkpoint::Command::Show { id, path } => {
                        super::changes::ChangesOp::Show { id, path }
                    }
                },
            };
            let value = manager.changes_op(&agent, op).await?;
            print(&value)?;
            if value["stage"] == "partly_restored" {
                return Err(format!(
                    "checkpoint_partial: some files were not restored; checkpoint {} holds them as they were before",
                    value["before"]
                ));
            }
        }
        "answer" if args.len() >= 3 => {
            let reply = answer_reply(&args[2..])?;
            let agent = manager.get(&args[0]).await?;
            print(&manager.answer(&agent, &args[1], reply).await?)?;
        }
        "operation" if !args.is_empty() => {
            let record = manager.operations_command(true, args).await?;
            return print_record(&manager, &record, args.len() == 1).await;
        }
        "ack" if args.len() == 5 && args[1] == "--run" && args[3] == "--revision" => {
            // Events recorded after this read were not shown to the caller.
            let read_at = crate::observation::now_ms();
            let agent = manager.get(&args[0]).await?;
            if agent.run != args[2] || agent.revision != args[4] {
                return Err("identity_mismatch: attention changed since it was displayed".into());
            }
            manager
                .acknowledge(&agent, super::Through::Before(read_at))
                .await?;
            print(&json!({"stage":"seen","revision":agent.revision}))?;
        }
        "send-keys" if args.len() >= 2 => {
            let agent = manager.get(&args[0]).await?;
            print(
                &manager
                    .keys(&agent, &args[1..], super::TrackedRetry::SameRun)
                    .await?,
            )?;
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
                    false,
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

/// What `wait` waits for (EV-07).
enum WaitFor<'a> {
    /// `TARGET --state S [--after-change]`
    State {
        target: &'a str,
        state: &'a str,
        after: bool,
    },
    /// `--operation KEY --stage S`
    Stage { key: &'a str, stage: &'a str },
    /// `--run RUN --ended`
    Ended { run: &'a str },
    /// `--pane %N|--window @N|--session NAME --closed`
    Closed { kind: &'a str, target: &'a str },
}

fn wait_options(args: &[String]) -> Result<(WaitFor<'_>, f64), String> {
    let usage = || {
        "usage: wait TARGET --state S | --operation KEY --stage S | --run RUN --ended | --pane %N|--window @N|--session NAME --closed [--timeout SECONDS]".to_owned()
    };
    let target = args.first().filter(|first| !first.starts_with("--"));
    let mut values: Vec<(&str, &str)> = Vec::new();
    let mut flags: Vec<&str> = Vec::new();
    let mut timeout = None;
    let mut i = usize::from(target.is_some());
    while i < args.len() {
        let option = args[i].as_str();
        match option {
            "--after-change" | "--ended" | "--closed" if !flags.contains(&option) => {
                flags.push(option);
                i += 1;
            }
            // The last one counts, as before.
            "--timeout" => {
                timeout = Some(value(args, i)?);
                i += 2;
            }
            "--state" | "--operation" | "--stage" | "--run" | "--pane" | "--window"
            | "--session"
                if !values.iter().any(|(name, _)| *name == option) =>
            {
                let given = value(args, i)?;
                if given.starts_with("--") {
                    return Err(usage());
                }
                values.push((option, given));
                i += 2;
            }
            _ => return Err(usage()),
        }
    }
    let timeout = match timeout {
        Some(text) => text
            .parse::<f64>()
            .map_err(|_| "invalid_argument: invalid timeout")?,
        None => 30.0,
    };
    if !(0.05..=300.0).contains(&timeout) {
        return Err("invalid_argument: timeout must be 0.05–300 seconds".into());
    }
    let named: Vec<&str> = values.iter().map(|(name, _)| *name).collect();
    let get = |name: &str| values.iter().find(|(n, _)| *n == name).map(|(_, v)| *v);
    let wanted = match (target, named.as_slice(), flags.as_slice()) {
        (Some(target), ["--state"], [] | ["--after-change"]) => WaitFor::State {
            target,
            state: get("--state").ok_or_else(usage)?,
            after: !flags.is_empty(),
        },
        (None, ["--operation", "--stage"] | ["--stage", "--operation"], []) => WaitFor::Stage {
            key: get("--operation").ok_or_else(usage)?,
            stage: get("--stage").ok_or_else(usage)?,
        },
        (None, ["--run"], ["--ended"]) => WaitFor::Ended {
            run: get("--run").ok_or_else(usage)?,
        },
        (None, [kind @ ("--pane" | "--window" | "--session")], ["--closed"]) => WaitFor::Closed {
            kind: kind.trim_start_matches("--"),
            target: get(kind).ok_or_else(usage)?,
        },
        (Some(_), [], _) => return Err("usage: wait requires --state".into()),
        _ => return Err(usage()),
    };
    Ok((wanted, timeout))
}

async fn wait(manager: &Manager, args: &[String]) -> Result<i32, String> {
    let (wanted, timeout) = wait_options(args)?;
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    match wanted {
        WaitFor::State {
            target,
            state,
            after,
        } => wait_state(manager, target, state, after, deadline).await,
        WaitFor::Stage { key, stage } => wait_stage(manager, key, stage, deadline).await,
        WaitFor::Ended { run } => wait_ended(manager, run, deadline).await,
        WaitFor::Closed { kind, target } => wait_closed(manager, kind, target, deadline).await,
    }
}

/// The exit status of a wait that ended for `end_reason`: 0 for what it
/// waited for, 6 when it could not tell, 5 otherwise.
fn wait_exit(end_reason: &str, waited_for: &str) -> i32 {
    match end_reason {
        _ if end_reason == waited_for => 0,
        "server_gone" | "observation_lost" | "timeout" => 6,
        _ => 5,
    }
}

/// The server is gone (killed, or its socket removed).
fn server_gone(error: &str) -> bool {
    [
        "no server running on",
        "server exited unexpectedly",
        "error connecting to",
    ]
    .iter()
    .any(|text| error.contains(text))
}

/// A wait that could not read the server: `server_gone` or
/// `observation_lost`, printed with `fields`.
fn wait_unread(error: &str, mut fields: Value) -> Result<i32, String> {
    let end_reason = if server_gone(error) {
        "server_gone"
    } else {
        "observation_lost"
    };
    fields["outcome"] = json!(end_reason);
    fields["end_reason"] = json!(end_reason);
    fields["reason"] = json!(error);
    print(&fields)?;
    Ok(6)
}

async fn pause(deadline: Instant) {
    tokio::time::sleep(
        Duration::from_millis(200).min(deadline.saturating_duration_since(Instant::now())),
    )
    .await;
}

async fn wait_state(
    manager: &Manager,
    target: &str,
    state: &str,
    after: bool,
    deadline: Instant,
) -> Result<i32, String> {
    validate_args(&[state.into()])?;
    if !["idle", "working", "blocked", "exited", "unknown"].contains(&state) {
        return Err("invalid_argument: unsupported wait state".into());
    }
    let initial = manager.get(target).await?;
    let mut changed = false;
    loop {
        let agent = match manager.get(&initial.pane_id).await {
            Ok(agent) => agent,
            Err(error) => {
                let absent = failure::classify(&error).1 == "target_absent";
                let reason = error.strip_prefix("target_absent: ").unwrap_or(&error);
                let end_reason = if server_gone(&error) {
                    "server_gone"
                } else if !absent {
                    "observation_lost"
                } else {
                    // The pane is still there and runs something else.
                    match manager
                        .command(&[
                            "display-message",
                            "-p",
                            "-t",
                            &initial.pane_id,
                            "#{pane_id}",
                        ])
                        .await
                    {
                        Ok(pane) if pane.trim() == initial.pane_id => "agent_left_foreground",
                        Err(error) if server_gone(&error) => "server_gone",
                        _ => "pane_closed",
                    }
                };
                print(
                    &json!({"outcome":if absent && end_reason != "server_gone" {"target_removed"} else {"observation_lost"},"end_reason":end_reason,"reason":reason,"pane_id":initial.pane_id,"run":initial.run}),
                )?;
                return Ok(if absent && end_reason != "server_gone" {
                    5
                } else {
                    6
                });
            }
        };
        if agent.boot != initial.boot
            || agent.generation != initial.generation
            || agent.run != initial.run
        {
            print(
                &json!({"outcome":"run_changed","end_reason":"run_changed","pane_id":initial.pane_id,"run":initial.run}),
            )?;
            return Ok(wait_exit("run_changed", "state_observed"));
        }
        changed |= agent.state != initial.state;
        if agent.state == state && (!after || changed) {
            print(
                &json!({"outcome":"state_observed","end_reason":"state_observed","state":state,"pane_id":agent.pane_id,"run":agent.run,"task_success":null}),
            )?;
            return Ok(0);
        }
        if agent.state == "exited" {
            print(
                &json!({"outcome":"process_exited","end_reason":"run_ended","pane_id":agent.pane_id,"run":agent.run}),
            )?;
            return Ok(wait_exit("run_ended", "state_observed"));
        }
        if Instant::now() >= deadline {
            print(
                &json!({"outcome":"timeout","end_reason":"timeout","pane_id":agent.pane_id,"run":agent.run}),
            )?;
            return Ok(wait_exit("timeout", "state_observed"));
        }
        pause(deadline).await;
    }
}

/// An operation's stage in order, and whether nothing can follow it. A
/// stage at the same rank as the one waited for (another confirmation of an
/// interrupt) counts. Stages outside the order end the operation without
/// reaching any.
fn stage_rank(stage: &str) -> Option<(u8, bool)> {
    Some(match stage {
        "pending" => (0, false),
        "dispatching" => (1, false),
        "delivered" | "process_started" | "interrupt_key_delivered" => (2, false),
        // The process ended before it was seen running; the pane closed;
        // the interrupt's key ended the provider; a person said it was
        // delivered (whether the provider took it stays unknown).
        "process_exited" | "pane_closed" | "provider_exited" | "user_confirmed_delivered" => {
            (2, true)
        }
        "native_accepted"
        | "provider_stopped"
        | "turn_end_observed"
        | "completed_before_interrupt" => (3, true),
        _ => return None,
    })
}

/// The stages a `wait --stage` may name for an action.
fn action_stages(action: &str) -> &'static [&'static str] {
    match action {
        "start" => &["process_started"],
        "prompt" => &["delivered", "user_confirmed_delivered", "native_accepted"],
        "interrupt" => &[
            "interrupt_key_delivered",
            "provider_stopped",
            "turn_end_observed",
            "completed_before_interrupt",
        ],
        "close" => &["pane_closed"],
        "answer" => &["native_accepted"],
        _ => &[],
    }
}

async fn wait_stage(
    manager: &Manager,
    key: &str,
    stage: &str,
    deadline: Instant,
) -> Result<i32, String> {
    validate_args(&[key.into(), stage.into()])?;
    let store = match manager.operation_store().await {
        Ok(store) => store,
        Err(error) if server_gone(&error) => {
            return wait_unread(&error, json!({"operation_key": key}));
        }
        Err(error) => return Err(error),
    };
    let missing = || {
        "target_absent: operation not found; it never existed or its namespace ended and it was deleted".to_owned()
    };
    let record = store.get(key)?.ok_or_else(missing)?;
    let stages = action_stages(&record.action);
    if !stages.contains(&stage) {
        return Err(format!(
            "invalid_argument: a {} operation's stages to wait for are {}",
            record.action,
            if stages.is_empty() {
                "none".to_owned()
            } else {
                stages.join(", ")
            }
        ));
    }
    let wanted = stage_rank(stage).map_or(u8::MAX, |(rank, _)| rank);
    loop {
        let record = match store.get(key) {
            Ok(Some(record)) => record,
            result => {
                let reason = result.err().unwrap_or_else(missing);
                return wait_unread(&reason, json!({"operation_key": key}));
            }
        };
        let report = |end_reason: &str| json!({"outcome":end_reason,"end_reason":end_reason,"operation_key":key,"action":record.action,"stage":record.state});
        match stage_rank(&record.state) {
            Some((rank, _)) if rank >= wanted => {
                print(&report("stage_reached"))?;
                return Ok(0);
            }
            Some((_, false)) => {}
            // Ended without reaching it: the stored stage's own status, or
            // a failure for a stage that is itself a success.
            ended => {
                print(&report("operation_ended"))?;
                return Ok(match ended {
                    _ if record.state == "user_confirmed_delivered" => 6,
                    Some(_) => 5,
                    None => failure::recorded_exit_code(&record.state, false),
                });
            }
        }
        if Instant::now() >= deadline {
            print(&report("timeout"))?;
            return Ok(wait_exit("timeout", "stage_reached"));
        }
        pause(deadline).await;
    }
}

/// Whether some agent runs `run`. A job stopped with Ctrl-Z leaves `list`
/// but is the same run when brought back, so a run masil did not start is
/// live while the process recorded for it (`EPOCH`) still exists.
async fn running(manager: &Manager, run: &str) -> Result<bool, String> {
    let agents = match manager.collect(None).await {
        Ok(agents) => agents,
        // Another writer won a pane's update twice: no answer this time.
        Err(error) if error.starts_with("identity_mismatch") => return Ok(true),
        Err(error) => return Err(error),
    };
    if agents
        .iter()
        .any(|agent| agent.run == run && agent.process != "exited")
    {
        return Ok(true);
    }
    Ok(manager.inventory().await?.iter().any(|fields| {
        fields.get(4).is_some_and(|dead| dead != "1")
            && fields.get(17).is_some_and(|recorded| {
                super::epoch_records(recorded).any(|(recorded, epoch)| {
                    recorded == run
                        && epoch.split_once(':').is_some_and(|(group, started)| {
                            group
                                .parse::<i32>()
                                .ok()
                                .filter(|group| *group > 1)
                                .is_some_and(|group| {
                                    crate::process::info(group)
                                        .is_some_and(|info| info.started.to_string() == started)
                                })
                        })
                })
            })
    }))
}

async fn wait_ended(manager: &Manager, run: &str, deadline: Instant) -> Result<i32, String> {
    if run.is_empty() || run.len() > 128 || run.chars().any(char::is_control) {
        return Err("invalid_argument: invalid run".into());
    }
    let mut seen = false;
    loop {
        let live = match running(manager, run).await {
            Ok(live) => live,
            Err(error) => return wait_unread(&error, json!({"run": run, "seen": seen})),
        };
        if !live {
            // `seen` false: no agent ran it when the wait began.
            print(&json!({"outcome":"run_ended","end_reason":"run_ended","run":run,"seen":seen}))?;
            return Ok(0);
        }
        seen = true;
        if Instant::now() >= deadline {
            print(&json!({"outcome":"timeout","end_reason":"timeout","run":run,"seen":seen}))?;
            return Ok(wait_exit("timeout", "run_ended"));
        }
        pause(deadline).await;
    }
}

/// `%N`, `@N` or `$N` with the number as the server prints it, so `%00`
/// is `%0`.
fn object_id(sigil: char, text: &str) -> Option<String> {
    let number = text.strip_prefix(sigil)?;
    (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| number.parse::<u32>().ok())
        .flatten()
        .map(|number| format!("{sigil}{number}"))
}

struct ClosedTarget<'a> {
    kind: &'a str,
    field: usize,
    id: &'a str,
    closed: &'a str,
}

async fn wait_closed(
    manager: &Manager,
    kind: &str,
    target: &str,
    deadline: Instant,
) -> Result<i32, String> {
    validate_args(&[target.into()])?;
    let closed = format!("{kind}_closed");
    let (sigil, field, listing, format) = match kind {
        "pane" => ('%', 3, "", ""),
        "window" => (
            '@',
            2,
            "list-windows",
            "#{window_id}\t#{session_name}:#{window_name}\t#{session_name}:#{window_index}",
        ),
        _ => ('$', 1, "list-sessions", "#{session_id}\t#{session_name}"),
    };
    // An ID that is gone was closed; a name is turned into its ID first
    // (exactly, never a prefix), so a rename is not a close.
    let (id, named) = match object_id(sigil, target) {
        Some(id) => (id, false),
        None if kind == "pane" => {
            return Err("invalid_argument: --pane takes a pane ID (%N)".into());
        }
        None => {
            // list-sessions takes no -a.
            let all: &[&str] = if kind == "session" { &[] } else { &["-a"] };
            let args = [&[listing][..], all, &["-F", format][..]].concat();
            let text = match manager.command(&args).await {
                Ok(text) => text,
                Err(error) if server_gone(&error) => {
                    return wait_unread(&error, json!({ kind: target, "seen": false }));
                }
                Err(error) => return Err(error),
            };
            // A window linked into a session twice is still one window.
            let mut found = text
                .lines()
                .filter(|line| line.split('\t').skip(1).any(|name| name == target))
                .filter_map(|line| line.split('\t').next())
                .collect::<Vec<_>>();
            found.sort_unstable();
            found.dedup();
            let mut found = found.into_iter();
            let id = found
                .next()
                .ok_or_else(|| format!("target_absent: {kind} not found"))?
                .to_owned();
            if found.next().is_some() {
                return Err(format!(
                    "invalid_argument: more than one {kind} is named {target}; use its ID"
                ));
            }
            (id, true)
        }
    };
    // A missing/unavailable bridge is not an observation loss before a
    // stream exists: preserve the old 200 ms native polling path in that
    // case. Once a watch ACK arrives, a gap/EOF/identity mismatch cannot be
    // reconciled by silently switching back to a different observation mode.
    let bridge_target = ClosedTarget {
        kind,
        field,
        id: &id,
        closed: &closed,
    };
    if let Ok(Some(endpoint)) = manager.bridge_endpoint().await
        && let Some(result) =
            wait_closed_bridge(manager, bridge_target, named, deadline, endpoint).await?
    {
        return Ok(result);
    }
    wait_closed_polling(manager, kind, field, &id, named, &closed, deadline).await
}

/// Polling fallback kept byte-for-byte equivalent in its native observation
/// behavior to the earlier waiter.
async fn wait_closed_polling(
    manager: &Manager,
    kind: &str,
    field: usize,
    id: &str,
    mut seen: bool,
    closed: &str,
    deadline: Instant,
) -> Result<i32, String> {
    let mut boot: Option<String> = None;
    loop {
        let present = match manager
            .command(&[
                "list-panes",
                "-a",
                "-F",
                "#{masil_core_boot_id}\t#{session_id}\t#{window_id}\t#{pane_id}",
            ])
            .await
        {
            Ok(text) => {
                let current = text
                    .lines()
                    .next()
                    .and_then(|line| line.split('\t').next())
                    .map(str::to_owned);
                // A restarted server reuses IDs: what was waited on is gone.
                let same = boot.get_or_insert_with(|| current.clone().unwrap_or_default())
                    == current.as_deref().unwrap_or_default();
                same && text
                    .lines()
                    .any(|line| line.split('\t').nth(field) == Some(id))
            }
            Err(error) => return wait_unread(&error, json!({ kind: id, "seen": seen })),
        };
        if !present {
            print(&json!({"outcome":closed,"end_reason":closed,kind:id,"seen":seen}))?;
            return Ok(0);
        }
        seen = true;
        if Instant::now() >= deadline {
            return wait_closed_timeout(kind, id, seen, closed);
        }
        pause(deadline).await;
    }
}

/// A one-time native check after a lifecycle watch ACK closes the subscribe
/// race: a removal between opening the stream and checking is either already
/// absent here or is retained in the lifecycle journal after the ACK fence.
async fn bridge_target_present(
    manager: &Manager,
    field: usize,
    id: &str,
    boot: &str,
) -> Result<bool, bridge::Error> {
    let text = manager
        .command(&[
            "list-panes",
            "-a",
            "-F",
            "#{masil_core_boot_id}\t#{session_id}\t#{window_id}\t#{pane_id}",
        ])
        .await
        .map_err(|error| bridge::Error::Lost(format!("checking lifecycle target: {error}")))?;
    if let Some(current) = text.lines().next().and_then(|line| line.split('\t').next())
        && current != boot
    {
        return Err(bridge::Error::BootMismatch);
    }
    Ok(text
        .lines()
        .any(|line| line.split('\t').nth(field) == Some(id)))
}

fn wait_closed_timeout(kind: &str, id: &str, seen: bool, closed: &str) -> Result<i32, String> {
    print(&json!({"outcome":"timeout","end_reason":"timeout",kind:id,"seen":seen}))?;
    Ok(wait_exit("timeout", closed))
}

fn wait_closed_observed(target: &ClosedTarget<'_>, seen: bool) -> Result<i32, String> {
    let mut output = json!({
        "outcome": target.closed,
        "end_reason": target.closed,
        "seen": seen,
    });
    output[target.kind] = json!(target.id);
    print(&output)?;
    Ok(0)
}

fn wait_closed_lost(
    kind: &str,
    id: &str,
    seen: bool,
    error: &bridge::Error,
) -> Result<i32, String> {
    print(&json!({
        "outcome":"observation_lost",
        "end_reason":"observation_lost",
        "reason":format!("bridge: {error}"),
        kind:id,
        "seen":seen,
    }))?;
    Ok(6)
}

/// The one native boot probe after a fenced stream fails. A changed boot is
/// equivalent to a gone server for a wait: IDs belong to the old core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BridgeProbe {
    SameBoot,
    ServerGone,
    BootChanged,
    Unreadable,
}

fn bridge_probe_end_reason(probe: BridgeProbe) -> &'static str {
    match probe {
        BridgeProbe::ServerGone | BridgeProbe::BootChanged => "server_gone",
        BridgeProbe::SameBoot | BridgeProbe::Unreadable => "observation_lost",
    }
}

async fn bridge_boot_probe(manager: &Manager, expected_boot: &str) -> (BridgeProbe, String) {
    match manager
        .command(&["display-message", "-p", "#{masil_core_boot_id}"])
        .await
    {
        Ok(boot) if boot.trim() == expected_boot => (BridgeProbe::SameBoot, String::new()),
        Ok(boot) => (
            BridgeProbe::BootChanged,
            format!("native core boot changed to {}", boot.trim()),
        ),
        Err(error) if server_gone(&error) => (BridgeProbe::ServerGone, error),
        Err(error) => (BridgeProbe::Unreadable, error),
    }
}

fn wait_closed_server_gone(kind: &str, id: &str, seen: bool, reason: &str) -> Result<i32, String> {
    print(&json!({
        "outcome":"server_gone",
        "end_reason":"server_gone",
        "reason":reason,
        kind:id,
        "seen":seen,
    }))?;
    Ok(6)
}

/// A watch has already crossed its hello fence, so it may not silently fall
/// back to polling. Probe once to distinguish a stopped/restarted server from
/// a live server whose stream can no longer be trusted.
async fn wait_closed_stream_lost(
    manager: &Manager,
    target: &ClosedTarget<'_>,
    seen: bool,
    endpoint: &bridge::Endpoint,
    error: &bridge::Error,
) -> Result<i32, String> {
    if error.is_server_exiting() {
        return wait_closed_server_gone(target.kind, target.id, seen, "bridge: server exiting");
    }
    let (probe, reason) = bridge_boot_probe(manager, &endpoint.core_boot_id).await;
    if bridge_probe_end_reason(probe) == "server_gone" {
        let reason = if reason.is_empty() {
            format!("bridge: {error}")
        } else {
            reason
        };
        return wait_closed_server_gone(target.kind, target.id, seen, &reason);
    }
    wait_closed_lost(target.kind, target.id, seen, error)
}

/// Preserve the polling waiter's ordering for a tiny timeout: one native
/// presence check happens before timeout is reported, even when the bridge
/// setup consumed the deadline.
async fn wait_closed_timeout_after_presence(
    manager: &Manager,
    target: &ClosedTarget<'_>,
    seen: bool,
    endpoint: &bridge::Endpoint,
) -> Result<i32, String> {
    match bridge_target_present(manager, target.field, target.id, &endpoint.core_boot_id).await {
        Ok(false) => wait_closed_observed(target, seen),
        Ok(true) => wait_closed_timeout(target.kind, target.id, true, target.closed),
        Err(bridge::Error::BootMismatch) => {
            wait_closed_server_gone(target.kind, target.id, seen, "native core boot changed")
        }
        Err(error) if server_gone(&error.to_string()) => {
            wait_closed_server_gone(target.kind, target.id, seen, &error.to_string())
        }
        Err(error) => wait_closed_lost(target.kind, target.id, seen, &error),
    }
}

/// `exited` does not mean a pane object closed: a remain-on-exit pane remains
/// watchable until the lifecycle stream says `removed`.
fn bridge_closed_event(target: &ClosedTarget<'_>, event: &bridge::Event) -> bool {
    match event {
        bridge::Event::Pane { pane_id, reason } => {
            target.kind == "pane" && pane_id == target.id && *reason == bridge::PaneReason::Removed
        }
        bridge::Event::Launch(_) => false,
        bridge::Event::WindowRemoved { window_id } => {
            target.kind == "window" && window_id == target.id
        }
        bridge::Event::SessionRemoved { session_id } => {
            target.kind == "session" && session_id == target.id
        }
        bridge::Event::ServerExiting => false,
    }
}

/// `Some` is a completed wait; `None` tells the caller that no bridge stream
/// could be established and it should use the historical polling fallback.
async fn wait_closed_bridge(
    manager: &Manager,
    target: ClosedTarget<'_>,
    mut seen: bool,
    deadline: Instant,
    endpoint: bridge::Endpoint,
) -> Result<Option<i32>, String> {
    let deadline_at = tokio::time::Instant::from_std(deadline);
    let client = match tokio::time::timeout_at(
        deadline_at,
        bridge::Client::connect(&endpoint, "wait-closed-hello"),
    )
    .await
    {
        Ok(Ok(client)) => client,
        Ok(Err(error)) if error.is_server_exiting() => {
            return Ok(Some(wait_closed_server_gone(
                target.kind,
                target.id,
                seen,
                "bridge: server exiting",
            )?));
        }
        Ok(Err(error)) if error.falls_back_to_polling() => return Ok(None),
        Ok(Err(error)) => {
            return Ok(Some(wait_closed_lost(
                target.kind,
                target.id,
                seen,
                &error,
            )?));
        }
        Err(_) => {
            return Ok(Some(
                wait_closed_timeout_after_presence(manager, &target, seen, &endpoint).await?,
            ));
        }
    };
    let lifecycle = target.kind != "pane";
    let panes = if lifecycle {
        Vec::new()
    } else {
        vec![target.id.to_owned()]
    };
    let mut watch = match tokio::time::timeout_at(
        deadline_at,
        client.watch("wait-closed-watch", panes, lifecycle),
    )
    .await
    {
        Ok(Ok(watch)) => watch,
        Ok(Err(error)) if target.kind == "pane" && error.is_target_gone() => {
            return Ok(Some(wait_closed_observed(&target, seen)?));
        }
        Ok(Err(error)) if error.is_server_exiting() => {
            return Ok(Some(wait_closed_server_gone(
                target.kind,
                target.id,
                seen,
                "bridge: server exiting",
            )?));
        }
        Ok(Err(error)) if error.falls_back_to_polling() => return Ok(None),
        Ok(Err(error)) => {
            return Ok(Some(
                wait_closed_stream_lost(manager, &target, seen, &endpoint, &error).await?,
            ));
        }
        Err(_) => {
            return Ok(Some(
                wait_closed_timeout_after_presence(manager, &target, seen, &endpoint).await?,
            ));
        }
    };

    if lifecycle {
        match bridge_target_present(manager, target.field, target.id, &endpoint.core_boot_id).await
        {
            Ok(false) => {
                return Ok(Some(wait_closed_observed(&target, seen)?));
            }
            Ok(true) => seen = true,
            Err(error) => {
                if error == bridge::Error::BootMismatch {
                    return Ok(Some(wait_closed_server_gone(
                        target.kind,
                        target.id,
                        seen,
                        "native core boot changed",
                    )?));
                }
                return Ok(Some(
                    wait_closed_stream_lost(manager, &target, seen, &endpoint, &error).await?,
                ));
            }
        }
    } else {
        // A successful scoped ACK includes the pane baseline, so it existed
        // at the stream fence even if it exits before the first event.
        seen = true;
    }

    loop {
        let event = tokio::select! {
            event = watch.next() => match event {
                Ok(event) => event,
                Err(error) => return Ok(Some(
                    wait_closed_stream_lost(manager, &target, seen, &endpoint, &error).await?,
                )),
            },
            _ = tokio::time::sleep_until(deadline_at) => {
                return Ok(Some(wait_closed_timeout(target.kind, target.id, seen, target.closed)?));
            }
        };
        if matches!(event, bridge::Event::ServerExiting) {
            return Ok(Some(wait_closed_server_gone(
                target.kind,
                target.id,
                seen,
                "bridge: server exiting",
            )?));
        }
        if bridge_closed_event(&target, &event) {
            return Ok(Some(wait_closed_observed(&target, seen)?));
        }
        // `exited` is deliberately not a completion: remain-on-exit leaves
        // the pane object present, and a later `removed` is authoritative.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_takes_one_form_at_a_time() {
        let parse = |line: &str| {
            let args = line.split(' ').map(str::to_owned).collect::<Vec<_>>();
            wait_options(&args).map(|(wanted, timeout)| {
                let form = match wanted {
                    WaitFor::State { after, .. } => format!("state after={after}"),
                    WaitFor::Stage { key, stage } => format!("stage {key} {stage}"),
                    WaitFor::Ended { run } => format!("ended {run}"),
                    WaitFor::Closed { kind, target } => format!("closed {kind} {target}"),
                };
                (form, timeout)
            })
        };
        assert_eq!(
            parse("api --state idle --after-change").unwrap(),
            ("state after=true".into(), 30.0)
        );
        assert_eq!(
            parse("--stage pane_closed --operation k --timeout 2").unwrap(),
            ("stage k pane_closed".into(), 2.0)
        );
        assert_eq!(parse("--run r1 --ended").unwrap().0, "ended r1");
        // A repeated timeout: the last one, as before.
        assert_eq!(
            parse("api --state idle --timeout 1 --timeout 2").unwrap().1,
            2.0
        );
        assert_eq!(
            parse("--session work --closed").unwrap().0,
            "closed session work"
        );
        for wrong in [
            "api --state idle --run r1",
            "--run r1",
            "--pane %1 --window @1 --closed",
            "--run r1 --ended --after-change",
            "--run --ended --ended",
            "api --state idle --state busy",
            "--operation k --stage x --timeout 0",
            "api --timeout 5",
        ] {
            assert!(parse(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn a_wait_exits_0_only_for_what_it_waited_for() {
        assert_eq!(wait_exit("pane_closed", "pane_closed"), 0);
        assert_eq!(wait_exit("pane_closed", "state_observed"), 5);
        assert_eq!(wait_exit("run_changed", "state_observed"), 5);
        for unknown in ["server_gone", "observation_lost", "timeout"] {
            assert_eq!(wait_exit(unknown, "run_ended"), 6);
        }
        // Every stage a wait may name is in the order.
        for action in ["start", "prompt", "interrupt", "close", "answer"] {
            for stage in action_stages(action) {
                assert!(
                    stage_rank(stage).is_some_and(|(rank, _)| rank >= 2),
                    "{stage}"
                );
            }
        }
        assert_eq!(stage_rank("not_applied"), None);
        assert_eq!(stage_rank("outcome_unknown"), None);
        // A key that ended the provider was delivered; nothing confirms it.
        assert_eq!(stage_rank("provider_exited"), Some((2, true)));
        assert_eq!(object_id('%', "%00").as_deref(), Some("%0"));
        assert_eq!(object_id('$', "$x"), None);
    }

    #[test]
    fn bridge_closed_wait_ignores_exit_until_the_object_is_removed() {
        let pane = ClosedTarget {
            kind: "pane",
            field: 3,
            id: "%4",
            closed: "pane_closed",
        };
        assert!(!bridge_closed_event(
            &pane,
            &bridge::Event::Pane {
                pane_id: "%4".into(),
                reason: bridge::PaneReason::Exited,
            }
        ));
        assert!(bridge_closed_event(
            &pane,
            &bridge::Event::Pane {
                pane_id: "%4".into(),
                reason: bridge::PaneReason::Removed,
            }
        ));
        let window = ClosedTarget {
            kind: "window",
            field: 2,
            id: "@2",
            closed: "window_closed",
        };
        assert!(bridge_closed_event(
            &window,
            &bridge::Event::WindowRemoved {
                window_id: "@2".into(),
            }
        ));
    }

    #[test]
    fn bridge_probe_maps_only_gone_or_restarted_cores_to_server_gone() {
        assert_eq!(
            bridge_probe_end_reason(BridgeProbe::SameBoot),
            "observation_lost"
        );
        assert_eq!(
            bridge_probe_end_reason(BridgeProbe::Unreadable),
            "observation_lost"
        );
        assert_eq!(
            bridge_probe_end_reason(BridgeProbe::ServerGone),
            "server_gone"
        );
        assert_eq!(
            bridge_probe_end_reason(BridgeProbe::BootChanged),
            "server_gone"
        );
    }

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
