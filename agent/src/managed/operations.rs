//! Durable management operations. A request is admitted and its dispatch
//! intent committed before any external effect; the confirmed stage is
//! committed after it. Nothing here repeats an effect whose outcome is
//! unknown. See docs/design/state-and-recovery.md sections 4–5.

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SCHEMA_VERSION: i64 = 2;
const V1: i64 = 1;
/// A store whose `meta.min_reader` is above this refuses this binary.
/// Additive changes keep min_reader; only a change older readers would
/// corrupt raises it.
pub(super) const READER_GENERATION: i64 = 2;
/// Live data (pages in use) at which pruning starts and admission stops,
/// docs/design/performance.md:145-150.
const SOFT_BYTES: i64 = 128 << 20;
const HIGH_WATER_BYTES: i64 = 224 << 20;
/// WAL content that checkpoints could not move, as when a reader holds an
/// old snapshot; admission stops above it.
const WAL_PRESSURE_BYTES: i64 = 32 << 20;
const JOURNAL_LIMIT_BYTES: i64 = 8 << 20;
const AUTOCHECKPOINT_PAGES: i64 = 2048;
/// The pre-v2 backup is kept this long after a successful upgrade.
const BACKUP_KEEP_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// SQLite 3.51.3 fixed the WAL-reset corruption bug.
const MIN_SQLITE: i32 = 3_051_003;
pub(super) const LEASE_MS: u64 = 30_000;
const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const PRUNE_ABOVE: i64 = 2048;
const CAPACITY: i64 = 4096;
const MAX_EVIDENCE: usize = 2048;

/// States from which a new attempt may start: the previous attempt is proven
/// to have had no effect.
/// States after which the same key may run again: nothing took effect.
/// `cwd_rejected`: the launched child refused its directory and never
/// started the agent.
const RETRYABLE: [&str; 3] = ["rejected_before_effect", "not_applied", "cwd_rejected"];
pub(super) const DISPATCHING: &str = "dispatching";
pub(super) const UNKNOWN: &str = "outcome_unknown";

const SCHEMA: &str = "
CREATE TABLE operations (
  id INTEGER PRIMARY KEY,
  key TEXT NOT NULL UNIQUE,
  namespace TEXT NOT NULL,
  action TEXT NOT NULL,
  client_id TEXT NOT NULL,
  explicit INTEGER NOT NULL,
  target TEXT NOT NULL,
  run TEXT,
  boot TEXT NOT NULL,
  digest TEXT NOT NULL,
  payload_bytes INTEGER NOT NULL,
  state TEXT NOT NULL,
  ticket TEXT NOT NULL,
  attempts INTEGER NOT NULL,
  lease_until_ms INTEGER NOT NULL,
  owner_pid INTEGER NOT NULL,
  created_ms INTEGER NOT NULL,
  updated_ms INTEGER NOT NULL,
  store_seq INTEGER NOT NULL
);
CREATE TABLE receipts (
  operation_id INTEGER NOT NULL REFERENCES operations(id) ON DELETE CASCADE,
  ordinal INTEGER NOT NULL,
  stage TEXT NOT NULL,
  source TEXT NOT NULL,
  evidence TEXT,
  at_ms INTEGER NOT NULL,
  store_seq INTEGER NOT NULL,
  PRIMARY KEY (operation_id, ordinal)
);
CREATE TABLE meta (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO meta VALUES ('store_seq', 0);
CREATE INDEX operations_state ON operations(state, lease_until_ms);
CREATE INDEX operations_updated ON operations(updated_ms);
";

/// Version 2: feature migrations are recorded by name so that phases can
/// add tables in any order, and coordinator features are switched on here.
const V2_SCHEMA: &str = "
CREATE TABLE schema_features (name TEXT PRIMARY KEY, applied_ms INTEGER NOT NULL);
CREATE TABLE coordinator_features (name TEXT PRIMARY KEY, enabled_ms INTEGER NOT NULL);
INSERT INTO meta VALUES ('min_reader', 2);
";

/// Additive schema per feature, applied once each in any order. Later
/// phases add entries; names are never reused.
const FEATURES: &[(&str, &str)] = &[
    ("inbox", INBOX_SCHEMA),
    ("provider_secrets", PROVIDER_SECRETS_SCHEMA),
    ("prompt_queue", PROMPT_QUEUE_SCHEMA),
    ("notify", NOTIFY_SCHEMA),
    ("schedules", SCHEDULES_SCHEMA),
];

/// Schedules (docs/schedules.md). A run row is claimed before its action
/// starts, so a due time runs at most once; `missed` rows settle the times
/// that were not run.
const SCHEDULES_SCHEMA: &str = "
CREATE TABLE schedules (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL UNIQUE,
  spec TEXT NOT NULL,
  action TEXT NOT NULL,
  enabled INTEGER NOT NULL,
  created_ms INTEGER NOT NULL,
  changed_ms INTEGER NOT NULL
);
CREATE TABLE schedule_runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  schedule_id INTEGER NOT NULL,
  due_ms INTEGER NOT NULL,
  state TEXT NOT NULL,
  started_ms INTEGER,
  ended_ms INTEGER,
  result TEXT,
  UNIQUE (schedule_id, due_ms)
);
";
/// Run rows kept per schedule.
const SCHEDULE_RUNS_KEPT: i64 = 200;

/// A schedule as stored, with the latest due time it settled.
#[derive(Clone, Debug)]
pub(crate) struct ScheduleRow {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) spec: String,
    pub(crate) action: Value,
    pub(crate) enabled: bool,
    pub(crate) created_ms: i64,
    pub(crate) changed_ms: i64,
    pub(crate) last_due: Option<i64>,
    pub(crate) last_state: Option<String>,
}

/// What `schedule_record` settled: the due time this caller may run, and
/// whether a missed-schedule event was written.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ScheduleClaim {
    pub(crate) run: Option<i64>,
    pub(crate) event: bool,
}

/// Notifications (docs/notifications.md). A row is written, in the same
/// transaction that moves the cursor past its event, before anything is
/// sent: a notification is tried at most once, never twice.
const NOTIFY_SCHEMA: &str = "
CREATE TABLE notify_log (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  event_id INTEGER NOT NULL,
  store_seq INTEGER NOT NULL,
  route TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  outcome TEXT NOT NULL,
  UNIQUE (event_id, store_seq, route)
);
CREATE TABLE notify_env (name TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO meta VALUES ('notify_enabled', 0);
INSERT INTO meta VALUES ('notify_seq', 0);
";
const NOTIFY_KEEP: i64 = 1000;
const NOTIFY_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// Events read per notification batch.
const NOTIFY_BATCH: i64 = 200;

/// An event a notification pass claimed, with the log rows for its routes.
#[derive(Clone, Debug)]
pub(crate) struct NotifyEvent {
    pub(crate) id: i64,
    pub(crate) kind: String,
    pub(crate) provider: String,
    pub(crate) pane: String,
    pub(crate) run: String,
    pub(crate) summary: Option<Value>,
    pub(crate) observed_ms: u64,
    /// Log row per route, to record how sending went.
    pub(crate) rows: Vec<(String, i64)>,
}

/// What a claim found: events to send, and how many were too old to.
#[derive(Default, Debug)]
pub(crate) struct NotifyClaim {
    pub(crate) events: Vec<NotifyEvent>,
    pub(crate) stale: usize,
}

/// The prompt queue (docs/prompt-queue.md). A row's prompt is the operation
/// `operation_key`; the trigger copies that operation's state to the row in
/// the same transaction, whoever changes it, so the row keeps it after the
/// operation is pruned. Bodies stay until the person removes them (D8); a
/// sent body goes 24 h after `sent_ms`.
const PROMPT_QUEUE_SCHEMA: &str = "
CREATE TABLE prompt_queue (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  run TEXT NOT NULL,
  pane TEXT NOT NULL,
  provider TEXT NOT NULL,
  position INTEGER NOT NULL,
  body TEXT,
  attachments TEXT NOT NULL,
  revision INTEGER NOT NULL,
  operation_key TEXT,
  op_state TEXT,
  op_updated_ms INTEGER,
  note TEXT,
  created_ms INTEGER NOT NULL,
  updated_ms INTEGER NOT NULL,
  sent_ms INTEGER
);
CREATE INDEX prompt_queue_run ON prompt_queue (run, position, id);
CREATE INDEX prompt_queue_operation ON prompt_queue (operation_key);
CREATE TRIGGER prompt_queue_follow AFTER UPDATE OF state ON operations
WHEN NEW.action = 'prompt'
BEGIN
  UPDATE prompt_queue SET op_state = NEW.state, op_updated_ms = NEW.updated_ms,
    sent_ms = CASE
      WHEN sent_ms IS NULL
        AND NEW.state IN ('delivered', 'native_accepted', 'user_confirmed_delivered')
      THEN NEW.updated_ms ELSE sent_ms END
  WHERE operation_key = NEW.key;
END;
";
/// A sent body is kept this long, the row a week (to read its receipt).
const QUEUE_SENT_BODY_MS: u64 = 24 * 60 * 60 * 1000;
const QUEUE_SENT_ROW_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// Queue quota: items and bytes of kept bodies and attachments.
const QUEUE_ITEMS: i64 = 256;
const QUEUE_BYTES: i64 = 2 << 20;
const QUEUE_RUN_ITEMS: i64 = 64;

/// Per-run secrets a provider's local server checks (OpenCode's server
/// password, docs/agent-answers.md). The store is 0600 and doctor bundles
/// leave it out.
const PROVIDER_SECRETS_SCHEMA: &str = "
CREATE TABLE provider_secrets (
  run TEXT PRIMARY KEY,
  provider TEXT NOT NULL,
  secret TEXT NOT NULL,
  created_ms INTEGER NOT NULL
);
";
/// Secrets of runs that are gone are kept this long against a run that is
/// registering while the cleanup lists live runs.
const SECRET_GRACE_MS: u64 = 60_000;
const SECRET_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Durable attention events (docs/inbox.md). AUTOINCREMENT keeps public IDs
/// unique after pruning; a resolution may arrive before its request.
const INBOX_SCHEMA: &str = "
CREATE TABLE inbox_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  store_seq INTEGER NOT NULL,
  source TEXT NOT NULL,
  source_ref TEXT NOT NULL,
  provider TEXT NOT NULL,
  pane TEXT NOT NULL,
  run TEXT NOT NULL,
  revision INTEGER,
  kind TEXT NOT NULL,
  native_ref TEXT,
  summary TEXT,
  observed_ms INTEGER NOT NULL,
  acked_ms INTEGER,
  resolved_ms INTEGER,
  resolution TEXT,
  UNIQUE (source, source_ref)
);
CREATE INDEX inbox_open ON inbox_events(resolved_ms, acked_ms, store_seq);
CREATE INDEX inbox_run ON inbox_events(run, store_seq);
CREATE TABLE inbox_resolutions (
  source TEXT NOT NULL,
  source_ref TEXT NOT NULL,
  resolved_ms INTEGER NOT NULL,
  resolution TEXT NOT NULL,
  PRIMARY KEY (source, source_ref)
);
INSERT INTO meta VALUES ('inbox_fence', 0);
INSERT INTO meta VALUES ('inbox_enabled', 0);
INSERT INTO meta VALUES ('inbox_dropped', 0);
";
const INBOX_KEEP: i64 = 4096;
const INBOX_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const INBOX_SUMMARY_BYTES: usize = 2048;

/// One attention event as a producer reports it.
pub(super) struct InboxEvent<'a> {
    pub source: &'a str,
    pub source_ref: &'a str,
    pub provider: &'a str,
    pub pane: &'a str,
    pub run: &'a str,
    pub revision: Option<i64>,
    pub kind: &'a str,
    pub native_ref: Option<&'a str>,
    pub summary: Option<Value>,
    pub observed_ms: u64,
}

#[derive(Debug)]
pub(super) struct Store {
    conn: Connection,
    path: PathBuf,
}

const QUEUE_COLUMNS: &str = "id, run, pane, provider, position, body, attachments, revision,
  operation_key, op_state, note, created_ms, updated_ms, sent_ms";

/// One queued prompt (docs/prompt-queue.md).
#[derive(Clone, Debug, Serialize)]
pub(super) struct QueueItem {
    pub id: i64,
    pub run: String,
    pub pane: String,
    pub provider: String,
    pub position: i64,
    /// None once a sent body's day is over.
    pub body: Option<String>,
    pub attachments: Value,
    pub revision: i64,
    pub operation_key: Option<String>,
    pub op_state: Option<String>,
    pub note: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub sent_ms: Option<u64>,
}

/// A change of a staged queued prompt.
pub(super) enum QueueChange {
    Body(String),
    Attachments(Value),
    /// 1-based place among the run's items.
    Position(i64),
}

impl QueueItem {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let attachments: String = row.get(6)?;
        Ok(Self {
            id: row.get(0)?,
            run: row.get(1)?,
            pane: row.get(2)?,
            provider: row.get(3)?,
            position: row.get(4)?,
            body: row.get(5)?,
            attachments: serde_json::from_str(&attachments).unwrap_or(Value::Array(Vec::new())),
            revision: row.get(7)?,
            operation_key: row.get(8)?,
            op_state: row.get(9)?,
            note: row.get(10)?,
            created_ms: row.get::<_, i64>(11)?.max(0) as u64,
            updated_ms: row.get::<_, i64>(12)?.max(0) as u64,
            sent_ms: row.get::<_, Option<i64>>(13)?.map(|at| at.max(0) as u64),
        })
    }

    fn hidden_after(mut self, now: u64) -> Self {
        if self
            .sent_ms
            .is_some_and(|sent| now.saturating_sub(sent) >= QUEUE_SENT_BODY_MS)
        {
            self.body = None;
        }
        self
    }

    /// staged, sending, sent, unknown or not_sent, from its operation.
    pub fn state(&self) -> &'static str {
        match self.op_state.as_deref() {
            None | Some("rejected_before_effect" | "not_applied") => "staged",
            Some(DISPATCHING) => "sending",
            Some("delivered" | "native_accepted" | "user_confirmed_delivered") => "sent",
            Some(UNKNOWN) => "unknown",
            Some("user_confirmed_not_delivered") => "not_sent",
            Some(_) => "unknown",
        }
    }

    fn stale() -> String {
        "queue_stale: the queued prompt changed since it was shown; look at it again".into()
    }

    /// Whether this item of `current_run` may change now.
    fn changeable(&self, revision: Option<i64>, current_run: &str) -> Result<(), String> {
        if self.run != current_run {
            return Err("queue_held: this prompt was queued for an earlier run of the agent; it can be removed or added again".into());
        }
        match self.state() {
            "staged" => {}
            "sending" => {
                return Err(
                    "queue_sending: this prompt is being sent; look at it again shortly".into(),
                );
            }
            state => {
                return Err(format!(
                    "queue_not_staged: this prompt is {state}; settle an unknown one with `agent operation resolve`, or queue a sent one again with `add --from`"
                ));
            }
        }
        if revision.is_some_and(|revision| revision != self.revision) {
            return Err(Self::stale());
        }
        Ok(())
    }
}

pub(super) struct NewOperation<'a> {
    pub namespace: &'a str,
    pub action: &'a str,
    pub id: &'a str,
    pub explicit: bool,
    pub target: &'a str,
    pub run: Option<&'a str>,
    pub boot: &'a str,
    pub digest: &'a str,
    pub payload_bytes: u64,
}

/// The queue row a prompt admission marks (`admit_with`).
pub(super) struct QueuedPrompt<'a> {
    pub item: i64,
    pub revision: i64,
    pub run: &'a str,
}

/// Permission to attempt the effect once, identified by `ticket`.
#[derive(Clone, Debug)]
pub(super) struct Ticket {
    pub op: i64,
    pub key: String,
    pub ticket: String,
}

#[derive(Debug)]
pub(super) enum Admission {
    Dispatch(Ticket),
    /// The key already has a result. Report it; never repeat the effect.
    Recorded(Record),
    /// A previous attempt's lease expired without a result.
    NeedsReconcile(Record),
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Record {
    #[serde(skip)]
    pub op: i64,
    pub operation_key: String,
    pub action: String,
    pub id: String,
    pub explicit: bool,
    pub target: String,
    pub run: Option<String>,
    pub boot: String,
    pub digest: String,
    pub payload_bytes: u64,
    pub state: String,
    #[serde(skip)]
    pub ticket: String,
    pub attempts: u32,
    pub lease_until_ms: u64,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub receipts: Vec<ReceiptRow>,
}

impl Record {
    /// The dispatch intent evidence of the current attempt.
    pub fn intent(&self) -> Option<&Value> {
        self.receipts
            .iter()
            .rev()
            .filter(|receipt| receipt.stage == "dispatch_intent")
            .filter_map(|receipt| receipt.evidence.as_ref())
            .find(|evidence| evidence["ticket"] == self.ticket.as_str())
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ReceiptRow {
    pub ordinal: u32,
    pub stage: String,
    pub source: String,
    pub evidence: Option<Value>,
    pub at_ms: u64,
}

pub(super) fn key(namespace: &str, action: &str, id: &str) -> String {
    format!("{namespace}/{action}/{id}")
}

/// Client-chosen operation IDs are short printable tokens.
pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

/// `$XDG_STATE_HOME/masil/operations`, else `~/.local/state/masil/operations`,
/// owner-only and canonical: SQLITE_OPEN_NOFOLLOW rejects any symlinked component.
pub(super) fn state_base() -> Result<PathBuf, String> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/state"))
        })
        .ok_or_else(|| "no absolute XDG_STATE_HOME or HOME".into())
}

/// One file per server socket.
fn store_name(socket: &Path) -> String {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    let digest = Sha256::digest(socket.as_os_str().as_bytes());
    let name: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{name}.sqlite3")
}

fn state_directory() -> Result<PathBuf, String> {
    private_directory(&state_base()?.join("masil/operations"))
}

/// Creates `directory` if needed and returns its canonical path, owned by
/// this user and closed to group and other.
pub(crate) fn private_directory(directory: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    fs::create_dir_all(directory).map_err(|error| format!("{}: {error}", directory.display()))?;
    let directory = directory
        .canonicalize()
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    let metadata = fs::symlink_metadata(&directory).map_err(|error| error.to_string())?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(format!(
            "{} is not a directory you own",
            directory.display()
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    Ok(directory)
}

fn sql(error: rusqlite::Error) -> String {
    format!("operation store: {error}")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) const STORE_NEWER: &str = "store_newer";

/// How far a run-wide read reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Through {
    /// Up to this store sequence, the newest a client displayed.
    Seq(i64),
    /// Events recorded at or before this time, when a command read the agent.
    Before(u64),
}

fn reader_allowed(minimum: Option<i64>) -> Result<(), String> {
    match minimum {
        Some(minimum) if minimum > READER_GENERATION => Err(format!(
            "{STORE_NEWER}: the operation store needs masil-agent reader generation {minimum}; \
             this process is generation {READER_GENERATION}. Restart long-running masil-agent \
             processes such as agent ui and session autosave after an upgrade"
        )),
        _ => Ok(()),
    }
}

fn backup_path(store: &Path) -> PathBuf {
    let mut path = store.as_os_str().to_owned();
    path.push(".pre-v2.bak");
    PathBuf::from(path)
}

/// Copies the store as it is now through a second, read-only connection.
/// The caller holds the write lock, so no commit can land in between.
fn take_backup(store: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let backup = backup_path(store);
    match fs::symlink_metadata(&backup) {
        // The leftover of an earlier failed attempt.
        Ok(metadata) if metadata.is_file() => {
            fs::remove_file(&backup).map_err(|error| format!("store backup: {error}"))?
        }
        Ok(_) => return Err(format!("{} is not a regular file", backup.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("store backup: {error}")),
    }
    let target = backup
        .to_str()
        .ok_or("store backup path is not UTF-8")?
        .to_owned();
    let reader = Connection::open_with_flags(
        store,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(sql)?;
    reader.busy_timeout(Duration::from_secs(2)).map_err(sql)?;
    let copied = reader
        .execute("VACUUM INTO ?1", [&target])
        .map_err(sql)
        .and_then(|_| {
            // The directory is owner-only, so the moment before this is not
            // visible to other users.
            fs::set_permissions(&backup, fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("store backup: {error}"))
        });
    if let Err(error) = copied {
        let _ = fs::remove_file(&backup);
        return Err(error);
    }
    Ok(backup)
}

/// Removes the pre-v2 backup once it is older than the keep period.
fn expire_backup(store: &Path, now: u64) {
    let backup = backup_path(store);
    let Ok(metadata) = fs::symlink_metadata(&backup) else {
        return;
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis() as u64);
    if metadata.is_file() && modified.is_some_and(|modified| modified + BACKUP_KEEP_MS <= now) {
        let _ = fs::remove_file(&backup);
    }
}

/// Applies each feature migration not yet recorded, one transaction each.
/// The check is repeated under the write lock, so concurrent openers apply
/// a feature once.
fn apply_features(
    conn: &mut Connection,
    features: &[(&str, &str)],
    now: u64,
) -> Result<(), String> {
    if features.is_empty() {
        return Ok(());
    }
    let applied = |conn: &Connection, name: &str| -> Result<bool, String> {
        conn.query_row(
            "SELECT 1 FROM schema_features WHERE name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()
        .map(|found| found.is_some())
        .map_err(sql)
    };
    for (name, schema) in features {
        if applied(conn, name)? {
            continue;
        }
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        if !applied(&tx, name)? {
            tx.execute_batch(schema).map_err(sql)?;
            tx.execute(
                "INSERT INTO schema_features VALUES (?1, ?2)",
                params![name, now as i64],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
    }
    Ok(())
}

/// Bytes in pages that hold data; freed pages stay in the file.
fn live_bytes(conn: &Connection) -> Result<i64, String> {
    conn.query_row(
        "SELECT ((SELECT page_count FROM pragma_page_count())
                 - (SELECT freelist_count FROM pragma_freelist_count()))
                * (SELECT page_size FROM pragma_page_size())",
        [],
        |row| row.get(0),
    )
    .map_err(sql)
}

impl Store {
    /// Open the store for one server socket (the environment). It lives in
    /// the private state directory, not beside the socket: temporary
    /// directories are cleaned while a server can still be running.
    pub fn open(socket: &Path) -> Result<Self, String> {
        let unavailable = |error: String| {
            if super::failure::has_registered_code(&error) {
                error
            } else {
                format!("store_unavailable: {error}")
            }
        };
        let directory = state_directory().map_err(unavailable)?;
        Self::open_path(&directory.join(store_name(socket))).map_err(unavailable)
    }

    /// Status of the store for `socket` without creating anything; `None`
    /// when no management command has used one for this server yet.
    pub fn inspect(socket: &Path) -> Result<Option<Value>, String> {
        let socket = socket
            .canonicalize()
            .map_err(|error| format!("native socket: {error}"))?;
        let Ok(directory) = state_base().map(|base| base.join("masil/operations")) else {
            return Ok(None);
        };
        let Ok(directory) = directory.canonicalize() else {
            return Ok(None);
        };
        let path = directory.join(store_name(&socket));
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return Ok(None);
        };
        super::store::validate_private_metadata(&metadata, "operation store")?;
        // Read-only: no migration, no journal-mode change, no instance.
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(sql)?;
        conn.busy_timeout(Duration::from_secs(2)).map_err(sql)?;
        let version = Self::user_version(&conn)?;
        let minimum: Option<i64> = if version >= SCHEMA_VERSION {
            conn.query_row(
                "SELECT value FROM meta WHERE name = 'min_reader'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?
        } else {
            None
        };
        if version < V1 || reader_allowed(minimum).is_err() {
            return Ok(Some(json!({
                "path": path,
                "schema_version": version,
                "min_reader": minimum,
                "reader_generation": READER_GENERATION,
                "initialized": version != 0,
                "supported": false,
            })));
        }
        let store = Self { conn, path };
        let mut status = store.status()?;
        // Per-connection settings of this reader, not of the writers.
        if let Some(object) = status.as_object_mut() {
            object.remove("synchronous");
            object.remove("fullfsync");
        }
        status["initialized"] = json!(true);
        if version == V1 {
            status["migrates_on_next_write"] = json!(true);
        }
        status["unresolved"] = json!(store.list(true, CAPACITY as usize)?.len());
        Ok(Some(status))
    }

    /// Coordinator features switched on in the store for `socket` under the
    /// state directory `base`, read without creating or changing anything.
    /// Empty when no store or no version 2 store exists yet. The inbox is a
    /// feature while its own switch is on, so the two never disagree.
    pub fn enabled_features(base: &Path, socket: &Path) -> Result<Vec<String>, String> {
        let socket = socket
            .canonicalize()
            .map_err(|error| format!("native socket: {error}"))?;
        let Ok(directory) = base.join("masil/operations").canonicalize() else {
            return Ok(Vec::new());
        };
        let path = directory.join(store_name(&socket));
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return Ok(Vec::new());
        };
        super::store::validate_private_metadata(&metadata, "operation store")?;
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(sql)?;
        conn.busy_timeout(Duration::from_secs(2)).map_err(sql)?;
        if Self::user_version(&conn)? < SCHEMA_VERSION {
            return Ok(Vec::new());
        }
        // A newer masil-agent owns this store: this one starts nothing for it.
        Self::check_reader_in(&conn)?;
        let mut statement = conn
            .prepare("SELECT name FROM coordinator_features ORDER BY name")
            .map_err(sql)?;
        let mut features = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        if Self::inbox_enabled_in(&conn)? && !features.iter().any(|name| name == "inbox") {
            features.push("inbox".into());
        }
        if Self::notify_enabled_in(&conn)? && !features.iter().any(|name| name == "notify") {
            features.push("notify".into());
        }
        let schedules: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schedules')",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if schedules
            && conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM schedules WHERE enabled = 1)",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(sql)?
            && !features.iter().any(|name| name == "schedules")
        {
            features.push("schedules".into());
        }
        // An `--answers` run keeps its secret until the coordinator sees it end.
        let secrets: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'provider_secrets')",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if secrets
            && conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM provider_secrets)",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(sql)?
            && !features.iter().any(|name| name == "answers")
        {
            features.push("answers".into());
        }
        features.sort();
        Ok(features)
    }

    /// Random identity of this database, recorded in the server so that a
    /// store deleted under a running server is noticed instead of recreated.
    pub fn instance(&self) -> Result<i64, String> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE name = 'instance'",
                [],
                |row| row.get(0),
            )
            .map_err(sql)
    }

    fn open_path(path: &Path) -> Result<Self, String> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => super::store::validate_private_metadata(&metadata, "operation store")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)
                {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
                        super::store::validate_private_metadata(&metadata, "operation store")?;
                    }
                    Err(error) => return Err(format!("operation store: {error}")),
                }
            }
            Err(error) => return Err(format!("operation store: {error}")),
        }
        if rusqlite::version_number() < MIN_SQLITE {
            return Err(format!(
                "store_unsupported: SQLite {} lacks the WAL-reset fix",
                rusqlite::version()
            ));
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(sql)?;
        conn.busy_timeout(Duration::from_secs(2)).map_err(sql)?;
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .map_err(sql)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(format!(
                "store_unsupported: operation store journal mode is {mode}"
            ));
        }
        // FULL syncs every commit. F_FULLFSYNC is off: every namespace (run,
        // server boot) ends with the OS, so power-loss durability would not
        // prevent any duplicate; `operations status` reports this profile.
        conn.execute_batch(&format!(
            "PRAGMA synchronous=FULL; PRAGMA fullfsync=OFF; PRAGMA checkpoint_fullfsync=OFF;
             PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF; PRAGMA secure_delete=ON;
             PRAGMA journal_size_limit={JOURNAL_LIMIT_BYTES}; PRAGMA wal_autocheckpoint={AUTOCHECKPOINT_PAGES};"
        ))
        .map_err(sql)?;
        let mut store = Self {
            conn,
            path: path.to_path_buf(),
        };
        store.migrate()?;
        Ok(store)
    }

    fn user_version(conn: &Connection) -> Result<i64, String> {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(sql)
    }

    fn migrate(&mut self) -> Result<(), String> {
        // The common case needs no write lock.
        if Self::user_version(&self.conn)? < SCHEMA_VERSION {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql)?;
            // Another process may have created or upgraded the store first.
            match Self::user_version(&tx)? {
                0 => {
                    tx.execute_batch(SCHEMA).map_err(sql)?;
                    tx.execute_batch(V2_SCHEMA).map_err(sql)?;
                    let instance = i64::from_str_radix(&super::nonce()?[..15], 16)
                        .map_err(|error| error.to_string())?;
                    tx.execute("INSERT INTO meta VALUES ('instance', ?1)", [instance])
                        .map_err(sql)?;
                    tx.pragma_update(None, "user_version", SCHEMA_VERSION)
                        .map_err(sql)?;
                    tx.commit().map_err(sql)?;
                }
                V1 => {
                    // The backup is taken while this transaction holds the
                    // write lock, so it is exactly the state being upgraded.
                    let backup = take_backup(&self.path)?;
                    let upgraded = tx
                        .execute_batch(V2_SCHEMA)
                        .and_then(|()| tx.pragma_update(None, "user_version", SCHEMA_VERSION))
                        .and_then(|()| tx.commit());
                    if let Err(error) = upgraded {
                        // The live store is still an intact version 1.
                        let _ = fs::remove_file(&backup);
                        return Err(sql(error));
                    }
                }
                _ => tx.commit().map_err(sql)?,
            }
        }
        self.check_reader()?;
        self.apply_features()?;
        expire_backup(&self.path, now_ms());
        Ok(())
    }

    /// Refuses a store that a newer, incompatible masil-agent has upgraded.
    fn check_reader(&self) -> Result<(), String> {
        let minimum: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE name = 'min_reader'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?;
        reader_allowed(minimum)
    }

    fn apply_features(&mut self) -> Result<(), String> {
        apply_features(&mut self.conn, FEATURES, now_ms())
    }

    /// Run inside every write transaction, so a long-lived connection
    /// notices a newer binary raising min_reader before it writes again.
    fn check_reader_in(conn: &Connection) -> Result<(), String> {
        let minimum: Option<i64> = conn
            .query_row(
                "SELECT value FROM meta WHERE name = 'min_reader'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?;
        reader_allowed(minimum)
    }

    fn next_seq(tx: &Connection) -> Result<i64, String> {
        Self::check_reader_in(tx)?;
        tx.query_row(
            "UPDATE meta SET value = value + 1 WHERE name = 'store_seq' RETURNING value",
            [],
            |row| row.get(0),
        )
        .map_err(sql)
    }

    fn append(
        tx: &rusqlite::Transaction<'_>,
        op: i64,
        stage: &str,
        source: &str,
        evidence: Option<&Value>,
        now: u64,
    ) -> Result<(), String> {
        let seq = Self::next_seq(tx)?;
        let evidence = evidence
            .map(Value::to_string)
            .filter(|text| text.len() <= MAX_EVIDENCE);
        tx.execute(
            "INSERT INTO receipts (operation_id, ordinal, stage, source, evidence, at_ms, store_seq)
             VALUES (?1, (SELECT COALESCE(MAX(ordinal), 0) + 1 FROM receipts WHERE operation_id = ?1),
                     ?2, ?3, ?4, ?5, ?6)",
            params![op, stage, source, evidence, now as i64, seq],
        )
        .map_err(sql)?;
        Ok(())
    }

    /// Admit a request and commit its dispatch intent in one transaction.
    /// `intent` holds what reconcile needs to judge this attempt later.
    pub fn admit(
        &mut self,
        new: &NewOperation<'_>,
        intent: Value,
        now: u64,
    ) -> Result<Admission, String> {
        self.admit_with(new, intent, now, None)
    }

    /// As `admit`; a dispatch of a queued prompt also marks its queue row in
    /// the same transaction, which commits only if the row still has the
    /// revision the text was made from.
    pub fn admit_with(
        &mut self,
        new: &NewOperation<'_>,
        intent: Value,
        now: u64,
        queued: Option<&QueuedPrompt<'_>>,
    ) -> Result<Admission, String> {
        let key = key(new.namespace, new.action, new.id);
        self.check_wal_pressure()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let existing = Self::record_in(&tx, &key)?;
        let ticket = super::nonce()?;
        let op = match existing {
            None => {
                let count: i64 = tx
                    .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
                    .map_err(sql)?;
                if count >= CAPACITY {
                    return Err("operation_store_full: resolve or wait for older operations to expire before new durable requests".into());
                }
                if live_bytes(&tx)? >= HIGH_WATER_BYTES {
                    return Err("store_full: the operation store holds 224 MiB of data; resolve or wait for older operations to expire".into());
                }
                let seq = Self::next_seq(&tx)?;
                tx.execute(
                    "INSERT INTO operations (key, namespace, action, client_id, explicit, target, run, boot,
                       digest, payload_bytes, state, ticket, attempts, lease_until_ms, owner_pid,
                       created_ms, updated_ms, store_seq)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1, ?13, ?14, ?15, ?15, ?16)",
                    params![
                        key,
                        new.namespace,
                        new.action,
                        new.id,
                        new.explicit,
                        new.target,
                        new.run,
                        new.boot,
                        new.digest,
                        new.payload_bytes as i64,
                        DISPATCHING,
                        ticket,
                        (now + LEASE_MS) as i64,
                        std::process::id() as i64,
                        now as i64,
                        seq
                    ],
                )
                .map_err(sql)?;
                tx.last_insert_rowid()
            }
            Some(record) => {
                let retryable = RETRYABLE.contains(&record.state.as_str());
                // A client key binds its content for good; only a slot that
                // masil numbered itself may carry new content after a proven no-op.
                let rebind = retryable && !record.explicit;
                if record.target != new.target && !rebind {
                    return Err("operation key was already used for a different target".into());
                }
                if record.digest != new.digest && !rebind {
                    return Err("operation was already used with different content".into());
                }
                if record.state == DISPATCHING {
                    if record.lease_until_ms > now {
                        return Err(format!(
                            "outcome_unknown: operation {key} is in progress; query its receipt before retrying"
                        ));
                    }
                    return Ok(Admission::NeedsReconcile(record));
                }
                if !retryable {
                    return Ok(Admission::Recorded(record));
                }
                let seq = Self::next_seq(&tx)?;
                tx.execute(
                    "UPDATE operations SET state = ?2, digest = ?3, payload_bytes = ?4, target = ?5,
                       ticket = ?6, attempts = attempts + 1, lease_until_ms = ?7, owner_pid = ?8,
                       updated_ms = ?9, store_seq = ?10 WHERE id = ?1",
                    params![
                        record.op,
                        DISPATCHING,
                        new.digest,
                        new.payload_bytes as i64,
                        new.target,
                        ticket,
                        (now + LEASE_MS) as i64,
                        std::process::id() as i64,
                        now as i64,
                        seq
                    ],
                )
                .map_err(sql)?;
                record.op
            }
        };
        let mut intent = intent;
        if let Some(object) = intent.as_object_mut() {
            object.insert("ticket".into(), json!(ticket));
            object.insert("digest".into(), json!(new.digest));
        }
        Self::append(&tx, op, "accepted_durable", "masil", None, now)?;
        Self::append(&tx, op, "dispatch_intent", "masil", Some(&intent), now)?;
        if let Some(queued) = queued {
            let marked = tx
                .execute(
                    "UPDATE prompt_queue SET operation_key = ?1, op_state = ?2, op_updated_ms = ?3,
                       revision = revision + 1, note = NULL, updated_ms = ?3
                     WHERE id = ?4 AND revision = ?5 AND run = ?6",
                    params![
                        key,
                        DISPATCHING,
                        now as i64,
                        queued.item,
                        queued.revision,
                        queued.run
                    ],
                )
                .map_err(sql)?;
            // Dropping the transaction undoes the admission.
            if marked != 1 {
                return Err(
                    "queue_stale: the queued prompt changed before it was sent; look at it again"
                        .into(),
                );
            }
        }
        tx.commit().map_err(sql)?;
        Ok(Admission::Dispatch(Ticket { op, key, ticket }))
    }

    /// Commit the result of this attempt. Fails if another process already
    /// resolved it (for example a reconcile after the lease expired).
    pub fn finish(
        &mut self,
        ticket: &Ticket,
        state: &str,
        source: &str,
        evidence: Option<&Value>,
        now: u64,
    ) -> Result<Record, String> {
        self.transition(
            ticket.op,
            &ticket.ticket,
            None,
            state,
            source,
            evidence,
            now,
        )
    }

    /// Replace `outcome_unknown` with a state established later: new
    /// evidence found by reconcile or an explicit user decision.
    pub fn settle_unknown(
        &mut self,
        record: &Record,
        state: &str,
        source: &str,
        evidence: Option<&Value>,
        now: u64,
    ) -> Result<Record, String> {
        if state == DISPATCHING || state == UNKNOWN {
            return Err("invalid operation transition".into());
        }
        let mut tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let seq = Self::next_seq(&tx)?;
        let changed = tx
            .execute(
                "UPDATE operations SET state = ?3, updated_ms = ?4, store_seq = ?5
                 WHERE id = ?1 AND ticket = ?2 AND state = ?6",
                params![record.op, record.ticket, state, now as i64, seq, UNKNOWN],
            )
            .map_err(sql)?;
        if changed != 1 {
            return Err("operation receipt changed concurrently; query it again".into());
        }
        Self::append(&tx, record.op, state, source, evidence, now)?;
        let updated =
            Self::record_in(&tx, &record.operation_key)?.ok_or("operation disappeared")?;
        let source_ref = format!("{}:{}", record.operation_key, record.ticket);
        Self::inbox_best_effort(&mut tx, |conn| {
            conn.execute(
                "UPDATE inbox_events SET resolved_ms = ?2, resolution = 'settled'
                 WHERE source = 'operation' AND source_ref = ?1 AND resolved_ms IS NULL",
                params![source_ref, now as i64],
            )
            .map(drop)
            .map_err(sql)
        })?;
        tx.commit().map_err(sql)?;
        Ok(updated)
    }

    /// Resolve an attempt whose lease expired, using evidence gathered by the caller.
    pub fn resolve(
        &mut self,
        record: &Record,
        state: &str,
        source: &str,
        evidence: Option<&Value>,
        now: u64,
    ) -> Result<Record, String> {
        self.transition(
            record.op,
            &record.ticket,
            Some(now),
            state,
            source,
            evidence,
            now,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn transition(
        &mut self,
        op: i64,
        ticket: &str,
        expired_at: Option<u64>,
        state: &str,
        source: &str,
        evidence: Option<&Value>,
        now: u64,
    ) -> Result<Record, String> {
        if state == DISPATCHING {
            return Err("invalid operation transition".into());
        }
        let mut tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let seq = Self::next_seq(&tx)?;
        let changed = tx
            .execute(
                "UPDATE operations SET state = ?3, updated_ms = ?4, store_seq = ?5
                 WHERE id = ?1 AND ticket = ?2 AND state = ?6
                   AND (?7 IS NULL OR lease_until_ms <= ?7)",
                params![
                    op,
                    ticket,
                    state,
                    now as i64,
                    seq,
                    DISPATCHING,
                    expired_at.map(|at| at as i64)
                ],
            )
            .map_err(sql)?;
        if changed != 1 {
            return Err("operation receipt changed concurrently; query it again".into());
        }
        Self::append(&tx, op, state, source, evidence, now)?;
        let key: String = tx
            .query_row("SELECT key FROM operations WHERE id = ?1", [op], |row| {
                row.get(0)
            })
            .map_err(sql)?;
        let record = Self::record_in(&tx, &key)?.ok_or("operation disappeared")?;
        if state == UNKNOWN {
            // An unknown outcome needs a person; the inbox records each
            // attempt's once. Best-effort: a failed insert is rolled back
            // to the savepoint and the transition still commits.
            let source_ref = format!("{}:{}", record.operation_key, record.ticket);
            let event = InboxEvent {
                source: "operation",
                source_ref: &source_ref,
                provider: "",
                pane: &record.target,
                run: record.run.as_deref().unwrap_or(""),
                revision: None,
                kind: "operation_unknown",
                native_ref: Some(&record.operation_key),
                summary: Some(json!({"action": record.action})),
                observed_ms: now,
            };
            Self::inbox_best_effort(&mut tx, |conn| {
                Self::inbox_insert_in(conn, &event).map(drop)
            })?;
        }
        tx.commit().map_err(sql)?;
        Ok(record)
    }

    pub fn get(&self, key: &str) -> Result<Option<Record>, String> {
        Self::record_in(&self.conn, key)
    }

    fn record_in(conn: &Connection, key: &str) -> Result<Option<Record>, String> {
        let record = conn
            .query_row(
                "SELECT id, key, action, client_id, explicit, target, run, boot, digest, payload_bytes,
                        state, ticket, attempts, lease_until_ms, created_ms, updated_ms
                 FROM operations WHERE key = ?1",
                [key],
                |row| {
                    Ok(Record {
                        op: row.get(0)?,
                        operation_key: row.get(1)?,
                        action: row.get(2)?,
                        id: row.get(3)?,
                        explicit: row.get(4)?,
                        target: row.get(5)?,
                        run: row.get(6)?,
                        boot: row.get(7)?,
                        digest: row.get(8)?,
                        payload_bytes: row.get::<_, i64>(9)? as u64,
                        state: row.get(10)?,
                        ticket: row.get(11)?,
                        attempts: row.get(12)?,
                        lease_until_ms: row.get::<_, i64>(13)? as u64,
                        created_ms: row.get::<_, i64>(14)? as u64,
                        updated_ms: row.get::<_, i64>(15)? as u64,
                        receipts: Vec::new(),
                    })
                },
            )
            .optional()
            .map_err(sql)?;
        let Some(mut record) = record else {
            return Ok(None);
        };
        let mut statement = conn
            .prepare(
                "SELECT ordinal, stage, source, evidence, at_ms FROM receipts
                 WHERE operation_id = ?1 ORDER BY ordinal",
            )
            .map_err(sql)?;
        record.receipts = statement
            .query_map([record.op], |row| {
                let evidence: Option<String> = row.get(3)?;
                Ok(ReceiptRow {
                    ordinal: row.get(0)?,
                    stage: row.get(1)?,
                    source: row.get(2)?,
                    evidence: evidence.and_then(|text| serde_json::from_str(&text).ok()),
                    at_ms: row.get::<_, i64>(4)? as u64,
                })
            })
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)?;
        Ok(Some(record))
    }

    /// Most recent operations first; `unresolved` keeps dispatching and unknown outcomes.
    pub fn list(&self, unresolved: bool, limit: usize) -> Result<Vec<Record>, String> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT key FROM operations
                 WHERE ?1 = 0 OR state IN ('dispatching', 'outcome_unknown')
                 ORDER BY store_seq DESC LIMIT ?2",
            )
            .map_err(sql)?;
        let keys = statement
            .query_map(params![unresolved, limit as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        keys.iter()
            .filter_map(|key| Self::record_in(&self.conn, key).transpose())
            .collect()
    }

    /// Attempts whose lease expired without a committed result.
    pub fn expired(&self, now: u64, limit: usize) -> Result<Vec<Record>, String> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT key FROM operations WHERE state = ?1 AND lease_until_ms <= ?2
                 ORDER BY store_seq LIMIT ?3",
            )
            .map_err(sql)?;
        let keys = statement
            .query_map(params![DISPATCHING, now as i64, limit as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        keys.iter()
            .filter_map(|key| Self::record_in(&self.conn, key).transpose())
            .collect()
    }

    pub fn needs_prune(&self, now: u64) -> Result<bool, String> {
        let by_records: bool = self
            .conn
            .query_row(
                "SELECT COUNT(*) > ?1 OR COALESCE(MIN(updated_ms), ?2) < ?3 FROM operations",
                params![
                    PRUNE_ABOVE,
                    now as i64,
                    now.saturating_sub(RETENTION_MS) as i64
                ],
                |row| row.get(0),
            )
            .map_err(sql)?;
        Ok(by_records || live_bytes(&self.conn)? >= SOFT_BYTES)
    }

    /// Refuses admission while the WAL holds more than the pressure limit
    /// that checkpoints could not move. The file size is checked first:
    /// journal_size_limit keeps it small unless a reader blocks checkpoints.
    fn check_wal_pressure(&self) -> Result<(), String> {
        let mut wal = self.path.as_os_str().to_owned();
        wal.push("-wal");
        let size = fs::metadata(PathBuf::from(wal))
            .map(|metadata| metadata.len() as i64)
            .unwrap_or(0);
        if size < WAL_PRESSURE_BYTES {
            return Ok(());
        }
        let (log, moved): (i64, i64) = self
            .conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(1)?, row.get(2)?))
            })
            .map_err(sql)?;
        let page: i64 = self
            .conn
            .query_row("PRAGMA page_size", [], |row| row.get(0))
            .map_err(sql)?;
        if (log - moved).max(0) * page >= WAL_PRESSURE_BYTES {
            return Err("store_busy: a reader holds the operation store's write-ahead log; close long-running masil-agent readers and retry".into());
        }
        Ok(())
    }

    /// Delete resolved operations whose namespace can no longer be retried:
    /// runs that are not live and server boots other than `boot`. Unresolved
    /// attempts and active namespaces are never deleted.
    pub fn prune(
        &mut self,
        live_runs: &HashSet<String>,
        boot: &str,
        now: u64,
    ) -> Result<usize, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let candidates = {
            let mut statement = tx
                .prepare(
                    "SELECT id, run, boot, explicit, state, updated_ms FROM operations
                     WHERE state != ?1 ORDER BY updated_ms",
                )
                .map_err(sql)?;
            statement
                .query_map([DISPATCHING], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, bool>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)? as u64,
                    ))
                })
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)?
        };
        Self::check_reader_in(&tx)?;
        let total = candidates.len() as i64;
        let oversize = live_bytes(&tx)? >= SOFT_BYTES;
        let mut removed = 0usize;
        for (id, run, op_boot, _explicit, state, updated) in candidates {
            let active = match &run {
                Some(run) => live_runs.contains(run),
                None => op_boot == boot,
            };
            let old = updated + RETENTION_MS <= now;
            let over = total - (removed as i64) > PRUNE_ABOVE;
            // A live namespace keeps everything: an automatic outcome_unknown
            // record is what `operation resolve` needs to release its slot.
            // Unknown outcomes also stay for the full retention period.
            let deletable = !active && (old || ((over || oversize) && state != "outcome_unknown"));
            if deletable {
                tx.execute("DELETE FROM operations WHERE id = ?1", [id])
                    .map_err(sql)?;
                removed += 1;
            }
        }
        tx.commit().map_err(sql)?;
        Ok(removed)
    }

    /// Opens the existing store for `socket` without creating anything, for
    /// event producers: None when there is no store yet or its instance is
    /// not the one the server recorded.
    pub fn open_existing(socket: &Path, instance: &str) -> Result<Option<Self>, String> {
        let Ok(directory) = state_base().map(|base| base.join("masil/operations")) else {
            return Ok(None);
        };
        let Ok(directory) = directory.canonicalize() else {
            return Ok(None);
        };
        let path = directory.join(store_name(socket));
        if fs::symlink_metadata(&path).is_err() {
            return Ok(None);
        }
        let store = Self::open_path(&path)?;
        // Producers never wait long: an event is best-effort.
        store
            .conn
            .busy_timeout(Duration::from_millis(200))
            .map_err(sql)?;
        Ok((store.instance()?.to_string() == instance).then_some(store))
    }

    /// The store for `socket` opened read-only without creating anything;
    /// None when it does not exist or predates schema 2.
    fn readonly(socket: &Path) -> Result<Option<Self>, String> {
        let Ok(socket) = socket.canonicalize() else {
            return Ok(None);
        };
        let Ok(directory) = state_base().map(|base| base.join("masil/operations")) else {
            return Ok(None);
        };
        let Ok(directory) = directory.canonicalize() else {
            return Ok(None);
        };
        let path = directory.join(store_name(&socket));
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return Ok(None);
        };
        super::store::validate_private_metadata(&metadata, "operation store")?;
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(sql)?;
        conn.busy_timeout(Duration::from_millis(200)).map_err(sql)?;
        if Self::user_version(&conn)? < SCHEMA_VERSION {
            return Ok(None);
        }
        Ok(Some(Self { conn, path }))
    }

    /// The store's inbox switch and instance, read without creating or
    /// changing anything; None when there is no store.
    pub fn inbox_probe(socket: &Path) -> Option<(bool, String)> {
        let store = Self::readonly(socket).ok().flatten()?;
        let instance = store.instance().ok()?.to_string();
        Some((store.inbox_enabled().unwrap_or(false), instance))
    }

    /// The store's inbox switch, or why it could not be read.
    pub fn inbox_flag_checked(socket: &Path) -> Result<bool, String> {
        match Self::readonly(socket)? {
            Some(store) => store.inbox_enabled(),
            None => Ok(false),
        }
    }

    /// The inbox read without creating or changing anything; None when there
    /// is no store or no inbox table yet.
    pub fn inbox_readonly(socket: &Path, all: bool, limit: usize) -> Result<Option<Value>, String> {
        let Some(store) = Self::readonly(socket)? else {
            return Ok(None);
        };
        let has_table: bool = store
            .conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'inbox_events')",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if !has_table {
            return Ok(None);
        }
        let mut value = store.inbox(all, limit)?;
        value["enabled"] = json!(store.inbox_enabled()?);
        value["unseen"] = json!(store.inbox_unseen_count()?);
        Ok(Some(value))
    }

    /// Whether the inbox is switched on; false before the inbox feature
    /// exists in this store.
    pub fn inbox_enabled(&self) -> Result<bool, String> {
        Self::inbox_enabled_in(&self.conn)
    }

    fn inbox_enabled_in(conn: &Connection) -> Result<bool, String> {
        Ok(conn
            .query_row(
                "SELECT value FROM meta WHERE name = 'inbox_enabled'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(sql)?
            == Some(1))
    }

    /// Runs an inbox write inside a savepoint of an operation's
    /// transaction. A failure is logged and rolled back to the savepoint, so
    /// the operation still commits.
    fn inbox_best_effort(
        tx: &mut rusqlite::Transaction<'_>,
        write: impl FnOnce(&Connection) -> Result<(), String>,
    ) -> Result<(), String> {
        let savepoint = tx.savepoint().map_err(sql)?;
        match write(&savepoint) {
            Ok(()) => savepoint.commit().map_err(sql),
            Err(error) => {
                super::inbox::log("inbox.log", &json!({"error": error}));
                Ok(())
            }
        }
    }

    pub fn set_inbox_enabled(&mut self, enabled: bool) -> Result<(), String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        // In the same transaction as `set_notify`'s check, so the two never
        // end up as notifications on with the inbox off.
        if !enabled && Self::notify_enabled_in(&tx)? {
            return Err("inbox_in_use: notifications read the inbox; run `masil-agent agent notify disable` first".into());
        }
        tx.execute(
            "UPDATE meta SET value = ?1 WHERE name = 'inbox_enabled'",
            [i64::from(enabled)],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)
    }

    // Notifications.

    fn notify_enabled_in(conn: &Connection) -> Result<bool, String> {
        Ok(conn
            .query_row(
                "SELECT value FROM meta WHERE name = 'notify_enabled'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(sql)?
            == Some(1))
    }

    pub fn notify_enabled(&self) -> Result<bool, String> {
        Self::notify_enabled_in(&self.conn)
    }

    /// Turns notifications on or off. Turning them on starts after the
    /// latest event, so nothing already recorded is notified; turning them
    /// on again keeps the cursor. Either way `env` replaces the environment
    /// the routes run in.
    pub fn set_notify(&mut self, enabled: bool, env: &[(String, String)]) -> Result<(), String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        if enabled && !Self::inbox_enabled_in(&tx)? {
            return Err("inbox_in_use: the inbox was switched off meanwhile; run `masil-agent agent notify enable` again".into());
        }
        if enabled && !Self::notify_enabled_in(&tx)? {
            tx.execute(
                "UPDATE meta SET value = (SELECT value FROM meta WHERE name = 'store_seq')
                 WHERE name = 'notify_seq'",
                [],
            )
            .map_err(sql)?;
        }
        if enabled {
            tx.execute("DELETE FROM notify_env", []).map_err(sql)?;
            for (name, value) in env {
                tx.execute(
                    "INSERT INTO notify_env (name, value) VALUES (?1, ?2)",
                    params![name, value],
                )
                .map_err(sql)?;
            }
        }
        tx.execute(
            "UPDATE meta SET value = ?1 WHERE name = 'notify_enabled'",
            [i64::from(enabled)],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)
    }

    pub fn notify_env(&self) -> Result<Vec<(String, String)>, String> {
        let mut statement = self
            .conn
            .prepare("SELECT name, value FROM notify_env ORDER BY name")
            .map_err(sql)?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    /// Claims the events recorded since the last claim: the cursor moves
    /// past every one read, and each event of `kinds` gets a log row per
    /// route, `sending`, or `skipped_stale` when observed before
    /// `stale_before`. Acknowledged, resolved and read-all events are
    /// passed over. Nothing is claimed while notifications are off.
    pub fn notify_claim(
        &mut self,
        kinds: &[String],
        routes: &[String],
        stale_before: u64,
        now: u64,
    ) -> Result<NotifyClaim, String> {
        let mut claim = NotifyClaim::default();
        // Most passes find nothing new; they take no write lock. The cursor
        // follows `store_seq` (operations move it too), so this stays false
        // until something is recorded.
        let pending: bool = self
            .conn
            .query_row(
                "SELECT COALESCE((SELECT value FROM meta WHERE name = 'notify_enabled') = 1
                   AND (SELECT value FROM meta WHERE name = 'store_seq')
                     > (SELECT value FROM meta WHERE name = 'notify_seq'), 0)",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if !pending {
            return Ok(claim);
        }
        loop {
            match self.notify_claim_batch(kinds, routes, stale_before, now, &mut claim) {
                Ok(true) => {}
                Ok(false) => return Ok(claim),
                // Batches already committed are claimed: they go out, and
                // the rest waits for the next pass.
                Err(_) if !claim.events.is_empty() || claim.stale > 0 => return Ok(claim),
                Err(error) => return Err(error),
            }
        }
    }

    /// One batch of [`Store::notify_claim`], committed on its own; true if
    /// there may be more.
    fn notify_claim_batch(
        &mut self,
        kinds: &[String],
        routes: &[String],
        stale_before: u64,
        now: u64,
        claim: &mut NotifyClaim,
    ) -> Result<bool, String> {
        struct Row {
            id: i64,
            seq: i64,
            kind: String,
            provider: String,
            pane: String,
            run: String,
            summary: Option<String>,
            observed: i64,
            closed: bool,
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        if !Self::notify_enabled_in(&tx)? {
            return Ok(false);
        }
        let meta = |name: &str| -> Result<i64, String> {
            tx.query_row("SELECT value FROM meta WHERE name = ?1", [name], |row| {
                row.get(0)
            })
            .map_err(sql)
        };
        let (cursor, fence, latest) = (
            meta("notify_seq")?,
            meta("inbox_fence")?,
            meta("store_seq")?,
        );
        let rows: Vec<Row> = {
            let mut statement = tx
                .prepare(
                    "SELECT id, store_seq, kind, provider, pane, run, summary, observed_ms,
                       acked_ms IS NOT NULL OR resolved_ms IS NOT NULL
                     FROM inbox_events WHERE store_seq > ?1 ORDER BY store_seq LIMIT ?2",
                )
                .map_err(sql)?;
            statement
                .query_map(params![cursor, NOTIFY_BATCH], |row| {
                    Ok(Row {
                        id: row.get(0)?,
                        seq: row.get(1)?,
                        kind: row.get(2)?,
                        provider: row.get(3)?,
                        pane: row.get(4)?,
                        run: row.get(5)?,
                        summary: row.get(6)?,
                        observed: row.get(7)?,
                        closed: row.get(8)?,
                    })
                })
                .map_err(sql)?
                .collect::<Result<_, _>>()
                .map_err(sql)?
        };
        for row in &rows {
            if row.closed || row.seq <= fence || !kinds.contains(&row.kind) {
                continue;
            }
            let stale = (row.observed as u64) < stale_before;
            let outcome = if stale { "skipped_stale" } else { "sending" };
            let mut logged = Vec::new();
            for route in routes {
                tx.execute(
                    "INSERT OR IGNORE INTO notify_log (event_id, store_seq, route, created_ms, outcome)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![row.id, row.seq, route, now as i64, outcome],
                )
                .map_err(sql)?;
                if tx.changes() == 1 {
                    logged.push((route.clone(), tx.last_insert_rowid()));
                }
            }
            if stale {
                claim.stale += 1;
            } else if !logged.is_empty() {
                claim.events.push(NotifyEvent {
                    id: row.id,
                    kind: row.kind.clone(),
                    provider: row.provider.clone(),
                    pane: row.pane.clone(),
                    run: row.run.clone(),
                    summary: row
                        .summary
                        .as_deref()
                        .and_then(|text| serde_json::from_str(text).ok()),
                    observed_ms: row.observed as u64,
                    rows: logged,
                });
            }
        }
        let more = rows.len() as i64 >= NOTIFY_BATCH;
        // Read to the end: under the write lock nothing can still commit
        // below `store_seq`, so the cursor goes all the way.
        let cursor = if more {
            rows.last().map_or(cursor, |row| row.seq)
        } else {
            latest
        };
        tx.execute(
            "UPDATE meta SET value = ?1 WHERE name = 'notify_seq'",
            [cursor],
        )
        .map_err(sql)?;
        if !more {
            // Keep the log bounded while here.
            tx.execute(
                "DELETE FROM notify_log WHERE created_ms < ?1
                   OR id <= (SELECT id FROM notify_log ORDER BY id DESC LIMIT 1 OFFSET ?2)",
                params![now.saturating_sub(NOTIFY_RETENTION_MS) as i64, NOTIFY_KEEP],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
        Ok(more)
    }

    /// How sending went for claimed rows.
    pub fn notify_outcome(&mut self, rows: &[i64], outcome: &str) -> Result<(), String> {
        let tx = self.conn.transaction().map_err(sql)?;
        for row in rows {
            tx.execute(
                "UPDATE notify_log SET outcome = ?2 WHERE id = ?1",
                params![row, outcome],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)
    }

    // Schedules.

    const SCHEDULE_COLUMNS: &'static str = "s.id, s.name, s.spec, s.action, s.enabled, s.created_ms, s.changed_ms,
        (SELECT MAX(due_ms) FROM schedule_runs r WHERE r.schedule_id = s.id),
        (SELECT state FROM schedule_runs r WHERE r.schedule_id = s.id ORDER BY due_ms DESC LIMIT 1)";

    fn schedule_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduleRow> {
        let action: String = row.get(3)?;
        Ok(ScheduleRow {
            id: row.get(0)?,
            name: row.get(1)?,
            spec: row.get(2)?,
            action: serde_json::from_str(&action).unwrap_or(Value::Null),
            enabled: row.get(4)?,
            created_ms: row.get(5)?,
            changed_ms: row.get(6)?,
            last_due: row.get(7)?,
            last_state: row.get(8)?,
        })
    }

    pub fn schedules(&self) -> Result<Vec<ScheduleRow>, String> {
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT {} FROM schedules s ORDER BY s.name",
                Self::SCHEDULE_COLUMNS
            ))
            .map_err(sql)?;
        statement
            .query_map([], Self::schedule_row)
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    fn schedule_named_in(tx: &Connection, name: &str) -> Result<ScheduleRow, String> {
        tx.query_row(
            &format!(
                "SELECT {} FROM schedules s WHERE s.name = ?1",
                Self::SCHEDULE_COLUMNS
            ),
            [name],
            Self::schedule_row,
        )
        .optional()
        .map_err(sql)?
        .ok_or_else(|| format!("schedule_absent: no schedule named {name}"))
    }

    pub fn schedule_add(
        &mut self,
        name: &str,
        spec: &str,
        action: &Value,
        now: u64,
    ) -> Result<ScheduleRow, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        let count: i64 = tx
            .query_row("SELECT COUNT(*) FROM schedules", [], |row| row.get(0))
            .map_err(sql)?;
        if count >= crate::schedule::MAX_SCHEDULES {
            return Err(format!(
                "schedule_invalid: a server holds at most {} schedules",
                crate::schedule::MAX_SCHEDULES
            ));
        }
        let inserted = tx
            .execute(
                "INSERT INTO schedules (name, spec, action, enabled, created_ms, changed_ms)
                 VALUES (?1, ?2, ?3, 1, ?4, ?4) ON CONFLICT (name) DO NOTHING",
                params![name, spec, action.to_string(), now as i64],
            )
            .map_err(sql)?;
        if inserted == 0 {
            return Err(format!("schedule_exists: a schedule named {name} exists"));
        }
        let row = Self::schedule_named_in(&tx, name)?;
        tx.commit().map_err(sql)?;
        Ok(row)
    }

    /// Switches a schedule on or off. Either way its times up to now are
    /// settled: switching on never makes up for the time it was off.
    pub fn schedule_set_enabled(
        &mut self,
        name: &str,
        enabled: bool,
        now: u64,
    ) -> Result<(), String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        Self::schedule_named_in(&tx, name)?;
        tx.execute(
            "UPDATE schedules SET enabled = ?2, changed_ms = ?3 WHERE name = ?1",
            params![name, enabled, now as i64],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)
    }

    pub fn schedule_remove(&mut self, name: &str) -> Result<(), String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        let row = Self::schedule_named_in(&tx, name)?;
        tx.execute("DELETE FROM schedule_runs WHERE schedule_id = ?1", [row.id])
            .map_err(sql)?;
        tx.execute("DELETE FROM schedules WHERE id = ?1", [row.id])
            .map_err(sql)?;
        tx.commit().map_err(sql)
    }

    pub fn schedule_runs(&self, name: &str, limit: usize) -> Result<Vec<Value>, String> {
        let row = Self::schedule_named_in(&self.conn, name)?;
        let mut statement = self
            .conn
            .prepare(
                "SELECT due_ms, state, started_ms, ended_ms, result FROM schedule_runs
                 WHERE schedule_id = ?1 ORDER BY due_ms DESC LIMIT ?2",
            )
            .map_err(sql)?;
        statement
            .query_map(params![row.id, limit as i64], |row| {
                let result: Option<String> = row.get(4)?;
                Ok(json!({
                    "due_ms": row.get::<_, i64>(0)?,
                    "state": row.get::<_, String>(1)?,
                    "started_ms": row.get::<_, Option<i64>>(2)?,
                    "ended_ms": row.get::<_, Option<i64>>(3)?,
                    "result": result.and_then(|text| serde_json::from_str::<Value>(&text).ok()),
                }))
            })
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    /// Settles what the scheduler found for one schedule, if the schedule
    /// is still on and unchanged since it was read (`changed_ms`): one
    /// `missed` row for the latest missed time (and an inbox event, when
    /// the inbox is on), and a `running` row for the time to run, claimed
    /// only while `now` is within the grace. A row already there means
    /// another settled it; nothing is claimed twice.
    pub fn schedule_record(
        &mut self,
        id: i64,
        changed_ms: i64,
        missed: Option<crate::schedule::Missed>,
        run: Option<i64>,
        now: u64,
    ) -> Result<ScheduleClaim, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        let current: Option<(String, bool, i64)> = tx
            .query_row(
                "SELECT name, enabled, changed_ms FROM schedules WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sql)?;
        let mut claim = ScheduleClaim::default();
        let Some((name, true, current_changed)) = current else {
            return Ok(claim);
        };
        if current_changed != changed_ms {
            return Ok(claim);
        }
        let now = now as i64;
        // A time to run that is late by now is missed after all.
        let (run, missed) = match run {
            Some(due) if now - due > crate::schedule::GRACE_MS => (
                None,
                Some(crate::schedule::Missed {
                    due_ms: due,
                    count: missed.map_or(0, |missed| missed.count) + 1,
                    at_least: missed.is_some_and(|missed| missed.at_least),
                }),
            ),
            other => (other, missed),
        };
        if let Some(missed) = missed {
            let result = json!({"missed": missed.count, "at_least": missed.at_least});
            let inserted = tx
                .execute(
                    "INSERT INTO schedule_runs (schedule_id, due_ms, state, ended_ms, result)
                     VALUES (?1, ?2, 'missed', ?3, ?4) ON CONFLICT DO NOTHING",
                    params![id, missed.due_ms, now, result.to_string()],
                )
                .map_err(sql)?;
            if inserted == 1 {
                let source_ref = format!("{id}:{}", missed.due_ms);
                // The event is a courtesy: a full store keeps the run rows.
                claim.event = Self::inbox_insert_in(
                    &tx,
                    &InboxEvent {
                        source: "schedule",
                        source_ref: &source_ref,
                        provider: "",
                        pane: "",
                        run: "",
                        revision: None,
                        kind: "missed_schedule",
                        native_ref: None,
                        summary: Some(json!({
                            "schedule": name,
                            "due_ms": missed.due_ms,
                            "missed": missed.count,
                            "at_least": missed.at_least,
                        })),
                        observed_ms: now as u64,
                    },
                )
                .is_ok_and(|event| event.is_some());
            }
        }
        if let Some(due) = run {
            let inserted = tx
                .execute(
                    "INSERT INTO schedule_runs (schedule_id, due_ms, state, started_ms)
                     VALUES (?1, ?2, 'running', ?3) ON CONFLICT DO NOTHING",
                    params![id, due, now],
                )
                .map_err(sql)?;
            if inserted == 1 {
                claim.run = Some(due);
            }
        }
        tx.execute(
            "DELETE FROM schedule_runs WHERE schedule_id = ?1 AND id <=
               (SELECT id FROM schedule_runs WHERE schedule_id = ?1 ORDER BY id DESC LIMIT 1 OFFSET ?2)",
            params![id, SCHEDULE_RUNS_KEPT],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(claim)
    }

    /// How a claimed run ended. A row settled otherwise meanwhile (marked
    /// unknown by a later coordinator) keeps that.
    pub fn schedule_finish(
        &mut self,
        id: i64,
        due_ms: i64,
        state: &str,
        result: &Value,
        now: u64,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE schedule_runs SET state = ?3, ended_ms = ?4, result = ?5
                 WHERE schedule_id = ?1 AND due_ms = ?2 AND state = 'running'",
                params![id, due_ms, state, now as i64, result.to_string()],
            )
            .map(drop)
            .map_err(sql)
    }

    /// Runs a coordinator that ended left `running`: started before
    /// `before_ms`, their outcome is unknown. They are never run again.
    pub fn schedule_settle(&mut self, before_ms: u64, now: u64) -> Result<usize, String> {
        self.conn
            .execute(
                "UPDATE schedule_runs SET state = 'unknown', ended_ms = ?2
                 WHERE state = 'running' AND started_ms < ?1",
                params![before_ms as i64, now as i64],
            )
            .map_err(sql)
    }

    /// Notification status read without creating or changing anything;
    /// None when there is no store or no notification table yet.
    pub fn notify_readonly(socket: &Path) -> Result<Option<Value>, String> {
        let Some(store) = Self::readonly(socket)? else {
            return Ok(None);
        };
        let has_table: bool = store
            .conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'notify_log')",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if !has_table {
            return Ok(None);
        }
        store.notify_status().map(Some)
    }

    /// Whether notifications are on, where the cursor is, and the latest
    /// log rows.
    pub fn notify_status(&self) -> Result<Value, String> {
        let cursor: i64 = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE name = 'notify_seq'",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        let mut statement = self
            .conn
            .prepare(
                "SELECT l.event_id, l.route, l.created_ms, l.outcome, e.kind, e.pane
                 FROM notify_log l LEFT JOIN inbox_events e ON e.id = l.event_id
                 ORDER BY l.id DESC LIMIT 20",
            )
            .map_err(sql)?;
        let recent: Vec<Value> = statement
            .query_map([], |row| {
                Ok(json!({
                    "event": row.get::<_, i64>(0)?,
                    "route": row.get::<_, String>(1)?,
                    "created_ms": row.get::<_, i64>(2)?,
                    "outcome": row.get::<_, String>(3)?,
                    "kind": row.get::<_, Option<String>>(4)?,
                    "pane": row.get::<_, Option<String>>(5)?,
                }))
            })
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)?;
        let unsent: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM notify_log WHERE outcome = 'sending'",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        Ok(json!({
            "enabled": self.notify_enabled()?,
            "inbox_enabled": self.inbox_enabled()?,
            "cursor": cursor,
            "interrupted": unsent,
            "recent": recent,
        }))
    }

    /// Records one attention event if the inbox is on; an existing event
    /// with the same source key is left as it is. Returns the new event ID.
    pub fn record_event(&mut self, event: &InboxEvent<'_>) -> Result<Option<i64>, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let id = Self::inbox_insert_in(&tx, event)?;
        tx.commit().map_err(sql)?;
        Ok(id)
    }

    fn inbox_insert_in(tx: &Connection, event: &InboxEvent<'_>) -> Result<Option<i64>, String> {
        if !Self::inbox_enabled_in(tx)? {
            return Ok(None);
        }
        if live_bytes(tx)? >= HIGH_WATER_BYTES {
            return Err("store_full: the operation store holds 224 MiB of data".into());
        }
        if event.source == "screen" {
            // A provider callback already recorded this request more
            // precisely than the screen can. A run whose turn ends a hook
            // records needs no screen return to idle.
            let covered: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM inbox_events
                       WHERE run = ?1 AND (source LIKE 'hook:%' OR source LIKE 'api:%') AND (
                         (?2 = 'blocked' AND kind IN ('blocked', 'approval_requested', 'question_asked')
                            AND resolved_ms IS NULL)
                         OR (?2 = 'returned_idle' AND kind = 'turn_completed')))",
                    params![event.run, event.kind],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if covered {
                return Ok(None);
            }
        }
        let seq = Self::next_seq(tx)?;
        // A resolution seen first makes a late request arrive resolved.
        let resolution: Option<(i64, String)> = tx
            .query_row(
                "SELECT resolved_ms, resolution FROM inbox_resolutions WHERE source = ?1 AND source_ref = ?2",
                params![event.source, event.source_ref],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql)?;
        let summary = event
            .summary
            .as_ref()
            .map(Value::to_string)
            .filter(|text| text.len() <= INBOX_SUMMARY_BYTES);
        let inserted = tx
            .execute(
                "INSERT INTO inbox_events (store_seq, source, source_ref, provider, pane, run, revision,
                   kind, native_ref, summary, observed_ms, resolved_ms, resolution)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT (source, source_ref) DO NOTHING",
                params![
                    seq,
                    event.source,
                    event.source_ref,
                    event.provider,
                    event.pane,
                    event.run,
                    event.revision,
                    event.kind,
                    event.native_ref,
                    summary,
                    event.observed_ms as i64,
                    resolution.as_ref().map(|(at, _)| *at),
                    resolution.as_ref().map(|(_, why)| why.as_str()),
                ],
            )
            .map_err(sql)?;
        let mut id = (inserted != 0).then(|| tx.last_insert_rowid());
        if id.is_none() && event.source.starts_with("hook:") && event.kind == "turn_completed" {
            // The same turn ends again after another Stop hook made it go
            // on, and the first end was resolved by the work in between or
            // already read: this end is new to the person. Hooks are not
            // redelivered, so a repeat is a real second end.
            id = tx
                .query_row(
                    "UPDATE inbox_events SET store_seq = ?3, observed_ms = ?4, acked_ms = NULL,
                       resolved_ms = NULL, resolution = NULL
                     WHERE source = ?1 AND source_ref = ?2
                       AND (resolution = 'next_turn' OR (resolved_ms IS NULL AND acked_ms IS NOT NULL))
                     RETURNING id",
                    params![
                        event.source,
                        event.source_ref,
                        seq,
                        event.observed_ms as i64
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql)?;
        }
        // Also for a repeated callback: a turn can end twice with one ID
        // when another Stop hook made it continue.
        if event.source.starts_with("hook:") {
            match event.kind {
                "blocked" | "approval_requested" | "question_asked" => {
                    // The callback describes the screen's `blocked` more precisely.
                    tx.execute(
                        "UPDATE inbox_events SET resolved_ms = ?2, resolution = 'covered'
                         WHERE run = ?1 AND source = 'screen' AND kind = 'blocked'
                           AND resolved_ms IS NULL",
                        params![event.run, event.observed_ms as i64],
                    )
                    .map_err(sql)?;
                }
                "turn_completed" => {
                    // Nothing in the run waits for the person once its turn
                    // ended, and a screen return to idle is this turn end.
                    tx.execute(
                        "UPDATE inbox_events SET resolved_ms = ?2, resolution = 'turn_completed'
                         WHERE run = ?1 AND resolved_ms IS NULL
                           AND kind IN ('blocked', 'approval_requested', 'question_asked')",
                        params![event.run, event.observed_ms as i64],
                    )
                    .map_err(sql)?;
                    tx.execute(
                        "UPDATE inbox_events SET resolved_ms = ?2, resolution = 'covered'
                         WHERE run = ?1 AND source = 'screen' AND kind = 'returned_idle'
                           AND resolved_ms IS NULL",
                        params![event.run, event.observed_ms as i64],
                    )
                    .map_err(sql)?;
                }
                _ => {}
            }
        }
        // A turn end read from the provider's own server is this turn's
        // screen return to idle. Requests of other sessions still wait, and
        // a read again after a restart is not a new turn end.
        if event.source.starts_with("api:") && event.kind == "turn_completed" {
            tx.execute(
                "UPDATE inbox_events SET resolved_ms = ?2, resolution = 'covered'
                 WHERE run = ?1 AND source = 'screen' AND kind = 'returned_idle'
                   AND resolved_ms IS NULL",
                params![event.run, event.observed_ms as i64],
            )
            .map_err(sql)?;
        }
        if id.is_some() {
            Self::inbox_prune_in(tx, event.observed_ms)?;
        }
        Ok(id)
    }

    /// Keeps at most INBOX_KEEP events for INBOX_RETENTION_MS, removing read
    /// or resolved events first; unread ones removed over the cap are counted.
    fn inbox_prune_in(tx: &Connection, now: u64) -> Result<(), String> {
        // A resolution waits only for a request that has not arrived yet.
        tx.execute(
            "DELETE FROM inbox_resolutions WHERE resolved_ms < ?1",
            [now.saturating_sub(INBOX_RETENTION_MS) as i64],
        )
        .map_err(sql)?;
        tx.execute(
            "DELETE FROM inbox_resolutions WHERE rowid IN (
               SELECT rowid FROM inbox_resolutions ORDER BY resolved_ms DESC LIMIT -1 OFFSET ?1)",
            [INBOX_KEEP],
        )
        .map_err(sql)?;
        let fence: i64 = tx
            .query_row(
                "SELECT value FROM meta WHERE name = 'inbox_fence'",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        tx.execute(
            "DELETE FROM inbox_events WHERE observed_ms < ?1",
            [now.saturating_sub(INBOX_RETENTION_MS) as i64],
        )
        .map_err(sql)?;
        let count: i64 = tx
            .query_row("SELECT COUNT(*) FROM inbox_events", [], |row| row.get(0))
            .map_err(sql)?;
        if count <= INBOX_KEEP {
            return Ok(());
        }
        let over = count - INBOX_KEEP;
        let removed_read = tx
            .execute(
                "DELETE FROM inbox_events WHERE id IN (
                   SELECT id FROM inbox_events
                   WHERE acked_ms IS NOT NULL OR resolved_ms IS NOT NULL OR store_seq <= ?1
                   ORDER BY store_seq LIMIT ?2)",
                params![fence, over],
            )
            .map_err(sql)? as i64;
        let unread = over - removed_read;
        if unread > 0 {
            tx.execute(
                "DELETE FROM inbox_events WHERE id IN (
                   SELECT id FROM inbox_events ORDER BY store_seq LIMIT ?1)",
                [unread],
            )
            .map_err(sql)?;
            tx.execute(
                "UPDATE meta SET value = value + ?1 WHERE name = 'inbox_dropped'",
                [unread],
            )
            .map_err(sql)?;
        }
        Ok(())
    }

    /// Marks the open request with this source key resolved, or remembers
    /// the resolution for a request that has not arrived yet.
    pub fn resolve_event(
        &mut self,
        source: &str,
        source_ref: &str,
        resolution: &str,
        now: u64,
    ) -> Result<(), String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        if !Self::inbox_enabled_in(&tx)? {
            return Ok(());
        }
        let known: bool = tx
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM inbox_events WHERE source = ?1 AND source_ref = ?2)",
                params![source, source_ref],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if known {
            tx.execute(
                "UPDATE inbox_events SET resolved_ms = ?3, resolution = ?4
                 WHERE source = ?1 AND source_ref = ?2 AND resolved_ms IS NULL",
                params![source, source_ref, now as i64, resolution],
            )
            .map_err(sql)?;
        } else {
            tx.execute(
                "INSERT INTO inbox_resolutions VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (source, source_ref) DO NOTHING",
                params![source, source_ref, now as i64, resolution],
            )
            .map_err(sql)?;
            Self::inbox_prune_in(&tx, now)?;
        }
        tx.commit().map_err(sql)
    }

    /// Clears sent bodies after a day and sent rows after a week. Writes
    /// only when something is due.
    fn queue_retention_in(tx: &Connection, now: u64) -> Result<(), String> {
        let body_before = now.saturating_sub(QUEUE_SENT_BODY_MS) as i64;
        let row_before = now.saturating_sub(QUEUE_SENT_ROW_MS) as i64;
        let due: bool = tx
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM prompt_queue WHERE sent_ms IS NOT NULL
                   AND (sent_ms <= ?2 OR (body IS NOT NULL AND sent_ms <= ?1)))",
                params![body_before, row_before],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if due {
            tx.execute(
                "DELETE FROM prompt_queue WHERE sent_ms IS NOT NULL AND sent_ms <= ?1",
                [row_before],
            )
            .map_err(sql)?;
            tx.execute(
                "UPDATE prompt_queue SET body = NULL WHERE body IS NOT NULL AND sent_ms <= ?1",
                [body_before],
            )
            .map_err(sql)?;
        }
        Ok(())
    }

    /// Refuses a change that would pass the queue's quota: kept bodies and
    /// attachments, counted in bytes.
    fn queue_quota_in(
        tx: &Connection,
        run: &str,
        added_items: i64,
        added_bytes: i64,
    ) -> Result<(), String> {
        let (items, bytes, run_items): (i64, i64, i64) = tx
            .query_row(
                "SELECT COUNT(*),
                   COALESCE(SUM(length(CAST(body AS BLOB)) + length(CAST(attachments AS BLOB))), 0),
                   COALESCE(SUM(run = ?1), 0)
                 FROM prompt_queue WHERE body IS NOT NULL",
                [run],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(sql)?;
        if items + added_items > QUEUE_ITEMS
            || run_items + added_items > QUEUE_RUN_ITEMS
            || bytes + added_bytes > QUEUE_BYTES
        {
            return Err(format!(
                "queue_full: the prompt queue holds at most {QUEUE_ITEMS} items ({QUEUE_RUN_ITEMS} per agent run) and 2 MiB; remove some first"
            ));
        }
        Ok(())
    }

    fn queue_item_in(tx: &Connection, id: i64, now: u64) -> Result<Option<QueueItem>, String> {
        tx.query_row(
            &format!("SELECT {QUEUE_COLUMNS} FROM prompt_queue WHERE id = ?1"),
            [id],
            QueueItem::from_row,
        )
        .optional()
        .map_err(sql)
        .map(|item| item.map(|item| item.hidden_after(now)))
    }

    /// Adds a staged prompt at the end of the run's queue.
    pub fn queue_add(
        &mut self,
        run: &str,
        pane: &str,
        provider: &str,
        body: &str,
        attachments: &Value,
        now: u64,
    ) -> Result<QueueItem, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        Self::queue_retention_in(&tx, now)?;
        let attachments = attachments.to_string();
        Self::queue_quota_in(&tx, run, 1, (body.len() + attachments.len()) as i64)?;
        let position: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(position), 0) + 1 FROM prompt_queue WHERE run = ?1",
                [run],
                |row| row.get(0),
            )
            .map_err(sql)?;
        tx.execute(
            "INSERT INTO prompt_queue (run, pane, provider, position, body, attachments, revision,
               created_ms, updated_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7)",
            params![run, pane, provider, position, body, attachments, now as i64],
        )
        .map_err(sql)?;
        let id = tx.last_insert_rowid();
        let item = Self::queue_item_in(&tx, id, now)?.ok_or("queue item disappeared")?;
        tx.commit().map_err(sql)?;
        Ok(item)
    }

    /// Adds drafts at the end of the run's queue in one transaction, leaving
    /// out any the run already waits on (unsent, same text); nothing is
    /// added if the rest does not fit.
    pub fn queue_add_drafts(
        &mut self,
        run: &str,
        pane: &str,
        provider: &str,
        drafts: &[String],
        now: u64,
    ) -> Result<usize, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        Self::queue_retention_in(&tx, now)?;
        let mut waiting: std::collections::HashSet<String> = {
            let mut statement = tx
                .prepare(
                    "SELECT body FROM prompt_queue
                     WHERE run = ?1 AND body IS NOT NULL AND sent_ms IS NULL",
                )
                .map_err(sql)?;
            statement
                .query_map([run], |row| row.get::<_, String>(0))
                .map_err(sql)?
                .collect::<Result<_, _>>()
                .map_err(sql)?
        };
        let fresh: Vec<&String> = drafts
            .iter()
            .filter(|draft| waiting.insert((*draft).clone()))
            .collect();
        if fresh.is_empty() {
            return Ok(0);
        }
        let attachments = "[]";
        let bytes: usize = fresh
            .iter()
            .map(|draft| draft.len() + attachments.len())
            .sum();
        Self::queue_quota_in(&tx, run, fresh.len() as i64, bytes as i64)?;
        let mut position: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(position), 0) FROM prompt_queue WHERE run = ?1",
                [run],
                |row| row.get(0),
            )
            .map_err(sql)?;
        for draft in &fresh {
            position += 1;
            tx.execute(
                "INSERT INTO prompt_queue (run, pane, provider, position, body, attachments, revision,
                   created_ms, updated_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7)",
                params![run, pane, provider, position, draft, attachments, now as i64],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
        Ok(fresh.len())
    }

    /// Every queued prompt, by run and position. A sent body past its day
    /// reads as gone even before the next change clears it.
    pub fn queue_items(&self, now: u64) -> Result<Vec<QueueItem>, String> {
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT {QUEUE_COLUMNS} FROM prompt_queue ORDER BY run, position, id"
            ))
            .map_err(sql)?;
        statement
            .query_map([], QueueItem::from_row)
            .map_err(sql)?
            .map(|item| item.map(|item| item.hidden_after(now)))
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    pub fn queue_item(&self, id: i64, now: u64) -> Result<Option<QueueItem>, String> {
        Self::queue_item_in(&self.conn, id, now)
    }

    /// Changes a staged prompt of the agent's current run. `revision`, when
    /// given, must be the one the caller showed.
    pub fn queue_change(
        &mut self,
        id: i64,
        revision: Option<i64>,
        current_run: &str,
        change: QueueChange,
        now: u64,
    ) -> Result<QueueItem, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        Self::queue_retention_in(&tx, now)?;
        let item = Self::queue_item_in(&tx, id, now)?
            .ok_or_else(|| format!("target_absent: queued prompt {id} does not exist"))?;
        item.changeable(revision, current_run)?;
        match change {
            QueueChange::Body(body) => {
                let grown = body.len() as i64 - item.body.as_deref().map_or(0, str::len) as i64;
                Self::queue_quota_in(&tx, &item.run, 0, grown.max(0))?;
                tx.execute(
                    "UPDATE prompt_queue SET body = ?2, revision = revision + 1, updated_ms = ?3
                     WHERE id = ?1",
                    params![id, body, now as i64],
                )
                .map_err(sql)?;
            }
            QueueChange::Attachments(attachments) => {
                let attachments = attachments.to_string();
                let grown = attachments.len() as i64 - item.attachments.to_string().len() as i64;
                Self::queue_quota_in(&tx, &item.run, 0, grown.max(0))?;
                tx.execute(
                    "UPDATE prompt_queue SET attachments = ?2, revision = revision + 1, updated_ms = ?3
                     WHERE id = ?1",
                    params![id, attachments, now as i64],
                )
                .map_err(sql)?;
            }
            QueueChange::Position(position) => {
                let ids: Vec<i64> = {
                    let mut statement = tx
                        .prepare(
                            "SELECT id FROM prompt_queue WHERE run = ?1 AND id != ?2 ORDER BY position, id",
                        )
                        .map_err(sql)?;
                    statement
                        .query_map(params![item.run, id], |row| row.get(0))
                        .map_err(sql)?
                        .collect::<Result<_, _>>()
                        .map_err(sql)?
                };
                let at = (position.max(1) as usize - 1).min(ids.len());
                let mut order = ids;
                order.insert(at, id);
                for (index, other) in order.iter().enumerate() {
                    tx.execute(
                        "UPDATE prompt_queue SET position = ?2 WHERE id = ?1",
                        params![other, index as i64 + 1],
                    )
                    .map_err(sql)?;
                }
                tx.execute(
                    "UPDATE prompt_queue SET revision = revision + 1, updated_ms = ?2 WHERE id = ?1",
                    params![id, now as i64],
                )
                .map_err(sql)?;
            }
        }
        let item = Self::queue_item_in(&tx, id, now)?.ok_or("queue item disappeared")?;
        tx.commit().map_err(sql)?;
        Ok(item)
    }

    /// Removes a queued prompt. One of a run that is not live any more goes
    /// in any state; one of a live run unless it is being sent or its
    /// outcome is unknown (which a person settles first).
    pub fn queue_remove(
        &mut self,
        id: i64,
        revision: Option<i64>,
        live_runs: &HashSet<String>,
        now: u64,
    ) -> Result<(), String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        Self::queue_retention_in(&tx, now)?;
        let item = Self::queue_item_in(&tx, id, now)?
            .ok_or_else(|| format!("target_absent: queued prompt {id} does not exist"))?;
        if live_runs.contains(&item.run) && matches!(item.state(), "sending" | "unknown") {
            item.changeable(revision, &item.run)?;
        } else if revision.is_some_and(|revision| revision != item.revision) {
            return Err(QueueItem::stale());
        }
        tx.execute("DELETE FROM prompt_queue WHERE id = ?1", [id])
            .map_err(sql)?;
        tx.commit().map_err(sql)
    }

    /// The reason the last send of an item did not go out; not a change of
    /// its content, so the revision stays.
    pub fn queue_note(&mut self, id: i64, note: &str, now: u64) -> Result<(), String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        tx.execute(
            "UPDATE prompt_queue SET note = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id, note.chars().take(512).collect::<String>(), now as i64],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)
    }

    /// Prompts of `run` delivered at or after `since` and not yet taken in
    /// by the provider, the latest delivery first.
    pub fn delivered_prompts(&self, run: &str, since: u64) -> Result<Vec<Record>, String> {
        let keys: Vec<String> = {
            let mut statement = self
                .conn
                .prepare(
                    "SELECT key FROM operations
                     WHERE namespace = ?1 AND action = 'prompt' AND state = 'delivered'
                       AND updated_ms >= ?2
                     ORDER BY updated_ms DESC LIMIT 16",
                )
                .map_err(sql)?;
            statement
                .query_map(params![format!("run:{run}"), since as i64], |row| {
                    row.get(0)
                })
                .map_err(sql)?
                .collect::<Result<_, _>>()
                .map_err(sql)?
        };
        let mut records = Vec::new();
        for key in keys {
            if let Some(record) = Self::record_in(&self.conn, &key)? {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Whether a prompt of `run` admitted (or retried) at or after `since` is
    /// still being delivered.
    pub fn prompt_dispatching(&self, run: &str, since: u64) -> Result<bool, String> {
        self.conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM operations
                   WHERE namespace = ?1 AND action = 'prompt' AND state = ?2 AND updated_ms >= ?3)",
                params![format!("run:{run}"), DISPATCHING, since as i64],
                |row| row.get(0),
            )
            .map_err(sql)
    }

    /// Records that the provider took a delivered prompt in as a turn of
    /// its own. False when the record is no longer `delivered`.
    pub fn accept_prompt(
        &mut self,
        record: &Record,
        source: &str,
        evidence: &Value,
        now: u64,
    ) -> Result<bool, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        let seq = Self::next_seq(&tx)?;
        let changed = tx
            .execute(
                "UPDATE operations SET state = 'native_accepted', updated_ms = ?3, store_seq = ?4
                 WHERE id = ?1 AND ticket = ?2 AND state = 'delivered'",
                params![record.op, record.ticket, now as i64, seq],
            )
            .map_err(sql)?;
        if changed == 1 {
            Self::append(
                &tx,
                record.op,
                "native_accepted",
                source,
                Some(evidence),
                now,
            )?;
        }
        tx.commit().map_err(sql)?;
        Ok(changed == 1)
    }

    /// Resolves the run's open request events from `source` whose native
    /// request is not in `present`.
    pub fn resolve_absent(
        &mut self,
        run: &str,
        source: &str,
        present: &[String],
        before: u64,
        resolution: &str,
        now: u64,
    ) -> Result<usize, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        let open: Vec<(i64, Option<String>)> = {
            let mut statement = tx
                .prepare(
                    "SELECT id, native_ref FROM inbox_events
                     WHERE run = ?1 AND source = ?2 AND resolved_ms IS NULL AND observed_ms < ?3
                       AND kind IN ('approval_requested', 'question_asked')",
                )
                .map_err(sql)?;
            statement
                .query_map(params![run, source, before as i64], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .map_err(sql)?
                .collect::<Result<_, _>>()
                .map_err(sql)?
        };
        let mut changed = 0;
        for (id, native) in open {
            if native.is_some_and(|native| present.contains(&native)) {
                continue;
            }
            changed += tx
                .execute(
                    "UPDATE inbox_events SET resolved_ms = ?2, resolution = ?3 WHERE id = ?1",
                    params![id, now as i64, resolution],
                )
                .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
        Ok(changed)
    }

    /// Resolves a run's open attention events of the given kinds.
    pub fn resolve_run(
        &mut self,
        run: &str,
        kinds: &[&str],
        resolution: &str,
        now: u64,
    ) -> Result<usize, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        let mut changed = 0;
        for kind in kinds {
            changed += tx
                .execute(
                    "UPDATE inbox_events SET resolved_ms = ?3, resolution = ?4
                     WHERE run = ?1 AND kind = ?2 AND resolved_ms IS NULL",
                    params![run, kind, now as i64, resolution],
                )
                .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
        Ok(changed)
    }

    /// Records the secret a provider started with for `run`.
    pub fn put_secret(
        &mut self,
        run: &str,
        provider: &str,
        secret: &str,
        now: u64,
    ) -> Result<(), String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        tx.execute(
            "INSERT INTO provider_secrets VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (run) DO UPDATE SET provider = ?2, secret = ?3, created_ms = ?4",
            params![run, provider, secret, now as i64],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)
    }

    /// The secret `run`'s provider started with, if any.
    pub fn secret(&self, run: &str, provider: &str) -> Result<Option<String>, String> {
        self.conn
            .query_row(
                "SELECT secret FROM provider_secrets WHERE run = ?1 AND provider = ?2",
                params![run, provider],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)
    }

    /// Forgets secrets of runs that are no longer live, after a grace for a
    /// run registering meanwhile, and every secret past the retention.
    pub fn prune_secrets(&mut self, live: &HashSet<String>, now: u64) -> Result<usize, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        let rows = {
            let mut statement = tx
                .prepare("SELECT run, created_ms FROM provider_secrets")
                .map_err(sql)?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)?
        };
        let mut removed = 0;
        for (run, created) in rows {
            let age = now.saturating_sub(created.max(0) as u64);
            if age >= SECRET_RETENTION_MS || (!live.contains(&run) && age >= SECRET_GRACE_MS) {
                removed += tx
                    .execute("DELETE FROM provider_secrets WHERE run = ?1", [&run])
                    .map_err(sql)?;
            }
        }
        tx.commit().map_err(sql)?;
        Ok(removed)
    }

    /// Runs with open attention events, for the coordinator's check of runs
    /// whose pane is gone.
    pub fn open_runs(&self) -> Result<Vec<String>, String> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT DISTINCT run FROM inbox_events
                 WHERE resolved_ms IS NULL AND source != 'operation' AND run != ''",
            )
            .map_err(sql)?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)
    }

    /// Resolves the open events of a run that ended, observed before `before`;
    /// later events belong to a pane the caller had not seen yet.
    pub fn resolve_ended_run(&mut self, run: &str, before: u64, now: u64) -> Result<usize, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        if !Self::inbox_enabled_in(&tx)? {
            return Ok(0);
        }
        let changed = tx
            .execute(
                "UPDATE inbox_events SET resolved_ms = ?3, resolution = 'run_ended'
                 WHERE run = ?1 AND resolved_ms IS NULL AND source != 'operation'
                   AND observed_ms < ?2",
                params![run, before as i64, now as i64],
            )
            .map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(changed)
    }

    /// Marks events read by ID; unknown IDs are reported back.
    pub fn ack_events(&mut self, ids: &[i64], now: u64) -> Result<Vec<i64>, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        let mut missing = Vec::new();
        for id in ids {
            let found: Option<i64> = tx
                .query_row("SELECT id FROM inbox_events WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .optional()
                .map_err(sql)?;
            if found.is_none() {
                missing.push(*id);
                continue;
            }
            tx.execute(
                "UPDATE inbox_events SET acked_ms = ?2 WHERE id = ?1 AND acked_ms IS NULL",
                params![id, now as i64],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
        Ok(missing)
    }

    /// Marks a run's events read, but only those the person can have seen:
    /// up to a sequence a client displayed, or recorded before the moment a
    /// command read the agent. Unknown outcomes are read one by one.
    pub fn ack_run(&mut self, run: &str, through: Through, now: u64) -> Result<usize, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        let (seq, before) = match through {
            Through::Seq(seq) => (seq, i64::MAX),
            Through::Before(at) => (i64::MAX, at as i64),
        };
        let changed = tx
            .execute(
                "UPDATE inbox_events SET acked_ms = ?4
                 WHERE run = ?1 AND source != 'operation' AND acked_ms IS NULL
                   AND store_seq <= ?2 AND observed_ms <= ?3",
                params![run, seq, before, now as i64],
            )
            .map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(changed)
    }

    /// The events a client lists as unseen, counted the same way.
    pub fn inbox_unseen_count(&self) -> Result<i64, String> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM inbox_events
                 WHERE acked_ms IS NULL AND resolved_ms IS NULL
                   AND store_seq > (SELECT value FROM meta WHERE name = 'inbox_fence')",
                [],
                |row| row.get(0),
            )
            .map_err(sql)
    }

    /// Marks everything up to `through` (default: now) read; events written
    /// later stay unread.
    pub fn read_all(&mut self, through: Option<i64>) -> Result<i64, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        Self::check_reader_in(&tx)?;
        let latest: i64 = tx
            .query_row(
                "SELECT value FROM meta WHERE name = 'store_seq'",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        let through = through.unwrap_or(latest).min(latest);
        tx.execute(
            "UPDATE meta SET value = MAX(value, ?1) WHERE name = 'inbox_fence'",
            [through],
        )
        .map_err(sql)?;
        let fence: i64 = tx
            .query_row(
                "SELECT value FROM meta WHERE name = 'inbox_fence'",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(fence)
    }

    /// Events newest last; unread and unresolved ones only unless `all`.
    pub fn inbox(&self, all: bool, limit: usize) -> Result<Value, String> {
        let fence: i64 = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE name = 'inbox_fence'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?
            .unwrap_or(0);
        let dropped: i64 = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE name = 'inbox_dropped'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?
            .unwrap_or(0);
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, store_seq, source, provider, pane, run, revision, kind, native_ref, summary,
                        observed_ms, acked_ms, resolved_ms, resolution
                 FROM (SELECT * FROM inbox_events
                       WHERE ?1 OR (acked_ms IS NULL AND resolved_ms IS NULL AND store_seq > ?2)
                       ORDER BY store_seq DESC LIMIT ?3)
                 ORDER BY store_seq",
            )
            .map_err(sql)?;
        let events = statement
            .query_map(params![all, fence, limit as i64], |row| {
                let summary: Option<String> = row.get(9)?;
                let seq: i64 = row.get(1)?;
                let acked: Option<i64> = row.get(11)?;
                let resolved: Option<i64> = row.get(12)?;
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "seq": seq,
                    "source": row.get::<_, String>(2)?,
                    "provider": row.get::<_, String>(3)?,
                    "pane": row.get::<_, String>(4)?,
                    "run": row.get::<_, String>(5)?,
                    "revision": row.get::<_, Option<i64>>(6)?,
                    "kind": row.get::<_, String>(7)?,
                    "native_ref": row.get::<_, Option<String>>(8)?,
                    "summary": summary.and_then(|text| serde_json::from_str::<Value>(&text).ok()),
                    "observed_ms": row.get::<_, i64>(10)?,
                    "read": acked.is_some() || seq <= fence,
                    "resolved_ms": resolved,
                    "resolution": row.get::<_, Option<String>>(13)?,
                }))
            })
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        Ok(json!({"events": events, "fence": fence, "dropped": dropped}))
    }

    pub fn status(&self) -> Result<Value, String> {
        let pragma = |name: &str| -> Result<Value, String> {
            self.conn
                .query_row(&format!("PRAGMA {name}"), [], |row| {
                    row.get::<_, rusqlite::types::Value>(0)
                })
                .map(|value| match value {
                    rusqlite::types::Value::Integer(n) => json!(n),
                    rusqlite::types::Value::Text(text) => json!(text),
                    _ => Value::Null,
                })
                .map_err(sql)
        };
        let mut statement = self
            .conn
            .prepare("SELECT state, COUNT(*) FROM operations GROUP BY state")
            .map_err(sql)?;
        let counts = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, json!(row.get::<_, i64>(1)?)))
            })
            .map_err(sql)?
            .collect::<Result<serde_json::Map<_, _>, _>>()
            .map_err(sql)?;
        let version = Self::user_version(&self.conn)?;
        let (minimum, features) = if version >= SCHEMA_VERSION {
            let minimum: Option<i64> = self
                .conn
                .query_row(
                    "SELECT value FROM meta WHERE name = 'min_reader'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql)?;
            let mut statement = self
                .conn
                .prepare("SELECT name FROM schema_features ORDER BY name")
                .map_err(sql)?;
            let features = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)?;
            (minimum, features)
        } else {
            (None, Vec::new())
        };
        let backup = backup_path(&self.path);
        let backup = fs::symlink_metadata(&backup).ok().map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_millis() as u64);
            json!({"path": backup, "modified_ms": modified, "kept_ms": BACKUP_KEEP_MS})
        });
        Ok(json!({
            "path": self.path,
            "sqlite_version": rusqlite::version(),
            "schema_version": version,
            "min_reader": minimum,
            "reader_generation": READER_GENERATION,
            "features": features,
            "live_bytes": live_bytes(&self.conn)?,
            "backup": backup,
            "journal_mode": pragma("journal_mode")?,
            "synchronous": pragma("synchronous")?,
            "fullfsync": pragma("fullfsync")?,
            "durability": "commit synced with fsync; F_FULLFSYNC off because run and boot namespaces end with the OS",
            "lease_ms": LEASE_MS,
            "retention_ms": RETENTION_MS,
            "capacity": CAPACITY,
            "counts": counts,
            "instance": self.instance()?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("masil-ops-{}", super::super::nonce().unwrap()));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        // SQLITE_OPEN_NOFOLLOW rejects symlinks anywhere in the path (/var on macOS).
        dir.canonicalize().unwrap()
    }

    fn request<'a>(digest: &'a str) -> NewOperation<'a> {
        NewOperation {
            namespace: "run:r1",
            action: "prompt",
            id: "1",
            explicit: true,
            target: "%1",
            run: Some("r1"),
            boot: "b1",
            digest,
            payload_bytes: 5,
        }
    }

    fn dispatch(admission: Admission) -> Ticket {
        match admission {
            Admission::Dispatch(ticket) => ticket,
            other => panic!("expected dispatch, got {other:?}"),
        }
    }

    fn queued_request<'a>(id: &'a str, digest: &'a str) -> NewOperation<'a> {
        NewOperation {
            namespace: "run:r1",
            action: "prompt",
            id,
            explicit: false,
            target: "%1",
            run: Some("r1"),
            boot: "b1",
            digest,
            payload_bytes: 5,
        }
    }

    #[test]
    fn a_queued_prompt_follows_its_operation_and_admission_needs_its_revision() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        let item = store
            .queue_add("r1", "%1", "codex", "hello", &json!([]), 100)
            .unwrap();
        assert_eq!(
            (item.state(), item.revision, item.position),
            ("staged", 1, 1)
        );
        // A stale revision admits nothing.
        let stale = QueuedPrompt {
            item: item.id,
            revision: 7,
            run: "r1",
        };
        let id = format!("q{}", item.id);
        assert!(
            store
                .admit_with(&queued_request(&id, "d1"), json!({}), 110, Some(&stale))
                .unwrap_err()
                .starts_with("queue_stale")
        );
        assert!(store.get(&key("run:r1", "prompt", &id)).unwrap().is_none());
        let current = QueuedPrompt {
            item: item.id,
            revision: 1,
            run: "r1",
        };
        let ticket = dispatch(
            store
                .admit_with(&queued_request(&id, "d1"), json!({}), 120, Some(&current))
                .unwrap(),
        );
        let sending = store.queue_item(item.id, 120).unwrap().unwrap();
        assert_eq!((sending.state(), sending.revision), ("sending", 2));
        assert!(
            store
                .queue_change(item.id, None, "r1", QueueChange::Body("x".into()), 125)
                .unwrap_err()
                .starts_with("queue_sending")
        );
        // A no-op attempt leaves it staged; the trigger follows the operation.
        store
            .finish(&ticket, "not_applied", "test", None, 130)
            .unwrap();
        assert_eq!(
            store.queue_item(item.id, 130).unwrap().unwrap().state(),
            "staged"
        );
        let edited = store
            .queue_change(
                item.id,
                Some(2),
                "r1",
                QueueChange::Body("hello again".into()),
                140,
            )
            .unwrap();
        let again = QueuedPrompt {
            item: item.id,
            revision: edited.revision,
            run: "r1",
        };
        let ticket = dispatch(
            store
                .admit_with(&queued_request(&id, "d2"), json!({}), 150, Some(&again))
                .unwrap(),
        );
        store
            .finish(&ticket, "delivered", "test", None, 160)
            .unwrap();
        let sent = store.queue_item(item.id, 160).unwrap().unwrap();
        assert_eq!((sent.state(), sent.sent_ms), ("sent", Some(160)));
        assert!(
            store
                .queue_change(item.id, None, "r1", QueueChange::Body("x".into()), 170)
                .unwrap_err()
                .starts_with("queue_not_staged")
        );
        // The sent body reads as gone after a day and is cleared at the next change.
        let day = 160 + QUEUE_SENT_BODY_MS;
        assert!(
            store
                .queue_item(item.id, day)
                .unwrap()
                .unwrap()
                .body
                .is_none()
        );
        store
            .queue_add("r1", "%1", "codex", "next", &json!([]), day)
            .unwrap();
        let raw: Option<String> = store
            .conn
            .query_row(
                "SELECT body FROM prompt_queue WHERE id = ?1",
                [item.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(raw.is_none());
    }

    #[test]
    fn queue_items_of_an_ended_run_are_held_and_removable_in_any_state() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        let first = store
            .queue_add("r1", "%1", "codex", "a", &json!([]), 100)
            .unwrap();
        let second = store
            .queue_add("r1", "%1", "codex", "b", &json!([]), 100)
            .unwrap();
        let moved = store
            .queue_change(second.id, Some(1), "r1", QueueChange::Position(1), 110)
            .unwrap();
        assert_eq!(moved.position, 1);
        assert_eq!(
            store.queue_item(first.id, 110).unwrap().unwrap().position,
            2
        );
        // The agent now runs r2: r1's items are held.
        assert!(
            store
                .queue_change(first.id, None, "r2", QueueChange::Body("c".into()), 120)
                .unwrap_err()
                .starts_with("queue_held")
        );
        let live: HashSet<String> = ["r2".to_owned()].into();
        store.queue_remove(first.id, None, &live, 130).unwrap();
        assert!(store.queue_item(first.id, 130).unwrap().is_none());
        assert!(
            store
                .queue_remove(second.id, Some(9), &live, 140)
                .unwrap_err()
                .starts_with("queue_stale")
        );
    }

    #[test]
    fn the_queue_quota_counts_bytes_not_characters() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        let korean = "가".repeat(10_000);
        for _ in 0..QUEUE_RUN_ITEMS {
            store
                .queue_add("r1", "%1", "codex", "x", &json!([]), 100)
                .unwrap();
        }
        assert!(
            store
                .queue_add("r1", "%1", "codex", "x", &json!([]), 100)
                .unwrap_err()
                .starts_with("queue_full")
        );
        // 30 KB of Korean text, 70 times, passes 2 MiB in bytes.
        let mut full = false;
        for index in 0..80 {
            if let Err(error) = store.queue_add(
                &format!("run{index}"),
                "%2",
                "codex",
                &korean,
                &json!([]),
                100,
            ) {
                assert!(error.starts_with("queue_full"));
                full = true;
                break;
            }
        }
        assert!(full);
    }

    #[test]
    fn a_delivered_prompt_is_accepted_once_and_only_from_delivered() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        let ticket = dispatch(
            store
                .admit(&request("d1"), json!({"accept_digest": "a1"}), 100)
                .unwrap(),
        );
        assert!(store.prompt_dispatching("r1", 0).unwrap());
        assert!(store.delivered_prompts("r1", 0).unwrap().is_empty());
        store
            .finish(&ticket, "delivered", "pane_ledger", None, 110)
            .unwrap();
        assert!(!store.prompt_dispatching("r1", 0).unwrap());
        let delivered = store.delivered_prompts("r1", 0).unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].intent().unwrap()["accept_digest"], "a1");
        // Outside the window, or another run, lists nothing.
        assert!(store.delivered_prompts("r1", 111).unwrap().is_empty());
        assert!(store.delivered_prompts("r2", 0).unwrap().is_empty());
        let turn = json!({"turn": "msg_1"});
        assert!(
            store
                .accept_prompt(&delivered[0], "provider_api", &turn, 120)
                .unwrap()
        );
        assert!(
            !store
                .accept_prompt(&delivered[0], "provider_api", &turn, 130)
                .unwrap()
        );
        let record = store.get("run:r1/prompt/1").unwrap().unwrap();
        assert_eq!(record.state, "native_accepted");
        assert_eq!(
            record.receipts.last().unwrap().evidence.as_ref().unwrap()["turn"],
            "msg_1"
        );
        assert!(store.delivered_prompts("r1", 0).unwrap().is_empty());
    }

    #[test]
    fn a_result_is_returned_and_never_dispatched_twice() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        let ticket = dispatch(store.admit(&request("d1"), json!({}), 100).unwrap());
        assert_eq!(ticket.key, "run:r1/prompt/1");
        let record = store
            .finish(&ticket, "delivered", "pane_ledger", None, 110)
            .unwrap();
        assert_eq!(record.state, "delivered");
        let stages: Vec<_> = record.receipts.iter().map(|r| r.stage.as_str()).collect();
        assert_eq!(stages, ["accepted_durable", "dispatch_intent", "delivered"]);
        match store.admit(&request("d1"), json!({}), 120).unwrap() {
            Admission::Recorded(record) => assert_eq!(record.state, "delivered"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(
            store
                .admit(&request("d2"), json!({}), 130)
                .unwrap_err()
                .contains("different content")
        );
        assert!(
            store
                .finish(&ticket, "delivered", "again", None, 140)
                .is_err()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn in_progress_and_expired_attempts_are_not_redispatched() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        let ticket = dispatch(store.admit(&request("d1"), json!({}), 100).unwrap());
        assert!(
            store
                .admit(&request("d1"), json!({}), 101)
                .unwrap_err()
                .contains("in progress")
        );
        let expired = match store
            .admit(&request("d1"), json!({}), 100 + LEASE_MS)
            .unwrap()
        {
            Admission::NeedsReconcile(record) => record,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(store.expired(100 + LEASE_MS, 8).unwrap().len(), 1);
        // A reconcile before the lease expires is refused.
        assert!(
            store
                .resolve(&expired, "outcome_unknown", "reconcile", None, 101)
                .is_err()
        );
        let resolved = store
            .resolve(
                &expired,
                "outcome_unknown",
                "reconcile",
                None,
                100 + LEASE_MS,
            )
            .unwrap();
        assert_eq!(resolved.state, "outcome_unknown");
        // The late owner cannot overwrite the reconciled result.
        assert!(
            store
                .finish(&ticket, "delivered", "late", None, 200 + LEASE_MS)
                .is_err()
        );
        assert!(matches!(
            store
                .admit(&request("d1"), json!({}), 300 + LEASE_MS)
                .unwrap(),
            Admission::Recorded(_)
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_slot_proven_unused_can_start_a_new_attempt() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        let first = dispatch(
            store
                .admit(&request("d1"), json!({"slot": 1}), 100)
                .unwrap(),
        );
        store
            .finish(&first, "rejected_before_effect", "guard", None, 101)
            .unwrap();
        // A client key keeps its content even after a proven no-op.
        assert!(
            store
                .admit(&request("d2"), json!({}), 102)
                .unwrap_err()
                .contains("different content")
        );
        let second = dispatch(
            store
                .admit(&request("d1"), json!({"slot": 2}), 103)
                .unwrap(),
        );
        assert_ne!(first.ticket, second.ticket);
        let record = store
            .finish(&second, "delivered", "pane_ledger", None, 104)
            .unwrap();
        assert_eq!(record.attempts, 2);
        assert_eq!(record.intent().unwrap()["slot"], 2);

        let auto = NewOperation {
            id: "auto-x",
            explicit: false,
            ..request("d1")
        };
        let first = dispatch(store.admit(&auto, json!({}), 105).unwrap());
        store
            .finish(&first, "not_applied", "fence", None, 106)
            .unwrap();
        let rebound = NewOperation {
            digest: "d9",
            ..auto
        };
        let second = dispatch(store.admit(&rebound, json!({}), 107).unwrap());
        assert_eq!(
            store
                .finish(&second, "delivered", "test", None, 108)
                .unwrap()
                .digest,
            "d9"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unknown_outcomes_settle_only_once() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        let ticket = dispatch(store.admit(&request("d1"), json!({}), 100).unwrap());
        let unknown = store.finish(&ticket, UNKNOWN, "test", None, 101).unwrap();
        let settled = store
            .settle_unknown(&unknown, "user_confirmed_delivered", "user", None, 102)
            .unwrap();
        assert_eq!(settled.state, "user_confirmed_delivered");
        assert!(
            store
                .settle_unknown(&unknown, "delivered", "late", None, 103)
                .is_err()
        );
        assert!(
            store
                .settle_unknown(&settled, UNKNOWN, "x", None, 104)
                .is_err()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prune_keeps_active_namespaces_and_unknown_outcomes() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        for (namespace, run, boot, id, state) in [
            ("run:live", Some("live"), "b1", "1", "delivered"),
            ("run:gone", Some("gone"), "b1", "1", "delivered"),
            ("run:gone", Some("gone"), "b1", "2", "outcome_unknown"),
            ("boot:b1", None, "b1", "s", "process_started"),
            ("boot:b0", None, "b0", "s", "process_started"),
        ] {
            let new = NewOperation {
                namespace,
                run,
                boot,
                id,
                ..request("d")
            };
            let ticket = dispatch(store.admit(&new, json!({}), 100).unwrap());
            store.finish(&ticket, state, "test", None, 100).unwrap();
        }
        // An automatic record in a live run stays too.
        let auto = NewOperation {
            namespace: "run:live",
            run: Some("live"),
            id: "auto-1",
            explicit: false,
            ..request("d")
        };
        let ticket = dispatch(store.admit(&auto, json!({}), 100).unwrap());
        store.finish(&ticket, UNKNOWN, "test", None, 100).unwrap();
        let live = HashSet::from(["live".to_owned()]);
        assert_eq!(store.prune(&live, "b1", 200).unwrap(), 0);
        assert_eq!(store.prune(&live, "b1", 100 + RETENTION_MS).unwrap(), 3);
        let left: Vec<_> = store
            .list(false, 10)
            .unwrap()
            .into_iter()
            .map(|r| r.operation_key)
            .collect();
        assert_eq!(left.len(), 3);
        assert!(left.contains(&"run:live/prompt/1".to_owned()));
        assert!(left.contains(&"run:live/prompt/auto-1".to_owned()));
        assert!(left.contains(&"boot:b1/prompt/s".to_owned()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unsafe_files_and_newer_schemas_are_refused() {
        let dir = temp();
        let path = dir.join("ops.sqlite3");
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Store::open_path(&path).is_err());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/tmp", &path).unwrap();
        assert!(Store::open_path(&path).is_err());
        fs::remove_file(&path).unwrap();

        let store = Store::open_path(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        store
            .conn
            .execute("UPDATE meta SET value = 9 WHERE name = 'min_reader'", [])
            .unwrap();
        drop(store);
        assert!(
            Store::open_path(&path)
                .unwrap_err()
                .starts_with(STORE_NEWER)
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn status_reports_the_durability_profile() {
        let dir = temp();
        let store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        let status = store.status().unwrap();
        assert_eq!(status["journal_mode"], "wal");
        assert_eq!(status["synchronous"], 2);
        assert_eq!(status["fullfsync"], 0);
        assert!(rusqlite::version_number() >= MIN_SQLITE);
        fs::remove_dir_all(dir).unwrap();
    }

    /// A store as the version 1 binary left it, with one resolved record.
    fn version_one(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))
            .unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute("INSERT INTO meta VALUES ('instance', 7)", [])
            .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        drop(conn);
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut store = Store {
            conn: Connection::open(path).unwrap(),
            path: path.to_path_buf(),
        };
        let ticket = dispatch(store.admit(&request("d1"), json!({}), 100).unwrap());
        store
            .finish(&ticket, "delivered", "test", None, 101)
            .unwrap();
    }

    #[test]
    fn a_new_store_starts_at_version_two_without_a_backup() {
        let dir = temp();
        let path = dir.join("ops.sqlite3");
        let store = Store::open_path(&path).unwrap();
        assert_eq!(Store::user_version(&store.conn).unwrap(), 2);
        assert!(!backup_path(&path).exists());
        let status = store.status().unwrap();
        assert_eq!(status["min_reader"], 2);
        assert_eq!(
            status["features"],
            json!([
                "inbox",
                "notify",
                "prompt_queue",
                "provider_secrets",
                "schedules"
            ])
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn version_one_upgrades_and_keeps_a_private_backup_of_the_old_store() {
        let dir = temp();
        let path = dir.join("ops.sqlite3");
        version_one(&path);
        let store = Store::open_path(&path).unwrap();
        assert_eq!(Store::user_version(&store.conn).unwrap(), 2);
        assert_eq!(store.instance().unwrap(), 7);
        assert!(store.get("run:r1/prompt/1").unwrap().is_some());
        let backup = backup_path(&path);
        let mode = fs::metadata(&backup).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // The backup is the version 1 store a downgrade can open.
        let old = Connection::open(&backup).unwrap();
        assert_eq!(Store::user_version(&old).unwrap(), 1);
        let rows: i64 = old
            .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_upgrade_leaves_version_one_and_no_backup() {
        let dir = temp();
        let path = dir.join("ops.sqlite3");
        version_one(&path);
        // The version 2 schema cannot create a table that already exists.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE schema_features (x)")
            .unwrap();
        drop(conn);
        assert!(Store::open_path(&path).is_err());
        let conn = Connection::open(&path).unwrap();
        assert_eq!(Store::user_version(&conn).unwrap(), 1);
        assert!(!backup_path(&path).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_backup_expires_after_the_keep_period() {
        let dir = temp();
        let path = dir.join("ops.sqlite3");
        version_one(&path);
        drop(Store::open_path(&path).unwrap());
        let backup = backup_path(&path);
        expire_backup(&path, now_ms());
        assert!(backup.exists());
        expire_backup(&path, now_ms() + BACKUP_KEEP_MS + 1000);
        assert!(!backup.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_first_opens_upgrade_once() {
        let dir = temp();
        let path = dir.join("ops.sqlite3");
        version_one(&path);
        let openers: Vec<_> = (0..6)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || Store::open_path(&path).map(drop))
            })
            .collect();
        for opener in openers {
            opener.join().unwrap().unwrap();
        }
        let conn = Connection::open(&path).unwrap();
        assert_eq!(Store::user_version(&conn).unwrap(), 2);
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'schema_features'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
        assert!(backup_path(&path).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_raised_min_reader_stops_an_open_writer() {
        let dir = temp();
        let path = dir.join("ops.sqlite3");
        let mut store = Store::open_path(&path).unwrap();
        let other = Connection::open(&path).unwrap();
        other
            .execute("UPDATE meta SET value = 3 WHERE name = 'min_reader'", [])
            .unwrap();
        let error = store.admit(&request("d1"), json!({}), 100).unwrap_err();
        assert!(error.starts_with(STORE_NEWER), "{error}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_newer_additive_schema_still_opens() {
        let dir = temp();
        let path = dir.join("ops.sqlite3");
        drop(Store::open_path(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 5).unwrap();
        conn.execute_batch(
            "CREATE TABLE later (x); INSERT INTO schema_features VALUES ('later', 1);",
        )
        .unwrap();
        drop(conn);
        let store = Store::open_path(&path).unwrap();
        assert_eq!(
            store.status().unwrap()["features"],
            json!([
                "inbox",
                "later",
                "notify",
                "prompt_queue",
                "provider_secrets",
                "schedules"
            ])
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn feature_migrations_apply_once_in_any_order() {
        let dir = temp();
        let path = dir.join("ops.sqlite3");
        let mut store = Store::open_path(&path).unwrap();
        let first = [
            ("alpha", "CREATE TABLE alpha (x)"),
            ("beta", "CREATE TABLE beta (x)"),
        ];
        apply_features(&mut store.conn, &first, 1).unwrap();
        let second = [
            ("gamma", "CREATE TABLE gamma (x)"),
            ("beta", "CREATE TABLE beta (x)"),
            ("alpha", "CREATE TABLE alpha (x)"),
        ];
        apply_features(&mut store.conn, &second, 2).unwrap();
        apply_features(&mut store.conn, &second, 3).unwrap();
        assert_eq!(
            store.status().unwrap()["features"],
            json!([
                "alpha",
                "beta",
                "gamma",
                "inbox",
                "notify",
                "prompt_queue",
                "provider_secrets",
                "schedules"
            ])
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn live_bytes_counts_pages_in_use() {
        let dir = temp();
        let store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        let live = live_bytes(&store.conn).unwrap();
        assert!(live > 0 && live < SOFT_BYTES);
        store.check_wal_pressure().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    fn event<'a>(source_ref: &'a str, run: &'a str, kind: &'a str, at: u64) -> InboxEvent<'a> {
        InboxEvent {
            source: "hook:opencode",
            source_ref,
            provider: "opencode",
            pane: "%1",
            run,
            revision: None,
            kind,
            native_ref: Some(source_ref),
            summary: Some(json!({"tool": "bash"})),
            observed_ms: at,
        }
    }

    fn unread(store: &Store) -> Vec<String> {
        store.inbox(false, 100).unwrap()["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["native_ref"].as_str().unwrap_or("").to_owned())
            .collect()
    }

    #[test]
    fn the_inbox_records_nothing_until_enabled_and_each_key_once() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        assert_eq!(
            store
                .record_event(&event("p1", "r1", "approval_requested", 10))
                .unwrap(),
            None
        );
        store.set_inbox_enabled(true).unwrap();
        let first = store
            .record_event(&event("p1", "r1", "approval_requested", 10))
            .unwrap();
        assert!(first.is_some());
        assert_eq!(
            store
                .record_event(&event("p1", "r1", "approval_requested", 11))
                .unwrap(),
            None
        );
        assert_eq!(unread(&store), ["p1"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_resolution_before_its_request_makes_the_request_arrive_resolved() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        store.set_inbox_enabled(true).unwrap();
        store
            .resolve_event("hook:opencode", "p2", "replied", 5)
            .unwrap();
        store
            .record_event(&event("p2", "r1", "approval_requested", 6))
            .unwrap();
        store
            .record_event(&event("p3", "r1", "approval_requested", 7))
            .unwrap();
        assert_eq!(unread(&store), ["p3"]);
        store
            .resolve_event("hook:opencode", "p3", "replied", 8)
            .unwrap();
        assert!(unread(&store).is_empty());
        let all = store.inbox(true, 100).unwrap();
        assert_eq!(all["events"][0]["resolution"], "replied");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reading_all_keeps_later_events_unread_and_runs_ack_through_a_revision() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        store.set_inbox_enabled(true).unwrap();
        store.record_event(&event("a", "r1", "blocked", 1)).unwrap();
        let fence = store.read_all(None).unwrap();
        store.record_event(&event("b", "r1", "blocked", 2)).unwrap();
        assert_eq!(unread(&store), ["b"]);
        // A stale reader cannot move the fence backwards.
        assert_eq!(store.read_all(Some(1)).unwrap(), fence);
        let screen = |source_ref, revision, at| InboxEvent {
            source: "screen",
            revision: Some(revision),
            ..event(source_ref, "r2", "blocked", at)
        };
        store.record_event(&screen("r2:9", 9, 3)).unwrap();
        store.record_event(&screen("r2:10", 10, 4)).unwrap();
        assert_eq!(store.ack_run("r2", Through::Before(3), 5).unwrap(), 1);
        assert_eq!(unread(&store), ["b", "r2:10"]);
        let missing = store.ack_events(&[999], 6).unwrap();
        assert_eq!(missing, [999]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_inbox_keeps_its_cap_and_counts_dropped_unread_events() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        store.set_inbox_enabled(true).unwrap();
        for index in 0..(INBOX_KEEP + 3) {
            let key = format!("k{index}");
            store
                .record_event(&event(&key, "r1", "blocked", 100))
                .unwrap();
        }
        let inbox = store.inbox(true, 10).unwrap();
        assert_eq!(inbox["dropped"], 3);
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM inbox_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, INBOX_KEEP);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unknown_outcome_becomes_an_inbox_event() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        store.set_inbox_enabled(true).unwrap();
        let ticket = dispatch(store.admit(&request("d1"), json!({}), 100).unwrap());
        store.finish(&ticket, UNKNOWN, "test", None, 101).unwrap();
        let inbox = store.inbox(false, 10).unwrap();
        assert_eq!(inbox["events"][0]["kind"], "operation_unknown");
        assert_eq!(inbox["events"][0]["native_ref"], "run:r1/prompt/1");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settling_an_unknown_outcome_resolves_its_event_and_inbox_failures_never_fail_it() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        store.set_inbox_enabled(true).unwrap();
        let ticket = dispatch(store.admit(&request("d1"), json!({}), 100).unwrap());
        let unknown = store.finish(&ticket, UNKNOWN, "test", None, 101).unwrap();
        assert_eq!(unread(&store), ["run:r1/prompt/1"]);
        store
            .settle_unknown(&unknown, "user_confirmed_delivered", "user", None, 102)
            .unwrap();
        assert!(unread(&store).is_empty());
        assert_eq!(
            store.inbox(true, 10).unwrap()["events"][0]["resolution"],
            "settled"
        );

        store.conn.execute("DROP TABLE inbox_events", []).unwrap();
        let other = NewOperation {
            id: "2",
            ..request("d2")
        };
        let ticket = dispatch(store.admit(&other, json!({}), 200).unwrap());
        let record = store.finish(&ticket, UNKNOWN, "test", None, 201).unwrap();
        assert_eq!(record.state, UNKNOWN);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_hook_turn_end_resolves_the_run_and_replaces_screen_returns_to_idle() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        store.set_inbox_enabled(true).unwrap();
        let screen = |source_ref, kind, at| InboxEvent {
            source: "screen",
            ..event(source_ref, "r1", kind, at)
        };
        store
            .record_event(&event("ask", "r1", "approval_requested", 1))
            .unwrap();
        store
            .record_event(&screen("r1:4", "returned_idle", 2))
            .unwrap();
        store
            .record_event(&event("turn-1", "r1", "turn_completed", 3))
            .unwrap();
        assert_eq!(unread(&store), ["turn-1"]);
        // Without a time window: the run's turn ends come from the hook.
        assert_eq!(
            store
                .record_event(&screen("r1:6", "returned_idle", 600_000))
                .unwrap(),
            None
        );
        // The same turn ending again still resolves what it raised since.
        store
            .record_event(&event("ask-2", "r1", "approval_requested", 600_000))
            .unwrap();
        store
            .record_event(&event("turn-1", "r1", "turn_completed", 600_001))
            .unwrap();
        assert_eq!(unread(&store), ["turn-1"]);
        // Another run still gets screen events.
        assert!(
            store
                .record_event(&InboxEvent {
                    source: "screen",
                    ..event("r2:1", "r2", "returned_idle", 600_001)
                })
                .unwrap()
                .is_some()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn notifications_claim_each_new_event_once_per_route() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        let kinds = ["approval_requested".to_owned(), "turn_completed".to_owned()];
        let routes = ["tmux".to_owned(), "hook".to_owned()];
        // A run per event: a turn's end resolves what the same run asked.
        let record = |store: &mut Store, key: &str, kind: &str, at: u64| {
            store
                .record_event(&event(key, key, kind, at))
                .unwrap()
                .unwrap()
        };
        // Notifications need the inbox, and the inbox stays while they are on.
        assert!(
            store
                .set_notify(true, &[])
                .unwrap_err()
                .starts_with("inbox_in_use")
        );
        store.set_inbox_enabled(true).unwrap();
        record(&mut store, "before", "approval_requested", 1_000);
        store
            .set_notify(true, &[("PATH".into(), "/bin".into())])
            .unwrap();
        assert!(
            store
                .set_inbox_enabled(false)
                .unwrap_err()
                .starts_with("inbox_in_use")
        );
        // Nothing recorded before they were switched on is sent.
        let claim = store.notify_claim(&kinds, &routes, 0, 2_000).unwrap();
        assert!(claim.events.is_empty() && claim.stale == 0);
        let fresh = record(&mut store, "fresh", "approval_requested", 10_000);
        record(&mut store, "other", "error", 10_000);
        record(&mut store, "old", "turn_completed", 100);
        let read = record(&mut store, "read", "approval_requested", 10_000);
        store.ack_events(&[read], 10_001).unwrap();
        let claim = store.notify_claim(&kinds, &routes, 5_000, 20_000).unwrap();
        assert_eq!(claim.events.len(), 1);
        assert_eq!(claim.events[0].id, fresh);
        let rows: Vec<&str> = claim.events[0]
            .rows
            .iter()
            .map(|(route, _)| route.as_str())
            .collect();
        assert_eq!(rows, ["tmux", "hook"]);
        assert_eq!(claim.stale, 1);
        // Claimed once: a second pass finds nothing.
        let again = store.notify_claim(&kinds, &routes, 5_000, 20_000).unwrap();
        assert!(again.events.is_empty() && again.stale == 0);
        // The cursor reaches `store_seq`, so the next pass reads only meta.
        let seqs = |store: &Store| -> (i64, i64) {
            store
                .conn
                .query_row(
                    "SELECT (SELECT value FROM meta WHERE name = 'notify_seq'),
                            (SELECT value FROM meta WHERE name = 'store_seq')",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap()
        };
        let (cursor, latest) = seqs(&store);
        assert_eq!(cursor, latest);
        store
            .notify_outcome(&[claim.events[0].rows[0].1], "sent")
            .unwrap();
        let status = store.notify_status().unwrap();
        assert_eq!(status["interrupted"], 1);
        assert_eq!(status["recent"].as_array().unwrap().len(), 4);
        // Switched on again: the cursor stays, the environment is replaced.
        store
            .set_notify(true, &[("HOME".into(), "/h".into())])
            .unwrap();
        assert_eq!(
            store.notify_env().unwrap(),
            [("HOME".to_owned(), "/h".to_owned())]
        );
        // Off: nothing is claimed and the cursor does not move.
        store.set_notify(false, &[]).unwrap();
        record(&mut store, "while-off", "approval_requested", 30_000);
        assert!(
            store
                .notify_claim(&kinds, &routes, 0, 30_000)
                .unwrap()
                .events
                .is_empty()
        );
        store.set_notify(true, &[]).unwrap();
        assert!(
            store
                .notify_claim(&kinds, &routes, 0, 30_000)
                .unwrap()
                .events
                .is_empty()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_due_time_is_claimed_once_and_a_late_one_is_missed() {
        use crate::schedule::Missed;
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        store.set_inbox_enabled(true).unwrap();
        let action = json!({"kind": "start"});
        let row = store
            .schedule_add("nightly", "every:60", &action, 1_000)
            .unwrap();
        assert!(
            store
                .schedule_add("nightly", "every:60", &action, 1_000)
                .unwrap_err()
                .starts_with("schedule_exists")
        );
        let claim = store
            .schedule_record(row.id, row.changed_ms, None, Some(61_000), 61_500)
            .unwrap();
        assert_eq!(
            claim,
            ScheduleClaim {
                run: Some(61_000),
                event: false
            }
        );
        // Another look, or another process, finds it taken.
        let again = store
            .schedule_record(row.id, row.changed_ms, None, Some(61_000), 61_600)
            .unwrap();
        assert_eq!(again.run, None);
        store
            .schedule_finish(row.id, 61_000, "done", &json!({"ok": true}), 62_000)
            .unwrap();
        // Past the grace when claimed: missed, with an inbox event.
        let late = store
            .schedule_record(
                row.id,
                row.changed_ms,
                Some(Missed {
                    due_ms: 121_000,
                    count: 1,
                    at_least: false,
                }),
                Some(181_000),
                181_000 + 60_001,
            )
            .unwrap();
        assert_eq!(
            late,
            ScheduleClaim {
                run: None,
                event: true
            }
        );
        let runs = store.schedule_runs("nightly", 10).unwrap();
        let states: Vec<(i64, &str)> = runs
            .iter()
            .map(|run| {
                (
                    run["due_ms"].as_i64().unwrap(),
                    run["state"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(states, [(181_000, "missed"), (61_000, "done")]);
        assert_eq!(runs[0]["result"]["missed"], 2);
        let events = store.inbox(false, 10).unwrap();
        assert_eq!(events["events"][0]["kind"], "missed_schedule");
        // Switched off and on: what was read before applies to nothing.
        store
            .schedule_set_enabled("nightly", false, 200_000)
            .unwrap();
        store
            .schedule_set_enabled("nightly", true, 200_001)
            .unwrap();
        let stale = store
            .schedule_record(row.id, row.changed_ms, None, Some(241_000), 241_000)
            .unwrap();
        assert_eq!(stale, ScheduleClaim::default());
        // A run a coordinator left behind is unknown, never run again.
        let fresh = store.schedules().unwrap().remove(0);
        store
            .schedule_record(fresh.id, fresh.changed_ms, None, Some(241_000), 241_000)
            .unwrap();
        assert_eq!(store.schedule_settle(241_001, 250_000).unwrap(), 1);
        store
            .schedule_finish(fresh.id, 241_000, "done", &json!({}), 251_000)
            .unwrap();
        assert_eq!(
            store.schedule_runs("nightly", 1).unwrap()[0]["state"],
            "unknown"
        );
        store.schedule_remove("nightly").unwrap();
        assert!(
            store
                .schedule_runs("nightly", 1)
                .unwrap_err()
                .starts_with("schedule_absent")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolutions_are_kept_only_for_requests_not_seen_yet() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        let resolutions = |store: &Store| -> i64 {
            store
                .conn
                .query_row("SELECT COUNT(*) FROM inbox_resolutions", [], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        store
            .resolve_event("hook:opencode", "off", "replied", 1)
            .unwrap();
        assert_eq!(resolutions(&store), 0);
        store.set_inbox_enabled(true).unwrap();
        store
            .record_event(&event("p1", "r1", "approval_requested", 2))
            .unwrap();
        store
            .resolve_run("r1", &["approval_requested"], "left_blocked", 3)
            .unwrap();
        store
            .resolve_event("hook:opencode", "p1", "replied", 4)
            .unwrap();
        assert_eq!(resolutions(&store), 0);
        for index in 0..(INBOX_KEEP + 2) {
            store
                .resolve_event("hook:opencode", &format!("early{index}"), "replied", 5)
                .unwrap();
        }
        assert_eq!(resolutions(&store), INBOX_KEEP);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_ended_run_resolves_only_events_seen_before_it_ended() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        store.set_inbox_enabled(true).unwrap();
        store
            .record_event(&event("a", "r1", "blocked", 10))
            .unwrap();
        store
            .record_event(&event("b", "r1", "blocked", 30))
            .unwrap();
        store
            .record_event(&event("c", "r2", "blocked", 10))
            .unwrap();
        let ticket = dispatch(store.admit(&request("d1"), json!({}), 5).unwrap());
        store.finish(&ticket, UNKNOWN, "test", None, 6).unwrap();
        let mut runs = store.open_runs().unwrap();
        runs.sort();
        assert_eq!(runs, ["r1", "r2"]);
        assert_eq!(store.resolve_ended_run("r1", 20, 40).unwrap(), 1);
        assert_eq!(unread(&store), ["b", "c", "run:r1/prompt/1"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn run_reads_stop_at_what_was_shown_and_skip_unknown_outcomes() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        store.set_inbox_enabled(true).unwrap();
        store
            .record_event(&event("p1", "r1", "approval_requested", 10))
            .unwrap();
        store
            .record_event(&event("p2", "r1", "approval_requested", 20))
            .unwrap();
        let ticket = dispatch(store.admit(&request("d1"), json!({}), 5).unwrap());
        store.finish(&ticket, UNKNOWN, "test", None, 6).unwrap();
        assert_eq!(store.inbox_unseen_count().unwrap(), 3);
        // A client that showed only p1 (seq 1) reads only p1.
        let p1_seq: i64 = store
            .conn
            .query_row(
                "SELECT store_seq FROM inbox_events WHERE source_ref = 'p1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(store.ack_run("r1", Through::Seq(p1_seq), 30).unwrap(), 1);
        assert_eq!(unread(&store), ["p2", "run:r1/prompt/1"]);
        // A command that read the agent at 15 leaves p2 (recorded at 20).
        assert_eq!(store.ack_run("r1", Through::Before(15), 31).unwrap(), 0);
        assert_eq!(store.ack_run("r1", Through::Before(25), 32).unwrap(), 1);
        // The unknown outcome stays for its own read.
        assert_eq!(unread(&store), ["run:r1/prompt/1"]);
        assert_eq!(store.inbox_unseen_count().unwrap(), 1);
        store.read_all(None).unwrap();
        assert_eq!(store.inbox_unseen_count().unwrap(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_turn_that_ends_again_after_going_on_is_unseen_again() {
        let dir = temp();
        let mut store = Store::open_path(&dir.join("ops.sqlite3")).unwrap();
        store.set_inbox_enabled(true).unwrap();
        let end = |at| InboxEvent {
            source: "hook:claude",
            ..event("turn_completed:r1:p1", "r1", "turn_completed", at)
        };
        store.record_event(&end(10)).unwrap();
        store
            .resolve_run("r1", &["turn_completed"], "next_turn", 20)
            .unwrap();
        assert!(unread(&store).is_empty());
        // Another Stop hook made the turn go on; it ends again, same key.
        assert!(store.record_event(&end(30)).unwrap().is_some());
        assert_eq!(unread(&store), ["turn_completed:r1:p1"]);
        // A plain duplicate stays one event.
        assert_eq!(store.record_event(&end(31)).unwrap(), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ids_are_short_printable_tokens() {
        assert!(valid_id("abc-1.2:3_x"));
        assert!(!valid_id(""));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(&"a".repeat(65)));
        assert!(!valid_id("한"));
    }
}
