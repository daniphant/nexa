//! Application state: the `App` struct, the commands/events that cross the
//! UI/network boundary, and presentation folding of the durable event stream.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::mpsc,
    time::{SystemTime, UNIX_EPOCH},
};

use nexa_harness::{DesktopSettings, ModelsSettings, SettingsFile, SettingsFileError};
use nexa_protocol::{
    Event, ModelRef, PresetSummary, ProviderSummary, ReasoningEffort, SessionSummary,
};

use crate::widgets;

#[must_use]
pub fn effort_label(effort: Option<ReasoningEffort>) -> &'static str {
    match effort {
        None => "Default",
        Some(ReasoningEffort::Minimal) => "Minimal",
        Some(ReasoningEffort::Low) => "Low",
        Some(ReasoningEffort::Medium) => "Medium",
        Some(ReasoningEffort::High) => "High",
        Some(ReasoningEffort::XHigh) => "Extra high",
        Some(ReasoningEffort::Max) => "Max",
    }
}

#[must_use]
pub fn effort_hint(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Minimal => "Lowest latency, almost no extra thinking",
        ReasoningEffort::Low => "Faster, lighter reasoning",
        ReasoningEffort::Medium => "Balanced thinking and speed",
        ReasoningEffort::High => "Deeper reasoning for harder tasks",
        ReasoningEffort::XHigh => "Extended reasoning",
        ReasoningEffort::Max => "Largest reasoning budget",
    }
}

/// Commands the UI sends to the network actor.
pub enum UiCmd {
    Bootstrap,
    ListPresets,
    Send {
        workspace: String,
        model: ModelRef,
        preset: Option<String>,
        effort: Option<ReasoningEffort>,
        text: String,
    },
    /// Detach without creating anything; the next message starts a chat.
    NewChat,
    ListSessions {
        workspace: String,
    },
    /// Explicitly continue an existing conversation.
    SwitchTo {
        session_id: String,
    },
}

/// Results the actor pushes back; each one triggers a repaint.
pub enum NetEvent {
    Providers(Vec<ProviderSummary>),
    Presets(Vec<PresetSummary>),
    Sessions {
        workspace: String,
        sessions: Vec<SessionSummary>,
    },
    /// A brand-new session materialized for this chat.
    SessionReady(String),
    /// The UI switched to an existing session; drop local presentation state.
    Switched(String),
    Event(Event),
    Detached,
    Notice(String),
}

#[derive(Debug)]
pub enum TranscriptEntry {
    User {
        text: String,
    },
    Assistant {
        run_id: String,
        text: String,
        streaming: bool,
    },
    Tool {
        call_id: String,
        name: String,
        arguments: String,
        result: Option<String>,
        failed: bool,
    },
    Error {
        text: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Running,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    User,
    Run,
    Tool,
    Done,
    Error,
}

pub struct ActivityLine {
    pub kind: ActivityKind,
    pub text: String,
}

#[derive(Clone)]
pub struct ModelChoice {
    pub provider_id: String,
    pub provider_name: String,
    pub model: String,
    pub reasoning_efforts: Option<Vec<ReasoningEffort>>,
}

/// Sessions previously opened under one workspace, for the sidebar's
/// per-project tree.
pub struct WorkspaceSessions {
    pub workspace: String,
    pub sessions: Vec<SessionSummary>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Models,
    Presets,
}

pub struct App {
    pub net_tx: mpsc::Sender<UiCmd>,
    pub net_rx: mpsc::Receiver<NetEvent>,
    pub choices: Vec<ModelChoice>,
    pub selected_model: usize,
    pub presets: Vec<PresetSummary>,
    pub selected_preset: Option<String>,
    pub effort: Option<ReasoningEffort>,
    pub transcript: Vec<TranscriptEntry>,
    pub activity: Vec<ActivityLine>,
    pub sessions_by_workspace: Vec<WorkspaceSessions>,
    pub session_titles: HashMap<String, String>,
    pub active_session: Option<String>,
    pub run_state: RunState,
    pub notice: Option<String>,
    pub draft: String,
    pub connected: bool,
    /// The workspace a new chat will be created in; not necessarily the
    /// workspace of the currently open session.
    pub workspace: String,
    pub sidebar_filter: String,
    pub sidebar_open: bool,
    pub activity_open: bool,
    pub settings_open: bool,
    pub settings_tab: SettingsTab,
    pub focus_composer: bool,
    pub model_filter: String,
    pub focus_model_search: bool,
    settings_path: PathBuf,
    models_settings: ModelsSettings,
    pub desktop_settings: DesktopSettings,
}

impl App {
    #[must_use]
    pub fn new(
        net_tx: mpsc::Sender<UiCmd>,
        net_rx: mpsc::Receiver<NetEvent>,
        workspace: String,
        settings_path: PathBuf,
        settings: SettingsFile,
    ) -> Self {
        let selected_preset = settings.desktop.default_preset.clone();
        Self {
            net_tx,
            net_rx,
            choices: Vec::new(),
            selected_model: 0,
            presets: Vec::new(),
            selected_preset,
            effort: settings.models.default_reasoning_effort,
            transcript: Vec::new(),
            activity: Vec::new(),
            sessions_by_workspace: Vec::new(),
            session_titles: HashMap::new(),
            active_session: None,
            run_state: RunState::Idle,
            notice: None,
            draft: String::new(),
            connected: false,
            workspace,
            sidebar_filter: String::new(),
            sidebar_open: true,
            activity_open: false,
            settings_open: false,
            settings_tab: SettingsTab::General,
            focus_composer: true,
            model_filter: String::new(),
            focus_model_search: false,
            settings_path,
            models_settings: settings.models,
            desktop_settings: settings.desktop,
        }
    }

    /// The active workspace plus every recently used one, most-recent
    /// first, for the initial per-project session listing.
    #[must_use]
    pub fn known_workspaces(&self) -> Vec<String> {
        let mut workspaces = vec![self.workspace.clone()];
        for recent in &self.desktop_settings.recent_workspaces {
            if !workspaces.contains(recent) {
                workspaces.push(recent.clone());
            }
        }
        workspaces
    }

    /// Switches which workspace the next new chat will be created in,
    /// remembers it, persists that choice, and refreshes its session list.
    pub fn select_workspace(&mut self, workspace: String) {
        self.workspace = workspace.clone();
        self.desktop_settings.remember_workspace(workspace.clone());
        if let Err(error) = self.save_settings() {
            self.notice = Some(format!("could not save settings: {error}"));
        }
        self.net_tx.send(UiCmd::ListSessions { workspace }).ok();
    }

    /// Persists the default model/effort chosen in Settings, alongside
    /// whatever `desktop` state is already tracked.
    pub fn set_default_model(&mut self, model: &ModelRef, effort: Option<ReasoningEffort>) {
        self.models_settings.default = Some(nexa_harness::DefaultModel {
            provider: model.provider.clone(),
            model: model.id.clone(),
        });
        self.models_settings.default_reasoning_effort = effort;
        if let Err(error) = self.save_settings() {
            self.notice = Some(format!("could not save settings: {error}"));
        }
    }

    pub fn set_default_preset(&mut self, preset: Option<String>) {
        self.desktop_settings.default_preset = preset;
        if let Err(error) = self.save_settings() {
            self.notice = Some(format!("could not save settings: {error}"));
        }
    }

    pub fn persist_current_defaults(&mut self) {
        let Some(choice) = self.selected_choice().cloned() else {
            return;
        };
        let model = ModelRef {
            provider: choice.provider_id,
            id: choice.model,
        };
        self.set_default_model(&model, self.selected_effort());
    }

    pub fn set_providers(&mut self, providers: Vec<ProviderSummary>) {
        self.connected = true;
        self.choices = providers
            .iter()
            .flat_map(|provider| {
                provider.models.iter().map(move |model| ModelChoice {
                    provider_id: provider.id.clone(),
                    provider_name: provider.name.clone(),
                    model: model.id.clone(),
                    reasoning_efforts: model.reasoning_efforts.clone(),
                })
            })
            .collect();
        if let Some(default) = &self.models_settings.default
            && let Some(index) = self.choices.iter().position(|choice| {
                choice.provider_id == default.provider && choice.model == default.model
            })
        {
            self.selected_model = index;
        }
        self.clamp_effort();
    }

    pub fn select_model(&mut self, index: usize) {
        if index < self.choices.len() {
            self.selected_model = index;
            self.clamp_effort();
            self.persist_current_defaults();
        }
    }

    pub fn select_effort(&mut self, effort: Option<ReasoningEffort>) {
        if effort.is_none_or(|level| self.offered_efforts().contains(&level)) {
            self.effort = effort;
            self.persist_current_defaults();
        }
    }

    pub fn clamp_effort(&mut self) {
        if let Some(effort) = self.effort
            && !self.offered_efforts().contains(&effort)
        {
            self.effort = None;
        }
    }

    /// Levels the current model actually accepts.
    ///
    /// A declared list is used as-is. When the provider did not enumerate
    /// levels, the model id is matched against known families (GPT-5.6 Sol
    /// and Luna include `max`; unknown ids use [`ReasoningEffort::COMMON`]).
    /// An empty declared list hides the picker.
    #[must_use]
    pub fn offered_efforts(&self) -> &[ReasoningEffort] {
        let Some(choice) = self.selected_choice() else {
            return &[];
        };
        match choice.reasoning_efforts.as_deref() {
            Some(efforts) => efforts,
            None => nexa_harness::inferred_reasoning_efforts(&choice.model)
                .unwrap_or(&ReasoningEffort::COMMON),
        }
    }

    #[must_use]
    pub fn effort_picker_visible(&self) -> bool {
        self.selected_choice().is_some() && !self.offered_efforts().is_empty()
    }

    #[must_use]
    pub fn selected_effort(&self) -> Option<ReasoningEffort> {
        self.effort
    }

    fn save_settings(&self) -> Result<(), SettingsFileError> {
        SettingsFile {
            models: self.models_settings.clone(),
            desktop: self.desktop_settings.clone(),
        }
        .save(&self.settings_path)
    }

    pub fn log_activity(&mut self, kind: ActivityKind, text: impl Into<String>) {
        self.activity.push(ActivityLine {
            kind,
            text: text.into(),
        });
        if self.activity.len() > 500 {
            self.activity.remove(0);
        }
    }

    pub fn reset_thread(&mut self) {
        self.transcript.clear();
        self.activity.clear();
        self.run_state = RunState::Idle;
        self.focus_composer = true;
    }

    pub fn apply_event(&mut self, event: &Event) {
        match event {
            Event::Message { text, model, .. } => {
                self.transcript
                    .push(TranscriptEntry::User { text: text.clone() });
                if let Some(id) = self.active_session.clone() {
                    self.remember_title(&id, text);
                }
                self.log_activity(
                    ActivityKind::User,
                    format!("{} / {}: {text}", model.provider, model.id),
                );
            }
            Event::RunStarted {
                model,
                reasoning_effort,
                ..
            } => {
                self.run_state = RunState::Running;
                let effort = reasoning_effort.map(|e| e.as_str()).unwrap_or("default");
                self.log_activity(
                    ActivityKind::Run,
                    format!("{} / {} · {effort}", model.provider, model.id),
                );
            }
            Event::AssistantTextDelta { run_id, text, .. } => {
                append_assistant_delta(&mut self.transcript, run_id, text);
            }
            Event::AssistantMessage {
                run_id,
                text,
                tool_calls,
                ..
            } => {
                finalize_assistant(&mut self.transcript, run_id, text);
                for call in tool_calls {
                    self.log_activity(
                        ActivityKind::Tool,
                        format!("{} {}", call.name, call.arguments),
                    );
                }
            }
            Event::ToolCallStarted { call, .. } => {
                self.transcript.push(TranscriptEntry::Tool {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    result: None,
                    failed: false,
                });
                self.log_activity(ActivityKind::Tool, format!("{} started", call.name));
            }
            Event::ToolCallCompleted { result, .. } => {
                if let Some(TranscriptEntry::Tool {
                    result: slot,
                    failed,
                    ..
                }) = self.transcript.iter_mut().rev().find(|entry| {
                    matches!(
                        entry,
                        TranscriptEntry::Tool { call_id, .. } if *call_id == result.tool_call_id
                    )
                }) {
                    *slot = Some(result.content.clone());
                    *failed = result.is_error;
                }
                let status = if result.is_error { "failed" } else { "done" };
                self.log_activity(ActivityKind::Tool, format!("tool {status}"));
            }
            Event::RunCompleted { .. } => {
                self.run_state = RunState::Idle;
                if let Some(TranscriptEntry::Assistant { streaming, .. }) =
                    self.transcript.last_mut()
                {
                    *streaming = false;
                }
                self.log_activity(ActivityKind::Done, "run completed");
            }
            Event::RunFailed { error, .. } => {
                self.run_state = RunState::Failed;
                self.transcript.push(TranscriptEntry::Error {
                    text: error.clone(),
                });
                self.log_activity(ActivityKind::Error, error.clone());
            }
        }
    }

    fn remember_title(&mut self, session_id: &str, text: &str) {
        self.session_titles
            .entry(session_id.to_owned())
            .or_insert_with(|| widgets::title_from_message(text));
    }

    #[must_use]
    pub fn session_title(&self, session_id: &str) -> String {
        self.session_titles
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| "New chat".to_owned())
    }

    #[must_use]
    pub fn selected_choice(&self) -> Option<&ModelChoice> {
        self.choices.get(self.selected_model)
    }

    #[must_use]
    pub fn has_streaming_assistant(&self) -> bool {
        matches!(
            self.transcript.last(),
            Some(TranscriptEntry::Assistant {
                streaming: true,
                ..
            })
        )
    }

    /// Replaces (or inserts) one workspace's entry in the sidebar tree.
    pub fn set_workspace_sessions(&mut self, workspace: String, sessions: Vec<SessionSummary>) {
        if let Some(entry) = self
            .sessions_by_workspace
            .iter_mut()
            .find(|entry| entry.workspace == workspace)
        {
            entry.sessions = sessions;
        } else {
            self.sessions_by_workspace.push(WorkspaceSessions {
                workspace,
                sessions,
            });
        }
    }
}

fn append_assistant_delta(transcript: &mut Vec<TranscriptEntry>, run_id: &str, text: &str) {
    if let Some(TranscriptEntry::Assistant {
        run_id: item_run_id,
        text: item_text,
        streaming: true,
    }) = transcript.last_mut()
        && item_run_id == run_id
    {
        item_text.push_str(text);
        return;
    }
    transcript.push(TranscriptEntry::Assistant {
        run_id: run_id.to_owned(),
        text: text.to_owned(),
        streaming: true,
    });
}

fn finalize_assistant(transcript: &mut Vec<TranscriptEntry>, run_id: &str, text: &str) {
    if let Some(TranscriptEntry::Assistant {
        run_id: item_run_id,
        text: item_text,
        streaming,
    }) = transcript.last_mut()
        && item_run_id == run_id
    {
        if !text.is_empty() {
            *item_text = text.to_owned();
        }
        *streaming = false;
        return;
    }
    if !text.is_empty() {
        transcript.push(TranscriptEntry::Assistant {
            run_id: run_id.to_owned(),
            text: text.to_owned(),
            streaming: false,
        });
    }
}

#[must_use]
pub fn short_id(id: &str) -> String {
    let rest = id.strip_prefix("session-").unwrap_or(id);
    match rest.split_once('-') {
        Some((fingerprint, suffix)) => {
            format!("{}…{suffix}", &fingerprint[..4.min(fingerprint.len())])
        }
        None => id.to_owned(),
    }
}

/// The last path component of a workspace, for compact display (e.g. a
/// project pill showing `nexa` instead of `/home/user/code/nexa`).
#[must_use]
pub fn workspace_label(workspace: &str) -> &str {
    workspace
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|label| !label.is_empty())
        .unwrap_or(workspace)
}

#[must_use]
pub fn relative_time(past_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default();
    let seconds = now.saturating_sub(past_ms) / 1000;
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexa_protocol::{ToolCall, ToolResult};

    fn app() -> App {
        let (net_tx, _) = mpsc::channel();
        let (_, net_rx) = mpsc::channel();
        App::new(
            net_tx,
            net_rx,
            "/tmp".to_owned(),
            PathBuf::from("/tmp/settings.toml"),
            SettingsFile::default(),
        )
    }

    fn model() -> ModelRef {
        ModelRef {
            provider: "local".to_owned(),
            id: "test-model".to_owned(),
        }
    }

    #[test]
    fn streamed_then_final_assistant_message_is_not_duplicated() {
        let mut app = app();
        app.apply_event(&Event::AssistantTextDelta {
            session_id: "s".into(),
            sequence: 1,
            run_id: "r".into(),
            text: "Hel".into(),
            created_at_ms: 0,
        });
        app.apply_event(&Event::AssistantTextDelta {
            session_id: "s".into(),
            sequence: 2,
            run_id: "r".into(),
            text: "lo".into(),
            created_at_ms: 0,
        });
        app.apply_event(&Event::AssistantMessage {
            session_id: "s".into(),
            sequence: 3,
            run_id: "r".into(),
            text: "Hello".into(),
            tool_calls: Vec::new(),
            created_at_ms: 0,
        });
        assert_eq!(app.transcript.len(), 1);
        match &app.transcript[0] {
            TranscriptEntry::Assistant {
                text, streaming, ..
            } => {
                assert_eq!(text, "Hello");
                assert!(!*streaming);
            }
            other => panic!("expected assistant, got role change: {other:?}"),
        }
    }

    #[test]
    fn tool_completion_updates_the_existing_card() {
        let mut app = app();
        app.apply_event(&Event::ToolCallStarted {
            session_id: "s".into(),
            sequence: 1,
            run_id: "r".into(),
            call: ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: "{\"path\":\"a.rs\"}".into(),
            },
            created_at_ms: 0,
        });
        app.apply_event(&Event::ToolCallCompleted {
            session_id: "s".into(),
            sequence: 2,
            run_id: "r".into(),
            result: ToolResult {
                tool_call_id: "c1".into(),
                content: "ok".into(),
                is_error: false,
            },
            created_at_ms: 0,
        });
        assert_eq!(app.transcript.len(), 1);
        match &app.transcript[0] {
            TranscriptEntry::Tool {
                name,
                result,
                failed,
                ..
            } => {
                assert_eq!(name, "read_file");
                assert_eq!(result.as_deref(), Some("ok"));
                assert!(!*failed);
            }
            _ => panic!("expected tool card"),
        }
    }

    #[test]
    fn first_user_message_becomes_the_session_title() {
        let mut app = app();
        app.active_session = Some("session-abcd-xyz".into());
        app.apply_event(&Event::Message {
            session_id: "session-abcd-xyz".into(),
            sequence: 1,
            client_id: "desktop".into(),
            model: model(),
            text: "Summarize this repository\n\nBe brief.".into(),
            created_at_ms: 0,
        });
        assert_eq!(
            app.session_title("session-abcd-xyz"),
            "Summarize this repository"
        );
    }

    #[test]
    fn unsupported_effort_falls_back_to_omit() {
        let mut app = app();
        app.choices.push(ModelChoice {
            provider_id: "local".into(),
            provider_name: "Local".into(),
            model: "limited".into(),
            reasoning_efforts: Some(vec![ReasoningEffort::Low]),
        });
        app.effort = Some(ReasoningEffort::Max);
        app.clamp_effort();
        assert_eq!(app.effort, None);
        assert_eq!(app.offered_efforts(), &[ReasoningEffort::Low]);
        assert!(app.effort_picker_visible());
    }

    #[test]
    fn undeclared_models_offer_the_common_effort_set() {
        let mut app = app();
        app.choices.push(ModelChoice {
            provider_id: "local".into(),
            provider_name: "Local".into(),
            model: "plain".into(),
            reasoning_efforts: None,
        });
        assert_eq!(app.offered_efforts(), &ReasoningEffort::COMMON);
        assert!(!app.offered_efforts().contains(&ReasoningEffort::Minimal));
        assert!(!app.offered_efforts().contains(&ReasoningEffort::Max));
    }

    #[test]
    fn gpt56_sol_and_luna_offer_max_when_the_provider_did_not_enumerate() {
        for id in ["gpt-5.6-sol", "gpt-5.6-luna"] {
            let mut app = app();
            app.choices.push(ModelChoice {
                provider_id: "nexus".into(),
                provider_name: "Nexus".into(),
                model: id.to_owned(),
                reasoning_efforts: None,
            });
            assert!(
                app.offered_efforts().contains(&ReasoningEffort::Max),
                "{id} should offer max"
            );
            assert!(app.offered_efforts().contains(&ReasoningEffort::XHigh));
        }
    }

    #[test]
    fn empty_declared_list_hides_the_effort_picker() {
        let mut app = app();
        app.choices.push(ModelChoice {
            provider_id: "local".into(),
            provider_name: "Local".into(),
            model: "direct".into(),
            reasoning_efforts: Some(Vec::new()),
        });
        assert!(app.offered_efforts().is_empty());
        assert!(!app.effort_picker_visible());
    }
}
