//! Versioned agent snapshots and crash-safe restore plans.

use super::operations::RestoreTarget;
use super::{Agent, Manager, executable, valid_name, validate_args};
use crate::{observation::now_ms, providers};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
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
        if fs::symlink_metadata(&receipt).is_ok()
            || fs::symlink_metadata(claims_path(&path)?).is_ok()
        {
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

    /// Restore resumable entries (T1-d). The plan lives in this server's
    /// operation store, keyed by the snapshot's SHA-256; each launch is a
    /// durable start (`boot:<boot>/start/restore.<plan>.<index>.<attempt>`),
    /// so a repeated or interrupted restore never starts a target twice. A
    /// claim beside the snapshot tells another server that this one
    /// restored it. Then every launched target is watched until it is
    /// ready, or `wait` runs out.
    pub async fn restore(&self, path: &Path, options: RestoreOptions) -> Result<Value, String> {
        let path = canonical_snapshot_path(path)?;
        let snapshot_lock = snapshot_lock(&path)?;
        let restore_lock = restore_lock(self)?;
        let text = read_text(&path, "snapshot")?;
        let snapshot: Snapshot =
            serde_json::from_str(&text).map_err(|error| format!("invalid snapshot: {error}"))?;
        validate_snapshot(&snapshot)?;
        let boot = self.boot().await?;
        let socket = self.native.socket.to_string_lossy().into_owned();
        // A receipt from before plans: what it proved carries over, in the
        // transaction that makes the plan (a bad receipt refuses every time).
        let receipt_path = receipt_path(&path)?;
        let mut legacy = Vec::new();
        if fs::symlink_metadata(&receipt_path).is_ok() {
            let receipt = load_receipt(&receipt_path, &snapshot)?;
            for entry in &receipt.entries {
                let (stage, detail) = match entry.state {
                    ReceiptState::Started => ("started", None),
                    ReceiptState::Pending => (
                        "unknown",
                        Some("a previous launch was pending; inspect native panes, then restore with --again".to_owned()),
                    ),
                    ReceiptState::FailedNotStarted => ("failed_not_started", entry.error.clone()),
                };
                legacy.push((
                    snapshot.agents[entry.index].name.clone(),
                    RestoreTarget {
                        index: entry.index,
                        stage: stage.into(),
                        boot: Some(boot.clone()),
                        attempt: 0,
                        start_key: None,
                        pane_id: entry.pane_id.clone(),
                        run: entry.run.clone(),
                        detail,
                    },
                ));
            }
        }
        let mut store = self.operation_store().await?;
        let (plan, _) = store.restore_plan(
            &super::prompt::sha256(&text),
            &path.to_string_lossy(),
            &legacy,
            now_ms(),
        )?;
        let rows = store.restore_targets(plan)?;
        let claims_path = claims_path(&path)?;
        let mut claims = load_claims(&claims_path, &snapshot)?;
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
        // Targets launched now: (result index, row).
        let mut launched: Vec<(usize, RestoreTarget)> = Vec::new();
        // Each other server is asked once.
        let mut boots: HashMap<String, Option<String>> = HashMap::new();

        for (index, entry) in snapshot.agents.iter().enumerate() {
            let row = rows.get(&index);
            let same_boot = row.is_some_and(|row| row.boot.as_deref() == Some(boot.as_str()));
            if let Some(row) = row {
                let live = row.run.as_ref().is_some_and(|run| {
                    current
                        .iter()
                        .any(|agent| &agent.run == run && agent.process == "running")
                });
                match row.stage.as_str() {
                    "started"
                    | "ready"
                    | "readiness_unconfirmed"
                    | "resume_mismatch"
                    | "exited"
                        if (same_boot || live) && !options.again =>
                    {
                        results.push(json!({
                            "index": index, "name": entry.name, "stage": "already_restored",
                            "action": "skipped", "pane_id": row.pane_id, "run": row.run,
                            "readiness": row.stage,
                        }));
                        continue;
                    }
                    // Launched before the plan kept keys: nothing to ask.
                    "unknown" if row.start_key.is_none() && !options.again => {
                        results.push(unknown_result(
                            index,
                            entry,
                            row.detail.as_deref().unwrap_or("launch outcome unknown"),
                        ));
                        continue;
                    }
                    _ => {}
                }
            }
            // A launch of this boot cut off before its result was kept:
            // its key answers (when it was admitted), and the checks below
            // would only see the agent it may have started.
            let asking = !options.again
                && same_boot
                && match row
                    .filter(|row| matches!(row.stage.as_str(), "pending" | "unknown"))
                    .and_then(|row| row.start_key.as_deref())
                {
                    Some(key) => store.get(key)?.is_some(),
                    None => false,
                };
            // Restored by another server that still runs.
            let claimed_elsewhere = match claims.get(&index) {
                Some(claim) if claim.socket != socket && !asking && !options.again => {
                    if !boots.contains_key(&claim.socket) {
                        let found = server_boot(Path::new(&claim.socket)).await;
                        boots.insert(claim.socket.clone(), found);
                    }
                    boots[&claim.socket].as_deref() == Some(claim.boot.as_str())
                }
                _ => false,
            };
            if claimed_elsewhere && let Some(claim) = claims.get(&index) {
                results.push(json!({
                    "index": index, "name": entry.name, "stage": "already_restored",
                    "action": "skipped", "restored_by": claim.socket,
                }));
                continue;
            }

            if !asking && current.iter().any(|agent| same_native_run(agent, entry)) {
                results.push(json!({
                    "index": index, "name": entry.name, "stage": "already_running",
                    "action": "skipped", "reason": "saved_native_run_is_live",
                    "pane_id": entry.native.pane_id,
                }));
                continue;
            }
            if !asking
                && let Some(session) = entry.native_session_ref.as_ref()
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
            if !asking && live_names.contains(&entry.name) {
                results.push(json!({
                    "index": index, "name": entry.name, "stage": "already_running",
                    "action": "skipped", "reason": "agent_name_is_already_running",
                }));
                continue;
            }
            if !asking && entry.native_session_ref.is_none() && !options.allow_fresh {
                results.push(json!({
                    "index": index, "name": entry.name,
                    "stage": "fresh_start_not_allowed", "action": "skipped",
                }));
                continue;
            }

            let mut target = RestoreTarget {
                index,
                stage: "pending".into(),
                boot: Some(boot.clone()),
                // The same key in this boot asks what it did; a new attempt
                // (--again) gets a key of its own.
                attempt: match row {
                    Some(row) if same_boot && options.again && row.start_key.is_some() => {
                        row.attempt + 1
                    }
                    Some(row) if same_boot => row.attempt,
                    _ => 0,
                },
                start_key: None,
                pane_id: None,
                run: None,
                detail: None,
            };
            let args = entry.original_args.as_deref().unwrap_or(&[]);
            if !asking && let Err(error) = preflight(entry, args) {
                let error = bounded_error(error);
                target.stage = "failed_not_started".into();
                target.detail = Some(error.clone());
                store.restore_set(plan, &entry.name, &target, now_ms())?;
                results.push(json!({
                    "index": index, "name": entry.name, "stage": "failed_not_started",
                    "action": "not_launched", "error": error, "can_retry": true,
                }));
                continue;
            }
            let id = format!("restore.{plan}.{index}.{}", target.attempt);
            target.start_key = Some(super::operations::key(
                &format!("boot:{boot}"),
                "start",
                &id,
            ));
            store.restore_set(plan, &entry.name, &target, now_ms())?;
            // Claimed before the launch: another server must not start it
            // while this one's outcome may still be unknown.
            let previous_claim = claims.insert(
                index,
                Claim {
                    socket: socket.clone(),
                    boot: boot.clone(),
                    start_key: target.start_key.clone().unwrap_or_default(),
                    at_ms: now_ms(),
                },
            );
            if let Err(error) = write_claims(&claims_path, &snapshot, &claims) {
                let error = bounded_error(format!("restore claim: {error}"));
                match previous_claim {
                    Some(claim) => claims.insert(index, claim),
                    None => claims.remove(&index),
                };
                target.stage = "failed_not_started".into();
                target.detail = Some(error.clone());
                store.restore_set(plan, &entry.name, &target, now_ms())?;
                results.push(json!({
                    "index": index, "name": entry.name, "stage": "failed_not_started",
                    "action": "not_launched", "error": error, "can_retry": true,
                }));
                continue;
            }

            let started = self
                .start_with_operation(
                    &entry.name,
                    &entry.provider,
                    Path::new(&entry.cwd),
                    args,
                    entry.native_session_ref.as_deref(),
                    None,
                    Some(super::durable::ClientKey {
                        pin: &boot,
                        id: &id,
                    }),
                    false,
                )
                .await;
            let stage = match &started {
                Ok(outcome) => outcome
                    .get("stage")
                    .and_then(Value::as_str)
                    .unwrap_or("outcome_unknown")
                    .to_owned(),
                Err(error) => super::failure::classify(error).1.to_owned(),
            };
            match (stage.as_str(), started) {
                ("process_started" | "process_exited", Ok(mut outcome)) => {
                    target.pane_id = outcome
                        .get("pane_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    target.run = outcome
                        .get("run")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    target.stage = if stage == "process_exited" {
                        "exited".into()
                    } else {
                        "started".into()
                    };
                    store.restore_set(plan, &entry.name, &target, now_ms())?;
                    live_names.insert(entry.name.clone());
                    if let Some(session) = entry.native_session_ref.as_ref() {
                        live_sessions.insert((entry.provider.clone(), session.clone()));
                    }
                    if let Some(object) = outcome.as_object_mut() {
                        object.insert("index".into(), json!(index));
                        object.insert("action".into(), json!("launched"));
                        if target.stage == "exited" {
                            object.insert("readiness".into(), json!("exited"));
                        }
                    }
                    if target.stage == "started" {
                        launched.push((results.len(), target.clone()));
                    }
                    results.push(outcome);
                }
                // Proven not to have started: a retry starts it.
                (
                    "cwd_rejected"
                    | "rejected_before_effect"
                    | "not_applied"
                    | "usage"
                    | "invalid_argument"
                    | "identity_mismatch"
                    | "unknown_provider",
                    result,
                ) => {
                    let error = bounded_error(match result {
                        Ok(outcome) => format!("launch ended as {stage}: {outcome}"),
                        Err(error) => error,
                    });
                    target.stage = "failed_not_started".into();
                    target.detail = Some(error.clone());
                    store.restore_set(plan, &entry.name, &target, now_ms())?;
                    // Proven not started: the claim goes back to what it was
                    // (another server's, under --again), or goes.
                    match previous_claim {
                        Some(claim) => claims.insert(index, claim),
                        None => claims.remove(&index),
                    };
                    let mut result = json!({
                        "index": index, "name": entry.name, "stage": "failed_not_started",
                        "action": "not_launched", "error": error, "can_retry": true,
                    });
                    if let Err(error) = write_claims(&claims_path, &snapshot, &claims) {
                        result["claim_error"] = json!(bounded_error(error));
                    }
                    results.push(result);
                }
                (_, result) => {
                    // The launch may have reached the server: the key is
                    // kept, and the next restore asks it what happened.
                    let error = bounded_error(match result {
                        Ok(outcome) => format!("launch ended as {stage}: {outcome}"),
                        Err(error) => error,
                    });
                    target.stage = "unknown".into();
                    target.detail = Some(error.clone());
                    store.restore_set(plan, &entry.name, &target, now_ms())?;
                    let mut result = unknown_result(index, entry, &error);
                    result["can_retry"] = json!(true);
                    results.push(result);
                }
            }
        }

        // Watching readiness needs neither lock: another restore or a save
        // may go on meanwhile.
        drop(snapshot_lock);
        drop(restore_lock);
        // AM-13: one deadline for every target launched now.
        if !launched.is_empty() && !options.wait.is_zero() {
            let ready = self.readiness(&snapshot, &mut launched, options.wait).await;
            // Only onto the same launch: another restore may have started
            // the target again while this one watched.
            for (_, target) in &launched {
                store.restore_settle(plan, target, now_ms())?;
            }
            for ((slot, target), reason) in launched.iter().zip(ready) {
                results[*slot]["readiness"] = json!(target.stage);
                if let Some(reason) = reason {
                    results[*slot]["readiness_reason"] = json!(reason);
                }
            }
        } else {
            for (slot, _) in &launched {
                results[*slot]["readiness"] = json!("not_checked");
            }
        }

        let readiness = |stage: &str| {
            results
                .iter()
                .filter(|result| result.get("readiness").and_then(Value::as_str) == Some(stage))
                .count()
        };
        let launched_count = result_count(&results, "process_started");
        let already_running = result_count(&results, "already_running");
        let already_restored = result_count(&results, "already_restored");
        let failed = result_count(&results, "failed_not_started");
        let unknown = result_count(&results, "unknown");
        let fresh_denied = result_count(&results, "fresh_start_not_allowed");
        // A target that ended before it could be seen ready is not restored.
        let ended = results
            .iter()
            .filter(|result| {
                matches!(
                    result.get("stage").and_then(Value::as_str),
                    Some("process_started" | "already_restored")
                ) && result.get("readiness").and_then(Value::as_str) == Some("exited")
            })
            .count();
        let satisfied = launched_count + already_running + already_restored - ended;
        Ok(json!({
            "stage": "restore_finished",
            "version": VERSION,
            "snapshot_id": snapshot.snapshot_id,
            "plan": plan,
            "all_started": satisfied == snapshot.agents.len(),
            "partial": satisfied != snapshot.agents.len(),
            "counts": {
                "total": snapshot.agents.len(),
                "launched": launched_count,
                "already_running": already_running,
                "already_restored": already_restored,
                "failed_not_started": failed,
                "unknown": unknown,
                "fresh_start_not_allowed": fresh_denied,
                "ready": readiness("ready"),
                "readiness_unconfirmed": readiness("readiness_unconfirmed"),
                "resume_mismatch": readiness("resume_mismatch"),
                "exited": readiness("exited"),
            },
            "entries": results,
        }))
    }

    /// Watches the targets launched by a restore until each is ready or
    /// cannot be shown to be, under one deadline. A fresh target is ready
    /// once it shows idle or working. A resumed one is ready when it
    /// reported the session it was asked to resume (`binding.resume`
    /// `verified`); without any callback a few seconds after it is up, or
    /// for OpenCode and Kilo (which report it with the first prompt), that
    /// cannot be shown, and it is `readiness_unconfirmed` at once.
    async fn readiness(
        &self,
        snapshot: &Snapshot,
        targets: &mut [(usize, RestoreTarget)],
        wait: std::time::Duration,
    ) -> Vec<Option<&'static str>> {
        let deadline = std::time::Instant::now() + wait;
        let mut reasons: Vec<Option<&'static str>> = vec![None; targets.len()];
        let mut up_since: Vec<Option<std::time::Instant>> = vec![None; targets.len()];
        loop {
            // A pass that could not read the server decides nothing.
            let (Ok(agents), Ok(panes)) = (
                self.list().await,
                self.command(&["list-panes", "-a", "-F", "#{pane_id}"])
                    .await,
            ) else {
                if std::time::Instant::now() >= deadline {
                    Self::unconfirmed(targets, &mut reasons);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                continue;
            };
            let now = std::time::Instant::now();
            for (number, (_, target)) in targets.iter_mut().enumerate() {
                if target.stage != "started" {
                    continue;
                }
                let entry = &snapshot.agents[target.index];
                let agent = agents
                    .iter()
                    .find(|agent| Some(&agent.run) == target.run.as_ref());
                let Some(agent) = agent else {
                    let gone = target
                        .pane_id
                        .as_ref()
                        .is_some_and(|pane| !panes.lines().any(|line| line == pane));
                    if gone {
                        target.stage = "exited".into();
                    }
                    continue;
                };
                if agent.process == "exited" {
                    target.stage = "exited".into();
                    continue;
                }
                let up = ["idle", "working"].contains(&agent.state.as_str());
                if up && up_since[number].is_none() {
                    up_since[number] = Some(now);
                }
                if entry.native_session_ref.is_none() {
                    if up {
                        target.stage = "ready".into();
                    }
                    continue;
                }
                match agent.binding.resume.as_deref() {
                    Some("verified") => target.stage = "ready".into(),
                    Some("mismatch") => target.stage = "resume_mismatch".into(),
                    Some("awaiting_prompt") if up => {
                        target.stage = "readiness_unconfirmed".into();
                        reasons[number] = Some("session_reported_with_first_prompt");
                    }
                    _ if up
                        && !agent.capabilities.callbacks_seen
                        && up_since[number]
                            .is_some_and(|since| now.duration_since(since) >= CALLBACK_GRACE) =>
                    {
                        target.stage = "readiness_unconfirmed".into();
                        reasons[number] = Some("no_session_report");
                    }
                    _ => {}
                }
            }
            if targets.iter().all(|(_, target)| target.stage != "started") {
                break;
            }
            if std::time::Instant::now() >= deadline {
                Self::unconfirmed(targets, &mut reasons);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        reasons
    }
}

impl Manager {
    /// Targets still undecided at the deadline.
    fn unconfirmed(targets: &mut [(usize, RestoreTarget)], reasons: &mut [Option<&'static str>]) {
        for (number, (_, target)) in targets.iter_mut().enumerate() {
            if target.stage == "started" {
                target.stage = "readiness_unconfirmed".into();
                reasons[number] = Some("deadline");
            }
        }
    }
}

/// How long a resumed agent that is up may go without any callback before
/// its resume is taken as one that cannot be shown.
const CALLBACK_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// `restore` options: fresh starts allowed, launch again what this server
/// or another already restored, and how long to watch for readiness.
pub(crate) struct RestoreOptions {
    pub allow_fresh: bool,
    pub again: bool,
    pub wait: std::time::Duration,
}

/// Which server launched a target (beside the snapshot, for other servers).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    socket: String,
    boot: String,
    start_key: String,
    at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claims {
    version: u32,
    snapshot_id: String,
    claims: BTreeMap<usize, Claim>,
}

fn claims_path(snapshot: &Path) -> Result<PathBuf, String> {
    let name = snapshot
        .file_name()
        .ok_or("snapshot path has no filename")?
        .to_string_lossy();
    Ok(snapshot.with_file_name(format!(".{name}.restore-claims.json")))
}

fn load_claims(path: &Path, snapshot: &Snapshot) -> Result<BTreeMap<usize, Claim>, String> {
    if fs::symlink_metadata(path).is_err() {
        return Ok(BTreeMap::new());
    }
    let claims: Claims = read_json(path, "restore claims")?;
    if claims.version != VERSION
        || claims.snapshot_id != snapshot.snapshot_id
        || claims
            .claims
            .keys()
            .any(|index| *index >= snapshot.agents.len())
    {
        return Err("restore claims do not belong to this snapshot".into());
    }
    Ok(claims.claims)
}

fn write_claims(
    path: &Path,
    snapshot: &Snapshot,
    claims: &BTreeMap<usize, Claim>,
) -> Result<(), String> {
    write_json_atomic(
        path,
        &Claims {
            version: VERSION,
            snapshot_id: snapshot.snapshot_id.clone(),
            claims: claims.clone(),
        },
    )
}

/// The boot ID of the server at `socket`, if one answers there.
async fn server_boot(socket: &Path) -> Option<String> {
    let context = crate::native_ui::Context {
        socket: socket.to_path_buf(),
        client: None,
    };
    let output = context
        .tmux(
            ["display-message", "-p", "#{masil_core_boot_id}"].map(std::ffi::OsString::from),
            None,
        )
        .await
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned()).filter(|boot| !boot.is_empty())
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

pub(crate) fn validate_private_metadata(
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

/// A private file's text, bounded as `read_json` reads it.
fn read_text(path: &Path, label: &str) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| format!("{label}: {error}"))?;
    validate_private_metadata(&metadata, label)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("{label}: {error}"))?;
    validate_private_metadata(&file.metadata().map_err(|error| error.to_string())?, label)?;
    let mut text = String::new();
    file.take(MAX_FILE + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("{label}: {error}"))?;
    if text.len() as u64 > MAX_FILE {
        return Err(format!("{label} exceeds {MAX_FILE} bytes"));
    }
    Ok(text)
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
