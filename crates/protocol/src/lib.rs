use serde::{Deserialize, Serialize};

pub const LOCAL_SESSION_ID: &str = "local";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    SendMessage {
        #[serde(rename = "clientId")]
        client_id: String,
        text: String,
    },
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
        text: String,
        #[serde(rename = "createdAtMs")]
        created_at_ms: u64,
    },
}

impl Event {
    #[must_use]
    pub fn sequence(&self) -> u64 {
        match self {
            Self::Message { sequence, .. } => *sequence,
        }
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Message { session_id, .. } => session_id,
        }
    }

    #[must_use]
    pub fn client_id(&self) -> &str {
        match self {
            Self::Message { client_id, .. } => client_id,
        }
    }
}
