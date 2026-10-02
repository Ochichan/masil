//! The user-wide worktree registry: one SQLite file that every server and
//! every job helper of this user shares.

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SCHEMA_VERSION: i64 = 1;
/// Raised only when a schema change would mislead an older binary.
const READER_GENERATION: i64 = 1;
/// Finished jobs and gone worktrees stay this long.
const RETAIN_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// At most this many jobs run at once, across the user.
pub(super) const MAX_RUNNING: usize = 2;
const LOCK_WAIT: Duration = Duration::from_secs(60);

/// States in which a worktree holds its name and path.
const ACTIVE: &str = "('reserved','creating','ready','removing','deleting')";
/// States in which a job has an owner that may still act.
pub(super) const OPEN: [&str; 3] = ["queued", "running", "cancel_requested"];

const SCHEMA: &str = "
CREATE TABLE meta(name TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
CREATE TABLE worktrees(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_common_dir TEXT NOT NULL,
  repo_key TEXT NOT NULL,
  repo_root TEXT NOT NULL,
  name TEXT NOT NULL,
  branch TEXT NOT NULL,
  branch_created INTEGER NOT NULL DEFAULT 0,
  from_oid TEXT,
  path TEXT NOT NULL,
  dev INTEGER,
  ino INTEGER,
  state TEXT NOT NULL
    CHECK(state IN ('reserved','creating','ready','removing','deleting','removed','failed')),
  removing_job INTEGER,
  created_ms INTEGER NOT NULL,
  removed_ms INTEGER
) STRICT;
CREATE UNIQUE INDEX worktrees_name ON worktrees(repo_key, name)
  WHERE state IN ('reserved','creating','ready','removing','deleting');
CREATE UNIQUE INDEX worktrees_path ON worktrees(path)
  WHERE state IN ('reserved','creating','ready','removing','deleting');
CREATE TABLE jobs(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  client_key TEXT UNIQUE,
  kind TEXT NOT NULL CHECK(kind IN ('create','remove','setup')),
  worktree_id INTEGER,
  resource_key TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('queued','running','cancel_requested','succeeded',
    'failed','cancelled','too_late','outcome_unknown')),
  owner_pid INTEGER,
  owner_started INTEGER,
  child_pid INTEGER,
  child_started INTEGER,
  request TEXT NOT NULL,
  result TEXT,
  output_tail TEXT,
  error TEXT,
  created_ms INTEGER NOT NULL,
  started_ms INTEGER,
  ended_ms INTEGER
) STRICT;
CREATE INDEX jobs_open ON jobs(state) WHERE state IN ('queued','running','cancel_requested');
CREATE TABLE leases(
  id INTEGER PRIMARY KEY,
  worktree_id INTEGER NOT NULL,
  socket TEXT NOT NULL,
  boot TEXT NOT NULL,
  run TEXT NOT NULL,
  pane TEXT,
  state TEXT NOT NULL CHECK(state IN ('launching','live')),
  until_ms INTEGER NOT NULL,
  created_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE runs(
  worktree_id INTEGER NOT NULL,
  socket TEXT NOT NULL,
  boot TEXT NOT NULL,
  run TEXT NOT NULL,
  session TEXT,
  started_ms INTEGER NOT NULL,
  ended_ms INTEGER,
  PRIMARY KEY(socket, boot, run)
) STRICT;
CREATE TABLE setup_paths(
  worktree_id INTEGER NOT NULL,
  path TEXT NOT NULL,
  kind TEXT NOT NULL,
  PRIMARY KEY(worktree_id, path)
) STRICT;
CREATE TABLE trusted_repos(
  common_dir TEXT PRIMARY KEY,
  dev INTEGER NOT NULL,
  ino INTEGER NOT NULL,
  file_sha256 TEXT NOT NULL,
  created_ms INTEGER NOT NULL
) STRICT;
";

fn sql(error: rusqlite::Error) -> String {
    format!("registry_unavailable: worktree registry: {error}")
}

pub(super) struct Registry {
    conn: Connection,
    dir: PathBuf,
}

/// Holds `registry.lock` until dropped.
pub(super) struct Lock(#[allow(dead_code)] File);

#[derive(Clone, Debug)]
pub(super) struct Job {
    pub(super) id: i64,
    pub(super) client_key: Option<String>,
    pub(super) kind: String,
    pub(super) worktree_id: Option<i64>,
    pub(super) resource_key: String,
    pub(super) state: String,
    pub(super) owner: Option<(i32, u64)>,
    pub(super) request: Value,
    pub(super) result: Option<Value>,
    pub(super) output_tail: Option<String>,
    pub(super) error: Option<String>,
    pub(super) created_ms: u64,
    pub(super) started_ms: Option<u64>,
    pub(super) ended_ms: Option<u64>,
    /// The git or setup process the owner runs now, in its own group.
    pub(super) child: Option<(i32, u64)>,
}

impl Job {
    pub(super) fn open(&self) -> bool {
        OPEN.contains(&self.state.as_str())
    }

    pub(super) fn to_json(&self, output: bool) -> Value {
        let mut value = json!({
            "id": self.id,
            "kind": self.kind,
            "state": self.state,
            "worktree_id": self.worktree_id,
            "created_ms": self.created_ms,
            "started_ms": self.started_ms,
            "ended_ms": self.ended_ms,
            "request": self.request,
        });
        if let Some(key) = &self.client_key {
            value["client_key"] = json!(key);
        }
        if let Some((pid, _)) = self.owner {
            value["owner_pid"] = json!(pid);
        }
        if let Some(result) = &self.result {
            value["result"] = result.clone();
        }
        if let Some(error) = &self.error {
            value["error"] = json!(error);
        }
        if output && let Some(tail) = &self.output_tail {
            value["output"] = json!(tail);
        }
        value
    }
}

#[derive(Clone, Debug)]
pub(super) struct Worktree {
    pub(super) id: i64,
    pub(super) repo_common_dir: String,
    pub(super) repo_key: String,
    pub(super) repo_root: String,
    pub(super) name: String,
    pub(super) branch: String,
    pub(super) branch_created: bool,
    pub(super) from_oid: Option<String>,
    pub(super) path: String,
    pub(super) identity: Option<(u64, u64)>,
    pub(super) state: String,
    pub(super) created_ms: u64,
}

impl Worktree {
    pub(super) fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "branch": self.branch,
            "branch_created": self.branch_created,
            "path": self.path,
            "repo": self.repo_root,
            "repo_common_dir": self.repo_common_dir,
            "repo_key": self.repo_key,
            "state": self.state,
            "created_ms": self.created_ms,
        })
    }
}

/// A trusted repository setup file: the git directory's identity and the
/// file's sha256.
pub(super) type Trust = ((u64, u64), String);

/// A run that uses a worktree.
#[derive(Clone, Debug)]
pub(super) struct LeaseRow {
    pub(super) id: i64,
    pub(super) worktree: i64,
    pub(super) socket: String,
    pub(super) boot: String,
    pub(super) run: String,
    pub(super) pane: Option<String>,
    pub(super) state: String,
    pub(super) until_ms: u64,
}

/// A new worktree row, before its directory exists.
pub(super) struct Reservation<'a> {
    pub(super) repo_common_dir: &'a str,
    pub(super) repo_key: &'a str,
    pub(super) repo_root: &'a str,
    pub(super) name: &'a str,
    pub(super) branch: &'a str,
    pub(super) from_oid: &'a str,
    pub(super) path: &'a str,
}

pub(super) enum Claim {
    Running(Box<Job>),
    Wait,
    /// Jobs on the same resource whose owners died; they are settled first.
    Reconcile(Vec<Job>),
    /// The job is no longer queued (cancelled, or claimed elsewhere).
    Gone,
}

pub(super) enum CancelStep {
    /// Queued: cancelled here; nothing ran.
    Cancelled(Job),
    /// Running: marked; its owner must be told.
    Signal(Job),
    /// Already finished, or already asked.
    Unchanged(Job),
}

/// How a job's owner ends it.
pub(super) enum End {
    Succeeded(Value),
    Failed(String),
    /// Refused before any effect, with what the person needs to decide.
    Refused(String, Value),
    /// Stopped before its effect.
    Cancelled(String),
}

const JOB_COLUMNS: &str = "id, client_key, kind, worktree_id, resource_key, state, owner_pid, \
    owner_started, request, result, output_tail, error, created_ms, started_ms, ended_ms, \
    child_pid, child_started";
const WORKTREE_COLUMNS: &str = "id, repo_common_dir, repo_key, repo_root, name, branch, \
    branch_created, from_oid, path, dev, ino, state, created_ms";

fn job_row(row: &Row<'_>) -> rusqlite::Result<Job> {
    let pid: Option<i64> = row.get(6)?;
    let started: Option<i64> = row.get(7)?;
    let request: String = row.get(8)?;
    let result: Option<String> = row.get(9)?;
    Ok(Job {
        id: row.get(0)?,
        client_key: row.get(1)?,
        kind: row.get(2)?,
        worktree_id: row.get(3)?,
        resource_key: row.get(4)?,
        state: row.get(5)?,
        owner: pid
            .zip(started)
            .map(|(pid, started)| (pid as i32, started as u64)),
        request: serde_json::from_str(&request).unwrap_or(Value::Null),
        result: result.and_then(|text| serde_json::from_str(&text).ok()),
        output_tail: row.get(10)?,
        error: row.get(11)?,
        created_ms: row.get::<_, i64>(12)? as u64,
        started_ms: row.get::<_, Option<i64>>(13)?.map(|ms| ms as u64),
        ended_ms: row.get::<_, Option<i64>>(14)?.map(|ms| ms as u64),
        child: row
            .get::<_, Option<i64>>(15)?
            .zip(row.get::<_, Option<i64>>(16)?)
            .map(|(pid, started)| (pid as i32, started as u64)),
    })
}

fn worktree_row(row: &Row<'_>) -> rusqlite::Result<Worktree> {
    let dev: Option<i64> = row.get(9)?;
    let ino: Option<i64> = row.get(10)?;
    Ok(Worktree {
        id: row.get(0)?,
        repo_common_dir: row.get(1)?,
        repo_key: row.get(2)?,
        repo_root: row.get(3)?,
        name: row.get(4)?,
        branch: row.get(5)?,
        branch_created: row.get::<_, i64>(6)? != 0,
        from_oid: row.get(7)?,
        path: row.get(8)?,
        identity: dev.zip(ino).map(|(dev, ino)| (dev as u64, ino as u64)),
        state: row.get(11)?,
        created_ms: row.get::<_, i64>(12)? as u64,
    })
}

/// Opens `path` as a private regular file, creating it owner-only.
fn private_file(path: &Path, label: &str) -> Result<File, String> {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path).map_err(|e| format!("{label}: {e}"))?;
            crate::managed::validate_private_metadata(&metadata, label)?;
            OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
                .map_err(|error| format!("{label}: {error}"))
        }
        Err(error) => Err(format!("{label}: {error}")),
    }
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

impl Registry {
    /// `$XDG_STATE_HOME/masil/worktrees`, else `~/.local/state/masil/worktrees`.
    pub(super) fn directory() -> Result<PathBuf, String> {
        crate::managed::private_directory(&crate::managed::state_base()?.join("masil/worktrees"))
    }

    pub(super) fn open() -> Result<Self, String> {
        Self::open_in(Self::directory()?)
    }

    /// The registry in `dir`, a private directory.
    pub(super) fn open_in(dir: PathBuf) -> Result<Self, String> {
        let path = dir.join("registry.sqlite3");
        drop(private_file(&path, "worktree registry")?);
        let conn = Connection::open_with_flags(
            &path,
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
            return Err(format!("registry_unavailable: journal mode is {mode}"));
        }
        conn.execute_batch(
            "PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;",
        )
        .map_err(sql)?;
        let mut registry = Self { conn, dir };
        registry.migrate()?;
        Ok(registry)
    }

    fn migrate(&mut self) -> Result<(), String> {
        let version = |conn: &Connection| -> Result<i64, String> {
            conn.query_row("PRAGMA user_version", [], |row| row.get(0))
                .map_err(sql)
        };
        if version(&self.conn)? == 0 {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql)?;
            if version(&tx)? == 0 {
                tx.execute_batch(SCHEMA).map_err(sql)?;
                tx.execute(
                    "INSERT INTO meta(name, value)
                     VALUES ('schema_version', ?1), ('min_reader', ?2), ('registry_id', ?3)",
                    params![
                        SCHEMA_VERSION.to_string(),
                        READER_GENERATION.to_string(),
                        crate::managed::nonce()?
                    ],
                )
                .map_err(sql)?;
                tx.execute_batch(&format!("PRAGMA user_version={SCHEMA_VERSION}"))
                    .map_err(sql)?;
            }
            tx.commit().map_err(sql)?;
        }
        let minimum: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE name = 'min_reader'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?;
        match minimum.and_then(|text| text.parse::<i64>().ok()) {
            Some(minimum) if minimum > READER_GENERATION => Err(format!(
                "registry_newer: the worktree registry needs masil-agent reader generation \
                 {minimum}; this one is {READER_GENERATION}"
            )),
            _ => Ok(()),
        }
    }

    /// A random ID that worktree markers carry, so a start can tell it uses
    /// the registry that made them.
    pub(super) fn registry_id(&self) -> Result<String, String> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE name = 'registry_id'",
                [],
                |row| row.get(0),
            )
            .map_err(sql)
    }

    /// Waits for `registry.lock`, which groups checks and changes that span
    /// git or tmux calls. SQLite transactions stay short and never hold it.
    pub(super) fn lock(&self) -> Result<Lock, String> {
        self.lock_within(LOCK_WAIT)
    }

    pub(super) fn lock_within(&self, wait: Duration) -> Result<Lock, String> {
        let file = private_file(&self.dir.join("registry.lock"), "worktree registry lock")?;
        let deadline = Instant::now() + wait;
        loop {
            // SAFETY: flock on an open descriptor this function owns.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Lock(file));
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(format!("worktree registry lock: {error}"));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "registry_busy: another worktree command has held the registry lock for {} s",
                    wait.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn job_log(&self, id: i64) -> Result<PathBuf, String> {
        let jobs = crate::managed::private_directory(&self.dir.join("jobs"))?;
        Ok(jobs.join(format!("{id}.log")))
    }

    /// Opens a job's private output file for appending.
    pub(super) fn open_job_log(&self, id: i64) -> Result<File, String> {
        let path = self.job_log(id)?;
        let file = private_file(&path, "worktree job output")?;
        drop(file);
        OpenOptions::new()
            .append(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| format!("worktree job output: {error}"))
    }

    // Jobs.

    /// A new queued job, or the one already made with this client key.
    pub(super) fn insert_job(
        &mut self,
        kind: &str,
        resource_key: &str,
        client_key: Option<&str>,
        request: &Value,
        now: u64,
    ) -> Result<(Job, bool), String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        if let Some(key) = client_key {
            let existing = tx
                .query_row(
                    &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE client_key = ?1"),
                    [key],
                    job_row,
                )
                .optional()
                .map_err(sql)?;
            if let Some(job) = existing {
                if job.kind != kind || job.request != *request {
                    return Err(format!(
                        "invalid_argument: client key {key} belongs to job {} with another request",
                        job.id
                    ));
                }
                return Ok((job, false));
            }
        }
        tx.execute(
            "INSERT INTO jobs(client_key, kind, resource_key, state, request, created_ms)
             VALUES (?1, ?2, ?3, 'queued', ?4, ?5)",
            params![
                client_key,
                kind,
                resource_key,
                request.to_string(),
                now as i64
            ],
        )
        .map_err(sql)?;
        let id = tx.last_insert_rowid();
        let job = tx
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                [id],
                job_row,
            )
            .map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok((job, true))
    }

    pub(super) fn set_job_worktree(&self, job: i64, worktree: i64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE jobs SET worktree_id = ?2 WHERE id = ?1",
                params![job, worktree],
            )
            .map(drop)
            .map_err(sql)
    }

    pub(super) fn job(&self, id: i64) -> Result<Option<Job>, String> {
        self.conn
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                [id],
                job_row,
            )
            .optional()
            .map_err(sql)
    }

    pub(super) fn jobs(&self, limit: usize) -> Result<Vec<Job>, String> {
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM jobs ORDER BY id DESC LIMIT ?1"
            ))
            .map_err(sql)?;
        statement
            .query_map([limit as i64], job_row)
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    pub(super) fn open_jobs(&self) -> Result<Vec<Job>, String> {
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM jobs
                 WHERE state IN ('queued','running','cancel_requested') ORDER BY id"
            ))
            .map_err(sql)?;
        statement
            .query_map([], job_row)
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    /// Records the helper that owns a job unless another owner is recorded;
    /// true when this helper is the owner.
    pub(super) fn set_owner(&self, id: i64, pid: i32, started: u64) -> Result<bool, String> {
        self.conn
            .execute(
                "UPDATE jobs SET owner_pid = ?2, owner_started = ?3
                 WHERE id = ?1 AND (owner_pid IS NULL OR (owner_pid = ?2 AND owner_started = ?3))",
                params![id, pid, started as i64],
            )
            .map(|changed| changed == 1)
            .map_err(sql)
    }

    /// Records the process group a job's owner runs now; `None` once it
    /// has been reaped.
    pub(super) fn set_child(&self, id: i64, child: Option<(i32, u64)>) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE jobs SET child_pid = ?2, child_started = ?3 WHERE id = ?1",
                params![
                    id,
                    child.map(|(pid, _)| pid),
                    child.map(|(_, started)| started as i64)
                ],
            )
            .map(drop)
            .map_err(sql)
    }

    /// Moves a queued job to running, owned by `owner`, when fewer than
    /// [`MAX_RUNNING`] jobs with live owners run and none uses the same
    /// resource. A dead owner's job on the same resource blocks the claim
    /// until it is settled.
    pub(super) fn claim(
        &mut self,
        id: i64,
        owner: (i32, u64),
        alive: &dyn Fn(&Job) -> bool,
        now: u64,
    ) -> Result<Claim, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let job = tx
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                [id],
                job_row,
            )
            .optional()
            .map_err(sql)?
            .ok_or_else(|| format!("target_absent: worktree job {id} is not in the registry"))?;
        if job.state != "queued" || job.owner.is_some_and(|recorded| recorded != owner) {
            return Ok(Claim::Gone);
        }
        let busy: Vec<Job> = {
            let mut statement = tx
                .prepare(&format!(
                    "SELECT {JOB_COLUMNS} FROM jobs
                     WHERE state IN ('running','cancel_requested') AND id != ?1"
                ))
                .map_err(sql)?;
            statement
                .query_map([id], job_row)
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)?
        };
        let (busy, dead): (Vec<Job>, Vec<Job>) = busy.into_iter().partition(|job| alive(job));
        let dead: Vec<Job> = dead
            .into_iter()
            .filter(|other| other.resource_key == job.resource_key)
            .collect();
        if !dead.is_empty() {
            return Ok(Claim::Reconcile(dead));
        }
        // Setup can run for hours; it neither counts toward nor waits for
        // the limit, and one worktree's setups still run one at a time.
        let counted = busy.iter().filter(|other| other.kind != "setup").count();
        if (job.kind != "setup" && counted >= MAX_RUNNING)
            || busy
                .iter()
                .any(|other| other.resource_key == job.resource_key)
        {
            return Ok(Claim::Wait);
        }
        tx.execute(
            "UPDATE jobs SET state = 'running', started_ms = ?2, owner_pid = ?3, owner_started = ?4
             WHERE id = ?1 AND state = 'queued'",
            params![id, now as i64, owner.0, owner.1 as i64],
        )
        .map_err(sql)?;
        let job = tx
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                [id],
                job_row,
            )
            .map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(Claim::Running(Box::new(job)))
    }

    pub(super) fn request_cancel(&mut self, id: i64, now: u64) -> Result<CancelStep, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let job = tx
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                [id],
                job_row,
            )
            .optional()
            .map_err(sql)?
            .ok_or_else(|| format!("target_absent: worktree job {id} is not in the registry"))?;
        let step = match job.state.as_str() {
            "queued" => {
                tx.execute(
                    "UPDATE jobs SET state = 'cancelled', ended_ms = ?2,
                       error = 'cancelled before it started' WHERE id = ?1",
                    params![id, now as i64],
                )
                .map_err(sql)?;
                CancelStep::Cancelled
            }
            "running" => {
                tx.execute(
                    "UPDATE jobs SET state = 'cancel_requested' WHERE id = ?1",
                    [id],
                )
                .map_err(sql)?;
                CancelStep::Signal
            }
            _ => CancelStep::Unchanged,
        };
        let job = tx
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                [id],
                job_row,
            )
            .map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(step(job))
    }

    /// The owner's last word on its job. A job someone else already ended
    /// (its owner was taken for dead) keeps that end.
    pub(super) fn finish_job(
        &mut self,
        id: i64,
        end: End,
        output_tail: Option<&str>,
        now: u64,
    ) -> Result<Job, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let state: String = tx
            .query_row("SELECT state FROM jobs WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .map_err(sql)?;
        if matches!(state.as_str(), "running" | "cancel_requested") {
            let (next, result, error) = match end {
                End::Succeeded(result) if state == "cancel_requested" => {
                    ("too_late", Some(result), None)
                }
                End::Succeeded(result) => ("succeeded", Some(result), None),
                End::Failed(error) => ("failed", None, Some(error)),
                End::Refused(error, result) => ("failed", Some(result), Some(error)),
                End::Cancelled(error) => ("cancelled", None, Some(error)),
            };
            tx.execute(
                "UPDATE jobs SET state = ?2, result = ?3, error = ?4, output_tail = ?5, ended_ms = ?6
                 WHERE id = ?1",
                params![
                    id,
                    next,
                    result.map(|value| value.to_string()),
                    error,
                    output_tail,
                    now as i64
                ],
            )
            .map_err(sql)?;
        }
        let job = tx
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                [id],
                job_row,
            )
            .map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(job)
    }

    /// Ends an open job whose owner is gone, only if it is still in the
    /// state and with the owner `seen` had; false if it changed.
    pub(super) fn end_orphan(
        &mut self,
        seen: &Job,
        state: &str,
        error: &str,
        result: Option<&Value>,
        now: u64,
    ) -> Result<bool, String> {
        self.conn
            .execute(
                "UPDATE jobs SET state = ?2, error = ?3, result = COALESCE(?4, result), ended_ms = ?5
                 WHERE id = ?1 AND state = ?6 AND owner_pid IS ?7 AND owner_started IS ?8
                   AND state IN ('queued','running','cancel_requested')",
                params![
                    seen.id,
                    state,
                    error,
                    result.map(|value| value.to_string()),
                    now as i64,
                    seen.state,
                    seen.owner.map(|(pid, _)| pid),
                    seen.owner.map(|(_, started)| started as i64)
                ],
            )
            .map(|changed| changed == 1)
            .map_err(sql)
    }

    // Worktrees.

    /// Whether `name` or `path` is held by a worktree that is not gone.
    pub(super) fn taken(&self, repo_key: &str, name: &str, path: &str) -> Result<bool, String> {
        self.conn
            .query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM worktrees WHERE state IN {ACTIVE}
                       AND ((repo_key = ?1 AND name = ?2) OR path = ?3))"
                ),
                params![repo_key, name, path],
                |row| row.get(0),
            )
            .map_err(sql)
    }

    /// Records the worktree a create job is about to make.
    pub(super) fn reserve(
        &mut self,
        job: i64,
        reservation: &Reservation<'_>,
        now: u64,
    ) -> Result<i64, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        tx.execute(
            "INSERT INTO worktrees(repo_common_dir, repo_key, repo_root, name, branch, from_oid,
               path, state, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'reserved', ?8)",
            params![
                reservation.repo_common_dir,
                reservation.repo_key,
                reservation.repo_root,
                reservation.name,
                reservation.branch,
                reservation.from_oid,
                reservation.path,
                now as i64
            ],
        )
        .map_err(sql)?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE jobs SET worktree_id = ?2 WHERE id = ?1",
            params![job, id],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(id)
    }

    pub(super) fn worktree(&self, id: i64) -> Result<Option<Worktree>, String> {
        self.conn
            .query_row(
                &format!("SELECT {WORKTREE_COLUMNS} FROM worktrees WHERE id = ?1"),
                [id],
                worktree_row,
            )
            .optional()
            .map_err(sql)
    }

    /// Worktrees, newest first; gone ones only with `all`.
    pub(super) fn worktrees(
        &self,
        repo_key: Option<&str>,
        all: bool,
    ) -> Result<Vec<Worktree>, String> {
        let filter = if all {
            String::new()
        } else {
            format!("AND state IN {ACTIVE}")
        };
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT {WORKTREE_COLUMNS} FROM worktrees
                 WHERE (?1 IS NULL OR repo_key = ?1) {filter} ORDER BY id DESC"
            ))
            .map_err(sql)?;
        statement
            .query_map([repo_key], worktree_row)
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    /// Worktrees still being made whose create job is no longer open.
    pub(super) fn unfinished_worktrees(&self) -> Result<Vec<i64>, String> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT id FROM worktrees w WHERE state IN ('reserved','creating')
                 AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.worktree_id = w.id
                   AND j.state IN ('queued','running','cancel_requested'))",
            )
            .map_err(sql)?;
        statement
            .query_map([], |row| row.get(0))
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    /// The newest job of each listed worktree.
    pub(super) fn last_job(&self, worktree: i64) -> Result<Option<Job>, String> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {JOB_COLUMNS} FROM jobs WHERE worktree_id = ?1 ORDER BY id DESC LIMIT 1"
                ),
                [worktree],
                job_row,
            )
            .optional()
            .map_err(sql)
    }

    pub(super) fn set_creating(&mut self, id: i64, identity: (u64, u64)) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE worktrees SET state = 'creating', dev = ?2, ino = ?3
                 WHERE id = ?1 AND state = 'reserved'",
                params![id, identity.0 as i64, identity.1 as i64],
            )
            .map_err(sql)
            .and_then(|changed| {
                (changed == 1)
                    .then_some(())
                    .ok_or_else(|| format!("worktree {id} is no longer reserved"))
            })
    }

    pub(super) fn set_branch_created(&mut self, id: i64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE worktrees SET branch_created = 1 WHERE id = ?1",
                [id],
            )
            .map(drop)
            .map_err(sql)
    }

    /// Moves a worktree from one of `from` to `to`; false if it was in none.
    pub(super) fn set_state(
        &mut self,
        id: i64,
        from: &[&str],
        to: &str,
        now: u64,
    ) -> Result<bool, String> {
        let from = from
            .iter()
            .map(|state| format!("'{state}'"))
            .collect::<Vec<_>>()
            .join(",");
        let gone = matches!(to, "removed" | "failed");
        self.conn
            .execute(
                &format!(
                    "UPDATE worktrees SET state = ?2,
                       removed_ms = CASE WHEN ?3 THEN ?4 ELSE removed_ms END
                     WHERE id = ?1 AND state IN ({from})"
                ),
                params![id, to, gone, now as i64],
            )
            .map(|changed| changed == 1)
            .map_err(sql)
    }

    // Leases.

    /// Records that `run` on `socket` is starting in a worktree, with its
    /// run history row.
    pub(super) fn insert_lease(
        &mut self,
        worktree: i64,
        socket: &str,
        boot: &str,
        run: &str,
        until_ms: u64,
        now: u64,
    ) -> Result<i64, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        tx.execute(
            "INSERT INTO leases(worktree_id, socket, boot, run, state, until_ms, created_ms)
             VALUES (?1, ?2, ?3, ?4, 'launching', ?5, ?6)",
            params![worktree, socket, boot, run, until_ms as i64, now as i64],
        )
        .map_err(sql)?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT OR IGNORE INTO runs(worktree_id, socket, boot, run, started_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![worktree, socket, boot, run, now as i64],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(id)
    }

    pub(super) fn leases(&self, worktree: Option<i64>) -> Result<Vec<LeaseRow>, String> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, worktree_id, socket, boot, run, pane, state, until_ms FROM leases
                 WHERE ?1 IS NULL OR worktree_id = ?1 ORDER BY id",
            )
            .map_err(sql)?;
        statement
            .query_map([worktree], |row| {
                Ok(LeaseRow {
                    id: row.get(0)?,
                    worktree: row.get(1)?,
                    socket: row.get(2)?,
                    boot: row.get(3)?,
                    run: row.get(4)?,
                    pane: row.get(5)?,
                    state: row.get(6)?,
                    until_ms: row.get::<_, i64>(7)? as u64,
                })
            })
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    /// The run is registered in its pane.
    pub(super) fn set_lease_live(&mut self, id: i64, pane: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE leases SET state = 'live', pane = ?2 WHERE id = ?1",
                params![id, pane],
            )
            .map(drop)
            .map_err(sql)
    }

    /// Drops a lease whose run ended or never started; its run history row
    /// records the end.
    pub(super) fn end_lease(&mut self, lease: &LeaseRow, now: u64) -> Result<(), String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        tx.execute("DELETE FROM leases WHERE id = ?1", [lease.id])
            .map_err(sql)?;
        tx.execute(
            "UPDATE runs SET ended_ms = ?4 WHERE socket = ?1 AND boot = ?2 AND run = ?3
               AND ended_ms IS NULL",
            params![lease.socket, lease.boot, lease.run, now as i64],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)
    }

    /// Sockets any run in this worktree used, from leases and history.
    pub(super) fn run_sockets(&self, worktree: i64) -> Result<Vec<String>, String> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT socket FROM leases WHERE worktree_id = ?1
                 UNION SELECT socket FROM runs WHERE worktree_id = ?1",
            )
            .map_err(sql)?;
        statement
            .query_map([worktree], |row| row.get(0))
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    /// Worktrees of a repository that are not gone, matching a name or else
    /// a branch.
    pub(super) fn find(&self, repo_key: &str, spec: &str) -> Result<Vec<Worktree>, String> {
        let by = |column: &str| -> Result<Vec<Worktree>, String> {
            let mut statement = self
                .conn
                .prepare(&format!(
                    "SELECT {WORKTREE_COLUMNS} FROM worktrees
                     WHERE repo_key = ?1 AND {column} = ?2 AND state IN {ACTIVE} ORDER BY id"
                ))
                .map_err(sql)?;
            statement
                .query_map(params![repo_key, spec], worktree_row)
                .map_err(sql)?
                .collect::<Result<_, _>>()
                .map_err(sql)
        };
        let named = by("name")?;
        if !named.is_empty() {
            return Ok(named);
        }
        by("branch")
    }

    /// Marks a ready (or half-deleted) worktree as being removed by `job`.
    pub(super) fn begin_removing(&mut self, id: i64, job: i64) -> Result<Option<String>, String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let state: Option<String> = tx
            .query_row("SELECT state FROM worktrees WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .map_err(sql)?;
        let Some(state) = state else {
            return Ok(None);
        };
        if state == "ready" {
            tx.execute(
                "UPDATE worktrees SET state = 'removing', removing_job = ?2 WHERE id = ?1",
                params![id, job],
            )
            .map_err(sql)?;
        } else if state == "deleting" {
            tx.execute(
                "UPDATE worktrees SET removing_job = ?2 WHERE id = ?1",
                params![id, job],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
        Ok(Some(state))
    }

    /// The job removing this worktree, if any.
    pub(super) fn removing_job(&self, id: i64) -> Result<Option<i64>, String> {
        self.conn
            .query_row(
                "SELECT removing_job FROM worktrees WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map(Option::flatten)
            .map_err(sql)
    }

    /// Ends a removal that did not delete anything: back to ready.
    pub(super) fn stop_removing(&mut self, id: i64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE worktrees SET state = CASE WHEN state = 'removing' THEN 'ready' ELSE state END,
                   removing_job = NULL WHERE id = ?1",
                [id],
            )
            .map(drop)
            .map_err(sql)
    }

    /// Records paths setup put in a worktree.
    pub(super) fn add_setup_paths(
        &mut self,
        worktree: i64,
        paths: &[String],
        kind: &str,
    ) -> Result<(), String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        for path in paths {
            tx.execute(
                "INSERT OR IGNORE INTO setup_paths(worktree_id, path, kind) VALUES (?1, ?2, ?3)",
                params![worktree, path, kind],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)
    }

    /// An open setup job of this worktree, if any.
    pub(super) fn open_setup(&self, worktree: i64) -> Result<Option<i64>, String> {
        self.conn
            .query_row(
                "SELECT id FROM jobs WHERE kind = 'setup' AND worktree_id = ?1
                   AND state IN ('queued','running','cancel_requested') LIMIT 1",
                [worktree],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)
    }

    /// Trusts a repository's own setup file: these bytes, in this git
    /// directory.
    pub(super) fn trust(
        &mut self,
        common_dir: &str,
        identity: (u64, u64),
        sha256: &str,
        now: u64,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO trusted_repos(common_dir, dev, ino, file_sha256, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(common_dir) DO UPDATE SET dev = ?2, ino = ?3, file_sha256 = ?4,
                   created_ms = ?5",
                params![
                    common_dir,
                    identity.0 as i64,
                    identity.1 as i64,
                    sha256,
                    now as i64
                ],
            )
            .map(drop)
            .map_err(sql)
    }

    pub(super) fn untrust(&mut self, common_dir: &str) -> Result<bool, String> {
        self.conn
            .execute(
                "DELETE FROM trusted_repos WHERE common_dir = ?1",
                [common_dir],
            )
            .map(|changed| changed == 1)
            .map_err(sql)
    }

    /// The git directory identity and file digest a repository is trusted
    /// with.
    pub(super) fn trusted(&self, common_dir: &str) -> Result<Option<Trust>, String> {
        self.conn
            .query_row(
                "SELECT dev, ino, file_sha256 FROM trusted_repos WHERE common_dir = ?1",
                [common_dir],
                |row| {
                    Ok((
                        (row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64),
                        row.get(2)?,
                    ))
                },
            )
            .optional()
            .map_err(sql)
    }

    /// Paths setup put in a worktree, which removal does not ask about.
    /// With their kind: `ignored` for a directory, `copy:<sha256>` for a
    /// copied file.
    pub(super) fn setup_paths(&self, worktree: i64) -> Result<Vec<(String, String)>, String> {
        let mut statement = self
            .conn
            .prepare("SELECT path, kind FROM setup_paths WHERE worktree_id = ?1 ORDER BY path")
            .map_err(sql)?;
        statement
            .query_map([worktree], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    /// Drops jobs that ended and worktrees that went more than 30 days ago,
    /// with their output files.
    pub(super) fn prune(&mut self, now: u64) -> Result<(), String> {
        let before = now.saturating_sub(RETAIN_MS) as i64;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let ids: Vec<i64> = {
            let mut statement = tx
                .prepare(
                    "SELECT id FROM jobs WHERE ended_ms IS NOT NULL AND ended_ms < ?1
                     AND state NOT IN ('queued','running','cancel_requested')",
                )
                .map_err(sql)?;
            statement
                .query_map([before], |row| row.get(0))
                .map_err(sql)?
                .collect::<Result<_, _>>()
                .map_err(sql)?
        };
        tx.execute(
            "DELETE FROM jobs WHERE ended_ms IS NOT NULL AND ended_ms < ?1
             AND state NOT IN ('queued','running','cancel_requested')",
            [before],
        )
        .map_err(sql)?;
        for table in ["leases", "runs", "setup_paths"] {
            tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE worktree_id IN (SELECT id FROM worktrees
                       WHERE state IN ('removed','failed') AND removed_ms < ?1)"
                ),
                [before],
            )
            .map_err(sql)?;
        }
        tx.execute(
            "DELETE FROM worktrees WHERE state IN ('removed','failed') AND removed_ms < ?1
             AND id NOT IN (SELECT worktree_id FROM jobs WHERE worktree_id IS NOT NULL)",
            [before],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)?;
        for id in ids {
            if let Ok(path) = self.job_log(id) {
                let _ = fs::remove_file(path);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        let dir =
            std::env::temp_dir().join(format!("masil-wt-{}", crate::managed::nonce().unwrap()));
        Registry::open_in(crate::managed::private_directory(&dir).unwrap()).unwrap()
    }

    fn queued(registry: &mut Registry, resource: &str) -> i64 {
        let (job, new) = registry
            .insert_job("create", resource, None, &json!({"r": resource}), 1)
            .unwrap();
        assert!(new);
        job.id
    }

    #[test]
    fn a_claim_waits_for_its_resource_and_the_running_limit() {
        let mut registry = registry();
        let a = queued(&mut registry, "repo:a");
        let a2 = queued(&mut registry, "repo:a");
        let b = queued(&mut registry, "repo:b");
        let c = queued(&mut registry, "repo:c");
        let live = |_: &Job| true;
        assert!(matches!(
            registry.claim(a, (10, 1), &live, 2).unwrap(),
            Claim::Running(_)
        ));
        assert!(matches!(
            registry.claim(a2, (11, 1), &live, 2).unwrap(),
            Claim::Wait
        ));
        assert!(matches!(
            registry.claim(b, (12, 1), &live, 2).unwrap(),
            Claim::Running(_)
        ));
        assert!(matches!(
            registry.claim(c, (13, 1), &live, 2).unwrap(),
            Claim::Wait
        ));
        // A job being cancelled still holds its slot.
        assert!(matches!(
            registry.request_cancel(a, 3).unwrap(),
            CancelStep::Signal(_)
        ));
        assert!(matches!(
            registry.claim(c, (13, 1), &live, 3).unwrap(),
            Claim::Wait
        ));
        registry
            .finish_job(a, End::Cancelled("stop".into()), None, 4)
            .unwrap();
        assert!(matches!(
            registry.claim(c, (13, 1), &live, 5).unwrap(),
            Claim::Running(_)
        ));
    }

    #[test]
    fn a_dead_owners_job_on_the_same_resource_is_settled_before_a_claim() {
        let mut registry = registry();
        let a = queued(&mut registry, "repo:a");
        let a2 = queued(&mut registry, "repo:a");
        let b = queued(&mut registry, "repo:b");
        let live = |_: &Job| true;
        assert!(matches!(
            registry.claim(a, (10, 1), &live, 2).unwrap(),
            Claim::Running(_)
        ));
        let only_new = |job: &Job| job.owner != Some((10, 1));
        match registry.claim(a2, (11, 1), &only_new, 3).unwrap() {
            Claim::Reconcile(jobs) => {
                assert_eq!(jobs.iter().map(|j| j.id).collect::<Vec<_>>(), [a])
            }
            _ => panic!("expected reconcile"),
        }
        // Another resource is not held up by it.
        assert!(matches!(
            registry.claim(b, (12, 1), &only_new, 3).unwrap(),
            Claim::Running(_)
        ));
        let seen = registry.job(a).unwrap().unwrap();
        assert!(
            registry
                .end_orphan(&seen, "failed", "gone", None, 4)
                .unwrap()
        );
        assert!(matches!(
            registry.claim(a2, (11, 1), &only_new, 5).unwrap(),
            Claim::Running(_)
        ));
    }

    #[test]
    fn only_the_recorded_owner_can_claim_or_record_itself() {
        let mut registry = registry();
        let a = queued(&mut registry, "repo:a");
        assert!(registry.set_owner(a, 10, 1).unwrap());
        assert!(registry.set_owner(a, 10, 1).unwrap());
        assert!(!registry.set_owner(a, 11, 1).unwrap());
        assert!(matches!(
            registry.claim(a, (11, 1), &|_| true, 2).unwrap(),
            Claim::Gone
        ));
        assert!(matches!(
            registry.claim(a, (10, 1), &|_| true, 2).unwrap(),
            Claim::Running(_)
        ));
        assert_eq!(registry.job(a).unwrap().unwrap().owner, Some((10, 1)));
    }

    #[test]
    fn a_finish_after_a_cancel_request_is_too_late_and_an_orphan_end_stays() {
        let mut registry = registry();
        let a = queued(&mut registry, "repo:a");
        registry.claim(a, (10, 1), &|_| true, 2).unwrap();
        registry.request_cancel(a, 3).unwrap();
        let job = registry
            .finish_job(a, End::Succeeded(json!({"ok": 1})), Some("out"), 4)
            .unwrap();
        assert_eq!(job.state, "too_late");
        assert_eq!(job.result, Some(json!({"ok": 1})));
        let seen = registry.job(a).unwrap().unwrap();
        assert!(
            !registry
                .end_orphan(&seen, "failed", "late", None, 5)
                .unwrap()
        );

        let b = queued(&mut registry, "repo:b");
        registry.claim(b, (11, 1), &|_| true, 2).unwrap();
        let seen = registry.job(b).unwrap().unwrap();
        assert!(
            registry
                .end_orphan(&seen, "outcome_unknown", "gone", None, 3)
                .unwrap()
        );
        // A job that changed since it was read keeps its state.
        let c = queued(&mut registry, "repo:c");
        let seen = registry.job(c).unwrap().unwrap();
        registry.claim(c, (12, 1), &|_| true, 2).unwrap();
        assert!(
            !registry
                .end_orphan(&seen, "failed", "late", None, 3)
                .unwrap()
        );
        assert_eq!(registry.job(c).unwrap().unwrap().state, "running");
        let job = registry
            .finish_job(b, End::Succeeded(json!({})), None, 4)
            .unwrap();
        assert_eq!(job.state, "outcome_unknown");
    }

    #[test]
    fn a_client_key_returns_its_job_and_refuses_another_request() {
        let mut registry = registry();
        let request = json!({"branch": "a"});
        let (first, new) = registry
            .insert_job("create", "r", Some("k1"), &request, 1)
            .unwrap();
        assert!(new);
        let (again, new) = registry
            .insert_job("create", "r", Some("k1"), &request, 2)
            .unwrap();
        assert!(!new);
        assert_eq!(again.id, first.id);
        let error = registry
            .insert_job("create", "r", Some("k1"), &json!({"branch": "b"}), 3)
            .unwrap_err();
        assert!(error.starts_with("invalid_argument:"), "{error}");
    }

    #[test]
    fn a_name_is_free_again_once_its_worktree_is_gone() {
        let mut registry = registry();
        let job = queued(&mut registry, "repo:a");
        let reservation = Reservation {
            repo_common_dir: "/r/.git",
            repo_key: "k",
            repo_root: "/r",
            name: "feat",
            branch: "feat",
            from_oid: "0",
            path: "/w/k/feat",
        };
        let id = registry.reserve(job, &reservation, 1).unwrap();
        assert!(registry.taken("k", "feat", "/elsewhere").unwrap());
        assert!(registry.reserve(job, &reservation, 1).is_err());
        assert!(
            registry
                .set_state(id, &["reserved", "creating"], "failed", 2)
                .unwrap()
        );
        assert!(!registry.taken("k", "feat", "/w/k/feat").unwrap());
        registry.reserve(job, &reservation, 3).unwrap();
    }

    #[test]
    fn prune_drops_old_ended_jobs_and_gone_worktrees() {
        let mut registry = registry();
        let old = queued(&mut registry, "repo:a");
        registry.claim(old, (10, 1), &|_| true, 2).unwrap();
        registry
            .finish_job(old, End::Failed("x".into()), None, 3)
            .unwrap();
        let open = queued(&mut registry, "repo:b");
        registry.prune(RETAIN_MS + 10).unwrap();
        assert!(registry.job(old).unwrap().is_none());
        assert!(registry.job(open).unwrap().is_some());
    }
}
