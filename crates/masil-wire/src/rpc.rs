//! Versioned JSON RPC envelopes, normalized timeline items, relay multiplexing
//! envelopes, and minimal push metadata shared across host, relay, and mobile.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HandshakeEnvelope {
    E2eeHello { v: u8, message: String },
    E2eeReady { v: u8, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "relay", rename_all = "snake_case")]
pub enum RelayControlMessage {
    SessionOpen {
        session_id: String,
    },
    SessionClose {
        session_id: String,
        reason: Option<String>,
    },
    Frame {
        session_id: String,
        kind: String,
        data: String,
    },
    Push {
        request: Box<PushNotificationRequest>,
    },
    Ping {
        ts: u64,
    },
    Pong {
        ts: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcRequest {
    pub v: u8,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcResponse {
    pub v: u8,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcErrorBody>,
}

impl RpcResponse {
    pub fn ok(id: impl Into<String>, req_kind: &str, value: Value) -> Self {
        Self {
            v: crate::PROTOCOL_VERSION,
            id: id.into(),
            kind: response_kind(req_kind),
            ok: true,
            value: Some(value),
            error: None,
        }
    }

    pub fn err(
        id: impl Into<String>,
        req_kind: &str,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            v: crate::PROTOCOL_VERSION,
            id: id.into(),
            kind: response_kind(req_kind),
            ok: false,
            value: None,
            error: Some(RpcErrorBody {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

fn response_kind(req_kind: &str) -> String {
    if let Some(prefix) = req_kind.strip_suffix(".request") {
        format!("{prefix}.response")
    } else {
        format!("{req_kind}.response")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcEvent {
    pub v: u8,
    pub sub: String,
    pub seq: u64,
    pub event: String,
    pub data: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BindingConfidence {
    ReportedByHook,
    ManuallySelected,
    UnverifiedHeuristic,
    None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    StructuredApi,
    TerminalConfirm,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalOption {
    pub id: String,
    pub label: String,
    pub key_sequence: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InteractivePromptKind {
    Permission,
    Question,
    Input,
    Confirmation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InteractivePromptMode {
    StructuredApi,
    TerminalTui,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InteractiveOption {
    pub id: String,
    pub index: usize,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub selected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<String>,
    #[serde(default)]
    pub is_custom_input: bool,
    #[serde(default)]
    pub is_destructive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredQuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredQuestion {
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<StructuredQuestionOption>,
    #[serde(default = "default_true")]
    pub custom: bool,
    #[serde(default)]
    pub multiple: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PromptRiskTier {
    Low,
    Medium,
    High,
    Critical,
}

impl PromptRiskTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "SAFE READ",
            Self::Medium => "WORKTREE WRITE",
            Self::High => "EXTERNAL / NET",
            Self::Critical => "DESTRUCTIVE / SECRET",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptRiskAssessment {
    pub tier: PromptRiskTier,
    pub label: String,
    pub score: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_rule_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_action: Option<String>,
    #[serde(default)]
    pub batch_eligible: bool,
    pub prompt_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InteractivePrompt {
    pub id: String,
    pub kind: InteractivePromptKind,
    pub mode: InteractivePromptMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_label: Option<String>,
    #[serde(default)]
    pub options: Vec<InteractiveOption>,
    #[serde(default)]
    pub selected_index: usize,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default)]
    pub allows_free_text: bool,
    #[serde(default)]
    pub free_text_only: bool,
    #[serde(default = "default_true")]
    pub allows_cancel: bool,
    #[serde(default)]
    pub can_navigate_tabs: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub always_patterns: Vec<String>,
    #[serde(default)]
    pub session_peers_count: usize,
    #[serde(default)]
    pub is_subagent: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<StructuredQuestion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint_line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<PromptRiskAssessment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineItem {
    UserMessage {
        id: String,
        seq: u64,
        ts_ms: i64,
        text: String,
    },
    AssistantMessage {
        id: String,
        seq: u64,
        ts_ms: i64,
        markdown: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    Thinking {
        id: String,
        seq: u64,
        ts_ms: i64,
        text: String,
        collapsed_default: bool,
    },
    ToolCall {
        id: String,
        seq: u64,
        ts_ms: i64,
        name: String,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        args_json: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        status: ToolStatus,
    },
    ApprovalBanner {
        id: String,
        seq: u64,
        ts_ms: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        prompt: String,
        options: Vec<ApprovalOption>,
        mode: ApprovalMode,
    },
    SystemNotice {
        id: String,
        seq: u64,
        ts_ms: i64,
        level: String,
        text: String,
    },
    PluginCard {
        id: String,
        seq: u64,
        ts_ms: i64,
        plugin_id: String,
        card_id: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        subtitle: Option<String>,
        blocks: Vec<Value>,
    },
}

pub const LIVE_ACTIVITY_MAX_ALIAS_CHARS: usize = 32;
pub const LIVE_ACTIVITY_MAX_SUMMARY_CHARS: usize = 60;

/// Strict whitelist of fixed event labels permitted to traverse the external
/// push bridge for iOS Live Activities and Android Ongoing Fleet Pulse (D-09).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveActivityEventKind {
    ApprovalRequested,
    QuestionAsked,
    LoopPaused,
    HardConflict,
    BsFlagged,
    TurnFinished,
}

impl LiveActivityEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApprovalRequested => "approval_requested",
            Self::QuestionAsked => "question_asked",
            Self::LoopPaused => "loop_paused",
            Self::HardConflict => "hard_conflict",
            Self::BsFlagged => "bs_flagged",
            Self::TurnFinished => "turn_finished",
        }
    }

    pub fn display_label(self) -> &'static str {
        match self {
            Self::ApprovalRequested => "Approval requested",
            Self::QuestionAsked => "Question waiting",
            Self::LoopPaused => "Loop paused",
            Self::HardConflict => "Worktree conflict",
            Self::BsFlagged => "Audit warning",
            Self::TurnFinished => "Turn finished",
        }
    }
}

/// Strictly whitelisted Live Activity / Ongoing Fleet Pulse payload (D-09).
/// `deny_unknown_fields` guarantees prompts, diffs, or raw code can never be
/// smuggled across the push relay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LiveActivityPayload {
    pub host_id: String,
    pub host_name: String,
    pub working_count: u32,
    pub blocked_count: u32,
    pub done_count: u32,
    pub alert_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_agent_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_event_kind: Option<LiveActivityEventKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_summary_line: Option<String>,
    pub deep_link: String,
    #[serde(default)]
    pub updated_at: u64,
}

impl LiveActivityPayload {
    pub fn validate(&self) -> Result<(), String> {
        if self.host_id.trim().is_empty() || self.host_id.len() > 64 {
            return Err("invalid host_id length".into());
        }
        if self.host_name.chars().count() > 64 {
            return Err("host_name exceeds 64 chars".into());
        }
        if let Some(ref alias) = self.top_agent_alias
            && alias.chars().count() > LIVE_ACTIVITY_MAX_ALIAS_CHARS
        {
            return Err(format!(
                "top_agent_alias exceeds {LIVE_ACTIVITY_MAX_ALIAS_CHARS} chars"
            ));
        }
        if let Some(ref summary) = self.top_summary_line {
            if summary.chars().count() > LIVE_ACTIVITY_MAX_SUMMARY_CHARS {
                return Err(format!(
                    "top_summary_line exceeds {LIVE_ACTIVITY_MAX_SUMMARY_CHARS} chars"
                ));
            }
            if summary.contains('\n') || summary.contains('\r') {
                return Err("top_summary_line must be single-line".into());
            }
        }
        if !self.deep_link.starts_with("masil://") || self.deep_link.len() > 256 {
            return Err("deep_link must be a bounded masil:// URI".into());
        }
        Ok(())
    }

    pub fn sanitized(&self) -> Self {
        let clean_single_line = |raw: &str, max_chars: usize| -> String {
            raw.replace(['\r', '\n', '\t'], " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(max_chars)
                .collect()
        };
        let top_agent_alias = self
            .top_agent_alias
            .as_deref()
            .map(|s| clean_single_line(s, LIVE_ACTIVITY_MAX_ALIAS_CHARS))
            .filter(|s| !s.is_empty());
        let top_summary_line = self
            .top_summary_line
            .as_deref()
            .map(|s| clean_single_line(s, LIVE_ACTIVITY_MAX_SUMMARY_CHARS))
            .filter(|s| !s.is_empty());
        let deep_link = if self.deep_link.starts_with("masil://") {
            self.deep_link.chars().take(256).collect()
        } else {
            format!("masil://host/{}", self.host_id)
        };
        Self {
            host_id: self.host_id.chars().take(64).collect(),
            host_name: clean_single_line(&self.host_name, 64),
            working_count: self.working_count,
            blocked_count: self.blocked_count,
            done_count: self.done_count,
            alert_count: self.alert_count,
            top_agent_alias,
            top_event_kind: self.top_event_kind,
            top_summary_line,
            deep_link,
            updated_at: self.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushDeviceToken {
    pub platform: String,
    pub token: String,
    /// Optional push channel discriminator: `None` / `"alert"` for standard alerts,
    /// `"liveactivity"` / `"liveactivity_start"` for iOS ActivityKit push tokens,
    /// or `"android_ongoing"` for Android silent data-only Fleet Pulse updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushNotificationRequest {
    pub host_id: String,
    pub host_name: String,
    pub agent_id: String,
    pub agent_name: String,
    pub event_kind: String,
    pub occurred_ms: i64,
    pub tokens: Vec<PushDeviceToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_activity: Option<LiveActivityPayload>,
}
