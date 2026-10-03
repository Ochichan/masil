//! Declarative, explicitly confirmed native session layouts.

use crate::managed::Manager;
use crate::providers;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const MAX_TEMPLATE_BYTES: u64 = 64 * 1024;
const MAX_WINDOWS: usize = 16;
const MAX_PANES_PER_WINDOW: usize = 16;
const MAX_PANES: usize = 64;
const MAX_ARGS: usize = 64;
const MAX_ARG_BYTES: usize = 8192;
const MAX_PATH_BYTES: usize = 4096;
/// Drafts an agent pane may queue, as the prompt queue allows a run.
const MAX_DRAFTS: usize = 64;
const MAX_DRAFT_BYTES: usize = 32 * 1024;
const DIRECT_EXEC: &str = "/usr/bin/env";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateFile {
    version: u32,
    session: String,
    root: Option<String>,
    windows: Vec<WindowFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowFile {
    name: Option<String>,
    layout: Option<String>,
    panes: Vec<PaneFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaneFile {
    cwd: Option<String>,
    command: Option<Vec<String>>,
    #[serde(default)]
    focus: bool,
    agent: Option<AgentFile>,
    /// An agent pane runs in this masil worktree; `cwd` is then a relative
    /// path inside it.
    worktree: Option<String>,
    /// Drafts put in the started agent's prompt queue, not sent.
    queue: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AgentFile {
    provider: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct Template {
    version: u32,
    session: String,
    root: String,
    windows: Vec<Window>,
}

#[derive(Clone, Debug, Serialize)]
struct Window {
    name: Option<String>,
    layout: Option<String>,
    panes: Vec<Pane>,
}

#[derive(Clone, Debug, Serialize)]
struct Pane {
    cwd: String,
    command: Option<Vec<String>>,
    focus: bool,
    agent: Option<AgentFile>,
    /// The worktree name or path, and the directory inside it, until it is
    /// resolved against the registry.
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree: Option<(String, Option<String>)>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    queue: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AgentAction {
    Start,
    Resume,
    SkipAlreadyRunning,
    SkipNameTaken,
}

#[derive(Debug, Serialize)]
struct Plan {
    stage: &'static str,
    template: String,
    session: String,
    session_exists: bool,
    executes_commands: bool,
    starts_agents: bool,
    requires_confirmation: bool,
    server_reachable: bool,
    windows: Vec<PlanWindow>,
    errors: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PlanWindow {
    name: Option<String>,
    layout: Option<String>,
    panes: Vec<PlanPane>,
}

#[derive(Debug, Serialize)]
struct PlanPane {
    index: usize,
    kind: &'static str,
    cwd: String,
    cwd_exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    argv: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<PlanAgent>,
}

#[derive(Debug, Serialize)]
struct PlanAgent {
    provider: String,
    name: String,
    session: Option<String>,
    action: AgentAction,
    #[serde(skip_serializing_if = "is_zero")]
    drafts: usize,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

struct PreparedPlan {
    template: Template,
    output: Plan,
    manager: Option<Manager>,
    actions: Vec<Vec<Option<AgentAction>>>,
}

#[derive(Debug, Deserialize)]
struct ManagedMetadata {
    name: String,
    provider: String,
    boot: String,
    generation: String,
    session: Option<String>,
    #[serde(default)]
    foreground_group: i32,
}

struct ExistingAgent {
    name: String,
    provider: String,
    session: Option<String>,
    running: bool,
}

#[derive(Debug, Serialize)]
struct ApplyResult {
    window: String,
    pane: usize,
    kind: &'static str,
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Drafts put in the started agent's queue.
    #[serde(skip_serializing_if = "Option::is_none")]
    queued: Option<usize>,
}

pub(crate) fn run(args: &[String]) -> Result<i32, String> {
    if args.is_empty() || args == ["help"] || args == ["--help"] || args == ["-h"] {
        println!(
            "masil-agent layout [--socket MASIL_SOCKET] list\n\
             masil-agent layout [--socket MASIL_SOCKET] show NAME\n\
             masil-agent layout [--socket MASIL_SOCKET] plan NAME [--session NAME]\n\
             masil-agent layout [--socket MASIL_SOCKET] apply NAME [--session NAME] [--yes]"
        );
        return Ok(0);
    }

    let mut index = 0;
    let mut socket = None;
    if args.get(index).is_some_and(|arg| arg == "--socket") {
        let value = args.get(index + 1).ok_or("missing value for --socket")?;
        if value.is_empty() {
            return Err("--socket must not be empty".into());
        }
        socket = Some(value.clone());
        index += 2;
    }
    let command = args.get(index).ok_or("missing layout command")?;
    let rest = &args[index + 1..];

    match command.as_str() {
        "list" if rest.is_empty() => {
            print_json(&list_layouts()?)?;
        }
        "show" if rest.len() == 1 => {
            let template = load_template(&rest[0])?;
            validate_directories(&template)?;
            print_json(&template)?;
        }
        "plan" if !rest.is_empty() => {
            let (name, session, yes) = command_options(rest, false)?;
            if yes {
                return Err("--yes is only valid with layout apply".into());
            }
            let socket = resolve_socket(socket.as_deref());
            let prepared = runtime()?.block_on(prepare_plan(
                load_template(name)?,
                name,
                session.as_deref(),
                socket,
                Duration::ZERO,
            ))?;
            print_json(&prepared.output)?;
        }
        "apply" if !rest.is_empty() => {
            let (name, session, yes) = command_options(rest, true)?;
            let socket = resolve_socket(socket.as_deref());
            let value = runtime()?.block_on(apply(name, session.as_deref(), socket, yes))?;
            print_json(&value)?;
        }
        _ => return Err("invalid layout command or arguments; use layout --help".into()),
    }
    Ok(0)
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
}

fn print_json(value: &impl Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn command_options(
    args: &[String],
    allow_yes: bool,
) -> Result<(&str, Option<String>, bool), String> {
    let name = args.first().ok_or("missing layout name")?;
    valid_layout_name(name)?;
    let mut session = None;
    let mut yes = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--session" if session.is_none() => {
                let value = args.get(index + 1).ok_or("missing value for --session")?;
                valid_session_name(value, 64, "session name")?;
                session = Some(value.clone());
                index += 2;
            }
            "--yes" if allow_yes && !yes => {
                yes = true;
                index += 1;
            }
            other => return Err(format!("unknown or repeated layout option: {other}")),
        }
    }
    Ok((name, session, yes))
}

fn resolve_socket(explicit: Option<&str>) -> Option<PathBuf> {
    explicit
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("TMUX").ok().and_then(|value| {
                value
                    .rsplitn(3, ',')
                    .last()
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from)
            })
        })
        .or_else(|| std::env::var_os("MASIL_AGENT_SOCKET").map(PathBuf::from))
}

fn layouts_dir() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(path.join("masil/layouts"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("HOME must be an absolute path")?;
    Ok(home.join(".config/masil/layouts"))
}

fn list_layouts() -> Result<Value, String> {
    let directory = layouts_dir()?;
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"layouts": []}));
        }
        Err(error) => return Err(format!("layout directory: {error}")),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("layout directory: {error}"))?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
            continue;
        }
        names.push(
            path.file_stem()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
    }
    names.sort();
    names.dedup();
    let layouts = names
        .into_iter()
        .map(|name| {
            match load_template(&name).and_then(|template| validate_directories(&template)) {
                Ok(()) => json!({"name":name,"valid":true}),
                Err(error) => json!({"name":name,"valid":false,"error":error}),
            }
        })
        .collect::<Vec<_>>();
    Ok(json!({"layouts": layouts}))
}

/// The file of the layout `name`.
pub(crate) fn template_path(name: &str) -> Result<PathBuf, String> {
    valid_layout_name(name)?;
    Ok(layouts_dir()?.join(format!("{name}.toml")))
}

/// A template file as parsed, with the SHA-256 of its bytes.
fn read_file(path: &Path) -> Result<(TemplateFile, String), String> {
    use sha2::{Digest, Sha256};
    let mut file = secure_open(path)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_TEMPLATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading layout {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_TEMPLATE_BYTES {
        return Err("layout template exceeds 64 KiB".into());
    }
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let text = std::str::from_utf8(&bytes).map_err(|_| "layout template is not UTF-8")?;
    let parsed: TemplateFile = toml::from_str(text).map_err(|error| error.to_string())?;
    Ok((parsed, digest))
}

fn load_template(name: &str) -> Result<Template, String> {
    let (parsed, _) = read_file(&template_path(name)?)?;
    normalize(parsed)
}

/// A layout as a schedule keeps it: the file, the digest of what the person
/// confirmed, and the directory a relative root is taken from.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct Kept {
    pub(crate) path: PathBuf,
    pub(crate) sha256: String,
    pub(crate) base: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session: Option<String>,
}

/// For `schedule add --layout`: the plan `layout plan` would show, what the
/// schedule keeps to apply it later as confirmed now, and whether it needs
/// confirming. That is decided by the template, not by what runs now: an
/// agent skipped today because it runs may start when the schedule does.
pub(crate) async fn plan_kept(
    name: &str,
    session: Option<&str>,
    socket: PathBuf,
) -> Result<(Value, Kept, bool), String> {
    let path = template_path(name)?;
    let (parsed, sha256) = read_file(&path)?;
    let base = std::env::current_dir().map_err(|error| error.to_string())?;
    let template = normalize_in(parsed, &base)?;
    let confirm = template
        .windows
        .iter()
        .flat_map(|window| &window.panes)
        .any(|pane| pane.agent.is_some() || pane.command.is_some());
    let prepared = prepare_plan(template, name, session, Some(socket), Duration::ZERO).await?;
    let plan = serde_json::to_value(&prepared.output).map_err(|error| error.to_string())?;
    let kept = Kept {
        path,
        sha256,
        base,
        session: session.map(str::to_owned),
    };
    Ok((plan, kept, confirm))
}

/// A scheduled apply, confirmed when the schedule was made. A file that
/// changed since is not applied. Each agent start waits up to `lock_wait`
/// for the management lock.
pub(crate) async fn apply_kept(
    kept: &Kept,
    socket: PathBuf,
    lock_wait: Duration,
) -> Result<Value, String> {
    let (parsed, sha256) = read_file(&kept.path)?;
    if sha256 != kept.sha256 {
        return Err(format!(
            "template_changed: {} changed after the schedule was made; run `schedule add` again to confirm it",
            kept.path.display()
        ));
    }
    let template = normalize_in(parsed, &kept.base)?;
    let label = kept.path.display().to_string();
    let prepared = prepare_plan(
        template,
        &label,
        kept.session.as_deref(),
        Some(socket),
        lock_wait,
    )
    .await?;
    apply_prepared(prepared, &label, true).await
}

/// The drafts the template at `path` gives the agent `agent`.
/// Paths are not resolved: drafts do not depend on them.
pub(crate) fn drafts_for(path: &Path, agent: &str) -> Result<Vec<String>, String> {
    let (parsed, _) = read_file(path)?;
    let drafts = parsed
        .windows
        .into_iter()
        .flat_map(|window| window.panes)
        .find(|pane| pane.agent.as_ref().is_some_and(|found| found.name == agent))
        .map(|pane| pane.queue.unwrap_or_default())
        .ok_or_else(|| format!("{} has no agent pane named {agent}", path.display()))?;
    if drafts.len() > MAX_DRAFTS
        || drafts
            .iter()
            .any(|draft| draft.trim().is_empty() || draft.len() > MAX_DRAFT_BYTES)
    {
        return Err(format!(
            "the queue of {agent} holds at most {MAX_DRAFTS} drafts, each not empty and at most 32 KiB"
        ));
    }
    Ok(drafts)
}

fn secure_open(path: &Path) -> Result<File, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("layout template: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err("layout template must be a regular file, not a symlink".into());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("layout template: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("layout template: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err("layout template must be a regular file".into());
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err("layout template must be owned by the current user".into());
    }
    if metadata.mode() & 0o022 != 0 {
        return Err("layout template must not be group/world-writable".into());
    }
    if metadata.len() > MAX_TEMPLATE_BYTES {
        return Err("layout template exceeds 64 KiB".into());
    }
    Ok(file)
}

fn normalize(parsed: TemplateFile) -> Result<Template, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    normalize_in(parsed, &cwd)
}

/// As [`normalize`], with a relative root taken from `base`.
fn normalize_in(parsed: TemplateFile, base: &Path) -> Result<Template, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    let mut template = normalize_from(parsed, base, home.as_deref())?;
    resolve_worktrees(&mut template)?;
    Ok(template)
}

/// Agent panes with `worktree` run in that masil worktree, found by name in
/// the repository of the template root. One that does not exist fails the
/// plan; layout never makes one.
fn resolve_worktrees(template: &mut Template) -> Result<(), String> {
    let root = PathBuf::from(&template.root);
    for (window_index, window) in template.windows.iter_mut().enumerate() {
        for (pane_index, pane) in window.panes.iter_mut().enumerate() {
            let Some((spec, cwd)) = pane.worktree.take() else {
                continue;
            };
            let dir = crate::worktree::start_dir(&spec, &root, cwd.as_deref().map(Path::new))
                .map_err(|error| format!("window {window_index} pane {pane_index}: {error}"))?;
            pane.cwd = path_text(&dir, "pane cwd")?;
        }
    }
    Ok(())
}

fn normalize_from(
    parsed: TemplateFile,
    current_dir: &Path,
    home: Option<&Path>,
) -> Result<Template, String> {
    if parsed.version != 1 {
        return Err("layout version must be 1".into());
    }
    valid_session_name(&parsed.session, 64, "session name")?;
    if parsed.windows.is_empty() || parsed.windows.len() > MAX_WINDOWS {
        return Err("layout must contain 1–16 windows".into());
    }
    let root = resolve_path(
        parsed.root.as_deref().unwrap_or("."),
        current_dir,
        home,
        "layout root",
    )?;
    let root_text = path_text(&root, "layout root")?;
    let mut windows = Vec::with_capacity(parsed.windows.len());
    let mut pane_count = 0;
    let mut focus_count = 0;
    for (window_index, window) in parsed.windows.into_iter().enumerate() {
        if let Some(name) = &window.name {
            valid_session_name(name, 32, "window name")?;
        }
        if let Some(layout) = &window.layout
            && !matches!(
                layout.as_str(),
                "even-horizontal" | "even-vertical" | "main-horizontal" | "main-vertical" | "tiled"
            )
        {
            return Err(format!(
                "invalid layout for window {window_index}: {layout}"
            ));
        }
        if window.panes.is_empty() || window.panes.len() > MAX_PANES_PER_WINDOW {
            return Err(format!("window {window_index} must contain 1–16 panes"));
        }
        pane_count += window.panes.len();
        if pane_count > MAX_PANES {
            return Err("layout must contain at most 64 panes".into());
        }
        let mut panes = Vec::with_capacity(window.panes.len());
        for (pane_index, mut pane) in window.panes.into_iter().enumerate() {
            if pane.command.is_some() && pane.agent.is_some() {
                return Err(format!(
                    "window {window_index} pane {pane_index} cannot set both command and agent"
                ));
            }
            if pane_index == 0 && pane.agent.is_some() {
                return Err(format!(
                    "window {window_index} first pane must be a shell or command pane"
                ));
            }
            if let Some(command) = &pane.command {
                validate_argv(command)?;
            }
            if let Some(agent) = &mut pane.agent {
                valid_agent_name(&agent.name)?;
                let provider = providers::find(&agent.provider)
                    .ok_or_else(|| format!("unknown agent provider: {}", agent.provider))?;
                agent.provider = provider.id.into();
                if let Some(session) = &agent.session {
                    providers::resume(provider.id, session)?;
                }
            }
            if pane.focus {
                focus_count += 1;
                if focus_count > 1 {
                    return Err("at most one pane may set focus = true".into());
                }
            }
            let queue = pane.queue.take().unwrap_or_default();
            if !queue.is_empty() && pane.agent.is_none() {
                return Err(format!(
                    "window {window_index} pane {pane_index} sets queue but is not an agent pane"
                ));
            }
            if queue.len() > MAX_DRAFTS
                || queue.iter().any(|draft| {
                    draft.trim().is_empty() || draft.len() > MAX_DRAFT_BYTES || draft.contains('\0')
                })
            {
                return Err(format!(
                    "window {window_index} pane {pane_index} queue holds at most {MAX_DRAFTS} drafts, each not empty and at most 32 KiB"
                ));
            }
            if let Some(spec) = pane.worktree {
                if pane.agent.is_none() {
                    return Err(format!(
                        "window {window_index} pane {pane_index} sets worktree but is not an agent pane"
                    ));
                }
                if pane
                    .cwd
                    .as_deref()
                    .is_some_and(|cwd| cwd.starts_with('/') || cwd.starts_with('~'))
                {
                    return Err(format!(
                        "window {window_index} pane {pane_index} cwd must be relative inside its worktree"
                    ));
                }
                panes.push(Pane {
                    // Replaced by the worktree's directory before the plan.
                    cwd: root_text.clone(),
                    command: pane.command,
                    focus: pane.focus,
                    agent: pane.agent,
                    worktree: Some((spec, pane.cwd)),
                    queue,
                });
                continue;
            }
            let pane_cwd =
                resolve_path(pane.cwd.as_deref().unwrap_or("."), &root, home, "pane cwd")?;
            panes.push(Pane {
                cwd: path_text(&pane_cwd, "pane cwd")?,
                command: pane.command,
                focus: pane.focus,
                agent: pane.agent,
                worktree: None,
                queue,
            });
        }
        windows.push(Window {
            name: window.name,
            layout: window.layout,
            panes,
        });
    }
    Ok(Template {
        version: parsed.version,
        session: parsed.session,
        root: root_text,
        windows,
    })
}

fn resolve_path(
    value: &str,
    base: &Path,
    home: Option<&Path>,
    field: &str,
) -> Result<PathBuf, String> {
    if value.is_empty() || value.len() > MAX_PATH_BYTES || value.chars().any(char::is_control) {
        return Err(format!(
            "{field} exceeds bounds or contains control characters"
        ));
    }
    let expanded = if let Some(suffix) = value.strip_prefix("~/") {
        home.ok_or_else(|| format!("HOME is required to expand {field}"))?
            .join(suffix)
    } else {
        PathBuf::from(value)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    Ok(clean_path(&absolute))
}

fn clean_path(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            other => clean.push(other.as_os_str()),
        }
    }
    clean
}

fn path_text(path: &Path, field: &str) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("{field} is not UTF-8"))?;
    if text.len() > MAX_PATH_BYTES {
        return Err(format!("resolved {field} exceeds {MAX_PATH_BYTES} bytes"));
    }
    // tmux expands formats in -c arguments. Manager::start canonicalizes its
    // cwd before it reaches tmux, so doubling '#' is not available for agent
    // panes; reject it consistently for every layout path.
    if text.contains('#') {
        return Err(format!("{field} must not contain '#'"));
    }
    Ok(text.to_owned())
}

fn valid_layout_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 32
        || !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
    {
        return Err(
            "layout name must begin with a lowercase letter and use 1–32 lowercase letters, digits, '-' or '_'"
                .into(),
        );
    }
    Ok(())
}

fn valid_agent_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 32
        || !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
    {
        return Err(
            "agent name must begin with a lowercase letter and use 1–32 lowercase letters, digits, '-' or '_'"
                .into(),
        );
    }
    Ok(())
}

fn valid_session_name(name: &str, max: usize, field: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > max
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.' || byte == b'-'
        })
    {
        return Err(format!(
            "{field} must use 1–{max} ASCII letters, digits, '_', '.' or '-'"
        ));
    }
    Ok(())
}

fn validate_argv(argv: &[String]) -> Result<(), String> {
    if argv.is_empty()
        || argv.len() > MAX_ARGS
        || argv.iter().map(String::len).sum::<usize>() > MAX_ARG_BYTES
        || argv
            .iter()
            .any(|argument| argument.chars().any(char::is_control))
    {
        return Err(
            "command argv must contain 1–64 arguments and at most 8192 bytes without control characters"
                .into(),
        );
    }
    // A lone word runs through `/usr/bin/env --`, which reads NAME=VALUE as
    // an assignment rather than a program.
    if argv.len() == 1 && argv[0].contains('=') {
        return Err("a one-word command must not contain '='; add its arguments".into());
    }
    Ok(())
}

fn cwd_exists(cwd: &str) -> bool {
    fs::metadata(cwd).is_ok_and(|metadata| metadata.is_dir())
}

fn validate_directories(template: &Template) -> Result<(), String> {
    for (window_index, window) in template.windows.iter().enumerate() {
        for (pane_index, pane) in window.panes.iter().enumerate() {
            if !cwd_exists(&pane.cwd) {
                return Err(format!(
                    "window {window_index} pane {pane_index} cwd is not a directory: {}",
                    pane.cwd
                ));
            }
        }
    }
    Ok(())
}

fn decode_managed_metadata(encoded: &str) -> Option<ManagedMetadata> {
    if encoded.len() > 16_384 || !encoded.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|text| u8::from_str_radix(text, 16).ok())
        })
        .collect();
    serde_json::from_slice(&bytes?).ok()
}

/// Reads only the identity fields needed by layout duplicate checks. The
/// ordinary Manager::list path also advances shared observation revisions,
/// which a plan or refused apply must not do.
async fn read_only_agents(manager: &Manager) -> Result<(Vec<ExistingAgent>, usize), String> {
    const SEP: char = '\u{1f}';
    let format = [
        "#{pane_id}",
        "#{pane_dead}",
        "#{masil_core_boot_id}",
        "#{masil_pty_generation}",
        "#{pane_current_command}",
        "#{masil_foreground_pgid}",
        "#{@masil-managed-agent}",
    ]
    .join(&SEP.to_string());
    let output = manager
        .command(&["list-panes", "-a", "-F", &format])
        .await?;
    let mut agents = Vec::new();
    let mut panes = HashSet::new();
    for line in output.lines() {
        let fields: Vec<_> = line.split(SEP).collect();
        let [
            pane,
            dead,
            boot,
            generation,
            command,
            foreground_group,
            encoded,
        ] = fields.as_slice()
        else {
            return Err("invalid native pane inventory".into());
        };
        // Only options are read here, so the 64-pane management limit (set by
        // screen capture cost) does not apply.
        if !panes.insert(*pane) {
            continue;
        }
        let dead = *dead == "1";
        let foreground_group = foreground_group.parse::<i32>().unwrap_or(0);
        let mut metadata = decode_managed_metadata(encoded).filter(|metadata| {
            metadata.boot == *boot
                && metadata.generation == *generation
                && valid_agent_name(&metadata.name).is_ok()
                && providers::find(&metadata.provider).is_some()
                && (dead || metadata.foreground_group == foreground_group)
        });
        let identified = providers::identify(command);
        let Some(provider) = identified.or_else(|| {
            metadata
                .as_ref()
                .and_then(|metadata| providers::find(&metadata.provider))
        }) else {
            continue;
        };
        metadata = metadata.filter(|metadata| metadata.provider == provider.id);
        let metadata_matched = metadata.is_some();
        let name = metadata
            .as_ref()
            .map(|metadata| metadata.name.clone())
            .unwrap_or_else(|| format!("{}-{}", provider.id, pane.trim_start_matches('%')));
        agents.push(ExistingAgent {
            name,
            provider: provider.id.to_owned(),
            session: metadata.and_then(|metadata| metadata.session),
            // Metadata that passed the filters above belongs to the pane's
            // current foreground group, which covers Node/Bun/Python
            // launchers whose command name is the runtime.
            running: !dead
                && (metadata_matched
                    || identified.is_some_and(|identified| identified.id == provider.id)),
        });
    }
    Ok((agents, panes.len()))
}

async fn prepare_plan(
    template: Template,
    name: &str,
    session_override: Option<&str>,
    socket: Option<PathBuf>,
    lock_wait: Duration,
) -> Result<PreparedPlan, String> {
    let session = session_override.unwrap_or(&template.session).to_owned();
    valid_session_name(&session, 64, "session name")?;

    let mut manager = None;
    let mut agents = Vec::new();
    let mut existing_panes = 0;
    let mut session_names = HashSet::new();
    if let Some(socket) = socket
        && let Ok(candidate) =
            Manager::new(socket, None).map(|manager| manager.waiting_for_lock(lock_wait))
        && let Ok((found, panes)) = read_only_agents(&candidate).await
        && let Ok(output) = candidate
            .command(&["list-sessions", "-F", "#{session_name}"])
            .await
    {
        agents = found;
        existing_panes = panes;
        session_names.extend(output.lines().map(str::to_owned));
        manager = Some(candidate);
    }
    let server_reachable = manager.is_some();
    let template_panes: usize = template
        .windows
        .iter()
        .map(|window| window.panes.len())
        .sum();
    let session_exists = session_names.contains(&session);
    let executes_commands = template
        .windows
        .iter()
        .flat_map(|window| &window.panes)
        .any(|pane| pane.command.is_some());
    let mut starts_agents = false;
    let mut errors = Vec::new();
    if !server_reachable {
        errors.push("native server is not reachable".into());
    }
    if session_exists {
        errors.push(format!("target session already exists: {session}"));
    }
    let mut used_agent_names = agents
        .iter()
        .map(|agent| agent.name.clone())
        .collect::<HashSet<_>>();
    let mut used_agent_sessions = agents
        .iter()
        .filter(|agent| agent.running)
        .filter_map(|agent| {
            agent
                .session
                .as_ref()
                .map(|session| (agent.provider.clone(), session.clone()))
        })
        .collect::<HashSet<_>>();

    let mut actions = Vec::with_capacity(template.windows.len());
    let mut windows = Vec::with_capacity(template.windows.len());
    for (window_index, window) in template.windows.iter().enumerate() {
        let mut window_actions = Vec::with_capacity(window.panes.len());
        let mut panes = Vec::with_capacity(window.panes.len());
        for (pane_index, pane) in window.panes.iter().enumerate() {
            let exists = cwd_exists(&pane.cwd);
            if !exists {
                errors.push(format!(
                    "window {window_index} pane {pane_index} cwd is not a directory: {}",
                    pane.cwd
                ));
            } else if fs::canonicalize(&pane.cwd)
                .map(|resolved| resolved.to_string_lossy().contains('#'))
                .unwrap_or(true)
            {
                // Agent launches pass the resolved directory to tmux, which
                // expands formats in it; a symlink must not smuggle one in.
                errors.push(format!(
                    "window {window_index} pane {pane_index} cwd resolves to a path containing '#' or cannot be resolved: {}",
                    pane.cwd
                ));
            }
            let (kind, argv, planned_agent, action) = if let Some(agent) = &pane.agent {
                let action =
                    duplicate_action(agent, &mut used_agent_names, &mut used_agent_sessions);
                starts_agents |= matches!(action, AgentAction::Start | AgentAction::Resume);
                (
                    "agent",
                    None,
                    Some(PlanAgent {
                        provider: agent.provider.clone(),
                        name: agent.name.clone(),
                        session: agent.session.clone(),
                        action,
                        drafts: pane.queue.len(),
                    }),
                    Some(action),
                )
            } else if let Some(command) = &pane.command {
                ("command", Some(command.clone()), None, None)
            } else {
                ("shell", None, None, None)
            };
            panes.push(PlanPane {
                index: pane_index,
                kind,
                cwd: pane.cwd.clone(),
                cwd_exists: exists,
                argv,
                agent: planned_agent,
            });
            window_actions.push(action);
        }
        windows.push(PlanWindow {
            name: window.name.clone(),
            layout: window.layout.clone(),
            panes,
        });
        actions.push(window_actions);
    }
    if starts_agents && existing_panes + template_panes > 64 {
        errors.push(format!(
            "agents are managed on at most 64 panes per server; this server has {existing_panes} and the layout adds {template_panes}"
        ));
    }
    Ok(PreparedPlan {
        template,
        output: Plan {
            stage: "plan",
            template: name.to_owned(),
            session,
            session_exists,
            executes_commands,
            starts_agents,
            requires_confirmation: executes_commands || starts_agents,
            server_reachable,
            windows,
            errors,
        },
        manager,
        actions,
    })
}

fn duplicate_action(
    agent: &AgentFile,
    used_names: &mut HashSet<String>,
    used_sessions: &mut HashSet<(String, String)>,
) -> AgentAction {
    if agent
        .session
        .as_ref()
        .is_some_and(|session| used_sessions.contains(&(agent.provider.clone(), session.clone())))
    {
        AgentAction::SkipAlreadyRunning
    } else if used_names.contains(&agent.name) {
        AgentAction::SkipNameTaken
    } else {
        used_names.insert(agent.name.clone());
        if let Some(session) = &agent.session {
            used_sessions.insert((agent.provider.clone(), session.clone()));
            AgentAction::Resume
        } else {
            AgentAction::Start
        }
    }
}

async fn apply(
    name: &str,
    session_override: Option<&str>,
    socket: Option<PathBuf>,
    confirmed: bool,
) -> Result<Value, String> {
    let prepared = prepare_plan(
        load_template(name)?,
        name,
        session_override,
        socket,
        Duration::ZERO,
    )
    .await?;
    apply_prepared(prepared, name, confirmed).await
}

async fn apply_prepared(
    prepared: PreparedPlan,
    name: &str,
    confirmed: bool,
) -> Result<Value, String> {
    if !prepared.output.errors.is_empty() {
        return Err(format!(
            "layout cannot be applied: {}; review `masil-agent layout plan {name}`",
            prepared.output.errors.join("; ")
        ));
    }
    if prepared.output.requires_confirmation && !confirmed {
        return Err(format!(
            "layout runs commands or agents; review `masil-agent layout plan {name}` and rerun apply with --yes"
        ));
    }
    let manager = prepared.manager.ok_or("native server is not reachable")?;
    let session = prepared.output.session.clone();
    let mut results = Vec::new();
    let mut previous = Vec::<Option<String>>::with_capacity(prepared.template.windows.len());
    let mut window_ids = Vec::<Option<String>>::with_capacity(prepared.template.windows.len());
    let mut focused = None;
    let mut session_created = false;

    for (window_index, window) in prepared.template.windows.iter().enumerate() {
        let label = window_label(window, window_index);
        let first = &window.panes[0];
        let created = if window_index == 0 {
            create_first_window(&manager, &session, window, first).await
        } else if !session_created {
            Err("target session was not created".into())
        } else {
            create_window(&manager, &session, window, first).await
        };
        match created {
            Ok((pane_id, window_id)) => {
                session_created = true;
                window_ids.push(Some(window_id));
                if first.focus {
                    focused = Some((label.clone(), 0, pane_id.clone()));
                }
                results.push(success_result(
                    label.clone(),
                    0,
                    pane_kind(first),
                    "created",
                    Some(pane_id.clone()),
                    None,
                ));
                previous.push(Some(pane_id));
            }
            Err(error) => {
                results.push(failed_result(label.clone(), 0, pane_kind(first), error));
                previous.push(None);
                window_ids.push(None);
            }
        }

        for (pane_index, pane) in window.panes.iter().enumerate().skip(1) {
            if let Some(agent) = &pane.agent {
                let action = prepared.actions[window_index][pane_index]
                    .expect("agent panes have a planned action");
                if matches!(
                    action,
                    AgentAction::SkipAlreadyRunning | AgentAction::SkipNameTaken
                ) {
                    results.push(success_result(
                        label.clone(),
                        pane_index,
                        "agent",
                        match action {
                            AgentAction::SkipAlreadyRunning => "skipped_already_running",
                            AgentAction::SkipNameTaken => "skipped_name_taken",
                            _ => unreachable!(),
                        },
                        None,
                        None,
                    ));
                    continue;
                }
                let target = match split_target(
                    &manager,
                    &previous[window_index],
                    &window_ids[window_index],
                )
                .await
                {
                    Ok(target) => target,
                    Err(error) => {
                        results.push(failed_result(label.clone(), pane_index, "agent", error));
                        continue;
                    }
                };
                let target = target.as_str();
                match manager
                    .start(
                        &agent.name,
                        &agent.provider,
                        Path::new(&pane.cwd),
                        &[],
                        agent.session.as_deref(),
                        Some(target),
                    )
                    .await
                {
                    Ok(value) => {
                        let Some(pane_id) = value
                            .get("pane_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                        else {
                            results.push(failed_result(
                                label.clone(),
                                pane_index,
                                "agent",
                                "agent start result has no pane ID".into(),
                            ));
                            continue;
                        };
                        let run = value.get("run").and_then(Value::as_str).map(str::to_owned);
                        if pane.focus {
                            focused = Some((label.clone(), pane_index, pane_id.clone()));
                        }
                        previous[window_index] = Some(pane_id.clone());
                        let mut result = success_result(
                            label.clone(),
                            pane_index,
                            "agent",
                            "started",
                            Some(pane_id.clone()),
                            run.clone(),
                        );
                        if let Some(run) = run.filter(|_| !pane.queue.is_empty()) {
                            // The agent runs either way; a full queue is told.
                            match manager
                                .queue_drafts(&run, &pane_id, &agent.provider, &pane.queue)
                                .await
                            {
                                Ok(added) => result.queued = Some(added),
                                Err(error) => result.error = Some(error),
                            }
                        }
                        results.push(result);
                    }
                    Err(error) => {
                        results.push(failed_result(label.clone(), pane_index, "agent", error));
                    }
                }
            } else {
                let target = match split_target(
                    &manager,
                    &previous[window_index],
                    &window_ids[window_index],
                )
                .await
                {
                    Ok(target) => target,
                    Err(error) => {
                        results.push(failed_result(
                            label.clone(),
                            pane_index,
                            pane_kind(pane),
                            error,
                        ));
                        continue;
                    }
                };
                let target = target.as_str();
                match create_split(&manager, target, pane).await {
                    Ok(pane_id) => {
                        if pane.focus {
                            focused = Some((label.clone(), pane_index, pane_id.clone()));
                        }
                        previous[window_index] = Some(pane_id.clone());
                        results.push(success_result(
                            label.clone(),
                            pane_index,
                            pane_kind(pane),
                            "created",
                            Some(pane_id),
                            None,
                        ));
                    }
                    Err(error) => results.push(failed_result(
                        label.clone(),
                        pane_index,
                        pane_kind(pane),
                        error,
                    )),
                }
            }
        }
    }

    for (window_index, window) in prepared.template.windows.iter().enumerate() {
        let Some(layout) = window.layout.as_deref() else {
            continue;
        };
        let label = window_label(window, window_index);
        let Some(target) = window_ids[window_index].as_deref() else {
            results.push(failed_result(
                label,
                0,
                "layout",
                "window has no available pane".into(),
            ));
            continue;
        };
        if let Err(error) = manager
            .command(&["select-layout", "-t", target, layout])
            .await
        {
            results.push(failed_result(label, 0, "layout", error));
        }
    }

    if let Some((window, pane, target)) = focused
        && let Err(error) = focus_pane(&manager, &target).await
    {
        results.push(failed_result(window, pane, "focus", error));
    }

    let failures = results
        .iter()
        .filter(|result| result.outcome == "failed")
        .count();
    let successes = results
        .iter()
        .filter(|result| matches!(result.outcome, "created" | "started"))
        .count();
    let stage = if failures == 0 {
        "applied"
    } else if successes > 0 {
        "partial"
    } else {
        "failed"
    };
    Ok(json!({"stage":stage,"session":session,"results":results}))
}

fn window_label(window: &Window, index: usize) -> String {
    window.name.clone().unwrap_or_else(|| index.to_string())
}

fn pane_kind(pane: &Pane) -> &'static str {
    if pane.agent.is_some() {
        "agent"
    } else if pane.command.is_some() {
        "command"
    } else {
        "shell"
    }
}

fn success_result(
    window: String,
    pane: usize,
    kind: &'static str,
    outcome: &'static str,
    pane_id: Option<String>,
    run: Option<String>,
) -> ApplyResult {
    ApplyResult {
        window,
        pane,
        kind,
        outcome,
        pane_id,
        run,
        error: None,
        queued: None,
    }
}

fn failed_result(window: String, pane: usize, kind: &'static str, error: String) -> ApplyResult {
    ApplyResult {
        window,
        pane,
        kind,
        outcome: "failed",
        pane_id: None,
        run: None,
        error: Some(error),
        queued: None,
    }
}

async fn create_first_window(
    manager: &Manager,
    session: &str,
    window: &Window,
    pane: &Pane,
) -> Result<(String, String), String> {
    let mut args = vec![
        OsString::from("new-session"),
        OsString::from("-d"),
        OsString::from("-P"),
        OsString::from("-F"),
        OsString::from("#{pane_id}\t#{window_id}"),
        OsString::from("-s"),
        OsString::from(session),
    ];
    if let Some(name) = &window.name {
        args.extend([OsString::from("-n"), OsString::from(name)]);
    }
    args.extend([OsString::from("-c"), OsString::from(&pane.cwd)]);
    append_command(&mut args, pane.command.as_deref());
    created_window(manager, args).await
}

async fn create_window(
    manager: &Manager,
    session: &str,
    window: &Window,
    pane: &Pane,
) -> Result<(String, String), String> {
    let mut args = vec![
        OsString::from("new-window"),
        OsString::from("-d"),
        OsString::from("-P"),
        OsString::from("-F"),
        OsString::from("#{pane_id}\t#{window_id}"),
        OsString::from("-t"),
        OsString::from(format!("={session}:")),
    ];
    if let Some(name) = &window.name {
        args.extend([OsString::from("-n"), OsString::from(name)]);
    }
    args.extend([OsString::from("-c"), OsString::from(&pane.cwd)]);
    append_command(&mut args, pane.command.as_deref());
    created_window(manager, args).await
}

async fn create_split(manager: &Manager, target: &str, pane: &Pane) -> Result<String, String> {
    let mut args = vec![
        OsString::from("split-window"),
        OsString::from("-d"),
        OsString::from("-P"),
        OsString::from("-F"),
        OsString::from("#{pane_id}"),
        OsString::from("-t"),
        OsString::from(target),
        OsString::from("-c"),
        OsString::from(&pane.cwd),
    ];
    append_command(&mut args, pane.command.as_deref());
    created_pane(manager, args).await
}

fn append_command(args: &mut Vec<OsString>, command: Option<&[String]>) {
    let Some(command) = command else {
        return;
    };
    args.push(OsString::from("--"));
    if command.len() == 1 {
        args.extend([
            OsString::from(DIRECT_EXEC),
            OsString::from("--"),
            OsString::from(&command[0]),
        ]);
    } else {
        args.extend(command.iter().map(OsString::from));
    }
}

async fn created_pane(manager: &Manager, args: Vec<OsString>) -> Result<String, String> {
    let output = manager.native.tmux(args, None).await?;
    let pane = String::from_utf8(output.stdout).map_err(|_| "native pane ID is not UTF-8")?;
    let pane = pane.trim();
    if pane.len() < 2
        || !pane.starts_with('%')
        || !pane[1..].bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("native command returned an invalid pane ID".into());
    }
    Ok(pane.to_owned())
}

/// A new window's first pane and the window itself.
async fn created_window(
    manager: &Manager,
    args: Vec<OsString>,
) -> Result<(String, String), String> {
    let output = manager.native.tmux(args, None).await?;
    let text = String::from_utf8(output.stdout).map_err(|_| "native window ID is not UTF-8")?;
    let (pane, window) = text
        .trim()
        .split_once('\t')
        .ok_or("native command returned no pane and window ID")?;
    if !valid_id(pane, '%') || !valid_id(window, '@') {
        return Err("native command returned an invalid pane or window ID".into());
    }
    Ok((pane.to_owned(), window.to_owned()))
}

fn valid_id(value: &str, prefix: char) -> bool {
    value.len() > 1
        && value.starts_with(prefix)
        && value[1..].bytes().all(|byte| byte.is_ascii_digit())
}

/// The pane to split next: the previous one while it lives, else the
/// window's active pane. A command that already exited closed its pane;
/// once every pane exits, the window itself is gone.
async fn split_target(
    manager: &Manager,
    previous: &Option<String>,
    window: &Option<String>,
) -> Result<String, String> {
    let window = window.as_deref().ok_or("window was not created")?;
    let panes = manager
        .command(&[
            "list-panes",
            "-t",
            window,
            "-F",
            "#{pane_id}\t#{pane_active}",
        ])
        .await
        .map_err(|_| "window closed: every pane in it exited".to_string())?;
    let mut active = None;
    for line in panes.lines() {
        let (pane, flag) = line.split_once('\t').unwrap_or((line, "0"));
        if previous.as_deref() == Some(pane) {
            return Ok(pane.to_owned());
        }
        if flag == "1" {
            active = Some(pane.to_owned());
        }
    }
    active.ok_or_else(|| "window has no pane".into())
}

async fn focus_pane(manager: &Manager, pane: &str) -> Result<(), String> {
    manager.command(&["select-window", "-t", pane]).await?;
    manager.command(&["select-pane", "-t", pane]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Template, String> {
        let parsed: TemplateFile = toml::from_str(text).map_err(|error| error.to_string())?;
        normalize_from(
            parsed,
            Path::new("/work/current"),
            Some(Path::new("/home/test")),
        )
    }

    #[test]
    fn parses_and_normalizes_version_one() {
        let template = parse(
            r#"
version = 1
session = "api"
root = "~/code/api"

[[windows]]
name = "dev"
layout = "main-vertical"

[[windows.panes]]
cwd = "."
command = ["npm", "run", "dev"]
focus = true

[[windows.panes]]
cwd = "../review"
agent = { provider = "codex", name = "api-review" }
"#,
        )
        .unwrap();

        assert_eq!(template.root, "/home/test/code/api");
        assert_eq!(template.windows[0].panes[0].cwd, "/home/test/code/api");
        assert_eq!(template.windows[0].panes[1].cwd, "/home/test/code/review");
    }

    #[test]
    fn rejects_unknown_fields_and_invalid_version() {
        assert!(parse("version = 1\nsession = \"api\"\nextra = true\nwindows = []").is_err());
        assert!(parse("version = 2\nsession = \"api\"\nwindows = []").is_err());
    }

    #[test]
    fn rejects_agent_as_first_pane_and_command_agent_conflict() {
        let first_agent = r#"
version = 1
session = "api"
[[windows]]
[[windows.panes]]
agent = { provider = "codex", name = "review" }
"#;
        assert!(parse(first_agent).is_err());

        let conflict = r#"
version = 1
session = "api"
[[windows]]
[[windows.panes]]
command = ["cat"]
agent = { provider = "codex", name = "review" }
"#;
        assert!(parse(conflict).is_err());
    }

    #[test]
    fn validates_layout_session_and_agent_names() {
        for name in ["a", "api_2", "api-review"] {
            assert!(valid_layout_name(name).is_ok());
            assert!(valid_agent_name(name).is_ok());
        }
        for name in ["", "2api", "Api", "api.name", &"a".repeat(33)] {
            assert!(valid_layout_name(name).is_err(), "{name}");
            assert!(valid_agent_name(name).is_err(), "{name}");
        }
        assert!(valid_session_name("API.dev-2", 64, "session").is_ok());
        assert!(valid_session_name("api/dev", 64, "session").is_err());
        assert!(valid_session_name(&"a".repeat(65), 64, "session").is_err());
    }

    #[test]
    fn validates_argv_bounds_and_direct_single_argument() {
        assert!(validate_argv(&["cat".into()]).is_ok());
        assert!(validate_argv(&[]).is_err());
        assert!(validate_argv(&vec!["x".into(); 65]).is_err());
        assert!(validate_argv(&["x".repeat(8193)]).is_err());
        assert!(validate_argv(&["bad\narg".into()]).is_err());

        let mut args = Vec::new();
        append_command(&mut args, Some(&["/bin/cat".into()]));
        assert_eq!(
            args,
            ["--", DIRECT_EXEC, "--", "/bin/cat"]
                .map(OsString::from)
                .to_vec()
        );
    }

    #[test]
    fn rejects_tmux_format_expansion_in_paths() {
        let template = r#"
version = 1
session = "api"
root = "/tmp/#(touch${IFS}PWNED)"
[[windows]]
[[windows.panes]]
"#;
        assert!(parse(template).is_err());
    }

    #[test]
    fn enforces_window_pane_and_focus_bounds() {
        let too_many_panes = format!(
            "version = 1\nsession = \"api\"\n[[windows]]\n{}",
            "[[windows.panes]]\n".repeat(17)
        );
        assert!(parse(&too_many_panes).is_err());

        let multiple_focus = r#"
version = 1
session = "api"
[[windows]]
[[windows.panes]]
focus = true
[[windows.panes]]
focus = true
"#;
        assert!(parse(multiple_focus).is_err());
    }
}
