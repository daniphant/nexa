use std::{
    collections::HashMap,
    error::Error,
    fmt,
    fmt::Write as _,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use nexa_harness::{Agent, AgentEvent, AgentRequest};
use nexa_protocol::{Event, ModelMessage, ModelRef, SessionInfo};
use sha2::{Digest, Sha256};
use tokio::{
    fs::{self, File, OpenOptions},
    io::AsyncWriteExt,
    sync::{Mutex, mpsc, oneshot},
};

const METADATA_FILE: &str = "session.json";
const EVENT_LOG_FILE: &str = "events.ndjson";

pub trait AgentFactory: Send + Sync {
    fn create(&self, workspace: &Path) -> Result<Option<Arc<dyn Agent>>, String>;
}

#[derive(Clone)]
pub struct SessionRegistry {
    sessions_directory: PathBuf,
    agent_factory: Arc<dyn AgentFactory>,
    sessions: Arc<Mutex<HashMap<String, OpenSession>>>,
}

#[derive(Clone)]
struct OpenSession {
    info: SessionInfo,
    runtime: LocalSession,
}

impl SessionRegistry {
    #[must_use]
    pub fn new(
        sessions_directory: impl Into<PathBuf>,
        agent_factory: Arc<dyn AgentFactory>,
    ) -> Self {
        Self {
            sessions_directory: sessions_directory.into(),
            agent_factory,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[must_use]
    pub fn without_agent(sessions_directory: impl Into<PathBuf>) -> Self {
        Self::new(sessions_directory, Arc::new(NoAgentFactory))
    }

    pub async fn open_workspace(
        &self,
        workspace: impl AsRef<Path>,
    ) -> Result<SessionInfo, RegistryError> {
        let workspace = fs::canonicalize(workspace.as_ref())
            .await
            .map_err(RegistryError::Workspace)?;
        let metadata = fs::metadata(&workspace)
            .await
            .map_err(RegistryError::Workspace)?;
        if !metadata.is_dir() {
            return Err(RegistryError::InvalidWorkspace(
                "workspace must be a directory".to_owned(),
            ));
        }
        let workspace = workspace
            .to_str()
            .ok_or_else(|| RegistryError::InvalidWorkspace("workspace must be UTF-8".to_owned()))?
            .to_owned();
        let info = SessionInfo {
            id: default_session_id(&workspace),
            workspace,
        };

        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(&info.id) {
            if session.info != info {
                return Err(RegistryError::InvalidMetadata(
                    "session ID is already bound to another workspace".to_owned(),
                ));
            }
            return Ok(session.info.clone());
        }
        let session = self.load_or_create(info, true).await?;
        let info = session.info.clone();
        sessions.insert(info.id.clone(), session);
        Ok(info)
    }

    pub async fn session(&self, session_id: &str) -> Result<LocalSession, RegistryError> {
        validate_session_id(session_id)?;
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(session_id) {
            return Ok(session.runtime.clone());
        }

        let metadata_path = self.session_directory(session_id).join(METADATA_FILE);
        let contents = match fs::read(&metadata_path).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(RegistryError::NotFound(session_id.to_owned()));
            }
            Err(error) => return Err(error.into()),
        };
        let info: SessionInfo = serde_json::from_slice(&contents)
            .map_err(|error| RegistryError::InvalidMetadata(error.to_string()))?;
        if info.id != session_id {
            return Err(RegistryError::InvalidMetadata(format!(
                "session metadata ID {:?} does not match {session_id:?}",
                info.id
            )));
        }
        let session = self.load_or_create(info, false).await?;
        let runtime = session.runtime.clone();
        sessions.insert(session_id.to_owned(), session);
        Ok(runtime)
    }

    async fn load_or_create(
        &self,
        info: SessionInfo,
        create: bool,
    ) -> Result<OpenSession, RegistryError> {
        if default_session_id(&info.workspace) != info.id {
            return Err(RegistryError::InvalidMetadata(
                "session ID does not match its workspace".to_owned(),
            ));
        }
        let workspace = fs::canonicalize(&info.workspace)
            .await
            .map_err(RegistryError::Workspace)?;
        if workspace.to_str() != Some(info.workspace.as_str()) {
            return Err(RegistryError::InvalidMetadata(
                "session workspace is not canonical".to_owned(),
            ));
        }

        let session_directory = self.session_directory(&info.id);
        let metadata_path = session_directory.join(METADATA_FILE);
        let event_log_path = session_directory.join(EVENT_LOG_FILE);
        match fs::read(&metadata_path).await {
            Ok(contents) => {
                let persisted: SessionInfo = serde_json::from_slice(&contents)
                    .map_err(|error| RegistryError::InvalidMetadata(error.to_string()))?;
                if persisted != info {
                    return Err(RegistryError::InvalidMetadata(
                        "session metadata does not match its workspace binding".to_owned(),
                    ));
                }
            }
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(&session_directory).await?;
                self.migrate_legacy_log(&event_log_path, &info.id).await?;
                let metadata = serde_json::to_vec_pretty(&info)
                    .map_err(|error| RegistryError::InvalidMetadata(error.to_string()))?;
                fs::write(&metadata_path, metadata).await?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(RegistryError::NotFound(info.id));
            }
            Err(error) => return Err(error.into()),
        }

        let agent = self
            .agent_factory
            .create(&workspace)
            .map_err(RegistryError::Agent)?;
        let runtime = match agent {
            Some(agent) => LocalSession::open_with_agent(&info.id, event_log_path, agent).await?,
            None => LocalSession::open(&info.id, event_log_path).await?,
        };
        Ok(OpenSession { info, runtime })
    }

    async fn migrate_legacy_log(
        &self,
        event_log_path: &Path,
        session_id: &str,
    ) -> Result<(), RegistryError> {
        if fs::try_exists(event_log_path).await? {
            return Ok(());
        }
        let migrated_path = self.sessions_directory.join("local.ndjson.migrated");
        if fs::try_exists(&migrated_path).await? {
            return Ok(());
        }
        let legacy_path = self.sessions_directory.join("local.ndjson");
        let contents = match fs::read_to_string(&legacy_path).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut migrated = Vec::new();
        for (index, line) in contents.lines().enumerate() {
            let mut event: Event = serde_json::from_str(line).map_err(|error| {
                RegistryError::InvalidMetadata(format!(
                    "invalid legacy event log line {}: {error}",
                    index + 1
                ))
            })?;
            let expected = u64::try_from(index).unwrap_or(u64::MAX) + 1;
            if event.session_id() != "local" || event.sequence() != expected {
                return Err(RegistryError::InvalidMetadata(format!(
                    "invalid legacy event sequence on line {}",
                    index + 1
                )));
            }
            replace_event_session_id(&mut event, session_id);
            serde_json::to_writer(&mut migrated, &event)
                .map_err(|error| RegistryError::InvalidMetadata(error.to_string()))?;
            migrated.push(b'\n');
        }
        fs::write(event_log_path, migrated).await?;
        fs::rename(legacy_path, migrated_path).await?;
        Ok(())
    }

    fn session_directory(&self, session_id: &str) -> PathBuf {
        self.sessions_directory.join(session_id)
    }
}

struct NoAgentFactory;

impl AgentFactory for NoAgentFactory {
    fn create(&self, _workspace: &Path) -> Result<Option<Arc<dyn Agent>>, String> {
        Ok(None)
    }
}

fn default_session_id(workspace: &str) -> String {
    let digest = Sha256::digest(workspace.as_bytes());
    let mut id = String::with_capacity(72);
    id.push_str("session-");
    for byte in digest {
        write!(&mut id, "{byte:02x}").expect("writing to a string cannot fail");
    }
    id
}

fn replace_event_session_id(event: &mut Event, replacement: &str) {
    let session_id = match event {
        Event::Message { session_id, .. }
        | Event::RunStarted { session_id, .. }
        | Event::AssistantTextDelta { session_id, .. }
        | Event::AssistantMessage { session_id, .. }
        | Event::ToolCallStarted { session_id, .. }
        | Event::ToolCallCompleted { session_id, .. }
        | Event::RunCompleted { session_id, .. }
        | Event::RunFailed { session_id, .. } => session_id,
    };
    *session_id = replacement.to_owned();
}

fn validate_session_id(session_id: &str) -> Result<(), RegistryError> {
    let digest = session_id
        .strip_prefix("session-")
        .ok_or_else(|| RegistryError::InvalidSessionId(session_id.to_owned()))?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RegistryError::InvalidSessionId(session_id.to_owned()));
    }
    Ok(())
}

#[derive(Clone)]
pub struct LocalSession {
    commands: mpsc::Sender<SessionCommand>,
}

impl LocalSession {
    pub async fn open(
        session_id: impl Into<String>,
        event_log_path: impl AsRef<Path>,
    ) -> Result<Self, SessionError> {
        Self::open_inner(session_id.into(), event_log_path.as_ref(), None).await
    }

    pub async fn open_with_agent(
        session_id: impl Into<String>,
        event_log_path: impl AsRef<Path>,
        agent: Arc<dyn Agent>,
    ) -> Result<Self, SessionError> {
        Self::open_inner(session_id.into(), event_log_path.as_ref(), Some(agent)).await
    }

    async fn open_inner(
        session_id: String,
        event_log_path: &Path,
        agent: Option<Arc<dyn Agent>>,
    ) -> Result<Self, SessionError> {
        if let Some(parent) = event_log_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let events = load_events(event_log_path, &session_id).await?;
        let event_log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(event_log_path)
            .await?;
        let (commands, receiver) = mpsc::channel(64);
        let weak_commands = commands.downgrade();
        let task = tokio::spawn(run_session(
            session_id,
            event_log,
            events,
            receiver,
            weak_commands,
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
    session_id: String,
    mut event_log: File,
    mut events: Vec<Event>,
    mut receiver: mpsc::Receiver<SessionCommand>,
    commands: mpsc::WeakSender<SessionCommand>,
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
                    session_id: session_id.clone(),
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
                        session_id: session_id.clone(),
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
                let event = protocol_event(&session_id, run_id, event, next_sequence(&events));
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
    commands: mpsc::WeakSender<SessionCommand>,
) {
    let mut agent_events = agent.start(request);
    let task = tokio::spawn(async move {
        let mut terminal_event_sent = false;
        while let Some(event) = agent_events.recv().await {
            terminal_event_sent |= matches!(&event, AgentEvent::Completed | AgentEvent::Failed(_));
            let Some(commands) = commands.upgrade() else {
                return;
            };
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
        if !terminal_event_sent && let Some(commands) = commands.upgrade() {
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

fn protocol_event(session_id: &str, run_id: String, event: AgentEvent, sequence: u64) -> Event {
    let session_id = session_id.to_owned();
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

async fn load_events(event_log_path: &Path, session_id: &str) -> Result<Vec<Event>, SessionError> {
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
            if event.session_id() != session_id || event.sequence() != expected_sequence {
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

#[derive(Debug)]
pub enum RegistryError {
    Io(io::Error),
    Workspace(io::Error),
    InvalidWorkspace(String),
    InvalidSessionId(String),
    NotFound(String),
    InvalidMetadata(String),
    Agent(String),
    Session(SessionError),
}

impl RegistryError {
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }

    #[must_use]
    pub fn is_invalid_request(&self) -> bool {
        matches!(
            self,
            Self::Workspace(_)
                | Self::InvalidWorkspace(_)
                | Self::InvalidSessionId(_)
                | Self::InvalidMetadata(_)
        )
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Workspace(error) => write!(formatter, "could not resolve workspace: {error}"),
            Self::InvalidWorkspace(error) => formatter.write_str(error),
            Self::InvalidSessionId(id) => write!(formatter, "invalid session ID {id:?}"),
            Self::NotFound(id) => write!(formatter, "session {id:?} was not found"),
            Self::InvalidMetadata(error) => write!(formatter, "invalid session metadata: {error}"),
            Self::Agent(error) => write!(formatter, "could not create session agent: {error}"),
            Self::Session(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for RegistryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) | Self::Workspace(error) => Some(error),
            Self::Session(error) => Some(error),
            Self::InvalidWorkspace(_)
            | Self::InvalidSessionId(_)
            | Self::NotFound(_)
            | Self::InvalidMetadata(_)
            | Self::Agent(_) => None,
        }
    }
}

impl From<io::Error> for RegistryError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<SessionError> for RegistryError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::InvalidModel(error) => formatter.write_str(error),
            Self::Busy => formatter.write_str("the session agent is already running"),
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

#[cfg(test)]
mod tests {
    use nexa_protocol::{Event, ModelRef, SessionInfo};
    use tempfile::tempdir;

    use super::SessionRegistry;

    #[tokio::test]
    async fn migrates_the_legacy_local_log_into_the_first_workspace() {
        let state = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let legacy_event = Event::Message {
            session_id: "local".to_owned(),
            sequence: 1,
            client_id: "legacy-client".to_owned(),
            model: ModelRef {
                provider: "legacy-provider".to_owned(),
                id: "legacy-model".to_owned(),
            },
            text: "legacy message".to_owned(),
            created_at_ms: 1,
        };
        let mut contents = serde_json::to_vec(&legacy_event).unwrap();
        contents.push(b'\n');
        tokio::fs::write(state.path().join("local.ndjson"), contents)
            .await
            .unwrap();

        let registry = SessionRegistry::without_agent(state.path());
        let info = registry.open_workspace(workspace.path()).await.unwrap();
        let session = registry.session(&info.id).await.unwrap();
        let mut events = session.subscribe().await.unwrap();
        let migrated = events.recv().await.unwrap();

        assert_eq!(migrated.session_id(), info.id);
        assert_eq!(migrated.sequence(), 1);
        assert!(!state.path().join("local.ndjson").exists());
        assert!(state.path().join("local.ndjson.migrated").exists());
        let persisted: SessionInfo = serde_json::from_slice(
            &tokio::fs::read(state.path().join(&info.id).join("session.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(persisted, info);
    }

    #[tokio::test]
    async fn reopening_a_workspace_returns_its_persisted_session() {
        let state = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let first = SessionRegistry::without_agent(state.path())
            .open_workspace(workspace.path())
            .await
            .unwrap();
        let second = SessionRegistry::without_agent(state.path())
            .open_workspace(workspace.path())
            .await
            .unwrap();

        assert_eq!(first, second);
    }
}
