//! Declarative UI Block Spec v1 shared across Ratatui TUI and React Native
//! Mobile, plus machine-readable schema generation (`masil-agent plugin schema`).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const UI_SPEC_VERSION: u32 = 1;
pub const MAX_UI_JSON_BYTES: usize = 256 * 1024;
pub const MAX_UI_BLOCKS: usize = 200;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum UiTone {
    #[default]
    Normal,
    Muted,
    Accent,
    Success,
    Warning,
    Danger,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiDocument {
    #[serde(default = "default_ui_version")]
    pub v: u32,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub blocks: Vec<UiBlock>,
    /// Reserved forward-compatible hybrid surface for optional rich widgets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_surface: Option<CustomSurface>,
}

fn default_ui_version() -> u32 {
    UI_SPEC_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomSurface {
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiMetric {
    pub label: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(default)]
    pub tone: UiTone,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiKeyValueRow {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub tone: UiTone,
    #[serde(default)]
    pub mono: bool,
    #[serde(default)]
    pub copyable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiListItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<String>,
    #[serde(default)]
    pub tone: UiTone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiButton {
    pub id: String,
    pub label: String,
    #[serde(default = "default_btn_style")]
    pub style: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
}

fn default_btn_style() -> String {
    "secondary".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiSelectOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiFormField {
    pub name: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default)]
    pub multiline: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiBlock {
    Header {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subtitle: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        badge: Option<String>,
        #[serde(default)]
        tone: UiTone,
    },
    Text {
        text: String,
        #[serde(default)]
        tone: UiTone,
        #[serde(default)]
        mono: bool,
    },
    Markdown {
        markdown: String,
    },
    MetricRow {
        items: Vec<UiMetric>,
    },
    Badge {
        label: String,
        #[serde(default)]
        tone: UiTone,
    },
    KeyValue {
        rows: Vec<UiKeyValueRow>,
    },
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        items: Vec<UiListItem>,
    },
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Code {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        code: String,
        #[serde(default = "default_true")]
        copyable: bool,
    },
    Diff {
        text: String,
    },
    Progress {
        label: String,
        value: f64,
        max: f64,
        #[serde(default)]
        tone: UiTone,
    },
    Sparkline {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        values: Vec<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
        #[serde(default)]
        tone: UiTone,
    },
    Divider,
    Callout {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        text: String,
        #[serde(default)]
        tone: UiTone,
    },
    Button {
        id: String,
        label: String,
        #[serde(default = "default_btn_style")]
        style: String,
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confirm: Option<String>,
    },
    ButtonRow {
        buttons: Vec<UiButton>,
    },
    Toggle {
        id: String,
        label: String,
        value: bool,
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
    },
    Select {
        id: String,
        label: String,
        value: String,
        options: Vec<UiSelectOption>,
        action: String,
    },
    Form {
        id: String,
        submit_label: String,
        action: String,
        fields: Vec<UiFormField>,
    },
    Empty {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
    },
}

fn default_true() -> bool {
    true
}

pub fn validate_ui_document(raw: &Value) -> Result<UiDocument, Vec<String>> {
    let encoded_len = serde_json::to_vec(raw)
        .map(|v| v.len())
        .unwrap_or(usize::MAX);
    if encoded_len > MAX_UI_JSON_BYTES {
        return Err(vec![format!(
            "UI payload exceeds {} KiB limit ({} bytes)",
            MAX_UI_JSON_BYTES / 1024,
            encoded_len
        )]);
    }
    let doc: UiDocument = serde_json::from_value(raw.clone())
        .map_err(|e| vec![format!("invalid UI document: {e}")])?;
    let mut errors = Vec::new();
    if doc.v != UI_SPEC_VERSION {
        errors.push(format!(
            "unsupported UI spec version {} (expected {UI_SPEC_VERSION})",
            doc.v
        ));
    }
    if doc.blocks.len() > MAX_UI_BLOCKS {
        errors.push(format!(
            "too many UI blocks ({}, max {MAX_UI_BLOCKS})",
            doc.blocks.len()
        ));
    }
    for (idx, block) in doc.blocks.iter().enumerate() {
        match block {
            UiBlock::MetricRow { items } if items.is_empty() || items.len() > 6 => {
                errors.push(format!("blocks[{idx}] metric_row must contain 1..=6 items"));
            }
            UiBlock::Table { columns, rows } => {
                if columns.is_empty() {
                    errors.push(format!("blocks[{idx}] table columns must not be empty"));
                }
                for (r_idx, row) in rows.iter().enumerate() {
                    if row.len() != columns.len() {
                        errors.push(format!(
                            "blocks[{idx}] table row {r_idx} has {} cells, expected {}",
                            row.len(),
                            columns.len()
                        ));
                    }
                }
            }
            UiBlock::Progress { value, max, .. } => {
                if !value.is_finite() || !max.is_finite() || *max <= 0.0 {
                    errors.push(format!(
                        "blocks[{idx}] progress requires finite value and max > 0"
                    ));
                }
            }
            UiBlock::Sparkline { values, .. } => {
                if values.is_empty() || values.iter().any(|v| !v.is_finite()) {
                    errors.push(format!(
                        "blocks[{idx}] sparkline requires non-empty finite values"
                    ));
                }
            }
            UiBlock::Button {
                id, style, action, ..
            } => {
                if id.trim().is_empty() || action.trim().is_empty() {
                    errors.push(format!(
                        "blocks[{idx}] button requires non-empty id and action"
                    ));
                }
                if !matches!(style.as_str(), "primary" | "secondary" | "danger") {
                    errors.push(format!(
                        "blocks[{idx}] button style must be primary, secondary, or danger"
                    ));
                }
            }
            UiBlock::ButtonRow { buttons } => {
                if buttons.is_empty() || buttons.len() > 4 {
                    errors.push(format!(
                        "blocks[{idx}] button_row must contain 1..=4 buttons"
                    ));
                }
                for b in buttons {
                    if !matches!(b.style.as_str(), "primary" | "secondary" | "danger") {
                        errors.push(format!(
                            "blocks[{idx}] button '{}' style must be primary, secondary, or danger",
                            b.id
                        ));
                    }
                }
            }
            UiBlock::Form {
                id, action, fields, ..
            } if id.trim().is_empty() || action.trim().is_empty() || fields.is_empty() => {
                errors.push(format!(
                    "blocks[{idx}] form requires non-empty id, action, and at least one field"
                ));
            }
            _ => {}
        }
    }
    if errors.is_empty() {
        Ok(doc)
    } else {
        Err(errors)
    }
}

/// Machine-readable live contract for AI coding agents (`masil-agent plugin schema`).
pub fn schema_document() -> Value {
    json!({
        "masil_version": env!("CARGO_PKG_VERSION"),
        "plugin_schema_version": super::manifest::PLUGIN_SCHEMA_VERSION,
        "ui_spec_version": UI_SPEC_VERSION,
        "paths": {
            "config": "~/.config/masil/plugins.toml",
            "plugins_dir": "~/.config/masil/plugins/<id>/",
            "manifest": "masil-plugin.toml",
            "theme_file": "theme.toml",
            "overview": "OVERVIEW.md",
            "state_dir": "$XDG_STATE_HOME/masil/plugins/<id>/"
        },
        "cli": [
            "masil-agent plugin init <id> [--dir PATH] [--template theme|command|panel|hook|preset|full] [--lang node|python|bash]",
            "masil-agent plugin install <path> [--id ID] [--no-build]",
            "masil-agent plugin ls [--json]",
            "masil-agent plugin check <id-or-path> [--json]",
            "masil-agent plugin test <id> <panel|command|hook> <target> [--args TEXT] [--action ID] [--cwd DIR] [--json]",
            "masil-agent plugin enable <id>",
            "masil-agent plugin disable <id>",
            "masil-agent plugin reload [id]",
            "masil-agent plugin remove <id>",
            "masil-agent plugin logs <id> [--tail N]",
            "masil-agent plugin theme list | set <theme-id> | reset",
            "masil-agent plugin skill status | install [--target claude|codex|agy|all] | uninstall [--target ...]",
            "masil-agent plugin schema [--json]"
        ],
        "manifest": {
            "required": ["schema_version", "id", "name"],
            "optional": ["version", "description", "author", "min_masil_version", "scope", "entry", "timeout_secs", "build", "themes", "panels", "commands", "hooks", "presets", "resident"],
            "scopes": ["read", "act", "admin"],
            "hook_events": super::manifest::ALLOWED_HOOK_EVENTS,
            "theme_colors": {
                "required": ["canvas", "panel", "text", "accent"],
                "optional": ["raised", "muted", "accent_text", "warning", "danger", "success", "track"],
                "format": "#RRGGBB"
            }
        },
        "handler_protocol": {
            "transport": "Single JSON object on stdin -> single JSON object on stdout; Masil API v1 available on FD 3 (MASIL_API_FD=3)",
            "env": ["MASIL_PLUGIN_ID", "MASIL_PLUGIN_DIR", "MASIL_PLUGIN_STATE_DIR", "MASIL_API_VERSION", "MASIL_API_FD"],
            "stdin_fields": ["v", "kind", "handler", "plugin_id", "action", "payload", "form_values", "args", "event", "event_data", "agent", "storage"],
            "panel_response": ["title?", "subtitle?", "blocks", "custom_surface?", "storage_patch?"],
            "command_response": ["toast?", "ui?", "draft?", "timeline_card?", "refresh_agents?", "storage_patch?"],
            "hook_response": ["ok?", "timeline_card?", "prompt_text?", "storage_patch?"]
        },
        "ui_blocks": [
            {"type": "header", "fields": ["title", "subtitle?", "badge?", "tone?"]},
            {"type": "text", "fields": ["text", "tone?", "mono?"]},
            {"type": "markdown", "fields": ["markdown"]},
            {"type": "metric_row", "fields": ["items: [{label, value, delta?, tone?}] (1..6)"]},
            {"type": "badge", "fields": ["label", "tone?"]},
            {"type": "key_value", "fields": ["rows: [{key, value, tone?, mono?, copyable?}]"]},
            {"type": "list", "fields": ["title?", "items: [{id?, title, subtitle?, badge?, tone?, action?, payload?}]"]},
            {"type": "table", "fields": ["columns: [string]", "rows: [[string]]"]},
            {"type": "code", "fields": ["language?", "code", "copyable?"]},
            {"type": "diff", "fields": ["text"]},
            {"type": "progress", "fields": ["label", "value", "max", "tone?"]},
            {"type": "sparkline", "fields": ["label?", "values: [number]", "unit?", "tone?"]},
            {"type": "divider", "fields": []},
            {"type": "callout", "fields": ["title?", "text", "tone"]},
            {"type": "button", "fields": ["id", "label", "style: primary|secondary|danger", "action", "payload?", "confirm?"]},
            {"type": "button_row", "fields": ["buttons: [button] (1..4)"]},
            {"type": "toggle", "fields": ["id", "label", "value", "action", "payload?"]},
            {"type": "select", "fields": ["id", "label", "value", "options: [{value, label}]", "action"]},
            {"type": "form", "fields": ["id", "submit_label", "action", "fields: [{name, label, placeholder?, value?, multiline?}]"]},
            {"type": "empty", "fields": ["title", "body?"]}
        ],
        "tones": ["normal", "muted", "accent", "success", "warning", "danger"],
        "safety": {
            "human_only_approvals": true,
            "excluded_api_verbs": ["answer", "send-keys", "start ... --"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_rich_ui_document() {
        let doc = json!({
            "v": 1,
            "title": "Repo Pulse",
            "blocks": [
                {"type": "header", "title": "masil", "badge": "main", "tone": "accent"},
                {"type": "metric_row", "items": [{"label": "Dirty", "value": "2", "tone": "warning"}]},
                {"type": "sparkline", "label": "Commits", "values": [1, 3, 2, 5, 4], "tone": "success"},
                {"type": "table", "columns": ["File", "State"], "rows": [["README.md", "M"]]},
                {"type": "button_row", "buttons": [{"id": "refresh", "label": "Refresh", "style": "primary", "action": "refresh"}]}
            ]
        });
        assert!(validate_ui_document(&doc).is_ok());
    }

    #[test]
    fn rejects_mismatched_table_columns() {
        let doc = json!({
            "v": 1,
            "blocks": [
                {"type": "table", "columns": ["A", "B"], "rows": [["only-one"]]}
            ]
        });
        let errs = validate_ui_document(&doc).unwrap_err();
        assert!(errs[0].contains("expected 2"));
    }
}
