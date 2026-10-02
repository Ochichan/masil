//! `masil-agent session`: snapshots of the server's sessions, windows and pane
//! layouts, and their restore. A restored pane starts a shell in its saved
//! directory; allowed programs run again and coding agents with a native
//! session resume it.

use crate::managed::Manager;
use crate::native_ui::tmux_quote;
use crate::providers;
use crate::ui::settings::catalog::Scope;
use crate::ui::settings::server::Server;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use unicode_width::UnicodeWidthStr;

const VERSION: u32 = 1;
/// Snapshots kept of each kind; older ones are removed after a save.
const KEEP: usize = 20;
/// Snapshots listed in the restore menu.
const MENU_ITEMS: usize = 12;
const MENU_BORDER_ROWS: usize = 2;
const MENU_BASE_ROWS: usize = 2; // Save and its separator.
const MENU_PAGING_ROWS: usize = 3; // Separator plus newer and older links.
const MENU_BORDER_COLUMNS: usize = 4;
const MENU_HOTKEY_COLUMNS: usize = 4; // Space plus "(1)".
const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SESSIONS: usize = 256;
const MAX_PANES: usize = 4096;
const MAX_TEXT: usize = 4096;
const MAX_LAYOUT: usize = 512 * 1024;
const MAX_ARGS: usize = 256;
const AUTOSAVE_MINUTES: u64 = 15;
const AUTOSAVE_CHECK: Duration = Duration::from_secs(30);
/// Programs a restore starts again when no @masil-restore-commands is set.
const DEFAULT_COMMANDS: &[&str] = &[
    "vi", "vim", "nvim", "view", "emacs", "nano", "micro", "hx", "helix", "less", "more", "man",
    "tail", "top", "htop", "btop",
];
const SEP: char = '\u{1f}';

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    created_at_ms: u64,
    kind: String,
    sessions: Vec<SavedSession>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SavedSession {
    name: String,
    last_attached: u64,
    active_window: i64,
    windows: Vec<SavedWindow>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SavedWindow {
    index: i64,
    name: String,
    zoomed: bool,
    layout: String,
    panes: Vec<SavedPane>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SavedPane {
    index: i64,
    active: bool,
    cwd: String,
    title: String,
    /// The command a restore runs in the pane's shell, if any.
    #[serde(default)]
    command: Option<Vec<String>>,
    /// A native coding-agent session resume command. Unlike ordinary pane
    /// commands, this does not depend on the restore command allow-list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resume: Option<Vec<String>>,
    /// The saved pane's ID; a pane listed under several sessions runs its
    /// command once.
    #[serde(default)]
    origin: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Manual,
    Auto,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }
}

struct Options {
    socket: String,
    client: Option<String>,
    auto: bool,
    words: Vec<String>,
}

fn usage() -> String {
    "usage: masil-agent session [--socket MASIL_SOCKET] [--client CLIENT] \
     save [--auto]|list|preview [NAME]|restore [NAME]|menu [PAGE]|autosave"
        .into()
}

pub(crate) fn run(args: &[String]) -> Result<i32, String> {
    let options = parse(args)?;
    // The saver leaves the tmux job at once: a server waits for its jobs
    // before it exits.
    if options.words == ["autosave"] && !detach()? {
        return Ok(0);
    }
    let server = Server::locate(Some(&options.socket))?;
    // From a key or menu, results go to the client's status line; run-shell
    // would show any output in the pane.
    match (execute(&server, &options), options.client.as_deref()) {
        (Ok((code, report)), None) => {
            if let Some(report) = report {
                println!("{report}");
            }
            Ok(code)
        }
        (Ok(_), Some(_)) => Ok(0),
        (Err(error), Some(client)) => {
            // run-shell would print a failed exit into the pane.
            tell(&server, Some(client), &error);
            Ok(0)
        }
        (Err(error), None) => Err(error),
    }
}

/// Runs a command; returns the exit code and a report for standard output.
fn execute(server: &Server, options: &Options) -> Result<(i32, Option<String>), String> {
    let korean = server.global("@masil-lang", Scope::Session).as_deref() == Some("ko");
    let text = |en: &str, ko: &str| {
        if korean { ko.to_owned() } else { en.to_owned() }
    };
    let words: Vec<&str> = options.words.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["save"] => {
            // --auto saves as an automatic snapshot, only if something changed.
            let kind = if options.auto {
                Kind::Auto
            } else {
                Kind::Manual
            };
            let result = save(server, &options.socket, kind);
            let message = match &result {
                Ok(Some((_, snapshot))) => {
                    let (sessions, panes) = counts(snapshot);
                    text(
                        &format!(
                            "Saved {}, {}",
                            plural(sessions, "session"),
                            plural(panes, "pane")
                        ),
                        &format!("세션 {sessions}개, pane {panes}개를 저장했습니다"),
                    )
                }
                Ok(None) if options.auto => text(
                    "Sessions unchanged since the last snapshot",
                    "마지막 저장 뒤 바뀐 세션이 없습니다",
                ),
                Ok(None) => text("Nothing to save", "저장할 세션이 없습니다"),
                Err(error) => text(
                    &format!("Could not save sessions: {error}"),
                    &format!("세션을 저장하지 못했습니다: {error}"),
                ),
            };
            tell(server, options.client.as_deref(), &message);
            let name = result
                .as_ref()
                .ok()
                .and_then(Option::as_ref)
                .map(|(name, _)| name.clone());
            let stage = match &result {
                Ok(Some(_)) => "saved",
                Ok(None) => "unchanged",
                Err(_) => "failed",
            };
            Ok((
                i32::from(result.is_err()),
                Some(
                    serde_json::json!({"stage": stage, "snapshot": name, "message": message})
                        .to_string(),
                ),
            ))
        }
        ["list"] => {
            let dir = snapshot_dir()?;
            let list = list_snapshots(&dir)?
                .into_iter()
                .filter_map(|name| {
                    let snapshot = load(&dir.join(format!("{name}.json"))).ok()?;
                    let (sessions, panes) = counts(&snapshot);
                    Some(serde_json::json!({"name": name, "kind": snapshot.kind,
                        "created_at_ms": snapshot.created_at_ms, "sessions": sessions, "panes": panes}))
                })
                .collect::<Vec<_>>();
            Ok((0, Some(serde_json::Value::Array(list).to_string())))
        }
        ["preview"] | ["preview", _] => {
            let (name, snapshot) = selected_snapshot(&words, "preview", &text)?;
            let plan = plan_restore(server, &snapshot)?;
            let output = serde_json::json!({
                "stage": "preview",
                "snapshot": name,
                "sessions": plan.sessions,
                "counts": plan.counts,
            });
            Ok((0, Some(output.to_string())))
        }
        ["restore"] | ["restore", _] => {
            let (name, snapshot) = selected_snapshot(&words, "restore", &text)?;
            let plan = plan_restore(server, &snapshot)?;
            let report = execute_restore(server, &plan, options.client.as_deref())?;
            let message = if report.restored.is_empty()
                && report.skipped.is_empty()
                && report.errors.is_empty()
            {
                text("Nothing to restore", "불러올 세션이 없습니다")
            } else {
                let mut message = text(
                    &format!(
                        "Restored {}, {}, {}",
                        plural(report.restored.len(), "session"),
                        plural(report.panes, "pane"),
                        plural(report.commands, "command")
                    ),
                    &format!(
                        "세션 {}개, pane {}개, 명령 {}개를 불러왔습니다",
                        report.restored.len(),
                        report.panes,
                        report.commands
                    ),
                );
                if !report.skipped.is_empty() {
                    message.push_str(&text(
                        &format!("; already open: {}", report.skipped.join(", ")),
                        &format!(". 이미 열린 세션: {}", report.skipped.join(", ")),
                    ));
                }
                if !report.errors.is_empty() {
                    message.push_str(&text(
                        &format!("; failed: {}", report.errors.join("; ")),
                        &format!(". 실패: {}", report.errors.join("; ")),
                    ));
                }
                message
            };
            tell(server, options.client.as_deref(), &message);
            Ok((
                0,
                Some(
                    serde_json::json!({"stage": "restored", "snapshot": name,
                        "restored": report.restored, "skipped": report.skipped,
                        "errors": report.errors,
                        "panes": report.panes, "commands": report.commands,
                        "layout_fallbacks": report.layout_fallbacks, "message": message})
                    .to_string(),
                ),
            ))
        }
        ["menu"] | ["menu", _] => {
            let client = options.client.as_deref().ok_or("menu requires --client")?;
            let page = match words.get(1) {
                None => 0,
                Some(value) => value
                    .parse::<usize>()
                    .ok()
                    .and_then(|page| page.checked_sub(1))
                    .ok_or_else(|| {
                        text(
                            "Invalid restore menu page",
                            "잘못된 불러오기 메뉴 페이지입니다",
                        )
                    })?,
            };
            menu(server, &options.socket, client, korean, page)?;
            Ok((0, None))
        }
        ["autosave"] => autosave(server, &options.socket).map(|code| (code, None)),
        _ => Err(usage()),
    }
}

fn selected_snapshot<F>(
    words: &[&str],
    command: &str,
    text: &F,
) -> Result<(String, Snapshot), String>
where
    F: Fn(&str, &str) -> String,
{
    debug_assert_eq!(words.first().copied(), Some(command));
    let dir = snapshot_dir()?;
    let name = match words.get(1) {
        Some(name) => valid_name(name)?.to_owned(),
        None => list_snapshots(&dir)?
            .into_iter()
            .next()
            .ok_or_else(|| text("No saved sessions", "저장된 세션이 없습니다"))?,
    };
    let snapshot = load(&dir.join(format!("{name}.json")))?;
    Ok((name, snapshot))
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut socket = None;
    let mut client = None;
    let mut auto = false;
    let mut words = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = Some(args.next().ok_or_else(usage)?.clone()),
            "--client" => client = Some(args.next().ok_or_else(usage)?.clone()),
            "--auto" => auto = true,
            _ => words.push(arg.clone()),
        }
    }
    let socket = match socket {
        Some(socket) => socket,
        None => std::env::var("TMUX")
            .ok()
            .and_then(|value| value.split(',').next().map(str::to_owned))
            .filter(|path| !path.is_empty())
            .ok_or("run this inside masil or pass --socket PATH")?,
    };
    if let Some(client) = &client
        && (client.is_empty() || client.len() > MAX_TEXT || client.chars().any(char::is_control))
    {
        return Err("invalid client name".into());
    }
    Ok(Options {
        socket,
        client,
        auto,
        words,
    })
}

/// Runs a masil command. A value ending in ';' would end the command, so
/// that ';' is escaped.
fn masil(server: &Server, args: &[&str]) -> Result<String, String> {
    let args: Vec<String> = args
        .iter()
        .map(|arg| match arg.strip_suffix(';') {
            Some(rest) => format!("{rest}\\;"),
            None => (*arg).to_owned(),
        })
        .collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    server.run(&refs)
}

/// Shows a message on the client that asked, or prints it.
fn tell(server: &Server, client: Option<&str>, message: &str) {
    match client {
        Some(client) => {
            let _ = masil(server, &["display-message", "-c", client, "-l", message]);
        }
        None => eprintln!("{message}"),
    }
}

/// "1 pane" or "2 panes".
fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("1 {word}")
    } else {
        format!("{count} {word}s")
    }
}

fn counts(snapshot: &Snapshot) -> (usize, usize) {
    let panes = snapshot
        .sessions
        .iter()
        .flat_map(|session| &session.windows)
        .map(|window| window.panes.len())
        .sum();
    (snapshot.sessions.len(), panes)
}

// ---------------------------------------------------------------- capture

fn fields(line: &str, count: usize) -> Option<Vec<&str>> {
    let fields: Vec<&str> = line.split(SEP).collect();
    (fields.len() == count).then_some(fields)
}

/// The sessions, windows and panes of the server, with the commands a restore
/// would run again.
fn capture(server: &Server, socket: &str) -> Result<Vec<SavedSession>, String> {
    let format = |parts: &[&str]| parts.join(&SEP.to_string());
    let sessions = masil(
        server,
        &[
            "list-sessions",
            "-F",
            &format(&[
                "#{session_name}",
                "#{session_last_attached}",
                "#{window_index}",
            ]),
        ],
    )?;
    let windows = masil(
        server,
        &[
            "list-windows",
            "-a",
            "-F",
            &format(&[
                "#{session_name}",
                "#{window_index}",
                "#{window_name}",
                "#{window_zoomed_flag}",
                "#{window_layout}",
            ]),
        ],
    )?;
    let panes = masil(
        server,
        &[
            "list-panes",
            "-a",
            "-F",
            &format(&[
                "#{session_name}",
                "#{window_index}",
                "#{pane_index}",
                "#{pane_id}",
                "#{pane_active}",
                "#{pane_current_path}",
                "#{pane_pid}",
                "#{masil_foreground_pgid}",
                "#{pane_title}",
                "#{host}",
            ]),
        ],
    )?;

    let allowed = allowed_commands(server);
    let agents = resumable_agents(socket);
    let mut saved = Vec::new();
    for line in sessions.lines() {
        let Some(f) = fields(line, 3) else { continue };
        if f[0].is_empty() || plain(f[0]).is_none() {
            continue;
        }
        saved.push(SavedSession {
            name: f[0].to_owned(),
            last_attached: f[1].parse().unwrap_or(0),
            active_window: f[2].parse().unwrap_or(0),
            windows: Vec::new(),
        });
    }
    for line in windows.lines() {
        let Some(f) = fields(line, 5) else { continue };
        let Some(session) = saved.iter_mut().find(|s| s.name == f[0]) else {
            continue;
        };
        session.windows.push(SavedWindow {
            index: f[1].parse().map_err(|_| "invalid window index")?,
            name: clean(f[2]),
            zoomed: f[3] == "1",
            layout: if f[4].len() <= MAX_LAYOUT && !f[4].chars().any(char::is_control) {
                f[4].to_owned()
            } else {
                String::new()
            },
            panes: Vec::new(),
        });
    }
    for line in panes.lines() {
        let Some(f) = fields(line, 10) else { continue };
        let Some(window) = saved
            .iter_mut()
            .find(|s| s.name == f[0])
            .and_then(|s| s.windows.iter_mut().find(|w| w.index.to_string() == f[1]))
        else {
            continue;
        };
        let pid: i32 = f[6].parse().unwrap_or(0);
        let group: i32 = f[7].parse().unwrap_or(0);
        // A command whose arguments hold control characters is never typed
        // back: they would edit the shell's line.
        // As before the split, an argv with control characters is dropped
        // here rather than failing the whole snapshot at validation.
        let resume = agents.get(f[3]).cloned().filter(|argv| safe_argv(argv));
        let command = if resume.is_none() {
            (group > 0 && group != pid)
                .then(|| crate::process::argv(group))
                .flatten()
                .filter(|argv| is_allowed(argv, &allowed))
                .filter(|argv| safe_argv(argv))
        } else {
            None
        };
        window.panes.push(SavedPane {
            index: f[2].parse().map_err(|_| "invalid pane index")?,
            active: f[4] == "1",
            cwd: plain(f[5]).unwrap_or_default(),
            title: if f[8] == f[9] {
                String::new()
            } else {
                plain(f[8]).unwrap_or_default()
            },
            command,
            resume,
            origin: f[3].to_owned(),
        });
    }
    for session in &mut saved {
        session.windows.retain(|window| !window.panes.is_empty());
        for window in &mut session.windows {
            window.panes.sort_by_key(|pane| pane.index);
        }
        session.windows.sort_by_key(|window| window.index);
    }
    saved.retain(|session| !session.windows.is_empty());
    Ok(saved)
}

/// Text without control characters and within the size limit.
fn plain(text: &str) -> Option<String> {
    (text.len() <= MAX_TEXT && !text.chars().any(char::is_control)).then(|| text.to_owned())
}

/// Text with control characters replaced, cut to the size limit.
fn clean(text: &str) -> String {
    let mut output = String::new();
    for character in text.chars() {
        if output.len() + character.len_utf8() > MAX_TEXT {
            break;
        }
        output.push(if character.is_control() {
            '_'
        } else {
            character
        });
    }
    output
}

fn safe_argv(argv: &[String]) -> bool {
    !argv.is_empty() && argv.len() <= MAX_ARGS && argv.iter().all(|arg| plain(arg).is_some())
}

fn allowed_commands(server: &Server) -> HashSet<String> {
    match server.global("@masil-restore-commands", Scope::Session) {
        Some(value) => value.split_whitespace().map(str::to_owned).collect(),
        None => DEFAULT_COMMANDS.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn program(argv: &[String]) -> &str {
    let first = argv.first().map(String::as_str).unwrap_or_default();
    first
        .rsplit('/')
        .next()
        .unwrap_or(first)
        .trim_start_matches('-')
}

fn is_allowed(argv: &[String], allowed: &HashSet<String>) -> bool {
    !argv.is_empty() && allowed.contains(program(argv))
}

/// Coding agents that report a native session, by pane ID, as the argv that
/// resumes that session.
fn resumable_agents(socket: &str) -> HashMap<String, Vec<String>> {
    let mut map = HashMap::new();
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return map;
    };
    let Ok(manager) = Manager::new(PathBuf::from(socket), None) else {
        return map;
    };
    let Ok(agents) = runtime.block_on(manager.list()) else {
        return map;
    };
    for agent in agents {
        if agent.process != "running" {
            continue;
        }
        if let Some(session) = &agent.session_id
            && let Ok(argv) = providers::resume(&agent.provider, session)
        {
            map.insert(agent.pane_id.clone(), argv);
        }
    }
    map
}

// ---------------------------------------------------------------- storage

fn snapshot_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/share"))
        })
        .ok_or("no home directory")?;
    let dir = base.join("masil/sessions");
    fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let metadata = fs::symlink_metadata(&dir).map_err(|error| error.to_string())?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(format!("{} is not a directory you own", dir.display()));
    }
    if metadata.mode() & 0o077 != 0 {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    Ok(dir)
}

fn valid_name(name: &str) -> Result<&str, String> {
    let ok = name.len() <= 64
        && name.ends_with("-manual") | name.ends_with("-auto")
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-');
    if ok {
        Ok(name)
    } else {
        Err("invalid snapshot name".into())
    }
}

/// Snapshot names, newest first.
fn list_snapshots(dir: &Path) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let file = entry.file_name();
        let Some(file) = file.to_str() else { continue };
        let Some(name) = file.strip_suffix(".json") else {
            continue;
        };
        if valid_name(name).is_ok() {
            names.push(name.to_owned());
        }
    }
    names.sort_by(|a, b| order(a).cmp(&order(b)));
    names.reverse();
    Ok(names)
}

/// Sort key for YYYYMMDD-HHMMSS[-N]-KIND, so -10- follows -9- and a plain
/// name comes before its -2- sibling.
fn order(name: &str) -> (&str, u32, &str) {
    let stamp = name.get(..15).unwrap_or(name);
    let rest = name.get(16..).unwrap_or("");
    match rest.split_once('-') {
        Some((n, kind)) if n.bytes().all(|b| b.is_ascii_digit()) => {
            (stamp, n.parse().unwrap_or(u32::MAX), kind)
        }
        _ => (stamp, 1, rest),
    }
}

fn load(path: &Path) -> Result<Snapshot, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(format!("{} is not a snapshot", path.display()));
    }
    let mut text = String::new();
    file.take(MAX_BYTES)
        .read_to_string(&mut text)
        .map_err(|error| error.to_string())?;
    let snapshot: Snapshot = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    validate(&snapshot)?;
    Ok(snapshot)
}

fn validate(snapshot: &Snapshot) -> Result<(), String> {
    let bad_text = |text: &str| text.len() > MAX_TEXT || text.chars().any(char::is_control);
    if snapshot.version != VERSION {
        return Err(format!("unsupported snapshot version {}", snapshot.version));
    }
    if snapshot.sessions.len() > MAX_SESSIONS {
        return Err("too many sessions".into());
    }
    let mut panes = 0;
    for session in &snapshot.sessions {
        if session.name.is_empty() || bad_text(&session.name) || session.name.contains([':', '.']) {
            return Err("invalid session name".into());
        }
        for window in &session.windows {
            if window.index < 0
                || bad_text(&window.name)
                || window.layout.len() > MAX_LAYOUT
                || window.layout.chars().any(char::is_control)
                || window.panes.is_empty()
            {
                return Err(format!("invalid window in session {}", session.name));
            }
            for pane in &window.panes {
                panes += 1;
                if bad_text(&pane.cwd) || bad_text(&pane.title) {
                    return Err(format!("invalid pane in session {}", session.name));
                }
                if let Some(argv) = &pane.command
                    && !safe_argv(argv)
                {
                    return Err(format!("invalid command in session {}", session.name));
                }
                if let Some(argv) = &pane.resume
                    && !safe_argv(argv)
                {
                    return Err(format!("invalid resume in session {}", session.name));
                }
                if pane.command.is_some() && pane.resume.is_some() {
                    return Err(format!(
                        "pane has both command and resume in session {}",
                        session.name
                    ));
                }
            }
        }
    }
    if panes > MAX_PANES {
        return Err("too many panes".into());
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// UTC time as YYYYMMDD-HHMMSS.
fn stamp(ms: u64) -> String {
    let seconds = ms / 1000;
    let (days, day) = (seconds / 86400, seconds % 86400);
    let (year, month, date) = civil(days as i64);
    format!(
        "{year:04}{month:02}{date:02}-{:02}{:02}{:02}",
        day / 3600,
        day % 3600 / 60,
        day % 60
    )
}

/// Days since 1970-01-01 as a proleptic Gregorian date.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let date = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, date)
}

/// Local time for menus, as MM-DD HH:MM.
fn local_time(ms: u64) -> String {
    let seconds = (ms / 1000) as libc::time_t;
    // SAFETY: tm is plain data and localtime_r writes it fully on success.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call.
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return stamp(ms);
    }
    format!(
        "{:02}-{:02} {:02}:{:02}",
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("snapshot path has no directory")?;
    let temporary = dir.join(format!(".snapshot.{}.tmp", std::process::id()));
    let result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        // A link fails instead of replacing a snapshot saved at the same time.
        fs::hard_link(&temporary, path).map_err(|error| match error.kind() {
            ErrorKind::AlreadyExists => EXISTS.to_owned(),
            _ => error.to_string(),
        })
    })();
    let _ = fs::remove_file(&temporary);
    result
}

const EXISTS: &str = "snapshot exists";

/// Saves a snapshot. An automatic save is skipped when nothing changed since
/// the latest snapshot. Returns the snapshot name, or None if not saved.
fn save(server: &Server, socket: &str, kind: Kind) -> Result<Option<(String, Snapshot)>, String> {
    let sessions = capture(server, socket)?;
    if sessions.is_empty() {
        return Ok(None);
    }
    let dir = snapshot_dir()?;
    let names = list_snapshots(&dir)?;
    if kind == Kind::Auto
        && let Some(latest) = names.first()
        && let Ok(previous) = load(&dir.join(format!("{latest}.json")))
        && previous.sessions == sessions
    {
        return Ok(None);
    }
    let created_at_ms = now_ms();
    let snapshot = Snapshot {
        version: VERSION,
        created_at_ms,
        kind: kind.as_str().into(),
        sessions,
    };
    validate(&snapshot)?;
    let base = stamp(created_at_ms);
    let bytes = serde_json::to_vec(&snapshot).map_err(|error| error.to_string())?;
    let mut name = format!("{base}-{}", kind.as_str());
    let mut n = 2;
    loop {
        match write_private(&dir.join(format!("{name}.json")), &bytes) {
            Err(error) if error == EXISTS && n < 100 => {
                name = format!("{base}-{n}-{}", kind.as_str());
                n += 1;
            }
            result => break result?,
        }
    }
    prune(&dir, kind)?;
    Ok(Some((name, snapshot)))
}

fn prune(dir: &Path, kind: Kind) -> Result<(), String> {
    let suffix = format!("-{}", kind.as_str());
    let names: Vec<_> = list_snapshots(dir)?
        .into_iter()
        .filter(|name| name.ends_with(&suffix))
        .collect();
    for name in names.iter().skip(KEEP) {
        let _ = fs::remove_file(dir.join(format!("{name}.json")));
    }
    Ok(())
}

// ---------------------------------------------------------------- restore

#[derive(Clone, Debug, Serialize)]
struct RestoreCounts {
    sessions: usize,
    panes: usize,
    commands: usize,
}

#[derive(Clone, Debug, Serialize)]
struct RestorePlan {
    sessions: Vec<RestoreSessionPlan>,
    counts: RestoreCounts,
}

#[derive(Clone, Debug, Serialize)]
struct RestoreSessionPlan {
    name: String,
    action: String,
    windows: Vec<RestoreWindowPlan>,
    #[serde(skip)]
    target_name: String,
    #[serde(skip)]
    active_window: i64,
    #[serde(skip)]
    last_attached: u64,
}

#[derive(Clone, Debug, Serialize)]
struct RestoreWindowPlan {
    index: i64,
    name: String,
    layout_fallback: bool,
    panes: Vec<RestorePanePlan>,
    #[serde(skip)]
    layout: String,
    #[serde(skip)]
    zoomed: bool,
    #[serde(skip)]
    width: u64,
    #[serde(skip)]
    height: u64,
}

#[derive(Clone, Debug, Serialize)]
struct RestorePanePlan {
    cwd: String,
    cwd_exists: bool,
    command: Option<Vec<String>>,
    command_allowed: bool,
    resume: Option<Vec<String>>,
    #[serde(skip)]
    effective_cwd: String,
    #[serde(skip)]
    title: String,
    #[serde(skip)]
    active: bool,
    #[serde(skip)]
    origin: String,
}

#[derive(Default)]
struct Report {
    restored: Vec<String>,
    skipped: Vec<String>,
    errors: Vec<String>,
    panes: usize,
    commands: usize,
    layout_fallbacks: usize,
}

/// Seconds a session may have existed and still count as just started.
const FRESH_SECONDS: u64 = 600;

/// Programs that count as an idle shell besides the default-shell.
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "nu", "elvish", "xonsh",
    "pwsh", "ash", "yash",
];

/// A session that looks just started: one window and one pane with the
/// default shell at its prompt, no scrollback, the cursor near the top and
/// created in the last few minutes. Only such a session is replaced.
fn pristine(server: &Server, name: &str) -> bool {
    let Ok(output) = masil(
        server,
        &[
            "list-panes",
            "-s",
            "-t",
            &format!("={name}"),
            "-F",
            "#{pane_pid} #{masil_foreground_pgid} #{session_windows} #{history_size} \
             #{cursor_y} #{session_created} #{pane_current_command} #{b:default-shell} \
             #{pane_start_command}",
        ],
    ) else {
        return false;
    };
    let lines: Vec<_> = output.lines().collect();
    let [line] = lines.as_slice() else {
        return false;
    };
    let parts: Vec<_> = line.split(' ').collect();
    let created = parts.get(5).and_then(|value| value.parse::<u64>().ok());
    let now = now_ms() / 1000;
    parts.len() == 9
        && parts[0] == parts[1]
        && parts[2] == "1"
        && parts[3] == "0"
        && parts[4].parse::<u32>().is_ok_and(|y| y <= 3)
        && created.is_some_and(|created| now.saturating_sub(created) <= FRESH_SECONDS)
        && (parts[6] == parts[7] || SHELLS.contains(&parts[6]))
        && parts[8].is_empty()
        // A stopped editor or a background job is a child of the shell.
        && parts[0].parse().is_ok_and(|pid| !has_children(pid))
}

/// Whether a process has children; true when that cannot be told.
#[cfg(target_os = "macos")]
fn has_children(pid: i32) -> bool {
    let mut pids = [0 as libc::pid_t; 16];
    let size = std::mem::size_of_val(&pids) as libc::c_int;
    // SAFETY: the buffer is valid for size bytes.
    let count = unsafe { libc::proc_listchildpids(pid, pids.as_mut_ptr().cast(), size) };
    count != 0
}

#[cfg(not(target_os = "macos"))]
fn has_children(pid: i32) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return true;
    };
    let pid = pid.to_string();
    entries.flatten().any(|entry| {
        fs::read_to_string(entry.path().join("stat"))
            .ok()
            .and_then(|stat| {
                // pid (comm) state ppid ...
                let (_, rest) = stat.rsplit_once(')')?;
                Some(rest.split_whitespace().nth(1)? == pid)
            })
            .unwrap_or(false)
    })
}

fn session_in_use(server: &Server, name: &str) -> bool {
    masil(
        server,
        &[
            "display-message",
            "-p",
            "-t",
            &format!("={name}:"),
            "#{session_attached}",
        ],
    )
    .is_ok_and(|value| value.trim().parse::<u64>().is_ok_and(|count| count > 0))
}

fn plan_restore(server: &Server, snapshot: &Snapshot) -> Result<RestorePlan, String> {
    let existing: HashSet<String> = masil(server, &["list-sessions", "-F", "#{session_name}"])
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    let mut started = HashSet::new();
    let mut sessions = Vec::with_capacity(snapshot.sessions.len());
    let mut counts = RestoreCounts {
        sessions: 0,
        panes: 0,
        commands: 0,
    };
    for session in &snapshot.sessions {
        let mut target_name = session.name.clone();
        let action = if existing.contains(&target_name) {
            if !pristine(server, &target_name) {
                started.extend(
                    session
                        .windows
                        .iter()
                        .flat_map(|window| &window.panes)
                        .filter(|pane| !pane.origin.is_empty())
                        .map(|pane| pane.origin.clone()),
                );
                if session_in_use(server, &target_name) {
                    "skip_in_use"
                } else {
                    "skip_open"
                }
            } else {
                target_name = format!("{target_name}-restored");
                let mut n = 2;
                while existing.contains(&target_name) {
                    target_name = format!("{}-restored{n}", session.name);
                    n += 1;
                }
                "create"
            }
        } else {
            "create"
        };
        let creating = action == "create";
        let mut windows = Vec::with_capacity(session.windows.len());
        for window in &session.windows {
            let (width, height) = layout_size(&window.layout).unwrap_or((80, 24));
            let layout_fallback = layout_size(&window.layout).is_none();
            let mut panes = Vec::with_capacity(window.panes.len());
            for pane in &window.panes {
                let cwd_exists = Path::new(&pane.cwd).is_dir();
                // Snapshots contain only commands accepted by the allow-list
                // when they were captured. Preserve that restore decision.
                let command_allowed = pane.command.is_some();
                let has_program = pane.resume.is_some() || command_allowed;
                let unique_origin = !creating
                    || !has_program
                    || pane.origin.is_empty()
                    || started.insert(pane.origin.clone());
                let will_run = creating && has_program && unique_origin;
                if creating {
                    counts.panes += 1;
                    counts.commands += usize::from(will_run);
                }
                panes.push(RestorePanePlan {
                    cwd: pane.cwd.clone(),
                    cwd_exists,
                    command: pane.command.clone(),
                    command_allowed,
                    resume: pane.resume.clone(),
                    effective_cwd: directory(&pane.cwd),
                    title: pane.title.clone(),
                    active: pane.active,
                    origin: pane.origin.clone(),
                });
            }
            windows.push(RestoreWindowPlan {
                index: window.index,
                name: window.name.clone(),
                layout_fallback,
                panes,
                layout: window.layout.clone(),
                zoomed: window.zoomed,
                width,
                height,
            });
        }
        if creating {
            counts.sessions += 1;
        }
        sessions.push(RestoreSessionPlan {
            name: session.name.clone(),
            action: action.into(),
            windows,
            target_name,
            active_window: session.active_window,
            last_attached: session.last_attached,
        });
    }
    Ok(RestorePlan { sessions, counts })
}

fn execute_restore(
    server: &Server,
    plan: &RestorePlan,
    client: Option<&str>,
) -> Result<Report, String> {
    let mut report = Report::default();
    let mut started = HashSet::new();
    let preferred = plan
        .sessions
        .iter()
        .max_by_key(|session| session.last_attached)
        .map(|session| session.name.clone());
    for session in &plan.sessions {
        if session.action != "create" {
            started.extend(
                session
                    .windows
                    .iter()
                    .flat_map(|window| &window.panes)
                    .filter(|pane| !pane.origin.is_empty())
                    .map(|pane| pane.origin.clone()),
            );
            report.skipped.push(session.name.clone());
            continue;
        }
        if let Err(error) = restore_session(server, session, &mut started, &mut report) {
            report.errors.push(format!("{}: {error}", session.name));
            continue;
        }
        report.restored.push(session.name.clone());
        if let Some(client) = client
            && preferred.as_deref() == Some(session.name.as_str())
        {
            let _ = masil(
                server,
                &[
                    "switch-client",
                    "-c",
                    client,
                    "-t",
                    &format!("={}", session.target_name),
                ],
            );
        }
        if session.target_name != session.name {
            // Replace the just-started session if it is still untouched and
            // no client uses it; otherwise both stay.
            let old = format!("={}", session.name);
            let attached = masil(
                server,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    &format!("{old}:"),
                    "#{session_attached}",
                ],
            )
            .unwrap_or_default();
            if attached.trim() == "0" && pristine(server, &session.name) {
                let _ = masil(server, &["kill-session", "-t", &old]);
                let _ = masil(
                    server,
                    &[
                        "rename-session",
                        "-t",
                        &format!("={}", session.target_name),
                        "--",
                        &literal(&session.name),
                    ],
                );
            }
        }
    }
    Ok(report)
}

/// Text for an argument the core expands as a format (-s, -n, -c, -T and
/// rename-session), so a saved '#' stays a '#'.
fn literal(text: &str) -> String {
    text.replace('#', "##")
}

fn directory(cwd: &str) -> String {
    if Path::new(cwd).is_dir() {
        cwd.to_owned()
    } else {
        std::env::var("HOME").unwrap_or_else(|_| "/".into())
    }
}

fn layout_size(layout: &str) -> Option<(u64, u64)> {
    let value: serde_json::Value = serde_json::from_str(layout).ok()?;
    let root = value.get("L")?;
    Some((root.get("w")?.as_u64()?, root.get("h")?.as_u64()?))
}

fn restore_session(
    server: &Server,
    session: &RestoreSessionPlan,
    started: &mut HashSet<String>,
    report: &mut Report,
) -> Result<(), String> {
    let name = &session.target_name;
    let target = |window: &RestoreWindowPlan| format!("={name}:{}", window.index);
    for (number, window) in session.windows.iter().enumerate() {
        let cwd = &window.panes[0].effective_cwd;
        if number == 0 {
            masil(
                server,
                &[
                    "new-session",
                    "-d",
                    "-s",
                    &literal(name),
                    "-n",
                    &literal(&window.name),
                    "-c",
                    &literal(cwd),
                    "-x",
                    &window.width.max(10).to_string(),
                    "-y",
                    &window.height.max(5).to_string(),
                ],
            )?;
            let first = masil(
                server,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    &format!("={name}:"),
                    "#{window_index}",
                ],
            )?;
            if first.trim() != window.index.to_string() {
                masil(
                    server,
                    &[
                        "move-window",
                        "-s",
                        &format!("={name}:{}", first.trim()),
                        "-t",
                        &target(window),
                    ],
                )?;
            }
        } else {
            masil(
                server,
                &[
                    "new-window",
                    "-d",
                    "-t",
                    &target(window),
                    "-n",
                    &literal(&window.name),
                    "-c",
                    &literal(cwd),
                ],
            )?;
        }
        // Extra panes start floating; the saved layout places every pane.
        for pane in &window.panes[1..] {
            masil(
                server,
                &[
                    "new-pane",
                    "-d",
                    "-t",
                    &target(window),
                    "-x",
                    "8",
                    "-y",
                    "4",
                    "-c",
                    &literal(&pane.effective_cwd),
                ],
            )?;
        }
        if window.layout.is_empty()
            || masil(
                server,
                &["select-layout", "-t", &target(window), &window.layout],
            )
            .is_err()
        {
            // Without the saved layout the extra panes are tiled.
            report.layout_fallbacks += 1;
            let floating = masil(
                server,
                &[
                    "list-panes",
                    "-t",
                    &target(window),
                    "-F",
                    "#{pane_id} #{pane_floating_flag}",
                ],
            )
            .unwrap_or_default();
            for line in floating.lines() {
                if let Some(id) = line.strip_suffix(" 1") {
                    let _ = masil(server, &["join-pane", "-d", "-s", id, "-t", id]);
                }
            }
            let _ = masil(server, &["select-layout", "-t", &target(window), "tiled"]);
        }
        let ids: Vec<String> = masil(
            server,
            &["list-panes", "-t", &target(window), "-F", "#{pane_id}"],
        )?
        .lines()
        .map(str::to_owned)
        .collect();
        let mut active = None;
        for (pane, id) in window.panes.iter().zip(&ids) {
            report.panes += 1;
            if !pane.title.is_empty() {
                let _ = masil(
                    server,
                    &["select-pane", "-t", id, "-T", &literal(&pane.title)],
                );
            }
            if let Some(argv) = pane
                .resume
                .as_ref()
                .or_else(|| {
                    pane.command_allowed
                        .then_some(pane.command.as_ref())
                        .flatten()
                })
                .filter(|_| pane.origin.is_empty() || started.insert(pane.origin.clone()))
            {
                let line = shell_line(argv);
                if masil(server, &["send-keys", "-t", id, "-l", "--", &line]).is_ok()
                    && masil(server, &["send-keys", "-t", id, "Enter"]).is_ok()
                {
                    report.commands += 1;
                }
            }
            if pane.active {
                active = Some(id.clone());
            }
        }
        if let Some(id) = active {
            let _ = masil(server, &["select-pane", "-t", &id]);
            if window.zoomed {
                let _ = masil(server, &["resize-pane", "-Z", "-t", &id]);
            }
        }
    }
    let _ = masil(
        server,
        &[
            "select-window",
            "-t",
            &format!("={name}:{}", session.active_window),
        ],
    );
    Ok(())
}

fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:@%+,-".contains(&b));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

fn shell_line(argv: &[String]) -> String {
    argv.iter()
        .map(|word| shell_word(word))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------- menu

/// A tmux command that runs this program with arguments, for a menu item.
fn run_self(socket: &str, client: &str, args: &[&str]) -> Result<String, String> {
    let program = std::env::current_exe().map_err(|error| error.to_string())?;
    let program = program.to_str().ok_or("program path is not UTF-8")?;
    let mut words = vec![shell_word(program), "session".into()];
    for arg in ["--socket", socket, "--client", client]
        .iter()
        .chain(args.iter())
    {
        words.push(shell_word(arg));
    }
    // The menu and then run-shell expand formats, so a literal # is
    // doubled twice.
    Ok(format!(
        "run-shell -b {}",
        tmux_quote(&words.join(" ").replace('#', "####"))
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MenuGeometry {
    width: usize,
    height: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MenuPage {
    start: usize,
    end: usize,
    newer: bool,
    older: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MenuPageError {
    TooShort,
    OutOfRange,
}

struct MenuSnapshot {
    name: String,
    created_at_ms: u64,
    kind: String,
    sessions: usize,
    panes: usize,
}

/// Reads both sides of the actual drawing area. A shared window can be larger
/// than one client, while a status column can make its window smaller.
fn parse_menu_geometry(value: &str) -> Option<MenuGeometry> {
    let fields = fields(value.trim_end_matches('\n'), 4)?;
    let dimensions = fields
        .iter()
        .map(|field| field.parse::<usize>().ok().filter(|value| *value > 0))
        .collect::<Option<Vec<_>>>()?;
    Some(MenuGeometry {
        width: dimensions[0].min(dimensions[2]),
        height: dimensions[1].min(dimensions[3]),
    })
}

fn parse_client_target(listing: &str, client: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let fields = fields(line, 3)?;
        let session = fields[1].strip_prefix('$')?;
        let pane = fields[2].strip_prefix('%')?;
        (fields[0] == client
            && !session.is_empty()
            && session.bytes().all(|byte| byte.is_ascii_digit())
            && !pane.is_empty()
            && pane.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| format!("{}:.{}", fields[1], fields[2]))
    })
}

/// Resolves the pane from the requested client itself. The agent may have
/// inherited TMUX_PANE from another session, so command target defaults are
/// not reliable here.
fn client_target(server: &Server, client: &str) -> Result<String, String> {
    // The session qualifier also disambiguates windows linked to other sessions.
    let format = ["#{client_name}", "#{session_id}", "#{pane_id}"].join(&SEP.to_string());
    let listing = masil(server, &["list-clients", "-F", &format])?;
    parse_client_target(&listing, client).ok_or_else(|| "client target unavailable".into())
}

fn menu_geometry(server: &Server, client: &str, target: &str) -> Result<MenuGeometry, String> {
    let separator = SEP.to_string();
    let format = [
        "#{masil_viewport_width}",
        "#{masil_viewport_height}",
        "#{window_width}",
        "#{window_height}",
    ]
    .join(&separator);
    let value = masil(
        server,
        &["display-message", "-c", client, "-t", target, "-p", &format],
    )?;
    parse_menu_geometry(&value).ok_or_else(|| "invalid client geometry".into())
}

/// Chooses a stable page size. Middle pages reserve both navigation rows, so
/// moving through the list never creates a menu taller than the window.
fn menu_page(total: usize, height: usize, page: usize) -> Result<MenuPage, MenuPageError> {
    let no_page_rows = height.saturating_sub(MENU_BORDER_ROWS + MENU_BASE_ROWS);
    if total <= no_page_rows.min(MENU_ITEMS) {
        if page != 0 {
            return Err(MenuPageError::OutOfRange);
        }
        return Ok(MenuPage {
            start: 0,
            end: total,
            newer: false,
            older: false,
        });
    }
    let page_size = height
        .saturating_sub(MENU_BORDER_ROWS + MENU_BASE_ROWS + MENU_PAGING_ROWS)
        .min(MENU_ITEMS);
    if page_size == 0 {
        return Err(MenuPageError::TooShort);
    }
    let pages = total.div_ceil(page_size);
    if page >= pages {
        return Err(MenuPageError::OutOfRange);
    }
    let start = page * page_size;
    let end = (start + page_size).min(total);
    Ok(MenuPage {
        start,
        end,
        newer: page > 0,
        older: end < total,
    })
}

fn fits(text: &str, width: usize) -> bool {
    // menu_add_item currently compares UTF-8 bytes before trimming by cells.
    // Satisfy both measures so Korean labels are not unexpectedly cut.
    text.len() <= width && UnicodeWidthStr::width(text) <= width
}

/// Keeps the detailed wording on ordinary terminals and falls back through
/// compact, still human-readable labels without exposing the storage name.
fn snapshot_menu_label(
    time: &str,
    kind: &str,
    sessions: usize,
    panes: usize,
    korean: bool,
    width: usize,
) -> Option<String> {
    let localized_kind = if kind == "auto" {
        if korean { "자동" } else { "auto" }
    } else if korean {
        "수동"
    } else {
        "saved"
    };
    let full = if korean {
        format!("{time}  {localized_kind}  세션 {sessions} · pane {panes}")
    } else {
        format!(
            "{time}  {localized_kind}  {}, {}",
            plural(sessions, "session"),
            plural(panes, "pane")
        )
    };
    let medium = if korean {
        format!("{time}  {localized_kind}  세션 {sessions} · {panes}p")
    } else {
        format!("{time}  {localized_kind}  {sessions}s · {panes}p")
    };
    let compact = format!("{time} · {sessions}/{panes}");
    [full, medium, compact, time.to_owned()]
        .into_iter()
        .find(|label| fits(label, width))
}

fn nav_label<'a>(wide: &'a str, compact: &'a str, width: usize) -> &'a str {
    if fits(wide, width) { wide } else { compact }
}

fn menu(
    server: &Server,
    socket: &str,
    client: &str,
    korean: bool,
    requested_page: usize,
) -> Result<(), String> {
    let text = |en: &'static str, ko: &'static str| if korean { ko } else { en };
    let target = client_target(server, client).map_err(|_| {
        text(
            "Could not find the requesting terminal",
            "요청한 터미널을 찾지 못했습니다",
        )
        .to_owned()
    })?;
    let geometry = menu_geometry(server, client, &target).map_err(|_| {
        text(
            "Could not read the terminal size",
            "터미널 크기를 확인하지 못했습니다",
        )
        .to_owned()
    })?;
    let title = text("Restore sessions", "세션 불러오기");
    let minimum_width = UnicodeWidthStr::width(title) + MENU_BORDER_COLUMNS;
    if geometry.width < minimum_width || geometry.height < 5 {
        return Err(text(
            "Terminal is too small for the restore menu",
            "세션 불러오기 메뉴를 열기에는 터미널이 너무 작습니다",
        )
        .into());
    }
    let dir = snapshot_dir()?;
    let snapshots = list_snapshots(&dir)?
        .into_iter()
        .filter_map(|name| {
            let snapshot = load(&dir.join(format!("{name}.json"))).ok()?;
            let (sessions, panes) = counts(&snapshot);
            Some(MenuSnapshot {
                name,
                created_at_ms: snapshot.created_at_ms,
                kind: snapshot.kind,
                sessions,
                panes,
            })
        })
        .collect::<Vec<_>>();
    let page = menu_page(snapshots.len(), geometry.height, requested_page).map_err(|error| {
        match error {
            MenuPageError::TooShort => text(
                "Terminal is too short for the restore menu",
                "세션 불러오기 메뉴를 열기에는 터미널 높이가 부족합니다",
            ),
            MenuPageError::OutOfRange => text(
                "Restore menu page is no longer available",
                "불러오기 메뉴 페이지가 더 이상 없습니다",
            ),
        }
        .to_owned()
    })?;
    let mut args: Vec<String> = [
        "display-menu",
        "-M",
        "-c",
        client,
        "-t",
        &target,
        "-x",
        "#{e|+:#{window_offset_x},#{e|/:#{e|-:#{masil_viewport_width},#{popup_width}},2}}",
        "-y",
        "#{e|+:#{window_offset_y},#{e|/:#{e|+:#{masil_viewport_height},#{popup_height}},2}}",
        "-T",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    args.push(format!("#[align=centre]{title}"));
    args.push(text("Save now", "지금 저장").into());
    args.push("C-s".into());
    args.push(run_self(socket, client, &["save"])?);
    args.push(String::new());
    let label_width = geometry
        .width
        .saturating_sub(MENU_BORDER_COLUMNS + MENU_HOTKEY_COLUMNS);
    for (shown, snapshot) in snapshots[page.start..page.end].iter().enumerate() {
        let label = snapshot_menu_label(
            &local_time(snapshot.created_at_ms),
            &snapshot.kind,
            snapshot.sessions,
            snapshot.panes,
            korean,
            label_width,
        )
        .ok_or_else(|| {
            text(
                "Terminal is too narrow for the restore menu",
                "세션 불러오기 메뉴를 열기에는 터미널 너비가 부족합니다",
            )
            .to_owned()
        })?;
        // Labels are formats; keep a literal #.
        args.push(label.replace('#', "##"));
        args.push(if shown < 9 {
            (shown + 1).to_string()
        } else {
            String::new()
        });
        args.push(run_self(socket, client, &["restore", &snapshot.name])?);
    }
    if snapshots.is_empty() {
        let empty = nav_label(
            text("No saved sessions", "저장된 세션이 없습니다"),
            text("None saved", "저장 없음"),
            geometry.width.saturating_sub(MENU_BORDER_COLUMNS + 1),
        );
        args.push(format!("-{empty}"));
        args.push(String::new());
        args.push(String::new());
    }
    if page.newer || page.older {
        args.push(String::new());
        let nav_width = geometry.width.saturating_sub(MENU_BORDER_COLUMNS);
        if page.newer {
            let page_number = requested_page.to_string();
            args.push(
                nav_label(
                    text("‹ Newer snapshots", "‹ 최신 저장본"),
                    text("‹ Newer", "‹ 최신"),
                    nav_width,
                )
                .into(),
            );
            args.push(String::new());
            args.push(run_self(socket, client, &["menu", &page_number])?);
        }
        if page.older {
            let page_number = (requested_page + 2).to_string();
            args.push(
                nav_label(
                    text("Older snapshots ›", "이전 저장본 ›"),
                    text("Older ›", "이전 ›"),
                    nav_width,
                )
                .into(),
            );
            args.push(String::new());
            args.push(run_self(socket, client, &["menu", &page_number])?);
        }
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    masil(server, &refs).map(|_| ())
}

// ---------------------------------------------------------------- autosave

/// Continues in a detached child with no terminal and closed standard
/// streams. Returns false in the parent.
fn detach() -> Result<bool, String> {
    // SAFETY: nothing has started threads yet; the child only continues the
    // single-threaded saver.
    match unsafe { libc::fork() } {
        -1 => return Err(std::io::Error::last_os_error().to_string()),
        0 => {}
        _ => return Ok(false),
    }
    // SAFETY: setsid, open and dup2 have no preconditions beyond valid
    // arguments.
    unsafe {
        libc::setsid();
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            for fd in 0..3 {
                libc::dup2(null, fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
    }
    Ok(true)
}

fn autosave_lock(dir: &Path, socket: &str) -> Result<Option<File>, String> {
    // FNV-1a keeps the lock name short and free of path characters.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in socket.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    let path = dir.join(format!(".autosave-{hash:016x}.lock"));
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|error| error.to_string())?;
    // SAFETY: the descriptor is valid while file lives.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error.to_string());
    }
    Ok(Some(file))
}

/// Saves automatically every @masil-autosave minutes while this server lives.
/// One saver runs per socket; it stops when no server answers there or the
/// masil UI layer is turned off, and waits while @masil-autosave is off or 0.
fn autosave(server: &Server, socket: &str) -> Result<i32, String> {
    // Before the saver lock: when the server restarts on this socket within
    // one check, the earlier saver keeps the lock and this one exits.
    crate::coordinator::restart_if_enabled(Path::new(socket));
    let dir = snapshot_dir()?;
    let Some(_lock) = autosave_lock(&dir, socket)? else {
        return Ok(0);
    };
    let mut last = std::time::Instant::now();
    let mut boot = None;
    loop {
        std::thread::sleep(AUTOSAVE_CHECK);
        // A server restarted on the same socket is saved by this saver; its
        // own saver found the lock taken.
        let Ok(state) = masil(
            server,
            &[
                "display-message",
                "-p",
                "#{@masil-agent}\u{1f}#{@masil-ui}\u{1f}#{@masil-autosave}\u{1f}#{masil_core_boot_id}\u{1f}#{@masil-inbox}\u{1f}#{@masil-answers}",
            ],
        ) else {
            // A slow reply is no reason to stop; a socket nobody listens on is.
            if UnixStream::connect(socket).is_err() {
                return Ok(0);
            }
            continue;
        };
        let fields: Vec<_> = state.trim_end_matches('\n').split('\u{1f}').collect();
        let [agent, ui, value, current, inbox, answers] = fields.as_slice() else {
            continue;
        };
        // A server restarted on this socket: start its coordinator if a
        // feature needs one.
        if boot.as_deref().is_some_and(|boot| boot != *current) {
            crate::coordinator::restart_if_enabled(Path::new(socket));
        } else if *inbox == "on" || *answers == "on" {
            // The inbox and `--answers` runs need a coordinator: bring back
            // one that ended or runs a replaced executable. A current one
            // answers at once; with no `--answers` run left nothing starts.
            crate::coordinator::revive(Path::new(socket));
        }
        boot = Some((*current).to_owned());
        // The core sets @masil-agent only when it loads the layer.
        if agent.is_empty() || *ui == "off" {
            return Ok(0);
        }
        // Off waits here, so turning it on again needs no new saver.
        let minutes = match *value {
            "" => AUTOSAVE_MINUTES,
            value => value.parse::<u64>().unwrap_or(0),
        };
        if minutes > 0 && last.elapsed() >= Duration::from_secs(minutes * 60) {
            let _ = save(server, socket, Kind::Auto);
            last = std::time::Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_utc_and_sortable() {
        assert_eq!(stamp(0), "19700101-000000");
        assert_eq!(stamp(1_790_683_743_000), "20260929-120903");
        assert!(stamp(1_000_000) < stamp(2_000_000));
    }

    #[test]
    fn names_and_words_are_checked() {
        assert!(valid_name("20260929-120903-manual").is_ok());
        assert!(valid_name("20260929-120903-2-auto").is_ok());
        assert!(valid_name("../x-manual").is_err());
        assert!(valid_name("20260929-120903").is_err());
        assert_eq!(shell_word("vim"), "vim");
        assert_eq!(shell_word("my file"), "'my file'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
        assert_eq!(shell_word("=notes"), "'=notes'");
        assert_eq!(literal("a#{b}"), "a##{b}");
        let mut names = vec![
            "20260929-120903-10-auto",
            "20260929-120903-auto",
            "20260929-120903-2-auto",
            "20260929-120904-manual",
            "20260929-120903-9-auto",
        ];
        names.sort_by(|a, b| order(a).cmp(&order(b)));
        assert_eq!(
            names,
            [
                "20260929-120903-auto",
                "20260929-120903-2-auto",
                "20260929-120903-9-auto",
                "20260929-120903-10-auto",
                "20260929-120904-manual",
            ]
        );
        assert_eq!(
            shell_line(&["vim".into(), "a b.txt".into()]),
            "vim 'a b.txt'"
        );
    }

    #[test]
    fn restore_menu_geometry_uses_the_smaller_drawing_area() {
        assert_eq!(
            parse_menu_geometry("48\u{1f}12\u{1f}44\u{1f}11\n"),
            Some(MenuGeometry {
                width: 44,
                height: 11,
            })
        );
        assert_eq!(parse_menu_geometry("48\u{1f}x\u{1f}44\u{1f}11\n"), None);
        assert_eq!(parse_menu_geometry("48\u{1f}12\u{1f}0\u{1f}11\n"), None);
    }

    #[test]
    fn restore_menu_target_comes_from_the_named_client() {
        let listing = "/dev/ttys001\u{1f}$0\u{1f}%9\n/dev/ttys002\u{1f}$1\u{1f}%9\n";
        assert_eq!(
            parse_client_target(listing, "/dev/ttys002").as_deref(),
            Some("$1:.%9")
        );
        assert_eq!(parse_client_target(listing, "/dev/ttys003"), None);
        assert_eq!(
            parse_client_target("/dev/ttys002\u{1f}$1\u{1f}$9\n", "/dev/ttys002"),
            None
        );
    }

    #[test]
    fn restore_menu_pages_fit_and_keep_every_snapshot_reachable() {
        assert_eq!(
            menu_page(8, 12, 0),
            Ok(MenuPage {
                start: 0,
                end: 8,
                newer: false,
                older: false,
            })
        );
        assert_eq!(
            menu_page(13, 12, 0),
            Ok(MenuPage {
                start: 0,
                end: 5,
                newer: false,
                older: true,
            })
        );
        assert_eq!(
            menu_page(13, 12, 1),
            Ok(MenuPage {
                start: 5,
                end: 10,
                newer: true,
                older: true,
            })
        );
        assert_eq!(
            menu_page(13, 12, 2),
            Ok(MenuPage {
                start: 10,
                end: 13,
                newer: true,
                older: false,
            })
        );
        assert_eq!(menu_page(13, 12, 3), Err(MenuPageError::OutOfRange));
        assert_eq!(menu_page(4, 7, 0), Err(MenuPageError::TooShort));
    }

    #[test]
    fn restore_menu_labels_preserve_detail_then_compact_by_real_width() {
        let wide = snapshot_menu_label("09-30 12:34", "manual", 1, 1, false, 80).unwrap();
        assert_eq!(wide, "09-30 12:34  saved  1 session, 1 pane");
        let narrow = snapshot_menu_label("09-30 12:34", "manual", 12, 34, false, 11).unwrap();
        assert_eq!(narrow, "09-30 12:34");
        assert!(snapshot_menu_label("09-30 12:34", "manual", 1, 1, false, 10).is_none());

        let korean = snapshot_menu_label("09-30 12:34", "manual", 1, 1, true, 40).unwrap();
        assert!(fits(&korean, 40));
        assert!(korean.contains("수동"));
        assert!(korean.len() <= 40);
    }

    #[test]
    fn commands_match_by_program_name() {
        let allowed: HashSet<String> = DEFAULT_COMMANDS.iter().map(|s| s.to_string()).collect();
        assert!(is_allowed(&["/usr/bin/vim".into(), "x".into()], &allowed));
        assert!(!is_allowed(&["rm".into(), "-rf".into()], &allowed));
        assert!(!is_allowed(&[], &allowed));
        assert_eq!(program(&["-zsh".into()]), "zsh");
    }

    #[test]
    fn snapshots_reject_bad_names_and_commands() {
        let pane = SavedPane {
            index: 0,
            active: true,
            cwd: "/tmp".into(),
            title: String::new(),
            command: Some(vec!["vim".into()]),
            resume: None,
            origin: "%0".into(),
        };
        let mut snapshot = Snapshot {
            version: VERSION,
            created_at_ms: 0,
            kind: "manual".into(),
            sessions: vec![SavedSession {
                name: "work".into(),
                last_attached: 0,
                active_window: 0,
                windows: vec![SavedWindow {
                    index: 0,
                    name: "w".into(),
                    zoomed: false,
                    layout: "{}".into(),
                    panes: vec![pane],
                }],
            }],
        };
        assert!(validate(&snapshot).is_ok());
        snapshot.sessions[0].name = "a:b".into();
        assert!(validate(&snapshot).is_err());
        snapshot.sessions[0].name = "work".into();
        snapshot.sessions[0].windows[0].panes[0].command = Some(Vec::new());
        assert!(validate(&snapshot).is_err());
        // A control character would edit the shell's line when typed back.
        snapshot.sessions[0].windows[0].panes[0].command =
            Some(vec!["tail".into(), "log\u{15}touch x\r".into()]);
        assert!(validate(&snapshot).is_err());
        assert!(!safe_argv(&["tail".into(), "a\u{3}".into()]));
        assert_eq!(clean("a\tb"), "a_b");
    }
}
