//! Left rail: new-chat, session search, and the workspace/session tree.

use egui::{Align2, Sense, vec2};

use crate::{
    state::{App, UiCmd, relative_time, workspace_label},
    theme, widgets,
};

pub fn show(ui: &mut egui::Ui, app: &mut App) {
    egui::containers::Sides::new().height(28.0).show(
        ui,
        |ui| {
            ui.label(
                egui::RichText::new("Nexa")
                    .font(theme::semibold(16.0))
                    .color(theme::TEXT_PRIMARY),
            );
        },
        |ui| {
            widgets::status_dot(
                ui,
                if app.connected {
                    theme::SUCCESS
                } else {
                    theme::WARNING
                },
            );
            if widgets::icon_button(ui, "‹", "Hide sidebar").clicked() {
                app.sidebar_open = false;
            }
        },
    );
    ui.add_space(10.0);
    if ui
        .add_sized(
            [ui.available_width(), 34.0],
            egui::Button::new(
                egui::RichText::new("+  New chat")
                    .font(theme::medium(14.0))
                    .color(theme::TEXT_PRIMARY),
            )
            .fill(theme::BG_FIELD)
            .stroke(egui::Stroke::NONE)
            .corner_radius(8),
        )
        .clicked()
    {
        app.net_tx.send(UiCmd::NewChat).ok();
        app.focus_composer = true;
    }
    ui.add_space(8.0);
    widgets::search_field(ui, &mut app.sidebar_filter, "Search chats…");
    ui.add_space(12.0);
    ui.label(
        egui::RichText::new("Workspaces")
            .font(theme::medium(11.0))
            .color(theme::TEXT_DIM),
    );
    ui.add_space(4.0);

    let footer_height = 44.0;
    let tree_height = (ui.available_height() - footer_height).max(0.0);
    egui::ScrollArea::vertical()
        .max_height(tree_height)
        .auto_shrink([false, true])
        .show(ui, |ui| session_tree(ui, app));

    ui.add_space(6.0);
    ui.separator();
    ui.add_space(4.0);
    if ui
        .add_sized(
            [ui.available_width(), 30.0],
            egui::Button::new(
                egui::RichText::new("Settings")
                    .font(theme::medium(13.5))
                    .color(theme::TEXT_SOFT),
            )
            .fill(egui::Color32::TRANSPARENT)
            .frame_when_inactive(false),
        )
        .clicked()
    {
        app.settings_open = true;
    }
}

pub fn rail(ui: &mut egui::Ui, app: &mut App) {
    ui.vertical_centered(|ui| {
        ui.add_space(6.0);
        if widgets::icon_button(ui, "›", "Show sidebar").clicked() {
            app.sidebar_open = true;
        }
        ui.add_space(8.0);
        if widgets::icon_button(ui, "+", "New chat").clicked() {
            app.net_tx.send(UiCmd::NewChat).ok();
            app.focus_composer = true;
        }
        ui.add_space(12.0);
        if widgets::icon_button(ui, "⚙", "Settings").clicked() {
            app.settings_open = true;
        }
    });
}

fn session_tree(ui: &mut egui::Ui, app: &mut App) {
    let filter = app.sidebar_filter.to_lowercase();
    let workspaces = app.known_workspaces();
    let mut any_shown = false;

    for workspace in &workspaces {
        let Some(sessions) = app
            .sessions_by_workspace
            .iter()
            .find(|entry| &entry.workspace == workspace)
            .map(|entry| &entry.sessions)
        else {
            continue;
        };
        let matches = |id: &str, title: &str| {
            filter.is_empty()
                || title.to_lowercase().contains(&filter)
                || id.to_lowercase().contains(&filter)
        };
        let titles: Vec<(String, String, u64)> = sessions
            .iter()
            .map(|summary| {
                (
                    summary.id.clone(),
                    app.session_title(&summary.id),
                    summary.last_opened_ms,
                )
            })
            .filter(|(id, title, _)| matches(id, title))
            .collect();
        if !filter.is_empty() && titles.is_empty() {
            continue;
        }
        any_shown = true;

        let header_id = ui.id().with(("workspace", workspace));
        let is_active_project = workspace == &app.workspace;
        let mut open = ui.ctx().data_mut(|data| {
            *data.get_temp_mut_or_insert_with(header_id, || is_active_project || !titles.is_empty())
        });
        let count = titles.len();
        if workspace_header(ui, workspace_label(workspace), open, count).clicked() {
            open = !open;
            ui.ctx().data_mut(|data| data.insert_temp(header_id, open));
        }
        if !open {
            continue;
        }
        if titles.is_empty() {
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new("No chats yet")
                    .color(theme::TEXT_DIM)
                    .italics()
                    .size(12.0),
            );
            ui.add_space(6.0);
            continue;
        }
        ui.add_space(2.0);
        for (id, title, last_opened_ms) in titles {
            let selected = app.active_session.as_deref() == Some(id.as_str());
            if session_row(ui, selected, &title, &relative_time(last_opened_ms)).clicked() {
                app.net_tx.send(UiCmd::SwitchTo { session_id: id }).ok();
            }
        }
        ui.add_space(6.0);
    }

    if !any_shown {
        let message = if filter.is_empty() {
            "No chats yet"
        } else {
            "No chats match that search"
        };
        ui.label(
            egui::RichText::new(message)
                .color(theme::TEXT_DIM)
                .size(12.5),
        );
    }
}

fn workspace_header(ui: &mut egui::Ui, label: &str, open: bool, count: usize) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
    widgets::paint_disclosure(
        ui,
        rect.left_center() + vec2(7.0, 0.0),
        open,
        theme::TEXT_SOFT,
    );
    ui.painter().text(
        rect.left_center() + vec2(16.0, 0.0),
        Align2::LEFT_CENTER,
        label,
        theme::medium(13.0),
        theme::TEXT_SOFT,
    );
    ui.painter().text(
        rect.right_center() + vec2(-2.0, 0.0),
        Align2::RIGHT_CENTER,
        count.to_string(),
        egui::FontId::proportional(11.0),
        theme::TEXT_DIM,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn session_row(ui: &mut egui::Ui, selected: bool, title: &str, subtitle: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::click());
    widgets::paint_row_bg(
        ui,
        rect.shrink2(vec2(0.0, 1.0)),
        selected,
        response.hovered(),
    );
    let text_rect = rect.shrink2(vec2(12.0, 7.0));
    ui.painter().with_clip_rect(text_rect).text(
        text_rect.left_top(),
        Align2::LEFT_TOP,
        title,
        theme::medium(13.0),
        theme::TEXT_PRIMARY,
    );
    ui.painter().text(
        text_rect.left_bottom(),
        Align2::LEFT_BOTTOM,
        subtitle,
        egui::FontId::proportional(11.0),
        theme::TEXT_DIM,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}
