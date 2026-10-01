//! Versioned agent snapshots and crash-safe restore receipts.

use super::{Agent, Manager, executable, valid_name, validate_args};
use crate::{observation::now_ms, providers};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const VERSION: u32 = 1;
const MAX_FILE: u64 = 256 * 1024;
const MAX_AGENTS: usize = 64;
const MAX_ERROR: usize = 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    snapshot_id: String,
    created_at_ms: u64,
    agents: Vec<SavedAgent>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedAgent {
    name: String,
    provider: String,
    cwd: String,
    native_session_ref: Option<String>,
    original_args: Option<Vec<String>>,
    managed: bool,
    native: NativeReference,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeReference {
    core_boot_id: String,
    pty_generation: String,
    pane_id: String,
    run: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    snapshot_id: String,
    entries: Vec<ReceiptEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptEntry {
    index: usize,
    state: ReceiptState,
    pane_id: Option<String>,
    run: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReceiptState {
    Pending,
    Started,
    FailedNotStarted,
}

impl Manager {
    /// Save native identities and resumable provider state. Screens and
    /// process environments are deliberately excluded.
    pub async fn save(&self, path: &Path) -> Result<Value, String> {
        let requested_path = path.to_string_lossy().into_owned();
        let path = canonical_snapshot_path(path)?;
        let _snapshot_lock = snapshot_lock(&path)?;
        let receipt = receipt_path(&path)?;
        if fs::symlink_metadata(&receipt).is_ok() {
            return Err("snapshot has a restore receipt; choose a new snapshot path".into());
        }
        if let Ok(existing) = fs::symlink_metadata(&path) {
            validate_private_metadata(&existing, "existing snapshot")?;
            let previous: Snapshot = read_json(&path, "existing snapshot")?;
            validate_snapshot(&previous)?;
        }

        let agents = self.list().await?;
        if agents.len() > MAX_AGENTS {
            return Err(format!("snapshot supports at most {MAX_AGENTS} agents"));
        }
        let saved: Vec<_> = agents.iter().map(saved_agent).collect();
        let resumable = saved
            .iter()
            .filter(|entry| entry.native_session_ref.is_some())
            .count();
        let snapshot = Snapshot {
            version: VERSION,
            snapshot_id: super::nonce()?,
            created_at_ms: now_ms(),
            agents: saved,
        };
        validate_snapshot(&snapshot)?;
        write_json_atomic(&path, &snapshot)?;
        Ok(json!({
            "stage": "snapshot_saved",
            "version": VERSION,
            "snapshot_id": snapshot.snapshot_id,
            "path": requested_path,
            "agents": snapshot.agents.len(),
            "resumable": resumable,
            "fresh_only": snapshot.agents.len() - resumable,
        }))
    }

    /// Restore resumable entries. A second call retries only entries whose
    /// prior failure was proven to happen before any launch attempt.
    pub async fn restore(&self, path: &Path, allow_fresh: bool) -> Result<Value, String> {
        let path = canonical_snapshot_path(path)?;
        let _snapshot_lock = snapshot_lock(&path)?;
        let _restore_lock = restore_lock(self)?;
        let snapshot: Snapshot = read_json(&path, "snapshot")?;
        validate_snapshot(&snapshot)?;
        let receipt_path = receipt_path(&path)?;
        let mut receipt = load_receipt(&receipt_path, &snapshot)?;
        let mut journal: BTreeMap<usize, ReceiptEntry> = receipt
            .entries
            .drain(..)
            .map(|entry| (entry.index, entry))
            .collect();
        let current = self.list().await?;
        let mut live_names: HashSet<String> = current
            .iter()
            .filter(|agent| agent.process == "running")
            .map(|agent| agent.name.clone())
            .collect();
        let mut live_sessions: HashSet<(String, String)> = current
            .iter()
            .filter(|agent| agent.process == "running")
            .filter_map(|agent| {
                agent
                    .session_id
                    .as_ref()
                    .map(|session| (agent.provider.clone(), session.clone()))
            })
            .collect();
        let mut results = Vec::with_capacity(snapshot.agents.len());

        for (index, entry) in snapshot.agents.iter().enumerate() {
            if let Some(previous) = journal.get(&index) {
                match previous.state {
                    ReceiptState::Started => {
                        results.push(json!({
                            "index": index, "name": entry.name, "stage": "already_restored",
                            "action": "skipped", "pane_id": previous.pane_id,
                            "run": previous.run,
                        }));
                        continue;
                    }
                    ReceiptState::Pending => {
                        results.push(unknown_result(
                            index,
                            entry,
                            "a previous launch was pending; inspect native panes before retrying",
                        ));
                        continue;
                    }
                    ReceiptState::FailedNotStarted => {}
                }
            }

            if current.iter().any(|agent| same_native_run(agent, entry)) {
                results.push(json!({
                    "index": index, "name": entry.name, "stage": "already_running",
                    "action": "skipped", "reason": "saved_native_run_is_live",
                    "pane_id": entry.native.pane_id,
                }));
                continue;
            }
            if let Some(session) = entry.native_session_ref.as_ref()
                && live_sessions.contains(&(entry.provider.clone(), session.clone()))
            {
                results.push(json!({
                    "index": index, "name": entry.name, "stage": "already_running",
                    "action": "skipped", "reason": "native_session_is_already_managed",
                    "native_session_requested": session,
                    "native_session_verified": false,
                }));
                continue;
            }
            if live_names.contains(&entry.name) {
                results.push(json!({
                    "index": index, "name": entry.name, "stage": "already_running",
                    "action": "skipped", "reason": "agent_name_is_already_running",
                }));
                continue;
            }
            if entry.native_session_ref.is_none() && !allow_fresh {
                results.push(json!({
                    "index": index, "name": entry.name,
                    "stage": "fresh_start_not_allowed", "action": "skipped",
                }));
                continue;
            }

            let args = entry.original_args.as_deref().unwrap_or(&[]);
            if let Err(error) = preflight(entry, args) {
                let error = bounded_error(error);
                journal.insert(
                    index,
                    ReceiptEntry {
                        index,
                        state: ReceiptState::FailedNotStarted,
                        pane_id: None,
                        run: None,
                        error: Some(error.clone()),
                    },
                );
                receipt.entries = journal.values().cloned().collect();
                write_json_atomic(&receipt_path, &receipt)?;
                results.push(json!({
                    "index": index, "name": entry.name, "stage": "failed_not_started",
                    "action": "not_launched", "error": error, "can_retry": true,
                }));
                continue;
            }

            journal.insert(
                index,
                ReceiptEntry {
                    index,
                    state: ReceiptState::Pending,
                    pane_id: None,
                    run: None,
                    error: None,
                },
            );
            receipt.entries = journal.values().cloned().collect();
            write_json_atomic(&receipt_path, &receipt)?;

            match self
                .start(
                    &entry.name,
                    &entry.provider,
                    Path::new(&entry.cwd),
                    args,
                    entry.native_session_ref.as_deref(),
                    None,
                )
                .await
            {
                Ok(mut outcome) => {
                    let pane_id = outcome
                        .get("pane_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let run = outcome
                        .get("run")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let (Some(pane_id), Some(run)) = (pane_id, run) else {
                        results.push(unknown_result(
                            index,
                            entry,
                            "process start returned without a complete native receipt",
                        ));
                        continue;
                    };
                    journal.insert(
                        index,
                        ReceiptEntry {
                            index,
                            state: ReceiptState::Started,
                            pane_id: Some(pane_id),
                            run: Some(run),
                            error: None,
                        },
                    );
                    receipt.entries = journal.values().cloned().collect();
                    if let Err(error) = write_json_atomic(&receipt_path, &receipt) {
                        results.push(unknown_result(
                            index,
                            entry,
                            &format!("process start returned, but its receipt failed: {error}"),
                        ));
                        break;
                    }
                    live_names.insert(entry.name.clone());
                    if let Some(session) = entry.native_session_ref.as_ref() {
                        live_sessions.insert((entry.provider.clone(), session.clone()));
                    }
                    if let Some(object) = outcome.as_object_mut() {
                        object.insert("index".into(), json!(index));
                        object.insert("action".into(), json!("launched"));
                    }
                    results.push(outcome);
                }
                Err(error) => {
                    // Manager::start may have reached tmux before returning an
                    // error. Keep the durable pending marker and fail closed.
                    results.push(unknown_result(index, entry, &bounded_error(error)));
                }
            }
        }

        let launched = result_count(&results, "process_started");
        let already_running = result_count(&results, "already_running");
        let already_restored = result_count(&results, "already_restored");
        let failed = result_count(&results, "failed_not_started");
        let unknown = result_count(&results, "unknown");
        let fresh_denied = result_count(&results, "fresh_start_not_allowed");
        let satisfied = launched + already_running + already_restored;
        Ok(json!({
            "stage": "restore_finished",
            "version": VERSION,
            "snapshot_id": snapshot.snapshot_id,
            "receipt": receipt_path.to_string_lossy(),
            "all_started": satisfied == snapshot.agents.len(),
            "partial": satisfied != snapshot.agents.len(),
            "counts": {
                "total": snapshot.agents.len(),
                "launched": launched,
                "already_running": already_running,
                "already_restored": already_restored,
                "failed_not_started": failed,
                "unknown": unknown,
                "fresh_start_not_allowed": fresh_denied,
            },
            "entries": results,
        }))
    }
}

fn saved_agent(agent: &Agent) -> SavedAgent {
    SavedAgent {
        name: agent.name.clone(),
        provider: agent.provider.clone(),
        cwd: agent.cwd.clone(),
        native_session_ref: agent.session_id.clone(),
        original_args: original_args(agent),
        managed: agent.metadata.is_some(),
        native: NativeReference {
            core_boot_id: agent.boot.clone(),
            pty_generation: agent.generation.clone(),
            pane_id: agent.pane_id.clone(),
            run: agent.run.clone(),
        },
    }
}

fn original_args(agent: &Agent) -> Option<Vec<String>> {
    let metadata = agent.metadata.as_ref()?;
    Some(metadata.original_args.clone())
}

fn same_native_run(agent: &Agent, saved: &SavedAgent) -> bool {
    agent.process == "running"
        && agent.boot == saved.native.core_boot_id
        && agent.generation == saved.native.pty_generation
        && agent.pane_id == saved.native.pane_id
        && agent.run == saved.native.run
}

fn preflight(entry: &SavedAgent, args: &[String]) -> Result<(), String> {
    let cwd = Path::new(&entry.cwd)
        .canonicalize()
        .map_err(|error| format!("working directory: {error}"))?;
    if !cwd.is_dir() {
        return Err("working directory is not a directory".into());
    }
    validate_args(args)?;
    let argv = match entry.native_session_ref.as_deref() {
        Some(session) => providers::resume(&entry.provider, session)?,
        None => vec![
            providers::find(&entry.provider)
                .ok_or("unknown_provider: unknown agent provider")?
                .command
                .into(),
        ],
    };
    executable(&argv[0])?;
    Ok(())
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<(), String> {
    if snapshot.version != VERSION {
        return Err(format!(
            "unsupported agent snapshot version: {}",
            snapshot.version
        ));
    }
    if snapshot.snapshot_id.len() != 32
        || !snapshot
            .snapshot_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("invalid snapshot ID".into());
    }
    if snapshot.agents.len() > MAX_AGENTS {
        return Err(format!("snapshot supports at most {MAX_AGENTS} agents"));
    }
    let mut names = HashSet::new();
    for entry in &snapshot.agents {
        if !valid_name(&entry.name) || !names.insert(entry.name.as_str()) {
            return Err("snapshot contains an invalid or duplicate agent name".into());
        }
        if providers::find(&entry.provider).is_none() {
            return Err(format!(
                "unknown_provider: snapshot contains unknown provider: {}",
                entry.provider
            ));
        }
        if entry.cwd.len() > 4096
            || !Path::new(&entry.cwd).is_absolute()
            || entry.cwd.chars().any(char::is_control)
        {
            return Err("snapshot contains an invalid working directory".into());
        }
        if !valid_boot_id(&entry.native.core_boot_id)
            || entry.native.pty_generation.parse::<u64>().is_err()
            || crate::pane_id(&entry.native.pane_id).is_err()
            || entry.native.run.is_empty()
            || entry.native.run.len() > 512
            || entry.native.run.chars().any(char::is_control)
        {
            return Err("snapshot contains an invalid native reference".into());
        }
        if entry.native_session_ref.as_ref().is_some_and(|session| {
            session.is_empty() || session.len() > 4096 || session.chars().any(char::is_control)
        }) {
            return Err("snapshot contains an invalid native session reference".into());
        }
        if let Some(args) = entry.original_args.as_ref() {
            validate_args(args)?;
        }
    }
    Ok(())
}

fn load_receipt(path: &Path, snapshot: &Snapshot) -> Result<Receipt, String> {
    let mut receipt = match fs::symlink_metadata(path) {
        Ok(_) => read_json(path, "restore receipt")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Receipt {
            version: VERSION,
            snapshot_id: snapshot.snapshot_id.clone(),
            entries: Vec::new(),
        },
        Err(error) => return Err(format!("restore receipt: {error}")),
    };
    if receipt.version != VERSION || receipt.snapshot_id != snapshot.snapshot_id {
        return Err("restore receipt does not belong to this snapshot".into());
    }
    let mut indices = HashSet::new();
    for entry in &receipt.entries {
        if entry.index >= snapshot.agents.len() || !indices.insert(entry.index) {
            return Err("restore receipt contains an invalid or duplicate entry".into());
        }
        match entry.state {
            ReceiptState::Pending
                if entry.pane_id.is_some() || entry.run.is_some() || entry.error.is_some() =>
            {
                return Err("pending restore receipt contains a launch result".into());
            }
            ReceiptState::Started
                if entry.pane_id.is_none() || entry.run.is_none() || entry.error.is_some() =>
            {
                return Err("started restore receipt is missing its launch result".into());
            }
            ReceiptState::FailedNotStarted
                if entry.error.is_none() || entry.pane_id.is_some() || entry.run.is_some() =>
            {
                return Err("failed restore receipt is missing its error".into());
            }
            _ => {}
        }
        if entry
            .error
            .as_ref()
            .is_some_and(|error| error.chars().count() > MAX_ERROR)
        {
            return Err("restore receipt error exceeds its bound".into());
        }
    }
    receipt.entries.sort_by_key(|entry| entry.index);
    Ok(receipt)
}

fn unknown_result(index: usize, entry: &SavedAgent, error: &str) -> Value {
    json!({
        "index": index,
        "name": entry.name,
        "stage": "unknown",
        "action": "skipped",
        "error": bounded_error(error.to_owned()),
        "can_retry": false,
    })
}

fn bounded_error(error: String) -> String {
    error.chars().take(MAX_ERROR).collect()
}

fn result_count(results: &[Value], stage: &str) -> usize {
    results
        .iter()
        .filter(|result| result.get("stage").and_then(Value::as_str) == Some(stage))
        .count()
}

fn valid_boot_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

pub(super) fn private_parent(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let metadata =
        fs::symlink_metadata(parent).map_err(|error| format!("snapshot directory: {error}"))?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err("snapshot directory must be a private owner directory".into());
    }
    Ok(parent.to_path_buf())
}

fn canonical_snapshot_path(path: &Path) -> Result<PathBuf, String> {
    let name = path.file_name().ok_or("snapshot path has no filename")?;
    let parent = private_parent(path)?;
    let parent = parent
        .canonicalize()
        .map_err(|error| format!("snapshot directory: {error}"))?;
    private_parent(&parent.join(name))?;
    Ok(parent.join(name))
}

pub(super) fn validate_private_metadata(
    metadata: &fs::Metadata,
    label: &str,
) -> Result<(), String> {
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(format!("{label} must be a private owner file"));
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path, label: &str) -> Result<T, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| format!("{label}: {error}"))?;
    validate_private_metadata(&metadata, label)?;
    if metadata.len() > MAX_FILE {
        return Err(format!("{label} exceeds {MAX_FILE} bytes"));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("{label}: {error}"))?;
    validate_private_metadata(&file.metadata().map_err(|error| error.to_string())?, label)?;
    let mut data = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_FILE + 1)
        .read_to_end(&mut data)
        .map_err(|error| format!("{label}: {error}"))?;
    if data.len() as u64 > MAX_FILE {
        return Err(format!("{label} exceeds {MAX_FILE} bytes"));
    }
    serde_json::from_slice(&data).map_err(|error| format!("invalid {label}: {error}"))
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let parent = private_parent(path)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        validate_private_metadata(&metadata, "destination")?;
    }
    let data = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    if data.len() as u64 > MAX_FILE {
        return Err(format!("serialized file exceeds {MAX_FILE} bytes"));
    }
    let name = path
        .file_name()
        .ok_or("path has no filename")?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.tmp.{}", super::nonce()?));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|error| format!("temporary file: {error}"))?;
        file.write_all(&data)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
            .map_err(|error| error.to_string())?;
        fs::rename(&temporary, path).map_err(|error| error.to_string())?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&parent)
            .map_err(|error| error.to_string())?;
        directory.sync_all().map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn receipt_path(snapshot: &Path) -> Result<PathBuf, String> {
    let name = snapshot
        .file_name()
        .ok_or("snapshot path has no filename")?
        .to_string_lossy();
    Ok(snapshot.with_file_name(format!(".{name}.restore.json")))
}

fn snapshot_lock(path: &Path) -> Result<File, String> {
    let parent = path.parent().ok_or("snapshot path has no parent")?;
    let name = path
        .file_name()
        .ok_or("path has no filename")?
        .to_string_lossy();
    open_lock(&parent.join(format!(".{name}.snapshot.lock")), "snapshot")
}

fn restore_lock(manager: &Manager) -> Result<File, String> {
    let parent = manager
        .native
        .socket
        .parent()
        .ok_or("socket has no parent")?;
    private_parent(&manager.native.socket)?;
    let name = manager
        .native
        .socket
        .file_name()
        .ok_or("socket has no filename")?
        .to_string_lossy();
    open_lock(&parent.join(format!(".{name}.restore.lock")), "restore")
}

fn open_lock(path: &Path, operation: &str) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("{operation} lock: {error}"))?;
    validate_private_metadata(
        &file.metadata().map_err(|error| error.to_string())?,
        &format!("{operation} lock"),
    )?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(format!("another {operation} operation is in progress"));
    }
    Ok(file)
}
