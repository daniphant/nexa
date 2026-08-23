//! Native GPU-rendered Nexa desktop client (egui + wgpu).
//!
//! The desktop app is a thin, GPU-drawn front end over the same headless
//! server the CLI uses: sessions, commands, and events all flow through
//! `nexa-client`. Conversations are lazy — a session materializes on the
//! server when the first message of a chat is sent, never at launch.

mod activity;
mod composer;
mod projects;
mod settings;
mod sidebar;
mod state;
mod theme;
mod transcript;
mod widgets;

use std::{path::Path, path::PathBuf, sync::mpsc, thread, time::Duration};

use nexa_client::{ClientError, EventStream as SseStream, NexaClient};
use nexa_harness::{LocalServerError, ServerProbe, SettingsFile, ensure_local_server};

use state::{App, NetEvent, RunState, UiCmd, short_id, workspace_label};
use widgets::CONTENT_WIDTH;

fn main() -> eframe::Result {
    let server_url = std::env::var("NEXA_SERVER_URL").ok().unwrap_or_else(|| {
        let port = std::env::var("NEXA_PORT").unwrap_or_else(|_| "4123".to_owned());
        format!("http://127.0.0.1:{port}")
    });
    let workspace_from_env = std::env::var_os("NEXA_WORKSPACE")
        .map(PathBuf::from)
        .and_then(|path| path.to_str().map(str::to_owned))
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|path| path.to_str().map(str::to_owned))
        })
        .expect("a usable workspace");

    let settings_path = default_settings_path();
    let settings = load_settings(&settings_path);
    let workspace = settings
        .desktop
        .active_workspace
        .clone()
        .unwrap_or(workspace_from_env);

    let mut client = NexaClient::new(server_url.clone(), "desktop".to_owned());
    if std::env::var_os("NEXA_SERVER_URL").is_none() {
        let token =
            nexa_harness::load_or_create_server_token(default_token_path()).unwrap_or_default();
        client = client.with_token(token);
    } else if let Ok(token) = std::env::var("NEXA_SERVER_TOKEN") {
        client = client.with_token(token);
    }

    let autostart_local = std::env::var_os("NEXA_SERVER_URL").is_none();
    let server_log_path = default_home().join("logs/server.log");

    // The network lives on its own tokio runtime thread; the UI thread only
    // exchanges messages with it and repaints when results arrive.
    let (ui_tx, ui_rx) = mpsc::channel::<NetEvent>();
    let (net_tx, net_rx) = mpsc::channel::<UiCmd>();
    let ctx = egui::Context::default();
    {
        let ctx = ctx.clone();
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            runtime.block_on(net_actor(
                client,
                net_rx,
                ui_tx,
                ctx,
                autostart_local,
                server_log_path,
            ));
        });
    }
    net_tx.send(UiCmd::Bootstrap).ok();
    net_tx.send(UiCmd::ListPresets).ok();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([860.0, 560.0])
            .with_title("Nexa"),
        ..Default::default()
    };
    eframe::run_native(
        "nexa-desktop",
        options,
        Box::new(move |cc| {
            theme::configure(&cc.egui_ctx);
            let app = App::new(net_tx, ui_rx, workspace, settings_path, settings);
            for workspace in app.known_workspaces() {
                app.net_tx.send(UiCmd::ListSessions { workspace }).ok();
            }
            Ok(Box::new(app))
        }),
    )
}

fn default_token_path() -> PathBuf {
    match std::env::var_os("NEXA_SERVER_TOKEN_FILE") {
        Some(path) => PathBuf::from(path),
        None => default_home().join("server.token"),
    }
}

fn default_settings_path() -> PathBuf {
    default_home().join("settings.toml")
}

fn default_home() -> PathBuf {
    match std::env::var_os("NEXA_HOME") {
        Some(home) => PathBuf::from(home),
        None => dirs::home_dir()
            .map(|home| home.join(".nexa"))
            .expect("could not resolve the Nexa home"),
    }
}

/// Settings are convenience state; a broken file never blocks the app.
async fn ensure_desktop_server(
    client: &NexaClient,
    ui_tx: &mpsc::Sender<NetEvent>,
    ctx: &egui::Context,
    log_path: &Path,
) -> Result<(), LocalServerError> {
    let client = client.clone();
    ensure_local_server(
        log_path,
        move || {
            let client = client.clone();
            async move {
                match client.providers().await {
                    Ok(_) => Ok(ServerProbe::Ready),
                    Err(error) if error.is_connect() => Ok(ServerProbe::Connecting),
                    Err(error) => Err(LocalServerError::Failed(error.to_string())),
                }
            }
        },
        || {
            push(
                ui_tx,
                ctx,
                NetEvent::Notice("Starting nexa-server…".to_owned()),
            );
        },
    )
    .await
}

fn load_settings(path: &Path) -> SettingsFile {
    match SettingsFile::load_or_default(path) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!(
                "Ignoring unreadable settings file {}: {error}",
                path.display()
            );
            SettingsFile::default()
        }
    }
}

async fn net_actor(
    mut client: NexaClient,
    net_rx: mpsc::Receiver<UiCmd>,
    ui_tx: mpsc::Sender<NetEvent>,
    ctx: egui::Context,
    autostart_local: bool,
    server_log_path: PathBuf,
) {
    while let Ok(command) = net_rx.recv() {
        match command {
            UiCmd::Bootstrap => {
                if autostart_local
                    && let Err(error) =
                        ensure_desktop_server(&client, &ui_tx, &ctx, &server_log_path).await
                {
                    push(&ui_tx, &ctx, NetEvent::Notice(error.to_string()));
                    continue;
                }
                match client.providers().await {
                    Ok(providers) => push(&ui_tx, &ctx, NetEvent::Providers(providers)),
                    Err(error) => push(&ui_tx, &ctx, NetEvent::Notice(error.to_string())),
                }
            }
            UiCmd::ListPresets => match client.presets().await {
                Ok(presets) => push(&ui_tx, &ctx, NetEvent::Presets(presets)),
                Err(error) => push(&ui_tx, &ctx, NetEvent::Notice(error.to_string())),
            },
            UiCmd::ListSessions { workspace } => match client.list_sessions(&workspace).await {
                Ok(sessions) => push(
                    &ui_tx,
                    &ctx,
                    NetEvent::Sessions {
                        workspace,
                        sessions,
                    },
                ),
                Err(error) => push(&ui_tx, &ctx, NetEvent::Notice(error.to_string())),
            },
            UiCmd::NewChat => {
                client.clear_session();
                push(&ui_tx, &ctx, NetEvent::Detached);
            }
            UiCmd::SwitchTo { session_id } => {
                client.select_session(&session_id);
                // Tell the UI to reset before replayed events start landing.
                push(&ui_tx, &ctx, NetEvent::Switched(session_id));
                if let Err(error) = subscribe_and_forward(&client, &ui_tx, &ctx).await {
                    push(&ui_tx, &ctx, NetEvent::Notice(error.to_string()));
                }
            }
            UiCmd::Send {
                workspace,
                model,
                preset,
                effort,
                text,
            } => {
                // Lazy sessions: the first message materializes one.
                if !client.has_session() {
                    match client.create_session(&workspace).await {
                        Ok(info) => {
                            client.select_session(&info.id);
                            push(&ui_tx, &ctx, NetEvent::SessionReady(info.id.clone()));
                            if let Err(error) = subscribe_and_forward(&client, &ui_tx, &ctx).await {
                                push(&ui_tx, &ctx, NetEvent::Notice(error.to_string()));
                                continue;
                            }
                            if let Ok(sessions) = client.list_sessions(&workspace).await {
                                push(
                                    &ui_tx,
                                    &ctx,
                                    NetEvent::Sessions {
                                        workspace: workspace.clone(),
                                        sessions,
                                    },
                                );
                            }
                        }
                        Err(error) => {
                            push(&ui_tx, &ctx, NetEvent::Notice(error.to_string()));
                            continue;
                        }
                    }
                }
                if let Err(error) = client.send_message(model, preset, effort, text).await {
                    push(&ui_tx, &ctx, NetEvent::Notice(error.to_string()));
                }
            }
        }
    }
}

/// Subscribes to `session_id`'s stream and forwards every replayed or live
/// event to the UI, tagging them with `switch_id` for state resets.
async fn subscribe_and_forward(
    client: &NexaClient,
    ui_tx: &mpsc::Sender<NetEvent>,
    ctx: &egui::Context,
) -> Result<(), ClientError> {
    let stream: SseStream = client.subscribe().await?;
    let ui_tx = ui_tx.clone();
    let ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let mut stream = stream;
        while let Ok(event) = stream.next().await {
            ui_tx.send(NetEvent::Event(event)).ok();
            ctx.request_repaint();
        }
    });
    drop(task);
    Ok(())
}

fn push(ui_tx: &mpsc::Sender<NetEvent>, ctx: &egui::Context, event: NetEvent) {
    ui_tx.send(event).ok();
    ctx.request_repaint();
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        drain_events(self);
        handle_shortcuts(ui.ctx(), self);

        settings::show(ui.ctx(), self);

        let sidebar_frame = egui::Frame::new()
            .fill(theme::BG_PANEL)
            .inner_margin(egui::Margin::same(12));
        if self.sidebar_open {
            egui::Panel::left("chats")
                .resizable(true)
                .default_size(272.0)
                .min_size(216.0)
                .max_size(420.0)
                .frame(sidebar_frame)
                .show(ui, |ui| sidebar::show(ui, self));
        } else {
            egui::Panel::left("chats-rail")
                .exact_size(56.0)
                .resizable(false)
                .frame(sidebar_frame)
                .show(ui, |ui| sidebar::rail(ui, self));
        }

        if self.activity_open {
            egui::Panel::right("activity")
                .resizable(true)
                .default_size(300.0)
                .min_size(220.0)
                .max_size(480.0)
                .frame(
                    egui::Frame::new()
                        .fill(theme::BG_PANEL)
                        .inner_margin(egui::Margin::same(12)),
                )
                .show(ui, |ui| activity::show(ui, self));
        }

        let has_thread = !self.transcript.is_empty();
        if has_thread {
            egui::Panel::bottom("composer")
                .resizable(false)
                .show_separator_line(false)
                .frame(
                    egui::Frame::new()
                        .fill(theme::BG_MAIN)
                        .inner_margin(egui::Margin {
                            left: 24,
                            right: 24,
                            top: 4,
                            bottom: 16,
                        }),
                )
                .show(ui, |ui| {
                    widgets::centered_column(ui, CONTENT_WIDTH, |ui| {
                        composer::show(ui, self, true);
                    });
                });
            egui::Panel::top("chat-header")
                .exact_size(52.0)
                .show_separator_line(false)
                .frame(
                    egui::Frame::new()
                        .fill(theme::BG_MAIN)
                        .inner_margin(egui::Margin::symmetric(20, 10)),
                )
                .show(ui, |ui| chat_header(ui, self));
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_MAIN)
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ui, |ui| {
                if let Some(notice) = self.notice.clone() {
                    notice_banner(ui, self, &notice);
                }
                if has_thread {
                    egui::ScrollArea::vertical()
                        .id_salt("transcript")
                        .stick_to_bottom(true)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add_space(4.0);
                            widgets::centered_column(ui, CONTENT_WIDTH, |ui| {
                                for entry in &self.transcript {
                                    transcript::render_entry(ui, entry);
                                }
                                if self.run_state == RunState::Running
                                    && !self.has_streaming_assistant()
                                {
                                    thinking_row(ui);
                                }
                            });
                            ui.add_space(12.0);
                        });
                } else {
                    empty_state(ui, self);
                }
            });

        let ctx = ui.ctx().clone();
        if self.run_state == RunState::Running {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(Duration::from_secs(15));
        }
    }
}

fn chat_header(ui: &mut egui::Ui, app: &mut App) {
    let title = app
        .active_session
        .as_deref()
        .map(|id| app.session_title(id))
        .unwrap_or_else(|| "New chat".to_owned());
    let workspace = workspace_label(&app.workspace).to_owned();
    let run_state = app.run_state;
    let activity_open = app.activity_open;
    let sidebar_closed = !app.sidebar_open;
    let mut show_sidebar = false;
    let mut toggle_activity = false;
    egui::containers::Sides::new().height(32.0).show(
        ui,
        |ui| {
            if sidebar_closed && widgets::icon_button(ui, "☰", "Show sidebar").clicked() {
                show_sidebar = true;
            }
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(title)
                        .font(theme::semibold(15.0))
                        .color(theme::TEXT_PRIMARY),
                );
                ui.label(
                    egui::RichText::new(workspace)
                        .size(11.5)
                        .color(theme::TEXT_DIM),
                );
            });
        },
        |ui| {
            match run_state {
                RunState::Running => {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new("Running")
                            .size(12.0)
                            .color(theme::WARNING),
                    );
                }
                RunState::Failed => {
                    ui.label(
                        egui::RichText::new("Failed")
                            .size(12.0)
                            .color(theme::DANGER),
                    );
                }
                RunState::Idle => {}
            }
            if widgets::ghost_button(ui, "Activity", activity_open).clicked() {
                toggle_activity = true;
            }
        },
    );
    if show_sidebar {
        app.sidebar_open = true;
    }
    if toggle_activity {
        app.activity_open = !app.activity_open;
    }
}

fn thinking_row(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(
            egui::RichText::new("Working…")
                .italics()
                .color(theme::TEXT_DIM)
                .size(14.0),
        );
    });
    ui.add_space(8.0);
}

fn notice_banner(ui: &mut egui::Ui, app: &mut App, notice: &str) {
    egui::Frame::new()
        .fill(theme::DANGER_BG)
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            egui::containers::Sides::new().show(
                ui,
                |ui| {
                    ui.label(egui::RichText::new(notice).color(theme::DANGER).size(13.0));
                },
                |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Dismiss")
                                    .size(12.0)
                                    .color(theme::TEXT_SOFT),
                            )
                            .frame_when_inactive(false),
                        )
                        .clicked()
                    {
                        app.notice = None;
                    }
                },
            );
        });
    ui.add_space(8.0);
}

fn empty_state(ui: &mut egui::Ui, app: &mut App) {
    let available = ui.available_size();
    ui.vertical_centered(|ui| {
        ui.add_space((available.y * 0.18).clamp(24.0, 140.0));
        ui.set_max_width(640.0);
        ui.label(
            egui::RichText::new("What should we work on?")
                .font(theme::semibold(26.0))
                .color(theme::TEXT_PRIMARY),
        );
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(workspace_label(&app.workspace))
                .font(theme::medium(14.0))
                .color(theme::TEXT_SOFT),
        );
        ui.label(
            egui::RichText::new(&app.workspace)
                .size(12.0)
                .color(theme::TEXT_DIM),
        );
        if !app.connected {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Connecting to nexa-server…")
                    .size(12.5)
                    .color(theme::WARNING),
            );
        } else if app.choices.is_empty() {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("No models configured. Run `nexa provider add`.")
                    .size(12.5)
                    .color(theme::WARNING),
            );
        }
        ui.add_space(22.0);
        composer::show(ui, app, false);
    });
}

fn handle_shortcuts(ctx: &egui::Context, app: &mut App) {
    let command = egui::Modifiers::COMMAND;
    ctx.input_mut(|input| {
        if input.consume_shortcut(&egui::KeyboardShortcut::new(command, egui::Key::N)) {
            app.net_tx.send(UiCmd::NewChat).ok();
            app.focus_composer = true;
        }
        if input.consume_shortcut(&egui::KeyboardShortcut::new(command, egui::Key::Comma)) {
            app.settings_open = !app.settings_open;
        }
        if input.consume_shortcut(&egui::KeyboardShortcut::new(
            egui::Modifiers {
                shift: true,
                ..command
            },
            egui::Key::A,
        )) {
            app.activity_open = !app.activity_open;
        }
        if input.consume_shortcut(&egui::KeyboardShortcut::new(command, egui::Key::L)) {
            app.focus_composer = true;
        }
        if input.key_pressed(egui::Key::Escape) && app.notice.is_some() && !app.settings_open {
            app.notice = None;
        }
    });
}

fn drain_events(app: &mut App) {
    while let Ok(event) = app.net_rx.try_recv() {
        match event {
            NetEvent::Providers(providers) => {
                app.set_providers(providers);
                if app
                    .notice
                    .as_deref()
                    .is_some_and(|notice| notice.starts_with("Starting nexa-server"))
                {
                    app.notice = None;
                }
            }
            NetEvent::Presets(presets) => app.presets = presets,
            NetEvent::Sessions {
                workspace,
                sessions,
            } => app.set_workspace_sessions(workspace, sessions),
            NetEvent::SessionReady(id) => {
                app.active_session = Some(id.clone());
                app.log_activity(
                    state::ActivityKind::Run,
                    format!("chat started as {}", short_id(&id)),
                );
            }
            NetEvent::Switched(id) => {
                app.active_session = Some(id.clone());
                app.reset_thread();
                app.log_activity(
                    state::ActivityKind::Run,
                    format!("reopened {}", short_id(&id)),
                );
            }
            NetEvent::Event(event) => {
                if app.active_session.as_deref() == Some(event.session_id()) {
                    app.apply_event(&event);
                }
            }
            NetEvent::Detached => {
                app.active_session = None;
                app.reset_thread();
            }
            NetEvent::Notice(notice) => app.notice = Some(notice),
        }
    }
}
