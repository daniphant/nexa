use std::{error::Error, fmt, io};

use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event as TerminalEvent, EventStream, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use nexa_client::{ClientError, NexaClient};
use nexa_protocol::{Event, ModelRef, ProviderSummary};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

pub async fn run(client: NexaClient) -> Result<(), TuiError> {
    let providers = client.providers().await?;
    let mut app = App::new(providers)?;
    let mut events = client.subscribe().await?;
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let mut terminal_events = EventStream::new();

    loop {
        terminal.draw(|frame| render(frame, &mut app))?;
        tokio::select! {
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(event)) => {
                        if handle_terminal_event(event, &client, &mut app).await? {
                            break;
                        }
                    }
                    Some(Err(error)) => return Err(error.into()),
                    None => return Err(TuiError::TerminalEventsClosed),
                }
            }
            event = events.next() => app.apply(event?),
        }
    }

    Ok(())
}

async fn handle_terminal_event(
    event: TerminalEvent,
    client: &NexaClient,
    app: &mut App,
) -> Result<bool, TuiError> {
    match event {
        TerminalEvent::Key(key)
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
        {
            handle_key(key, client, app).await
        }
        TerminalEvent::Mouse(mouse) => {
            match mouse.kind {
                MouseEventKind::ScrollUp => app.scroll_up(3),
                MouseEventKind::ScrollDown => app.scroll_down(3),
                _ => {}
            }
            Ok(false)
        }
        TerminalEvent::Resize(_, _)
        | TerminalEvent::FocusGained
        | TerminalEvent::FocusLost
        | TerminalEvent::Paste(_) => Ok(false),
        TerminalEvent::Key(_) => Ok(false),
    }
}

async fn handle_key(key: KeyEvent, client: &NexaClient, app: &mut App) -> Result<bool, TuiError> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

    if app.picker_open {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => app.previous_model(),
            KeyCode::Down | KeyCode::Char('j') => app.next_model(),
            KeyCode::Enter => app.choose_model(),
            KeyCode::Esc => app.picker_open = false,
            _ => {}
        }
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('m') {
        app.open_model_picker();
        return Ok(false);
    }

    match key.code {
        KeyCode::Enter => {
            if app.input_text().trim() == "/quit" || app.input_text().trim() == "/exit" {
                return Ok(true);
            }
            if let Some(message) = app.message_to_send() {
                app.submitting = true;
                let result = client.send_message(app.model().clone(), message).await;
                app.submitting = false;
                match result {
                    Ok(_) => {
                        app.clear_input();
                        app.notice = None;
                        app.follow_tail = true;
                    }
                    Err(error) => app.notice = Some(error.to_string()),
                }
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
    Ok(false)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelChoice {
    provider_id: String,
    provider_name: String,
    model: String,
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
    fn new(providers: Vec<ProviderSummary>) -> Result<Self, TuiError> {
        let choices = providers
            .into_iter()
            .flat_map(|provider| {
                provider.models.into_iter().map(move |model| ModelChoice {
                    provider_id: provider.id.clone(),
                    provider_name: provider.name.clone(),
                    model,
                })
            })
            .collect::<Vec<_>>();
        if choices.is_empty() {
            return Err(TuiError::NoModels);
        }
        let picker_open = choices.len() > 1;
        Ok(Self {
            choices,
            selected_model: 0,
            picker_model: 0,
            picker_open,
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

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
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
        .unwrap_or(" Enter send  ·  Ctrl+M models  ·  PgUp/PgDn scroll  ·  Ctrl+C quit ");
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
    use nexa_protocol::{ApiFormat, Event, ProviderSummary, ToolCall, ToolResult};
    use ratatui::{Terminal, backend::TestBackend};

    use super::{App, RunState, ToolState, TranscriptItem, render};

    fn app() -> App {
        App::new(vec![ProviderSummary {
            id: "local".to_owned(),
            name: "Local".to_owned(),
            api_format: ApiFormat::ChatCompletions,
            models: vec!["model".to_owned()],
        }])
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
}
