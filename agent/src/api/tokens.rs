//! API tokens (P9, docs/extensions.md): for connections a person sets up,
//! such as an SSH forced command. A token file and the registry of its hash
//! share one private directory, so a forced command finds both from the
//! file's path alone, whatever its environment. A connection holds the
//! token's value and checks its hash, not its name: a token made again
//! under an old name is a new one.

use super::scope::Scope;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAX_TOKENS: usize = 32;
const REGISTRY: &str = "registry.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    name: String,
    scope: String,
    sha256: String,
    created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revoked_ms: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    tokens: Vec<Entry>,
}

/// A token a connection came with.
pub(crate) struct Held {
    pub(crate) name: String,
    pub(crate) scope: Scope,
    directory: PathBuf,
    sha256: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `~/.config/masil/api-tokens` (or under `XDG_CONFIG_HOME`), private.
fn directory() -> Result<PathBuf, String> {
    let root = crate::managed::endpoints::config_root()
        .ok_or("api_token: no configuration directory (HOME)")?;
    crate::managed::private_directory(&root.join("masil/api-tokens"))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.as_bytes()[0].is_ascii_lowercase()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

fn private_read(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("api_token: {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("api_token: {error}"))?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(format!(
            "api_token: {} must be a private file of yours",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("api_token: {error}"))?;
    Ok(bytes)
}

fn read_registry(directory: &Path) -> Result<Registry, String> {
    let path = directory.join(REGISTRY);
    if std::fs::symlink_metadata(&path).is_err() {
        return Ok(Registry::default());
    }
    let bytes = private_read(&path, 256 * 1024)?;
    serde_json::from_slice(&bytes).map_err(|error| format!("api_token: registry: {error}"))
}

/// Changes the registry under its lock, written whole and renamed in.
fn change<T>(work: impl FnOnce(&mut Registry, &Path) -> Result<T, String>) -> Result<T, String> {
    let directory = directory()?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join(".lock"))
        .map_err(|error| format!("api_token: lock: {error}"))?;
    // SAFETY: flock on an open descriptor; released when it closes.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err("api_token: lock".into());
    }
    let mut registry = read_registry(&directory)?;
    let result = work(&mut registry, &directory)?;
    let temporary = directory.join(format!(".registry-{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)
        .map_err(|error| format!("api_token: {error}"))?;
    let bytes = serde_json::to_vec_pretty(&registry).map_err(|error| error.to_string())?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("api_token: {error}"))?;
    std::fs::rename(&temporary, directory.join(REGISTRY))
        .map_err(|error| format!("api_token: {error}"))?;
    Ok(result)
}

/// A new token: its file, readable only by you, and its hash on the books.
pub(crate) fn create(name: &str, scope: Scope) -> Result<Value, String> {
    if !valid_name(name) {
        return Err("invalid_argument: a token name begins with a lowercase letter and uses 1-32 lowercase letters, digits, '-' or '_'".into());
    }
    change(|registry, directory| {
        if registry
            .tokens
            .iter()
            .any(|entry| entry.name == name && entry.revoked_ms.is_none())
        {
            return Err(format!(
                "api_token: a token named {name} exists; revoke it first"
            ));
        }
        if registry
            .tokens
            .iter()
            .filter(|entry| entry.revoked_ms.is_none())
            .count()
            >= MAX_TOKENS
        {
            return Err("api_token: at most 32 tokens".into());
        }
        let mut value = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut random| random.read_exact(&mut value))
            .map_err(|error| format!("api_token: {error}"))?;
        let value = hex(&value);
        let path = directory.join(name);
        let _ = std::fs::remove_file(&path);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| format!("api_token: {error}"))?;
        file.write_all(value.as_bytes())
            .map_err(|error| format!("api_token: {error}"))?;
        registry.tokens.retain(|entry| entry.name != name);
        registry.tokens.push(Entry {
            name: name.to_owned(),
            scope: scope.name().to_owned(),
            sha256: hex(&Sha256::digest(value.as_bytes())),
            created_ms: crate::observation::now_ms(),
            revoked_ms: None,
        });
        Ok(json!({"name": name, "scope": scope.name(), "file": path}))
    })
}

pub(crate) fn list() -> Result<Value, String> {
    let registry = read_registry(&directory()?)?;
    let tokens: Vec<Value> = registry
        .tokens
        .iter()
        .map(|entry| {
            json!({
                "name": entry.name,
                "scope": entry.scope,
                "created_ms": entry.created_ms,
                "revoked_ms": entry.revoked_ms,
            })
        })
        .collect();
    Ok(json!({"tokens": tokens}))
}

/// Revoked: its file goes and its hash stays on the books as revoked.
pub(crate) fn revoke(name: &str) -> Result<Value, String> {
    change(|registry, directory| {
        let entry = registry
            .tokens
            .iter_mut()
            .find(|entry| entry.name == name && entry.revoked_ms.is_none())
            .ok_or_else(|| format!("target_absent: no token named {name}"))?;
        entry.revoked_ms = Some(crate::observation::now_ms());
        let _ = std::fs::remove_file(directory.join(name));
        Ok(json!({"name": name, "revoked": true}))
    })
}

/// The token in `file` (an absolute path), found on the books beside it.
pub(crate) fn load(file: &Path) -> Result<Held, String> {
    if !file.is_absolute() {
        return Err("api_token: the token file is an absolute path".into());
    }
    let directory = file
        .parent()
        .ok_or("api_token: the token file has no directory")?
        .to_owned();
    let value = private_read(file, 128)?;
    let sha256 = hex(&Sha256::digest(&value));
    let registry = read_registry(&directory)?;
    let entry = registry
        .tokens
        .iter()
        .find(|entry| entry.sha256 == sha256 && entry.revoked_ms.is_none())
        .ok_or("api_token: the token is revoked or unknown")?;
    Ok(Held {
        name: entry.name.clone(),
        scope: Scope::parse(&entry.scope)?,
        directory,
        sha256,
    })
}

/// Whether the held token is still on the books.
pub(crate) fn alive(held: &Held) -> Result<(), String> {
    let registry = read_registry(&held.directory)?;
    registry
        .tokens
        .iter()
        .any(|entry| entry.sha256 == held.sha256 && entry.revoked_ms.is_none())
        .then_some(())
        .ok_or_else(|| format!("api_token: token {} was revoked", held.name))
}
