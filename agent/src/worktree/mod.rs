//! `masil-agent worktree`: git worktrees that masil makes and tracks (P5).
//! Jobs that change git run in a detached helper each, so they need no
//! server and survive the command that asked for them.

mod create;
mod git;
mod helper;
mod registry;

use registry::{Job, Registry, now_ms};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const HELP: &str = "\
usage: masil-agent worktree create REPO BRANCH [--from REF] [--name NAME] [--key KEY] [--no-wait]
       masil-agent worktree list [REPO] [--all]
       masil-agent worktree jobs [JOB]
       masil-agent worktree cancel JOB [--no-wait]";
/// Jobs `worktree jobs` shows.
const JOB_LIST: usize = 50;
/// Output lines a failed job's error shows.
const ERROR_LINES: usize = 5;

pub(crate) fn run(args: &[String]) -> Result<i32, String> {
    match args.first().map(String::as_str) {
        None | Some("--help" | "help") => {
            println!("{HELP}");
            Ok(0)
        }
        Some("create") => create(&args[1..]),
        Some("list") => list(&args[1..]),
        Some("jobs") => jobs(&args[1..]),
        Some("cancel") => cancel(&args[1..]),
        Some("job-run") => match &args[1..] {
            [id] => helper::run(job_id(id)?),
            _ => Err(format!("usage: {HELP}")),
        },
        Some(other) => Err(format!("usage: unknown worktree command {other}\n{HELP}")),
    }
}

fn print(value: &Value) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn job_id(text: &str) -> Result<i64, String> {
    text.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| format!("invalid_argument: {text} is not a job ID"))
}

/// Client keys follow the operation ID rule.
fn valid_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

/// Splits flags that take a value, bare flags and positional arguments.
struct Parsed {
    positional: Vec<String>,
    values: Vec<(String, String)>,
    flags: Vec<String>,
}

fn parse(args: &[String], with_value: &[&str], bare: &[&str]) -> Result<Parsed, String> {
    let mut parsed = Parsed {
        positional: Vec::new(),
        values: Vec::new(),
        flags: Vec::new(),
    };
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if with_value.contains(&argument.as_str()) {
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("usage: {argument} needs a value"))?;
            if parsed.values.iter().any(|(flag, _)| flag == argument) {
                return Err(format!("usage: {argument} given twice"));
            }
            parsed.values.push((argument.clone(), value.clone()));
            index += 2;
            continue;
        }
        if bare.contains(&argument.as_str()) {
            parsed.flags.push(argument.clone());
        } else if argument.starts_with('-') {
            return Err(format!("usage: unknown option {argument}\n{HELP}"));
        } else {
            parsed.positional.push(argument.clone());
        }
        index += 1;
    }
    Ok(parsed)
}

impl Parsed {
    fn value(&self, flag: &str) -> Option<String> {
        self.values
            .iter()
            .find(|(name, _)| name == flag)
            .map(|(_, value)| value.clone())
    }

    fn flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|name| name == flag)
    }
}

/// A repository as jobs name it: the main worktree, the common git
/// directory and the key derived from that directory.
pub(super) struct Repo {
    root: String,
    common_dir: String,
    key: String,
}

fn utf8(path: PathBuf, what: &str) -> Result<String, String> {
    path.into_os_string()
        .into_string()
        .map_err(|_| format!("invalid_argument: the {what} path is not UTF-8"))
}

fn repository(path: &str) -> Result<Repo, String> {
    let dir = Path::new(path)
        .canonicalize()
        .map_err(|error| format!("invalid_argument: {path}: {error}"))?;
    let invalid =
        |error: String| format!("repo_invalid: {path} is not in a git work tree: {error}");
    let output = git::query(
        &dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map_err(invalid)?;
    let common = Path::new(output.trim())
        .canonicalize()
        .map_err(|error| invalid(error.to_string()))?;
    // The main worktree outlives every linked one, so jobs run git there.
    let list = git::query(&dir, &["worktree", "list", "--porcelain", "-z"]).map_err(invalid)?;
    let main = list
        .split('\0')
        .find_map(|field| field.strip_prefix("worktree "))
        .ok_or_else(|| invalid("git lists no worktree".into()))?;
    let bare = list.split('\0').any(|field| field == "bare");
    if bare {
        return Err(format!("repo_invalid: {path} is a bare repository"));
    }
    let root = Path::new(main)
        .canonicalize()
        .map_err(|error| invalid(error.to_string()))?;
    let common_dir = utf8(common, "git directory")?;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(common_dir.as_bytes());
    Ok(Repo {
        root: utf8(root, "repository")?,
        key: digest[..8].iter().map(|b| format!("{b:02x}")).collect(),
        common_dir,
    })
}

/// `$XDG_DATA_HOME/masil/worktrees`, else `~/.local/share/masil/worktrees`,
/// private and canonical.
fn data_root() -> Result<String, String> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/share"))
        })
        .ok_or("no absolute XDG_DATA_HOME or HOME")?;
    utf8(
        crate::managed::private_directory(&base.join("masil/worktrees"))?,
        "worktree root",
    )
}

fn check_branch(repo: &Repo, branch: &str) -> Result<(), String> {
    let normalized = git::query(
        Path::new(&repo.root),
        &["check-ref-format", "--branch", branch],
    )
    .map_err(|error| format!("invalid_argument: {branch} is not a branch name: {error}"))?;
    if branch.starts_with('-') || normalized.trim() != branch {
        return Err(format!("invalid_argument: {branch} is not a branch name"));
    }
    Ok(())
}

fn create(args: &[String]) -> Result<i32, String> {
    let parsed = parse(args, &["--from", "--name", "--key"], &["--no-wait"])?;
    let [repo, branch] = parsed.positional.as_slice() else {
        return Err(format!("usage: {HELP}"));
    };
    let repo = repository(repo)?;
    check_branch(&repo, branch)?;
    let name = parsed.value("--name");
    if let Some(name) = &name
        && !create::valid_name(name)
    {
        return Err(format!(
            "invalid_argument: a worktree name is 1-64 of A-Z a-z 0-9 . _ - and does not start with . or -: {name}"
        ));
    }
    let key = parsed.value("--key");
    if let Some(key) = &key
        && !valid_key(key)
    {
        return Err(format!("invalid_argument: {key} is not a valid key"));
    }
    let from = parsed.value("--from");
    if let Some(from) = &from {
        let resolved = if from.starts_with('-') {
            None
        } else {
            git::probe(
                Path::new(&repo.root),
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &format!("{from}^{{commit}}"),
                ],
            )?
        };
        if resolved.is_none() {
            return Err(format!("invalid_argument: --from {from} is not a commit"));
        }
    }
    let request = create::Request {
        repo_root: repo.root,
        common_dir: repo.common_dir,
        repo_key: repo.key.clone(),
        branch: branch.clone(),
        from,
        name,
        data_root: data_root()?,
    };
    let request = serde_json::to_value(&request).map_err(|error| error.to_string())?;
    let mut registry = Registry::open()?;
    helper::sweep(&mut registry)?;
    registry.prune(now_ms())?;
    let (job, new) = registry.insert_job(
        "create",
        &format!("repo:{}", repo.key),
        key.as_deref(),
        &request,
        now_ms(),
    )?;
    // A retry with the same key waits for the job it made; it never starts
    // a second helper.
    let child = if new {
        match helper::spawn(&registry, job.id) {
            Ok(child) => Some(child),
            Err(error) => {
                registry.end_orphan(&job, "failed", &error, None, now_ms())?;
                return Err(format!("job_failed: {error} (worktree job {})", job.id));
            }
        }
    } else {
        None
    };
    if parsed.flag("--no-wait") {
        print(&job.to_json(false))?;
        return Ok(0);
    }
    let job = helper::wait(&mut registry, job.id, child)?;
    report(&job)
}

/// Prints a finished job; failures become the error of the command.
fn report(job: &Job) -> Result<i32, String> {
    let error = || {
        let mut text = job.error.clone().unwrap_or_default();
        if let Some(output) = &job.output_tail {
            let lines: Vec<&str> = output.lines().collect();
            let start = lines.len().saturating_sub(ERROR_LINES);
            for line in &lines[start..] {
                text.push_str("\n  ");
                text.push_str(line);
            }
        }
        text
    };
    match job.state.as_str() {
        "failed" => Err(format!("job_failed: worktree job {}: {}", job.id, error())),
        "cancelled" => Err(format!(
            "job_cancelled: worktree job {}: {}",
            job.id,
            error()
        )),
        "outcome_unknown" => Err(format!(
            "outcome_unknown: worktree job {}: {}",
            job.id,
            error()
        )),
        _ => {
            print(&job.to_json(false))?;
            Ok(0)
        }
    }
}

fn list(args: &[String]) -> Result<i32, String> {
    let parsed = parse(args, &[], &["--all"])?;
    let key = match parsed.positional.as_slice() {
        [] => None,
        [repo] => Some(repository(repo)?.key),
        _ => return Err(format!("usage: {HELP}")),
    };
    let registry = Registry::open()?;
    let mut worktrees = Vec::new();
    for worktree in registry.worktrees(key.as_deref(), parsed.flag("--all"))? {
        let mut value = worktree.to_json();
        if let Some(job) = registry.last_job(worktree.id)? {
            value["last_job"] = json!({"id": job.id, "kind": job.kind, "state": job.state});
        }
        worktrees.push(value);
    }
    print(&json!({"worktrees": worktrees}))?;
    Ok(0)
}

fn jobs(args: &[String]) -> Result<i32, String> {
    let registry = Registry::open()?;
    match args {
        [] => {
            let jobs: Vec<Value> = registry
                .jobs(JOB_LIST)?
                .iter()
                .map(|job| {
                    let mut value = job.to_json(false);
                    if job.open() {
                        value["owner_alive"] = json!(helper::alive(job));
                    }
                    value
                })
                .collect();
            print(&json!({"jobs": jobs}))?;
        }
        [id] => {
            let id = job_id(id)?;
            let job = registry.job(id)?.ok_or_else(|| {
                format!("target_absent: worktree job {id} is not in the registry")
            })?;
            let mut value = job.to_json(true);
            if job.open() {
                value["owner_alive"] = json!(helper::alive(&job));
            }
            print(&value)?;
        }
        _ => return Err(format!("usage: {HELP}")),
    }
    Ok(0)
}

fn cancel(args: &[String]) -> Result<i32, String> {
    let parsed = parse(args, &[], &["--no-wait"])?;
    let [id] = parsed.positional.as_slice() else {
        return Err(format!("usage: {HELP}"));
    };
    let id = job_id(id)?;
    let mut registry = Registry::open()?;
    let mut job = helper::cancel(&mut registry, id)?;
    if job.open() && !parsed.flag("--no-wait") {
        job = helper::wait(&mut registry, id, None)?;
    }
    print(&job.to_json(false))?;
    Ok(0)
}
