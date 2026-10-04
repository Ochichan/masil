//! Bounded, manifest-driven terminal-screen detection for coding agents.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::providers;

const ENGINE_VERSION: u32 = 4;
const MAX_MANIFEST_BYTES: usize = 512 * 1024;
const MAX_SCREEN_BYTES: usize = 1024 * 1024;
const MAX_REGION_CACHE_BYTES: usize = MAX_SCREEN_BYTES;
const MAX_TITLE_BYTES: usize = 8192;
const MAX_RULES_PER_MANIFEST: usize = 128;
const MAX_GATE_DEPTH: usize = 8;
const MAX_TOTAL_GATES: usize = 512;
const MAX_MATCHERS_PER_GATE: usize = 32;
const MAX_TOTAL_MATCHERS: usize = 1024;
const MAX_MATCHER_CHARS: usize = 512;
const MAX_REGION_LINES: usize = u16::MAX as usize;

const BUNDLED_MANIFESTS: &[(&str, &str)] = &[
    ("amp", include_str!("manifests/amp.toml")),
    ("agy", include_str!("manifests/antigravity.toml")),
    ("claude", include_str!("manifests/claude.toml")),
    ("cline", include_str!("manifests/cline.toml")),
    ("codex", include_str!("manifests/codex.toml")),
    ("cursor", include_str!("manifests/cursor.toml")),
    ("devin", include_str!("manifests/devin.toml")),
    ("droid", include_str!("manifests/droid.toml")),
    ("gemini", include_str!("manifests/gemini.toml")),
    ("copilot", include_str!("manifests/github-copilot.toml")),
    ("grok", include_str!("manifests/grok.toml")),
    ("hermes", include_str!("manifests/hermes.toml")),
    ("kilo", include_str!("manifests/kilo.toml")),
    ("kimi", include_str!("manifests/kimi.toml")),
    ("kiro", include_str!("manifests/kiro.toml")),
    ("letta", include_str!("manifests/letta.toml")),
    ("maki", include_str!("manifests/maki.toml")),
    ("muse", include_str!("manifests/muse.toml")),
    ("opencode", include_str!("manifests/opencode.toml")),
    ("pi", include_str!("manifests/pi.toml")),
    ("qodercli", include_str!("manifests/qodercli.toml")),
    ("qwen", include_str!("manifests/qwen.toml")),
];

/// Immutable detection engine. Call `load` again to pick up override changes.
#[derive(Debug)]
pub struct Engine {
    manifests: HashMap<&'static str, ManifestEntry>,
}

impl Engine {
    /// Registers the 22 bundled manifests and loads any local masil overrides.
    ///
    /// Overrides live in `$XDG_CONFIG_HOME/masil/agent-detection`, falling
    /// back to `~/.config/masil/agent-detection`. A present invalid override
    /// fails the load rather than silently weakening detection. Bundled
    /// manifests are compiled when first used by this engine.
    pub fn load() -> Result<Self, String> {
        Self::load_with_override_directory(override_directory())
    }

    fn load_with_override_directory(directory: Option<PathBuf>) -> Result<Self, String> {
        let mut engine = Self::load_bundled();
        let Some(directory) = directory else {
            return Ok(engine);
        };
        for (id, _) in BUNDLED_MANIFESTS {
            let path = directory.join(format!("{id}.toml"));
            if !path.exists() {
                continue;
            }
            let bytes = read_override(&path)?;
            let text = std::str::from_utf8(&bytes)
                .map_err(|error| format!("override {} is not UTF-8: {error}", path.display()))?;
            let loaded = load_manifest(text, ManifestSource::Override(path.clone()))
                .map_err(|error| format!("invalid override {}: {error}", path.display()))?;
            if providers::find(&loaded.manifest.id).map(|provider| provider.id) != Some(*id)
                && !loaded
                    .manifest
                    .aliases
                    .iter()
                    .any(|alias| providers::find(alias).map(|provider| provider.id) == Some(*id))
            {
                return Err(format!(
                    "invalid override {}: manifest id {} does not match {id}",
                    path.display(),
                    loaded.manifest.id
                ));
            }
            engine.manifests.insert(*id, ManifestEntry::Loaded(loaded));
        }
        Ok(engine)
    }

    /// The keys that interrupt this provider's turn; empty when masil has
    /// not measured them.
    pub fn interrupt_keys(&self, provider: &str) -> Vec<String> {
        providers::find(provider)
            .and_then(|provider| self.manifests.get(provider.id))
            .and_then(|manifest| manifest.loaded().ok())
            .map(|loaded| loaded.manifest.interrupt_keys.clone())
            .unwrap_or_default()
    }

    /// Whether this provider's manifest can tell an idle screen.
    pub fn has_idle_rule(&self, provider: &str) -> bool {
        providers::find(provider)
            .and_then(|provider| self.manifests.get(provider.id))
            .and_then(|manifest| manifest.loaded().ok())
            .is_some_and(|loaded| {
                loaded
                    .manifest
                    .rules
                    .iter()
                    .any(|rule| rule.state == State::Idle)
            })
    }

    /// Whether this provider's rule of that id marks a turn the interrupt
    /// keys end.
    pub fn interruptible_rule(&self, provider: &str, rule: &str) -> bool {
        providers::find(provider)
            .and_then(|provider| self.manifests.get(provider.id))
            .and_then(|manifest| manifest.loaded().ok())
            .is_some_and(|loaded| {
                loaded
                    .manifest
                    .rules
                    .iter()
                    .any(|candidate| candidate.id == rule && candidate.interruptible)
            })
    }

    /// Whether screen detection has rules for this provider.
    pub fn has_manifest(&self, provider: &str) -> bool {
        providers::find(provider).is_some_and(|provider| self.manifests.contains_key(provider.id))
    }

    /// Explains the highest-priority rule matched by the current screen.
    #[cfg(test)]
    pub fn explain(&self, provider: &str, screen: &str, title: &str) -> Value {
        self.explain_with_progress(provider, screen, title, "")
    }

    pub fn explain_with_progress(
        &self,
        provider: &str,
        screen: &str,
        title: &str,
        progress: &str,
    ) -> Value {
        let Some(provider) = providers::find(provider) else {
            return unknown_explanation(provider, "unknown_provider", None, Vec::new());
        };
        let Some(manifest) = self.manifests.get(provider.id) else {
            return unknown_explanation(provider.id, "manifest_unavailable", None, Vec::new());
        };
        let loaded = match manifest.loaded() {
            Ok(loaded) => loaded,
            Err(error) => return unavailable_explanation(provider.id, error),
        };
        if screen.len() > MAX_SCREEN_BYTES || title.len() > MAX_TITLE_BYTES || progress.len() > 31 {
            return unknown_explanation(
                provider.id,
                "input_limit_exceeded",
                Some(loaded),
                Vec::new(),
            );
        }

        let input = DetectionInput {
            screen,
            osc_title: title,
            osc_progress: progress,
        };
        let mut matched: Option<(&ManifestRule, &str)> = None;
        let mut explanations = Vec::with_capacity(loaded.manifest.rules.len());
        let mut regions = HashMap::new();
        let mut region_cache_bytes = 0;
        for (rule, compiled) in loaded.manifest.rules.iter().zip(&loaded.compiled_rules) {
            let region_text = regions
                .entry(rule.region.trim())
                .or_insert_with(|| RegionText::new(region(input, &rule.region)));
            let previous_lower_bytes = region_text.lower.get().map_or(0, String::len);
            let rule_matched = compiled_gate_matches(&compiled.gate, region_text);
            region_cache_bytes +=
                region_text.lower.get().map_or(0, String::len) - previous_lower_bytes;
            explanations.push(json!({
                "id": rule.id,
                "priority": rule.priority,
                "region": rule.region,
                "state": rule.state.label(),
                "matched": rule_matched,
                "evidence": {
                    "contains": rule.contains,
                    "regex": rule.regex,
                    "line_regex": rule.line_regex,
                    "all_count": rule.all.len(),
                    "any_count": rule.any.len(),
                    "not_count": rule.not_gate.len(),
                    "region_bytes": region_text.text.len(),
                    "region_preview": region_text.preview,
                },
            }));
            if rule_matched
                && matched
                    .as_ref()
                    .is_none_or(|(previous, _)| rule.priority > previous.priority)
            {
                matched = Some((rule, region_text.text));
            }
            // Custom manifests can request many overlapping large regions.
            // Retain at most this budget between rules, plus the active region.
            if region_cache_bytes > MAX_REGION_CACHE_BYTES {
                regions.clear();
                region_cache_bytes = 0;
            }
        }

        let Some((rule, _)) = matched else {
            return unknown_explanation(
                provider.id,
                "no_matching_rule",
                Some(loaded),
                explanations,
            );
        };
        let state = rule.state;
        // Any blocker on the screen, whichever rule won: a key meant for a
        // turn would answer it.
        let blocked_rule_matched = explanations
            .iter()
            .any(|rule| rule["matched"] == true && rule["state"] == "blocked");
        let matched_rule = json!({
            "id": rule.id,
            "priority": rule.priority,
            "region": rule.region,
            "state": state.label(),
        });
        json!({
            "provider": provider.id,
            "agent": provider.id,
            "state": state.label(),
            "source": loaded.source.label(),
            "manifest_source": loaded.source.label(),
            "manifest_version": loaded.manifest.version,
            "matched_rule": matched_rule,
            "visible_idle": rule.visible_idle && state == State::Idle,
            "visible_blocker": rule.visible_blocker && state == State::Blocked,
            "visible_working": rule.visible_working && state == State::Working,
            "skip_state_update": rule.skip_state_update,
            "interruptible": rule.interruptible && state == State::Working,
            "blocked_rule_matched": blocked_rule_matched,
            "interrupt_keys": loaded.manifest.interrupt_keys,
            "skipped_update_reason": rule.skip_state_update.then(|| format!("matched_rule:{}", rule.id)),
            "fallback_reason": Value::Null,
            "warning": Value::Null,
            "explanations": explanations,
        })
    }

    fn load_bundled() -> Self {
        let mut manifests = HashMap::with_capacity(BUNDLED_MANIFESTS.len());
        for (id, text) in BUNDLED_MANIFESTS {
            manifests.insert(*id, ManifestEntry::bundled(id, text));
        }
        Self { manifests }
    }
}

#[derive(Debug)]
enum ManifestEntry {
    Bundled {
        id: &'static str,
        text: &'static str,
        loaded: OnceLock<Result<LoadedManifest, String>>,
    },
    Loaded(LoadedManifest),
}

impl ManifestEntry {
    fn bundled(id: &'static str, text: &'static str) -> Self {
        Self::Bundled {
            id,
            text,
            loaded: OnceLock::new(),
        }
    }

    fn loaded(&self) -> Result<&LoadedManifest, &str> {
        match self {
            Self::Bundled { id, text, loaded } => loaded
                .get_or_init(|| load_bundled_manifest(id, text))
                .as_ref()
                .map_err(String::as_str),
            Self::Loaded(loaded) => Ok(loaded),
        }
    }

    #[cfg(test)]
    fn bundled_is_loaded(&self) -> bool {
        match self {
            Self::Bundled { loaded, .. } => loaded.get().is_some(),
            Self::Loaded(_) => false,
        }
    }
}

fn load_bundled_manifest(id: &str, text: &str) -> Result<LoadedManifest, String> {
    let loaded = load_manifest(text, ManifestSource::Bundled)
        .map_err(|error| format!("invalid bundled {id} manifest: {error}"))?;
    if loaded.manifest.id != id {
        return Err(format!(
            "invalid bundled {id} manifest: id is {}",
            loaded.manifest.id
        ));
    }
    Ok(loaded)
}

#[derive(Debug)]
struct LoadedManifest {
    manifest: AgentManifest,
    compiled_rules: Vec<CompiledRule>,
    source: ManifestSource,
}

#[derive(Debug)]
enum ManifestSource {
    Bundled,
    Override(PathBuf),
}

impl ManifestSource {
    fn label(&self) -> String {
        match self {
            Self::Bundled => "bundled".to_string(),
            Self::Override(path) => format!("override:{}", path.display()),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentManifest {
    id: String,
    version: Option<String>,
    min_engine_version: Option<u32>,
    #[serde(default, rename = "updated_at")]
    _updated_at: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    /// The keys that interrupt a turn, sent in order (engine 4). None
    /// means masil does not know how to interrupt this provider.
    #[serde(default)]
    interrupt_keys: Vec<String>,
    #[serde(default)]
    rules: Vec<ManifestRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestRule {
    id: String,
    #[serde(default)]
    state: State,
    #[serde(default)]
    priority: i32,
    #[serde(default = "default_region")]
    region: String,
    #[serde(default)]
    visible_idle: bool,
    #[serde(default)]
    visible_blocker: bool,
    #[serde(default)]
    visible_working: bool,
    #[serde(default)]
    skip_state_update: bool,
    /// A working screen on which the interrupt keys end the turn (engine 4).
    #[serde(default)]
    interruptible: bool,
    #[serde(default)]
    all: Vec<ManifestGate>,
    #[serde(default)]
    any: Vec<ManifestGate>,
    #[serde(default, rename = "not")]
    not_gate: Vec<ManifestGate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

fn default_region() -> String {
    "whole_recent".to_string()
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ManifestGate {
    #[serde(default)]
    all: Vec<ManifestGate>,
    #[serde(default)]
    any: Vec<ManifestGate>,
    #[serde(default, rename = "not")]
    not_gate: Vec<ManifestGate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum State {
    Idle,
    Working,
    Blocked,
    #[default]
    Unknown,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug)]
struct CompiledRule {
    gate: CompiledGate,
}

#[derive(Debug)]
struct CompiledGate {
    all: Vec<CompiledGate>,
    any: Vec<CompiledGate>,
    not_gate: Vec<CompiledGate>,
    contains: Vec<String>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
}

#[derive(Default)]
struct Complexity {
    gates: usize,
    matchers: usize,
}

fn load_manifest(text: &str, source: ManifestSource) -> Result<LoadedManifest, String> {
    if text.len() > MAX_MANIFEST_BYTES {
        return Err(format!("manifest exceeds {MAX_MANIFEST_BYTES} bytes"));
    }
    let manifest: AgentManifest = toml::from_str(text).map_err(|error| error.to_string())?;
    let compiled_rules = validate_manifest(&manifest)?;
    Ok(LoadedManifest {
        manifest,
        compiled_rules,
        source,
    })
}

fn validate_manifest(manifest: &AgentManifest) -> Result<Vec<CompiledRule>, String> {
    if manifest.id.trim().is_empty() {
        return Err("manifest id must not be empty".into());
    }
    if let Some(version) = &manifest.version {
        validate_version(version)?;
    }
    if manifest
        .min_engine_version
        .is_some_and(|version| version > ENGINE_VERSION)
    {
        return Err(format!(
            "manifest requires engine {}, current engine is {ENGINE_VERSION}",
            manifest.min_engine_version.unwrap_or_default()
        ));
    }
    let interrupting =
        !manifest.interrupt_keys.is_empty() || manifest.rules.iter().any(|rule| rule.interruptible);
    if interrupting
        && manifest
            .min_engine_version
            .is_none_or(|version| version < 4)
    {
        return Err("interrupt_keys and interruptible need min_engine_version = 4".into());
    }
    if manifest.interrupt_keys.len() > 4
        || manifest
            .interrupt_keys
            .iter()
            .any(|key| !matches!(key.as_str(), "Escape" | "C-c" | "C-d" | "C-g" | "q"))
    {
        return Err("interrupt_keys has at most 4 of Escape, C-c, C-d, C-g, q".into());
    }
    if manifest.rules.is_empty() {
        return Err("manifest must contain at least one rule".into());
    }
    if manifest.rules.len() > MAX_RULES_PER_MANIFEST {
        return Err(format!(
            "manifest contains {} rules, max is {MAX_RULES_PER_MANIFEST}",
            manifest.rules.len()
        ));
    }
    let mut complexity = Complexity::default();
    let mut compiled_rules = Vec::with_capacity(manifest.rules.len());
    for rule in &manifest.rules {
        if rule.id.trim().is_empty() {
            return Err("manifest rule id must not be empty".into());
        }
        if rule.interruptible && rule.state != State::Working {
            return Err(format!(
                "rule {} is interruptible without state = \"working\"",
                rule.id
            ));
        }
        if rule.skip_state_update {
            if rule.state != State::Unknown {
                return Err(format!(
                    "rule {} uses skip_state_update without state = \"unknown\"",
                    rule.id
                ));
            }
            if rule.visible_idle || rule.visible_blocker || rule.visible_working {
                return Err(format!(
                    "rule {} uses skip_state_update with visible state evidence",
                    rule.id
                ));
            }
        }
        validate_region(&rule.region)
            .map_err(|error| format!("rule {} uses invalid region: {error}", rule.id))?;
        if rule.region.trim().starts_with("top_non_empty_lines(")
            && manifest
                .min_engine_version
                .is_some_and(|version| version < 3)
        {
            return Err(format!(
                "rule {} uses top_non_empty_lines but min_engine_version is below 3",
                rule.id
            ));
        }
        let gate = gate_from_rule(rule);
        let gate = validate_gate(&gate, "rule", 0, &mut complexity)
            .map_err(|error| format!("rule {} has invalid matcher gates: {error}", rule.id))?;
        compiled_rules.push(CompiledRule { gate });
    }
    Ok(compiled_rules)
}

fn validate_version(version: &str) -> Result<(), String> {
    let trimmed = version.trim();
    if trimmed.is_empty() {
        return Err("version must not be empty".into());
    }
    for segment in trimmed.split('.') {
        if segment.is_empty() {
            return Err(format!("version {trimmed:?} contains an empty segment"));
        }
        if !segment.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(format!("version {trimmed:?} must be dotted numeric"));
        }
        segment
            .parse::<u64>()
            .map_err(|_| format!("version {trimmed:?} contains an oversized segment"))?;
    }
    Ok(())
}

fn gate_from_rule(rule: &ManifestRule) -> ManifestGate {
    ManifestGate {
        all: rule.all.clone(),
        any: rule.any.clone(),
        not_gate: rule.not_gate.clone(),
        contains: rule.contains.clone(),
        regex: rule.regex.clone(),
        line_regex: rule.line_regex.clone(),
    }
}

fn validate_gate(
    gate: &ManifestGate,
    context: &str,
    depth: usize,
    complexity: &mut Complexity,
) -> Result<CompiledGate, String> {
    if depth > MAX_GATE_DEPTH {
        return Err(format!("{context} exceeds max gate depth {MAX_GATE_DEPTH}"));
    }
    complexity.gates += 1;
    if complexity.gates > MAX_TOTAL_GATES {
        return Err(format!("manifest exceeds max gate count {MAX_TOTAL_GATES}"));
    }
    let direct = gate.contains.len() + gate.regex.len() + gate.line_regex.len();
    if direct > MAX_MATCHERS_PER_GATE {
        return Err(format!(
            "{context} has {direct} direct matchers, max is {MAX_MATCHERS_PER_GATE}"
        ));
    }
    complexity.matchers += direct;
    if complexity.matchers > MAX_TOTAL_MATCHERS {
        return Err(format!(
            "manifest exceeds max matcher count {MAX_TOTAL_MATCHERS}"
        ));
    }
    if !gate_has_positive_matcher(gate) {
        return Err(format!("{context} must contain a positive matcher"));
    }
    for matcher in gate
        .contains
        .iter()
        .chain(&gate.regex)
        .chain(&gate.line_regex)
    {
        if matcher.chars().count() > MAX_MATCHER_CHARS {
            return Err(format!(
                "{context} matcher exceeds max length {MAX_MATCHER_CHARS}"
            ));
        }
    }
    let regex = gate
        .regex
        .iter()
        .map(|pattern| {
            Regex::new(pattern)
                .map_err(|error| format!("{context} contains invalid regex {pattern:?}: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let line_regex = gate
        .line_regex
        .iter()
        .map(|pattern| {
            Regex::new(pattern).map_err(|error| {
                format!("{context} contains invalid line_regex {pattern:?}: {error}")
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let all = gate
        .all
        .iter()
        .map(|nested| validate_gate(nested, "all gate", depth + 1, complexity))
        .collect::<Result<Vec<_>, _>>()?;
    let any = gate
        .any
        .iter()
        .map(|nested| validate_gate(nested, "any gate", depth + 1, complexity))
        .collect::<Result<Vec<_>, _>>()?;
    let not_gate = gate
        .not_gate
        .iter()
        .map(|nested| {
            if !gate_has_any_matcher(nested) {
                return Err(format!("{context} contains an empty not gate"));
            }
            validate_not_gate(nested, depth + 1, complexity)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CompiledGate {
        all,
        any,
        not_gate,
        contains: gate
            .contains
            .iter()
            .map(|value| value.to_lowercase())
            .collect(),
        regex,
        line_regex,
    })
}

fn validate_not_gate(
    gate: &ManifestGate,
    depth: usize,
    complexity: &mut Complexity,
) -> Result<CompiledGate, String> {
    if depth > MAX_GATE_DEPTH {
        return Err(format!("not gate exceeds max gate depth {MAX_GATE_DEPTH}"));
    }
    complexity.gates += 1;
    if complexity.gates > MAX_TOTAL_GATES {
        return Err(format!("manifest exceeds max gate count {MAX_TOTAL_GATES}"));
    }
    if !gate_has_any_matcher(gate) {
        return Err("not gate must contain a matcher".into());
    }
    let direct = gate.contains.len() + gate.regex.len() + gate.line_regex.len();
    if direct > MAX_MATCHERS_PER_GATE {
        return Err(format!(
            "not gate has {direct} direct matchers, max is {MAX_MATCHERS_PER_GATE}"
        ));
    }
    complexity.matchers += direct;
    if complexity.matchers > MAX_TOTAL_MATCHERS {
        return Err(format!(
            "manifest exceeds max matcher count {MAX_TOTAL_MATCHERS}"
        ));
    }
    for matcher in gate
        .contains
        .iter()
        .chain(&gate.regex)
        .chain(&gate.line_regex)
    {
        if matcher.chars().count() > MAX_MATCHER_CHARS {
            return Err(format!(
                "not gate matcher exceeds max length {MAX_MATCHER_CHARS}"
            ));
        }
    }
    let regex = gate
        .regex
        .iter()
        .map(|pattern| {
            Regex::new(pattern).map_err(|error| format!("invalid not regex {pattern:?}: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let line_regex = gate
        .line_regex
        .iter()
        .map(|pattern| {
            Regex::new(pattern)
                .map_err(|error| format!("invalid not line_regex {pattern:?}: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let all = gate
        .all
        .iter()
        .map(|nested| validate_gate(nested, "not all gate", depth + 1, complexity))
        .collect::<Result<Vec<_>, _>>()?;
    let any = gate
        .any
        .iter()
        .map(|nested| validate_gate(nested, "not any gate", depth + 1, complexity))
        .collect::<Result<Vec<_>, _>>()?;
    let not_gate = gate
        .not_gate
        .iter()
        .map(|nested| validate_not_gate(nested, depth + 1, complexity))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CompiledGate {
        all,
        any,
        not_gate,
        contains: gate
            .contains
            .iter()
            .map(|value| value.to_lowercase())
            .collect(),
        regex,
        line_regex,
    })
}

fn gate_has_positive_matcher(gate: &ManifestGate) -> bool {
    !gate.contains.is_empty()
        || !gate.regex.is_empty()
        || !gate.line_regex.is_empty()
        || !gate.all.is_empty()
        || !gate.any.is_empty()
}

fn gate_has_any_matcher(gate: &ManifestGate) -> bool {
    gate_has_positive_matcher(gate) || !gate.not_gate.is_empty()
}

struct RegionText<'a> {
    text: &'a str,
    lower: OnceLock<String>,
    preview: String,
}

impl<'a> RegionText<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            lower: OnceLock::new(),
            preview: bounded_preview(text),
        }
    }
}

fn compiled_gate_matches(gate: &CompiledGate, region: &RegionText<'_>) -> bool {
    (gate.contains.is_empty() || {
        let lower = region.lower.get_or_init(|| region.text.to_lowercase());
        gate.contains.iter().all(|needle| lower.contains(needle))
    }) && gate.regex.iter().all(|regex| regex.is_match(region.text))
        && gate
            .line_regex
            .iter()
            .all(|regex| region.text.lines().any(|line| regex.is_match(line)))
        && gate
            .all
            .iter()
            .all(|nested| compiled_gate_matches(nested, region))
        && (gate.any.is_empty()
            || gate
                .any
                .iter()
                .any(|nested| compiled_gate_matches(nested, region)))
        && !gate
            .not_gate
            .iter()
            .any(|nested| compiled_gate_matches(nested, region))
}

#[derive(Clone, Copy)]
struct DetectionInput<'a> {
    screen: &'a str,
    osc_title: &'a str,
    osc_progress: &'a str,
}

fn validate_region(spec: &str) -> Result<(), String> {
    let spec = spec.trim();
    match spec {
        "whole_recent"
        | "after_last_prompt_marker"
        | "before_current_prompt_marker"
        | "whole_recent_without_current_prompt_marker"
        | "current_prompt_block_marker"
        | "after_current_prompt_block_marker"
        | "prompt_box_body"
        | "above_prompt_box"
        | "last_non_empty_above_prompt_box"
        | "after_last_horizontal_rule"
        | "osc_title"
        | "osc_progress" => Ok(()),
        _ if region_count(spec, "bottom_lines", true).is_some()
            || region_count(spec, "bottom_non_empty_lines", true).is_some()
            || region_count(spec, "top_non_empty_lines", false).is_some() =>
        {
            Ok(())
        }
        _ => Err(spec.to_string()),
    }
}

fn region<'a>(input: DetectionInput<'a>, spec: &str) -> &'a str {
    let spec = spec.trim();
    match spec {
        "osc_title" => return input.osc_title,
        "osc_progress" => return input.osc_progress,
        _ => {}
    }
    let content = input.screen;
    match spec {
        "whole_recent" => content,
        "after_last_prompt_marker" => after_last_prompt_marker(content),
        "before_current_prompt_marker" => before_current_prompt_marker(content),
        "whole_recent_without_current_prompt_marker" => {
            whole_recent_without_current_prompt_marker(content)
        }
        "current_prompt_block_marker" => current_prompt_block_marker(content).unwrap_or(""),
        "after_current_prompt_block_marker" => {
            after_current_prompt_block_marker(content).unwrap_or("")
        }
        "prompt_box_body" => prompt_box_body(content).unwrap_or(""),
        "above_prompt_box" => above_prompt_box(content),
        "last_non_empty_above_prompt_box" => last_non_empty_line(above_prompt_box(content)),
        "after_last_horizontal_rule" => after_last_horizontal_rule(content),
        _ => {
            if let Some(count) = region_count(spec, "bottom_lines", true) {
                return bottom_lines(content, count);
            }
            if let Some(count) = region_count(spec, "bottom_non_empty_lines", true) {
                return bottom_non_empty_lines(content, count);
            }
            if let Some(count) = region_count(spec, "top_non_empty_lines", false) {
                return top_non_empty_lines(content, count);
            }
            ""
        }
    }
}

fn region_count(spec: &str, name: &str, allow_zero: bool) -> Option<usize> {
    let raw = spec
        .strip_prefix(name)?
        .strip_prefix('(')?
        .strip_suffix(')')?;
    if raw.is_empty()
        || (!allow_zero && raw.starts_with('0'))
        || !raw.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let count = raw.parse::<usize>().ok()?;
    (count <= MAX_REGION_LINES && (allow_zero || count > 0)).then_some(count)
}

fn bottom_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    slice_from_line_index(content, &lines, lines.len().saturating_sub(count))
}

fn bottom_non_empty_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(start) = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index)
    else {
        return "";
    };
    slice_from_line_index(content, &lines, start)
}

fn top_non_empty_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(end) = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index + 1)
    else {
        return "";
    };
    &content[..line_start_offset(content, &lines, end)]
}

fn after_last_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(index) = lines.iter().rposition(|line| codex_prompt_line(line)) else {
        return content;
    };
    slice_from_line_index(content, &lines, index + 1)
}

fn before_current_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(index) = current_codex_prompt_index(&lines) else {
        return content;
    };
    &content[..line_start_offset(content, &lines, index)]
}

fn whole_recent_without_current_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    if current_codex_prompt_index(&lines).is_some() {
        ""
    } else {
        content
    }
}

fn current_prompt_block_marker(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let prompt = current_codex_prompt_index(&lines)?;
    lines[..prompt]
        .iter()
        .rev()
        .find(|line| codex_block_marker_line(line))
        .copied()
}

fn after_current_prompt_block_marker(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let prompt = current_codex_prompt_index(&lines)?;
    let block = lines[..prompt]
        .iter()
        .rposition(|line| codex_block_marker_line(line))?;
    Some(slice_from_line_index(content, &lines, block))
}

fn current_codex_prompt_index(lines: &[&str]) -> Option<usize> {
    let prompt = lines.iter().rposition(|line| codex_prompt_line(line))?;
    if lines[prompt + 1..]
        .iter()
        .any(|line| codex_block_marker_line(line))
    {
        None
    } else {
        Some(prompt)
    }
}

fn codex_prompt_line(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn codex_block_marker_line(line: &str) -> bool {
    line.starts_with('•') || line.starts_with('■') || line.starts_with('✗') || line.starts_with('✓')
}

fn prompt_box_body(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let top = prompt_box_top_border_index(&lines)?;
    let start = line_start_offset(content, &lines, top + 1);
    let end_index = lines[top + 1..]
        .iter()
        .position(|line| is_horizontal_rule(line))
        .map(|relative| top + 1 + relative)
        .unwrap_or(lines.len());
    let end = line_start_offset(content, &lines, end_index);
    Some(&content[start..end])
}

fn above_prompt_box(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(top) = prompt_box_top_border_index(&lines) else {
        return content;
    };
    &content[..line_start_offset(content, &lines, top)]
}

fn last_non_empty_line(content: &str) -> &str {
    content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

fn after_last_horizontal_rule(content: &str) -> &str {
    let mut rule_end = 0;
    let mut offset = 0;
    for line in content.lines() {
        let next = (offset + line.len() + 1).min(content.len());
        if is_horizontal_rule(line) {
            rule_end = next;
        }
        offset = next;
    }
    &content[rule_end..]
}

fn prompt_box_top_border_index(lines: &[&str]) -> Option<usize> {
    let mut borders = 0;
    for index in (0..lines.len()).rev() {
        if is_horizontal_rule(lines[index]) {
            borders += 1;
            if borders == 2 {
                return Some(index);
            }
        }
    }
    None
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let count = trimmed
        .chars()
        .take_while(|character| *character == '─')
        .count();
    if count == 0 {
        return false;
    }
    let end = trimmed
        .char_indices()
        .nth(count)
        .map(|(index, _)| index)
        .unwrap_or(trimmed.len());
    trimmed[end..].trim_start().is_empty() || count >= 3
}

fn slice_from_line_index<'a>(content: &'a str, lines: &[&str], index: usize) -> &'a str {
    &content[line_start_offset(content, lines, index)..]
}

fn line_start_offset(content: &str, lines: &[&str], index: usize) -> usize {
    lines[..index.min(lines.len())]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>()
        .min(content.len())
}

fn bounded_preview(text: &str) -> String {
    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(240).collect();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
}

fn unknown_explanation(
    provider: &str,
    reason: &str,
    manifest: Option<&LoadedManifest>,
    explanations: Vec<Value>,
) -> Value {
    json!({
        "provider": provider,
        "agent": provider,
        "state": "unknown",
        "source": manifest.map(|loaded| loaded.source.label()),
        "manifest_source": manifest.map(|loaded| loaded.source.label()),
        "manifest_version": manifest.and_then(|loaded| loaded.manifest.version.clone()),
        "matched_rule": Value::Null,
        "visible_idle": false,
        "visible_blocker": false,
        "visible_working": false,
        "skip_state_update": false,
        "skipped_update_reason": Value::Null,
        "fallback_reason": reason,
        "warning": Value::Null,
        "explanations": explanations,
    })
}

fn unavailable_explanation(provider: &str, warning: &str) -> Value {
    let mut explanation = unknown_explanation(provider, "manifest_unavailable", None, Vec::new());
    explanation["warning"] = Value::String(warning.to_owned());
    explanation
}

pub(crate) fn override_directory() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(path).join("masil").join("agent-detection"));
    }
    if cfg!(windows)
        && let Some(path) = std::env::var_os("APPDATA")
    {
        return Some(PathBuf::from(path).join("masil").join("agent-detection"));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join(".config").join("masil").join("agent-detection"))
}

fn read_override(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("could not inspect override {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("override {} is not a regular file", path.display()));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("could not open override {}: {error}", path.display()))?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect override {}: {error}", path.display()))?;
    if !opened_metadata.is_file() {
        return Err(format!("override {} is not a regular file", path.display()));
    }

    let mut bytes =
        Vec::with_capacity(opened_metadata.len().min(MAX_MANIFEST_BYTES as u64 + 1) as usize);
    file.take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read override {}: {error}", path.display()))?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(format!(
            "override {} exceeds {MAX_MANIFEST_BYTES} bytes",
            path.display()
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new() -> Self {
            loop {
                let sequence = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "masil-detection-test-{}-{sequence}",
                    std::process::id()
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("could not create test directory: {error}"),
                }
            }
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn synthetic(text: &str) -> Engine {
        let loaded = load_manifest(text, ManifestSource::Bundled).unwrap();
        Engine {
            manifests: HashMap::from([("codex", ManifestEntry::Loaded(loaded))]),
        }
    }

    #[test]
    fn bundled_manifests_are_valid_v3_schema() {
        for (id, text) in BUNDLED_MANIFESTS {
            load_bundled_manifest(id, text).unwrap();
        }
    }

    #[test]
    fn bundled_manifests_compile_only_when_used() {
        let engine = Engine::load_bundled();
        assert_eq!(engine.manifests.len(), BUNDLED_MANIFESTS.len());
        assert_eq!(
            engine
                .manifests
                .values()
                .filter(|manifest| manifest.bundled_is_loaded())
                .count(),
            0
        );

        engine.explain("codex", "", "");

        assert!(engine.manifests["codex"].bundled_is_loaded());
        assert_eq!(
            engine
                .manifests
                .values()
                .filter(|manifest| manifest.bundled_is_loaded())
                .count(),
            1
        );
    }

    #[test]
    fn invalid_bundled_manifest_is_reported_as_unavailable() {
        let engine = Engine {
            manifests: HashMap::from([(
                "codex",
                ManifestEntry::bundled(
                    "codex",
                    r#"
id = "codex"
[[rules]]
id = "invalid"
state = "idle"
regex = ["("]
"#,
                ),
            )]),
        };

        let explanation = engine.explain("codex", "anything", "");

        assert_eq!(explanation["state"], "unknown");
        assert_eq!(explanation["fallback_reason"], "manifest_unavailable");
        assert!(
            explanation["warning"]
                .as_str()
                .is_some_and(|warning| warning.contains("invalid bundled codex manifest"))
        );
    }

    #[test]
    fn overrides_are_eagerly_validated_and_new_engines_see_changes() {
        let directory = TempDirectory::new();
        let path = directory.path().join("qwen.toml");
        std::fs::write(
            &path,
            r#"
id = "qwen"
[[rules]]
id = "invalid"
state = "idle"
regex = ["("]
"#,
        )
        .unwrap();

        let error =
            Engine::load_with_override_directory(Some(directory.path().to_owned())).unwrap_err();
        assert!(error.contains("invalid override"), "{error}");
        assert!(error.contains("invalid regex"), "{error}");

        std::fs::write(
            &path,
            r#"
id = "qwen"
[[rules]]
id = "first"
state = "idle"
contains = ["first version"]
"#,
        )
        .unwrap();
        let first =
            Engine::load_with_override_directory(Some(directory.path().to_owned())).unwrap();
        assert_eq!(first.explain("qwen", "first version", "")["state"], "idle");

        std::fs::write(
            &path,
            r#"
id = "qwen"
[[rules]]
id = "second"
state = "working"
contains = ["second version"]
"#,
        )
        .unwrap();
        let second =
            Engine::load_with_override_directory(Some(directory.path().to_owned())).unwrap();

        assert_eq!(
            first.explain("qwen", "second version", "")["state"],
            "unknown"
        );
        assert_eq!(
            second.explain("qwen", "second version", "")["state"],
            "working"
        );
    }

    #[test]
    fn priority_recursive_gates_and_unknown_fallback_are_deterministic() {
        let engine = synthetic(
            r#"
id = "codex"
version = "1.0"
min_engine_version = 3

[[rules]]
id = "low"
state = "idle"
priority = 10
contains = ["ready"]

[[rules]]
id = "high"
state = "blocked"
priority = 20
visible_blocker = true
all = [{ contains = ["ready"] }]
any = [{ contains = ["approve"] }, { line_regex = ['^confirm$'] }]
not = [{ contains = ["cancelled"] }]
"#,
        );
        let blocked = engine.explain("codex", "ready\nconfirm", "");
        assert_eq!(blocked["state"], "blocked");
        assert_eq!(blocked["matched_rule"]["id"], "high");
        assert_eq!(blocked["visible_blocker"], true);

        let unknown = engine.explain("codex", "nothing useful", "");
        assert_eq!(unknown["state"], "unknown");
        assert_eq!(unknown["fallback_reason"], "no_matching_rule");
    }

    #[test]
    fn shared_regions_preserve_unicode_gates_and_refresh_between_screens() {
        let engine = synthetic(
            r#"
id = "codex"
[[rules]]
id = "ready"
state = "idle"
priority = 10
contains = ["äready"]
[[rules]]
id = "confirm"
state = "blocked"
priority = 20
all = [{ contains = ["ÄREADY"] }]
any = [{ line_regex = ['^confirm$'] }]
not = [{ contains = ["cancel"] }]
[[rules]]
id = "case_sensitive"
state = "working"
priority = 5
regex = ['^ÄREADY']
[[rules]]
id = "title"
state = "working"
priority = 30
region = "osc_title"
contains = ["äready"]
"#,
        );
        let first = engine.explain("codex", "ÄREADY\nconfirm\n", "unrelated");
        assert_eq!(first["matched_rule"]["id"], "confirm");
        let matched: Vec<_> = first["explanations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|rule| rule["matched"].as_bool().unwrap())
            .collect();
        assert_eq!(matched, [true, true, true, false]);
        assert_eq!(
            first["explanations"][1]["evidence"]["region_preview"],
            "ÄREADY\nconfirm\n"
        );
        let second = engine.explain("codex", "äready\nconfirm\ncancel", "unrelated");
        assert_eq!(second["matched_rule"]["id"], "ready");
        assert_eq!(second["explanations"][2]["matched"], false);
        let third = engine.explain("codex", "nothing", "ÄREADY");
        assert_eq!(third["matched_rule"]["id"], "title");
        assert_eq!(third["explanations"][0]["matched"], false);
    }

    #[test]
    fn previews_truncate_at_unicode_character_boundaries() {
        for count in [0, 239, 240, 241, 10_000] {
            let input = "한".repeat(count);
            let expected = "한".repeat(count.min(240)) + if count > 240 { "..." } else { "" };
            assert_eq!(bounded_preview(&input), expected);
        }
    }

    #[test]
    fn regions_and_skip_state_update_follow_manifest_semantics() {
        let engine = synthetic(
            r#"
id = "codex"
version = "1.0"
min_engine_version = 3

[[rules]]
id = "overlay"
state = "unknown"
priority = 30
region = "bottom_non_empty_lines(2)"
skip_state_update = true
contains = ["menu", "close"]

[[rules]]
id = "top"
state = "blocked"
priority = 20
region = "top_non_empty_lines(1)"
contains = ["trust"]

[[rules]]
id = "title"
state = "working"
priority = 10
region = "osc_title"
contains = ["running"]
"#,
        );
        let overlay = engine.explain("codex", "trust\n\nmenu\nclose\n", "running");
        assert_eq!(overlay["matched_rule"]["id"], "overlay");
        assert_eq!(overlay["skip_state_update"], true);
        assert_eq!(overlay["visible_working"], false);
    }

    #[test]
    fn invalid_and_excessive_manifests_fail_closed() {
        let too_deep = format!(
            "id = \"codex\"\n[[rules]]\nid = \"deep\"\nstate = \"idle\"\n{}contains = [\"x\"]{}",
            "all = [{ ".repeat(MAX_GATE_DEPTH + 1),
            " }]".repeat(MAX_GATE_DEPTH + 1)
        );
        let depth_error = load_manifest(&too_deep, ManifestSource::Bundled).unwrap_err();
        assert!(depth_error.contains("max gate depth"), "{depth_error}");

        let matcher_list = std::iter::repeat_n("\"x\"", MAX_MATCHERS_PER_GATE + 1)
            .collect::<Vec<_>>()
            .join(", ");
        let too_many_matchers = format!(
            "id = \"codex\"\n[[rules]]\nid = \"wide\"\nstate = \"idle\"\ncontains = [{matcher_list}]"
        );
        let matcher_error = load_manifest(&too_many_matchers, ManifestSource::Bundled).unwrap_err();
        assert!(matcher_error.contains("direct matchers"), "{matcher_error}");

        let unsupported = r#"
id = "codex"
min_engine_version = 5
[[rules]]
id = "future"
state = "idle"
contains = ["x"]
"#;
        assert!(load_manifest(unsupported, ManifestSource::Bundled).is_err());

        let invalid_version = r#"
id = "codex"
version = "next"
[[rules]]
id = "bad_version"
state = "idle"
contains = ["x"]
"#;
        assert!(load_manifest(invalid_version, ManifestSource::Bundled).is_err());
    }

    #[test]
    fn unsupported_provider_and_oversized_input_are_unknown() {
        let engine = Engine::load_bundled();
        assert_eq!(engine.explain("shell", "", "")["state"], "unknown");
        let oversized = "x".repeat(MAX_SCREEN_BYTES + 1);
        assert_eq!(
            engine.explain("codex", &oversized, "")["fallback_reason"],
            "input_limit_exceeded"
        );
    }
}
