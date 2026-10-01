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
const RETRYABLE: [&str; 2] = ["rejected_before_effect", "not_applied"];
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
const FEATURES: &[(&str, &str)] = &[];

#[derive(Debug)]
pub(super) struct Store {
    conn: Connection,
    path: PathBuf,
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
fn state_base() -> Result<PathBuf, String> {
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
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let directory = state_base()?.join("masil/operations");
    fs::create_dir_all(&directory).map_err(|error| format!("{}: {error}", directory.display()))?;
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
        let unavailable = |error: String| format!("store_unavailable: {error}");
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
                "SQLite {} lacks the WAL-reset fix",
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
            return Err(format!("operation store journal mode is {mode}"));
        }
        // FULL syncs every commit. F_FULLFSYNC is off: every namespace (run,
        // server boot) ends with the OS, so power-loss durability would not
        // prevent any duplicate; `operations status` reports this profile.
        conn.execute_batch(&format!(
            "PRAGMA synchronous=FULL; PRAGMA fullfsync=OFF; PRAGMA checkpoint_fullfsync=OFF;
             PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;
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

    fn next_seq(tx: &rusqlite::Transaction<'_>) -> Result<i64, String> {
        // Every write passes here, so a long-lived connection notices a
        // newer binary raising min_reader before it writes again.
        let minimum: Option<i64> = tx
            .query_row(
                "SELECT value FROM meta WHERE name = 'min_reader'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?;
        reader_allowed(minimum)?;
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
                            "operation {key} is in progress; query its receipt before retrying"
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
        let tx = self
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
        let tx = self
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
        assert_eq!(status["features"], json!([]));
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
        assert_eq!(store.status().unwrap()["features"], json!(["later"]));
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
            json!(["alpha", "beta", "gamma"])
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

    #[test]
    fn ids_are_short_printable_tokens() {
        assert!(valid_id("abc-1.2:3_x"));
        assert!(!valid_id(""));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(&"a".repeat(65)));
        assert!(!valid_id("한"));
    }
}
