use super::model::{Language, Theme};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u8,
    language: String,
    theme: String,
}

pub(super) struct Preferences {
    path: Option<PathBuf>,
    pub language: Language,
    pub theme: Theme,
    pub warning: Option<String>,
}

impl Preferences {
    pub fn load() -> Self {
        let path = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
            .filter(|p| p.is_absolute())
            .map(|p| p.join("masil/ui.json"));
        Self::from_path(path)
    }

    fn from_path(path: Option<PathBuf>) -> Self {
        let mut result = Self {
            path,
            language: Language::English,
            theme: Theme::Dark,
            warning: None,
        };
        if let Some(path) = &result.path {
            let loaded = (|| -> Result<Option<Stored>, String> {
                let file = match OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(path)
                {
                    Ok(file) => file,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(_) => {
                        return Err(
                            "Cannot read UI preferences; changes apply to this session".into()
                        );
                    }
                };
                if !file.metadata().map_err(|e| e.to_string())?.is_file() {
                    return Err("UI preferences are not a regular file".into());
                }
                let mut bytes = Vec::new();
                file.take(16385)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                if bytes.len() > 16384 {
                    return Err("UI preference file is too large".into());
                }
                let stored: Stored = serde_json::from_slice(&bytes)
                    .map_err(|_| "Invalid UI preferences; changes apply to this session")?;
                if stored.version != 1 {
                    return Err("Unsupported UI preferences; changes apply to this session".into());
                }
                Ok(Some(stored))
            })();
            match loaded {
                Ok(Some(stored)) => match (stored.language.parse(), stored.theme.parse()) {
                    (Ok(language), Ok(theme)) => {
                        result.language = language;
                        result.theme = theme;
                    }
                    _ => {
                        result.warning =
                            Some("Invalid UI preferences; changes apply to this session".into());
                        result.path = None;
                    }
                },
                Err(message) => {
                    result.warning = Some(message);
                    result.path = None;
                }
                Ok(None) => {}
            }
        }
        result
    }

    pub fn save(&self, language: Language, theme: Theme) -> Result<(), String> {
        let path = self
            .path
            .as_ref()
            .ok_or("Preferences apply to this session only")?;
        let parent = path.parent().ok_or("Invalid preference directory")?;
        fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create UI preference directory: {e}"))?;
        // Only this app's directory is chmodded. Parent XDG directories are untouched.
        let metadata = fs::symlink_metadata(parent).map_err(|e| e.to_string())?;
        if !metadata.is_dir() {
            return Err("UI preference directory is not a directory".into());
        }
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let temp = parent.join(format!(".ui-{}.tmp", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|e| e.to_string())?;
        let result = (|| {
            let body = serde_json::to_vec_pretty(&Stored {
                version: 1,
                language: language.as_str().into(),
                theme: theme.as_str().into(),
            })
            .map_err(|e| e.to_string())?;
            file.write_all(&body).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            fs::rename(&temp, path).map_err(|e| e.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_preferences_are_not_overwritten() {
        let root = std::env::temp_dir().join(format!("masil-ui-prefs-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("ui.json");
        fs::write(&path, b"{\"version\":999}").unwrap();
        let prefs = Preferences::from_path(Some(path.clone()));
        assert!(prefs.warning.is_some());
        assert!(prefs.save(Language::Korean, Theme::Light).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{\"version\":999}");
        fs::remove_file(&path).unwrap();
        let prefs = Preferences::from_path(Some(path.clone()));
        prefs.save(Language::Korean, Theme::Light).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            Preferences::from_path(Some(path)).language,
            Language::Korean
        );
        fs::remove_dir_all(root).unwrap();
    }
}
