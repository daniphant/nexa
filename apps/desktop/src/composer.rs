//! The message composer: workspace, preset, model, and reasoning pickers
//! live in the input card itself rather than a header.

use egui::{Key, KeyboardShortcut, Modifiers, RichText};

use nexa_protocol::ModelRef;

use crate::{
    state::{App, UiCmd, effort_hint, effort_label, workspace_label},
    theme,
    widgets::{self, PICKER_WIDTH},
};

/// Renders the composer card. `docked` is true when the card is pinned to
/// the bottom of a thread (pickers open upward); false for the empty-state
/// centered card (pickers open downward).
pub fn show(ui: &mut egui::Ui, app: &mut App, docked: bool) {
    egui::Frame::new()
        .fill(theme::BG_PANEL)
        .stroke(egui::Stroke::new(1.0, theme::STROKE))
        .corner_radius(egui::CornerRadius::same(16))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                workspace_pill(ui, app, docked);
                preset_pill(ui, app, docked);
            });
            ui.add_space(8.0);

            let draft_id = ui.make_persistent_id("composer-draft");
            let rows = app
                .draft
                .split('\n')
                .count()
                .clamp(if docked { 2 } else { 3 }, 10);
            let output = egui::TextEdit::multiline(&mut app.draft)
                .id(draft_id)
                .frame(egui::Frame::NONE)
                .hint_text(
                    RichText::new("Describe a task, or ask about this project…")
                        .color(theme::TEXT_DIM),
                )
                .font(egui::FontId::proportional(15.0))
                .desired_width(f32::INFINITY)
                .desired_rows(rows)
                .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
                .show(ui);

            if app.focus_composer {
                output.response.request_focus();
                app.focus_composer = false;
            }

            let focused = output.response.has_focus();
            let enter = focused
                && ui.input(|input| {
                    input.key_pressed(Key::Enter)
                        && !input.modifiers.shift
                        && !input.modifiers.command
                });

            ui.add_space(8.0);
            let can_send = !app.draft.trim().is_empty() && app.selected_choice().is_some();
            let mut send_clicked = false;
            egui::containers::Sides::new().height(34.0).show(
                ui,
                |ui| {
                    model_pill(ui, app, docked);
                    if app.effort_picker_visible() {
                        effort_pill(ui, app, docked);
                    }
                },
                |ui| {
                    send_clicked = widgets::send_button(ui, can_send).clicked();
                },
            );
            if (enter || send_clicked) && can_send {
                send_draft(app);
                app.focus_composer = true;
            }
        });

    ui.add_space(6.0);
    ui.label(
        RichText::new("Enter to send  ·  Shift+Enter for a new line")
            .size(11.5)
            .color(theme::TEXT_DIM),
    );
}

fn send_draft(app: &mut App) {
    let choice = app.selected_choice().expect("checked by caller").clone();
    let model = ModelRef {
        provider: choice.provider_id,
        id: choice.model,
    };
    app.notice = None;
    app.net_tx
        .send(UiCmd::Send {
            workspace: app.workspace.clone(),
            model,
            preset: app.selected_preset.clone(),
            effort: app.selected_effort(),
            text: std::mem::take(&mut app.draft),
        })
        .ok();
}

fn workspace_pill(ui: &mut egui::Ui, app: &mut App, docked: bool) {
    let (popup_id, open) = widgets::picker_id(ui.ctx(), "picker-workspace");
    let label = workspace_label(&app.workspace).to_owned();
    let button = widgets::pill(ui, &label, open);
    widgets::picker(&button, popup_id, 320.0, docked, |ui| {
        widgets::picker_title(ui, "Workspace");
        ui.label(
            RichText::new("New chats start here. Switching folders does not move an open thread.")
                .size(11.5)
                .color(theme::TEXT_DIM),
        );
        ui.add_space(6.0);
        widgets::picker_list(ui, |ui| {
            for workspace in app.known_workspaces() {
                let active = workspace == app.workspace;
                let title = workspace_label(&workspace).to_owned();
                if widgets::picker_row(ui, &workspace, active, &title, Some(workspace.as_str()))
                    .clicked()
                {
                    app.select_workspace(workspace);
                    ui.close();
                }
            }
        });
        ui.add_space(6.0);
        if ui
            .add(
                egui::Button::new(
                    RichText::new("Choose folder…")
                        .font(theme::medium(13.0))
                        .color(theme::TEXT_PRIMARY),
                )
                .fill(theme::BG_FIELD)
                .min_size(egui::vec2(ui.available_width(), 30.0))
                .corner_radius(8),
            )
            .clicked()
        {
            if let Some(path) = crate::projects::pick_folder() {
                app.select_workspace(path);
            }
            ui.close();
        }
    });
}

fn preset_pill(ui: &mut egui::Ui, app: &mut App, docked: bool) {
    let (popup_id, open) = widgets::picker_id(ui.ctx(), "picker-preset");
    let label = app
        .selected_preset
        .clone()
        .unwrap_or_else(|| "Standard".to_owned());
    let button = widgets::pill(ui, &label, open);
    widgets::picker(&button, popup_id, PICKER_WIDTH, docked, |ui| {
        widgets::picker_title(ui, "Agent preset");
        ui.label(
            RichText::new("Scopes which tools this chat may use.")
                .size(11.5)
                .color(theme::TEXT_DIM),
        );
        ui.add_space(6.0);
        widgets::picker_list(ui, |ui| {
            if widgets::picker_row(
                ui,
                "standard",
                app.selected_preset.is_none(),
                "Standard",
                Some("Every tool"),
            )
            .clicked()
            {
                app.selected_preset = None;
                ui.close();
            }
            for preset in app.presets.clone() {
                let active = app.selected_preset.as_deref() == Some(preset.name.as_str());
                let tools = if preset.tools.is_empty() {
                    "Every tool".to_owned()
                } else {
                    preset.tools.join(", ")
                };
                let subtitle = if preset.description.is_empty() {
                    tools
                } else {
                    format!("{} · {tools}", preset.description)
                };
                if widgets::picker_row(ui, &preset.name, active, &preset.name, Some(&subtitle))
                    .clicked()
                {
                    app.selected_preset = Some(preset.name);
                    ui.close();
                }
            }
        });
    });
}

fn model_pill(ui: &mut egui::Ui, app: &mut App, docked: bool) {
    let (popup_id, open) = widgets::picker_id(ui.ctx(), "picker-model");
    if button_just_opened(ui, &popup_id, open) {
        app.model_filter.clear();
        app.focus_model_search = true;
    }
    let label = app
        .selected_choice()
        .map(|choice| choice.model.clone())
        .unwrap_or_else(|| "No models".to_owned());
    let button = widgets::pill(ui, &label, open);
    widgets::picker(&button, popup_id, 320.0, docked, |ui| {
        widgets::picker_title(ui, "Model");
        if app.choices.is_empty() {
            ui.label(
                RichText::new("No models configured. Run `nexa provider add`.")
                    .size(12.0)
                    .color(theme::TEXT_DIM),
            );
            return;
        }

        let search = widgets::picker_search(ui, &mut app.model_filter, "Search models…");
        if app.focus_model_search {
            search.request_focus();
            app.focus_model_search = false;
        }
        ui.add_space(4.0);

        let filter = app.model_filter.to_lowercase();
        let visible: Vec<usize> = app
            .choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| {
                filter.is_empty()
                    || choice.model.to_lowercase().contains(&filter)
                    || choice.provider_name.to_lowercase().contains(&filter)
                    || choice.provider_id.to_lowercase().contains(&filter)
            })
            .map(|(index, _)| index)
            .collect();

        if search.has_focus()
            && ui.input(|input| input.key_pressed(Key::Enter))
            && visible.len() == 1
        {
            app.select_model(visible[0]);
            ui.close();
            return;
        }

        if visible.is_empty() {
            ui.label(
                RichText::new("No models match that search.")
                    .size(12.0)
                    .color(theme::TEXT_DIM),
            );
            return;
        }

        let mut last_provider = String::new();
        widgets::picker_list(ui, |ui| {
            for index in visible {
                let choice = app.choices[index].clone();
                if choice.provider_id != last_provider {
                    widgets::picker_section(ui, &choice.provider_name);
                    last_provider.clone_from(&choice.provider_id);
                }
                if widgets::picker_row(
                    ui,
                    (&choice.provider_id, &choice.model),
                    index == app.selected_model,
                    &choice.model,
                    Some(&choice.provider_name),
                )
                .clicked()
                {
                    app.select_model(index);
                    ui.close();
                }
            }
        });
    });
}

fn effort_pill(ui: &mut egui::Ui, app: &mut App, docked: bool) {
    let (popup_id, open) = widgets::picker_id(ui.ctx(), "picker-effort");
    let label = effort_label(app.effort);
    let button = widgets::pill(ui, label, open);
    widgets::picker(&button, popup_id, PICKER_WIDTH, docked, |ui| {
        widgets::picker_title(ui, "Reasoning");
        ui.label(
            RichText::new("Only the levels this model accepts.")
                .size(11.5)
                .color(theme::TEXT_DIM),
        );
        ui.add_space(6.0);
        widgets::picker_list(ui, |ui| {
            if widgets::picker_row(
                ui,
                "default",
                app.effort.is_none(),
                effort_label(None),
                Some("Omit the parameter; the provider chooses"),
            )
            .clicked()
            {
                app.select_effort(None);
                ui.close();
            }
            let offered: Vec<_> = app.offered_efforts().to_vec();
            for effort in offered {
                if widgets::picker_row(
                    ui,
                    effort.as_str(),
                    app.effort == Some(effort),
                    effort_label(Some(effort)),
                    Some(effort_hint(effort)),
                )
                .clicked()
                {
                    app.select_effort(Some(effort));
                    ui.close();
                }
            }
        });
    });
}

fn button_just_opened(ui: &egui::Ui, popup_id: &egui::Id, open: bool) -> bool {
    let key = popup_id.with("was-open");
    let was_open = ui
        .ctx()
        .data(|data| data.get_temp::<bool>(key).unwrap_or(false));
    ui.ctx().data_mut(|data| data.insert_temp(key, open));
    open && !was_open
}
