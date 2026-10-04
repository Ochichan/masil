//! Saved choices in `settings.conf` and the built-in UI layer text.

use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The layer masil loads at startup; the core embeds the same file.
pub(crate) const LAYER: &str = include_str!("../../../../core/masil-ui-layer.conf");

const HEADER: &str =
    "# masil settings, version 1. Written by `masil-agent settings`; other lines are kept.";
const MAX_BYTES: u64 = 64 * 1024;
const INTERNAL_MARKERS: &[&str] = &["@masil-autosave-used", "@masil-coordinator-boot"];

/// The same rule as the core: an absolute XDG_CONFIG_HOME, else ~/.config.
pub(crate) fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })?;
    Some(base.join("masil/settings.conf"))
}

/// User configuration files the core reads after the layer, when present.
/// The list is the core's TMUX_CONF expanded as tmux.c expand_path does:
/// plain concatenation, and duplicates compared as strings.
pub(crate) fn tmux_conf_files() -> Vec<PathBuf> {
    let joined = |base: &OsStr, rest: &str| {
        let mut path = base.to_owned();
        path.push(rest);
        path
    };
    // HOME when set and not empty, else the password database, as the core's
    // find_home.
    let home = std::env::home_dir().map(PathBuf::into_os_string);
    let mut files = vec![OsString::from("/etc/tmux.conf")];
    if let Some(home) = &home {
        files.push(joined(home, "/.tmux.conf"));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        files.push(joined(&xdg, "/tmux/tmux.conf"));
    }
    if let Some(home) = &home {
        files.push(joined(home, "/.config/tmux/tmux.conf"));
    }
    let mut unique: Vec<OsString> = Vec::new();
    for file in files {
        if !unique.contains(&file) {
            unique.push(file);
        }
    }
    // A relative XDG_CONFIG_HOME names a file in the server's startup
    // directory, which cannot be recovered here; reading it from this
    // process's directory would read a different file.
    unique
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
        .collect()
}

/// Holds an exclusive lock on the settings directory while alive, so two
/// settings screens do not overwrite each other's choices.
pub(crate) struct Lock {
    _file: fs::File,
}

impl Lock {
    pub(crate) fn take(path: &Path) -> Result<Self, String> {
        let directory = path.parent().ok_or("settings path has no directory")?;
        fs::create_dir_all(directory).map_err(|error| error.to_string())?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join(".settings.lock"))
            .map_err(|error| error.to_string())?;
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(Self { _file: file })
    }
}

/// One `set -g NAME VALUE` or `set -gF NAME VALUE` line of the layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LayerLine {
    pub text: String,
    pub option: String,
    pub format: bool,
    pub value: String,
}

/// The single-option lines of a layer section, in order.
pub(crate) fn layer_lines(section: &str) -> Vec<LayerLine> {
    let mut lines = Vec::new();
    for line in section.lines() {
        let text = line.trim();
        let Some(rest) = text.strip_prefix("set ") else {
            continue;
        };
        let (format, rest) = if let Some(rest) = rest.strip_prefix("-gF ") {
            (true, rest)
        } else if let Some(rest) = rest.strip_prefix("-g ") {
            (false, rest)
        } else {
            continue;
        };
        let Some((option, value)) = rest.split_once(' ') else {
            continue;
        };
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value);
        lines.push(LayerLine {
            text: text.to_owned(),
            option: option.to_owned(),
            format,
            value: value.to_owned(),
        });
    }
    lines
}

/// Lines of settings.conf. Managed lines are `set -g @masil-KEY VALUE`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Saved {
    lines: Vec<String>,
}

impl Saved {
    pub(crate) fn parse(text: &str) -> Self {
        Self {
            lines: text
                .lines()
                .filter(|line| *line != HEADER)
                .map(str::to_owned)
                .collect(),
        }
    }

    fn managed(line: &str) -> Option<(&str, &str)> {
        let rest = line.strip_prefix("set -g ")?;
        let boundary = rest.find(char::is_whitespace)?;
        let key = &rest[..boundary];
        let value = rest[boundary..].trim_start();
        (key.starts_with("@masil-") && tmux_word(value).is_some()).then_some((key, value))
    }

    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.lines
            .iter()
            .rev()
            .filter_map(|line| Self::managed(line))
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, value)| value)
    }

    pub(crate) fn get_value(&self, key: &str) -> Option<String> {
        tmux_word(self.get(key)?)
    }

    /// Replaces the key's line in place, or appends it.
    pub(crate) fn set(&mut self, key: &str, value: &str) {
        let line = format!("set -g {key} {value}");
        let mut replaced = false;
        self.lines.retain_mut(|existing| {
            if Self::managed(existing).is_some_and(|(candidate, _)| candidate == key) {
                if replaced {
                    return false;
                }
                *existing = line.clone();
                replaced = true;
            }
            true
        });
        if !replaced {
            self.lines.push(line);
        }
    }

    fn set_quoted(&mut self, key: &str, value: &str) {
        self.set(key, &crate::native_ui::tmux_quote(value));
    }

    /// Removes every managed line for `key`, leaving all other lines alone.
    pub(crate) fn remove(&mut self, key: &str) {
        self.lines
            .retain(|line| Self::managed(line).is_none_or(|(candidate, _)| candidate != key));
    }

    pub(crate) fn remove_all_managed(&mut self) {
        self.lines.retain(|line| {
            Self::managed(line).is_none_or(|(key, _)| INTERNAL_MARKERS.contains(&key))
        });
    }

    pub(crate) fn render(&self) -> String {
        let mut text = String::from(HEADER);
        text.push('\n');
        for line in &self.lines {
            text.push_str(line);
            text.push('\n');
        }
        text
    }
}

/// Decodes the one tmux word used as a managed setting value. This supports
/// the single-quoted form produced by `tmux_quote`, including its quoted
/// apostrophe sequence, as well as existing plain and double-quoted values.
fn tmux_word(value: &str) -> Option<String> {
    #[derive(Clone, Copy)]
    enum Quote {
        Single,
        Double,
    }

    if value.is_empty() {
        return None;
    }
    let mut output = String::new();
    let mut quote = None;
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        match (quote, character) {
            (None, '\'') => quote = Some(Quote::Single),
            (None, '"') => quote = Some(Quote::Double),
            (None, '\\') => output.push(characters.next()?),
            (None, character) if character.is_whitespace() => return None,
            (None, character) => output.push(character),
            (Some(Quote::Single), '\'') => quote = None,
            (Some(Quote::Single), character) => output.push(character),
            (Some(Quote::Double), '"') => quote = None,
            (Some(Quote::Double), '\\') => output.push(characters.next()?),
            (Some(Quote::Double), character) => output.push(character),
        }
    }
    quote.is_none().then_some(output)
}

fn socket_marker_entries(value: &str) -> Option<Vec<&str>> {
    if matches!(value, "" | "on" | "off") {
        return Some(Vec::new());
    }
    let inner = value.strip_prefix('|')?.strip_suffix('|')?;
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let entries = inner.split('|').collect::<Vec<_>>();
    entries
        .iter()
        .all(|entry| !entry.is_empty())
        .then_some(entries)
}

pub(crate) fn socket_marker_contains(value: &str, socket: &str) -> bool {
    socket_marker_entries(value).is_some_and(|entries| {
        entries
            .into_iter()
            .any(|entry| same_socket_path(Path::new(entry), Path::new(socket)))
    })
}

fn same_socket_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn canonical_socket_text(socket: &str) -> String {
    Path::new(socket)
        .canonicalize()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_else(|| socket.to_owned())
}

pub(crate) fn changed_socket_marker(
    current: Option<&str>,
    socket: &str,
    enabled: bool,
) -> Result<Option<String>, String> {
    if !Path::new(socket).is_absolute() {
        return Err("coordinator boot marker requires an absolute socket path".into());
    }
    if socket.contains('|') || socket.contains(['\n', '\r']) {
        return Err("coordinator boot marker socket contains an unsupported character".into());
    }
    let mut entries = match current {
        Some(value) => socket_marker_entries(value)
            .ok_or("invalid coordinator boot marker")?
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        None => Vec::new(),
    };
    let mut unique = Vec::with_capacity(entries.len() + usize::from(enabled));
    for entry in entries.drain(..) {
        if enabled || !same_socket_path(Path::new(&entry), Path::new(socket)) {
            let entry = canonical_socket_text(&entry);
            if !unique.contains(&entry) {
                unique.push(entry);
            }
        }
    }
    if enabled && !unique.iter().any(|entry| entry == socket) {
        unique.push(socket.to_owned());
    }
    if unique.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!("|{}|", unique.join("|"))))
    }
}

/// Reads settings.conf. A missing file is empty; a symlink or other
/// unexpected file is refused so it is never replaced.
pub(crate) fn load(path: &Path) -> Result<Saved, String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Saved::default()),
        Err(_) => {
            return Err(format!(
                "{} is not a readable regular file; changes apply to this session only",
                path.display()
            ));
        }
    };
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(format!(
            "{} is not a small regular file; changes apply to this session only",
            path.display()
        ));
    }
    let mut text = String::new();
    file.take(MAX_BYTES)
        .read_to_string(&mut text)
        .map_err(|_| format!("{} is not UTF-8 text", path.display()))?;
    Ok(Saved::parse(&text))
}

/// Writes settings.conf privately through a temporary file and rename.
pub(crate) fn save(path: &Path, saved: &Saved) -> Result<(), String> {
    let directory = path.parent().ok_or("settings path has no directory")?;
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.is_file()
    {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let temporary = directory.join(format!(".settings.conf.{}", std::process::id()));
    let result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(saved.render().as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
        fs::rename(&temporary, path).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Adds or removes an internal on/off marker without disturbing user lines.
/// Returns without rewriting the file when it already has the wanted state.
pub(crate) fn update_marker(path: &Path, key: &str, enabled: bool) -> Result<(), String> {
    let _lock = Lock::take(path)?;
    let mut saved = load(path)?;
    if enabled {
        if saved.get(key) == Some("on") {
            return Ok(());
        }
        saved.set(key, "on");
    } else {
        if saved.get(key).is_none() {
            return Ok(());
        }
        saved.remove(key);
    }
    save(path, &saved)
}

/// Adds or removes one absolute socket from an internal marker list. An
/// unchanged list returns before taking the settings lock. A change is
/// checked again under the lock before the atomic replacement.
pub(crate) fn update_socket_marker(
    path: &Path,
    key: &str,
    socket: &str,
    enabled: bool,
) -> Result<(Option<String>, bool), String> {
    let current = load(path)?.get_value(key);
    let changed = changed_socket_marker(current.as_deref(), socket, enabled)?;
    if changed == current {
        return Ok((changed, false));
    }
    let _lock = Lock::take(path)?;
    let mut saved = load(path)?;
    let current = saved.get_value(key);
    let changed = changed_socket_marker(current.as_deref(), socket, enabled)?;
    if changed == current {
        return Ok((changed, false));
    }
    match &changed {
        Some(value) => saved.set_quoted(key, value),
        None => saved.remove(key),
    }
    save(path, &saved)?;
    Ok((changed, true))
}

/// Text of one `## masil:section NAME` block of the layer.
pub(crate) fn section(name: &str) -> Option<String> {
    let start = format!("## masil:section {name}\n");
    let begin = LAYER.find(&start)? + start.len();
    let end = LAYER[begin..].find("## masil:end")? + begin;
    Some(LAYER[begin..end].to_owned())
}

/// Names of the layer's sections in order.
#[cfg(test)]
pub(crate) fn section_names() -> Vec<&'static str> {
    LAYER
        .lines()
        .filter_map(|line| line.strip_prefix("## masil:section "))
        .collect()
}

/// The layer's own `@masil-*` options that are not saved choices: colours,
/// menu behaviour and the remembered sidebar width.
pub(crate) fn layer_user_options() -> Vec<String> {
    let mut names = Vec::new();
    for line in LAYER.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some("set") {
            continue;
        }
        let _ = words.next();
        if let Some(name) = words.next()
            && (name.starts_with("@masil-c-")
                || name == "@masil-menu-stay-open"
                || name == "@masil-sidebar-width")
            && !names.iter().any(|known| known == name)
        {
            names.push(name.to_owned());
        }
    }
    names
}

/// Global tmux options the layer sets, excluding its own `@` options.
pub(crate) fn managed_options() -> Vec<String> {
    let mut names = Vec::new();
    for line in LAYER.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some("set") {
            continue;
        }
        let Some(flags) = words.next() else { continue };
        if !flags.starts_with("-g") || flags.contains('u') {
            continue;
        }
        let Some(name) = words.next() else { continue };
        let name = name.split('[').next().unwrap_or(name);
        if !name.starts_with('@') && !names.iter().any(|known| known == name) {
            names.push(name.to_owned());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::settings::catalog::{Kind, SETTINGS, UI_KEY};

    #[test]
    fn set_replaces_in_place_and_keeps_other_lines() {
        let mut saved = Saved::parse(&format!(
            "{HEADER}\n# mine\nset -g @masil-theme light\nbind x kill-pane\nset -g @masil-theme dark\n"
        ));
        assert_eq!(saved.get("@masil-theme"), Some("dark"));
        saved.set("@masil-theme", "terminal");
        saved.set("@masil-lang", "ko");
        assert_eq!(
            saved.render(),
            format!(
                "{HEADER}\n# mine\nset -g @masil-theme terminal\nbind x kill-pane\nset -g @masil-lang ko\n"
            )
        );
        saved.remove_all_managed();
        assert_eq!(
            saved.render(),
            format!("{HEADER}\n# mine\nbind x kill-pane\n")
        );
    }

    #[test]
    fn marker_lines_are_added_and_removed_without_touching_other_lines() {
        let directory = std::env::temp_dir().join(format!(
            "masil-marker-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("settings.conf");
        fs::write(
            &path,
            "# mine\nset -g @masil-theme light\nbind x kill-pane\n",
        )
        .unwrap();

        let first = "/tmp/masil socket/one";
        let second = "/tmp/masil/socket'two";
        assert_eq!(
            update_socket_marker(&path, "@masil-coordinator-boot", first, true).unwrap(),
            (Some(format!("|{first}|")), true)
        );
        fs::remove_file(directory.join(".settings.lock")).unwrap();
        assert_eq!(
            update_socket_marker(&path, "@masil-coordinator-boot", first, true).unwrap(),
            (Some(format!("|{first}|")), false)
        );
        assert!(!directory.join(".settings.lock").exists());
        assert_eq!(
            update_socket_marker(&path, "@masil-coordinator-boot", second, true).unwrap(),
            (Some(format!("|{first}|{second}|")), true)
        );
        let saved = load(&path).unwrap();
        assert_eq!(
            saved.get_value("@masil-coordinator-boot"),
            Some(format!("|{first}|{second}|"))
        );
        assert!(
            saved
                .get("@masil-coordinator-boot")
                .is_some_and(|value| value.starts_with('\'') && value.ends_with('\''))
        );

        assert_eq!(
            update_socket_marker(&path, "@masil-coordinator-boot", first, false).unwrap(),
            (Some(format!("|{second}|")), true)
        );
        assert_eq!(
            update_socket_marker(&path, "@masil-coordinator-boot", second, false).unwrap(),
            (None, true)
        );
        assert_eq!(
            load(&path).unwrap().render(),
            format!("{HEADER}\n# mine\nset -g @masil-theme light\nbind x kill-pane\n")
        );
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn socket_markers_compare_canonical_paths() {
        let directory = std::env::temp_dir().join(format!(
            "masil-marker-paths-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&directory);
        let real = directory.join("real");
        fs::create_dir_all(&real).unwrap();
        let socket = real.join("socket[one]*?");
        fs::write(&socket, "").unwrap();
        let alias = directory.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let aliased = alias.join("socket[one]*?");
        let canonical = socket.canonicalize().unwrap();
        let marker = format!("|{}|", aliased.display());
        assert!(socket_marker_contains(&marker, canonical.to_str().unwrap()));
        assert_eq!(
            changed_socket_marker(Some(&marker), canonical.to_str().unwrap(), true).unwrap(),
            Some(format!("|{}|", canonical.display()))
        );

        let current = std::env::current_dir().unwrap().canonicalize().unwrap();
        let root_depth = current.components().count().saturating_sub(1);
        let mut relative = PathBuf::new();
        for _ in 0..root_depth {
            relative.push("..");
        }
        relative.push(canonical.strip_prefix("/").unwrap());
        let marker = format!("|{}|", relative.display());
        assert!(socket_marker_contains(&marker, canonical.to_str().unwrap()));
        assert_eq!(
            changed_socket_marker(Some(&marker), canonical.to_str().unwrap(), true).unwrap(),
            Some(format!("|{}|", canonical.display()))
        );

        if let (Ok(tmp), Ok(private_tmp)) = (
            Path::new("/tmp").canonicalize(),
            Path::new("/private/tmp").canonicalize(),
        ) && tmp == private_tmp
        {
            let name = format!("masil-marker-alias-{}", std::process::id());
            let tmp_socket = Path::new("/tmp").join(&name);
            fs::write(&tmp_socket, "").unwrap();
            let private_socket = Path::new("/private/tmp").join(&name);
            let marker = format!("|{}|", tmp_socket.display());
            assert!(socket_marker_contains(
                &marker,
                private_socket.to_str().unwrap()
            ));
            fs::remove_file(tmp_socket).unwrap();
        }
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn reset_keeps_internal_markers() {
        let mut saved = Saved::parse(
            "set -g @masil-theme dark\nset -g @masil-autosave-used on\nset -g @masil-coordinator-boot '|/tmp/one|'\n",
        );
        saved.remove_all_managed();
        assert_eq!(saved.get("@masil-theme"), None);
        assert_eq!(
            saved.get_value("@masil-autosave-used").as_deref(),
            Some("on")
        );
        assert_eq!(
            saved.get_value("@masil-coordinator-boot").as_deref(),
            Some("|/tmp/one|")
        );
    }

    #[test]
    fn every_section_is_found_and_balanced() {
        let names = section_names();
        assert_eq!(
            names,
            [
                "base", "panes", "colors", "styles", "bar", "keys", "sessions"
            ]
        );
        for name in names {
            let text = section(name).unwrap();
            assert_eq!(
                text.matches('{').count(),
                text.matches('}').count(),
                "unbalanced section {name}"
            );
        }
    }

    #[test]
    fn layer_defaults_match_the_catalog() {
        for setting in SETTINGS {
            for section in setting.sections {
                assert!(
                    section_names().contains(section),
                    "{} names a missing section",
                    setting.key
                );
            }
            let key = setting.key;
            let pattern = format!("#{{?#{{{key}}},#{{{key}}},");
            for (index, _) in LAYER.match_indices(&pattern) {
                let rest = &LAYER[index + pattern.len()..];
                let default = &rest[..rest.find('}').unwrap()];
                assert_eq!(default, setting.default, "layer default for {key}");
            }
            if let Kind::Choices(choices) = setting.kind {
                assert!(choices.iter().any(|choice| choice.value == setting.default));
            }
            assert!(setting.accepts(setting.default));
        }
        assert!(LAYER.contains(UI_KEY));
    }

    #[test]
    fn style_lines_are_single_options() {
        let lines = layer_lines(&section("styles").unwrap());
        assert!(lines.len() >= 10);
        assert!(
            lines
                .iter()
                .all(|line| line.format && !line.option.starts_with('@'))
        );
        let status = lines
            .iter()
            .find(|line| line.option == "status-style")
            .unwrap();
        assert_eq!(status.value, "bg=#{@masil-c-panel},fg=#{@masil-c-muted}");
        let base = layer_lines(&section("base").unwrap());
        assert!(base.iter().any(|line| line.option == "status-position"
            && line.value.contains("@masil-status-position")));
    }

    #[test]
    fn managed_options_are_real_tmux_options() {
        let names = managed_options();
        for expected in [
            "status-position",
            "pane-border-status",
            "status-format",
            "mouse",
        ] {
            assert!(names.iter().any(|name| name == expected), "{expected}");
        }
        assert!(names.iter().all(|name| !name.starts_with('@')));
    }

    #[test]
    fn save_refuses_symlinks_and_writes_privately() {
        let directory = std::env::temp_dir().join(format!("masil-settings-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("settings.conf");
        let mut saved = Saved::default();
        saved.set("@masil-theme", "light");
        save(&path, &saved).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(load(&path).unwrap().get("@masil-theme"), Some("light"));
        update_marker(&path, "@masil-autosave-used", true).unwrap();
        assert_eq!(load(&path).unwrap().get("@masil-autosave-used"), Some("on"));
        update_marker(&path, "@masil-autosave-used", false).unwrap();
        assert_eq!(load(&path).unwrap().get("@masil-autosave-used"), None);
        assert_eq!(load(&path).unwrap().get("@masil-theme"), Some("light"));
        let link = directory.join("link.conf");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(load(&link).is_err());
        assert!(save(&link, &saved).is_err());
        fs::remove_dir_all(&directory).unwrap();
    }
}
