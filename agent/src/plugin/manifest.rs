//! Plugin manifest, Omarchy-style theme palette loading, validation, and global
//! registry (`~/.config/masil/plugins.toml`).

use crate::api::scope::Scope;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const PLUGIN_SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 128 * 1024;
const MAX_PLUGINS: usize = 64;

pub const ALLOWED_HOOK_EVENTS: &[&str] = &[
    "agent.state_changed",
    "agent.started",
    "agent.closed",
    "inbox.event",
    "theme.changed",
    "prompt.before",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub active_theme: Option<String>,
    #[serde(default)]
    pub disabled: Vec<String>,
    /// Local developer plugins registered by absolute path (`plugin install /abs/path`).
    #[serde(default)]
    pub linked: BTreeMap<String, String>,
    #[serde(default)]
    pub reload_nonce: u64,
}

fn default_true() -> bool {
    true
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            active_theme: None,
            disabled: Vec::new(),
            linked: BTreeMap::new(),
            reload_nonce: 0,
        }
    }
}

pub fn config_root() -> Result<PathBuf, String> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("masil"));
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config").join("masil"))
}

pub fn state_root() -> Result<PathBuf, String> {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("masil").join("plugins"));
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("masil")
        .join("plugins"))
}

pub fn plugins_dir() -> Result<PathBuf, String> {
    Ok(config_root()?.join("plugins"))
}

pub fn plugins_config_path() -> Result<PathBuf, String> {
    Ok(config_root()?.join("plugins.toml"))
}

fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    Ok(())
}

fn write_atomic_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("cannot open {}: {e}", tmp.display()))?;
    f.write_all(bytes)
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    let _ = f.sync_all();
    fs::rename(&tmp, path).map_err(|e| format!("cannot move {}: {e}", path.display()))
}

impl PluginsConfig {
    pub fn load() -> Result<Self, String> {
        let path = plugins_config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        toml::from_str(&body).map_err(|e| format!("invalid plugins.toml: {e}"))
    }

    pub fn save(&self) -> Result<(), String> {
        let path = plugins_config_path()?;
        let body = toml::to_string_pretty(self)
            .map_err(|e| format!("cannot serialize plugins.toml: {e}"))?;
        write_atomic_private(&path, body.as_bytes())
    }

    pub fn set_plugin_enabled(&mut self, id: &str, enabled: bool) {
        self.disabled.retain(|item| item != id);
        if !enabled {
            self.disabled.push(id.to_owned());
        }
        self.reload_nonce = self.reload_nonce.saturating_add(1);
    }

    pub fn bump_reload(&mut self) {
        self.reload_nonce = self.reload_nonce.saturating_add(1);
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub min_masil_version: Option<String>,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub entry: Vec<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub build: Vec<Vec<String>>,
    #[serde(default)]
    pub themes: Vec<ThemeContribution>,
    #[serde(default)]
    pub panels: Vec<PanelContribution>,
    #[serde(default)]
    pub commands: Vec<CommandContribution>,
    #[serde(default)]
    pub hooks: Vec<HookContribution>,
    #[serde(default)]
    pub presets: Vec<PresetContribution>,
    #[serde(default)]
    pub resident: Option<ResidentConfig>,
}

fn default_schema_version() -> u32 {
    PLUGIN_SCHEMA_VERSION
}

fn default_version() -> String {
    "0.1.0".into()
}

fn default_scope() -> String {
    "read".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeContribution {
    pub id: String,
    pub name: String,
    #[serde(default = "default_dark")]
    pub appearance: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub colors: Option<ThemeColors>,
}

fn default_dark() -> String {
    "dark".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeColors {
    pub canvas: String,
    pub panel: String,
    #[serde(default)]
    pub raised: Option<String>,
    pub text: String,
    #[serde(default)]
    pub muted: Option<String>,
    pub accent: String,
    #[serde(default)]
    pub accent_text: Option<String>,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(default)]
    pub danger: Option<String>,
    #[serde(default)]
    pub success: Option<String>,
    #[serde(default)]
    pub track: Option<String>,
}

/// Fully expanded 11-color palette shared across Rust TUI and Mobile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedTheme {
    pub id: String,
    pub plugin_id: String,
    pub name: String,
    pub appearance: String,
    pub canvas: String,
    pub panel: String,
    pub raised: String,
    pub text: String,
    pub muted: String,
    pub accent: String,
    pub accent_text: String,
    pub warning: String,
    pub danger: String,
    pub success: String,
    pub track: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeFileWrapper {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub appearance: Option<String>,
    #[serde(default)]
    pub colors: Option<ThemeColors>,
    #[serde(flatten)]
    pub flat: Option<ThemeColors>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelContribution {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default = "default_agent_context")]
    pub context: String,
    pub handler: String,
    #[serde(default)]
    pub refresh_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandContribution {
    pub id: String,
    pub slash: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub argument_hint: Option<String>,
    #[serde(default = "default_agent_context")]
    pub context: String,
    pub handler: String,
    #[serde(default)]
    pub icon: Option<String>,
}

fn default_agent_context() -> String {
    "agent".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookContribution {
    pub event: String,
    pub handler: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetContribution {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub worktree: bool,
    #[serde(default)]
    pub initial_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResidentConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub events: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedPlugin {
    pub id: String,
    pub dir: PathBuf,
    pub source: String,
    pub enabled: bool,
    pub manifest: Option<PluginManifest>,
    pub themes: Vec<ResolvedTheme>,
    pub overview: Option<String>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl LoadedPlugin {
    pub fn is_active(&self, global_enabled: bool) -> bool {
        global_enabled && self.enabled && self.errors.is_empty() && self.manifest.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCatalog {
    pub enabled: bool,
    pub active_theme: Option<String>,
    pub reload_nonce: u64,
    pub plugins: Vec<LoadedPlugin>,
}

impl PluginCatalog {
    pub fn active_plugins(&self) -> impl Iterator<Item = &LoadedPlugin> {
        let global = self.enabled;
        self.plugins.iter().filter(move |p| p.is_active(global))
    }

    pub fn all_themes(&self) -> Vec<ResolvedTheme> {
        let mut out = Vec::new();
        for p in self.active_plugins() {
            out.extend(p.themes.iter().cloned());
        }
        out
    }

    pub fn active_resolved_theme(&self) -> Option<ResolvedTheme> {
        let id = self.active_theme.as_deref()?;
        if matches!(id, "dark" | "light" | "system" | "terminal") {
            return None;
        }
        self.all_themes().into_iter().find(|t| t.id == id)
    }

    pub fn find_plugin(&self, id: &str) -> Option<&LoadedPlugin> {
        self.plugins.iter().find(|p| p.id == id)
    }

    pub fn has_hooks_for(&self, event: &str) -> bool {
        self.active_plugins().any(|p| {
            p.manifest
                .as_ref()
                .is_some_and(|m| m.hooks.iter().any(|h| h.event == event))
        })
    }
}

pub fn parse_hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let stripped = hex.trim().strip_prefix('#')?;
    if stripped.len() != 6 || !stripped.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&stripped[0..2], 16).ok()?;
    let g = u8::from_str_radix(&stripped[2..4], 16).ok()?;
    let b = u8::from_str_radix(&stripped[4..6], 16).ok()?;
    Some((r, g, b))
}

fn srgb_channel_luminance(c: u8) -> f64 {
    let s = f64::from(c) / 255.0;
    if s <= 0.03928 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

pub fn relative_luminance(rgb: (u8, u8, u8)) -> f64 {
    0.2126 * srgb_channel_luminance(rgb.0)
        + 0.7152 * srgb_channel_luminance(rgb.1)
        + 0.0722 * srgb_channel_luminance(rgb.2)
}

pub fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn blend_hex(base: (u8, u8, u8), target: (u8, u8, u8), weight: f64) -> String {
    let mix = |a: u8, b: u8| -> u8 {
        ((f64::from(a) * (1.0 - weight) + f64::from(b) * weight).round()).clamp(0.0, 255.0) as u8
    };
    format!(
        "#{:02x}{:02x}{:02x}",
        mix(base.0, target.0),
        mix(base.1, target.1),
        mix(base.2, target.2)
    )
}

fn resolve_theme(
    plugin_id: &str,
    dir: &Path,
    decl: &ThemeContribution,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Option<ResolvedTheme> {
    if !valid_id(&decl.id) {
        errors.push(format!(
            "theme '{}': id must be 1-48 lowercase letters, digits, '-' or '_'",
            decl.id
        ));
        return None;
    }
    if !matches!(decl.appearance.as_str(), "dark" | "light") {
        errors.push(format!(
            "theme '{}': appearance must be 'dark' or 'light'",
            decl.id
        ));
        return None;
    }
    let colors = if let Some(inline) = &decl.colors {
        inline.clone()
    } else if let Some(rel) = &decl.file {
        let path = resolve_safe_subpath(dir, rel);
        let Some(path) = path else {
            errors.push(format!(
                "theme '{}': file '{}' escapes plugin directory",
                decl.id, rel
            ));
            return None;
        };
        let Ok(body) = fs::read_to_string(&path) else {
            errors.push(format!(
                "theme '{}': cannot read {}",
                decl.id,
                path.display()
            ));
            return None;
        };
        match toml::from_str::<ThemeFileWrapper>(&body) {
            Ok(w) => match w.colors.or(w.flat) {
                Some(c) => c,
                None => {
                    errors.push(format!(
                        "theme '{}': {} is missing required color fields (canvas, panel, text, accent)",
                        decl.id, rel
                    ));
                    return None;
                }
            },
            Err(e) => {
                errors.push(format!("theme '{}': invalid {}: {e}", decl.id, rel));
                return None;
            }
        }
    } else {
        errors.push(format!(
            "theme '{}': provide inline [themes.colors] or file = \"theme.toml\"",
            decl.id
        ));
        return None;
    };

    let Some(canvas_rgb) = parse_hex_rgb(&colors.canvas) else {
        errors.push(format!(
            "theme '{}': canvas '{}' must be #RRGGBB",
            decl.id, colors.canvas
        ));
        return None;
    };
    let Some(panel_rgb) = parse_hex_rgb(&colors.panel) else {
        errors.push(format!(
            "theme '{}': panel '{}' must be #RRGGBB",
            decl.id, colors.panel
        ));
        return None;
    };
    let Some(text_rgb) = parse_hex_rgb(&colors.text) else {
        errors.push(format!(
            "theme '{}': text '{}' must be #RRGGBB",
            decl.id, colors.text
        ));
        return None;
    };
    let Some(accent_rgb) = parse_hex_rgb(&colors.accent) else {
        errors.push(format!(
            "theme '{}': accent '{}' must be #RRGGBB",
            decl.id, colors.accent
        ));
        return None;
    };

    let is_dark = decl.appearance == "dark";
    let opt_hex =
        |field: &str, val: &Option<String>, fallback: String, errs: &mut Vec<String>| match val {
            Some(v) => {
                if parse_hex_rgb(v).is_none() {
                    errs.push(format!(
                        "theme '{}': {field} '{v}' must be #RRGGBB",
                        decl.id
                    ));
                    fallback
                } else {
                    v.trim().to_ascii_lowercase()
                }
            }
            None => fallback,
        };

    let default_raised = if is_dark {
        blend_hex(panel_rgb, (255, 255, 255), 0.08)
    } else {
        blend_hex(panel_rgb, (0, 0, 0), 0.07)
    };
    let default_muted = blend_hex(text_rgb, canvas_rgb, 0.36);
    let default_accent_text = if relative_luminance(accent_rgb) > 0.36 {
        "#0c1016".to_owned()
    } else {
        "#ffffff".to_owned()
    };
    let default_warning = if is_dark { "#ebb85c" } else { "#905400" }.to_owned();
    let default_danger = if is_dark { "#e86f75" } else { "#aa2a34" }.to_owned();
    let default_success = if is_dark { "#74c48a" } else { "#167036" }.to_owned();
    let default_track = blend_hex(panel_rgb, text_rgb, 0.16);

    let raised = opt_hex("raised", &colors.raised, default_raised, errors);
    let muted = opt_hex("muted", &colors.muted, default_muted, errors);
    let accent_text = opt_hex(
        "accent_text",
        &colors.accent_text,
        default_accent_text,
        errors,
    );
    let warning = opt_hex("warning", &colors.warning, default_warning, errors);
    let danger = opt_hex("danger", &colors.danger, default_danger, errors);
    let success = opt_hex("success", &colors.success, default_success, errors);
    let track = opt_hex("track", &colors.track, default_track, errors);

    let text_panel_contrast = contrast_ratio(text_rgb, panel_rgb);
    if text_panel_contrast < 4.5 {
        warnings.push(format!(
            "theme '{}': text/panel contrast ratio is {:.2}:1 (recommend >= 4.5:1)",
            decl.id, text_panel_contrast
        ));
    }
    if let Some(at_rgb) = parse_hex_rgb(&accent_text) {
        let btn_contrast = contrast_ratio(at_rgb, accent_rgb);
        if btn_contrast < 3.0 {
            warnings.push(format!(
                "theme '{}': accent_text/accent contrast ratio is {:.2}:1 (recommend >= 4.5:1)",
                decl.id, btn_contrast
            ));
        }
    }

    Some(ResolvedTheme {
        id: decl.id.clone(),
        plugin_id: plugin_id.to_owned(),
        name: decl.name.clone(),
        appearance: decl.appearance.clone(),
        canvas: colors.canvas.trim().to_ascii_lowercase(),
        panel: colors.panel.trim().to_ascii_lowercase(),
        raised,
        text: colors.text.trim().to_ascii_lowercase(),
        muted,
        accent: colors.accent.trim().to_ascii_lowercase(),
        accent_text,
        warning,
        danger,
        success,
        track,
    })
}

pub fn resolve_safe_subpath(dir: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(dir.join(rel_path))
}

/// Resolves a plugin's `entry` argv relative to `plugin_dir`. If an argument is
/// a relative path to an existing file inside `plugin_dir`, converts it to an
/// absolute path. Bare commands such as `node` or `python3` pass through to PATH.
pub fn resolve_entry_argv(dir: &Path, entry: &[String]) -> Result<Vec<String>, String> {
    if entry.is_empty() {
        return Err("plugin has no entry command configured".into());
    }
    let mut resolved = Vec::with_capacity(entry.len());
    for (idx, part) in entry.iter().enumerate() {
        if part.is_empty() {
            return Err("entry argv contains an empty string".into());
        }
        let p = Path::new(part);
        if p.is_absolute() {
            resolved.push(part.clone());
        } else if idx == 0 && (part.starts_with("./") || part.contains('/')) {
            let Some(joined) = resolve_safe_subpath(dir, part) else {
                return Err(format!("entry '{part}' escapes plugin directory"));
            };
            resolved.push(joined.to_string_lossy().into_owned());
        } else if idx > 0 && !part.starts_with('-') {
            if let Some(joined) = resolve_safe_subpath(dir, part)
                && joined.exists()
            {
                resolved.push(joined.to_string_lossy().into_owned());
            } else {
                resolved.push(part.clone());
            }
        } else {
            resolved.push(part.clone());
        }
    }
    Ok(resolved)
}

pub fn load_plugin_dir(
    dir: &Path,
    override_id: Option<&str>,
    source: &str,
    disabled_ids: &HashSet<String>,
) -> LoadedPlugin {
    let fallback_id = override_id
        .map(str::to_owned)
        .or_else(|| dir.file_name().and_then(|n| n.to_str()).map(str::to_owned))
        .unwrap_or_else(|| "unknown".into());
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let manifest_path = dir.join("masil-plugin.toml");

    if !dir.is_dir() {
        errors.push(format!(
            "plugin directory does not exist: {}",
            dir.display()
        ));
        return LoadedPlugin {
            id: fallback_id.clone(),
            dir: dir.to_path_buf(),
            source: source.to_owned(),
            enabled: !disabled_ids.contains(&fallback_id),
            manifest: None,
            themes: Vec::new(),
            overview: None,
            errors,
            warnings,
        };
    }

    let meta = match fs::metadata(&manifest_path) {
        Ok(m) => m,
        Err(_) => {
            errors.push(format!("missing {}", manifest_path.display()));
            return LoadedPlugin {
                id: fallback_id.clone(),
                dir: dir.to_path_buf(),
                source: source.to_owned(),
                enabled: !disabled_ids.contains(&fallback_id),
                manifest: None,
                themes: Vec::new(),
                overview: None,
                errors,
                warnings,
            };
        }
    };
    if meta.len() > MAX_MANIFEST_BYTES {
        errors.push("masil-plugin.toml exceeds 128 KiB".into());
        return LoadedPlugin {
            id: fallback_id.clone(),
            dir: dir.to_path_buf(),
            source: source.to_owned(),
            enabled: !disabled_ids.contains(&fallback_id),
            manifest: None,
            themes: Vec::new(),
            overview: None,
            errors,
            warnings,
        };
    }

    let body = match fs::read_to_string(&manifest_path) {
        Ok(b) => b,
        Err(e) => {
            errors.push(format!("cannot read masil-plugin.toml: {e}"));
            return LoadedPlugin {
                id: fallback_id.clone(),
                dir: dir.to_path_buf(),
                source: source.to_owned(),
                enabled: !disabled_ids.contains(&fallback_id),
                manifest: None,
                themes: Vec::new(),
                overview: None,
                errors,
                warnings,
            };
        }
    };

    let mut manifest: PluginManifest = match toml::from_str(&body) {
        Ok(m) => m,
        Err(e) => {
            errors.push(format!("invalid masil-plugin.toml: {e}"));
            return LoadedPlugin {
                id: fallback_id.clone(),
                dir: dir.to_path_buf(),
                source: source.to_owned(),
                enabled: !disabled_ids.contains(&fallback_id),
                manifest: None,
                themes: Vec::new(),
                overview: None,
                errors,
                warnings,
            };
        }
    };

    if let Some(id) = override_id {
        manifest.id = id.to_owned();
    }
    let id = manifest.id.clone();

    if manifest.schema_version != PLUGIN_SCHEMA_VERSION {
        errors.push(format!(
            "unsupported schema_version {} (supported: {PLUGIN_SCHEMA_VERSION})",
            manifest.schema_version
        ));
    }
    if !valid_id(&manifest.id) {
        errors.push(
            "id must be 1-48 lowercase letters, digits, '-' or '_' starting with a letter".into(),
        );
    }
    if manifest.name.trim().is_empty() {
        errors.push("name must not be empty".into());
    }
    if Scope::parse(&manifest.scope).is_err() {
        errors.push("scope must be read, act, or admin".into());
    }
    if manifest
        .timeout_secs
        .is_some_and(|secs| !(1..=300).contains(&secs))
    {
        errors.push("timeout_secs must be between 1 and 300".into());
    }

    let needs_entry = !manifest.panels.is_empty()
        || !manifest.commands.is_empty()
        || !manifest.hooks.is_empty()
        || manifest.resident.as_ref().is_some_and(|r| r.enabled);
    if needs_entry && manifest.entry.is_empty() {
        errors.push(
            "entry is required when declaring panels, commands, hooks, or a resident worker".into(),
        );
    } else if !manifest.entry.is_empty() {
        match resolve_entry_argv(dir, &manifest.entry) {
            Ok(argv) => {
                // Verify script target exists when second arg looks like a local script file.
                if let Some(first) = argv.first()
                    && Path::new(first).is_absolute()
                    && !Path::new(first).exists()
                {
                    errors.push(format!("entry executable not found: {first}"));
                }
                if let Some(script) = manifest.entry.get(1)
                    && !script.starts_with('-')
                    && (script.ends_with(".mjs")
                        || script.ends_with(".js")
                        || script.ends_with(".ts")
                        || script.ends_with(".py")
                        || script.ends_with(".sh"))
                    && let Some(sub) = resolve_safe_subpath(dir, script)
                    && !sub.exists()
                {
                    errors.push(format!("entry script not found: {}", sub.display()));
                }
            }
            Err(e) => errors.push(e),
        }
    }

    let mut seen_panels = HashSet::new();
    for panel in &manifest.panels {
        if !valid_id(&panel.id) || !seen_panels.insert(panel.id.clone()) {
            errors.push(format!("panel '{}': duplicate or invalid id", panel.id));
        }
        if !matches!(panel.context.as_str(), "agent" | "global") {
            errors.push(format!(
                "panel '{}': context must be 'agent' or 'global'",
                panel.id
            ));
        }
        if panel.handler.trim().is_empty() {
            errors.push(format!("panel '{}': handler must not be empty", panel.id));
        }
    }

    let mut seen_cmds = HashSet::new();
    for cmd in &manifest.commands {
        if !valid_id(&cmd.id) || !seen_cmds.insert(cmd.id.clone()) {
            errors.push(format!("command '{}': duplicate or invalid id", cmd.id));
        }
        let slash = cmd.slash.trim_start_matches('/');
        if !valid_id(slash) {
            errors.push(format!(
                "command '{}': slash '{}' must be lowercase letters, digits, '-' or '_'",
                cmd.id, cmd.slash
            ));
        }
        if !matches!(cmd.context.as_str(), "agent" | "global") {
            errors.push(format!(
                "command '{}': context must be 'agent' or 'global'",
                cmd.id
            ));
        }
        if cmd.handler.trim().is_empty() {
            errors.push(format!("command '{}': handler must not be empty", cmd.id));
        }
    }

    for hook in &manifest.hooks {
        if !ALLOWED_HOOK_EVENTS.contains(&hook.event.as_str()) {
            errors.push(format!(
                "hook '{}': unsupported event (allowed: {})",
                hook.event,
                ALLOWED_HOOK_EVENTS.join(", ")
            ));
        }
        if hook.handler.trim().is_empty() {
            errors.push(format!("hook '{}': handler must not be empty", hook.event));
        }
    }

    let mut seen_presets = HashSet::new();
    for preset in &manifest.presets {
        if !valid_id(&preset.id) || !seen_presets.insert(preset.id.clone()) {
            errors.push(format!("preset '{}': duplicate or invalid id", preset.id));
        }
        if preset.provider.trim().is_empty() {
            errors.push(format!(
                "preset '{}': provider must not be empty",
                preset.id
            ));
        }
    }

    let mut themes = Vec::new();
    let mut seen_themes = HashSet::new();
    for t in &manifest.themes {
        if !seen_themes.insert(t.id.clone()) {
            errors.push(format!("duplicate theme id '{}'", t.id));
            continue;
        }
        if let Some(resolved) = resolve_theme(&id, dir, t, &mut errors, &mut warnings) {
            themes.push(resolved);
        }
    }

    let overview = fs::read_to_string(dir.join("OVERVIEW.md"))
        .ok()
        .map(|s| s.trim().chars().take(4096).collect::<String>())
        .filter(|s| !s.is_empty());

    LoadedPlugin {
        id: id.clone(),
        dir: dir.to_path_buf(),
        source: source.to_owned(),
        enabled: !disabled_ids.contains(&id),
        manifest: Some(manifest),
        themes,
        overview,
        errors,
        warnings,
    }
}

pub fn load_catalog() -> Result<PluginCatalog, String> {
    let config = PluginsConfig::load()?;
    let disabled: HashSet<String> = config.disabled.iter().cloned().collect();
    let mut plugins = Vec::new();
    let mut seen = HashSet::new();

    // 1. Linked local developer folders first (explicit user install takes precedence).
    for (id, raw_path) in &config.linked {
        if plugins.len() >= MAX_PLUGINS {
            break;
        }
        let dir = PathBuf::from(raw_path);
        let loaded = load_plugin_dir(&dir, Some(id), &format!("local:{raw_path}"), &disabled);
        seen.insert(loaded.id.clone());
        plugins.push(loaded);
    }

    // 2. Installed folders under ~/.config/masil/plugins/<id>/.
    if let Ok(root) = plugins_dir()
        && root.is_dir()
        && let Ok(entries) = fs::read_dir(&root)
    {
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            if plugins.len() >= MAX_PLUGINS {
                break;
            }
            let Some(folder_name) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if folder_name.starts_with('.') {
                continue;
            }
            if seen.contains(folder_name) {
                continue;
            }
            let loaded = load_plugin_dir(&dir, None, "installed", &disabled);
            if seen.insert(loaded.id.clone()) {
                plugins.push(loaded);
            }
        }
    }

    Ok(PluginCatalog {
        enabled: config.enabled,
        active_theme: config.active_theme,
        reload_nonce: config.reload_nonce,
        plugins,
    })
}

fn file_mtime_token(path: &Path) -> String {
    match fs::metadata(path) {
        Ok(m) => {
            let ms = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis())
                .unwrap_or(0);
            format!("{}:{ms}", m.len())
        }
        Err(_) => "0".into(),
    }
}

fn dir_fingerprint(dir: &Path, depth: usize, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        out.push("missing".into());
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.')
            || matches!(
                name,
                "node_modules" | "__pycache__" | "target" | ".git" | "dist"
            )
        {
            continue;
        }
        if path.is_file() {
            out.push(format!("{name}={}", file_mtime_token(&path)));
        } else if depth > 0 && path.is_dir() {
            let mut sub = Vec::new();
            dir_fingerprint(&path, depth - 1, &mut sub);
            if !sub.is_empty() {
                out.push(format!("{name}/[{}]", sub.join(",")));
            }
        }
    }
}

/// Fast fingerprint for hot-reloading in remote daemon and TUI.
/// Covers `plugins.toml`, every file inside linked and installed plugin
/// folders (up to 1 subdirectory deep, including custom theme TOMLs and
/// handler scripts), and AI skill installation status.
pub fn catalog_fingerprint() -> String {
    let mut parts = Vec::new();
    if let Ok(cfg_path) = plugins_config_path() {
        parts.push(format!("cfg={}", file_mtime_token(&cfg_path)));
    }
    if let Ok(cfg) = PluginsConfig::load() {
        parts.push(format!(
            "n={}:e={}:t={}:dis={}",
            cfg.reload_nonce,
            cfg.enabled,
            cfg.active_theme.as_deref().unwrap_or(""),
            cfg.disabled.join(",")
        ));
        for (id, raw) in &cfg.linked {
            let p = Path::new(raw);
            let mut files = Vec::new();
            dir_fingerprint(p, 1, &mut files);
            parts.push(format!("l:{id}=[{}]", files.join(",")));
        }
    }
    if let Ok(root) = plugins_dir()
        && let Ok(entries) = fs::read_dir(&root)
    {
        let mut names: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        names.sort();
        for dir in names {
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if name.starts_with('.') {
                continue;
            }
            let mut files = Vec::new();
            dir_fingerprint(&dir, 1, &mut files);
            parts.push(format!("d:{name}=[{}]", files.join(",")));
        }
    }
    let skills_fp: Vec<String> = super::skill::inspect_all()
        .into_iter()
        .map(|s| format!("{}:{}", s.id, u8::from(s.skill_installed)))
        .collect();
    parts.push(format!("sk={}", skills_fp.join(",")));
    parts.join("|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_theme_toml_and_checks_contrast() {
        let dir = std::env::temp_dir().join(format!("masil-theme-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("theme.toml"),
            r##"
canvas = "#1e1e2e"
panel = "#181825"
text = "#cdd6f4"
accent = "#cba6f7"
"##,
        )
        .unwrap();
        fs::write(
            dir.join("masil-plugin.toml"),
            r#"
schema_version = 1
id = "catppuccin-mocha"
name = "Catppuccin Mocha"
version = "0.1.0"

[[themes]]
id = "catppuccin-mocha"
name = "Catppuccin Mocha"
appearance = "dark"
file = "theme.toml"
"#,
        )
        .unwrap();

        let loaded = load_plugin_dir(&dir, None, "test", &HashSet::new());
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(loaded.themes.len(), 1);
        assert_eq!(loaded.themes[0].canvas, "#1e1e2e");
        assert!(parse_hex_rgb(&loaded.themes[0].raised).is_some());
        assert!(parse_hex_rgb(&loaded.themes[0].accent_text).is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_invalid_hex_and_escaping_theme_path() {
        let dir = std::env::temp_dir().join(format!("masil-theme-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("masil-plugin.toml"),
            r#"
schema_version = 1
id = "bad-theme"
name = "Bad"

[[themes]]
id = "bad"
name = "Bad"
appearance = "dark"
file = "../etc/passwd"
"#,
        )
        .unwrap();

        let loaded = load_plugin_dir(&dir, None, "test", &HashSet::new());
        assert!(!loaded.errors.is_empty());
        assert!(loaded.errors[0].contains("escapes plugin directory"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dir_fingerprint_tracks_handler_scripts_and_custom_theme_files() {
        let dir = std::env::temp_dir().join(format!("masil-fp-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("themes")).unwrap();
        fs::write(dir.join("masil-plugin.toml"), "id = \"demo\"\n").unwrap();
        fs::write(dir.join("plugin.mjs"), "console.log('v1');\n").unwrap();
        fs::write(dir.join("themes/nord.toml"), "accent = \"#88c0d0\"\n").unwrap();

        let mut fp1 = Vec::new();
        dir_fingerprint(&dir, 1, &mut fp1);
        assert!(fp1.iter().any(|s| s.starts_with("plugin.mjs=")), "{fp1:?}");
        assert!(
            fp1.iter().any(|s| s.starts_with("themes/[nord.toml=")),
            "{fp1:?}"
        );

        fs::write(dir.join("plugin.mjs"), "console.log('v2-longer');\n").unwrap();
        let mut fp2 = Vec::new();
        dir_fingerprint(&dir, 1, &mut fp2);
        assert_ne!(fp1, fp2);

        fs::write(
            dir.join("themes/nord.toml"),
            "accent = \"#81a1c1-updated\"\n",
        )
        .unwrap();
        let mut fp3 = Vec::new();
        dir_fingerprint(&dir, 1, &mut fp3);
        assert_ne!(fp2, fp3);

        let _ = fs::remove_dir_all(&dir);
    }
}
