use std::{
    error::Error,
    fmt, io,
    time::{SystemTime, UNIX_EPOCH},
};

use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event as TerminalEvent, EventStream, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use nexa_client::{ClientError, EventStream as SseStream, NexaClient};
use nexa_protocol::{Event, ModelRef, ProviderSummary, ReasoningEffort, SessionSummary};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use tokio::sync::mpsc;

/// Async work inside the TUI reports back through this channel so the main
/// loop can swap its event stream or open overlays.
pub enum Control {
    /// A freshly subscribed event stream, tagged with its session.
    StreamReady {
        session_id: String,
        stream: SseStream,
    },
    /// The client detached from its session (`/new` before any message).
    Detached,
    /// The listing requested by `/sessions`.
    Sessions(Vec<SessionSummary>),
    /// Background work failed; shown as a notice.
    Failed(String),
}

pub async fn run(
    client: NexaClient,
    workspace: String,
    preferred_model: Option<ModelRef>,
    preferred_effort: Option<ReasoningEffort>,
) -> Result<(ModelRef, Option<ReasoningEffort>), TuiError> {
    let providers = client.providers().await?;
    let mut app = App::new(providers, preferred_model, preferred_effort)?;
    // Lazy sessions: nothing is created until the first message is sent, so
    // there is no stream to subscribe to at startup.
    let mut events: Option<SseStream> = if client.has_session() {
        Some(client.subscribe().await?)
    } else {
        None
    };
    let (controls, mut control_events) = mpsc::unbounded_channel::<Control>();
    let mut client = client;
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let mut terminal_events = EventStream::new();
    let (submission_results, mut submissions) = mpsc::unbounded_channel();

    loop {
        terminal.draw(|frame| render(frame, &mut app))?;
        tokio::select! {
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(event)) => {
                        if handle_terminal_event(
                            event,
                            &mut client,
                            &workspace,
                            &controls,
                            &submission_results,
                            &mut app,
                        ) {
                            break;
                        }
                    }
                    Some(Err(error)) => return Err(error.into()),
                    None => return Err(TuiError::TerminalEventsClosed),
                }
            }
            event = async { events.as_mut().expect("stream present").next().await }, if events.is_some() => {
                app.apply(event?);
            }
            submission = submissions.recv() => {
                if let Some(submission) = submission {
                    app.finish_submission(submission);
                }
            }
            control = control_events.recv() => {
                match control {
                    Some(Control::StreamReady { session_id, stream }) => {
                        client.select_session(&session_id);
                        events = Some(stream);
                        app.reset_for_session_switch();
                    }
                    Some(Control::Detached) => {
                        client.clear_session();
                        events = None;
                        app.reset_for_session_switch();
                    }
                    Some(Control::Sessions(sessions)) => app.open_sessions_overlay(sessions),
                    Some(Control::Failed(error)) => app.notice = Some(error),
                    None => {}
                }
            }
        }
    }

    Ok((app.model(), app.effort))
}

fn handle_terminal_event(
    event: TerminalEvent,
    client: &mut NexaClient,
    workspace: &str,
    controls: &mpsc::UnboundedSender<Control>,
    submissions: &mpsc::UnboundedSender<Submission>,
    app: &mut App,
) -> bool {
    match event {
        TerminalEvent::Key(key)
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
        {
            handle_key(key, client, workspace, controls, submissions, app)
        }
        TerminalEvent::Mouse(mouse) => {
            match mouse.kind {
                MouseEventKind::ScrollUp => app.scroll_up(3),
                MouseEventKind::ScrollDown => app.scroll_down(3),
                _ => {}
            }
            false
        }
        TerminalEvent::Resize(_, _)
        | TerminalEvent::FocusGained
        | TerminalEvent::FocusLost
        | TerminalEvent::Paste(_) => false,
        TerminalEvent::Key(_) => false,
    }
}

fn handle_key(
    key: KeyEvent,
    client: &mut NexaClient,
    workspace: &str,
    controls: &mpsc::UnboundedSender<Control>,
    submissions: &mpsc::UnboundedSender<Submission>,
    app: &mut App,
) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }

    if app.sessions_open {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => app.previous_session(),
            KeyCode::Down | KeyCode::Char('j') => app.next_session(),
            KeyCode::Enter => {
                if let Some(summary) = app.choose_session() {
                    switch_to_session(client, controls, &summary);
                }
            }
            KeyCode::Esc => app.sessions_open = false,
            _ => {}
        }
        return false;
    }

    if app.picker_open {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => app.previous_model(),
            KeyCode::Down | KeyCode::Char('j') => app.next_model(),
            KeyCode::Enter => app.choose_model(),
            KeyCode::Esc => app.picker_open = false,
            _ => {}
        }
        return false;
    }

    if key.code == KeyCode::F(2) {
        app.open_model_picker();
        return false;
    }

    match key.code {
        KeyCode::Enter => {
            let input = app.input_text();
            let trimmed_input = input.trim();
            if trimmed_input.starts_with('/') {
                if execute_command(trimmed_input, client, workspace, controls, app) {
                    return true;
                }
                return false;
            }
            if let Some((model, message)) = app.begin_submission() {
                let effort = app.effort;
                let preset = None; // the TUI does not use agent presets yet
                let mut task_client = client.clone();
                let workspace_owned = workspace.to_owned();
                let submissions = submissions.clone();
                let controls = controls.clone();
                let task = tokio::spawn(async move {
                    // Lazy sessions: the first message materializes one.
                    if !task_client.has_session() {
                        match task_client.create_session(&workspace_owned).await {
                            Ok(info) => {
                                task_client.select_session(&info.id);
                                match task_client.subscribe().await {
                                    Ok(stream) => {
                                        let _ = controls.send(Control::StreamReady {
                                            session_id: info.id,
                                            stream,
                                        });
                                    }
                                    Err(error) => {
                                        let _ = controls.send(Control::Failed(error.to_string()));
                                    }
                                }
                            }
                            Err(error) => {
                                let _ = controls.send(Control::Failed(error.to_string()));
                            }
                        }
                    }
                    let result = task_client
                        .send_message(model, preset, effort, message.clone())
                        .await
                        .map(|_| ())
                        .map_err(|error| error.to_string());
                    let _ = submissions.send(Submission { message, result });
                });
                drop(task);
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => app.clear_input(),
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            app.insert(character);
        }
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete(),
        KeyCode::Left => app.move_left(),
        KeyCode::Right => app.move_right(),
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.input.len(),
        KeyCode::PageUp => app.scroll_up(8),
        KeyCode::PageDown => app.scroll_down(8),
        KeyCode::Up => app.scroll_up(1),
        KeyCode::Down => app.scroll_down(1),
        _ => {}
    }
    false
}

struct Submission {
    message: String,
    result: Result<(), String>,
}

/// `/sessions` — fetches the workspace's sessions and opens the overlay.
fn list_sessions(client: &NexaClient, workspace: &str, controls: &mpsc::UnboundedSender<Control>) {
    let client = client.clone();
    let workspace = workspace.to_owned();
    let controls = controls.clone();
    let task = tokio::spawn(async move {
        match client.list_sessions(workspace).await {
            Ok(sessions) => {
                let _ = controls.send(Control::Sessions(sessions));
            }
            Err(error) => {
                let _ = controls.send(Control::Failed(error.to_string()));
            }
        }
    });
    drop(task);
}

/// Dispatches a `/command` line. Returns `true` when the TUI should exit.
fn execute_command(
    raw: &str,
    client: &mut NexaClient,
    workspace: &str,
    controls: &mpsc::UnboundedSender<Control>,
    app: &mut App,
) -> bool {
    let mut parts = raw[1..].split_whitespace();
    match parts
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "new" => {
            start_new_session(client, workspace, controls);
            app.notice = Some("Creating a new chat…".to_owned());
            app.clear_input();
            false
        }
        "sessions" => {
            list_sessions(client, workspace, controls);
            app.clear_input();
            false
        }
        _ => app.execute_command(raw),
    }
}

/// `/new` — detaches from the current chat. A replacement conversation only
/// materializes when its first message is sent, so abandoned fresh starts
/// never touch disk.
fn start_new_session(
    client: &mut NexaClient,
    workspace: &str,
    controls: &mpsc::UnboundedSender<Control>,
) {
    let _ = workspace;
    client.clear_session();
    let _ = controls.send(Control::Detached);
}

/// `/sessions` overlay Enter — subscribes to the chosen session.
fn switch_to_session(
    client: &mut NexaClient,
    controls: &mpsc::UnboundedSender<Control>,
    summary: &SessionSummary,
) {
    client.select_session(&summary.id);
    let session_id = summary.id.clone();
    let switched = client.clone();
    let controls = controls.clone();
    let task = tokio::spawn(async move {
        match switched.subscribe().await {
            Ok(stream) => {
                let _ = controls.send(Control::StreamReady { session_id, stream });
            }
            Err(error) => {
                let _ = controls.send(Control::Failed(error.to_string()));
            }
        }
    });
    drop(task);
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelChoice {
    provider_id: String,
    provider_name: String,
    model: String,
    reasoning_efforts: Option<Vec<ReasoningEffort>>,
}

impl ModelChoice {
    fn model_ref(&self) -> ModelRef {
        ModelRef {
            provider: self.provider_id.clone(),
            id: self.model.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RunState {
    Idle,
    Running,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TranscriptItem {
    User(String),
    Assistant {
        run_id: String,
        text: String,
        finalized: bool,
    },
    Tool {
        call_id: String,
        name: String,
        state: ToolState,
    },
    Error(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ToolState {
    Running,
    Completed,
    Failed,
}

struct App {
    choices: Vec<ModelChoice>,
    selected_model: usize,
    picker_model: usize,
    picker_open: bool,
    effort: Option<ReasoningEffort>,
    sessions: Option<Vec<SessionSummary>>,
    sessions_cursor: usize,
    sessions_open: bool,
    transcript: Vec<TranscriptItem>,
    input: Vec<char>,
    cursor: usize,
    run_state: RunState,
    submitting: bool,
    notice: Option<String>,
    scroll: usize,
    follow_tail: bool,
}

impl App {
    fn new(
        providers: Vec<ProviderSummary>,
        preferred_model: Option<ModelRef>,
        preferred_effort: Option<ReasoningEffort>,
    ) -> Result<Self, TuiError> {
        let choices = providers
            .into_iter()
            .flat_map(|provider| {
                provider.models.into_iter().map(move |model| ModelChoice {
                    provider_id: provider.id.clone(),
                    provider_name: provider.name.clone(),
                    model: model.id,
                    reasoning_efforts: model.reasoning_efforts,
                })
            })
            .collect::<Vec<_>>();
        if choices.is_empty() {
            return Err(TuiError::NoModels);
        }
        // A previously persisted default that still exists is selected
        // silently; anything else falls back to asking when there is a
        // choice to make.
        let selected_model = preferred_model.and_then(|preferred| {
            choices.iter().position(|choice| {
                choice.provider_id == preferred.provider && choice.model == preferred.id
            })
        });
        let picker_open = selected_model.is_none() && choices.len() > 1;
        Ok(Self {
            choices,
            selected_model: selected_model.unwrap_or(0),
            picker_model: 0,
            picker_open,
            effort: preferred_effort,
            sessions: None,
            sessions_cursor: 0,
            sessions_open: false,
            transcript: Vec::new(),
            input: Vec::new(),
            cursor: 0,
            run_state: RunState::Idle,
            submitting: false,
            notice: None,
            scroll: 0,
            follow_tail: true,
        })
    }

    fn model_choice(&self) -> &ModelChoice {
        &self.choices[self.selected_model]
    }

    fn model(&self) -> ModelRef {
        self.model_choice().model_ref()
    }

    fn open_model_picker(&mut self) {
        self.picker_model = self.selected_model;
        self.picker_open = true;
    }

    fn previous_model(&mut self) {
        self.picker_model = self
            .picker_model
            .checked_sub(1)
            .unwrap_or(self.choices.len() - 1);
    }

    fn next_model(&mut self) {
        self.picker_model = (self.picker_model + 1) % self.choices.len();
    }

    fn choose_model(&mut self) {
        self.selected_model = self.picker_model;
        self.picker_open = false;
        self.drop_unsupported_effort();
    }

    fn open_sessions_overlay(&mut self, sessions: Vec<SessionSummary>) {
        if sessions.is_empty() {
            self.notice = Some("No saved sessions for this workspace yet.".to_owned());
            return;
        }
        self.sessions_cursor = 0;
        self.sessions = Some(sessions);
        self.sessions_open = true;
    }

    fn previous_session(&mut self) {
        let Some(sessions) = &self.sessions else {
            return;
        };
        self.sessions_cursor = self
            .sessions_cursor
            .checked_sub(1)
            .unwrap_or(sessions.len() - 1);
    }

    fn next_session(&mut self) {
        let Some(sessions) = &self.sessions else {
            return;
        };
        self.sessions_cursor = (self.sessions_cursor + 1) % sessions.len();
    }

    fn choose_session(&mut self) -> Option<SessionSummary> {
        let summary = self
            .sessions
            .as_ref()
            .and_then(|sessions| sessions.get(self.sessions_cursor))
            .cloned();
        self.sessions_open = false;
        summary
    }

    /// Drops presentation state so the freshly subscribed stream can replay
    /// another session's transcript into a clean slate.
    fn reset_for_session_switch(&mut self) {
        self.transcript.clear();
        self.scroll = 0;
        self.follow_tail = true;
        self.submitting = false;
        self.run_state = RunState::Idle;
        self.notice = None;
    }

    /// The efforts selectable for the active model.
    ///
    /// A declared list is used as-is. When the provider did not say, the
    /// widely-supported [`ReasoningEffort::COMMON`] set is offered instead of
    /// every wire value.
    fn offered_efforts(&self) -> &[ReasoningEffort] {
        match self.model_choice().reasoning_efforts.as_deref() {
            Some(efforts) => efforts,
            None => nexa_harness::inferred_reasoning_efforts(&self.model_choice().model)
                .unwrap_or(&ReasoningEffort::COMMON),
        }
    }

    fn supports_effort(&self, effort: ReasoningEffort) -> bool {
        self.offered_efforts().contains(&effort)
    }

    /// Drops the current effort when a freshly selected model does not
    /// report supporting it.
    fn drop_unsupported_effort(&mut self) {
        if let Some(effort) = self.effort
            && !self.supports_effort(effort)
        {
            self.effort = None;
        }
    }

    fn input_text(&self) -> String {
        self.input.iter().collect()
    }

    fn message_to_send(&self) -> Option<String> {
        if self.submitting {
            return None;
        }
        let message = self.input_text();
        (!message.trim().is_empty()).then_some(message)
    }

    fn begin_submission(&mut self) -> Option<(ModelRef, String)> {
        let message = self.message_to_send()?;
        let model = self.model();
        self.submitting = true;
        self.notice = None;
        self.clear_input();
        Some((model, message))
    }

    fn finish_submission(&mut self, submission: Submission) {
        self.submitting = false;
        match submission.result {
            Ok(()) => self.follow_tail = true,
            Err(error) => {
                if self.input.is_empty() {
                    self.input = submission.message.chars().collect();
                    self.cursor = self.input.len();
                }
                self.notice = Some(error);
            }
        }
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    /// Executes a `/command` line. Returns `true` when the TUI should exit.
    fn execute_command(&mut self, raw: &str) -> bool {
        let mut parts = raw[1..].split_whitespace();
        let name = parts.next().unwrap_or_default().to_ascii_lowercase();
        let argument = parts.next();
        match name.as_str() {
            "quit" | "exit" => true,
            "clear" => {
                // Presentation-only: the durable event log stays intact.
                self.transcript.clear();
                self.scroll = 0;
                self.follow_tail = true;
                self.notice = Some("Cleared the transcript view.".to_owned());
                false
            }
            "model" => match argument {
                None => {
                    self.open_model_picker();
                    false
                }
                Some(spec) => {
                    self.select_model_by_spec(spec);
                    false
                }
            },
            "reasoning" => match argument {
                None => {
                    self.cycle_effort();
                    false
                }
                Some(level) => {
                    self.set_effort(level);
                    false
                }
            },
            "" => {
                self.notice = Some(
                    "Usage: /new · /sessions · /model [provider/model] · /reasoning [effort] · /clear"
                        .to_owned(),
                );
                false
            }
            other => {
                self.notice = Some(format!(
                    "Unknown command /{other}. Available: /new, /sessions, /model, /reasoning, /clear, /quit."
                ));
                false
            }
        }
    }

    /// Switches models from a `"provider-id/model-id"` spec, falling back to
    /// a unique bare model-ID match.
    fn select_model_by_spec(&mut self, spec: &str) {
        let position = match spec.split_once('/') {
            Some((provider_id, model)) => self
                .choices
                .iter()
                .position(|choice| choice.provider_id == provider_id && choice.model == model),
            None => {
                let matches = self
                    .choices
                    .iter()
                    .enumerate()
                    .filter(|(_, choice)| choice.model == spec)
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();
                matches.first().copied().filter(|_| matches.len() == 1)
            }
        };
        match position {
            Some(index) => {
                let name = format!(
                    "{} / {}",
                    self.choices[index].provider_name, self.choices[index].model
                );
                self.selected_model = index;
                self.drop_unsupported_effort();
                self.notice = Some(format!("Switched to {name}."));
            }
            None => {
                self.notice = Some(format!("No model matches {spec:?}."));
            }
        }
    }

    /// Steps through no-effort and every effort level the active model
    /// supports.
    fn cycle_effort(&mut self) {
        let mut levels = vec![None];
        levels.extend(self.offered_efforts().iter().copied().map(Some));
        let current = levels.iter().position(|level| *level == self.effort);
        let next = current.map_or(0, |index| (index + 1) % levels.len());
        self.effort = levels[next];
        self.notice = Some(match self.effort {
            Some(effort) => format!("Reasoning effort: {}.", effort.as_str()),
            None => "Reasoning effort: default.".to_owned(),
        });
    }

    fn set_effort(&mut self, raw_level: &str) {
        match ReasoningEffort::parse(raw_level) {
            Some(effort) if self.supports_effort(effort) => {
                self.effort = Some(effort);
                self.notice = Some(format!("Reasoning effort: {}.", effort.as_str()));
            }
            Some(_) => {
                self.notice = Some(format!(
                    "{} does not report supporting effort {raw_level:?}.",
                    self.model_choice().model
                ));
            }
            None => {
                let levels = ReasoningEffort::ALL
                    .map(|effort| effort.as_str())
                    .join(", ");
                self.notice = Some(format!("Unknown effort {raw_level:?}. Levels: {levels}."));
            }
        }
    }

    fn insert(&mut self, character: char) {
        self.input.insert(self.cursor, character);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.input.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
        }
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.len());
    }

    fn scroll_up(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
        self.follow_tail = false;
    }

    fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_add(lines);
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Message { text, .. } => self.transcript.push(TranscriptItem::User(text)),
            Event::RunStarted { .. } => self.run_state = RunState::Running,
            Event::AssistantTextDelta { run_id, text, .. } => {
                if let Some(TranscriptItem::Assistant {
                    run_id: item_run_id,
                    text: item_text,
                    finalized: false,
                }) = self.transcript.last_mut()
                    && *item_run_id == run_id
                {
                    item_text.push_str(&text);
                } else {
                    self.transcript.push(TranscriptItem::Assistant {
                        run_id,
                        text,
                        finalized: false,
                    });
                }
            }
            Event::AssistantMessage { run_id, text, .. } => {
                if let Some(TranscriptItem::Assistant {
                    run_id: item_run_id,
                    text: item_text,
                    finalized,
                }) = self.transcript.last_mut()
                    && *item_run_id == run_id
                    && !*finalized
                {
                    *item_text = text;
                    *finalized = true;
                } else {
                    self.transcript.push(TranscriptItem::Assistant {
                        run_id,
                        text,
                        finalized: true,
                    });
                }
            }
            Event::ToolCallStarted { call, .. } => self.transcript.push(TranscriptItem::Tool {
                call_id: call.id,
                name: call.name,
                state: ToolState::Running,
            }),
            Event::ToolCallCompleted { result, .. } => {
                if let Some(TranscriptItem::Tool { state, .. }) =
                    self.transcript.iter_mut().rev().find(|item| {
                        matches!(item, TranscriptItem::Tool { call_id, .. } if *call_id == result.tool_call_id)
                    })
                {
                    *state = if result.is_error {
                        ToolState::Failed
                    } else {
                        ToolState::Completed
                    };
                }
            }
            Event::RunCompleted { .. } => self.run_state = RunState::Idle,
            Event::RunFailed { error, .. } => {
                self.run_state = RunState::Failed;
                self.transcript.push(TranscriptItem::Error(error));
            }
        }
    }
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, app, sections[0]);
    render_transcript(frame, app, sections[1]);
    render_composer(frame, app, sections[2]);
    render_footer(frame, app, sections[3]);
    if app.sessions_open {
        render_sessions_picker(frame, app, area);
    }
    if app.picker_open {
        render_model_picker(frame, app, area);
    }
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let model = app.model_choice();
    let state = if app.submitting {
        "sending"
    } else {
        match app.run_state {
            RunState::Idle => "idle",
            RunState::Running => "running",
            RunState::Failed => "failed",
        }
    };
    let line = Line::from(vec![
        Span::styled(
            " NEXA ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  local  "),
        Span::styled(
            format!("{} / {}", model.provider_name, model.model),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("  ·  "),
        Span::styled(
            match app.effort {
                Some(effort) => format!("effort {}", effort.as_str()),
                None => "effort default".to_owned(),
            },
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("  ·  "),
        Span::styled(state, state_style(&app.run_state)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn state_style(state: &RunState) -> Style {
    match state {
        RunState::Idle => Style::default().fg(Color::DarkGray),
        RunState::Running => Style::default().fg(Color::Yellow),
        RunState::Failed => Style::default().fg(Color::Red),
    }
}

fn render_transcript(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let lines = transcript_lines(app);
    let line_count = visual_line_count(&lines, area.width);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max_scroll = line_count.saturating_sub(usize::from(area.height));
    if app.follow_tail {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll == max_scroll {
            app.follow_tail = true;
        }
    }
    let scroll = u16::try_from(app.scroll).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((scroll, 0)), area);
}

fn visual_line_count(lines: &[Line<'_>], width: u16) -> usize {
    let width = usize::from(width.max(1));
    lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum()
}

fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    if app.transcript.is_empty() {
        return vec![
            Line::default(),
            Line::from("  Start a conversation with the selected model.")
                .style(Style::default().fg(Color::DarkGray)),
        ];
    }

    let mut lines = Vec::new();
    for item in &app.transcript {
        match item {
            TranscriptItem::User(text) => {
                if !lines.is_empty() {
                    lines.push(Line::default());
                }
                lines.push(Line::from(Span::styled(
                    "You",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.extend(text.lines().map(|line| Line::from(line.to_owned())));
            }
            TranscriptItem::Assistant { text, .. } if !text.is_empty() => {
                if !lines.is_empty() {
                    lines.push(Line::default());
                }
                lines.push(Line::from(Span::styled(
                    "Assistant",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.extend(text.lines().map(|line| Line::from(line.to_owned())));
            }
            TranscriptItem::Tool { name, state, .. } => {
                let (label, color) = match state {
                    ToolState::Running => ("running", Color::Yellow),
                    ToolState::Completed => ("done", Color::DarkGray),
                    ToolState::Failed => ("failed", Color::Red),
                };
                lines.push(Line::from(vec![
                    Span::styled("  ↳ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(name.clone(), Style::default().fg(Color::Magenta)),
                    Span::styled(format!(" · {label}"), Style::default().fg(color)),
                ]));
            }
            TranscriptItem::Error(error) => {
                lines.push(Line::from(Span::styled(
                    format!("Run failed: {error}"),
                    Style::default().fg(Color::Red),
                )));
            }
            TranscriptItem::Assistant { .. } => {}
        }
    }
    lines
}

fn render_composer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Message ");
    let inner = block.inner(area);
    let input = app.input_text();
    let available_width = usize::from(inner.width.max(1));
    let horizontal_scroll = app.cursor.saturating_sub(available_width.saturating_sub(1));
    let text = if input.is_empty() {
        Line::from("Ask Nexa anything…").style(Style::default().fg(Color::DarkGray))
    } else {
        Line::from(input)
    };
    frame.render_widget(
        Paragraph::new(text)
            .block(block)
            .scroll((0, u16::try_from(horizontal_scroll).unwrap_or(u16::MAX))),
        area,
    );
    if !app.picker_open {
        let cursor_column = app.cursor.saturating_sub(horizontal_scroll);
        frame.set_cursor_position(Position::new(
            inner
                .x
                .saturating_add(u16::try_from(cursor_column).unwrap_or(u16::MAX)),
            inner.y,
        ));
    }
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let text = app
        .notice
        .as_deref()
        .unwrap_or(" Enter send  ·  F2 models  ·  PgUp/PgDn scroll  ·  Ctrl+C quit ");
    let style = if app.notice.is_some() {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

fn render_model_picker(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let height = u16::try_from(app.choices.len().saturating_add(4))
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(2));
    let popup = centered_rect(70, height.max(5), area);
    let lines = app
        .choices
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            let marker = if index == app.picker_model {
                "› "
            } else {
                "  "
            };
            let style = if index == app.picker_model {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(format!(
                "{marker}{} / {}  ({})",
                choice.provider_name, choice.model, choice.provider_id
            ))
            .style(style)
        })
        .chain(std::iter::once(Line::default()))
        .chain(std::iter::once(
            Line::from("↑/↓ select  ·  Enter choose  ·  Esc close")
                .style(Style::default().fg(Color::DarkGray)),
        ))
        .collect::<Vec<_>>();
    let picker = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" Model "),
        )
        .alignment(Alignment::Left);
    frame.render_widget(Clear, popup);
    frame.render_widget(picker, popup);
}

fn render_sessions_picker(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(sessions) = &app.sessions else {
        return;
    };
    let now = current_ms();
    let height = u16::try_from(sessions.len().saturating_add(4))
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(2));
    let popup = centered_rect(70, height.max(5), area);
    let lines = sessions
        .iter()
        .enumerate()
        .map(|(index, summary)| {
            let marker = if index == app.sessions_cursor {
                "› "
            } else {
                "  "
            };
            let style = if index == app.sessions_cursor {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(format!(
                "{marker}{}  ·  last opened {}",
                short_session_id(&summary.id),
                relative_time(summary.last_opened_ms, now),
            ))
            .style(style)
        })
        .chain(std::iter::once(Line::default()))
        .chain(std::iter::once(
            Line::from("↑/↓ select  ·  Enter switch  ·  Esc close")
                .style(Style::default().fg(Color::DarkGray)),
        ))
        .collect::<Vec<_>>();
    let picker = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" Sessions "),
        )
        .alignment(Alignment::Left);
    frame.render_widget(Clear, popup);
    frame.render_widget(picker, popup);
}

/// Trims the deterministic prefix for display: `session-1a2b…-9c8d7e6f`.
fn short_session_id(id: &str) -> String {
    let rest = id.strip_prefix("session-").unwrap_or(id);
    match rest.split_once('-') {
        Some((fingerprint, suffix)) => {
            format!("{}…{suffix}", &fingerprint[..4.min(fingerprint.len())])
        }
        None => id.to_owned(),
    }
}

/// Coarse "time since" label; precise stamps live in session metadata.
fn relative_time(past_ms: u64, now_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(past_ms) / 1000;
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

fn current_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

fn centered_rect(width_percent: u16, height: u16, area: Rect) -> Rect {
    let vertical_margin = area.height.saturating_sub(height) / 2;
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(vertical_margin),
            Constraint::Length(height.min(area.height)),
            Constraint::Min(0),
        ])
        .split(area)[1];
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical)[1]
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

#[derive(Debug)]
pub enum TuiError {
    Client(ClientError),
    Io(io::Error),
    NoModels,
    TerminalEventsClosed,
}

impl fmt::Display for TuiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "{error}"),
            Self::NoModels => formatter
                .write_str("the runtime has no configured models; run `nexa provider add` first"),
            Self::TerminalEventsClosed => formatter.write_str("terminal event stream closed"),
        }
    }
}

impl Error for TuiError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Client(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::NoModels | Self::TerminalEventsClosed => None,
        }
    }
}

impl From<ClientError> for TuiError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

impl From<io::Error> for TuiError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use nexa_protocol::{
        ApiFormat, Event, ModelSummary, ProviderSummary, ReasoningEffort, ToolCall, ToolResult,
    };
    use ratatui::{Terminal, backend::TestBackend};

    use super::{App, RunState, Submission, ToolState, TranscriptItem, render};

    fn app() -> App {
        App::new(
            vec![ProviderSummary {
                id: "local".to_owned(),
                name: "Local".to_owned(),
                api_format: ApiFormat::ChatCompletions,
                models: vec![ModelSummary {
                    id: "model".to_owned(),
                    reasoning_efforts: None,
                }],
            }],
            None,
            None,
        )
        .unwrap()
    }

    fn app_with_choices() -> App {
        App::new(
            vec![ProviderSummary {
                id: "local".to_owned(),
                name: "Local".to_owned(),
                api_format: ApiFormat::ChatCompletions,
                models: vec![
                    ModelSummary {
                        id: "plain".to_owned(),
                        reasoning_efforts: None,
                    },
                    ModelSummary {
                        id: "thinker".to_owned(),
                        reasoning_efforts: Some(vec![
                            ReasoningEffort::Low,
                            ReasoningEffort::High,
                            ReasoningEffort::XHigh,
                        ]),
                    },
                    ModelSummary {
                        id: "limited".to_owned(),
                        reasoning_efforts: Some(vec![ReasoningEffort::Minimal]),
                    },
                ],
            }],
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn final_assistant_message_replaces_streamed_draft_without_duplication() {
        let mut app = app();
        app.apply(Event::AssistantTextDelta {
            session_id: "local".to_owned(),
            sequence: 1,
            run_id: "run-1".to_owned(),
            text: "hel".to_owned(),
            created_at_ms: 1,
        });
        app.apply(Event::AssistantTextDelta {
            session_id: "local".to_owned(),
            sequence: 2,
            run_id: "run-1".to_owned(),
            text: "lo".to_owned(),
            created_at_ms: 2,
        });
        app.apply(Event::AssistantMessage {
            session_id: "local".to_owned(),
            sequence: 3,
            run_id: "run-1".to_owned(),
            text: "hello".to_owned(),
            tool_calls: Vec::new(),
            created_at_ms: 3,
        });

        assert_eq!(
            app.transcript,
            vec![TranscriptItem::Assistant {
                run_id: "run-1".to_owned(),
                text: "hello".to_owned(),
                finalized: true,
            }]
        );
    }

    #[test]
    fn a_second_inference_round_gets_its_own_assistant_item() {
        let mut app = app();
        for event in [
            Event::AssistantMessage {
                session_id: "local".to_owned(),
                sequence: 1,
                run_id: "run-1".to_owned(),
                text: "Checking.".to_owned(),
                tool_calls: Vec::new(),
                created_at_ms: 1,
            },
            Event::AssistantTextDelta {
                session_id: "local".to_owned(),
                sequence: 2,
                run_id: "run-1".to_owned(),
                text: "Done.".to_owned(),
                created_at_ms: 2,
            },
        ] {
            app.apply(event);
        }

        assert_eq!(app.transcript.len(), 2);
    }

    #[test]
    fn tool_completion_updates_the_existing_activity_row() {
        let mut app = app();
        app.apply(Event::ToolCallStarted {
            session_id: "local".to_owned(),
            sequence: 1,
            run_id: "run-1".to_owned(),
            call: ToolCall {
                id: "call-1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{}".to_owned(),
            },
            created_at_ms: 1,
        });
        app.apply(Event::ToolCallCompleted {
            session_id: "local".to_owned(),
            sequence: 2,
            run_id: "run-1".to_owned(),
            result: ToolResult {
                tool_call_id: "call-1".to_owned(),
                content: "ok".to_owned(),
                is_error: false,
            },
            created_at_ms: 2,
        });

        assert!(matches!(
            app.transcript.as_slice(),
            [TranscriptItem::Tool {
                state: ToolState::Completed,
                ..
            }]
        ));
    }

    #[test]
    fn run_failure_is_visible_but_does_not_make_the_composer_busy() {
        let mut app = app();
        app.apply(Event::RunFailed {
            session_id: "local".to_owned(),
            sequence: 1,
            run_id: "run-1".to_owned(),
            error: "connection lost".to_owned(),
            created_at_ms: 1,
        });
        app.insert('h');

        assert_eq!(app.run_state, RunState::Failed);
        assert_eq!(app.message_to_send().as_deref(), Some("h"));
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem::Error(error)) if error == "connection lost"
        ));
    }

    #[test]
    fn submission_state_is_visible_and_a_failed_message_can_be_retried() {
        let mut app = app();
        for character in "hello".chars() {
            app.insert(character);
        }

        let (_, message) = app.begin_submission().unwrap();
        assert!(app.submitting);
        assert!(app.input.is_empty());

        app.finish_submission(Submission {
            message,
            result: Err("server unavailable".to_owned()),
        });
        assert!(!app.submitting);
        assert_eq!(app.input_text(), "hello");
        assert_eq!(app.notice.as_deref(), Some("server unavailable"));
    }

    #[test]
    fn renders_the_session_transcript_and_composer() {
        let mut app = app();
        app.apply(Event::Message {
            session_id: "local".to_owned(),
            sequence: 1,
            client_id: "cli".to_owned(),
            model: app.model(),
            text: "Hello, Nexa".to_owned(),
            created_at_ms: 1,
        });
        let backend = TestBackend::new(80, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("NEXA"));
        assert!(rendered.contains("Hello, Nexa"));
        assert!(rendered.contains("Message"));
    }

    #[test]
    fn reasoning_command_sets_validated_efforts() {
        let mut app = app_with_choices();
        // "thinker" reports low / high / xhigh only.
        app.execute_command("/model local/thinker");
        assert_eq!(app.selected_model, 1);

        app.execute_command("/reasoning xhigh");
        assert_eq!(app.effort, Some(ReasoningEffort::XHigh));

        app.execute_command("/reasoning minimal");
        assert_eq!(
            app.effort,
            Some(ReasoningEffort::XHigh),
            "unsupported levels must be rejected"
        );
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("does not report supporting")
        );

        app.execute_command("/reasoning banana");
        assert_eq!(app.effort, Some(ReasoningEffort::XHigh));
        assert!(app.notice.as_deref().unwrap().contains("Unknown effort"));
    }

    #[test]
    fn reasoning_without_argument_cycles_supported_levels() {
        let mut app = app_with_choices();
        app.selected_model = 1; // "thinker": low, high, xhigh

        app.execute_command("/reasoning");
        assert_eq!(app.effort, Some(ReasoningEffort::Low));
        app.execute_command("/reasoning");
        assert_eq!(app.effort, Some(ReasoningEffort::High));
        app.execute_command("/reasoning");
        assert_eq!(app.effort, Some(ReasoningEffort::XHigh));
        app.execute_command("/reasoning");
        assert_eq!(app.effort, None);
    }

    #[test]
    fn model_command_switches_by_spec_or_bare_id_and_drops_efforts() {
        let mut app = app_with_choices();
        app.selected_model = 1;
        app.effort = Some(ReasoningEffort::XHigh);

        // "limited" only declares minimal, so xhigh must be dropped.
        app.execute_command("/model local/limited");
        assert_eq!(app.selected_model, 2);
        assert_eq!(app.effort, None);
        assert!(app.notice.as_deref().unwrap().contains("Switched"));

        // Unknown capabilities never restrict the effort.
        app.execute_command("/model local/plain");
        assert_eq!(app.selected_model, 0);
        app.effort = Some(ReasoningEffort::Max);

        // Bare model IDs match when unique.
        app.execute_command("/model limited");
        assert_eq!(app.selected_model, 2);
        assert_eq!(app.effort, None);

        app.execute_command("/model nope/missing");
        assert_eq!(app.selected_model, 2);
        assert!(app.notice.as_deref().unwrap().contains("No model matches"));
    }

    #[test]
    fn clear_command_resets_the_transcript_view_only() {
        let mut app = app();
        app.apply(Event::Message {
            session_id: "local".to_owned(),
            sequence: 1,
            client_id: "cli".to_owned(),
            model: app.model(),
            text: "Hello, Nexa".to_owned(),
            created_at_ms: 1,
        });
        assert!(!app.transcript.is_empty());

        assert!(!app.execute_command("/clear"));
        assert!(app.transcript.is_empty());
    }
}
