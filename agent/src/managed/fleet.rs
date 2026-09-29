//! Opt-in aggregation. Each endpoint refreshes independently while a view is open.
use super::{
    Agent, Manager,
    endpoints::{self, Action, Endpoint, Expected, Request},
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::task::{AbortHandle, JoinSet};

#[derive(Clone, Serialize)]
pub(crate) struct EndpointStatus {
    pub id: String,
    pub label: String,
    pub connected: bool,
    pub error: Option<String>,
}
pub(crate) struct FleetSnapshot {
    pub epoch: String,
    pub agents: Vec<Agent>,
    pub endpoints: Vec<EndpointStatus>,
}
struct Cached {
    key: String,
    boot: String,
    agents: Vec<Agent>,
    error: Option<String>,
    updated: Instant,
}
type PollResult = (String, String, Result<(String, Vec<Agent>), String>);
#[derive(Default)]
struct State {
    cache: HashMap<String, Cached>,
    pending: HashMap<String, (String, AbortHandle)>,
    work: JoinSet<PollResult>,
}
pub(crate) struct Fleet {
    pub local: Arc<Manager>,
    state: Mutex<State>,
}
pub(super) fn key(endpoint: &Endpoint) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(endpoint).expect("endpoint serializes"))
    )
}
impl Fleet {
    pub fn new(local: Manager) -> Result<Self, String> {
        endpoints::load()?;
        Ok(Self {
            local: Arc::new(local),
            state: Mutex::new(State::default()),
        })
    }
    pub fn endpoint_ids(&self) -> Result<Vec<String>, String> {
        let mut ids = vec!["local".into()];
        ids.extend(
            endpoints::load()?
                .into_iter()
                .filter(|e| e.enabled)
                .map(|e| e.id),
        );
        Ok(ids)
    }
    pub fn endpoint_key(&self, id: &str) -> Result<String, String> {
        if id == "local" {
            return Ok("local".into());
        }
        let endpoint = endpoints::load()?
            .into_iter()
            .find(|e| e.id == id && e.enabled)
            .ok_or("unknown or disabled endpoint")?;
        Ok(key(&endpoint))
    }
    pub async fn poll(&self) -> Result<FleetSnapshot, String> {
        let endpoints = endpoints::load()?;
        {
            let mut state = self.state.lock().map_err(|_| "fleet state unavailable")?;
            state
                .cache
                .retain(|id, _| id == "local" || endpoints.iter().any(|e| e.id == *id));
            state.pending.retain(|id, (old, handle)| {
                let keep = id == "local"
                    || endpoints
                        .iter()
                        .any(|e| e.id == *id && e.enabled && key(e) == *old);
                if !keep {
                    handle.abort();
                }
                keep
            });
            while let Some(completed) = state.work.try_join_next() {
                let Ok((id, fingerprint, result)) = completed else {
                    continue;
                };
                if id != "local" && !endpoints.iter().any(|e| e.enabled && key(e) == fingerprint) {
                    continue;
                }
                state.pending.remove(&id);
                match result {
                    Ok((boot, agents)) => {
                        state.cache.insert(
                            id,
                            Cached {
                                key: fingerprint,
                                boot,
                                agents,
                                error: None,
                                updated: Instant::now(),
                            },
                        );
                    }
                    Err(error) => {
                        let entry = state.cache.entry(id).or_insert_with(|| Cached {
                            key: fingerprint.clone(),
                            boot: String::new(),
                            agents: Vec::new(),
                            error: None,
                            updated: Instant::now(),
                        });
                        if entry.key != fingerprint {
                            entry.agents.clear();
                            entry.boot.clear();
                            entry.key = fingerprint;
                        }
                        entry.error = Some(error);
                        entry.updated = Instant::now();
                    }
                }
            }
            for endpoint in &endpoints {
                if endpoint.enabled && !state.pending.contains_key(&endpoint.id) {
                    let request_endpoint = endpoint.clone();
                    let handle = state.work.spawn(async move {
                        let result = request_endpoint
                            .call(&Request::List)
                            .await
                            .and_then(parse_inventory);
                        let fingerprint = key(&request_endpoint);
                        (request_endpoint.id, fingerprint, result)
                    });
                    state
                        .pending
                        .insert(endpoint.id.clone(), (key(endpoint), handle));
                }
            }
            if !state.pending.contains_key("local") {
                let local = self.local.clone();
                let handle = state.work.spawn(async move {
                    let result = async {
                        let before = local.boot().await?;
                        let agents = local.list_view().await?;
                        if before != local.boot().await? {
                            return Err("native server restarted during inventory".to_owned());
                        }
                        Ok((before, agents))
                    }
                    .await;
                    ("local".to_owned(), "local".to_owned(), result)
                });
                state
                    .pending
                    .insert("local".into(), ("local".into(), handle));
            }
        }
        let state = self.state.lock().map_err(|_| "fleet state unavailable")?;
        let mut agents = Vec::new();
        let mut statuses = Vec::new();
        let mut epoch = Vec::new();
        let mut sources = vec![(
            "local".to_owned(),
            "Local".to_owned(),
            true,
            "local".to_owned(),
        )];
        sources.extend(
            endpoints
                .iter()
                .map(|e| (e.id.clone(), e.label.clone(), e.enabled, key(e))),
        );
        for (id, label, enabled, fingerprint) in sources {
            let cached = state.cache.get(&id).filter(|c| c.key == fingerprint);
            let error = if !enabled {
                Some("disabled".to_owned())
            } else {
                cached.map_or_else(
                    || Some("connecting".to_owned()),
                    |c| {
                        c.error.clone().or_else(|| {
                            (c.updated.elapsed() > Duration::from_secs(5))
                                .then(|| "refresh overdue".to_owned())
                        })
                    },
                )
            };
            let connected = error.is_none();
            epoch.push(json!([id, fingerprint, cached.map(|c| &c.boot), connected]));
            if let Some(cached) = cached {
                for mut agent in cached.agents.clone() {
                    agent.endpoint_id = id.clone();
                    agent.endpoint_key = fingerprint.clone();
                    agent.stale = !connected;
                    if !endpoints.is_empty() {
                        agent.id = format!("{id}::{}", agent.name);
                        agent.endpoint_label = label.clone();
                    }
                    agents.push(agent);
                }
            }
            statuses.push(EndpointStatus {
                id,
                label,
                connected,
                error,
            });
        }
        Ok(FleetSnapshot {
            epoch: serde_json::to_string(&epoch).map_err(|e| e.to_string())?,
            agents,
            endpoints: statuses,
        })
    }
    fn remote(&self, agent: &Agent) -> Result<Option<Endpoint>, String> {
        if agent.stale {
            return Err("endpoint is unavailable; refresh before acting".into());
        }
        if agent.endpoint_id.is_empty() || agent.endpoint_id == "local" {
            return Ok(None);
        }
        let endpoint = endpoints::load()?
            .into_iter()
            .find(|e| e.id == agent.endpoint_id && e.enabled)
            .ok_or("endpoint was removed or disabled")?;
        if key(&endpoint) != agent.endpoint_key {
            return Err("endpoint connection changed; refresh before acting".into());
        }
        Ok(Some(endpoint))
    }
    async fn action(
        &self,
        endpoint: Endpoint,
        agent: &Agent,
        action: Action,
    ) -> Result<Value, String> {
        endpoint
            .call(&Request::Action {
                expected: Expected::from(agent),
                action,
            })
            .await
    }
    pub async fn read(&self, agent: &Agent, history: bool) -> Result<String, String> {
        if let Some(e) = self.remote(agent)? {
            let value = self.action(e, agent, Action::Read { history }).await?;
            value["text"]
                .as_str()
                .map(str::to_owned)
                .ok_or("invalid remote screen response".into())
        } else {
            self.local.read(agent, history).await
        }
    }
    pub async fn rename(&self, agent: &Agent, name: &str) -> Result<(), String> {
        if let Some(e) = self.remote(agent)? {
            self.action(e, agent, Action::Rename { name: name.into() })
                .await
                .map(|_| ())
        } else {
            self.local.rename(agent, name).await
        }
    }
    pub async fn draft(&self, agent: &Agent, text: &str) -> Result<Value, String> {
        if let Some(e) = self.remote(agent)? {
            self.action(e, agent, Action::Draft { text: text.into() })
                .await
        } else {
            self.local.draft(agent, text).await
        }
    }
    pub async fn prompt(&self, agent: &Agent, text: &str) -> Result<Value, String> {
        if let Some(e) = self.remote(agent)? {
            self.action(
                e,
                agent,
                Action::Prompt {
                    text: text.into(),
                    operation: None,
                },
            )
            .await
        } else {
            self.local.prompt(agent, text).await
        }
    }
    pub async fn keys(&self, agent: &Agent, keys: &[String]) -> Result<Value, String> {
        if let Some(e) = self.remote(agent)? {
            self.action(
                e,
                agent,
                Action::Keys {
                    keys: keys.to_vec(),
                },
            )
            .await
        } else {
            self.local.keys(agent, keys).await
        }
    }
    pub async fn acknowledge(&self, agent: &Agent) -> Result<(), String> {
        if let Some(e) = self.remote(agent)? {
            self.action(e, agent, Action::Ack).await.map(|_| ())
        } else {
            self.local.acknowledge(agent).await
        }
    }
    pub async fn close(&self, agent: &Agent) -> Result<(), String> {
        if let Some(e) = self.remote(agent)? {
            self.action(e, agent, Action::Close).await.map(|_| ())
        } else {
            self.local.close(agent).await
        }
    }
    pub async fn resume(&self, agent: &Agent, name: &str) -> Result<Value, String> {
        if let Some(e) = self.remote(agent)? {
            self.action(e, agent, Action::Resume { name: name.into() })
                .await
        } else {
            let current = self.local.get(&agent.pane_id).await?;
            if current.run != agent.run
                || current.boot != agent.boot
                || current.session_id != agent.session_id
            {
                return Err("agent session changed before resume".into());
            }
            self.local
                .start(
                    name,
                    &agent.provider,
                    Path::new(&agent.cwd),
                    &[],
                    Some(
                        agent
                            .session_id
                            .as_deref()
                            .ok_or("no native session reference")?,
                    ),
                    None,
                )
                .await
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn start_at(
        &self,
        endpoint: &str,
        expected_key: &str,
        name: &str,
        provider: &str,
        cwd: &Path,
        args: &[String],
        session: Option<&str>,
        split: Option<&str>,
    ) -> Result<Value, String> {
        if endpoint == "local" {
            if expected_key != "local" {
                return Err("local endpoint identity changed".into());
            }
            return self
                .local
                .start(name, provider, cwd, args, session, split)
                .await;
        }
        let e = endpoints::load()?
            .into_iter()
            .find(|e| e.id == endpoint && e.enabled)
            .ok_or("unknown or disabled endpoint")?;
        if key(&e) != expected_key {
            return Err("endpoint connection changed since the start dialog opened".into());
        }
        e.call(&Request::Start {
            name: name.into(),
            provider: provider.into(),
            cwd: cwd.to_string_lossy().into_owned(),
            args: args.to_vec(),
            session: session.map(str::to_owned),
            split: split.map(str::to_owned),
        })
        .await
    }
    pub async fn focus_from(&self, agent: &Agent, origin: Option<&str>) -> Result<(), String> {
        let Some(endpoint) = self.remote(agent)? else {
            return self.local.focus_from(agent, origin).await;
        };
        // Revalidate before creating a connection window; no remote process is started by polling.
        self.action(endpoint.clone(), agent, Action::Read { history: false })
            .await?;
        let _lock = self.local.lock()?;
        let marker = format!(
            "{:x}",
            Sha256::digest(format!(
                "{}:{}:{}:{}",
                key(&endpoint),
                agent.boot,
                agent.pane_id,
                agent.run
            ))
        );
        let existing = self
            .local
            .command(&[
                "list-panes",
                "-a",
                "-F",
                "#{pane_id}\t#{rmux_pty_generation}\t#{pane_dead}\t#{@rmux-agent-connection}",
            ])
            .await?;
        let mut target = None;
        for line in existing.lines() {
            let f: Vec<_> = line.split('\t').collect();
            if f.len() != 4 || f[2] != "0" {
                continue;
            }
            let parts: Vec<_> = f[3].split(':').collect();
            if parts.len() == 3 && parts[0] == marker && parts[1] == f[1] {
                self.action(
                    endpoint.clone(),
                    agent,
                    Action::ConnectionStatus {
                        lease: parts[2].into(),
                    },
                )
                .await?;
                target = Some((f[0].to_owned(), f[1].to_owned()));
                break;
            }
        }
        let boot = self.local.boot().await?;
        if target.is_none() {
            let executable = std::env::current_exe().map_err(|e| e.to_string())?;
            let fingerprint = key(&endpoint);
            let lease = super::nonce()?;
            let mut args = vec![
                "new-window".to_owned(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}\t#{rmux_pty_generation}".into(),
                "-n".into(),
                format!("{}:{}", endpoint.id, agent.name),
            ];
            let config_root = std::env::var_os("XDG_CONFIG_HOME")
                .map(std::path::PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(std::path::PathBuf::from)
                        .filter(|p| p.is_absolute())
                        .map(|p| p.join(".config"))
                })
                .ok_or("endpoint configuration root unavailable")?;
            args.extend([
                "-e".into(),
                format!(
                    "XDG_CONFIG_HOME={}",
                    config_root
                        .to_str()
                        .ok_or("endpoint configuration root is not UTF-8")?
                ),
            ]);
            for name in ["HOME", "PATH", "SSH_AUTH_SOCK"] {
                if let Ok(value) = std::env::var(name) {
                    args.extend(["-e".into(), format!("{name}={value}")]);
                } else if name == "SSH_AUTH_SOCK" {
                    args.extend(["-e".into(), "SSH_AUTH_SOCK=".into()]);
                }
            }
            args.extend([
                "--".into(),
                executable.to_string_lossy().into_owned(),
                "agent".into(),
                "endpoint-connect".into(),
                endpoint.id.clone(),
                fingerprint,
                agent.pane_id.clone(),
                agent.boot.clone(),
                agent.generation.clone(),
                agent.run.clone(),
                lease.clone(),
            ]);
            let output = self
                .local
                .command(&args.iter().map(String::as_str).collect::<Vec<_>>())
                .await?;
            let fields: Vec<_> = output.trim().split('\t').collect();
            if fields.len() != 2 {
                return Err("connection window outcome unknown; inspect local panes".into());
            }
            self.local
                .command(&[
                    "set-option",
                    "-p",
                    "-t",
                    fields[0],
                    "@rmux-agent-connection",
                    &format!("{marker}:{}:{lease}", fields[1]),
                ])
                .await?;
            let verified = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match self
                        .action(
                            endpoint.clone(),
                            agent,
                            Action::ConnectionStatus {
                                lease: lease.clone(),
                            },
                        )
                        .await
                    {
                        Ok(_) => return Ok::<(), String>(()),
                        Err(error) if error.contains("starting or no longer exists") => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        Err(error) => return Err(error),
                    }
                }
            })
            .await
            .map_err(|_| {
                "remote connection was not confirmed; inspect its local connection window"
                    .to_owned()
            })?;
            verified?;
            target = Some((fields[0].into(), fields[1].into()));
        }
        let (pane, generation) = target.ok_or("missing connection pane")?;
        self.local
            .native
            .navigate(&boot, &pane, &generation, origin, &self.local.native.socket)
            .await
            .map(|_| ())
    }
}
fn parse_inventory(value: Value) -> Result<(String, Vec<Agent>), String> {
    let boot = value["boot"]
        .as_str()
        .filter(|s| s.len() == 36)
        .ok_or("invalid endpoint boot identity")?
        .to_owned();
    let agents: Vec<Agent> = serde_json::from_value(value["agents"].clone())
        .map_err(|e| format!("invalid endpoint inventory: {e}"))?;
    if agents.len() > 64
        || agents.iter().any(|a| {
            a.boot != boot
                || crate::pane_id(&a.pane_id).is_err()
                || a.run.is_empty()
                || a.run.len() > 128
        })
    {
        return Err("invalid endpoint agent identities".into());
    }
    Ok((boot, agents))
}
