//! Attention events written to the durable inbox by the processes that see
//! them: integration hooks, and management commands that move an agent's
//! tracked state. Writing is best-effort and never changes a command's
//! result. The inbox is switched on per store; the server option
//! `@masil-inbox` caches that switch so that, while it is off, no process
//! opens the store.

use super::Manager;
use super::operations::{InboxEvent, Store, Through};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// What one producer saw.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Effect {
    Event {
        source: String,
        source_ref: String,
        provider: String,
        pane: String,
        run: String,
        revision: Option<i64>,
        kind: &'static str,
        native_ref: Option<String>,
        summary: Option<Value>,
    },
    /// A request with this key was answered, rejected or replaced.
    Resolve {
        source: String,
        source_ref: String,
        resolution: &'static str,
    },
    /// The person acknowledged the run; events they cannot have seen yet
    /// stay unread.
    AckRun { run: String, through: Through },
    /// The run's open events of these kinds no longer need attention.
    ResolveRun {
        run: String,
        kinds: &'static [&'static str],
        resolution: &'static str,
    },
    /// The run's open request events from `source`, recorded before
    /// `before`, whose request is not in `present`: the provider no longer
    /// waits for them.
    ResolveAbsent {
        run: String,
        source: String,
        present: Vec<String>,
        before: u64,
        resolution: &'static str,
    },
}

/// Kinds that wait for the person until resolved.
pub(super) const WAITING: &[&str] = &["blocked", "approval_requested", "question_asked"];
/// Kinds that report a finished turn; the next turn resolves them.
pub(super) const TURN_ENDS: &[&str] = &["turn_completed", "returned_idle", "error"];
pub(super) const ALL: &[&str] = &[
    "blocked",
    "approval_requested",
    "question_asked",
    "turn_completed",
    "returned_idle",
    "error",
    "observation_lost",
];

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Appends one line to a small log in the state directory; inbox writes
/// must not print, since a hook's output belongs to its provider. A
/// long-lived process logs each error code at most once a minute.
pub(super) fn log(name: &str, value: &Value) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    const LIMIT: u64 = 64 * 1024;
    static LAST: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> =
        std::sync::OnceLock::new();
    if cfg!(test) {
        return;
    }
    let code = value["error"]
        .as_str()
        .map(|error| error.split(':').next().unwrap_or(error).to_owned())
        .unwrap_or_default();
    if let Ok(mut last) = LAST.get_or_init(Default::default).lock() {
        let now = now_ms();
        if last
            .get(&code)
            .is_some_and(|at| now.saturating_sub(*at) < 60_000)
        {
            return;
        }
        last.insert(code, now);
    }
    let Ok(base) = super::operations::state_base() else {
        return;
    };
    let directory = base.join("masil");
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join(name);
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > LIMIT) {
        let _ = std::fs::rename(&path, directory.join(format!("{name}.1")));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
    {
        let mut line = value.clone();
        line["at_ms"] = json!(now_ms());
        let _ = writeln!(file, "{line}");
    }
}

/// Applies every effect, past failures, so one pane's failed event does
/// not cost another pane its resolution; returns the first error.
fn apply(store: &mut Store, effects: &[Effect]) -> Result<(), String> {
    let now = now_ms();
    let mut first_error = None;
    for effect in effects {
        let applied = match effect {
            Effect::Event {
                source,
                source_ref,
                provider,
                pane,
                run,
                revision,
                kind,
                native_ref,
                summary,
            } => store
                .record_event(&InboxEvent {
                    source,
                    source_ref,
                    provider,
                    pane,
                    run,
                    revision: *revision,
                    kind,
                    native_ref: native_ref.as_deref(),
                    summary: summary.clone(),
                    observed_ms: now,
                })
                .map(drop),
            Effect::Resolve {
                source,
                source_ref,
                resolution,
            } => store.resolve_event(source, source_ref, resolution, now),
            Effect::ResolveRun {
                run,
                kinds,
                resolution,
            } => store.resolve_run(run, kinds, resolution, now).map(drop),
            Effect::AckRun { run, through } => store.ack_run(run, *through, now).map(drop),
            Effect::ResolveAbsent {
                run,
                source,
                present,
                before,
                resolution,
            } => store
                .resolve_absent(run, source, present, *before, resolution, now)
                .map(drop),
        };
        if let Err(error) = applied {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

impl Manager {
    /// The store instance to write to when the inbox is on. After a server
    /// start the server knows neither the switch nor the store, so the
    /// first producer fills both from the existing store.
    pub(super) async fn inbox_instance(&self) -> Option<String> {
        let text = self
            .command(&[
                "display-message",
                "-p",
                "#{@masil-inbox}\u{1f}#{@masil-operation-store}",
            ])
            .await
            .ok()?;
        let (flag, instance) = text.trim_end_matches('\n').split_once('\u{1f}')?;
        match flag {
            "on" if !instance.is_empty() => return Some(instance.to_owned()),
            "on" | "" => {}
            _ => return None,
        }
        let socket = self.native.socket.clone();
        let probe = tokio::task::spawn_blocking(move || Store::inbox_probe(&socket))
            .await
            .ok()
            .flatten();
        let enabled = probe.as_ref().is_some_and(|(enabled, _)| *enabled);
        // -o leaves a value another process set meanwhile.
        if flag.is_empty() {
            let _ = self
                .command(&[
                    "set-option",
                    "-gqo",
                    "@masil-inbox",
                    if enabled { "on" } else { "off" },
                ])
                .await;
        }
        let (true, stored) = probe? else {
            return None;
        };
        if instance.is_empty() {
            // As `operation_store` does on first use: the server adopts the
            // store that exists. Opening it checks the instance again.
            let _ = self
                .command(&["set-option", "-gqo", super::STORE_OPTION, &stored])
                .await;
            return Some(stored);
        }
        Some(instance.to_owned())
    }

    /// Writes the effects to the inbox if it is on, then nudges a running
    /// coordinator. Failures are logged and otherwise ignored.
    pub(super) async fn inbox_apply(&self, effects: Vec<Effect>) {
        if effects.is_empty() {
            return;
        }
        // The coordinator keeps its connection and is the one poked.
        if self.is_resident() {
            if let Err(error) = self
                .with_resident_store(move |store| apply(store, &effects))
                .await
            {
                log("inbox.log", &json!({"error": error}));
            }
            return;
        }
        let Some(instance) = self.inbox_instance().await else {
            return;
        };
        let socket: PathBuf = self.native.socket.clone();
        let written = tokio::task::spawn_blocking(move || {
            let mut store = Store::open_existing(&socket, &instance)?
                .ok_or("the operation store is missing or replaced")?;
            apply(&mut store, &effects)
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string()));
        match written {
            Ok(()) => crate::coordinator::poke(&self.native.socket),
            Err(error) => log("inbox.log", &json!({"error": error})),
        }
    }
}

/// One inbox event as the management window lists it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InboxItem {
    pub id: i64,
    pub seq: i64,
    pub source: String,
    pub pane: String,
    pub run: String,
    pub revision: Option<i64>,
    pub kind: String,
    pub summary: Option<Value>,
    pub observed_ms: u64,
    pub read: bool,
    pub resolution: Option<String>,
}

/// The inbox as the management window shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct InboxView {
    pub enabled: bool,
    pub unseen: i64,
    pub events: Vec<InboxItem>,
}

impl InboxView {
    fn from_value(value: &Value) -> Self {
        let events = value["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|event| {
                Some(InboxItem {
                    id: event["id"].as_i64()?,
                    seq: event["seq"].as_i64()?,
                    source: event["source"].as_str()?.to_owned(),
                    pane: event["pane"].as_str()?.to_owned(),
                    run: event["run"].as_str()?.to_owned(),
                    revision: event["revision"].as_i64(),
                    kind: event["kind"].as_str()?.to_owned(),
                    summary: event
                        .get("summary")
                        .filter(|value| !value.is_null())
                        .cloned(),
                    observed_ms: event["observed_ms"].as_u64()?,
                    read: event["read"].as_bool().unwrap_or(false),
                    resolution: event["resolution"].as_str().map(str::to_owned),
                })
            })
            .collect();
        Self {
            enabled: value["enabled"].as_bool().unwrap_or(false),
            unseen: value["unseen"].as_i64().unwrap_or(0),
            events,
        }
    }
}

impl Manager {
    /// The server's cached inbox switch; one native command, no store.
    pub(crate) async fn inbox_switch(&self) -> bool {
        match self
            .command(&["display-message", "-p", "#{@masil-inbox}"])
            .await
            .as_deref()
            .map(str::trim)
        {
            Ok("on") => true,
            // Unknown after a server start: the store decides and fills it.
            Ok("") => self.inbox_instance().await.is_some(),
            _ => false,
        }
    }

    /// The inbox read-only, for the management window: unseen events, or
    /// the most recent ones when `all`.
    pub(crate) async fn inbox_view(&self, all: bool, limit: usize) -> Result<InboxView, String> {
        let socket = self.native.socket.clone();
        let value = tokio::task::spawn_blocking(move || read(&socket, all, limit))
            .await
            .map_err(|error| error.to_string())??;
        Ok(InboxView::from_value(&value))
    }

    /// Marks events read, or everything through `through_seq`, in the store
    /// the server recorded. Never creates a store; nudges a running
    /// coordinator to recount its badge.
    pub(crate) async fn inbox_mark_read(
        &self,
        ids: Vec<i64>,
        through_seq: Option<i64>,
    ) -> Result<(), String> {
        let instance = self
            .inbox_instance()
            .await
            .ok_or("inbox_off: the inbox is off or has no store")?;
        let socket = self.native.socket.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = Store::open_existing(&socket, &instance)?
                .ok_or("store_replaced: the operation store is missing or replaced")?;
            if !ids.is_empty() {
                store.ack_events(&ids, now_ms())?;
            }
            if let Some(through) = through_seq {
                store.read_all(Some(through))?;
            }
            Ok::<_, String>(())
        })
        .await
        .map_err(|error| error.to_string())??;
        crate::coordinator::recount(&self.native.socket);
        Ok(())
    }
}

/// Switches the inbox on or off. Enable sets the cached switch first and
/// disable clears the store's switch first, so a failure part way leaves
/// the cache on, which only costs a store open per event.
///
/// The coordinator watches while the inbox is on: enable starts it, and
/// both tell a running one to read its features again. A coordinator that
/// does not start leaves the inbox on; commands still write their events.
pub(super) async fn set_enabled(manager: &Manager, enabled: bool) -> Result<Value, String> {
    // The store must be the server's: another state directory would switch
    // an inbox that no producer reads.
    let socket = manager.native.socket.clone();
    tokio::task::spawn_blocking(move || crate::coordinator::check_state(&socket))
        .await
        .map_err(|error| error.to_string())??;
    if enabled {
        manager
            .command(&["set-option", "-gq", "@masil-inbox", "on"])
            .await?;
    }
    let mut store = manager.operation_store().await?;
    store.set_inbox_enabled(enabled)?;
    if !enabled {
        // The badge goes with the switch, even with no coordinator to clear
        // it.
        manager
            .command(&[
                "set-option",
                "-gq",
                "@masil-inbox",
                "off",
                ";",
                "set-option",
                "-gqu",
                crate::coordinator::BADGE_OPTION,
            ])
            .await?;
    }
    let socket = manager.native.socket.clone();
    let coordinator = tokio::task::spawn_blocking(move || {
        if !enabled {
            crate::coordinator::reload(&socket);
            return None;
        }
        Some(match crate::coordinator::ensure(&socket) {
            Ok(_) => {
                crate::coordinator::reload(&socket);
                "running".to_owned()
            }
            Err(error) => format!("error: {error}"),
        })
    })
    .await
    .map_err(|error| error.to_string())?;
    let mut value = json!({"inbox": if enabled { "on" } else { "off" }});
    if let Some(coordinator) = coordinator {
        value["coordinator"] = json!(coordinator);
    }
    Ok(value)
}

/// The inbox read-only: an absent store or table is an empty inbox.
pub(super) fn read(socket: &Path, all: bool, limit: usize) -> Result<Value, String> {
    match Store::inbox_readonly(socket, all, limit)? {
        Some(value) => Ok(value),
        None => Ok(json!({"events": [], "fence": 0, "dropped": 0, "enabled": false})),
    }
}

fn usage() -> String {
    "usage: masil-agent agent inbox [list] [--all] [--limit N] | ack ID... | read-all [--through SEQ] | enable | disable | status".into()
}

/// `agent inbox ...`. Listing and status read the store without creating
/// it; ack and read-all write through the recorded store.
pub(super) async fn command(manager: &Manager, args: &[String]) -> Result<Value, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] | ["list", ..] | ["--all", ..] | ["--limit", ..] => {
            let rest = if words.first() == Some(&"list") {
                &words[1..]
            } else {
                &words[..]
            };
            let mut all = false;
            let mut limit = 100usize;
            let mut index = 0;
            while index < rest.len() {
                match rest[index] {
                    "--all" => {
                        all = true;
                        index += 1;
                    }
                    "--limit" => {
                        limit = rest
                            .get(index + 1)
                            .and_then(|value| value.parse().ok())
                            .filter(|limit| (1..=1000).contains(limit))
                            .ok_or("invalid_argument: --limit must be 1-1000")?;
                        index += 2;
                    }
                    _ => return Err(usage()),
                }
            }
            let socket = manager.native.socket.clone();
            tokio::task::spawn_blocking(move || read(&socket, all, limit))
                .await
                .map_err(|error| error.to_string())?
        }
        ["ack", ids @ ..] if !ids.is_empty() => {
            let ids = ids
                .iter()
                .map(|id| id.parse::<i64>())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "invalid_argument: event IDs are integers")?;
            let mut store = manager.operation_store().await?;
            let missing = store.ack_events(&ids, now_ms())?;
            crate::coordinator::recount(&manager.native.socket);
            if !missing.is_empty() {
                return Err(format!("target_absent: no inbox event {missing:?}"));
            }
            Ok(json!({"acked": ids}))
        }
        ["read-all"] | ["read-all", "--through", _] => {
            let through = match words.as_slice() {
                [_, _, value] => Some(
                    value
                        .parse::<i64>()
                        .map_err(|_| "invalid_argument: --through takes a sequence number")?,
                ),
                _ => None,
            };
            let mut store = manager.operation_store().await?;
            let fence = store.read_all(through)?;
            crate::coordinator::recount(&manager.native.socket);
            Ok(json!({"fence": fence}))
        }
        ["enable"] => set_enabled(manager, true).await,
        ["disable"] => set_enabled(manager, false).await,
        ["status"] => {
            let socket = manager.native.socket.clone();
            let checked = tokio::task::spawn_blocking(move || Store::inbox_flag_checked(&socket))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            // A store that cannot be read now repairs nothing.
            let readable = checked.is_ok();
            let stored = checked.unwrap_or(false);
            let cached = manager
                .command(&["show-options", "-gqv", "@masil-inbox"])
                .await?
                .trim()
                .to_owned();
            let wanted = if stored { "on" } else { "off" };
            // The store decides; a stale cache is corrected here.
            let repaired = readable && cached != wanted;
            if repaired {
                manager
                    .command(&["set-option", "-gq", "@masil-inbox", wanted])
                    .await?;
            }
            // Also records the store in the server if nothing did yet. A
            // server that names a replaced store records nothing.
            let recording = match manager.inbox_instance().await {
                Some(instance) => {
                    let socket = manager.native.socket.clone();
                    tokio::task::spawn_blocking(move || Store::inbox_probe(&socket))
                        .await
                        .ok()
                        .flatten()
                        .is_some_and(|(enabled, stored)| enabled && stored == instance)
                }
                None => false,
            };
            // The badge shows the store's unseen count, or nothing. A failed
            // read repairs nothing.
            let unseen = if !readable {
                None
            } else if stored {
                manager
                    .inbox_view(false, 1)
                    .await
                    .ok()
                    .map(|view| view.unseen)
            } else {
                Some(0)
            };
            let badge = manager
                .command(&["show-options", "-gqv", crate::coordinator::BADGE_OPTION])
                .await?
                .trim()
                .parse::<i64>()
                .unwrap_or(0);
            let repair = unseen.filter(|unseen| *unseen != badge);
            if let Some(unseen) = repair {
                let value = unseen.to_string();
                let args: &[&str] = if unseen == 0 {
                    &["set-option", "-gqu", crate::coordinator::BADGE_OPTION]
                } else {
                    &[
                        "set-option",
                        "-gq",
                        crate::coordinator::BADGE_OPTION,
                        &value,
                    ]
                };
                manager.command(args).await?;
            }
            let socket = manager.native.socket.clone();
            let coordinator =
                tokio::task::spawn_blocking(move || crate::coordinator::status(&socket))
                    .await
                    .map_err(|error| error.to_string())?
                    .map(|status| {
                        let watch = &status["coordinator"]["watch"];
                        json!({
                            "state": status["state"],
                            "last_tick_ms": watch["last_tick_ms"],
                            "last_error": watch["last_error"],
                        })
                    })
                    .unwrap_or_else(|error| json!({"state": "unknown", "last_error": error}));
            Ok(json!({
                "enabled": stored,
                "cache": cached,
                "repaired": repaired,
                "recording": recording,
                "unseen": unseen,
                "badge_repaired": repair.is_some(),
                "coordinator": coordinator,
            }))
        }
        _ => Err(usage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_kinds_are_attention_kinds() {
        assert!(WAITING.iter().all(|kind| ALL.contains(kind)));
    }
}
