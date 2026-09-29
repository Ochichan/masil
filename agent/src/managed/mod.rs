//! Native agent management. Nothing runs until a management command or view is opened.
mod cli;
pub(crate) mod endpoints;
pub(crate) mod fleet;
mod integration;
mod prompt;
mod remote_cli;
mod store;
mod view;

use crate::{detection::Engine, native_ui, observation::now_ms, providers};
pub(crate) use cli::run;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const META: &str = "@masil-managed-agent";
const TRACKED: &str = "@masil-managed-observation";
const FORMAT: &str = "#{q:pane_id}\t#{q:window_id}\t#{q:session_name}\t#{q:pane_pid}\t#{q:pane_dead}\t#{q:masil_core_boot_id}\t#{q:masil_pty_generation}\t#{q:pane_current_command}\t#{q:pane_current_path}\t#{q:pane_title}\t#{q:pane_tty}\t#{q:@masil-managed-agent}\t#{q:@masil-managed-observation}\t#{q:masil_foreground_pgid}\t#{q:pane_output_generation}\t#{q:masil_osc_progress}";
const CAPTURE_BATCH_SIZE: usize = 12;
const MAX_PS_TTY_ARGUMENT: usize = 4096;

struct CaptureRequest {
    pane: String,
    identity: String,
    guard: String,
}

enum PreparedEvidence {
    Cached(Value),
    Screen(String),
}

fn store_cached_evidence(
    cache: &mut HashMap<String, (String, Value)>,
    pane: String,
    key: String,
    evidence: Value,
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
    pub evidence: Value,
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
    #[serde(skip)]
    metadata: Option<Metadata>,
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
    cache: Mutex<HashMap<String, (String, Value)>>,
}

impl Manager {
    pub fn new(socket: PathBuf, client: Option<String>) -> Result<Self, String> {
        let socket = socket
            .canonicalize()
            .map_err(|e| format!("native socket: {e}"))?;
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
            .await?;
        String::from_utf8(result.stdout).map_err(|_| "native response is not UTF-8".into())
    }

    pub async fn boot(&self) -> Result<String, String> {
        let boot = self
            .command(&["display-message", "-p", "#{masil_core_boot_id}"])
            .await?
            .trim()
            .to_owned();
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
        if requests.iter().any(|request| {
            current_by_pane
                .get(request.pane.as_str())
                .map_or(true, |identity| identity != &request.identity)
        }) {
            return Err("native pane identity changed during observation".into());
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
        let output = self.native.guarded_groups(&groups).await?;
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
                        let evidence = self.engine.explain_with_progress(
                            provider.id,
                            &screen,
                            &fields[9],
                            &fields[15],
                        );
                        let mut cache = self
                            .cache
                            .lock()
                            .map_err(|_| "detection cache unavailable")?;
                        store_cached_evidence(&mut cache, fields[0].clone(), key, evidence.clone());
                        evidence
                    }
                }
            } else {
                json!({"state":"unknown", "source":"process", "reason": if dead {"process_exited"} else {"foreground_not_verified"}})
            };
            let mut state = evidence["state"].as_str().unwrap_or("unknown").to_owned();
            let metadata = metadata.filter(|m| {
                m.provider == provider.id && (dead || m.foreground_group == foreground_group)
            });
            if let Some(report) = metadata.as_ref().and_then(|m| m.report.as_ref())
                && foreground
                && now_ms().saturating_sub(report.at) <= 30_000
            {
                // A visible blocker is stronger than a hook claiming idle/working.
                if evidence["visible_blocker"] != true || report.state == "blocked" {
                    state = report.state.clone();
                    evidence = json!({"state":state,"source":"run_report","sequence":report.sequence,"observed_at_ms":report.at,"screen":evidence});
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
            if evidence["skip_state_update"] == true && previous.run == run {
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
                previous
            };
            let revision = tracked.revision.to_string();
            let name = metadata
                .as_ref()
                .map(|m| m.name.clone())
                .unwrap_or_else(|| {
                    format!("{}-{}", provider.id, fields[0].trim_start_matches('%'))
                });
            let mut agent = Agent {
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
                process: if dead {
                    "exited"
                } else if foreground {
                    "running"
                } else {
                    "unknown"
                }
                .into(),
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
                metadata,
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
                self.guarded(&agent, vec![Self::option(&agent, TRACKED, encoded.clone())])
                    .await?;
                agent.tracked_encoded = encoded;
            }
            agents.push(agent);
        }
        Ok(agents)
    }

    pub async fn get(&self, target: &str) -> Result<Agent, String> {
        let mut matches = self
            .collect(Some(target))
            .await?
            .into_iter()
            .filter(|a| a.pane_id == target || a.name == target);
        let agent = matches.next().ok_or("agent target not found")?;
        if matches.next().is_some() {
            return Err("ambiguous agent name; use a pane ID".into());
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
            .await?;
        let text = String::from_utf8(output.stdout).map_err(|_| "native output is not UTF-8")?;
        if text.trim() == "masil-agent-stale" {
            return Err("agent run changed before reading".into());
        }
        Ok(text)
    }

    async fn guarded(&self, agent: &Agent, commands: Vec<Vec<String>>) -> Result<(), String> {
        let guard = identity_guard(agent);
        let out = self
            .native
            .guarded_script(&agent.pane_id, &guard, &commands, "masil-agent-stale")
            .await?;
        if String::from_utf8_lossy(&out.stdout).contains("masil-agent-stale") {
            return Err("agent run changed before the action".into());
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
            .await?;
        if String::from_utf8_lossy(&out.stdout).contains("masil-agent-stale") {
            return Err("agent foreground or run changed before input delivery".into());
        }
        Ok(())
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
        let _lock = self.lock()?;
        self.unique_name(name, None).await?;
        let provider = providers::find(provider).ok_or("unknown agent provider")?;
        let cwd = cwd
            .canonicalize()
            .map_err(|e| format!("working directory: {e}"))?;
        if !cwd.is_dir() {
            return Err("working directory is not a directory".into());
        }
        validate_args(args)?;
        let mut argv = if let Some(session) = session {
            providers::resume(provider.id, session)?
        } else {
            vec![provider.command.into()]
        };
        argv.extend_from_slice(args);
        let executable = executable(&argv[0])?;
        argv[0] = executable.to_string_lossy().into_owned();
        let run = nonce()?;
        let mut command: Vec<OsString> = if let Some(target) = split {
            crate::pane_id(target)?;
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
            cwd.as_os_str().into(),
        ]);
        for (key, value) in [
            ("MASIL_AGENT_RUN", run.clone()),
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
        let result = self.native.tmux(command, None).await?;
        let line = String::from_utf8(result.stdout).map_err(|_| "invalid launch result")?;
        let fields: Vec<_> = line.trim().split('\t').collect();
        if fields.len() != 4 {
            return Err("launch outcome unknown; inspect panes before retrying".into());
        }
        let metadata = Metadata {
            name: name.into(),
            provider: provider.id.into(),
            boot: fields[1].into(),
            generation: fields[2].into(),
            run: run.clone(),
            argv,
            session: session.map(str::to_owned),
            report: None,
            last_sequence: 0,
            foreground_group: fields[3]
                .parse()
                .map_err(|_| "invalid launch process identity")?,
            original_args: args.to_vec(),
        };
        self.command(&[
            "set-option",
            "-p",
            "-t",
            fields[0],
            META,
            &encode(&metadata)?,
        ])
        .await
        .map_err(|e| {
            format!(
                "pane {} was launched, but registration failed: {e}; do not blindly retry",
                fields[0]
            )
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
            .await
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
            .map(|_| ())
    }

    pub async fn keys(&self, agent: &Agent, keys: &[String]) -> Result<Value, String> {
        validate_args(keys)?;
        if agent.process != "running" {
            return Err("agent is not verified in the foreground".into());
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
            return Err("draft must contain 1–32768 bytes without NUL".into());
        }
        // Buffer preparation cannot type into a shell or answer an approval prompt.
        self.native
            .tmux(
                ["load-buffer", "-b", "masil-agent-draft", "-"]
                    .iter()
                    .map(OsString::from),
                Some(text.as_bytes().to_vec()),
            )
            .await?;
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
    ) -> Result<(), String> {
        let agent = self.get(pane).await?;
        if agent.run != run {
            return Err("stale agent report".into());
        }
        self.report_snapshot(&agent, sequence, state, session).await
    }

    pub(crate) async fn report_snapshot(
        &self,
        agent: &Agent,
        sequence: u64,
        state: &str,
        session: Option<&str>,
    ) -> Result<(), String> {
        if !["idle", "working", "blocked", "unknown"].contains(&state) {
            return Err("invalid agent state".into());
        }
        self.report_inner(agent, sequence, Some(state), session)
            .await
    }

    pub(crate) async fn report_identity_snapshot(
        &self,
        agent: &Agent,
        sequence: u64,
        session: &str,
    ) -> Result<(), String> {
        self.report_inner(agent, sequence, None, Some(session))
            .await
    }

    async fn report_inner(
        &self,
        agent: &Agent,
        sequence: u64,
        state: Option<&str>,
        session: Option<&str>,
    ) -> Result<(), String> {
        let mut metadata = agent.metadata.clone().ok_or("agent is not managed")?;
        if metadata.run != agent.run || agent.process != "running" {
            return Err("stale agent report".into());
        }
        if sequence <= metadata.last_sequence
            || metadata
                .report
                .as_ref()
                .is_some_and(|r| sequence <= r.sequence)
        {
            return Err("report sequence did not advance".into());
        }
        if let Some(session) = session {
            if session.is_empty() || session.len() > 512 || session.chars().any(char::is_control) {
                return Err("invalid session reference".into());
            }
            metadata.session = Some(session.into());
        }
        metadata.last_sequence = sequence;
        if let Some(state) = state {
            metadata.report = Some(Report {
                sequence,
                state: state.into(),
                at: now_ms(),
            });
        }
        self.guarded_input(agent, vec![Self::option(agent, META, encode(&metadata)?)])
            .await
    }

    pub async fn close(&self, agent: &Agent) -> Result<(), String> {
        self.guarded(
            agent,
            vec![vec!["kill-pane".into(), "-t".into(), agent.pane_id.clone()]],
        )
        .await
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
            return Err("another agent management operation is in progress".into());
        }
        Ok(file)
    }
}

impl Agent {
    pub fn projection(&self) -> Value {
        let mut projection = json!({"id":self.id,"source_id":self.provider,"session_id":self.session_id.as_deref().unwrap_or("unverified"),"pane_id":self.pane_id,
            "native":{"exists":self.process=="running","activity":if ["idle","working"].contains(&self.state.as_str()){self.state.as_str()}else{"unknown"},"attention":if self.state=="blocked"{"needs_input"}else if self.returned_idle{"returned_idle"}else{"none"},"permission_count":0,"question_count":0,"pending_count":u8::from(self.state=="blocked"||self.returned_idle),"freshness":"fresh","observed_at_ms":now_ms()},
            "core":{"process":self.process,"pty_generation":self.generation,"freshness":"fresh"},"binding":"explicit_unverified","frontend_verified":false,
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
        return Err("arguments exceed bounds or contain control characters".into());
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
        if fields.len() != 16 || crate::pane_id(&fields[0]).is_err() {
            return Err("invalid native pane inventory".into());
        }
        if [&fields[11], &fields[12]]
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
    let batch = if tty_argument.len() <= MAX_PS_TTY_ARGUMENT {
        native_ui::run_process(
            std::ffi::OsStr::new("/bin/ps"),
            [
                OsString::from("-t"),
                OsString::from(&tty_argument),
                OsString::from("-o"),
                OsString::from("tty=,pgid=,args="),
            ],
            None,
        )
        .await
        .ok()
        .and_then(|result| String::from_utf8(result.stdout).ok())
        .and_then(|text| parse_ps_inventory(&text, &runtime))
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

fn parse_ps_inventory(
    text: &str,
    expected: &[(&String, &String, &String, i32)],
) -> Option<HashMap<(String, i32), &'static providers::Provider>> {
    let expected: HashSet<_> = expected
        .iter()
        .map(|(_, tty, _, group)| (tty.trim_start_matches("/dev/").to_owned(), *group))
        .collect();
    let requested_ttys: HashSet<_> = expected.iter().map(|(tty, _)| tty.as_str()).collect();
    let mut found = HashMap::new();
    for line in text.lines() {
        let (tty, rest) = line.trim().split_once(char::is_whitespace)?;
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
        )
        .unwrap();
        assert_eq!(
            parsed
                .get(&("ttys001".to_owned(), 42))
                .map(|provider| provider.id),
            Some("hermes")
        );
        assert!(parse_ps_inventory("ttys999 42 python3.12 /opt/hermes\n", &expected).is_none());
    }

    #[test]
    fn full_cache_replaces_existing_pane_without_evicting_other_snapshots() {
        let mut cache = (0..64)
            .map(|index| {
                (
                    format!("%{index}"),
                    (format!("old-{index}"), json!({"pane":index})),
                )
            })
            .collect::<HashMap<_, _>>();
        let snapshot = cache.clone();
        store_cached_evidence(&mut cache, "%0".into(), "new".into(), json!({"pane":0}));
        store_cached_evidence(&mut cache, "%64".into(), "new".into(), json!({"pane":64}));

        assert_eq!(cache.len(), 64);
        assert_eq!(cache["%0"].0, "new");
        assert!(!cache.contains_key("%64"));
        assert_eq!(snapshot["%0"].0, "old-0");
        assert_eq!(snapshot["%63"], cache["%63"]);
    }
}
