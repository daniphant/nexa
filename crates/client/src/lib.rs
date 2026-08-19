use std::{error::Error, fmt, str::Utf8Error, sync::Arc, time::Duration};

use nexa_protocol::{
    AcceptedCommand, Command, Event, ModelRef, OpenSessionRequest, ProviderSummary, SessionInfo,
};
use reqwest::{Client, RequestBuilder, Response, StatusCode};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct NexaClient {
    http: Client,
    server_url: String,
    client_id: String,
    session_id: Option<String>,
    token: Option<Arc<str>>,
}

impl NexaClient {
    #[must_use]
    pub fn new(server_url: impl Into<String>, client_id: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            server_url: server_url.into(),
            client_id: client_id.into(),
            session_id: None,
            token: None,
        }
    }

    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(Arc::from(token.into()));
        self
    }

    #[must_use]
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub async fn providers(&self) -> Result<Vec<ProviderSummary>, ClientError> {
        let response = self
            .request(self.http.get(self.endpoint("/providers")))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?;
        let response = accepted_response(response).await?;
        Ok(response.json().await?)
    }

    pub async fn open_workspace(
        &self,
        workspace: impl Into<String>,
    ) -> Result<SessionInfo, ClientError> {
        let response = self
            .request(self.http.post(self.endpoint("/sessions/open")))
            .json(&OpenSessionRequest {
                workspace: workspace.into(),
            })
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?;
        let response = accepted_response(response).await?;
        Ok(response.json().await?)
    }

    pub async fn subscribe(&self) -> Result<EventStream, ClientError> {
        let session_id = self.session_id()?;
        let response = self
            .request(
                self.http
                    .get(self.endpoint(&format!("/sessions/{session_id}/events"))),
            )
            .send()
            .await?;
        Ok(EventStream::new(accepted_response(response).await?))
    }

    pub async fn send_message(
        &self,
        model: ModelRef,
        text: impl Into<String>,
    ) -> Result<AcceptedCommand, ClientError> {
        let session_id = self.session_id()?.to_owned();
        let response = self
            .request(self.http.post(self.endpoint("/commands")))
            .json(&Command::SendMessage {
                session_id,
                client_id: self.client_id.clone(),
                model,
                text: text.into(),
            })
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?;
        let response = accepted_response(response).await?;
        Ok(response.json().await?)
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.server_url.trim_end_matches('/'))
    }

    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.token {
            Some(token) => request.bearer_auth(token.as_ref()),
            None => request,
        }
    }

    fn session_id(&self) -> Result<&str, ClientError> {
        self.session_id
            .as_deref()
            .ok_or(ClientError::SessionNotSelected)
    }
}

pub struct EventStream {
    response: Response,
    buffer: Vec<u8>,
}

impl EventStream {
    fn new(response: Response) -> Self {
        Self {
            response,
            buffer: Vec::new(),
        }
    }

    pub async fn next(&mut self) -> Result<Event, ClientError> {
        loop {
            match take_event(&mut self.buffer)? {
                FrameRead::Event(event) => return Ok(event),
                FrameRead::Skipped => continue,
                FrameRead::Incomplete => {}
            }
            let chunk = self
                .response
                .chunk()
                .await?
                .ok_or(ClientError::StreamClosed)?;
            self.buffer.extend_from_slice(&chunk);
        }
    }
}

#[derive(Debug)]
enum FrameRead {
    Incomplete,
    Skipped,
    Event(Event),
}

async fn accepted_response(response: Response) -> Result<Response, ClientError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response
        .text()
        .await
        .unwrap_or_else(|error| format!("could not read response: {error}"));
    Err(ClientError::Rejected { status, body })
}

fn take_event(buffer: &mut Vec<u8>) -> Result<FrameRead, ClientError> {
    let Some((boundary, separator_length)) = frame_boundary(buffer) else {
        return Ok(FrameRead::Incomplete);
    };
    let frame = buffer.drain(..boundary).collect::<Vec<_>>();
    buffer.drain(..separator_length);
    let data = std::str::from_utf8(&frame)?
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Ok(FrameRead::Skipped);
    }
    Ok(FrameRead::Event(serde_json::from_str(&data)?))
}

fn frame_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2))
        .or_else(|| {
            buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| (index, 4))
        })
}

#[derive(Debug)]
pub enum ClientError {
    Http(reqwest::Error),
    InvalidEvent(serde_json::Error),
    InvalidUtf8(Utf8Error),
    Rejected { status: StatusCode, body: String },
    SessionNotSelected,
    StreamClosed,
}

impl ClientError {
    #[must_use]
    pub fn is_connect(&self) -> bool {
        matches!(self, Self::Http(error) if error.is_connect())
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(error) => write!(formatter, "{error}"),
            Self::InvalidEvent(error) => write!(formatter, "invalid runtime event: {error}"),
            Self::InvalidUtf8(error) => write!(formatter, "runtime sent invalid UTF-8: {error}"),
            Self::Rejected { status, body } => {
                write!(formatter, "runtime rejected the request ({status}): {body}")
            }
            Self::SessionNotSelected => formatter.write_str("no Nexa session is selected"),
            Self::StreamClosed => formatter.write_str("runtime event stream closed"),
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            Self::InvalidEvent(error) => Some(error),
            Self::InvalidUtf8(error) => Some(error),
            Self::Rejected { .. } | Self::SessionNotSelected | Self::StreamClosed => None,
        }
    }
}

impl From<reqwest::Error> for ClientError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(error: serde_json::Error) -> Self {
        Self::InvalidEvent(error)
    }
}

impl From<Utf8Error> for ClientError {
    fn from(error: Utf8Error) -> Self {
        Self::InvalidUtf8(error)
    }
}

#[cfg(test)]
mod tests {
    use nexa_protocol::Event;

    use super::{FrameRead, take_event};

    #[test]
    fn decodes_an_event_without_exposing_sse_metadata() {
        let mut buffer = concat!(
            "id: 3\n",
            "event: message\n",
            "data: {\"type\":\"run_completed\",\"sessionId\":\"local\",",
            "\"sequence\":3,\"runId\":\"run-1\",\"createdAtMs\":1}\n\n"
        )
        .as_bytes()
        .to_vec();

        let FrameRead::Event(event) = take_event(&mut buffer).unwrap() else {
            panic!("expected a complete event");
        };
        assert!(matches!(event, Event::RunCompleted { sequence: 3, .. }));
        assert!(buffer.is_empty());
    }

    #[test]
    fn waits_for_a_complete_frame() {
        let mut buffer = b"data: {\"type\":\"run_completed\"}".to_vec();
        assert!(matches!(
            take_event(&mut buffer).unwrap(),
            FrameRead::Incomplete
        ));
        assert!(!buffer.is_empty());
    }

    #[test]
    fn waits_for_a_complete_utf8_frame_before_decoding_it() {
        let frame = concat!(
            "data: {\"type\":\"assistant_text_delta\",\"sessionId\":\"local\",",
            "\"sequence\":1,\"runId\":\"run-1\",\"text\":\"olá\",\"createdAtMs\":1}\n\n"
        )
        .as_bytes();
        let split = frame.iter().position(|byte| *byte == 0xc3).unwrap() + 1;
        let mut buffer = frame[..split].to_vec();

        assert!(matches!(
            take_event(&mut buffer).unwrap(),
            FrameRead::Incomplete
        ));
        buffer.extend_from_slice(&frame[split..]);
        let FrameRead::Event(event) = take_event(&mut buffer).unwrap() else {
            panic!("expected a complete event");
        };

        assert!(matches!(
            event,
            Event::AssistantTextDelta { text, .. } if text == "olá"
        ));
    }

    #[test]
    fn skips_a_keep_alive_without_waiting_for_another_chunk() {
        let mut buffer = concat!(
            ": keep-alive\n\n",
            "data: {\"type\":\"run_completed\",\"sessionId\":\"local\",",
            "\"sequence\":3,\"runId\":\"run-1\",\"createdAtMs\":1}\n\n"
        )
        .as_bytes()
        .to_vec();

        assert!(matches!(
            take_event(&mut buffer).unwrap(),
            FrameRead::Skipped
        ));
        let FrameRead::Event(event) = take_event(&mut buffer).unwrap() else {
            panic!("expected the buffered event after the keep-alive");
        };
        assert!(matches!(event, Event::RunCompleted { sequence: 3, .. }));
        assert!(buffer.is_empty());
    }
}
