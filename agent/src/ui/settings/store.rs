//! Saved choices in `settings.conf` and the built-in UI layer text.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The layer rmux loads at startup; the core embeds the same file.
pub(crate) const LAYER: &str = include_str!("../../../../core/rmux-ui-layer.conf");

const HEADER: &str =
    "# rmux settings, version 1. Written by `rmux-agent settings`; other lines are kept.";
const MAX_BYTES: u64 = 64 * 1024;

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
    Some(base.join("rmux/settings.conf"))
}

/// User configuration files the core reads after the layer, when present,
/// in the core's TMUX_CONF order.
pub(crate) fn tmux_conf_files() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut files = vec![PathBuf::from("/etc/tmux.conf")];
    if let Some(home) = &home {
        files.push(home.join(".tmux.conf"));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        files.push(xdg.join("tmux/tmux.conf"));
    }
    if let Some(home) = &home {
        files.push(home.join(".config/tmux/tmux.conf"));
    }
    let mut unique = Vec::new();
    for file in files {
        if file.is_file() && !unique.contains(&file) {
            unique.push(file);
        }
    }
    unique
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

/// Lines of settings.conf. Managed lines are `set -g @rmux-KEY VALUE`.
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
        let mut words = line.split_whitespace();
        match (words.next(), words.next(), words.next(), words.next()) {
            (Some("set"), Some("-g"), Some(key), Some(value))
                if key.starts_with("@rmux-") && words.next().is_none() =>
            {
                Some((key, value))
            }
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.lines
            .iter()
            .rev()
            .filter_map(|line| Self::managed(line))
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, value)| value)
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

    pub(crate) fn remove_all_managed(&mut self) {
        self.lines.retain(|line| Self::managed(line).is_none());
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

/// Text of one `## rmux:section NAME` block of the layer.
pub(crate) fn section(name: &str) -> Option<String> {
    let start = format!("## rmux:section {name}\n");
    let begin = LAYER.find(&start)? + start.len();
    let end = LAYER[begin..].find("## rmux:end")? + begin;
    Some(LAYER[begin..end].to_owned())
}

/// Names of the layer's sections in order.
#[cfg(test)]
pub(crate) fn section_names() -> Vec<&'static str> {
    LAYER
        .lines()
        .filter_map(|line| line.strip_prefix("## rmux:section "))
        .collect()
}

/// The layer's own `@rmux-*` options that are not saved choices: colours,
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
            && (name.starts_with("@rmux-c-")
                || name == "@rmux-menu-stay-open"
                || name == "@rmux-sidebar-width")
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
            "{HEADER}\n# mine\nset -g @rmux-theme light\nbind x kill-pane\nset -g @rmux-theme dark\n"
        ));
        assert_eq!(saved.get("@rmux-theme"), Some("dark"));
        saved.set("@rmux-theme", "terminal");
        saved.set("@rmux-lang", "ko");
        assert_eq!(
            saved.render(),
            format!(
                "{HEADER}\n# mine\nset -g @rmux-theme terminal\nbind x kill-pane\nset -g @rmux-lang ko\n"
            )
        );
        saved.remove_all_managed();
        assert_eq!(
            saved.render(),
            format!("{HEADER}\n# mine\nbind x kill-pane\n")
        );
    }

    #[test]
    fn every_section_is_found_and_balanced() {
        let names = section_names();
        assert_eq!(names, ["base", "panes", "colors", "styles", "bar", "keys"]);
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
        assert_eq!(status.value, "bg=#{@rmux-c-panel},fg=#{@rmux-c-muted}");
        let base = layer_lines(&section("base").unwrap());
        assert!(
            base.iter().any(|line| line.option == "status-position"
                && line.value.contains("@rmux-status-position"))
        );
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
        let directory = std::env::temp_dir().join(format!("rmux-settings-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("settings.conf");
        let mut saved = Saved::default();
        saved.set("@rmux-theme", "light");
        save(&path, &saved).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(load(&path).unwrap().get("@rmux-theme"), Some("light"));
        let link = directory.join("link.conf");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(load(&link).is_err());
        assert!(save(&link, &saved).is_err());
        fs::remove_dir_all(&directory).unwrap();
    }
}
