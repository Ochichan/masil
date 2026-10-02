//! The prompt queue (docs/prompt-queue.md): prompts a person prepares for
//! a run and sends one at a time, never on their own.
//!
//! Items live in the operation store (`prompt_queue`). Sending one goes
//! through the durable prompt path with the operation `q<id>`, and the
//! admission marks the item in the same transaction, so an item is sent at
//! most once whatever the processes do. An item's state is its
//! operation's, copied by a store trigger.

use super::operations::{QueueChange, QueueItem, Store};
use super::{Agent, Manager, now_ms};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The longest a path check may take (a stalled network mount).
const CHECK_LIMIT: Duration = Duration::from_secs(2);
const ATTACHMENTS: usize = 16;
/// The preview a list shows of each body.
const PREVIEW_CHARS: usize = 200;
/// The prompt path's own limit (prompt.rs).
const TEXT_BYTES: usize = 32_768;
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "heic"];

/// A local path the item refers to, as checked when it was added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Attachment {
    pub path: String,
    pub kind: String,
    pub bytes: u64,
    pub checked_ms: u64,
}

/// Resolves and checks a path without blocking on what it names: no
/// FIFO, socket or device, and a regular file or directory that can be
/// read. `base` resolves a relative path.
fn check_path(raw: &str, base: &Path) -> Result<Attachment, String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("invalid_argument: an attachment needs a path".into());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let joined = match (raw.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if Path::new(raw).is_absolute() => PathBuf::from(raw),
        _ => base.join(raw),
    };
    let missing = |why: &str| format!("attachment_missing: {raw}: {why}");
    let real = std::fs::canonicalize(&joined).map_err(|error| missing(&error.to_string()))?;
    let path = real
        .to_str()
        .ok_or_else(|| missing("the path is not valid UTF-8"))?
        .to_owned();
    if path
        .chars()
        .any(|c| c.is_control() || matches!(c, '\'' | '"' | '`'))
    {
        return Err(missing(
            "paths with control characters or quotes cannot be put in a prompt",
        ));
    }
    let metadata = std::fs::metadata(&real).map_err(|error| missing(&error.to_string()))?;
    let kind = metadata.file_type();
    if kind.is_fifo() || kind.is_socket() || kind.is_block_device() || kind.is_char_device() {
        return Err(missing("not a regular file or directory"));
    }
    if metadata.is_dir() {
        std::fs::read_dir(&real)
            .map_err(|error| missing(&error.to_string()))?
            .next();
        return Ok(Attachment {
            path,
            kind: "directory".into(),
            bytes: 0,
            checked_ms: now_ms(),
        });
    }
    // Non-blocking, in case the name became a FIFO since the look above.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(&real)
        .map_err(|error| missing(&error.to_string()))?;
    let opened = file
        .metadata()
        .map_err(|error| missing(&error.to_string()))?;
    if !opened.is_file() || opened.ino() != metadata.ino() {
        return Err(missing("the path changed while it was checked"));
    }
    let image = real
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        });
    Ok(Attachment {
        path,
        kind: if image { "image" } else { "file" }.into(),
        bytes: opened.len(),
        checked_ms: now_ms(),
    })
}

/// `check_path` on a thread of its own, within CHECK_LIMIT. A detached
/// thread: a path stuck on a dead mount must not hold up the runtime's
/// shutdown, as a blocking task would.
async fn check(raw: &str, base: &Path) -> Result<Attachment, String> {
    let (raw, base) = (raw.to_owned(), base.to_owned());
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(check_path(&raw, &base));
    });
    match tokio::time::timeout(CHECK_LIMIT, receiver).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("attachment_missing: the path check stopped".into()),
        Err(_) => Err("attachment_missing: the path did not answer within 2 s".into()),
    }
}

/// A path dropped or pasted into a field: shell escapes, quotes and
/// `file://` URLs become the plain path.
pub(crate) fn dropped_path(text: &str) -> String {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("file://") {
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        return percent_decode(rest);
    }
    for quote in ['\'', '"'] {
        if let Some(inner) = text
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner.to_owned();
        }
    }
    let mut output = String::new();
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            if let Some(next) = characters.next() {
                output.push(next);
            }
        } else {
            output.push(character);
        }
    }
    output
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            output.push(byte);
            index += 3;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

/// The text a queued prompt sends: the body, then its paths. A path with a
/// space is quoted; a multi-line body gets them on a line of their own. The
/// text never starts with the paths, so it cannot open a provider's `/` or
/// `@` menu (docs/prompt-queue.md).
pub(super) fn compose(body: &str, attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return body.to_owned();
    }
    let paths = attachments
        .iter()
        .map(|attachment| {
            if attachment.path.chars().any(char::is_whitespace) {
                format!("'{}'", attachment.path)
            } else {
                attachment.path.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let body = body.trim_end();
    if body.contains('\n') {
        format!("{body}\n{paths}")
    } else {
        format!("{body} {paths}")
    }
}

/// The prompt path's rules, for the text an item would send.
fn sendable(text: &str) -> Result<(), String> {
    if text.trim().is_empty()
        || text.len() > TEXT_BYTES
        || text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(format!(
            "invalid_argument: a queued prompt with its paths must be 1–{TEXT_BYTES} bytes without control keys"
        ));
    }
    Ok(())
}

fn attachments_of(item: &QueueItem) -> Vec<Attachment> {
    serde_json::from_value(item.attachments.clone()).unwrap_or_default()
}

/// What a list shows of an item: its state, a preview, never more of the
/// body than PREVIEW_CHARS.
fn public(item: &QueueItem, held: bool) -> Value {
    let body = item.body.as_deref();
    let preview = body.map(|body| body.chars().take(PREVIEW_CHARS).collect::<String>());
    json!({
        "id": item.id,
        "run": item.run,
        "pane": item.pane,
        "position": item.position,
        "state": item.state(),
        "held": held,
        "revision": item.revision,
        "preview": preview,
        "bytes": body.map(str::len),
        "lines": body.map(|body| body.lines().count()),
        "attachments": item.attachments,
        "note": item.note,
        "operation_key": item.operation_key,
        "sent_ms": item.sent_ms,
    })
}

/// A change the management window asks for (ui/queue.rs).
#[derive(Clone, Debug)]
pub(crate) enum QueueOp {
    Add {
        body: String,
    },
    Show {
        id: i64,
    },
    Edit {
        id: i64,
        body: String,
        revision: i64,
    },
    Attach {
        id: i64,
        path: String,
        revision: i64,
    },
    Move {
        id: i64,
        position: i64,
        revision: i64,
    },
    Remove {
        id: i64,
        revision: i64,
    },
    Send {
        id: i64,
        revision: i64,
    },
    AddFrom {
        id: i64,
    },
}

impl Manager {
    /// One window change, for the agent the pane runs now: an item of an
    /// earlier run is then held, whatever the window showed.
    pub(crate) async fn queue_op(&self, agent: &Agent, op: QueueOp) -> Result<Value, String> {
        let agent = self.get(&agent.pane_id).await?;
        let base = PathBuf::from(&agent.cwd);
        match op {
            QueueOp::Add { body } => self.queue_add(&agent, &body, &[], &base).await,
            QueueOp::Show { id } => self.queue_show(id).await,
            QueueOp::Edit { id, body, revision } => {
                self.queue_edit(&agent, id, &body, Some(revision)).await
            }
            QueueOp::Attach { id, path, revision } => {
                self.queue_attach(&agent, id, &dropped_path(&path), Some(revision), &base)
                    .await
            }
            QueueOp::Move {
                id,
                position,
                revision,
            } => self.queue_move(&agent, id, position, Some(revision)).await,
            QueueOp::Remove { id, revision } => self.queue_remove(id, Some(revision)).await,
            QueueOp::Send { id, revision } => {
                self.queue_send(&agent, Some(id), Some(revision)).await
            }
            QueueOp::AddFrom { id } => self.queue_add_from(&agent, id).await,
        }
    }

    /// The store for a read: none when this server never made one.
    async fn queue_store_for_read(&self) -> Result<Option<Store>, String> {
        let instance = self
            .command(&["show-options", "-gqv", super::STORE_OPTION])
            .await?
            .trim()
            .to_owned();
        if instance.is_empty() {
            return Ok(None);
        }
        let socket = self.native.socket.clone();
        tokio::task::spawn_blocking(move || Store::open_existing(&socket, &instance))
            .await
            .map_err(|error| error.to_string())?
    }

    /// The agent's queue: its run's items, then those of earlier runs in
    /// the same pane (held).
    pub(crate) async fn queue_view(&self, agent: &Agent) -> Result<Value, String> {
        let Some(store) = self.queue_store_for_read().await? else {
            return Ok(json!({"run": agent.run, "items": [], "held": []}));
        };
        let items = store.queue_items(now_ms())?;
        let current = items
            .iter()
            .filter(|item| item.run == agent.run)
            .map(|item| public(item, false))
            .collect::<Vec<_>>();
        let held = items
            .iter()
            .filter(|item| item.run != agent.run && item.pane == agent.pane_id)
            .map(|item| public(item, true))
            .collect::<Vec<_>>();
        Ok(json!({"run": agent.run, "items": current, "held": held}))
    }

    /// Items of runs no pane runs now.
    pub(crate) async fn queue_held(&self) -> Result<Value, String> {
        let Some(store) = self.queue_store_for_read().await? else {
            return Ok(json!({"held": []}));
        };
        let (live, _) = self.live_runs().await?;
        let held = store
            .queue_items(now_ms())?
            .iter()
            .filter(|item| !live.contains(&item.run))
            .map(|item| public(item, true))
            .collect::<Vec<_>>();
        Ok(json!({"held": held}))
    }

    /// The whole body of one item.
    pub(crate) async fn queue_show(&self, id: i64) -> Result<Value, String> {
        let store = self
            .queue_store_for_read()
            .await?
            .ok_or_else(|| format!("target_absent: queued prompt {id} does not exist"))?;
        let item = store
            .queue_item(id, now_ms())?
            .ok_or_else(|| format!("target_absent: queued prompt {id} does not exist"))?;
        let mut value = public(&item, false);
        value["body"] = json!(item.body);
        Ok(value)
    }

    pub(crate) async fn queue_add(
        &self,
        agent: &Agent,
        body: &str,
        paths: &[String],
        base: &Path,
    ) -> Result<Value, String> {
        if paths.len() > ATTACHMENTS {
            return Err(format!(
                "invalid_argument: at most {ATTACHMENTS} attachments"
            ));
        }
        let mut attachments = Vec::new();
        for path in paths {
            attachments.push(check(path, base).await?);
        }
        sendable(&compose(body, &attachments))?;
        let mut store = self.operation_store().await?;
        let item = store.queue_add(
            &agent.run,
            &agent.pane_id,
            &agent.provider,
            body,
            &json!(attachments),
            now_ms(),
        )?;
        Ok(public(&item, false))
    }

    /// A new item for the agent's run with another item's body and paths
    /// (an earlier run's, one not sent, one sent).
    pub(crate) async fn queue_add_from(&self, agent: &Agent, id: i64) -> Result<Value, String> {
        let store = self.operation_store().await?;
        let item = store
            .queue_item(id, now_ms())?
            .ok_or_else(|| format!("target_absent: queued prompt {id} does not exist"))?;
        let body = item
            .body
            .clone()
            .ok_or("queue_not_staged: that prompt's body was cleared a day after it was sent")?;
        drop(store);
        let paths = attachments_of(&item)
            .into_iter()
            .map(|attachment| attachment.path)
            .collect::<Vec<_>>();
        self.queue_add(agent, &body, &paths, Path::new("/")).await
    }

    async fn queue_change(
        &self,
        agent: &Agent,
        id: i64,
        revision: Option<i64>,
        change: QueueChange,
    ) -> Result<Value, String> {
        let mut store = self.operation_store().await?;
        let item = store.queue_change(id, revision, &agent.run, change, now_ms())?;
        Ok(public(&item, false))
    }

    pub(crate) async fn queue_edit(
        &self,
        agent: &Agent,
        id: i64,
        body: &str,
        revision: Option<i64>,
    ) -> Result<Value, String> {
        let current = self.queue_current(id).await?;
        sendable(&compose(body, &attachments_of(&current)))?;
        // Checked against these paths: changed meanwhile, it is refused.
        self.queue_change(
            agent,
            id,
            revision.or(Some(current.revision)),
            QueueChange::Body(body.to_owned()),
        )
        .await
    }

    pub(crate) async fn queue_attach(
        &self,
        agent: &Agent,
        id: i64,
        path: &str,
        revision: Option<i64>,
        base: &Path,
    ) -> Result<Value, String> {
        let current = self.queue_current(id).await?;
        let mut attachments = attachments_of(&current);
        if attachments.len() >= ATTACHMENTS {
            return Err(format!(
                "invalid_argument: at most {ATTACHMENTS} attachments"
            ));
        }
        attachments.push(check(path, base).await?);
        sendable(&compose(
            current.body.as_deref().unwrap_or_default(),
            &attachments,
        ))?;
        self.queue_change(
            agent,
            id,
            revision.or(Some(current.revision)),
            QueueChange::Attachments(json!(attachments)),
        )
        .await
    }

    pub(crate) async fn queue_detach(
        &self,
        agent: &Agent,
        id: i64,
        index: usize,
        revision: Option<i64>,
    ) -> Result<Value, String> {
        let current = self.queue_current(id).await?;
        let mut attachments = attachments_of(&current);
        if index == 0 || index > attachments.len() {
            return Err("invalid_argument: no attachment with that number".into());
        }
        attachments.remove(index - 1);
        self.queue_change(
            agent,
            id,
            revision.or(Some(current.revision)),
            QueueChange::Attachments(json!(attachments)),
        )
        .await
    }

    pub(crate) async fn queue_move(
        &self,
        agent: &Agent,
        id: i64,
        position: i64,
        revision: Option<i64>,
    ) -> Result<Value, String> {
        self.queue_change(agent, id, revision, QueueChange::Position(position))
            .await
    }

    pub(crate) async fn queue_remove(
        &self,
        id: i64,
        revision: Option<i64>,
    ) -> Result<Value, String> {
        let (live, _) = self.live_runs().await?;
        let mut store = self.operation_store().await?;
        store.queue_remove(id, revision, &live, now_ms())?;
        Ok(json!({"removed": id}))
    }

    async fn queue_current(&self, id: i64) -> Result<QueueItem, String> {
        let store = self.operation_store().await?;
        store
            .queue_item(id, now_ms())?
            .ok_or_else(|| format!("target_absent: queued prompt {id} does not exist"))
    }

    /// Sends one staged item (the first by default) through the durable
    /// prompt path. Nothing waits for the agent to become idle.
    pub(crate) async fn queue_send(
        &self,
        agent: &Agent,
        id: Option<i64>,
        revision: Option<i64>,
    ) -> Result<Value, String> {
        let store = self.operation_store().await?;
        let items = store.queue_items(now_ms())?;
        drop(store);
        let item = match id {
            Some(id) => items
                .into_iter()
                .find(|item| item.id == id)
                .ok_or_else(|| format!("target_absent: queued prompt {id} does not exist"))?,
            None => items
                .into_iter()
                .filter(|item| item.run == agent.run && item.state() == "staged")
                .min_by_key(|item| (item.position, item.id))
                .ok_or("target_absent: nothing is queued for this agent")?,
        };
        if item.run != agent.run {
            return Err("queue_held: this prompt was queued for an earlier run of the agent; add it again with `add --from`".into());
        }
        match item.state() {
            "staged" => {}
            // A send that was cut off: once its lease is over, the pane
            // settles it (the agent need not be idle for that). Not applied,
            // it is sent below; otherwise its settled outcome is the answer.
            "sending" => {
                let key = item.operation_key.clone().unwrap_or_default();
                let mut store = self.operation_store().await?;
                let record = store.get(&key)?;
                let Some(record) = record.filter(|record| record.lease_until_ms <= now_ms()) else {
                    return Err(
                        "queue_sending: this prompt is being sent; look at it again shortly".into(),
                    );
                };
                // Read again under the lock: another process may have
                // settled it, or the slow send finished, meanwhile.
                let settled = {
                    let _lock = self.lock()?;
                    match store.get(&key)? {
                        Some(record)
                            if record.state == super::operations::DISPATCHING
                                && record.lease_until_ms <= now_ms() =>
                        {
                            self.reconcile_prompt(&mut store, &record).await?
                        }
                        Some(record) => record,
                        None => record,
                    }
                };
                if !matches!(
                    settled.state.as_str(),
                    "not_applied" | "rejected_before_effect"
                ) {
                    return Ok(json!({
                        "stage": settled.state,
                        "operation_key": settled.operation_key,
                        "item": item.id,
                    }));
                }
            }
            "sent" => {
                return Err("queue_not_staged: this prompt was sent; see its receipt, or queue it again with `add --from`".into());
            }
            state => {
                return Err(format!(
                    "queue_not_staged: this prompt is {state}; {}",
                    if state == "unknown" {
                        "settle it with `agent operation resolve` first"
                    } else {
                        "queue it again with `add --from`"
                    }
                ));
            }
        }
        if revision.is_some_and(|revision| revision != item.revision) {
            return Err(
                "queue_stale: the queued prompt changed since it was shown; look at it again"
                    .into(),
            );
        }
        let body = item
            .body
            .clone()
            .ok_or("queue_not_staged: this prompt has no body")?;
        // The paths again, as they are now: the same file, still readable.
        let mut attachments = Vec::new();
        for attachment in attachments_of(&item) {
            let now = check(&attachment.path, Path::new("/")).await;
            match now {
                Ok(now) if now.path == attachment.path => attachments.push(now),
                Ok(_) => {
                    return self
                        .queue_refused(
                            item.id,
                            format!(
                                "attachment_missing: {} now resolves elsewhere",
                                attachment.path
                            ),
                        )
                        .await;
                }
                Err(error) => return self.queue_refused(item.id, error).await,
            }
        }
        let text = compose(&body, &attachments);
        if let Err(error) = sendable(&text) {
            return self.queue_refused(item.id, error).await;
        }
        match self
            .prompt_queued(agent, &text, item.id, item.revision)
            .await
        {
            // Admission found the same operation still within its lease.
            Err(error) if error.contains("is in progress") => Err(format!(
                "queue_sending: this prompt is being sent; look at it again shortly ({error})"
            )),
            // Refused before any effect: the item stays staged with why.
            Err(error) if !error.starts_with("outcome_unknown") => {
                self.queue_refused(item.id, error).await
            }
            result => result,
        }
    }

    /// Records why an item was not sent, and returns that error.
    async fn queue_refused<T>(&self, id: i64, error: String) -> Result<T, String> {
        if let Ok(mut store) = self.operation_store().await {
            let _ = store.queue_note(id, &error, now_ms());
        }
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(path: &str) -> Attachment {
        Attachment {
            path: path.into(),
            kind: "file".into(),
            bytes: 1,
            checked_ms: 0,
        }
    }

    #[test]
    fn paths_follow_the_body_and_a_spaced_one_is_quoted() {
        assert_eq!(compose("look", &[]), "look");
        assert_eq!(
            compose(
                "look at these ",
                &[attachment("/a/b.rs"), attachment("/My Docs/c.md")]
            ),
            "look at these /a/b.rs '/My Docs/c.md'"
        );
        assert_eq!(
            compose("line one\n```\ncode\n```\n", &[attachment("/a")]),
            "line one\n```\ncode\n```\n/a"
        );
        assert!(sendable("ok\tfine\nyes").is_ok());
        assert!(sendable("bad\u{1b}[2J").is_err());
        assert!(sendable(&"x".repeat(TEXT_BYTES + 1)).is_err());
    }

    #[test]
    fn dropped_paths_lose_their_escapes() {
        assert_eq!(dropped_path("/My\\ Docs/a\\ b.txt"), "/My Docs/a b.txt");
        assert_eq!(dropped_path("'/My Docs/a.txt'"), "/My Docs/a.txt");
        assert_eq!(dropped_path("\"/x y\""), "/x y");
        assert_eq!(
            dropped_path("file:///Users/me/%ED%95%9C%EA%B8%80.txt"),
            "/Users/me/한글.txt"
        );
        assert_eq!(dropped_path("  /plain/path  "), "/plain/path");
    }

    #[test]
    fn checks_take_files_and_directories_and_refuse_fifos_and_quotes() {
        let dir = std::env::temp_dir().join(format!("masil-queue-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("한글 파일.txt");
        std::fs::write(&file, "x").unwrap();
        let checked = check_path(file.to_str().unwrap(), Path::new("/")).unwrap();
        assert_eq!((checked.kind.as_str(), checked.bytes), ("file", 1));
        assert_eq!(check_path(".", &dir).unwrap().kind, "directory");
        let fifo = dir.join("fifo");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a valid path; the FIFO is removed with the directory.
        unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert!(
            check_path(fifo.to_str().unwrap(), Path::new("/"))
                .unwrap_err()
                .starts_with("attachment_missing")
        );
        let quoted = dir.join("it's.txt");
        std::fs::write(&quoted, "x").unwrap();
        assert!(check_path(quoted.to_str().unwrap(), Path::new("/")).is_err());
        assert!(check_path("missing.txt", &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
