use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LOCAL_SESSION_ID: &str = "local";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    SendMessage {
        #[serde(rename = "clientId")]
        client_id: String,
        model: ModelRef,
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
