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
        request: PushNotificationRequest,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushDeviceToken {
    pub platform: String,
    pub token: String,
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
}
