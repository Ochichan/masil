//! Attention events written to the durable inbox by the processes that see
//! them: integration hooks, and management commands that move an agent's
//! tracked state. Writing is best-effort and never changes a command's
//! result. The inbox is switched on per store; the server option
//! `@masil-inbox` caches that switch so that, while it is off, no process
//! opens the store.

use super::Manager;
use super::operations::{InboxEvent, Store};
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
    /// The person acknowledged the run at this tracked revision.
    AckRun { run: String, revision: i64 },
    /// The run's open events of these kinds no longer need attention.
    ResolveRun {
        run: String,
        kinds: &'static [&'static str],
        resolution: &'static str,
    },
}

/// Kinds that wait for the person until resolved.
pub(super) const WAITING: &[&str] = &["blocked", "approval_requested", "question_asked"];
pub(super) const ALL: &[&str] = &[
    "blocked",
    "approval_requested",
    "question_asked",
    "turn_completed",
    "returned_idle",
];

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Appends one line to a small log in the state directory; inbox writes
/// must not print, since a hook's output belongs to its provider.
pub(super) fn log(name: &str, value: &Value) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    const LIMIT: u64 = 64 * 1024;
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

fn apply(store: &mut Store, effects: &[Effect]) -> Result<(), String> {
    let now = now_ms();
    for effect in effects {
        match effect {
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
            } => {
                store.record_event(&InboxEvent {
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
                })?;
            }
            Effect::Resolve {
                source,
                source_ref,
                resolution,
            } => store.resolve_event(source, source_ref, resolution, now)?,
            Effect::ResolveRun {
                run,
                kinds,
                resolution,
            } => {
                store.resolve_run(run, kinds, resolution, now)?;
            }
            Effect::AckRun { run, revision } => {
                store.ack_run(run, *revision, now)?;
            }
        }
    }
    Ok(())
}

impl Manager {
    /// The store instance to write to when the inbox is on, filling the
    /// server's cached switch once after a server start.
    async fn inbox_instance(&self) -> Option<String> {
        let text = self
            .command(&[
                "display-message",
                "-p",
                "#{@masil-inbox}\u{1f}#{@masil-operation-store}",
            ])
            .await
            .ok()?;
        let (flag, instance) = text.trim_end_matches('\n').split_once('\u{1f}')?;
        let instance = instance.to_owned();
        match flag {
            "on" if !instance.is_empty() => Some(instance),
            "" => {
                let socket = self.native.socket.clone();
                let enabled = tokio::task::spawn_blocking(move || Store::inbox_flag(&socket))
                    .await
                    .unwrap_or(false);
                // -o leaves a value another process set meanwhile.
                let _ = self
                    .command(&[
                        "set-option",
                        "-gqo",
                        "@masil-inbox",
                        if enabled { "on" } else { "off" },
                    ])
                    .await;
                (enabled && !instance.is_empty()).then_some(instance)
            }
            _ => None,
        }
    }

    /// Writes the effects to the inbox if it is on, then nudges a running
    /// coordinator. Failures are logged and otherwise ignored.
    pub(super) async fn inbox_apply(&self, effects: Vec<Effect>) {
        if effects.is_empty() {
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

/// Switches the inbox on or off. Enable sets the cached switch first and
/// disable clears the store's switch first, so a failure part way leaves
/// the cache on, which only costs a store open per event.
pub(super) async fn set_enabled(manager: &Manager, enabled: bool) -> Result<Value, String> {
    if enabled {
        manager
            .command(&["set-option", "-gq", "@masil-inbox", "on"])
            .await?;
    }
    let mut store = manager.operation_store().await?;
    store.set_inbox_enabled(enabled)?;
    if !enabled {
        manager
            .command(&["set-option", "-gq", "@masil-inbox", "off"])
            .await?;
    }
    Ok(json!({"inbox": if enabled { "on" } else { "off" }}))
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
            Ok(json!({"fence": store.read_all(through)?}))
        }
        ["enable"] => set_enabled(manager, true).await,
        ["disable"] => set_enabled(manager, false).await,
        ["status"] => {
            let socket = manager.native.socket.clone();
            let stored = tokio::task::spawn_blocking(move || Store::inbox_flag(&socket))
                .await
                .unwrap_or(false);
            let cached = manager
                .command(&["show-options", "-gqv", "@masil-inbox"])
                .await?
                .trim()
                .to_owned();
            let wanted = if stored { "on" } else { "off" };
            // The store decides; a stale cache is corrected here.
            let repaired = cached != wanted;
            if repaired {
                manager
                    .command(&["set-option", "-gq", "@masil-inbox", wanted])
                    .await?;
            }
            Ok(json!({"enabled": stored, "cache": cached, "repaired": repaired}))
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
