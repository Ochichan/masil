//! Explicit, bounded child-server endpoints and their stdin RPC protocol.

use super::{Agent, Manager};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Mutex;

const VERSION: u8 = 1;
const MAX_ENDPOINTS: usize = 8;
const MAX_CONFIG_BYTES: u64 = 128 * 1024;
const MAX_INPUT_BYTES: usize = 256 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const CALL_DEADLINE: Duration = Duration::from_secs(8);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Endpoint {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    pub socket: String,
    pub host: Option<String>,
    pub binary: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u8,
    endpoints: Vec<Endpoint>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    List,
    View {
        args: Vec<String>,
    },
    Save {
        path: String,
    },
    Restore {
        path: String,
        allow_fresh: bool,
    },
    Get {
        target: String,
    },
    Action {
        expected: Expected,
        action: Action,
    },
    Start {
        name: String,
        provider: String,
        cwd: String,
        args: Vec<String>,
        session: Option<String>,
        split: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Expected {
    pub pane_id: String,
    pub boot: String,
    pub generation: String,
    pub run: String,
    pub revision: String,
    pub session_id: Option<String>,
}

impl From<&Agent> for Expected {
    fn from(agent: &Agent) -> Self {
        Self {
            pane_id: agent.pane_id.clone(),
            boot: agent.boot.clone(),
            generation: agent.generation.clone(),
            run: agent.run.clone(),
            revision: agent.revision.clone(),
            session_id: agent.session_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Action {
    Read {
        history: bool,
    },
    Rename {
        name: String,
    },
    Keys {
        keys: Vec<String>,
    },
    Draft {
        text: String,
    },
    Prompt {
        text: String,
        operation: Option<u64>,
    },
    Receipt {
        operation: Option<u64>,
    },
    ConnectionStatus {
        lease: String,
    },
    Ack,
    Close,
    Resume {
        name: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u8,
    ok: bool,
    value: Option<Value>,
    error: Option<String>,
}

impl Endpoint {
    pub async fn call(&self, request: &Request) -> Result<Value, String> {
        if !self.enabled {
            return Err(format!("endpoint '{}' is disabled", self.id));
        }
        self.clone().call_owned(request.clone()).await
    }

    async fn call_owned(self, request: Request) -> Result<Value, String> {
        validate_endpoint(&self)?;
        let input = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        if input.len() > MAX_INPUT_BYTES {
            return Err("endpoint request exceeds 262144 bytes".into());
        }

        let mut command = self.command();
        command.process_group(0);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("endpoint '{}' could not start: {error}", self.id))?;
        let mut process_group =
            ProcessGroup::new(child.id().ok_or("endpoint process identity unavailable")?)?;
        let mut stdin = child.stdin.take().ok_or("endpoint stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("endpoint stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("endpoint stderr unavailable")?;
        let output_bytes = Arc::new(Mutex::new(0usize));
        let stdout_bytes = Arc::clone(&output_bytes);
        let stderr_bytes = Arc::clone(&output_bytes);

        let operation = async {
            let write = async move {
                stdin
                    .write_all(&input)
                    .await
                    .map_err(|error| format!("endpoint request write failed: {error}"))?;
                stdin
                    .shutdown()
                    .await
                    .map_err(|error| format!("endpoint request close failed: {error}"))
            };
            let read_stdout = read_limited(stdout, stdout_bytes);
            let read_stderr = read_limited(stderr, stderr_bytes);
            let wait = child.wait();
            tokio::join!(write, read_stdout, read_stderr, wait)
        };

        let (write, stdout, stderr, status) =
            match tokio::time::timeout(CALL_DEADLINE, operation).await {
                Ok(result) => result,
                Err(_) => {
                    process_group.kill();
                    let _ = child.wait().await;
                    return Err(format!("endpoint '{}' timed out after 8 seconds", self.id));
                }
            };
        process_group.disarm();
        write?;
        let stdout = stdout?;
        let stderr = stderr?;
        let status = status.map_err(|error| format!("endpoint wait failed: {error}"))?;
        if !status.success() {
            let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
            return Err(if detail.is_empty() {
                format!("endpoint '{}' exited with {status}", self.id)
            } else {
                format!("endpoint '{}' exited with {status}: {detail}", self.id)
            });
        }
        let envelope: Envelope = serde_json::from_slice(&stdout)
            .map_err(|_| format!("endpoint '{}' returned an invalid RPC response", self.id))?;
        if envelope.version != VERSION {
            return Err(format!(
                "endpoint '{}' uses unsupported RPC version {}",
                self.id, envelope.version
            ));
        }
        match (envelope.ok, envelope.value, envelope.error) {
            (true, Some(value), None) => Ok(value),
            (false, None, Some(error)) if !error.is_empty() && error.len() <= 4096 => Err(error),
            _ => Err(format!(
                "endpoint '{}' returned an invalid RPC envelope",
                self.id
            )),
        }
    }

    fn command(&self) -> Command {
        if let Some(host) = &self.host {
            let mut command = Command::new("ssh");
            let remote = [&self.binary, "agent", "--socket", &self.socket, "rpc"]
                .into_iter()
                .map(shell_quote)
                .collect::<Vec<_>>()
                .join(" ");
            command
                .arg("-o")
                .arg("BatchMode=yes")
                .arg("-o")
                .arg("StrictHostKeyChecking=yes")
                .arg("-o")
                .arg("ConnectTimeout=3")
                .arg("--")
                .arg(host)
                .arg(remote);
            command
        } else {
            let mut command = Command::new(&self.binary);
            command
                .arg("agent")
                .arg("--socket")
                .arg(&self.socket)
                .arg("rpc");
            command
        }
    }
}

struct ProcessGroup {
    id: i32,
    armed: bool,
}

impl ProcessGroup {
    fn new(id: u32) -> Result<Self, String> {
        let id = i32::try_from(id).map_err(|_| "endpoint process identity is invalid")?;
        Ok(Self { id, armed: true })
    }

    fn kill(&mut self) {
        if self.armed {
            // The child is placed in a fresh group whose ID is its PID before exec.
            unsafe {
                libc::kill(-self.id, libc::SIGKILL);
            }
            self.armed = false;
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn read_limited<R>(mut reader: R, total: Arc<Mutex<usize>>) -> Result<Vec<u8>, String>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|error| format!("endpoint output read failed: {error}"))?;
        if count == 0 {
            return Ok(output);
        }
        let mut used = total.lock().await;
        *used = used
            .checked_add(count)
            .ok_or("endpoint output size overflow")?;
        if *used > MAX_OUTPUT_BYTES {
            return Err("endpoint output exceeds 1048576 bytes".into());
        }
        drop(used);
        output.extend_from_slice(&buffer[..count]);
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn load() -> Result<Vec<Endpoint>, String> {
    load_from(&config_path()?)
}

fn load_from(path: &Path) -> Result<Vec<Endpoint>, String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("endpoint configuration: {error}")),
    };
    validate_directory(
        path.parent()
            .ok_or("endpoint configuration has no directory")?,
    )?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() > MAX_CONFIG_BYTES
    {
        return Err("endpoint configuration must be a small private owner file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("endpoint configuration: {error}"))?;
    if bytes.len() as u64 >= MAX_CONFIG_BYTES {
        return Err("endpoint configuration is too large".into());
    }
    let stored: Stored = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid endpoint configuration: {error}"))?;
    if stored.version != VERSION {
        return Err(format!(
            "unsupported endpoint configuration version {}",
            stored.version
        ));
    }
    validate_endpoints(&stored.endpoints)?;
    Ok(stored.endpoints)
}

pub(crate) fn configure(args: &[String]) -> Result<Value, String> {
    match args.first().map(String::as_str) {
        Some("list") if args.len() == 1 => {
            Ok(json!({"version": VERSION, "endpoints": load()?}))
        }
        Some("add") => configure_add(&args[1..]),
        Some(command @ ("remove" | "enable" | "disable")) if args.len() == 2 => {
            configure_update(command, &args[1])
        }
        _ => Err("endpoints requires list, add ID --socket PATH [--host USER@HOST] [--binary ABSOLUTE_OR_masil-agent] [--label LABEL], remove ID, enable ID, or disable ID".into()),
    }
}

fn configure_add(args: &[String]) -> Result<Value, String> {
    let id = args.first().ok_or("endpoint add requires an ID")?.clone();
    let mut socket = None;
    let mut host = None;
    let mut binary = None;
    let mut label = None;
    let mut index = 1;
    while index < args.len() {
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("missing value for {}", args[index]))?
            .clone();
        match args[index].as_str() {
            "--socket" if socket.is_none() => socket = Some(value),
            "--host" if host.is_none() => host = Some(value),
            "--binary" if binary.is_none() => binary = Some(value),
            "--label" if label.is_none() => label = Some(value),
            option => return Err(format!("unknown or repeated endpoint option: {option}")),
        }
        index += 2;
    }
    let endpoint = Endpoint {
        label: label.unwrap_or_else(|| id.clone()),
        id,
        enabled: true,
        socket: socket.ok_or("endpoint add requires --socket PATH")?,
        host,
        binary: binary.unwrap_or_else(|| "masil-agent".into()),
    };
    validate_endpoint(&endpoint)?;
    let path = config_path()?;
    let _lock = ConfigLock::take(&path)?;
    let mut endpoints = load_from(&path)?;
    if endpoints
        .iter()
        .any(|candidate| candidate.id == endpoint.id)
    {
        return Err(format!("endpoint '{}' already exists", endpoint.id));
    }
    if endpoints.len() >= MAX_ENDPOINTS {
        return Err("at most 8 endpoints may be configured".into());
    }
    endpoints.push(endpoint.clone());
    save(&path, &endpoints)?;
    Ok(json!({"stage":"endpoint_added", "endpoint":endpoint}))
}

fn configure_update(command: &str, id: &str) -> Result<Value, String> {
    validate_id(id)?;
    let path = config_path()?;
    let _lock = ConfigLock::take(&path)?;
    let mut endpoints = load_from(&path)?;
    let index = endpoints
        .iter()
        .position(|endpoint| endpoint.id == id)
        .ok_or_else(|| format!("endpoint '{id}' not found"))?;
    if command == "remove" {
        let endpoint = endpoints.remove(index);
        save(&path, &endpoints)?;
        return Ok(json!({"stage":"endpoint_removed", "endpoint":endpoint}));
    }
    endpoints[index].enabled = command == "enable";
    let endpoint = endpoints[index].clone();
    save(&path, &endpoints)?;
    Ok(
        json!({"stage":if endpoint.enabled {"endpoint_enabled"} else {"endpoint_disabled"}, "endpoint":endpoint}),
    )
}

fn config_path() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })
        .ok_or("endpoint configuration needs an absolute XDG_CONFIG_HOME or HOME")?;
    Ok(base.join("masil/endpoints.json"))
}

fn validate_endpoints(endpoints: &[Endpoint]) -> Result<(), String> {
    if endpoints.len() > MAX_ENDPOINTS {
        return Err("endpoint configuration contains more than 8 endpoints".into());
    }
    let mut ids = HashSet::new();
    for endpoint in endpoints {
        validate_endpoint(endpoint)?;
        if !ids.insert(endpoint.id.as_str()) {
            return Err(format!("duplicate endpoint ID '{}'", endpoint.id));
        }
    }
    Ok(())
}

fn validate_endpoint(endpoint: &Endpoint) -> Result<(), String> {
    validate_id(&endpoint.id)?;
    validate_text("endpoint label", &endpoint.label, 1, 64)?;
    validate_text("endpoint socket", &endpoint.socket, 1, 4096)?;
    if !Path::new(&endpoint.socket).is_absolute() {
        return Err("endpoint socket must be an absolute path".into());
    }
    validate_text("endpoint binary", &endpoint.binary, 1, 4096)?;
    if endpoint.binary != "masil-agent" && !Path::new(&endpoint.binary).is_absolute() {
        return Err("endpoint binary must be 'masil-agent' or an absolute path".into());
    }
    if let Some(host) = &endpoint.host {
        validate_host(host)?;
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<(), String> {
    if id == "local"
        || id.len() > 32
        || !id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
    {
        return Err("endpoint ID must begin with a lowercase letter, use 1-32 lowercase letters, digits, '-' or '_', and cannot be 'local'".into());
    }
    Ok(())
}

fn validate_text(label: &str, value: &str, minimum: usize, maximum: usize) -> Result<(), String> {
    if !(minimum..=maximum).contains(&value.len())
        || value.chars().any(|character| character.is_control())
    {
        return Err(format!(
            "{label} must contain {minimum}-{maximum} bytes without control characters"
        ));
    }
    Ok(())
}

fn validate_host(host: &str) -> Result<(), String> {
    validate_text("endpoint host", host, 3, 255)?;
    let (user, hostname) = host
        .split_once('@')
        .filter(|(_, hostname)| !hostname.contains('@'))
        .ok_or("endpoint host must use USER@HOST")?;
    if user.is_empty()
        || user.len() > 64
        || !user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        || hostname.is_empty()
        || hostname.starts_with('-')
        || !hostname
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-:[]".contains(&byte))
    {
        return Err("endpoint host must use a bounded USER@HOST without shell characters".into());
    }
    Ok(())
}

fn validate_directory(directory: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(directory)
        .map_err(|error| format!("endpoint configuration directory: {error}"))?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err("endpoint configuration directory must be a private owner directory".into());
    }
    Ok(())
}

fn save(path: &Path, endpoints: &[Endpoint]) -> Result<(), String> {
    validate_endpoints(endpoints)?;
    let directory = path
        .parent()
        .ok_or("endpoint configuration has no directory")?;
    validate_directory(directory)?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (!metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600)
    {
        return Err("endpoint configuration must be a private owner file".into());
    }
    let stored = Stored {
        version: VERSION,
        endpoints: endpoints.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&stored).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err("endpoint configuration is too large".into());
    }
    let temporary = directory.join(format!(".endpoints.json.{}.tmp", std::process::id()));
    let result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|error| format!("endpoint configuration temporary file: {error}"))?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.write_all(b"\n").map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
        fs::rename(&temporary, path).map_err(|error| error.to_string())?;
        let directory_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(directory)
            .map_err(|error| error.to_string())?;
        directory_file.sync_all().map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

struct ConfigLock {
    _file: fs::File,
}

impl ConfigLock {
    fn take(path: &Path) -> Result<Self, String> {
        let directory = path
            .parent()
            .ok_or("endpoint configuration has no directory")?;
        match fs::symlink_metadata(directory) {
            Ok(_) => validate_directory(directory)?,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::create_dir_all(directory).map_err(|error| error.to_string())?;
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                    .map_err(|error| error.to_string())?;
                validate_directory(directory)?;
            }
            Err(error) => return Err(error.to_string()),
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join(".endpoints.lock"))
            .map_err(|error| format!("endpoint configuration lock: {error}"))?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
        {
            return Err("endpoint configuration lock must be a private owner file".into());
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "endpoint configuration lock: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self { _file: file })
    }
}

pub(crate) async fn serve(manager: &Manager, bytes: &[u8]) -> Value {
    let result = if bytes.len() > MAX_INPUT_BYTES {
        Err("RPC request exceeds 262144 bytes".into())
    } else {
        match decode_request(bytes) {
            Ok(request) => rpc(manager, request).await,
            Err(error) => Err(format!("invalid RPC request: {error}")),
        }
    };
    match result {
        Ok(value) => json!({"version":VERSION, "ok":true, "value":value}),
        Err(error) => json!({"version":VERSION, "ok":false, "error":error}),
    }
}

fn decode_request(bytes: &[u8]) -> Result<Request, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let object = value
        .as_object()
        .ok_or("RPC request must be a JSON object")?;
    let operation = object
        .get("operation")
        .and_then(Value::as_str)
        .ok_or("RPC request needs a string operation")?;
    let allowed: &[&str] = match operation {
        "list" => &["operation"],
        "view" => &["operation", "args"],
        "save" => &["operation", "path"],
        "restore" => &["operation", "path", "allow_fresh"],
        "get" => &["operation", "target"],
        "action" => &["operation", "expected", "action"],
        "start" => &[
            "operation",
            "name",
            "provider",
            "cwd",
            "args",
            "session",
            "split",
        ],
        _ => return Err(format!("unknown RPC operation '{operation}'")),
    };
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(format!("unknown field '{field}'"));
    }
    serde_json::from_value(value).map_err(|error| error.to_string())
}

pub(crate) async fn rpc(manager: &Manager, request: Request) -> Result<Value, String> {
    match request {
        Request::List => {
            let before = manager.boot().await?;
            let agents = manager.list_view().await?;
            let after = manager.boot().await?;
            if before != after {
                return Err("native server restarted while listing agents".into());
            }
            Ok(json!({"boot":before, "agents":agents}))
        }
        Request::View { args } => manager.view(&args).await,
        Request::Save { path } => manager.save(Path::new(&path)).await,
        Request::Restore { path, allow_fresh } => {
            manager.restore(Path::new(&path), allow_fresh).await
        }
        Request::Get { target } => {
            validate_text("agent target", &target, 1, 128)?;
            serde_json::to_value(manager.get(&target).await?).map_err(|error| error.to_string())
        }
        Request::Start {
            name,
            provider,
            cwd,
            args,
            session,
            split,
        } => {
            manager
                .start(
                    &name,
                    &provider,
                    Path::new(&cwd),
                    &args,
                    session.as_deref(),
                    split.as_deref(),
                )
                .await
        }
        Request::Action { expected, action } => {
            let agent = manager.get(&expected.pane_id).await?;
            let allow_revision_change = matches!(
                action,
                Action::Receipt { .. } | Action::ConnectionStatus { .. }
            );
            verify_expected(&expected, &agent, allow_revision_change)?;
            match action {
                Action::Read { history } => Ok(json!({
                    "pane_id":agent.pane_id,
                    "run":agent.run,
                    "source":if history {"recent"} else {"visible"},
                    "text":manager.read(&agent, history).await?
                })),
                Action::Rename { name } => {
                    manager.rename(&agent, &name).await?;
                    Ok(json!({"stage":"registered", "pane_id":agent.pane_id, "name":name}))
                }
                Action::Keys { keys } => manager.keys(&agent, &keys).await,
                Action::Draft { text } => manager.draft(&agent, &text).await,
                Action::Prompt { text, operation } => {
                    manager
                        .prompt_with_operation(&agent, &text, operation)
                        .await
                }
                Action::Receipt { operation } => manager.prompt_receipt(&agent, operation).await,
                Action::ConnectionStatus { lease } => {
                    super::remote_cli::connection_status(manager, &agent, &lease).await
                }
                Action::Ack => {
                    manager.acknowledge(&agent).await?;
                    Ok(json!({"stage":"seen", "revision":agent.revision}))
                }
                Action::Close => {
                    manager.close(&agent).await?;
                    Ok(json!({"stage":"pane_closed", "pane_id":agent.pane_id, "run":agent.run}))
                }
                Action::Resume { name } => {
                    let session = manager.resume_session(&agent)?;
                    manager
                        .start(
                            &name,
                            &agent.provider,
                            Path::new(&agent.cwd),
                            &[],
                            Some(session),
                            None,
                        )
                        .await
                }
            }
        }
    }
}

fn verify_expected(
    expected: &Expected,
    actual: &Agent,
    allow_revision_change: bool,
) -> Result<(), String> {
    if expected.pane_id != actual.pane_id
        || expected.boot != actual.boot
        || expected.generation != actual.generation
        || expected.run != actual.run
        || expected.session_id != actual.session_id
        || (!allow_revision_change && expected.revision != actual.revision)
    {
        return Err("agent identity or state changed since it was listed".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(host: Option<&str>) -> Endpoint {
        Endpoint {
            id: "build".into(),
            label: "Build".into(),
            enabled: true,
            socket: "/tmp/masil socket's name".into(),
            host: host.map(str::to_owned),
            binary: "/opt/masil tools/masil-agent".into(),
        }
    }

    #[test]
    fn local_command_has_fixed_argv() {
        let command = endpoint(None).command();
        let command = command.as_std();
        assert_eq!(command.get_program(), "/opt/masil tools/masil-agent");
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["agent", "--socket", "/tmp/masil socket's name", "rpc"]
        );
    }

    #[test]
    fn ssh_command_uses_strict_options_and_quotes_each_remote_argument() {
        let command = endpoint(Some("dev@example.test")).command();
        let command = command.as_std();
        assert_eq!(command.get_program(), "ssh");
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ConnectTimeout=3",
                "--",
                "dev@example.test",
                "'/opt/masil tools/masil-agent' 'agent' '--socket' '/tmp/masil socket'\"'\"'s name' 'rpc'",
            ]
        );
        assert!(validate_host("dev@example.test;touch /tmp/injected").is_err());
    }

    #[tokio::test]
    async fn output_reader_stops_above_the_shared_limit() {
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let writing = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; MAX_OUTPUT_BYTES + 1])
                .await
                .unwrap();
        });
        let error = read_limited(reader, Arc::new(Mutex::new(0)))
            .await
            .unwrap_err();
        assert_eq!(error, "endpoint output exceeds 1048576 bytes");
        writing.abort();
    }

    #[tokio::test]
    async fn cancelling_a_call_kills_and_reaps_its_process_group() {
        let directory =
            std::env::temp_dir().join(format!("masil-endpoint-cancel-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).unwrap();
        let script = directory.join("slow-agent");
        let pids = directory.join("pids");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$$\" > {}\nsleep 30 &\nprintf '%s\\n' \"$!\" >> {}\nwait\n",
                shell_quote(pids.to_str().unwrap()),
                shell_quote(pids.to_str().unwrap())
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = Endpoint {
            binary: script.to_string_lossy().into_owned(),
            socket: "/tmp/unused-masil.sock".into(),
            ..endpoint(None)
        };
        let call = tokio::spawn(async move { endpoint.call(&Request::List).await });
        let recorded = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = fs::read_to_string(&pids) {
                    let values = text
                        .lines()
                        .filter_map(|value| value.parse::<i32>().ok())
                        .collect::<Vec<_>>();
                    if values.len() == 2 {
                        break values;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if recorded.iter().all(|pid| unsafe {
                    libc::kill(*pid, 0) == -1
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn typed_requests_reject_unknown_fields() {
        let error = decode_request(br#"{"operation":"list","unexpected":true}"#).unwrap_err();
        assert!(error.contains("unknown field"));
    }
}
