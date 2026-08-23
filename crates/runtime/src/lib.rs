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
use nexa_protocol::{Event, ModelMessage, ModelRef, ReasoningEffort, SessionInfo, SessionSummary};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    fs::{self, File, OpenOptions},
    io::AsyncWriteExt,
    sync::{Mutex, mpsc, oneshot},
};

const METADATA_FILE: &str = "session.json";
const EVENT_LOG_FILE: &str = "events.ndjson";

async fn canonical_workspace(workspace: &Path) -> Result<String, RegistryError> {
    let workspace = fs::canonicalize(workspace)
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
    workspace
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| RegistryError::InvalidWorkspace("workspace must be UTF-8".to_owned()))
}

async fn persist_metadata(
    metadata_path: &Path,
    meta: &PersistedSession,
) -> Result<(), RegistryError> {
    let metadata = serde_json::to_vec_pretty(meta)
        .map_err(|error| RegistryError::InvalidMetadata(error.to_string()))?;
    atomic_write(metadata_path, metadata).await?;
    Ok(())
}

/// Merges on-disk and in-memory copies of session metadata by ID, keeping the
/// freshest activity stamp of each.
fn merge_known(metadatas: Vec<PersistedSession>) -> Vec<PersistedSession> {
    let mut by_id: HashMap<String, PersistedSession> = HashMap::new();
    for meta in metadatas {
        match by_id.get_mut(&meta.id) {
            Some(existing) => {
                if meta.last_opened_ms > existing.last_opened_ms {
                    *existing = meta;
                }
            }
            None => {
                by_id.insert(meta.id.clone(), meta);
            }
        }
    }
    by_id.into_values().collect()
}

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
    meta: PersistedSession,
    runtime: LocalSession,
}

impl OpenSession {
    fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.meta.id.clone(),
            workspace: self.meta.workspace.clone(),
        }
    }
}

/// On-disk session metadata. Identity fields (`id`, `workspace`) are
/// immutable once written; activity timestamps update on open.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedSession {
    id: String,
    workspace: String,
    #[serde(default)]
    created_at_ms: u64,
    #[serde(default)]
    last_opened_ms: u64,
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

    /// Creates and opens a brand-new session bound to `workspace`.
    pub async fn create_session(
        &self,
        workspace: impl AsRef<Path>,
    ) -> Result<SessionInfo, RegistryError> {
        let workspace = canonical_workspace(workspace.as_ref()).await?;
        // Random IDs collide with probability ~0, but retry regardless.
        for _ in 0..4 {
            let id = unique_session_id(&workspace)?;
            let exists = fs::try_exists(self.session_directory(&id))
                .await
                .unwrap_or(true);
            if exists {
                continue;
            }
            let meta = PersistedSession {
                id,
                workspace: workspace.clone(),
                created_at_ms: timestamp_ms(),
                last_opened_ms: 0,
            };
            return self.open_meta(meta, true).await;
        }
        Err(RegistryError::InvalidMetadata(
            "could not allocate a unique session ID".to_owned(),
        ))
    }

    /// Sessions previously bound to `workspace`, most recently opened first.
    pub async fn list_sessions(
        &self,
        workspace: impl AsRef<Path>,
    ) -> Result<Vec<SessionSummary>, RegistryError> {
        let workspace = canonical_workspace(workspace.as_ref()).await?;
        let mut known = self.scan_workspace_sessions(&workspace).await?;
        let sessions = self.sessions.lock().await;
        for session in sessions.values() {
            if session.meta.workspace == workspace {
                known.push(session.meta.clone());
            }
        }
        drop(sessions);

        let mut known = merge_known(known);
        known.sort_by_key(|meta| std::cmp::Reverse(meta.last_opened_ms));
        Ok(known
            .into_iter()
            .map(|meta| SessionSummary {
                id: meta.id,
                created_at_ms: meta.created_at_ms,
                last_opened_ms: meta.last_opened_ms,
            })
            .collect())
    }

    /// Opens the freshly created session, persisting its metadata and
    /// spawning its runtime actor.
    async fn open_meta(
        &self,
        mut meta: PersistedSession,
        allow_create: bool,
    ) -> Result<SessionInfo, RegistryError> {
        validate_session_id(&meta.id)?;
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get_mut(&meta.id) {
            session.meta.last_opened_ms = timestamp_ms();
            return Ok(session.info());
        }

        let directory = self.session_directory(&meta.id);
        let metadata_path = directory.join(METADATA_FILE);
        let event_log_path = directory.join(EVENT_LOG_FILE);
        match fs::read(&metadata_path).await {
            Ok(contents) => {
                let persisted: PersistedSession = serde_json::from_slice(&contents)
                    .map_err(|error| RegistryError::InvalidMetadata(error.to_string()))?;
                if persisted.id != meta.id || persisted.workspace != meta.workspace {
                    return Err(RegistryError::InvalidMetadata(
                        "session metadata does not match its workspace binding".to_owned(),
                    ));
                }
                meta.created_at_ms = persisted.created_at_ms;
                meta.last_opened_ms = persisted.last_opened_ms;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && allow_create => {
                fs::create_dir_all(&directory).await?;
                if meta.created_at_ms == 0 {
                    meta.created_at_ms = timestamp_ms();
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(RegistryError::NotFound(meta.id));
            }
            Err(error) => return Err(error.into()),
        }
        meta.last_opened_ms = timestamp_ms();
        persist_metadata(&metadata_path, &meta).await?;

        let workspace = canonical_workspace(Path::new(&meta.workspace))
            .await
            .map_err(|error| match error {
                RegistryError::InvalidWorkspace(message) => RegistryError::InvalidMetadata(message),
                other => other,
            })?;
        let agent = self
            .agent_factory
            .create(Path::new(&workspace))
            .map_err(RegistryError::Agent)?;
        let runtime = match agent {
            Some(agent) => LocalSession::open_with_agent(&meta.id, event_log_path, agent).await?,
            None => LocalSession::open(&meta.id, event_log_path).await?,
        };
        let info = SessionInfo {
            id: meta.id.clone(),
            workspace: meta.workspace.clone(),
        };
        sessions.insert(meta.id.clone(), OpenSession { meta, runtime });
        Ok(info)
    }

    /// Opens an existing session by ID without touching its activity stamp.
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
        let meta: PersistedSession = serde_json::from_slice(&contents)
            .map_err(|error| RegistryError::InvalidMetadata(error.to_string()))?;
        if meta.id != session_id {
            return Err(RegistryError::InvalidMetadata(format!(
                "session metadata ID {:?} does not match {session_id:?}",
                meta.id
            )));
        }
        let event_log_path = self.session_directory(session_id).join(EVENT_LOG_FILE);
        let workspace = PathBuf::from(&meta.workspace);
        let agent = self
            .agent_factory
            .create(&workspace)
            .map_err(RegistryError::Agent)?;
        let runtime = match agent {
            Some(agent) => LocalSession::open_with_agent(session_id, event_log_path, agent).await?,
            None => LocalSession::open(session_id, event_log_path).await?,
        };
        sessions.insert(
            session_id.to_owned(),
            OpenSession {
                meta,
                runtime: runtime.clone(),
            },
        );
        Ok(runtime)
    }

    fn session_directory(&self, session_id: &str) -> PathBuf {
        self.sessions_directory.join(session_id)
    }

    /// Sessions bound to `workspace` found on disk. In-memory entries are
    /// merged by the callers so freshly created sessions list immediately.
    async fn scan_workspace_sessions(
        &self,
        workspace: &str,
    ) -> Result<Vec<PersistedSession>, RegistryError> {
        let mut found = Vec::new();
        let mut entries = match fs::read_dir(&self.sessions_directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(found),
            Err(error) => return Err(error.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if !file_type.is_dir() {
                // Stray files (e.g. a quarantined legacy log) are not sessions.
                continue;
            }
            let metadata_path = entry.path().join(METADATA_FILE);
            let contents = match fs::read(&metadata_path).await {
                Ok(contents) => contents,
                Err(_) => continue,
            };
            let Ok(meta) = serde_json::from_slice::<PersistedSession>(&contents) else {
                // Unreadable or legacy-shaped metadata: skip, never block listing.
                continue;
            };
            if meta.workspace == workspace {
                found.push(meta);
            }
        }
        Ok(found)
    }
}

struct NoAgentFactory;

impl AgentFactory for NoAgentFactory {
    fn create(&self, _workspace: &Path) -> Result<Option<Arc<dyn Agent>>, String> {
        Ok(None)
    }
}

/// Fresh sessions pair the workspace fingerprint with random bytes so they
/// stay filesystem-safe, unique per workspace, and sortable by creation.
fn unique_session_id(workspace: &str) -> Result<String, RegistryError> {
    let digest = Sha256::digest(workspace.as_bytes());
    let mut random = [0_u8; 4];
    getrandom::fill(&mut random).map_err(|error| {
        RegistryError::InvalidMetadata(format!("could not generate a session ID: {error}"))
    })?;
    let mut id = String::with_capacity(32);
    id.push_str("session-");
    for byte in &digest[..8] {
        write!(&mut id, "{byte:02x}").expect("writing to a string cannot fail");
    }
    id.push('-');
    for byte in random {
        write!(&mut id, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Ok(id)
}

fn validate_session_id(session_id: &str) -> Result<(), RegistryError> {
    let rest = session_id
        .strip_prefix("session-")
        .ok_or_else(|| RegistryError::InvalidSessionId(session_id.to_owned()))?;
    let hex = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_hexdigit());
    let valid = rest.len() <= 80
        && rest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        && !rest.starts_with('-')
        && !rest.ends_with('-')
        && !rest.contains("--")
        && rest.split('-').all(hex);
    if valid {
        Ok(())
    } else {
        Err(RegistryError::InvalidSessionId(session_id.to_owned()))
    }
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
        reasoning_effort: Option<ReasoningEffort>,
        allowed_tools: Option<Vec<String>>,
        text: &str,
    ) -> Result<Event, SessionError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(SessionCommand::AppendMessage {
                client_id: client_id.to_owned(),
                model: model.clone(),
                reasoning_effort,
                allowed_tools,
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
        reasoning_effort: Option<ReasoningEffort>,
        allowed_tools: Option<Vec<String>>,
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
                reasoning_effort,
                allowed_tools,
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
                        reasoning_effort,
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
                            reasoning_effort,
                            allowed_tools,
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
            if event.session_id() != session_id {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "event log line {} belongs to session {:?}, not {session_id:?}",
                        index + 1,
                        event.session_id()
                    ),
                )
                .into());
            }
            if event.sequence() != expected_sequence {
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

async fn atomic_write(path: &Path, contents: Vec<u8>) -> io::Result<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;

        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("file has no parent directory"))?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&contents)?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    })
    .await
    .map_err(io::Error::other)?
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
            Self::Workspace(_) | Self::InvalidWorkspace(_) | Self::InvalidSessionId(_)
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
    use tempfile::tempdir;

    use super::{RegistryError, SessionRegistry};

    /// Bumps wall-clock time past a millisecond boundary so activity stamps
    /// differ deterministically.
    async fn tick() {
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }

    #[tokio::test]
    async fn every_created_session_gets_a_distinct_id() {
        let state = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let registry = SessionRegistry::without_agent(state.path());

        let first = registry.create_session(workspace.path()).await.unwrap();
        tick().await;
        let second = registry.create_session(workspace.path()).await.unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(first.workspace, second.workspace);

        // Re-opening by ID works across registry instances.
        assert!(
            SessionRegistry::without_agent(state.path())
                .session(&second.id)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn creates_and_lists_multiple_sessions_per_workspace() {
        let state = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let registry = SessionRegistry::without_agent(state.path());

        let first = registry.create_session(workspace.path()).await.unwrap();
        tick().await;
        let second = registry.create_session(workspace.path()).await.unwrap();
        assert_ne!(first.id, second.id);

        let listed = registry.list_sessions(workspace.path()).await.unwrap();
        assert_eq!(listed.len(), 2);
        // Most recently opened first: the freshly created session.
        assert_eq!(listed[0].id, second.id);
        assert_eq!(listed[1].id, first.id);
    }

    #[tokio::test]
    async fn workspaces_keep_independent_session_lists() {
        let state = tempdir().unwrap();
        let workspace_a = tempdir().unwrap();
        let workspace_b = tempdir().unwrap();
        let registry = SessionRegistry::without_agent(state.path());

        registry.create_session(workspace_a.path()).await.unwrap();
        tick().await;
        let b_only = registry.create_session(workspace_b.path()).await.unwrap();
        tick().await;
        registry.create_session(workspace_a.path()).await.unwrap();

        let listed_a = registry.list_sessions(workspace_a.path()).await.unwrap();
        assert_eq!(listed_a.len(), 2);
        let listed_b = registry.list_sessions(workspace_b.path()).await.unwrap();
        assert_eq!(listed_b.len(), 1);
        assert_eq!(listed_b[0].id, b_only.id);
    }

    #[tokio::test]
    async fn rejects_a_session_id_that_could_escape_the_sessions_directory() {
        let state = tempdir().unwrap();
        let registry = SessionRegistry::without_agent(state.path());

        assert!(matches!(
            registry.session("../../etc").await,
            Err(RegistryError::InvalidSessionId(_))
        ));
    }
}
