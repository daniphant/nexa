use std::{
    error::Error,
    fmt, io,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use nexa_protocol::{Event, LOCAL_SESSION_ID};
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
        let event_log_path = event_log_path.as_ref();
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
        let task = tokio::spawn(run_session(event_log, events, receiver));
        drop(task);

        Ok(Self { commands })
    }

    pub async fn append_message(&self, client_id: &str, text: &str) -> Result<Event, SessionError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(SessionCommand::AppendMessage {
                client_id: client_id.to_owned(),
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
        text: String,
        reply: oneshot::Sender<Result<Event, SessionError>>,
    },
    Subscribe {
        reply: oneshot::Sender<mpsc::UnboundedReceiver<Event>>,
    },
}

async fn run_session(
    mut event_log: File,
    mut events: Vec<Event>,
    mut commands: mpsc::Receiver<SessionCommand>,
) {
    let mut subscribers: Vec<mpsc::UnboundedSender<Event>> = Vec::new();

    while let Some(command) = commands.recv().await {
        match command {
            SessionCommand::AppendMessage {
                client_id,
                text,
                reply,
            } => {
                let event = Event::Message {
                    session_id: LOCAL_SESSION_ID.to_owned(),
                    sequence: u64::try_from(events.len()).unwrap_or(u64::MAX) + 1,
                    client_id,
                    text,
                    created_at_ms: timestamp_ms(),
                };

                let result = persist_event(&mut event_log, &event).await.map(|()| {
                    events.push(event.clone());
                    subscribers.retain(|subscriber| subscriber.send(event.clone()).is_ok());
                    event
                });
                let _ = reply.send(result);
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
    Closed,
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Closed => formatter.write_str("session runtime closed"),
        }
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Closed => None,
        }
    }
}

impl From<io::Error> for SessionError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
