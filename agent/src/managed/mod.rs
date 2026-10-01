//! Native agent management. Nothing runs until a management command or view is opened.
mod cli;
mod commands;
mod durable;
pub(crate) mod endpoints;
mod evidence;
pub(crate) mod failure;
pub(crate) mod find;
pub(crate) mod fleet;
mod inbox;
mod integration;
mod operations;
mod prompt;
mod remote_cli;
mod store;
mod view;

use crate::{detection::Engine, native_ui, observation::now_ms, providers};
pub(crate) use cli::run;
use evidence::Evidence;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const META: &str = "@masil-managed-agent";
const TRACKED: &str = "@masil-managed-observation";
const EVIDENCE: &str = "@masil-agent-run-evidence";
const STORE_OPTION: &str = "@masil-operation-store";
const FORMAT: &str = "#{q:pane_id}\t#{q:window_id}\t#{q:session_name}\t#{q:pane_pid}\t#{q:pane_dead}\t#{q:masil_core_boot_id}\t#{q:masil_pty_generation}\t#{q:pane_current_command}\t#{q:pane_current_path}\t#{q:pane_title}\t#{q:pane_tty}\t#{q:@masil-managed-agent}\t#{q:@masil-managed-observation}\t#{q:masil_foreground_pgid}\t#{q:pane_output_generation}\t#{q:masil_osc_progress}\t#{q:@masil-agent-run-evidence}";
const CAPTURE_BATCH_SIZE: usize = 12;
const MAX_PS_TTY_ARGUMENT: usize = 4096;
const TRACKED_REJECTED: &str = "masil-agent-stale";

pub(super) fn server_unreachable(error: String) -> String {
    if [
        "starting native command:",
        "native command has no",
        "writing native command:",
        "closing native command input:",
        "native command timed out",
        "waiting for native command:",
        "reading native command:",
    ]
    .iter()
    .any(|prefix| error.starts_with(prefix))
    {
        format!("server_unreachable: {error}")
    } else {
        error
    }
}

/// A guarded group that never started, or that tmux refused, ran nothing;
/// any later transport error leaves part of the group possibly applied.
fn group_error(error: String) -> String {
    if error.starts_with("starting native command:")
        || error.starts_with("native command failed:")
        || error.starts_with("native command exited with exit status")
    {
        server_unreachable(error)
    } else {
        format!("outcome_unknown: {error}")
    }
}

struct CaptureRequest {
    pane: String,
    identity: String,
    guard: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Guarded {
    Applied,
    Rejected,
}

struct PreparedLaunch {
    provider: &'static providers::Provider,
    cwd: PathBuf,
    argv: Vec<String>,
    args: Vec<String>,
}

enum PreparedEvidence {
    Cached(Arc<Evidence>),
    Screen(String),
}

fn store_cached_evidence(
    cache: &mut HashMap<String, (String, Arc<Evidence>)>,
    pane: String,
    key: String,
    evidence: Arc<Evidence>,
) {
    if cache.contains_key(&pane) || cache.len() < 64 {
        cache.insert(pane, (key, evidence));
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    name: String,
    provider: String,
    boot: String,
    generation: String,
    run: String,
    argv: Vec<String>,
    session: Option<String>,
    report: Option<Report>,
    #[serde(default)]
    last_sequence: u64,
    #[serde(default)]
    foreground_group: i32,
    #[serde(default)]
    original_args: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    sequence: u64,
    state: String,
    at: u64,
}

/// Evidence about one run, kept outside `@masil-managed-agent` so binaries
/// that predate it still decode the agent metadata. Unknown fields are
/// ignored for the same reason.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct RunEvidence {
    run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    binding: Option<Binding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    integration: Option<IntegrationSeen>,
    /// Origin of the metadata report with the same sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report: Option<ReportSource>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ReportSource {
    sequence: u64,
    source: String,
}

/// How `Metadata::session` was established for this run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Binding {
    session: String,
    source: BindingSource,
    sequence: u64,
    at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    event: Option<String>,
    /// The resume request that the first report contradicted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requested: Option<String>,
    /// The latest different session reported without a switch event. It stays
    /// until an explicit switch: a child provider process started inside the
    /// pane inherits the run identity and can report its own session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    conflict: Option<Contradiction>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Contradiction {
    session: String,
    source: BindingSource,
    sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    event: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BindingSource {
    /// `start --session`; the provider has not reported it.
    Requested,
    /// A provider hook carrying this run's identity reported it.
    NativeCallback,
    /// An OpenCode-family callback that declared frontend scope. The scope is
    /// a payload field, so it is not proof of what the TUI shows.
    FrontendCallback,
    /// `agent report --session` from a process holding this run's identity.
    RunReport,
}

impl BindingSource {
    fn label(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::NativeCallback => "native_callback",
            Self::FrontendCallback => "frontend_callback",
            Self::RunReport => "run_report",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Requested => 0,
            Self::RunReport => 1,
            Self::NativeCallback => 2,
            Self::FrontendCallback => 3,
        }
    }
}

/// Integration callbacks accepted during this run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct IntegrationSeen {
    lifecycle: bool,
    session: bool,
    sequence: u64,
    event: String,
    at: u64,
}

/// Where a report came from. Only integration hooks are provider-native.
#[derive(Clone, Copy)]
pub(crate) enum ReportOrigin<'a> {
    Run,
    Callback {
        event: &'a str,
        frontend: bool,
        /// The provider announced a deliberate session change.
        switch: bool,
    },
}

/// What a session report did to the binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BindingOutcome {
    Established,
    Confirmed,
    Switched,
    /// The first report named another session than `start --session`.
    ReplacedRequest,
    /// A different session without a switch event. The bound session is kept
    /// and a lifecycle state in the same report is not applied.
    Contradicted,
}

impl BindingOutcome {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Established => "established",
            Self::Confirmed => "confirmed",
            Self::Switched => "switched",
            Self::ReplacedRequest => "replaced_request",
            Self::Contradicted => "contradicted",
        }
    }
}

/// Public view of the agent binding. No state claims that the TUI shows this
/// session; `reported` means a process holding this run's identity said so.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct BindingView {
    pub state: String,
    pub session_id: Option<String>,
    pub source: Option<String>,
    pub requested_session: Option<String>,
    pub conflicting_session: Option<String>,
    pub event: Option<String>,
    pub sequence: u64,
    pub observed_at_ms: u64,
}

/// Evidence-based capability contract for one agent run. Values from an
/// endpoint that predates it are empty and mean unknown, not unsupported.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct CapabilityView {
    pub contract: u32,
    pub state_authority: String,
    pub session_identity: String,
    pub prompt_submit: String,
    pub provider_ack: bool,
    pub interrupt: String,
    pub approval_response: bool,
    pub question_response: bool,
    pub turn_identity: bool,
    pub resume: bool,
    pub integration: String,
    pub callbacks_seen: bool,
    pub lifecycle_seen: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tracked {
    run: String,
    state: String,
    report_sequence: u64,
    revision: u64,
    seen: bool,
    #[serde(default)]
    returned_idle: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Agent {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub pane_id: String,
    pub window_id: String,
    pub workspace: String,
    pub cwd: String,
    pub boot: String,
    pub generation: String,
    pub run: String,
    pub process: String,
    pub state: String,
    pub session_id: Option<String>,
    pub evidence: Arc<Evidence>,
    pub seen: bool,
    pub revision: String,
    pub returned_idle: bool,
    #[serde(default)]
    pub endpoint_id: String,
    #[serde(default)]
    pub endpoint_label: String,
    #[serde(default)]
    pub endpoint_key: String,
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub binding: BindingView,
    #[serde(default)]
    pub capabilities: CapabilityView,
    #[serde(skip)]
    metadata: Option<Metadata>,
    #[serde(skip)]
    run_evidence: Option<RunEvidence>,
    #[serde(skip)]
    encoded: String,
    #[serde(skip)]
    foreground_command: String,
    #[serde(skip)]
    tracked: Tracked,
    #[serde(skip)]
    tracked_encoded: String,
    #[serde(skip)]
    foreground_group: i32,
    #[serde(skip)]
    output_generation: u64,
    #[serde(skip)]
    title: String,
    #[serde(skip)]
    progress: String,
}

pub(crate) struct Manager {
    pub native: native_ui::Context,
    engine: Engine,
    cache: Mutex<HashMap<String, (String, Arc<Evidence>)>>,
}

impl Manager {
    pub fn new(socket: PathBuf, client: Option<String>) -> Result<Self, String> {
        let socket = socket
            .canonicalize()
            .map_err(|e| format!("server_unreachable: native socket: {e}"))?;
        Ok(Self {
            native: native_ui::Context { socket, client },
            engine: Engine::load()?,
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn reload(&mut self) -> Result<(), String> {
        self.engine = Engine::load()?;
        self.cache
            .get_mut()
            .map_err(|_| "detection cache unavailable")?
            .clear();
        Ok(())
    }

    pub async fn command(&self, args: &[&str]) -> Result<String, String> {
        let result = self
            .native
            .tmux(args.iter().map(OsString::from), None)
            .await
            .map_err(server_unreachable)?;
        String::from_utf8(result.stdout).map_err(|_| "native response is not UTF-8".into())
    }

    pub async fn boot(&self) -> Result<String, String> {
        let boot = self
            .command(&["display-message", "-p", "#{masil_core_boot_id}"])
            .await?;
        valid_boot(boot.trim()).map(str::to_owned)
    }

    async fn inventory(&self) -> Result<Vec<Vec<String>>, String> {
        let output = self.command(&["list-panes", "-a", "-F", FORMAT]).await?;
        let records = records(&output)?;
        if records.len() > 64 {
            return Err("agent management supports at most 64 panes per server".into());
        }
        Ok(records)
    }

    async fn capture_cache_misses(
        &self,
        inventory: &[Vec<String>],
        identified: &HashMap<String, &'static providers::Provider>,
        target: Option<&str>,
    ) -> Result<HashMap<String, PreparedEvidence>, String> {
        let mut requests = Vec::new();
        let mut seen = HashSet::new();
        let live_panes: HashSet<_> = inventory.iter().map(|fields| fields[0].as_str()).collect();
        let mut cached_snapshot = {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| "detection cache unavailable")?;
            cache.retain(|pane, _| live_panes.contains(pane.as_str()));
            cache.clone()
        };
        let mut prepared = HashMap::new();
        for fields in inventory {
            if fields[4] == "1"
                || !seen.insert(fields[0].clone())
                || target.is_some_and(|value| value.starts_with('%') && value != fields[0])
            {
                continue;
            }
            let Some(provider) = identified.get(&fields[0]) else {
                continue;
            };
            let key = observation_key(fields, provider.id);
            if let Some((_, evidence)) = cached_snapshot
                .remove(&fields[0])
                .filter(|(candidate, _)| candidate == &key)
            {
                prepared.insert(fields[0].clone(), PreparedEvidence::Cached(evidence));
            } else {
                requests.push(CaptureRequest {
                    pane: fields[0].clone(),
                    identity: capture_identity(fields, provider.id),
                    guard: capture_guard(fields),
                });
            }
        }
        if requests.is_empty() {
            return Ok(prepared);
        }

        for chunk in requests.chunks(CAPTURE_BATCH_SIZE) {
            let batch = self.capture_batch(chunk).await;
            if let Ok(screens) = batch {
                prepared.extend(chunk.iter().zip(screens).map(|(request, screen)| {
                    (request.pane.clone(), PreparedEvidence::Screen(screen))
                }));
            } else {
                for request in chunk {
                    prepared.insert(
                        request.pane.clone(),
                        PreparedEvidence::Screen(self.read_pane(&request.pane, false).await?),
                    );
                }
            }
        }

        // Capture output is untrusted terminal data. Re-read run identity after
        // the batch. Output, title, and progress are allowed to advance while a
        // pane is captured; their old cache key simply misses on the next poll.
        let current = self.inventory().await?;
        validate_inventory(&current)?;
        let current_identified = identify_foregrounds(&current).await;
        let current_by_pane: HashMap<_, _> = current
            .iter()
            .filter_map(|fields| {
                current_identified
                    .get(&fields[0])
                    .map(|provider| (fields[0].as_str(), capture_identity(fields, provider.id)))
            })
            .collect();
        if requests
            .iter()
            .any(|request| current_by_pane.get(request.pane.as_str()) != Some(&request.identity))
        {
            return Err(
                "identity_mismatch: native pane identity changed during observation".into(),
            );
        }
        Ok(prepared)
    }

    async fn capture_batch(&self, requests: &[CaptureRequest]) -> Result<Vec<String>, String> {
        let batch_nonce = nonce()?;
        let mut frames = Vec::with_capacity(requests.len());
        let groups = requests
            .iter()
            .enumerate()
            .map(|(index, request)| {
                let begin = format!("masil-capture-{batch_nonce}-{index}-begin");
                let end = format!("masil-capture-{batch_nonce}-{index}-end");
                let rejected = format!("masil-capture-{batch_nonce}-{index}-rejected");
                frames.push((begin.clone(), end.clone(), rejected.clone()));
                (
                    request.pane.clone(),
                    request.guard.clone(),
                    vec![
                        vec!["display-message".into(), "-p".into(), begin],
                        vec![
                            "capture-pane".into(),
                            "-p".into(),
                            "-J".into(),
                            "-t".into(),
                            request.pane.clone(),
                        ],
                        vec!["display-message".into(), "-p".into(), end],
                    ],
                    rejected,
                )
            })
            .collect::<Vec<_>>();
        let output = self
            .native
            .guarded_groups(&groups)
            .await
            .map_err(server_unreachable)?;
        parse_capture_frames(&output.stdout, &frames)
            .ok_or_else(|| "native capture batch framing is ambiguous".into())
    }

    pub async fn list(&self) -> Result<Vec<Agent>, String> {
        self.collect(None).await
    }

    async fn collect(&self, target: Option<&str>) -> Result<Vec<Agent>, String> {
        let inventory = self.inventory().await?;
        validate_inventory(&inventory)?;
        let identified = identify_foregrounds(&inventory).await;
        let mut prepared = self
            .capture_cache_misses(&inventory, &identified, target)
            .await?;
        let mut agents = Vec::new();
        let mut tracked_groups = Vec::new();
        let mut tracked_updates = Vec::new();
        let mut inbox_effects = Vec::new();
        let mut seen_panes = HashSet::new();
        for fields in inventory {
            // A linked window can occur in more than one session.
            if !seen_panes.insert(fields[0].clone()) {
                continue;
            }
            if target.is_some_and(|t| t.starts_with('%') && t != fields[0]) {
                continue;
            }
            let encoded = &fields[11];
            let metadata = decode::<Metadata>(encoded).filter(|m| {
                m.boot == fields[5]
                    && m.generation == fields[6]
                    && valid_name(&m.name)
                    && m.run.len() <= 128
                    && !m.run.chars().any(char::is_control)
                    && providers::find(&m.provider).is_some()
            });
            let dead = fields[4] == "1";
            let foreground_group = fields[13].parse::<i32>().unwrap_or(0);
            let metadata = metadata.filter(|m| dead || m.foreground_group == foreground_group);
            let identified = identified.get(&fields[0]).copied();
            let Some(provider) =
                identified.or_else(|| metadata.as_ref().and_then(|m| providers::find(&m.provider)))
            else {
                continue;
            };
            let foreground = identified.is_some_and(|p| p.id == provider.id);
            let mut evidence = if foreground {
                let key = observation_key(&fields, provider.id);
                match prepared
                    .remove(&fields[0])
                    .ok_or("native capture result is unavailable")?
                {
                    PreparedEvidence::Cached(evidence) => evidence,
                    PreparedEvidence::Screen(screen) => {
                        let evidence =
                            Arc::new(Evidence::from_value(self.engine.explain_with_progress(
                                provider.id,
                                &screen,
                                &fields[9],
                                &fields[15],
                            )));
                        let mut cache = self
                            .cache
                            .lock()
                            .map_err(|_| "detection cache unavailable")?;
                        store_cached_evidence(&mut cache, fields[0].clone(), key, evidence.clone());
                        evidence
                    }
                }
            } else {
                Arc::new(Evidence::from_value(
                    json!({"state":"unknown", "source":"process", "reason": if dead {"process_exited"} else {"foreground_not_verified"}}),
                ))
            };
            let mut state = evidence.state().to_owned();
            let metadata = metadata.filter(|m| {
                m.provider == provider.id && (dead || m.foreground_group == foreground_group)
            });
            let run_evidence = metadata
                .as_ref()
                .and_then(|m| decode::<RunEvidence>(&fields[16]).filter(|e| e.run == m.run));
            let mut report_authority = None;
            if let Some(report) = metadata.as_ref().and_then(|m| m.report.as_ref())
                && foreground
                && now_ms().saturating_sub(report.at) <= 30_000
            {
                // A visible blocker is stronger than a hook claiming idle/working.
                if !evidence.visible_blocker() || report.state == "blocked" {
                    state = report.state.clone();
                    evidence = run_report_evidence(evidence, report);
                    report_authority = Some(
                        run_evidence
                            .as_ref()
                            .and_then(|e| e.report.as_ref())
                            .filter(|source| source.sequence == report.sequence)
                            .map_or(REPORT_SOURCE_RUN, |source| source.source.as_str()),
                    );
                }
            }
            if dead {
                state = "exited".into();
            }
            let run = metadata.as_ref().map(|m| m.run.clone()).unwrap_or_else(|| {
                format!(
                    "{}-{}-{}-{foreground_group}",
                    fields[5], fields[0], fields[6]
                )
            });
            let previous = decode::<Tracked>(&fields[12]).unwrap_or_default();
            if evidence.skip_state_update() && previous.run == run {
                state = previous.state.clone();
            }
            let sequence = metadata
                .as_ref()
                .and_then(|m| m.report.as_ref())
                .map(|r| r.sequence)
                .unwrap_or(0);
            let changed = previous.run != run
                || previous.state != state
                || (state == "blocked" && previous.report_sequence != sequence);
            let returned_idle = previous.run == run
                && state == "idle"
                && (previous.state == "working" || previous.returned_idle);
            let tracked = if changed {
                Tracked {
                    run: run.clone(),
                    state: state.clone(),
                    report_sequence: sequence,
                    revision: previous
                        .revision
                        .checked_add(1)
                        .ok_or("observation revision exhausted")?,
                    seen: false,
                    returned_idle,
                }
            } else {
                previous.clone()
            };
            if changed {
                inbox_effects.extend(screen_effects(
                    provider.id,
                    &fields[0],
                    &previous,
                    &tracked,
                    dead,
                ));
            }
            let revision = tracked.revision.to_string();
            let name = metadata
                .as_ref()
                .map(|m| m.name.clone())
                .unwrap_or_else(|| {
                    format!("{}-{}", provider.id, fields[0].trim_start_matches('%'))
                });
            let process = if dead {
                "exited"
            } else if foreground {
                "running"
            } else {
                "unknown"
            };
            let binding = binding_view(metadata.as_ref(), run_evidence.as_ref());
            let capabilities = capability_view(
                provider.id,
                self.engine.has_manifest(provider.id),
                run_evidence.as_ref(),
                process,
                report_authority,
                &binding,
            );
            let agent = Agent {
                id: name.clone(),
                name,
                provider: provider.id.into(),
                pane_id: fields[0].clone(),
                window_id: fields[1].clone(),
                workspace: fields[2].clone(),
                cwd: fields[8].clone(),
                boot: fields[5].clone(),
                generation: fields[6].clone(),
                run,
                process: process.into(),
                state,
                session_id: metadata.as_ref().and_then(|m| m.session.clone()),
                evidence,
                seen: tracked.seen,
                revision,
                returned_idle: tracked.returned_idle,
                endpoint_id: String::new(),
                endpoint_label: String::new(),
                endpoint_key: String::new(),
                stale: false,
                binding,
                capabilities,
                metadata,
                run_evidence,
                encoded: encoded.clone(),
                foreground_command: fields[7].clone(),
                tracked,
                tracked_encoded: fields[12].clone(),
                foreground_group,
                output_generation: fields[14]
                    .parse()
                    .map_err(|_| "invalid output generation")?,
                title: fields[9].clone(),
                progress: fields[15].clone(),
            };
            if changed {
                let encoded = encode(&agent.tracked)?;
                tracked_groups.push((
                    agent.pane_id.clone(),
                    identity_guard(&agent),
                    vec![Self::option(&agent, TRACKED, encoded.clone())],
                    TRACKED_REJECTED.into(),
                ));
                tracked_updates.push((agents.len(), encoded));
            }
            agents.push(agent);
        }
        self.write_tracked_updates(&mut agents, &tracked_groups, &tracked_updates)
            .await?;
        // Only after the tracked revisions are committed: a lost CAS above
        // records nothing.
        self.inbox_apply(inbox_effects).await;
        Ok(agents)
    }

    async fn write_tracked_updates(
        &self,
        agents: &mut [Agent],
        groups: &[native_ui::GuardedGroup],
        updates: &[(usize, String)],
    ) -> Result<(), String> {
        if groups.len() != updates.len() {
            return Err("invalid tracked update batch".into());
        }
        let sizes = groups
            .iter()
            .map(native_ui::guarded_group_script_bytes)
            .collect::<Vec<_>>();
        if sizes.iter().any(|size| *size > native_ui::MAX_SCRIPT) {
            return Err("native tracked update exceeds script limit".into());
        }

        let mut start = 0;
        while start < groups.len() {
            let mut end = start;
            let mut bytes = 0;
            while end < groups.len() && bytes + sizes[end] <= native_ui::MAX_SCRIPT {
                bytes += sizes[end];
                end += 1;
            }
            let output = self
                .native
                .guarded_groups(&groups[start..end])
                .await
                .map_err(server_unreachable)?;
            if String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line == TRACKED_REJECTED)
            {
                return Err("identity_mismatch: agent run changed before the action".into());
            }
            for (agent, encoded) in &updates[start..end] {
                agents[*agent].tracked_encoded = encoded.clone();
            }
            start = end;
        }
        Ok(())
    }

    pub async fn get(&self, target: &str) -> Result<Agent, String> {
        if target.starts_with('%') {
            crate::pane_id(target).map_err(|error| format!("unknown_target_syntax: {error}"))?;
        }
        let mut matches = self
            .collect(Some(target))
            .await?
            .into_iter()
            .filter(|a| a.pane_id == target || a.name == target);
        let agent = matches
            .next()
            .ok_or("target_absent: agent target not found")?;
        if matches.next().is_some() {
            return Err("unknown_target_syntax: ambiguous agent name; use a pane ID".into());
        }
        Ok(agent)
    }

    pub async fn read_pane(&self, pane: &str, history: bool) -> Result<String, String> {
        crate::pane_id(pane)?;
        let mut args = vec!["capture-pane", "-p", "-J", "-t", pane];
        if history {
            args.extend(["-S", "-200"]);
        }
        self.command(&args).await
    }

    pub async fn read(&self, agent: &Agent, history: bool) -> Result<String, String> {
        let mut command = vec![
            "capture-pane".into(),
            "-p".into(),
            "-J".into(),
            "-t".into(),
            agent.pane_id.clone(),
        ];
        if history {
            command.extend(["-S".into(), "-200".into()]);
        }
        let output = self
            .native
            .guarded_script(
                &agent.pane_id,
                &identity_guard(agent),
                &[command],
                "masil-agent-stale",
            )
            .await
            .map_err(server_unreachable)?;
        let text = String::from_utf8(output.stdout).map_err(|_| "native output is not UTF-8")?;
        if text.trim() == "masil-agent-stale" {
            return Err("identity_mismatch: agent run changed before reading".into());
        }
        Ok(text)
    }

    async fn guarded(&self, agent: &Agent, commands: Vec<Vec<String>>) -> Result<(), String> {
        let guard = identity_guard(agent);
        let out = self
            .native
            .guarded_script(&agent.pane_id, &guard, &commands, "masil-agent-stale")
            .await
            .map_err(server_unreachable)?;
        if String::from_utf8_lossy(&out.stdout).contains("masil-agent-stale") {
            return Err("identity_mismatch: agent run changed before the action".into());
        }
        Ok(())
    }

    async fn guarded_input(&self, agent: &Agent, commands: Vec<Vec<String>>) -> Result<(), String> {
        self.guarded_input_condition(agent, commands, None).await
    }

    async fn guarded_input_condition(
        &self,
        agent: &Agent,
        commands: Vec<Vec<String>>,
        condition: Option<&str>,
    ) -> Result<(), String> {
        match self
            .guarded_input_outcome(agent, commands, condition)
            .await?
        {
            Guarded::Applied => Ok(()),
            Guarded::Rejected => Err(
                "identity_mismatch: agent foreground or run changed before input delivery".into(),
            ),
        }
    }

    /// `Rejected` proves the guard failed before any command ran. An `Err`
    /// leaves the outcome open: part of the group may have run.
    async fn guarded_input_outcome(
        &self,
        agent: &Agent,
        commands: Vec<Vec<String>>,
        condition: Option<&str>,
    ) -> Result<Guarded, String> {
        if agent.foreground_command.is_empty()
            || !agent
                .foreground_command
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_./-".contains(&b))
        {
            return Err("cannot guard this foreground command".into());
        }
        let mut guards = vec![
            identity_guard(agent),
            "#{==:#{pane_dead},0}".into(),
            "#{==:#{pane_input_off},0}".into(),
            format!(
                "#{{==:#{{pane_current_command}},{}}}",
                agent.foreground_command
            ),
        ];
        if let Some(condition) = condition {
            guards.push(condition.into());
        }
        let guard = and(&guards);
        let out = self
            .native
            .guarded_group(&agent.pane_id, &guard, &commands, "masil-agent-stale")
            .await
            .map_err(group_error)?;
        if String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line == "masil-agent-stale")
        {
            return Ok(Guarded::Rejected);
        }
        Ok(Guarded::Applied)
    }

    /// Read pane options exactly as a guard format sees them.
    async fn pane_values(&self, pane: &str, options: &[&str]) -> Result<Vec<String>, String> {
        crate::pane_id(pane)?;
        let format = options
            .iter()
            .map(|option| format!("#{{{option}}}"))
            .collect::<Vec<_>>()
            .join("\t");
        let output = self
            .command(&["display-message", "-p", "-t", pane, &format])
            .await
            .map_err(server_unreachable)?;
        let values: Vec<String> = output
            .strip_suffix('\n')
            .unwrap_or(&output)
            .split('\t')
            .map(str::to_owned)
            .collect();
        if values.len() != options.len() {
            return Err("invalid native option values".into());
        }
        Ok(values)
    }

    /// Set pane options only if each `(option, expected)` still holds. The
    /// values are hex or nonces, so they are literal inside the guard format.
    /// `false` means another writer changed one of them first.
    async fn compare_and_set(
        &self,
        pane: &str,
        expected: &[(&str, &str)],
        values: &[(&str, String)],
    ) -> Result<bool, String> {
        let guard = and(&expected
            .iter()
            .map(|(option, value)| format!("#{{==:#{{{option}}},{value}}}"))
            .collect::<Vec<_>>());
        let commands = values
            .iter()
            .map(|(option, value)| {
                vec![
                    "set-option".into(),
                    "-p".into(),
                    "-t".into(),
                    pane.into(),
                    (*option).into(),
                    value.clone(),
                ]
            })
            .collect::<Vec<_>>();
        let output = self
            .native
            .guarded_group(pane, &guard, &commands, "masil-agent-stale")
            .await
            .map_err(server_unreachable)?;
        Ok(!String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line == "masil-agent-stale"))
    }

    /// Open the operation store and check that it is the one this server
    /// used before. A deleted store is not silently replaced: its receipts
    /// are what prevents a repeated effect.
    async fn operation_store(&self) -> Result<operations::Store, String> {
        let recorded = self
            .command(&["show-options", "-gqv", STORE_OPTION])
            .await?
            .trim()
            .to_owned();
        self.operation_store_recorded(&recorded).await
    }

    /// As `operation_store`, with the server's recorded instance already read
    /// as part of another format query (saves one native command).
    async fn operation_store_recorded(&self, recorded: &str) -> Result<operations::Store, String> {
        let store = operations::Store::open(&self.native.socket)?;
        let instance = store.instance()?.to_string();
        if recorded.is_empty() {
            self.command(&["set-option", "-g", STORE_OPTION, &instance])
                .await?;
        } else if recorded != instance {
            return Err("store_replaced: the operation store for this server was deleted or replaced while the server kept running, so earlier receipts are gone; inspect the agents, then run `masil-agent agent operations adopt`".into());
        }
        let mut store = store;
        self.maintain(&mut store).await?;
        Ok(store)
    }

    /// The session a resume would reopen. A conflicting binding is refused:
    /// it could be a child provider's session rather than this agent's.
    pub(crate) fn resume_session<'a>(&self, agent: &'a Agent) -> Result<&'a str, String> {
        if agent.binding.state == "conflict" {
            return Err(format!(
                "binding_conflict: native session binding is in conflict (bound {}, also reported {}); start a new agent with --session to choose the conversation",
                agent.binding.session_id.as_deref().unwrap_or("none"),
                agent
                    .binding
                    .conflicting_session
                    .as_deref()
                    .or(agent.binding.requested_session.as_deref())
                    .unwrap_or("unknown")
            ));
        }
        agent
            .session_id
            .as_deref()
            .ok_or_else(|| "no native session reference was reported".into())
    }

    fn option(agent: &Agent, key: &str, value: String) -> Vec<String> {
        vec![
            "set-option".into(),
            "-p".into(),
            "-t".into(),
            agent.pane_id.clone(),
            key.into(),
            value,
        ]
    }

    pub async fn rename(&self, agent: &Agent, name: &str) -> Result<(), String> {
        let _lock = self.lock()?;
        self.unique_name(name, Some(&agent.pane_id)).await?;
        let mut metadata = agent.metadata.clone().unwrap_or_else(|| Metadata {
            provider: agent.provider.clone(),
            boot: agent.boot.clone(),
            generation: agent.generation.clone(),
            run: agent.run.clone(),
            foreground_group: agent.foreground_group,
            ..Metadata::default()
        });
        metadata.name = name.into();
        self.guarded(agent, vec![Self::option(agent, META, encode(&metadata)?)])
            .await
    }

    async fn unique_name(&self, name: &str, except: Option<&str>) -> Result<(), String> {
        if !valid_name(name) {
            return Err("name must begin with a lowercase letter and use 1–32 lowercase letters, digits, '-' or '_'".into());
        }
        if self
            .list()
            .await?
            .iter()
            .any(|a| a.name == name && Some(a.pane_id.as_str()) != except)
        {
            return Err("agent name already exists".into());
        }
        Ok(())
    }

    pub async fn start(
        &self,
        name: &str,
        provider: &str,
        cwd: &Path,
        args: &[String],
        session: Option<&str>,
        split: Option<&str>,
    ) -> Result<Value, String> {
        self.start_with_operation(name, provider, cwd, args, session, split, None)
            .await
    }

    /// Checks that must pass before a launch is admitted. Callers hold the lock.
    async fn prepare_launch(
        &self,
        name: &str,
        provider: &str,
        cwd: &Path,
        args: &[String],
        session: Option<&str>,
        split: Option<&str>,
    ) -> Result<PreparedLaunch, String> {
        self.unique_name(name, None).await?;
        let provider =
            providers::find(provider).ok_or("unknown_provider: unknown agent provider")?;
        let cwd = cwd
            .canonicalize()
            .map_err(|e| format!("working directory: {e}"))?;
        if !cwd.is_dir() {
            return Err("working directory is not a directory".into());
        }
        validate_args(args)?;
        if let Some(target) = split {
            crate::pane_id(target).map_err(|error| format!("unknown_target_syntax: {error}"))?;
        }
        let mut argv = if let Some(session) = session {
            providers::resume(provider.id, session)?
        } else {
            vec![provider.command.into()]
        };
        argv.extend_from_slice(args);
        let executable = executable(&argv[0])?;
        argv[0] = executable.to_string_lossy().into_owned();
        // Codex's shared daemon runs every session with the first TUI's
        // environment, so hooks would name the wrong pane.
        if provider.id == "codex"
            && providers::codex_daemon_flag_applies(&argv[1..])
            && providers::codex_no_daemon(&executable)
        {
            argv.insert(1, "--no-daemon".into());
        }
        Ok(PreparedLaunch {
            provider,
            cwd,
            argv,
            args: args.to_vec(),
        })
    }

    /// Create the pane with `run` as its identity and register it.
    async fn launch(
        &self,
        prepared: PreparedLaunch,
        name: &str,
        run: &str,
        session: Option<&str>,
        split: Option<&str>,
    ) -> Result<Value, String> {
        let PreparedLaunch {
            provider,
            cwd,
            argv,
            args,
        } = prepared;
        let mut command: Vec<OsString> = if let Some(target) = split {
            vec![
                "split-window".into(),
                "-h".into(),
                "-t".into(),
                target.into(),
            ]
        } else {
            vec!["new-window".into(), "-n".into(), name.into()]
        };
        command.extend([
            "-d".into(),
            "-P".into(),
            "-F".into(),
            "#{pane_id}\t#{masil_core_boot_id}\t#{masil_pty_generation}\t#{pane_pid}".into(),
            "-c".into(),
            format_literal_path(&cwd),
        ]);
        for (key, value) in [
            ("MASIL_AGENT_RUN", run.to_owned()),
            (
                "MASIL_AGENT_SOCKET",
                self.native.socket.to_string_lossy().into_owned(),
            ),
            (
                "MASIL_AGENT_BIN",
                std::env::current_exe()
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned(),
            ),
        ] {
            command.extend(["-e".into(), format!("{key}={value}").into()]);
        }
        command.push("--".into());
        command.extend([
            std::env::current_exe()
                .map_err(|e| e.to_string())?
                .as_os_str()
                .to_owned(),
            "agent".into(),
            "--socket".into(),
            self.native.socket.as_os_str().to_owned(),
            "exec-managed".into(),
        ]);
        command.extend(argv.iter().map(OsString::from));
        let result = self
            .native
            .tmux(command, None)
            .await
            .map_err(server_unreachable)?;
        self.finish_launch(result, provider, name, run, session, argv, args)
            .await
            .map_err(String::from)
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_launch(
        &self,
        result: native_ui::ProcessOutput,
        provider: &'static providers::Provider,
        name: &str,
        run: &str,
        session: Option<&str>,
        argv: Vec<String>,
        args: Vec<String>,
    ) -> Result<Value, failure::AfterEffect> {
        let line = String::from_utf8(result.stdout)
            .map_err(|_| failure::AfterEffect::new("invalid launch result"))?;
        let fields: Vec<_> = line.trim().split('\t').collect();
        if fields.len() != 4 {
            return Err(failure::AfterEffect::new(
                "launch outcome unknown; inspect panes before retrying",
            ));
        }
        let metadata = Metadata {
            name: name.into(),
            provider: provider.id.into(),
            boot: fields[1].into(),
            generation: fields[2].into(),
            run: run.into(),
            argv,
            session: session.map(str::to_owned),
            report: None,
            last_sequence: 0,
            foreground_group: fields[3]
                .parse()
                .map_err(|_| failure::AfterEffect::new("invalid launch process identity"))?,
            original_args: args,
        };
        let encoded = encode(&metadata).map_err(failure::AfterEffect::new)?;
        let mut registration = vec!["set-option", "-p", "-t", fields[0], META, &encoded];
        let evidence = match session {
            Some(session) => Some(
                encode(&RunEvidence {
                    run: run.into(),
                    binding: Some(Binding {
                        session: session.into(),
                        source: BindingSource::Requested,
                        sequence: 0,
                        at: now_ms(),
                        event: None,
                        requested: None,
                        conflict: None,
                    }),
                    ..RunEvidence::default()
                })
                .map_err(failure::AfterEffect::new)?,
            ),
            None => None,
        };
        if let Some(evidence) = &evidence {
            registration.extend([";", "set-option", "-p", "-t", fields[0], EVIDENCE, evidence]);
        }
        self.command(&registration).await.map_err(|e| {
            failure::AfterEffect::new(format!(
                "pane {} was launched, but registration failed: {e}; do not blindly retry",
                fields[0]
            ))
        })?;
        Ok(
            json!({"stage":"process_started","pane_id":fields[0],"run":run,"name":name,"provider":provider.id,"native_session_requested":session,"native_session_verified":false,"provider_accepted":false}),
        )
    }

    pub async fn acknowledge(&self, agent: &Agent) -> Result<(), String> {
        if agent.state != "blocked" && !agent.returned_idle {
            return Err("agent has no observed attention request".into());
        }
        let mut tracked = agent.tracked.clone();
        tracked.seen = true;
        self.guarded(agent, vec![Self::option(agent, TRACKED, encode(&tracked)?)])
            .await?;
        self.inbox_apply(vec![inbox::Effect::AckRun {
            run: agent.run.clone(),
            revision: i64::try_from(agent.tracked.revision).unwrap_or(i64::MAX),
        }])
        .await;
        Ok(())
    }

    pub async fn focus(&self, agent: &Agent) -> Result<(), String> {
        self.focus_from(agent, None).await
    }

    pub async fn focus_from(&self, agent: &Agent, origin: Option<&str>) -> Result<(), String> {
        self.native
            .navigate(
                &agent.boot,
                &agent.pane_id,
                &agent.generation,
                origin,
                &self.native.socket,
            )
            .await
            .map_err(|error| {
                if error.contains("identity") || error.contains("stale") {
                    format!("identity_mismatch: {error}")
                } else {
                    server_unreachable(error)
                }
            })
            .map(|_| ())
    }

    pub async fn keys(&self, agent: &Agent, keys: &[String]) -> Result<Value, String> {
        validate_args(keys)?;
        if agent.process != "running" {
            return Err("identity_mismatch: agent is not verified in the foreground".into());
        }
        let mut args = vec![
            "send-keys".into(),
            "-t".into(),
            agent.pane_id.clone(),
            "--".into(),
        ];
        args.extend_from_slice(keys);
        self.guarded_input(agent, vec![args]).await?;
        Ok(
            json!({"stage":"keys_delivered","pane_id":agent.pane_id,"run":agent.run,"provider_accepted":false}),
        )
    }

    pub async fn draft(&self, agent: &Agent, text: &str) -> Result<Value, String> {
        if text.is_empty() || text.len() > 32_768 || text.contains('\0') {
            return Err("invalid_argument: draft must contain 1–32768 bytes without NUL".into());
        }
        // Buffer preparation cannot type into a shell or answer an approval prompt.
        self.native
            .tmux(
                ["load-buffer", "-b", "masil-agent-draft", "-"]
                    .iter()
                    .map(OsString::from),
                Some(text.as_bytes().to_vec()),
            )
            .await
            .map_err(server_unreachable)?;
        Ok(
            json!({"stage":"draft_prepared","buffer":"masil-agent-draft","pane_id":agent.pane_id,"run":agent.run,"submitted":false}),
        )
    }

    pub async fn report(
        &self,
        pane: &str,
        run: &str,
        sequence: u64,
        state: &str,
        session: Option<&str>,
    ) -> Result<ReportResult, String> {
        let agent = self.get(pane).await?;
        if agent.run != run {
            return Err("identity_mismatch: stale agent report".into());
        }
        self.report_snapshot(&agent, sequence, state, session, ReportOrigin::Run)
            .await
    }

    pub(crate) async fn report_snapshot(
        &self,
        agent: &Agent,
        sequence: u64,
        state: &str,
        session: Option<&str>,
        origin: ReportOrigin<'_>,
    ) -> Result<ReportResult, String> {
        if !["idle", "working", "blocked", "unknown"].contains(&state) {
            return Err("invalid agent state".into());
        }
        self.report_inner(agent, sequence, Some(state), session, origin)
            .await
    }

    pub(crate) async fn report_identity_snapshot(
        &self,
        agent: &Agent,
        sequence: u64,
        session: &str,
        origin: ReportOrigin<'_>,
    ) -> Result<ReportResult, String> {
        self.report_inner(agent, sequence, None, Some(session), origin)
            .await
    }

    async fn report_inner(
        &self,
        agent: &Agent,
        sequence: u64,
        state: Option<&str>,
        session: Option<&str>,
        origin: ReportOrigin<'_>,
    ) -> Result<ReportResult, String> {
        let mut metadata = agent.metadata.clone().ok_or("agent is not managed")?;
        if metadata.run != agent.run || agent.process != "running" {
            return Err("identity_mismatch: stale agent report".into());
        }
        if sequence <= metadata.last_sequence
            || metadata
                .report
                .as_ref()
                .is_some_and(|r| sequence <= r.sequence)
        {
            return Err("report sequence did not advance".into());
        }
        let mut evidence = agent
            .run_evidence
            .clone()
            .filter(|e| e.run == metadata.run)
            .unwrap_or_else(|| RunEvidence {
                run: metadata.run.clone(),
                ..RunEvidence::default()
            });
        let now = now_ms();
        let mut binding = None;
        if let Some(session) = session {
            if session.is_empty() || session.len() > 512 || session.chars().any(char::is_control) {
                return Err("invalid session reference".into());
            }
            binding = Some(apply_binding(
                &mut metadata,
                &mut evidence,
                session,
                sequence,
                now,
                origin,
            ));
        }
        metadata.last_sequence = sequence;
        // State from a report that named another session is not this run's.
        let state = state.filter(|_| binding != Some(BindingOutcome::Contradicted));
        if let Some(state) = state {
            metadata.report = Some(Report {
                sequence,
                state: state.into(),
                at: now,
            });
            evidence.report = Some(ReportSource {
                sequence,
                source: match origin {
                    ReportOrigin::Run => REPORT_SOURCE_RUN,
                    ReportOrigin::Callback { .. } => REPORT_SOURCE_CALLBACK,
                }
                .into(),
            });
        }
        if let ReportOrigin::Callback { event, .. } = origin {
            let seen = evidence.integration.get_or_insert_with(Default::default);
            seen.lifecycle |= state.is_some();
            seen.session |= session.is_some();
            seen.sequence = sequence;
            seen.event = bounded_event(event).unwrap_or_default();
            seen.at = now;
        }
        self.guarded_input(
            agent,
            vec![
                Self::option(agent, META, encode(&metadata)?),
                Self::option(agent, EVIDENCE, encode(&evidence)?),
            ],
        )
        .await?;
        Ok(ReportResult {
            binding,
            state_applied: state.is_some(),
        })
    }

    pub async fn close(&self, agent: &Agent) -> Result<(), String> {
        self.close_with_operation(agent, None).await.map(|_| ())
    }

    fn lock(&self) -> Result<std::fs::File, String> {
        use std::os::unix::fs::MetadataExt;
        let parent = self.native.socket.parent().ok_or("socket has no parent")?;
        let name = self
            .native
            .socket
            .file_name()
            .ok_or("socket has no filename")?
            .to_string_lossy();
        let path = parent.join(format!(".{name}.managed.lock"));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| format!("agent management lock: {e}"))?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err("agent lock must be a private owner file".into());
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(LOCK_BUSY.into());
        }
        Ok(file)
    }

    /// As `lock`, waiting up to `limit` for another operation to finish, so
    /// an interrupt is not refused just because a prompt is being delivered.
    async fn lock_within(&self, limit: std::time::Duration) -> Result<std::fs::File, String> {
        let deadline = std::time::Instant::now() + limit;
        loop {
            match self.lock() {
                Err(error) if error == LOCK_BUSY && std::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                result => return result,
            }
        }
    }
}

/// Inbox effects of one tracked change seen on screen: the run's open
/// requests resolve when it leaves `blocked` or ends, and a new `blocked`
/// or a return to idle is an event.
fn screen_effects(
    provider: &str,
    pane: &str,
    previous: &Tracked,
    tracked: &Tracked,
    dead: bool,
) -> Vec<inbox::Effect> {
    let mut effects = Vec::new();
    let run = tracked.run.clone();
    if !previous.run.is_empty() && previous.run != run {
        effects.push(inbox::Effect::ResolveRun {
            run: previous.run.clone(),
            kinds: inbox::ALL,
            resolution: "run_ended",
        });
    } else if previous.state == "blocked" && tracked.state != "blocked" {
        effects.push(inbox::Effect::ResolveRun {
            run: run.clone(),
            kinds: inbox::WAITING,
            resolution: "left_blocked",
        });
    }
    let event = |kind: &'static str| inbox::Effect::Event {
        source: "screen".into(),
        source_ref: format!("{run}:{}", tracked.revision),
        provider: provider.into(),
        pane: pane.into(),
        run: run.clone(),
        revision: i64::try_from(tracked.revision).ok(),
        kind,
        native_ref: None,
        summary: None,
    };
    if dead || tracked.state == "exited" {
        effects.push(inbox::Effect::ResolveRun {
            run: run.clone(),
            kinds: inbox::ALL,
            resolution: "run_ended",
        });
    } else if tracked.state == "blocked" {
        effects.push(event("blocked"));
    } else if tracked.returned_idle && tracked.state == "idle" {
        effects.push(event("returned_idle"));
    }
    effects
}

const LOCK_BUSY: &str = "lock_busy: another agent management operation is in progress";

/// Status of the durable operation store for a server socket, without
/// creating it. `None` when no management command has used one yet.
pub(crate) fn operation_store_status(socket: &Path) -> Result<Option<Value>, String> {
    operations::Store::inspect(socket)
}

/// The state directory this process uses for operation stores, from its own
/// environment: absolute XDG_STATE_HOME, else HOME/.local/state.
pub(crate) fn state_base() -> Result<std::path::PathBuf, String> {
    operations::state_base()
}

/// Coordinator features switched on for `socket` in the stores under `base`.
pub(crate) fn coordinator_features(base: &Path, socket: &Path) -> Result<Vec<String>, String> {
    operations::Store::enabled_features(base, socket)
}

impl Agent {
    /// The desk's binding label. Rows from an endpoint without run evidence
    /// keep the configured-but-unverified wording.
    fn binding_label(&self) -> &'static str {
        match self.binding.state.as_str() {
            "reported" => "managed_reported",
            "requested" => "managed_requested",
            "conflict" => "managed_conflict",
            "none" => "managed_none",
            _ => "explicit_unverified",
        }
    }

    pub fn projection(&self) -> Value {
        let mut projection = json!({"id":self.id,"source_id":self.provider,"session_id":self.session_id.as_deref().unwrap_or("unverified"),"pane_id":self.pane_id,
            "native":{"exists":self.process=="running","activity":if ["idle","working"].contains(&self.state.as_str()){self.state.as_str()}else{"unknown"},"attention":if self.state=="blocked"{"needs_input"}else if self.returned_idle{"returned_idle"}else{"none"},"permission_count":0,"question_count":0,"pending_count":u8::from(self.state=="blocked"||self.returned_idle),"freshness":"fresh","observed_at_ms":now_ms()},
            "core":{"process":self.process,"pty_generation":self.generation,"freshness":"fresh"},"binding":self.binding_label(),"frontend_verified":false,
            "attention":{"revision":self.revision,"acknowledged":self.seen,"pending":self.state=="blocked"||self.returned_idle,"available":self.state=="blocked"||self.returned_idle},
            "capabilities":{"read":true,"input":false,"approval":false,"completion":false,"child_aggregation":false}});
        if !self.endpoint_label.is_empty() {
            projection["source_id"] = json!(format!("{} / {}", self.endpoint_label, self.provider));
        }
        if self.stale {
            projection["native"]["freshness"] = json!("stale");
            projection["core"]["freshness"] = json!("stale");
            projection["core"]["process"] = json!("unknown");
        }
        projection
    }
}

/// Static contract for a provider before any run supplies evidence.
fn provider_contract(engine: &Engine, id: &str) -> Result<Value, String> {
    let provider = providers::find(id).ok_or("unknown_provider: unknown agent provider")?;
    let integration = integration::target_capability(provider.id);
    let resume = providers::resume(provider.id, "example").is_ok();
    Ok(json!({
        "provider": provider.id,
        "command": provider.command,
        "aliases": provider.aliases,
        "contract": 1,
        "detection": if engine.has_manifest(provider.id) { "screen_manifest" } else { "none" },
        "resume": resume,
        "integration": integration,
        "state_authority": match integration {
            Some("full_lifecycle") => "native_callback_when_reported",
            _ if engine.has_manifest(provider.id) => "screen_detection",
            _ => "process_only",
        },
        "session_binding": match (provider.id, integration, resume) {
            ("opencode" | "kilo", Some(_), _) => "frontend_callback",
            (_, Some(_), _) => "native_callback",
            (_, None, true) => "requested_only",
            _ => "none",
        },
        "prompt_submit": "guarded_paste",
        "provider_ack": false,
        "interrupt": "key_delivery",
        "approval_response": false,
        "question_response": false,
        "turn_identity": false,
    }))
}

const REPORT_SOURCE_RUN: &str = "run_report";
const REPORT_SOURCE_CALLBACK: &str = "native_callback";

/// Result of one accepted report.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReportResult {
    pub binding: Option<BindingOutcome>,
    pub state_applied: bool,
}

fn bounded_event(event: &str) -> Option<String> {
    (!event.is_empty() && event.len() <= 64 && !event.chars().any(char::is_control))
        .then(|| event.to_owned())
}

/// Record who reported `session` for this run. The first report may replace a
/// resume request (kept visible as a conflict). Afterwards only a switch event
/// changes the session; any other different session is recorded as a conflict
/// and the bound session stays, because a child provider process in the pane
/// holds the same run identity.
fn apply_binding(
    metadata: &mut Metadata,
    evidence: &mut RunEvidence,
    session: &str,
    sequence: u64,
    now: u64,
    origin: ReportOrigin<'_>,
) -> BindingOutcome {
    let (source, event, switch) = match origin {
        ReportOrigin::Run => (BindingSource::RunReport, None, false),
        ReportOrigin::Callback {
            event,
            frontend,
            switch,
        } => (
            if frontend {
                BindingSource::FrontendCallback
            } else {
                BindingSource::NativeCallback
            },
            bounded_event(event),
            switch,
        ),
    };
    let fresh = |requested: Option<String>| Binding {
        session: session.into(),
        source,
        sequence,
        at: now,
        event: event.clone(),
        requested,
        conflict: None,
    };
    let current = metadata.session.clone();
    // Evidence only describes the session it was recorded for.
    let bound = evidence
        .binding
        .take()
        .filter(|binding| current.as_deref() == Some(binding.session.as_str()));
    let (binding, outcome) = match (current.as_deref(), bound) {
        (None, _) => (fresh(None), BindingOutcome::Established),
        (Some(current), Some(mut binding)) if current == session => {
            if binding.source == BindingSource::Requested {
                // A report confirms the request; the conflict record stays.
                let conflict = binding.conflict.take();
                (
                    Binding {
                        conflict,
                        ..fresh(None)
                    },
                    BindingOutcome::Confirmed,
                )
            } else {
                if source.rank() > binding.source.rank() {
                    binding.source = source;
                    binding.sequence = sequence;
                    binding.at = now;
                    binding.event = event.clone();
                }
                (binding, BindingOutcome::Confirmed)
            }
        }
        (Some(current), None) if current == session => (fresh(None), BindingOutcome::Confirmed),
        (
            Some(current),
            Some(Binding {
                source: BindingSource::Requested,
                conflict: None,
                ..
            }),
        ) => (fresh(Some(current.into())), BindingOutcome::ReplacedRequest),
        (Some(_), _) if switch => (fresh(None), BindingOutcome::Switched),
        (Some(current), bound) => {
            let mut binding = bound.unwrap_or_else(|| Binding {
                session: current.into(),
                source: BindingSource::RunReport,
                sequence: 0,
                at: 0,
                event: None,
                requested: None,
                conflict: None,
            });
            binding.conflict = Some(Contradiction {
                session: session.into(),
                source,
                sequence,
                event,
            });
            evidence.binding = Some(binding);
            return BindingOutcome::Contradicted;
        }
    };
    metadata.session = Some(binding.session.clone());
    evidence.binding = Some(binding);
    outcome
}

fn binding_view(metadata: Option<&Metadata>, evidence: Option<&RunEvidence>) -> BindingView {
    let Some(session) = metadata.and_then(|m| m.session.clone()) else {
        return BindingView {
            state: "none".into(),
            ..BindingView::default()
        };
    };
    let Some(binding) = evidence
        .and_then(|e| e.binding.as_ref())
        .filter(|binding| binding.session == session)
    else {
        // Written by a binary that predates run evidence: origin unknown.
        return BindingView {
            state: "unverified".into(),
            session_id: Some(session),
            ..BindingView::default()
        };
    };
    let state = if binding.requested.is_some() || binding.conflict.is_some() {
        "conflict"
    } else if binding.source == BindingSource::Requested {
        "requested"
    } else {
        "reported"
    };
    BindingView {
        state: state.into(),
        session_id: Some(session),
        source: (binding.sequence > 0 || binding.source == BindingSource::Requested)
            .then(|| binding.source.label().into()),
        requested_session: binding.requested.clone(),
        conflicting_session: binding.conflict.as_ref().map(|c| c.session.clone()),
        event: binding.event.clone(),
        sequence: binding.sequence,
        observed_at_ms: binding.at,
    }
}

fn capability_view(
    provider: &str,
    has_manifest: bool,
    evidence: Option<&RunEvidence>,
    process: &str,
    report_authority: Option<&str>,
    binding: &BindingView,
) -> CapabilityView {
    let running = process == "running";
    let seen = evidence.and_then(|e| e.integration.as_ref());
    let state_authority = match report_authority {
        _ if !running => "process_only",
        Some(source) => source,
        None if has_manifest => "screen_detection",
        None => "process_only",
    };
    let session_identity = match (&binding.source, &binding.session_id) {
        (Some(source), _) => source.as_str(),
        (None, Some(_)) => "unverified",
        (None, None) => "none",
    };
    CapabilityView {
        contract: 1,
        state_authority: state_authority.into(),
        session_identity: session_identity.into(),
        prompt_submit: if running {
            "guarded_paste"
        } else {
            "unavailable"
        }
        .into(),
        provider_ack: false,
        interrupt: if running {
            "key_delivery"
        } else {
            "unavailable"
        }
        .into(),
        approval_response: false,
        question_response: false,
        turn_identity: false,
        resume: binding.session_id.is_some()
            && binding.state != "conflict"
            && providers::resume(provider, "example").is_ok(),
        integration: integration::target_capability(provider)
            .unwrap_or("none")
            .into(),
        callbacks_seen: seen.is_some(),
        lifecycle_seen: seen.is_some_and(|seen| seen.lifecycle),
    }
}

fn run_report_evidence(evidence: Arc<Evidence>, report: &Report) -> Arc<Evidence> {
    Arc::new(Evidence::report_overlay(
        &evidence,
        &report.state,
        report.sequence,
        report.at,
    ))
}

/// tmux expands formats in a `-c` directory; doubling `#` keeps it literal.
fn format_literal_path(path: &Path) -> OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let mut bytes = Vec::with_capacity(path.as_os_str().len());
    for &byte in path.as_os_str().as_bytes() {
        bytes.push(byte);
        if byte == b'#' {
            bytes.push(b'#');
        }
    }
    OsString::from_vec(bytes)
}

fn valid_boot(boot: &str) -> Result<&str, String> {
    if boot.len() != 36
        || !boot.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return Err("invalid native server identity".into());
    }
    Ok(boot)
}

fn valid_name(name: &str) -> bool {
    name.len() <= 32
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn and(guards: &[String]) -> String {
    guards
        .iter()
        .rev()
        .cloned()
        .reduce(|tail, head| format!("#{{&&:{head},{tail}}}"))
        .unwrap_or_else(|| "0".into())
}

fn identity_guard(agent: &Agent) -> String {
    and(&[
        format!("#{{==:#{{masil_core_boot_id}},{}}}", agent.boot),
        format!("#{{==:#{{masil_pty_generation}},{}}}", agent.generation),
        format!("#{{==:#{{{META}}},{}}}", agent.encoded),
        format!("#{{==:#{{{TRACKED}}},{}}}", agent.tracked_encoded),
        format!(
            "#{{==:#{{masil_foreground_pgid}},{}}}",
            if agent.foreground_group > 0 {
                agent.foreground_group.to_string()
            } else {
                String::new()
            }
        ),
    ])
}

fn validate_args(args: &[String]) -> Result<(), String> {
    if args.len() > 64
        || args.iter().map(String::len).sum::<usize>() > 8192
        || args.iter().any(|a| a.chars().any(char::is_control))
    {
        return Err(
            "invalid_argument: arguments exceed bounds or contain control characters".into(),
        );
    }
    Ok(())
}

fn executable(command: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(command))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| format!("agent executable is not installed: {command}"))
}

fn nonce() -> Result<String, String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| e.to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn encode<T: Serialize>(value: &T) -> Result<String, String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() > 8192 {
        return Err("agent metadata exceeds limit".into());
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn decode<T: for<'a> Deserialize<'a>>(value: &str) -> Option<T> {
    if value.len() > 16384 || !value.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = value
        .as_bytes()
        .chunks_exact(2)
        .map(|p| {
            std::str::from_utf8(p)
                .ok()
                .and_then(|s| u8::from_str_radix(s, 16).ok())
        })
        .collect();
    serde_json::from_slice(&bytes?).ok()
}

/// Decode tmux's `q:` escaping without interpreting any text as shell code.
fn records(text: &str) -> Result<Vec<Vec<String>>, String> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut value = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => value.push(chars.next().ok_or("truncated native escaping")?),
            '\t' => row.push(std::mem::take(&mut value)),
            '\n' => {
                row.push(std::mem::take(&mut value));
                rows.push(std::mem::take(&mut row));
            }
            _ => value.push(ch),
        }
    }
    if !value.is_empty() || !row.is_empty() {
        return Err("truncated native inventory".into());
    }
    Ok(rows)
}

fn validate_inventory(inventory: &[Vec<String>]) -> Result<(), String> {
    for fields in inventory {
        if fields.len() != 17 || crate::pane_id(&fields[0]).is_err() {
            return Err("invalid native pane inventory".into());
        }
        if [&fields[11], &fields[12], &fields[16]]
            .iter()
            .any(|value| value.len() > 16384 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err("invalid agent metadata encoding".into());
        }
        if fields[5].len() != 36
            || !fields[5]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
            || fields[6].parse::<u64>().is_err()
        {
            return Err(
                "native core identity unavailable; rebuild and restart this masil server".into(),
            );
        }
    }
    Ok(())
}

fn observation_key(fields: &[String], provider: &str) -> String {
    json!([
        fields[5], fields[6], fields[13], fields[14], provider, fields[7], fields[9], fields[15]
    ])
    .to_string()
}

fn capture_identity(fields: &[String], provider: &str) -> String {
    json!([fields[5], fields[6], fields[13], provider, fields[7]]).to_string()
}

fn capture_guard(fields: &[String]) -> String {
    and(&[
        format!("#{{==:#{{masil_core_boot_id}},{}}}", fields[5]),
        format!("#{{==:#{{masil_pty_generation}},{}}}", fields[6]),
        format!("#{{==:#{{masil_foreground_pgid}},{}}}", fields[13]),
    ])
}

fn parse_capture_frames(output: &[u8], frames: &[(String, String, String)]) -> Option<Vec<String>> {
    let mut cursor = 0;
    let mut screens = Vec::with_capacity(frames.len());
    for (begin, end, rejected) in frames {
        let begin = format!("{begin}\n");
        let rejected = format!("{rejected}\n");
        if output.get(cursor..)?.starts_with(rejected.as_bytes()) {
            return None;
        }
        if !output.get(cursor..)?.starts_with(begin.as_bytes()) {
            return None;
        }
        cursor += begin.len();
        let end = format!("{end}\n");
        let offset = output
            .get(cursor..)?
            .windows(end.len())
            .position(|window| window == end.as_bytes())?;
        let screen = std::str::from_utf8(output.get(cursor..cursor + offset)?).ok()?;
        screens.push(screen.to_owned());
        cursor += offset + end.len();
    }
    (cursor == output.len()).then_some(screens)
}

async fn identify_foregrounds(
    inventory: &[Vec<String>],
) -> HashMap<String, &'static providers::Provider> {
    let mut identified = HashMap::new();
    let mut runtime = Vec::new();
    for fields in inventory {
        if fields[4] == "1" {
            continue;
        }
        let Ok(group) = fields[13].parse::<i32>() else {
            continue;
        };
        if group <= 0 {
            continue;
        }
        if let Some(provider) = providers::identify(&fields[7]) {
            identified.insert(fields[0].clone(), provider);
        } else if providers::is_runtime(&fields[7]) && valid_tty(&fields[10]) {
            runtime.push((&fields[0], &fields[10], &fields[7], group));
        }
    }
    if runtime.is_empty() {
        return identified;
    }

    let mut ttys = Vec::new();
    for (_, tty, _, _) in &runtime {
        let tty = tty.trim_start_matches("/dev/");
        if !ttys.contains(&tty) {
            ttys.push(tty);
        }
    }
    let tty_argument = ttys.join(",");
    let tty_names = ps_tty_names(&runtime);
    let columns = if tty_names.is_some() {
        "tdev=,pgid=,args="
    } else {
        "tty=,pgid=,args="
    };
    let batch = if tty_argument.len() <= MAX_PS_TTY_ARGUMENT {
        native_ui::run_process(
            std::ffi::OsStr::new("/bin/ps"),
            [
                OsString::from("-t"),
                OsString::from(&tty_argument),
                OsString::from("-o"),
                OsString::from(columns),
            ],
            None,
        )
        .await
        .ok()
        .and_then(|result| String::from_utf8(result.stdout).ok())
        .and_then(|text| parse_ps_inventory(&text, &runtime, tty_names.as_ref()))
    } else {
        None
    };

    if let Some(batch) = batch {
        for (pane, tty, _, group) in runtime {
            if let Some(provider) = batch.get(&(tty.trim_start_matches("/dev/").to_owned(), group))
            {
                identified.insert(pane.clone(), *provider);
            }
        }
    } else {
        for (pane, tty, command, group) in runtime {
            if let Some(provider) = identify_foreground(tty, command, group).await {
                identified.insert(pane.clone(), provider);
            }
        }
    }
    identified
}

// Darwin's symbolic tty formatting resolves each device name separately. Match
// the numeric device instead, without weakening the tty + process-group check.
#[cfg(target_os = "macos")]
fn ps_tty_names(expected: &[(&String, &String, &String, i32)]) -> Option<HashMap<String, String>> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let mut names = HashMap::new();
    let mut seen = HashSet::new();
    for (_, tty, _, _) in expected {
        if !seen.insert(tty.as_str()) {
            continue;
        }
        let metadata = std::fs::metadata(tty).ok()?;
        if !metadata.file_type().is_char_device() {
            return None;
        }
        let device = metadata.rdev() as libc::dev_t;
        let key = format!("{}/{}", libc::major(device), libc::minor(device));
        let name = tty.trim_start_matches("/dev/").to_owned();
        if names.insert(key, name).is_some() {
            // Ambiguous aliases use the original symbolic query.
            return None;
        }
    }
    Some(names)
}

#[cfg(not(target_os = "macos"))]
fn ps_tty_names(_: &[(&String, &String, &String, i32)]) -> Option<HashMap<String, String>> {
    None
}

fn parse_ps_inventory(
    text: &str,
    expected: &[(&String, &String, &String, i32)],
    tty_names: Option<&HashMap<String, String>>,
) -> Option<HashMap<(String, i32), &'static providers::Provider>> {
    let expected: HashSet<_> = expected
        .iter()
        .map(|(_, tty, _, group)| (tty.trim_start_matches("/dev/").to_owned(), *group))
        .collect();
    let requested_ttys: HashSet<_> = expected.iter().map(|(tty, _)| tty.as_str()).collect();
    let mut found = HashMap::new();
    for line in text.lines() {
        let (tty, rest) = line.trim().split_once(char::is_whitespace)?;
        let tty = match tty_names {
            Some(names) => names.get(tty)?.as_str(),
            None => tty,
        };
        let (group, args) = rest.trim_start().split_once(char::is_whitespace)?;
        let group = group.parse::<i32>().ok()?;
        let args = args.trim_start();
        if !requested_ttys.contains(tty) {
            return None;
        }
        let key = (tty.to_owned(), group);
        if expected.contains(&key)
            && let Some(provider) = providers::identify(args)
        {
            found.entry(key).or_insert(provider);
        }
    }
    Some(found)
}

fn valid_tty(tty: &str) -> bool {
    tty.starts_with("/dev/")
        && tty.len() <= 128
        && tty
            .trim_start_matches("/dev/")
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
}

async fn identify_foreground(
    tty: &str,
    command: &str,
    group: i32,
) -> Option<&'static providers::Provider> {
    if group <= 0 {
        return None;
    }
    if let Some(provider) = providers::identify(command) {
        return Some(provider);
    }
    if !providers::is_runtime(command) {
        return None;
    }
    if !valid_tty(tty) {
        return None;
    }
    let result = native_ui::run_process(
        std::ffi::OsStr::new("/bin/ps"),
        [
            OsString::from("-t"),
            tty.trim_start_matches("/dev/").into(),
            "-o".into(),
            "pgid=,args=".into(),
        ],
        None,
    )
    .await
    .ok()?;
    let text = String::from_utf8(result.stdout).ok()?;
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let (pgid, args) = line.split_once(char::is_whitespace)?;
            if pgid.parse::<i32>().ok()? != group {
                return None;
            }
            providers::identify(args.trim())
        })
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_escaping_preserves_delimiters_and_never_executes() {
        assert_eq!(
            records("a\\\tb\t\\#\\{x\\}\tline\\\nb\n").unwrap(),
            vec![vec!["a\tb", "#{x}", "line\nb"]]
        );
        assert!(records("unfinished\\").is_err());
        assert!(records("unfinished").is_err());
    }
    #[test]
    fn metadata_roundtrip_and_invalid_limits() {
        let m = Metadata {
            name: "agent".into(),
            ..Metadata::default()
        };
        assert_eq!(
            decode::<Metadata>(&encode(&m).unwrap()).unwrap().name,
            "agent"
        );
        assert!(decode::<Metadata>("a").is_none());
        assert!(!valid_name("a;kill-server"));
        assert!(validate_args(&["hello\nworld".into()]).is_err());
    }

    fn requested(session: &str) -> (Metadata, RunEvidence) {
        (
            Metadata {
                run: "r1".into(),
                session: Some(session.into()),
                ..Metadata::default()
            },
            RunEvidence {
                run: "r1".into(),
                binding: Some(Binding {
                    session: session.into(),
                    source: BindingSource::Requested,
                    sequence: 0,
                    at: 1,
                    event: None,
                    requested: None,
                    conflict: None,
                }),
                ..RunEvidence::default()
            },
        )
    }

    fn callback(event: &str) -> ReportOrigin<'_> {
        ReportOrigin::Callback {
            event,
            frontend: false,
            switch: false,
        }
    }

    fn switch(event: &str) -> ReportOrigin<'_> {
        ReportOrigin::Callback {
            event,
            frontend: false,
            switch: true,
        }
    }

    fn view(metadata: &Metadata, evidence: &RunEvidence) -> BindingView {
        binding_view(Some(metadata), Some(evidence))
    }

    #[test]
    fn a_confirming_callback_reports_a_requested_session() {
        let (mut metadata, mut evidence) = requested("ses-a");
        assert_eq!(view(&metadata, &evidence).state, "requested");
        let outcome = apply_binding(
            &mut metadata,
            &mut evidence,
            "ses-a",
            7,
            10,
            callback("SessionStart"),
        );
        assert_eq!(outcome, BindingOutcome::Confirmed);
        let view = view(&metadata, &evidence);
        assert_eq!(view.state, "reported");
        assert_eq!(view.source.as_deref(), Some("native_callback"));
        assert_eq!(
            (view.sequence, view.event.as_deref()),
            (7, Some("SessionStart"))
        );
        assert_eq!(view.requested_session, None);
    }

    #[test]
    fn the_first_report_replaces_a_request_and_stays_visible_as_a_conflict() {
        let (mut metadata, mut evidence) = requested("ses-a");
        let outcome = apply_binding(
            &mut metadata,
            &mut evidence,
            "ses-b",
            7,
            10,
            callback("SessionStart"),
        );
        assert_eq!(outcome, BindingOutcome::ReplacedRequest);
        assert_eq!(metadata.session.as_deref(), Some("ses-b"));
        let current = view(&metadata, &evidence);
        assert_eq!(current.state, "conflict");
        assert_eq!(current.requested_session.as_deref(), Some("ses-a"));

        // An announced switch clears it; the new session is reported.
        let outcome = apply_binding(
            &mut metadata,
            &mut evidence,
            "ses-c",
            9,
            11,
            switch("SessionStart"),
        );
        assert_eq!(outcome, BindingOutcome::Switched);
        let current = view(&metadata, &evidence);
        assert_eq!(
            (current.state.as_str(), current.session_id.as_deref()),
            ("reported", Some("ses-c"))
        );
    }

    #[test]
    fn a_later_unannounced_session_is_a_sticky_conflict_that_keeps_the_binding() {
        let mut metadata = Metadata {
            run: "r1".into(),
            ..Metadata::default()
        };
        let mut evidence = RunEvidence {
            run: "r1".into(),
            ..RunEvidence::default()
        };
        assert_eq!(
            apply_binding(
                &mut metadata,
                &mut evidence,
                "parent",
                3,
                10,
                callback("SessionStart")
            ),
            BindingOutcome::Established
        );
        // A child provider in a tool call inherits the run identity.
        assert_eq!(
            apply_binding(
                &mut metadata,
                &mut evidence,
                "child",
                4,
                11,
                callback("SessionStart")
            ),
            BindingOutcome::Contradicted
        );
        assert_eq!(metadata.session.as_deref(), Some("parent"));
        let current = view(&metadata, &evidence);
        assert_eq!(current.state, "conflict");
        assert_eq!(current.conflicting_session.as_deref(), Some("child"));
        // Repeating the bound session does not clear the conflict.
        assert_eq!(
            apply_binding(
                &mut metadata,
                &mut evidence,
                "parent",
                5,
                12,
                callback("SessionStart")
            ),
            BindingOutcome::Confirmed
        );
        assert_eq!(view(&metadata, &evidence).state, "conflict");
        // Only an announced switch does.
        apply_binding(
            &mut metadata,
            &mut evidence,
            "resumed",
            6,
            13,
            switch("SessionStart"),
        );
        assert_eq!(view(&metadata, &evidence).state, "reported");
    }

    #[test]
    fn repeated_sessions_keep_the_strongest_evidence() {
        let mut metadata = Metadata::default();
        let mut evidence = RunEvidence::default();
        let frontend = ReportOrigin::Callback {
            event: "chat.message",
            frontend: true,
            switch: false,
        };
        apply_binding(&mut metadata, &mut evidence, "root", 3, 10, frontend);
        apply_binding(
            &mut metadata,
            &mut evidence,
            "root",
            4,
            11,
            callback("session.idle"),
        );
        apply_binding(
            &mut metadata,
            &mut evidence,
            "root",
            5,
            12,
            ReportOrigin::Run,
        );
        let current = view(&metadata, &evidence);
        assert_eq!(current.source.as_deref(), Some("frontend_callback"));
        assert_eq!(current.sequence, 3);

        let mut metadata = Metadata::default();
        let mut evidence = RunEvidence::default();
        apply_binding(
            &mut metadata,
            &mut evidence,
            "ses",
            3,
            10,
            ReportOrigin::Run,
        );
        apply_binding(
            &mut metadata,
            &mut evidence,
            "ses",
            4,
            11,
            callback("SessionStart"),
        );
        assert_eq!(
            view(&metadata, &evidence).source.as_deref(),
            Some("native_callback")
        );
    }

    #[test]
    fn evidence_from_another_run_or_session_is_ignored() {
        assert_eq!(binding_view(None, None).state, "none");
        let legacy = Metadata {
            session: Some("old".into()),
            ..Metadata::default()
        };
        assert_eq!(binding_view(Some(&legacy), None).state, "unverified");
        let (metadata, mut evidence) = requested("ses-a");
        evidence.binding.as_mut().unwrap().session = "other".into();
        assert_eq!(view(&metadata, &evidence).state, "unverified");
        assert_eq!(bounded_event(&"x".repeat(65)), None);
        assert_eq!(bounded_event("bad\nevent"), None);
    }

    #[test]
    fn capabilities_follow_evidence_not_provider_names() {
        let none = binding_view(None, None);
        let screen = capability_view("claude", true, None, "running", None, &none);
        assert_eq!(screen.state_authority, "screen_detection");
        assert_eq!(screen.session_identity, "none");
        assert_eq!(screen.integration, "native_session_only");
        assert!(!screen.callbacks_seen && !screen.resume && !screen.provider_ack);
        assert!(!screen.approval_response && !screen.turn_identity);

        let mut metadata = Metadata::default();
        let mut evidence = RunEvidence::default();
        apply_binding(
            &mut metadata,
            &mut evidence,
            "ses",
            2,
            10,
            callback("session_start"),
        );
        evidence.integration = Some(IntegrationSeen {
            lifecycle: true,
            session: true,
            sequence: 2,
            event: "agent_start".into(),
            at: 10,
        });
        let binding = view(&metadata, &evidence);
        let native = capability_view(
            "pi",
            true,
            Some(&evidence),
            "running",
            Some(REPORT_SOURCE_CALLBACK),
            &binding,
        );
        assert_eq!(native.state_authority, "native_callback");
        assert_eq!(native.session_identity, "native_callback");
        assert!(native.lifecycle_seen && native.callbacks_seen && native.resume);

        let exited = capability_view("pi", true, Some(&evidence), "exited", None, &binding);
        assert_eq!(exited.state_authority, "process_only");
        assert_eq!(exited.prompt_submit, "unavailable");
        assert_eq!(exited.interrupt, "unavailable");

        apply_binding(
            &mut metadata,
            &mut evidence,
            "child",
            3,
            11,
            callback("session_start"),
        );
        let conflict = view(&metadata, &evidence);
        assert!(!capability_view("pi", true, Some(&evidence), "running", None, &conflict).resume);
    }

    #[test]
    fn run_evidence_stays_within_the_option_limit() {
        let (mut metadata, mut evidence) = requested(&"s".repeat(512));
        apply_binding(
            &mut metadata,
            &mut evidence,
            &"t".repeat(512),
            9,
            10,
            callback(&"e".repeat(64)),
        );
        apply_binding(
            &mut metadata,
            &mut evidence,
            &"u".repeat(512),
            10,
            11,
            callback(&"e".repeat(64)),
        );
        evidence.integration = Some(IntegrationSeen {
            event: "e".repeat(64),
            ..IntegrationSeen::default()
        });
        evidence.report = Some(ReportSource {
            sequence: u64::MAX,
            source: REPORT_SOURCE_CALLBACK.into(),
        });
        assert_eq!(
            decode::<RunEvidence>(&encode(&evidence).unwrap()),
            Some(evidence)
        );
    }

    #[test]
    fn metadata_keeps_the_schema_older_binaries_decode() {
        let metadata = Metadata {
            name: "agent".into(),
            session: Some("ses".into()),
            ..Metadata::default()
        };
        let json = serde_json::to_value(&metadata).unwrap();
        let fields: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            fields,
            [
                "argv",
                "boot",
                "foreground_group",
                "generation",
                "last_sequence",
                "name",
                "original_args",
                "provider",
                "report",
                "run",
                "session"
            ]
        );
    }

    #[test]
    fn launch_directories_escape_tmux_formats() {
        assert_eq!(
            format_literal_path(Path::new("/w/#{session_name}/#(x)")),
            OsString::from("/w/##{session_name}/##(x)")
        );
        assert_eq!(
            format_literal_path(Path::new("/plain")),
            OsString::from("/plain")
        );
    }

    #[test]
    fn capture_frames_preserve_multiline_unicode_and_reject_ambiguous_markers() {
        let frames = vec![
            (
                "fresh-0-begin".into(),
                "fresh-0-end".into(),
                "fresh-0-rejected".into(),
            ),
            (
                "fresh-1-begin".into(),
                "fresh-1-end".into(),
                "fresh-1-rejected".into(),
            ),
        ];
        let output = "fresh-0-begin\nfirst\n한국어\nold-separator-like-text\nfresh-0-end\nfresh-1-begin\nsecond\nline\nfresh-1-end\n".as_bytes();
        assert_eq!(
            parse_capture_frames(output, &frames).unwrap(),
            ["first\n한국어\nold-separator-like-text\n", "second\nline\n"]
        );

        let ambiguous = b"fresh-0-begin\ntext\nfresh-0-end\nforged\nfresh-0-end\nfresh-1-begin\nsecond\nfresh-1-end\n";
        assert!(parse_capture_frames(ambiguous, &frames).is_none());
        assert!(parse_capture_frames(b"fresh-0-rejected\n", &frames[..1]).is_none());
    }

    #[test]
    fn batched_ps_matches_only_requested_tty_and_group() {
        let pane = "%1".to_owned();
        let tty = "/dev/ttys001".to_owned();
        let command = "python3.12".to_owned();
        let expected = vec![(&pane, &tty, &command, 42)];
        let parsed = parse_ps_inventory(
            "ttys001  7 /bin/sh\nttys001  42 python3.12 /opt/hermes\n",
            &expected,
            None,
        )
        .unwrap();
        assert_eq!(
            parsed
                .get(&("ttys001".to_owned(), 42))
                .map(|provider| provider.id),
            Some("hermes")
        );
        assert!(
            parse_ps_inventory("ttys999 42 python3.12 /opt/hermes\n", &expected, None).is_none()
        );
    }

    #[test]
    fn numeric_ps_preserves_tty_and_foreground_group_matching() {
        let pane = "%1".to_owned();
        let tty = "/dev/ttys066".to_owned();
        let command = "node".to_owned();
        let expected = vec![(&pane, &tty, &command, 42)];
        let names = HashMap::from([("16/66".to_owned(), "ttys066".to_owned())]);
        let args = "node /opt/codex.js";
        let parsed = parse_ps_inventory(
            &format!("16/66 7 {args}\n16/66 42 {args}\n"),
            &expected,
            Some(&names),
        )
        .unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[&("ttys066".to_owned(), 42)].id, "codex");
        for device in ["16/67", "17/66", "??", "-16/66", "16/-66", "16/66/0"] {
            assert!(
                parse_ps_inventory(&format!("{device} 42 {args}\n"), &expected, Some(&names))
                    .is_none()
            );
        }
        assert!(
            parse_ps_inventory(&format!("16/66 7 {args}\n"), &expected, Some(&names))
                .unwrap()
                .is_empty()
        );
        let foreign = HashMap::from([("16/66".to_owned(), "ttys999".to_owned())]);
        assert!(
            parse_ps_inventory(&format!("16/66 42 {args}\n"), &expected, Some(&foreign)).is_none()
        );
    }

    #[test]
    fn full_cache_replaces_existing_pane_without_evicting_other_snapshots() {
        let mut cache = (0..64)
            .map(|index| {
                (
                    format!("%{index}"),
                    (
                        format!("old-{index}"),
                        Arc::new(Evidence::from_value(json!({"pane":index}))),
                    ),
                )
            })
            .collect::<HashMap<_, _>>();
        let snapshot = cache.clone();
        store_cached_evidence(
            &mut cache,
            "%0".into(),
            "new".into(),
            Arc::new(Evidence::from_value(json!({"pane":0}))),
        );
        store_cached_evidence(
            &mut cache,
            "%64".into(),
            "new".into(),
            Arc::new(Evidence::from_value(json!({"pane":64}))),
        );

        assert_eq!(cache.len(), 64);
        assert_eq!(cache["%0"].0, "new");
        assert!(!cache.contains_key("%64"));
        assert_eq!(snapshot["%0"].0, "old-0");
        assert_eq!(snapshot["%63"].0, cache["%63"].0);
        assert!(Arc::ptr_eq(&snapshot["%63"].1, &cache["%63"].1));
    }

    #[test]
    fn agent_evidence_json_roundtrip_preserves_full_public_shape() {
        let evidence = json!({
            "state": "blocked",
            "source": "screen",
            "explanations": [
                {"rule": "question", "matched": true, "detail": "Proceed?"},
                {"rule": "approval", "matched": false, "detail": null}
            ],
            "visible_blocker": true
        });
        let agent = Agent {
            id: "codex-1".into(),
            name: "codex-1".into(),
            provider: "codex".into(),
            pane_id: "%1".into(),
            window_id: "@1".into(),
            workspace: "main".into(),
            cwd: "/tmp/project".into(),
            boot: "00000000-0000-0000-0000-000000000000".into(),
            generation: "1".into(),
            run: "run-1".into(),
            process: "running".into(),
            state: "blocked".into(),
            session_id: Some("session-1".into()),
            evidence: Arc::new(Evidence::from_value(evidence.clone())),
            seen: false,
            revision: "3".into(),
            returned_idle: false,
            endpoint_id: String::new(),
            endpoint_label: String::new(),
            endpoint_key: String::new(),
            stale: false,
            binding: BindingView::default(),
            capabilities: CapabilityView::default(),
            metadata: None,
            run_evidence: None,
            encoded: String::new(),
            foreground_command: String::new(),
            tracked: Tracked::default(),
            tracked_encoded: String::new(),
            foreground_group: 0,
            output_generation: 0,
            title: String::new(),
            progress: String::new(),
        };

        let serialized = serde_json::to_value(&agent).unwrap();
        assert_eq!(serialized["evidence"], evidence);
        assert!(serialized.get("metadata").is_none());

        let decoded: Agent = serde_json::from_value(serialized.clone()).unwrap();
        assert_eq!(serde_json::to_value(&decoded.evidence).unwrap(), evidence);
        assert_eq!(serde_json::to_value(decoded).unwrap(), serialized);
    }

    #[test]
    fn cached_evidence_survives_report_overlay_and_screen_refresh() {
        let original = Arc::new(Evidence::from_value(json!({
            "state": "blocked",
            "source": "screen",
            "explanations": [{"rule": "question", "detail": "Proceed?"}],
            "visible_blocker": true
        })));
        let mut cache = HashMap::new();
        store_cached_evidence(&mut cache, "%1".into(), "screen-1".into(), original.clone());

        let report = Report {
            sequence: 7,
            state: "blocked".into(),
            at: 1234,
        };
        let reported = run_report_evidence(cache["%1"].1.clone(), &report);
        store_cached_evidence(
            &mut cache,
            "%1".into(),
            "screen-2".into(),
            Arc::new(Evidence::from_value(json!({
                "state": "working",
                "source": "screen",
                "explanations": [{"rule": "activity", "detail": "Generating"}]
            }))),
        );

        let reported = serde_json::to_value(&reported).unwrap();
        assert_eq!(reported["source"], "run_report");
        assert_eq!(reported["screen"], serde_json::to_value(&original).unwrap());
        assert_eq!(
            serde_json::to_value(&original).unwrap()["explanations"],
            json!([{"rule": "question", "detail": "Proceed?"}])
        );
        assert_eq!(cache["%1"].0, "screen-2");
        assert_eq!(cache["%1"].1.state(), "working");
        assert_eq!(
            serde_json::to_value(&cache["%1"].1).unwrap()["explanations"][0]["detail"],
            "Generating"
        );
    }
}
