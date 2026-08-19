use std::{error::Error, path::Path, sync::Arc, time::Duration};

use nexa_harness::{
    Agent, AgentEvent, AgentRequest, HarnessAgent, InferenceRequest, Provider, ProviderEvent,
    WorkspaceTools,
};
use nexa_protocol::{
    ApiFormat, Command, Event, ModelMessage, ModelRef, OpenSessionRequest, ProviderSummary,
    SessionInfo, ToolCall,
};
use nexa_runtime::{AgentFactory, SessionRegistry};
use reqwest::{Client, Response, StatusCode};
use tempfile::tempdir;
use tokio::{net::TcpListener, task::JoinHandle};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
const TEST_TOKEN: &str = "test-local-server-token";

#[tokio::test]
async fn two_clients_share_one_ordered_replayable_session() -> TestResult {
    let state = tempdir()?;
    let workspace = tempdir()?;
    let registry = SessionRegistry::without_agent(state.path());
    let (base_url, server) = start_server(registry).await?;
    let http = Client::new();
    let session = open_session(&http, &base_url, workspace.path()).await?;
    let mut client_a = EventClient::connect(&http, &base_url, &session.id).await?;
    let mut client_b = EventClient::connect(&http, &base_url, &session.id).await?;

    let alice = send_message(&http, &base_url, &session.id, "alice", "hello from alice");
    let bob = send_message(&http, &base_url, &session.id, "bob", "hello from bob");
    let (alice_response, bob_response) = tokio::join!(alice, bob);
    assert_eq!(alice_response?, StatusCode::CREATED);
    assert_eq!(bob_response?, StatusCode::CREATED);

    let events_for_a = [client_a.next().await?, client_a.next().await?];
    let events_for_b = [client_b.next().await?, client_b.next().await?];
    assert_eq!(events_for_a, events_for_b);
    assert_eq!(
        events_for_a.iter().map(Event::sequence).collect::<Vec<_>>(),
        [1_u64, 2_u64]
    );

    let mut client_ids = events_for_a
        .iter()
        .filter_map(Event::client_id)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    client_ids.sort();
    assert_eq!(client_ids, ["alice", "bob"]);

    drop(client_a);
    let mut reconnected = EventClient::connect(&http, &base_url, &session.id).await?;
    assert_eq!(reconnected.next().await?, events_for_a[0]);
    assert_eq!(reconnected.next().await?, events_for_a[1]);

    let persisted =
        tokio::fs::read_to_string(state.path().join(&session.id).join("events.ndjson")).await?;
    assert_eq!(persisted.lines().count(), 2);

    server.abort();
    Ok(())
}

#[tokio::test]
async fn an_agent_response_uses_the_same_durable_stream() -> TestResult {
    let state = tempdir()?;
    let workspace = tempdir()?;
    let registry = SessionRegistry::new(
        state.path(),
        Arc::new(FixedAgentFactory {
            agent: Arc::new(ReplyingAgent),
        }),
    );
    let (base_url, server) = start_server(registry).await?;
    let http = Client::new();
    let session = open_session(&http, &base_url, workspace.path()).await?;
    let mut client_a = EventClient::connect(&http, &base_url, &session.id).await?;
    let mut client_b = EventClient::connect(&http, &base_url, &session.id).await?;
    assert_eq!(
        send_message(&http, &base_url, &session.id, "alice", "hello agent").await?,
        StatusCode::CREATED
    );

    let events_for_a = receive_run(&mut client_a).await?;
    let events_for_b = receive_run(&mut client_b).await?;
    assert_eq!(events_for_a, events_for_b);
    assert!(matches!(events_for_a[0], Event::Message { .. }));
    assert!(matches!(events_for_a[1], Event::RunStarted { .. }));
    assert!(events_for_a.iter().any(|event| matches!(
        event,
        Event::AssistantMessage { text, .. } if text == "hello human"
    )));
    assert_eq!(
        events_for_a.iter().map(Event::sequence).collect::<Vec<_>>(),
        (1..=u64::try_from(events_for_a.len())?).collect::<Vec<_>>()
    );

    let persisted =
        tokio::fs::read_to_string(state.path().join(&session.id).join("events.ndjson")).await?;
    assert_eq!(persisted.lines().count(), events_for_a.len());

    server.abort();
    Ok(())
}

#[tokio::test]
async fn rejects_an_unknown_provider_model_pair_before_recording_it() -> TestResult {
    let state = tempdir()?;
    let workspace = tempdir()?;
    let registry = SessionRegistry::new(
        state.path(),
        Arc::new(FixedAgentFactory {
            agent: Arc::new(RejectingAgent),
        }),
    );
    let (base_url, server) = start_server(registry).await?;
    let http = Client::new();
    let session = open_session(&http, &base_url, workspace.path()).await?;

    let status = send_message(&http, &base_url, &session.id, "alice", "hello agent").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let event_log = state.path().join(&session.id).join("events.ndjson");
    assert!(tokio::fs::read_to_string(event_log).await?.is_empty());

    server.abort();
    Ok(())
}

#[tokio::test]
async fn one_server_keeps_workspace_tools_isolated_by_session() -> TestResult {
    let state = tempdir()?;
    let workspace_a = tempdir()?;
    let workspace_b = tempdir()?;
    tokio::fs::write(workspace_a.path().join("marker.txt"), "workspace alpha").await?;
    tokio::fs::write(workspace_b.path().join("marker.txt"), "workspace beta").await?;

    let provider: Arc<dyn Provider> = Arc::new(ReadMarkerProvider);
    let registry = SessionRegistry::new(state.path(), Arc::new(WorkspaceAgentFactory { provider }));
    let (base_url, server) = start_server(registry).await?;
    let http = Client::new();
    let session_a = open_session(&http, &base_url, workspace_a.path()).await?;
    let session_b = open_session(&http, &base_url, workspace_b.path()).await?;
    assert_ne!(session_a.id, session_b.id);

    let mut events_a = EventClient::connect(&http, &base_url, &session_a.id).await?;
    let mut events_b = EventClient::connect(&http, &base_url, &session_b.id).await?;
    let send_a = send_message(&http, &base_url, &session_a.id, "alice", "read marker");
    let send_b = send_message(&http, &base_url, &session_b.id, "bob", "read marker");
    let (response_a, response_b) = tokio::join!(send_a, send_b);
    assert_eq!(response_a?, StatusCode::CREATED);
    assert_eq!(response_b?, StatusCode::CREATED);

    let text_a = assistant_text(&receive_run(&mut events_a).await?);
    let text_b = assistant_text(&receive_run(&mut events_b).await?);
    assert!(text_a.contains("workspace alpha"));
    assert!(!text_a.contains("workspace beta"));
    assert!(text_b.contains("workspace beta"));
    assert!(!text_b.contains("workspace alpha"));

    server.abort();
    Ok(())
}

#[tokio::test]
async fn exposes_the_runtime_provider_catalog() -> TestResult {
    let state = tempdir()?;
    let registry = SessionRegistry::without_agent(state.path());
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let catalog = vec![ProviderSummary {
        id: "deepseek".to_owned(),
        name: "DeepSeek".to_owned(),
        api_format: ApiFormat::ChatCompletions,
        models: vec!["deepseek-chat".to_owned()],
    }];
    let server = tokio::spawn(async move {
        nexa_server::serve_with_catalog(listener, registry, catalog, TEST_TOKEN.to_owned())
            .await
            .expect("test server should remain available");
    });

    let providers = Client::new()
        .get(format!("{base_url}/providers"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<ProviderSummary>>()
        .await?;
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].id, "deepseek");
    assert_eq!(providers[0].models, ["deepseek-chat"]);

    server.abort();
    Ok(())
}

#[tokio::test]
async fn rejects_workspace_access_without_the_local_server_token() -> TestResult {
    let state = tempdir()?;
    let workspace = tempdir()?;
    let registry = SessionRegistry::without_agent(state.path());
    let (base_url, server) = start_server(registry).await?;

    let response = Client::new()
        .post(format!("{base_url}/sessions/open"))
        .json(&OpenSessionRequest {
            workspace: workspace.path().to_string_lossy().into_owned(),
        })
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(state.path().read_dir()?.next().is_none());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn reports_corrupt_session_metadata_as_a_server_error() -> TestResult {
    let state = tempdir()?;
    let workspace = tempdir()?;
    let info = SessionRegistry::without_agent(state.path())
        .open_workspace(workspace.path())
        .await?;
    tokio::fs::write(
        state.path().join(&info.id).join("session.json"),
        b"not json",
    )
    .await?;
    let (base_url, server) = start_server(SessionRegistry::without_agent(state.path())).await?;

    let response = Client::new()
        .get(format!("{base_url}/sessions/{}/events", info.id))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    server.abort();
    Ok(())
}

struct FixedAgentFactory {
    agent: Arc<dyn Agent>,
}

impl AgentFactory for FixedAgentFactory {
    fn create(&self, _workspace: &Path) -> Result<Option<Arc<dyn Agent>>, String> {
        Ok(Some(Arc::clone(&self.agent)))
    }
}

struct WorkspaceAgentFactory {
    provider: Arc<dyn Provider>,
}

impl AgentFactory for WorkspaceAgentFactory {
    fn create(&self, workspace: &Path) -> Result<Option<Arc<dyn Agent>>, String> {
        let tools = WorkspaceTools::new(workspace).map_err(|error| error.to_string())?;
        let agent: Arc<dyn Agent> = Arc::new(HarnessAgent::new(
            Arc::clone(&self.provider),
            Arc::new(tools),
        ));
        Ok(Some(agent))
    }
}

struct ReplyingAgent;

impl Agent for ReplyingAgent {
    fn start(&self, _request: AgentRequest) -> tokio::sync::mpsc::UnboundedReceiver<AgentEvent> {
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        events
            .send(AgentEvent::AssistantTextDelta("hello human".to_owned()))
            .expect("test receiver should remain open");
        events
            .send(AgentEvent::AssistantMessage {
                text: "hello human".to_owned(),
                tool_calls: Vec::new(),
            })
            .expect("test receiver should remain open");
        events
            .send(AgentEvent::Completed)
            .expect("test receiver should remain open");
        receiver
    }
}

struct RejectingAgent;

impl Agent for RejectingAgent {
    fn validate_model(&self, _model: &ModelRef) -> Result<(), String> {
        Err("unknown provider/model pair".to_owned())
    }

    fn start(&self, _request: AgentRequest) -> tokio::sync::mpsc::UnboundedReceiver<AgentEvent> {
        panic!("an invalid model must not start the agent")
    }
}

struct ReadMarkerProvider;

impl Provider for ReadMarkerProvider {
    fn stream(
        &self,
        request: InferenceRequest,
    ) -> tokio::sync::mpsc::UnboundedReceiver<Result<ProviderEvent, String>> {
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        match request.messages.last() {
            Some(ModelMessage::Tool(result)) => {
                events
                    .send(Ok(ProviderEvent::TextDelta(result.content.clone())))
                    .expect("test receiver should remain open");
            }
            _ => {
                events
                    .send(Ok(ProviderEvent::ToolCall(ToolCall {
                        id: "read-marker".to_owned(),
                        name: "read_file".to_owned(),
                        arguments: r#"{"path":"marker.txt"}"#.to_owned(),
                    })))
                    .expect("test receiver should remain open");
            }
        }
        events
            .send(Ok(ProviderEvent::Completed))
            .expect("test receiver should remain open");
        receiver
    }
}

async fn start_server(registry: SessionRegistry) -> TestResult<(String, JoinHandle<()>)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        nexa_server::serve(listener, registry, TEST_TOKEN.to_owned())
            .await
            .expect("test server should remain available");
    });
    Ok((base_url, server))
}

async fn open_session(
    client: &Client,
    base_url: &str,
    workspace: &Path,
) -> TestResult<SessionInfo> {
    Ok(client
        .post(format!("{base_url}/sessions/open"))
        .bearer_auth(TEST_TOKEN)
        .json(&OpenSessionRequest {
            workspace: workspace
                .to_str()
                .ok_or("temporary workspace must be UTF-8")?
                .to_owned(),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn send_message(
    client: &Client,
    base_url: &str,
    session_id: &str,
    client_id: &str,
    text: &str,
) -> TestResult<StatusCode> {
    let response = client
        .post(format!("{base_url}/commands"))
        .bearer_auth(TEST_TOKEN)
        .json(&Command::SendMessage {
            session_id: session_id.to_owned(),
            client_id: client_id.to_owned(),
            model: ModelRef {
                provider: "test-provider".to_owned(),
                id: "test-model".to_owned(),
            },
            text: text.to_owned(),
        })
        .send()
        .await?;
    Ok(response.status())
}

async fn receive_run(client: &mut EventClient) -> TestResult<Vec<Event>> {
    let mut events = Vec::new();
    loop {
        let event = client.next().await?;
        let terminal = matches!(event, Event::RunCompleted { .. } | Event::RunFailed { .. });
        events.push(event);
        if terminal {
            return Ok(events);
        }
    }
}

fn assistant_text(events: &[Event]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            Event::AssistantMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

struct EventClient {
    response: Response,
    buffer: String,
}

impl EventClient {
    async fn connect(client: &Client, base_url: &str, session_id: &str) -> TestResult<Self> {
        let response = client
            .get(format!("{base_url}/sessions/{session_id}/events"))
            .bearer_auth(TEST_TOKEN)
            .timeout(Duration::from_secs(2))
            .send()
            .await?
            .error_for_status()?;
        Ok(Self {
            response,
            buffer: String::new(),
        })
    }

    async fn next(&mut self) -> TestResult<Event> {
        loop {
            if let Some(event) = self.take_event()? {
                return Ok(event);
            }
            let chunk = self
                .response
                .chunk()
                .await?
                .ok_or("event stream closed before the next event")?;
            self.buffer.push_str(std::str::from_utf8(&chunk)?);
        }
    }

    fn take_event(&mut self) -> TestResult<Option<Event>> {
        let Some(boundary) = self.buffer.find("\n\n") else {
            return Ok(None);
        };
        let frame = self.buffer[..boundary].to_owned();
        self.buffer.drain(..boundary + 2);
        let data = frame.lines().find_map(|line| line.strip_prefix("data: "));
        data.map(serde_json::from_str)
            .transpose()
            .map_err(Into::into)
    }
}
