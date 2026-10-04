//! Talks to the running masil server and applies saved choices.

use super::catalog::{SETTINGS, Scope, Setting, UI_KEY};
use super::store::{self, LAYER};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(3);
const OUTPUT_LIMIT: u64 = 256 * 1024;
const NO_ANSWER: &str = "masil did not answer";

pub(crate) struct Server {
    binary: PathBuf,
    socket: PathBuf,
    pane: Option<String>,
}

/// What happened to a change. Only `Applied` claims the change took effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    Applied,
    /// Applied, but a session or window value hides it here.
    Hidden(&'static str),
    /// Applied now; saving failed, so it lasts until the server stops.
    SessionOnly(String),
    /// Saved; the masil UI layer is off, so nothing changed on screen.
    SavedWhileOff,
    Failed(String),
}

impl Server {
    /// Uses --socket when given, else the server of the surrounding masil.
    pub(crate) fn locate(socket: Option<&str>) -> Result<Self, String> {
        let socket = match socket {
            Some(path) => PathBuf::from(path),
            None => {
                let value = std::env::var("TMUX")
                    .map_err(|_| "run this inside masil or pass --socket PATH")?;
                PathBuf::from(value.split(',').next().unwrap_or_default())
            }
        };
        if !socket.is_absolute() {
            return Err("the masil socket path must be absolute".into());
        }
        let binary = crate::native_ui::native_executable()
            .ok()
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("masil"));
        let pane = std::env::var("TMUX_PANE")
            .ok()
            .filter(|pane| pane.starts_with('%'));
        Ok(Self {
            binary,
            socket,
            pane,
        })
    }

    /// Runs one masil command and returns its standard output.
    pub(crate) fn run(&self, args: &[&str]) -> Result<String, String> {
        let (success, out, err) = self.execute(args, Some(TIMEOUT))?;
        if success {
            Ok(out)
        } else {
            // Configuration errors from source-file arrive on standard output.
            let message = err
                .lines()
                .chain(out.lines())
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("masil command failed");
            Err(message.to_owned())
        }
    }

    /// Runs one masil command, waiting at most `timeout` when given, and
    /// returns whether it succeeded with its standard output and error.
    fn execute(
        &self,
        args: &[&str],
        timeout: Option<Duration>,
    ) -> Result<(bool, String, String), String> {
        // -u keeps UTF-8 output, which some list formats rely on, without a
        // UTF-8 locale.
        let mut child = Command::new(&self.binary)
            .arg("-u")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot run masil: {error}"))?;
        let mut stdout = child.stdout.take().ok_or("masil output unavailable")?;
        let mut stderr = child.stderr.take().ok_or("masil output unavailable")?;
        let out = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = (&mut stdout).take(OUTPUT_LIMIT).read_to_string(&mut text);
            text
        });
        let err = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = (&mut stderr).take(OUTPUT_LIMIT).read_to_string(&mut text);
            text
        });
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                break status;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                let _ = child.kill();
                let _ = child.wait();
                // The pipes close with the child, so the readers end; a
                // caller that forks next must not leave them running.
                let _ = out.join();
                let _ = err.join();
                return Err(format!("{NO_ANSWER}: {}", args.join(" ")));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let out = out.join().unwrap_or_default();
        let err = err.join().unwrap_or_default();
        Ok((status.success(), out, err))
    }

    /// The global value of an option, or None when it is unset.
    pub(crate) fn global(&self, option: &str, scope: Scope) -> Option<String> {
        let flags = match scope {
            Scope::Session => "-gqv",
            Scope::Window => "-gwqv",
        };
        self.run(&["show-options", flags, option])
            .ok()
            .map(|text| text.trim_end_matches('\n').to_owned())
            .filter(|text| !text.is_empty())
    }

    /// A value set on this session or window that hides the global one.
    fn local(&self, option: &str, scope: Scope) -> Option<String> {
        let pane = self.pane.as_deref()?;
        let flags = match scope {
            Scope::Session => "-qv",
            Scope::Window => "-wqv",
        };
        self.run(&["show-options", flags, "-t", pane, option])
            .ok()
            .map(|text| text.trim_end_matches('\n').to_owned())
            .filter(|text| !text.is_empty())
    }

    /// Whether a session or window value hides the global one: the value of
    /// this pane's session or window inside masil, else of any session or
    /// window. Targets are read in chained commands that stay well under the
    /// client's command size limit; a chain that fails, as when a target
    /// closes meanwhile, is read again one target at a time.
    fn hidden(&self, option: &str, scope: Scope, value: &str) -> bool {
        const CHAIN_BYTES: usize = 12 * 1024;
        if self.pane.is_some() {
            return self
                .local(option, scope)
                .is_some_and(|local| local != value);
        }
        let (targets, flags) = match scope {
            Scope::Session => (self.run(&["list-sessions", "-F", "#{session_id}"]), "-qv"),
            Scope::Window => (
                self.run(&["list-windows", "-a", "-F", "#{window_id}"]),
                "-wqv",
            ),
        };
        let Ok(targets) = targets else {
            return false;
        };
        // Grouped sessions list their shared windows once per session.
        let mut unique: Vec<&str> = Vec::new();
        for target in targets.lines().filter(|line| !line.is_empty()) {
            if !unique.contains(&target) {
                unique.push(target);
            }
        }
        let differs = |text: &str| {
            text.lines()
                .any(|local| !local.is_empty() && local != value)
        };
        let per_command = option.len() + flags.len() + 32;
        for chunk in unique.chunks((CHAIN_BYTES / per_command).max(1)) {
            let mut args = Vec::new();
            for target in chunk {
                if !args.is_empty() {
                    args.push(";");
                }
                args.extend(["show-options", flags, "-t", target, option]);
            }
            match self.run(&args) {
                Ok(text) if differs(&text) => return true,
                Ok(_) => {}
                // A server that stops answering would make every retry wait.
                Err(error) if error.starts_with(NO_ANSWER) => return false,
                Err(_) => {
                    for target in chunk {
                        match self.run(&["show-options", flags, "-t", target, option]) {
                            Ok(text) if differs(&text) => return true,
                            Err(error) if error.starts_with(NO_ANSWER) => return false,
                            _ => {}
                        }
                    }
                }
            }
        }
        false
    }

    pub(crate) fn layer_on(&self) -> bool {
        self.global(UI_KEY, Scope::Session).as_deref() != Some("off")
    }

    /// Runs configuration text through a private temporary file.
    pub(crate) fn source(&self, text: &str) -> Result<(), String> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "masil-settings-{}-{}.conf",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|error| error.to_string())?;
            file.write_all(text.as_bytes())
                .map_err(|error| error.to_string())?;
            drop(file);
            let path = path.to_str().ok_or("temporary path is not UTF-8")?;
            self.run(&["source-file", path]).map(drop)
        })();
        let _ = std::fs::remove_file(&path);
        result
    }

    /// Unbinds the layer's buttons and keys, found by their `masil-ui:` notes.
    fn unbind_layer_keys(&self) -> Result<(), String> {
        for table in ["root", "prefix"] {
            let listing = self.run(&["list-keys", "-N", "-T", table])?;
            for line in listing.lines() {
                let words = line.split_whitespace().collect::<Vec<_>>();
                if let Some(index) = words.iter().position(|word| *word == "masil-ui:")
                    && index > 0
                {
                    self.run(&["unbind-key", "-T", table, words[index - 1]])?;
                }
            }
        }
        Ok(())
    }
}

fn save_choice(path: Option<&Path>, key: &str, value: &str) -> Result<(), String> {
    let path = path.ok_or("no configuration directory; changes apply to this session only")?;
    let _lock = store::Lock::take(path)?;
    let mut saved = store::load(path)?;
    saved.set(key, value);
    store::save(path, &saved)
}

impl Server {
    /// Whether an option still holds the value its layer line gives it, so
    /// nothing else (such as the user's tmux.conf) has set it.
    fn owned(&self, line: &store::LayerLine) -> bool {
        let Ok(current) = self.run(&["show-options", "-gv", &line.option]) else {
            return false;
        };
        let expected = if line.format {
            match self.run(&["display-message", "-p", &line.value]) {
                Ok(value) => value,
                Err(_) => return false,
            }
        } else {
            line.value.clone()
        };
        current.trim_end_matches('\n') == expected.trim_end_matches('\n')
    }

    /// tmux's own defaults for the given global options, read from a
    /// private server started without any configuration.
    fn tmux_defaults(&self, options: &[String]) -> Result<Vec<String>, String> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let socket = std::env::temp_dir().join(format!(
            "masil-defaults-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let socket = socket.to_str().ok_or("temporary path is not UTF-8")?;
        // A private socket and no configuration give tmux's own defaults.
        let mut args = vec!["-S", socket, "-f", "/dev/null", "start-server"];
        for option in options {
            args.extend([";", "show-options", "-gv", option.as_str()]);
        }
        let output = Command::new(&self.binary)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("cannot read tmux defaults: {error}"));
        let _ = std::fs::remove_file(socket);
        let output = output?;
        if !output.status.success() {
            return Err("cannot read tmux defaults".into());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let values = text.lines().map(str::to_owned).collect::<Vec<_>>();
        if values.len() != options.len() {
            return Err("unexpected tmux defaults".into());
        }
        Ok(values)
    }

    /// Re-reads the user's tmux configuration for the masil menu. The menu
    /// runs this under run-shell -b, so there is no time limit, like the
    /// core's own source-file; every error line is returned.
    pub(crate) fn reload_user_config(&self) -> Result<(), String> {
        let files = store::tmux_conf_files();
        if files.is_empty() {
            return Ok(());
        }
        let mut args = vec!["source-file", "-q"];
        args.extend(files.iter().filter_map(|path| path.to_str()));
        let (success, out, err) = self.execute(&args, None)?;
        if success {
            return Ok(());
        }
        let lines: Vec<&str> = err
            .lines()
            .chain(out.lines())
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .collect();
        Err(if lines.is_empty() {
            "masil command failed".to_owned()
        } else {
            lines.join("\n")
        })
    }

    /// Re-reads the user's tmux configuration so it keeps precedence over
    /// the layer, as at startup.
    fn source_user_config(&self) -> Result<(), String> {
        let files = store::tmux_conf_files();
        if files.is_empty() {
            return Ok(());
        }
        let mut args = vec!["source-file", "-q"];
        args.extend(files.iter().filter_map(|path| path.to_str()));
        self.run(&args).map(drop)
    }
}

/// Saves a choice and applies it now. Lines that depend on the choice are
/// re-applied only while they still hold the layer's value; the chosen
/// option itself is always applied. The effect is read back.
pub(crate) fn apply(
    server: &Server,
    path: Option<&Path>,
    setting: &Setting,
    value: &str,
) -> Outcome {
    if !setting.accepts(value) {
        return Outcome::Failed(format!("unsupported value {value}"));
    }
    let layer_on = server.layer_on();

    // Decide what to re-apply before the choice changes the expected values.
    let mut sources = Vec::new();
    let mut lines = Vec::new();
    if layer_on {
        for name in setting.sections {
            let Some(text) = store::section(name) else {
                return Outcome::Failed(format!("layer section {name} is missing"));
            };
            if matches!(*name, "keys" | "colors") {
                sources.push(text);
                continue;
            }
            let candidates = store::layer_lines(&text);
            // Under the tmux theme the layer leaves styles at tmux's
            // defaults, so an option still at its default belongs to it.
            let leaving_tmux = setting.key == "@masil-theme"
                && server.global("@masil-theme", Scope::Session).as_deref() == Some("tmux")
                && value != "tmux";
            let defaults = if leaving_tmux {
                let options = candidates
                    .iter()
                    .map(|line| line.option.clone())
                    .collect::<Vec<_>>();
                match server.tmux_defaults(&options) {
                    Ok(values) => Some(values),
                    Err(error) => return Outcome::Failed(error),
                }
            } else {
                None
            };
            for (index, line) in candidates.into_iter().enumerate() {
                let owned = match &defaults {
                    Some(values) => server
                        .run(&["show-options", "-gv", &line.option])
                        .is_ok_and(|current| current.trim_end_matches('\n') == values[index]),
                    None => server.owned(&line),
                };
                if line.value.contains(setting.key) || owned {
                    lines.push(line);
                }
            }
        }
    }

    if let Err(error) = server.run(&["set-option", "-g", setting.key, value]) {
        return Outcome::Failed(error);
    }
    let saved = save_choice(path, setting.key, value);
    if !layer_on {
        return match saved {
            Ok(()) => Outcome::SavedWhileOff,
            Err(error) => Outcome::Failed(format!("not saved: {error}")),
        };
    }
    for text in &sources {
        if let Err(error) = server.source(text) {
            return Outcome::Failed(error);
        }
    }
    // The tmux theme leaves styles at their tmux defaults.
    let unset = setting.key == "@masil-theme" && value == "tmux";
    for line in &lines {
        let result = if unset {
            server.run(&["set-option", "-gu", &line.option]).map(drop)
        } else {
            server.source(&line.text)
        };
        if let Err(error) = result {
            return Outcome::Failed(error);
        }
    }

    let Some((option, scope)) = setting.readback else {
        return finish(saved, Outcome::Applied);
    };
    let effective = server.global(option, scope);
    if effective.as_deref() != Some(value) {
        return Outcome::Failed(format!(
            "masil reports {option} = {}",
            effective.as_deref().unwrap_or("(unset)")
        ));
    }
    let outcome = if server.hidden(option, scope, value) {
        Outcome::Hidden(match scope {
            Scope::Session => "session",
            Scope::Window => "window",
        })
    } else {
        Outcome::Applied
    };
    finish(saved, outcome)
}

fn finish(saved: Result<(), String>, applied: Outcome) -> Outcome {
    match (saved, applied) {
        (_, Outcome::Failed(error)) => Outcome::Failed(error),
        (Err(error), _) => Outcome::SessionOnly(error),
        (Ok(()), outcome) => outcome,
    }
}

/// Turns the layer on or off. Off restores tmux defaults for every option
/// the layer sets, removes its buttons and colours and re-reads the user's
/// tmux.conf. On applies the layer and then the user's tmux.conf, as at
/// startup.
pub(crate) fn set_layer(server: &Server, path: Option<&Path>, on: bool) -> Outcome {
    let value = if on { "on" } else { "off" };
    if let Err(error) = server.run(&["set-option", "-g", UI_KEY, value]) {
        return Outcome::Failed(error);
    }
    let saved = save_choice(path, UI_KEY, value);
    let result = if on {
        server
            .source(LAYER)
            .and_then(|()| server.source_user_config())
    } else {
        (|| {
            for option in store::managed_options() {
                server.run(&["set-option", "-gu", &option])?;
            }
            for option in store::layer_user_options() {
                server.run(&["set-option", "-gu", &option])?;
            }
            server.unbind_layer_keys()?;
            server.source_user_config()
        })()
    };
    finish(
        saved,
        match result {
            Ok(()) => Outcome::Applied,
            Err(error) => Outcome::Failed(error),
        },
    )
}

/// Forgets every saved choice and applies the layer defaults, then the
/// user's tmux.conf.
pub(crate) fn reset(server: &Server, path: Option<&Path>) -> Outcome {
    let saved = match path {
        Some(path) => store::Lock::take(path).and_then(|_lock| {
            let mut saved = store::load(path)?;
            saved.remove_all_managed();
            store::save(path, &saved)
        }),
        None => Err("no configuration directory".into()),
    };
    let result = (|| {
        for setting in SETTINGS {
            server.run(&["set-option", "-gu", setting.key])?;
        }
        server.run(&["set-option", "-gu", UI_KEY])?;
        server.source(LAYER)?;
        server.source_user_config()
    })();
    finish(
        saved,
        match result {
            Ok(()) => Outcome::Applied,
            Err(error) => Outcome::Failed(error),
        },
    )
}
