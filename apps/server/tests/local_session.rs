use std::{error::Error, sync::Arc, time::Duration};

use nexa_harness::{Agent, AgentEvent, AgentRequest};
use nexa_protocol::{ApiFormat, Command, Event, ModelRef, ProviderSummary};
use nexa_runtime::LocalSession;
use reqwest::{Client, Response, StatusCode};
use tempfile::tempdir;
use tokio::{net::TcpListener, task::JoinHandle};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[tokio::test]
async fn two_clients_share_one_ordered_replayable_session() -> TestResult {
    let directory = tempdir()?;
    let event_log_path = directory.path().join("local.ndjson");
    let session = LocalSession::open(&event_log_path).await?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let server = spawn_server(listener, session);

    let http = Client::new();
    let mut client_a = EventClient::connect(&http, &base_url).await?;
    let mut client_b = EventClient::connect(&http, &base_url).await?;

    let alice = send_message(&http, &base_url, "alice", "hello from alice");
    let bob = send_message(&http, &base_url, "bob", "hello from bob");
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
    let mut reconnected_client = EventClient::connect(&http, &base_url).await?;
    assert_eq!(reconnected_client.next().await?, events_for_a[0]);
    assert_eq!(reconnected_client.next().await?, events_for_a[1]);

    let persisted = tokio::fs::read_to_string(event_log_path).await?;
    assert_eq!(persisted.lines().count(), 2);

    server.abort();
    Ok(())
}

#[tokio::test]
async fn an_agent_response_uses_the_same_durable_stream() -> TestResult {
    let directory = tempdir()?;
    let event_log_path = directory.path().join("local.ndjson");
    let session = LocalSession::open_with_agent(&event_log_path, Arc::new(ReplyingAgent)).await?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let server = spawn_server(listener, session);

    let http = Client::new();
    let mut client_a = EventClient::connect(&http, &base_url).await?;
    let mut client_b = EventClient::connect(&http, &base_url).await?;
    assert_eq!(
        send_message(&http, &base_url, "alice", "hello agent").await?,
        StatusCode::CREATED
    );

    let mut events_for_a = Vec::new();
    let mut events_for_b = Vec::new();
    loop {
        let event_a = client_a.next().await?;
        let event_b = client_b.next().await?;
        let completed = matches!(event_a, Event::RunCompleted { .. });
        events_for_a.push(event_a);
        events_for_b.push(event_b);
        if completed {
            break;
        }
    }

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
    let persisted = tokio::fs::read_to_string(event_log_path).await?;
    assert_eq!(persisted.lines().count(), events_for_a.len());

    server.abort();
    Ok(())
}

#[tokio::test]
async fn rejects_an_unknown_provider_model_pair_before_recording_it() -> TestResult {
    let directory = tempdir()?;
    let event_log_path = directory.path().join("local.ndjson");
    let session = LocalSession::open_with_agent(&event_log_path, Arc::new(RejectingAgent)).await?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let server = spawn_server(listener, session);

    let status = send_message(&Client::new(), &base_url, "alice", "hello agent").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(tokio::fs::read_to_string(event_log_path).await?.is_empty());

    server.abort();
    Ok(())
}

#[tokio::test]
async fn exposes_the_runtime_provider_catalog() -> TestResult {
    let directory = tempdir()?;
    let session = LocalSession::open(directory.path().join("local.ndjson")).await?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let catalog = vec![ProviderSummary {
        id: "deepseek".to_owned(),
        name: "DeepSeek".to_owned(),
        api_format: ApiFormat::ChatCompletions,
        models: vec!["deepseek-chat".to_owned()],
    }];
    let server = tokio::spawn(async move {
        nexa_server::serve_with_catalog(listener, session, catalog)
            .await
            .expect("test server should remain available");
    });

    let providers = Client::new()
        .get(format!("{base_url}/providers"))
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

fn spawn_server(listener: TcpListener, session: LocalSession) -> JoinHandle<()> {
    tokio::spawn(async move {
        nexa_server::serve(listener, session)
            .await
            .expect("test server should remain available");
    })
}

async fn send_message(
    client: &Client,
    base_url: &str,
    client_id: &str,
    text: &str,
) -> TestResult<StatusCode> {
    let response = client
        .post(format!("{base_url}/commands"))
        .json(&Command::SendMessage {
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

struct EventClient {
    response: Response,
    buffer: String,
}

impl EventClient {
    async fn connect(client: &Client, base_url: &str) -> TestResult<Self> {
        let response = client
            .get(format!("{base_url}/events"))
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
