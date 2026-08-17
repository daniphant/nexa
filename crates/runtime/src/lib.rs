use std::{
    error::Error,
    fmt, io,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use nexa_harness::{Agent, AgentEvent, AgentRequest};
use nexa_protocol::{Event, LOCAL_SESSION_ID, ModelMessage, ModelRef};
use tokio::{
    fs::{self, File, OpenOptions},
    io::AsyncWriteExt,
    sync::{mpsc, oneshot},
};

#[derive(Clone)]
pub struct LocalSession {
    commands: mpsc::Sender<SessionCommand>,
}

impl LocalSession {
    pub async fn open(event_log_path: impl AsRef<Path>) -> Result<Self, SessionError> {
        Self::open_inner(event_log_path.as_ref(), None).await
    }

    pub async fn open_with_agent(
        event_log_path: impl AsRef<Path>,
        agent: Arc<dyn Agent>,
    ) -> Result<Self, SessionError> {
        Self::open_inner(event_log_path.as_ref(), Some(agent)).await
    }

    async fn open_inner(
        event_log_path: &Path,
        agent: Option<Arc<dyn Agent>>,
    ) -> Result<Self, SessionError> {
        if let Some(parent) = event_log_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let events = load_events(event_log_path).await?;
        let event_log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(event_log_path)
            .await?;
        let (commands, receiver) = mpsc::channel(64);
        let task = tokio::spawn(run_session(
            event_log,
            events,
            receiver,
            commands.clone(),
            agent,
        ));
        drop(task);

        Ok(Self { commands })
    }

    pub async fn append_message(
        &self,
        client_id: &str,
        model: &ModelRef,
        text: &str,
    ) -> Result<Event, SessionError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(SessionCommand::AppendMessage {
                client_id: client_id.to_owned(),
                model: model.clone(),
                text: text.to_owned(),
                reply,
            })
            .await
            .map_err(|_| SessionError::Closed)?;
        response.await.map_err(|_| SessionError::Closed)?
    }

    pub async fn subscribe(&self) -> Result<mpsc::UnboundedReceiver<Event>, SessionError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(SessionCommand::Subscribe { reply })
            .await
            .map_err(|_| SessionError::Closed)?;
        response.await.map_err(|_| SessionError::Closed)
    }
}

enum SessionCommand {
    AppendMessage {
        client_id: String,
        model: ModelRef,
        text: String,
        reply: oneshot::Sender<Result<Event, SessionError>>,
    },
    AgentEvent {
        run_id: String,
        event: AgentEvent,
    },
    Subscribe {
        reply: oneshot::Sender<mpsc::UnboundedReceiver<Event>>,
    },
}

async fn run_session(
    mut event_log: File,
    mut events: Vec<Event>,
    mut receiver: mpsc::Receiver<SessionCommand>,
    commands: mpsc::Sender<SessionCommand>,
    agent: Option<Arc<dyn Agent>>,
) {
    let mut subscribers: Vec<mpsc::UnboundedSender<Event>> = Vec::new();
    let mut active_run: Option<String> = None;

    while let Some(command) = receiver.recv().await {
        match command {
            SessionCommand::AppendMessage {
                client_id,
                model,
                text,
                reply,
            } => {
                if active_run.is_some() {
                    let _ = reply.send(Err(SessionError::Busy));
                    continue;
                }
                if let Some(agent) = &agent
                    && let Err(error) = agent.validate_model(&model)
                {
                    let _ = reply.send(Err(SessionError::InvalidModel(error)));
                    continue;
                }

                let event = Event::Message {
                    session_id: LOCAL_SESSION_ID.to_owned(),
                    sequence: next_sequence(&events),
                    client_id,
                    model: model.clone(),
                    text,
                    created_at_ms: timestamp_ms(),
                };
                let result =
                    append_event(&mut event_log, &mut events, &mut subscribers, event.clone())
                        .await;
                if let Err(error) = result {
                    let _ = reply.send(Err(error));
                    continue;
                }

                if let Some(agent) = &agent {
                    let run_id = format!("run-{}-{}", timestamp_ms(), event.sequence());
                    let run_started = Event::RunStarted {
                        session_id: LOCAL_SESSION_ID.to_owned(),
                        sequence: next_sequence(&events),
                        run_id: run_id.clone(),
                        model: model.clone(),
                        created_at_ms: timestamp_ms(),
                    };
                    if let Err(error) =
                        append_event(&mut event_log, &mut events, &mut subscribers, run_started)
                            .await
                    {
                        let _ = reply.send(Err(error));
                        continue;
                    }

                    active_run = Some(run_id.clone());
                    forward_agent_events(
                        Arc::clone(agent),
                        AgentRequest {
                            model,
                            messages: model_history(&events),
                        },
                        run_id,
                        commands.clone(),
                    );
                }
                let _ = reply.send(Ok(event));
            }
            SessionCommand::AgentEvent { run_id, event } => {
                if active_run.as_deref() != Some(&run_id) {
                    continue;
                }
                let terminal = matches!(event, AgentEvent::Completed | AgentEvent::Failed(_));
                let event = protocol_event(run_id, event, next_sequence(&events));
                if append_event(&mut event_log, &mut events, &mut subscribers, event)
                    .await
                    .is_err()
                {
                    break;
                }
                if terminal {
                    active_run = None;
                }
            }
            SessionCommand::Subscribe { reply } => {
                let (subscriber, receiver) = mpsc::unbounded_channel();
                for event in &events {
                    if subscriber.send(event.clone()).is_err() {
                        break;
                    }
                }
                subscribers.push(subscriber);
                let _ = reply.send(receiver);
            }
        }
    }
}

fn forward_agent_events(
    agent: Arc<dyn Agent>,
    request: AgentRequest,
    run_id: String,
    commands: mpsc::Sender<SessionCommand>,
) {
    let mut agent_events = agent.start(request);
    let task = tokio::spawn(async move {
        let mut terminal_event_sent = false;
        while let Some(event) = agent_events.recv().await {
            terminal_event_sent |= matches!(&event, AgentEvent::Completed | AgentEvent::Failed(_));
            if commands
                .send(SessionCommand::AgentEvent {
                    run_id: run_id.clone(),
                    event,
                })
                .await
                .is_err()
            {
                return;
            }
        }
        if !terminal_event_sent {
            let _ = commands
                .send(SessionCommand::AgentEvent {
                    run_id,
                    event: AgentEvent::Failed("agent stopped without a terminal event".to_owned()),
                })
                .await;
        }
    });
    drop(task);
}

fn protocol_event(run_id: String, event: AgentEvent, sequence: u64) -> Event {
    let session_id = LOCAL_SESSION_ID.to_owned();
    let created_at_ms = timestamp_ms();
    match event {
        AgentEvent::AssistantTextDelta(text) => Event::AssistantTextDelta {
            session_id,
            sequence,
            run_id,
            text,
            created_at_ms,
        },
        AgentEvent::AssistantMessage { text, tool_calls } => Event::AssistantMessage {
            session_id,
            sequence,
            run_id,
            text,
            tool_calls,
            created_at_ms,
        },
        AgentEvent::ToolCallStarted(call) => Event::ToolCallStarted {
            session_id,
            sequence,
            run_id,
            call,
            created_at_ms,
        },
        AgentEvent::ToolCallCompleted(result) => Event::ToolCallCompleted {
            session_id,
            sequence,
            run_id,
            result,
            created_at_ms,
        },
        AgentEvent::Completed => Event::RunCompleted {
            session_id,
            sequence,
            run_id,
            created_at_ms,
        },
        AgentEvent::Failed(error) => Event::RunFailed {
            session_id,
            sequence,
            run_id,
            error,
            created_at_ms,
        },
    }
}

async fn append_event(
    event_log: &mut File,
    events: &mut Vec<Event>,
    subscribers: &mut Vec<mpsc::UnboundedSender<Event>>,
    event: Event,
) -> Result<(), SessionError> {
    persist_event(event_log, &event).await?;
    events.push(event.clone());
    subscribers.retain(|subscriber| subscriber.send(event.clone()).is_ok());
    Ok(())
}

fn model_history(events: &[Event]) -> Vec<ModelMessage> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Message { text, .. } => Some(ModelMessage::User {
                content: text.clone(),
            }),
            Event::AssistantMessage {
                text, tool_calls, ..
            } => Some(ModelMessage::Assistant {
                content: text.clone(),
                tool_calls: tool_calls.clone(),
            }),
            Event::ToolCallCompleted { result, .. } => Some(ModelMessage::Tool(result.clone())),
            Event::RunStarted { .. }
            | Event::AssistantTextDelta { .. }
            | Event::ToolCallStarted { .. }
            | Event::RunCompleted { .. }
            | Event::RunFailed { .. } => None,
        })
        .collect()
}

async fn load_events(event_log_path: &Path) -> Result<Vec<Event>, SessionError> {
    let contents = match fs::read_to_string(event_log_path).await {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };

    contents
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let event: Event = serde_json::from_str(line).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid event log line {}: {error}", index + 1),
                )
            })?;
            let expected_sequence = u64::try_from(index).unwrap_or(u64::MAX) + 1;
            if event.session_id() != LOCAL_SESSION_ID || event.sequence() != expected_sequence {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid event log sequence on line {}", index + 1),
                )
                .into());
            }
            Ok(event)
        })
        .collect()
}

async fn persist_event(event_log: &mut File, event: &Event) -> Result<(), SessionError> {
    let mut serialized = serde_json::to_vec(event)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    serialized.push(b'\n');
    event_log.write_all(&serialized).await?;
    event_log.sync_data().await?;
    Ok(())
}

fn next_sequence(events: &[Event]) -> u64 {
    u64::try_from(events.len()).unwrap_or(u64::MAX) + 1
}

fn timestamp_ms() -> u64 {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(milliseconds).unwrap_or(u64::MAX)
}

#[derive(Debug)]
pub enum SessionError {
    Io(io::Error),
    InvalidModel(String),
    Busy,
    Closed,
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::InvalidModel(error) => formatter.write_str(error),
            Self::Busy => formatter.write_str("the local agent is already running"),
            Self::Closed => formatter.write_str("session runtime closed"),
        }
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidModel(_) | Self::Busy | Self::Closed => None,
        }
    }
}

impl From<io::Error> for SessionError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
