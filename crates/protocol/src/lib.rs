use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire-protocol revision. Servers advertise it on every response via the
/// `x-nexa-protocol` header; clients refuse servers older than themselves.
pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiFormat {
    #[default]
    ChatCompletions,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
}

impl ReasoningEffort {
    /// Every effort level, ordered from least to most expensive.
    pub const ALL: [ReasoningEffort; 6] = [
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    /// Levels shown when a provider says reasoning is supported but does not
    /// enumerate which values it accepts.
    ///
    /// This is Grok Build's fallback menu and OpenCode's widely-supported set
    /// plus `xhigh`. `minimal` and `max` stay out unless a model lists them.
    pub const COMMON: [ReasoningEffort; 4] = [Self::Low, Self::Medium, Self::High, Self::XHigh];

    /// Parses the lowercase wire spelling used by OpenAI-compatible APIs.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|effort| {
            let wire = match effort {
                Self::XHigh => "xhigh",
                other => other.serialize_str(),
            };
            wire.eq_ignore_ascii_case(value)
        })
    }

    fn serialize_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// The lowercase wire spelling sent to providers.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.serialize_str()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSummary {
    pub id: String,
    pub name: String,
    pub api_format: ApiFormat,
    pub models: Vec<ModelSummary>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSummary {
    pub id: String,
    /// Reasoning effort levels the provider reports for this model. `None`
    /// means the endpoint did not declare support either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_efforts: Option<Vec<ReasoningEffort>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRequest {
    pub workspace: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub workspace: String,
}

/// A named agent preset: an immutable tool scope applied per chat.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetSummary {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Tool names the preset allows. An empty list means every tool.
    pub tools: Vec<String>,
}

/// A persisted session without its transcript: enough for pickers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub created_at_ms: u64,
    pub last_opened_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedCommand {
    pub accepted: bool,
    pub sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    SendMessage {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "clientId")]
        client_id: String,
        model: ModelRef,
        /// Agent preset scoping the tools available for this run.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preset: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<ReasoningEffort>,
        text: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelMessage {
    User {
        content: String,
    },
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    Tool(ToolResult),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Message {
        #[serde(rename = "sessionId")]
        session_id: String,
        sequence: u64,
        #[serde(rename = "clientId")]
        client_id: String,
        model: ModelRef,
        text: String,
        #[serde(rename = "createdAtMs")]
        created_at_ms: u64,
    },
    RunStarted {
        #[serde(rename = "sessionId")]
        session_id: String,
        sequence: u64,
        #[serde(rename = "runId")]
        run_id: String,
        model: ModelRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<ReasoningEffort>,
        #[serde(rename = "createdAtMs")]
        created_at_ms: u64,
    },
    AssistantTextDelta {
        #[serde(rename = "sessionId")]
        session_id: String,
        sequence: u64,
        #[serde(rename = "runId")]
        run_id: String,
        text: String,
        #[serde(rename = "createdAtMs")]
        created_at_ms: u64,
    },
    AssistantMessage {
        #[serde(rename = "sessionId")]
        session_id: String,
        sequence: u64,
        #[serde(rename = "runId")]
        run_id: String,
        text: String,
        #[serde(rename = "toolCalls")]
        tool_calls: Vec<ToolCall>,
        #[serde(rename = "createdAtMs")]
        created_at_ms: u64,
    },
    ToolCallStarted {
        #[serde(rename = "sessionId")]
        session_id: String,
        sequence: u64,
        #[serde(rename = "runId")]
        run_id: String,
        call: ToolCall,
        #[serde(rename = "createdAtMs")]
        created_at_ms: u64,
    },
    ToolCallCompleted {
        #[serde(rename = "sessionId")]
        session_id: String,
        sequence: u64,
        #[serde(rename = "runId")]
        run_id: String,
        result: ToolResult,
        #[serde(rename = "createdAtMs")]
        created_at_ms: u64,
    },
    RunCompleted {
        #[serde(rename = "sessionId")]
        session_id: String,
        sequence: u64,
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "createdAtMs")]
        created_at_ms: u64,
    },
    RunFailed {
        #[serde(rename = "sessionId")]
        session_id: String,
        sequence: u64,
        #[serde(rename = "runId")]
        run_id: String,
        error: String,
        #[serde(rename = "createdAtMs")]
        created_at_ms: u64,
    },
}

impl Event {
    #[must_use]
    pub fn sequence(&self) -> u64 {
        match self {
            Self::Message { sequence, .. }
            | Self::RunStarted { sequence, .. }
            | Self::AssistantTextDelta { sequence, .. }
            | Self::AssistantMessage { sequence, .. }
            | Self::ToolCallStarted { sequence, .. }
            | Self::ToolCallCompleted { sequence, .. }
            | Self::RunCompleted { sequence, .. }
            | Self::RunFailed { sequence, .. } => *sequence,
        }
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Message { session_id, .. }
            | Self::RunStarted { session_id, .. }
            | Self::AssistantTextDelta { session_id, .. }
            | Self::AssistantMessage { session_id, .. }
            | Self::ToolCallStarted { session_id, .. }
            | Self::ToolCallCompleted { session_id, .. }
            | Self::RunCompleted { session_id, .. }
            | Self::RunFailed { session_id, .. } => session_id,
        }
    }

    #[must_use]
    pub fn client_id(&self) -> Option<&str> {
        match self {
            Self::Message { client_id, .. } => Some(client_id),
            _ => None,
        }
    }
}
