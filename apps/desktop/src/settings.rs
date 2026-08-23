//! Settings modal: General defaults, Models catalog, and Agent presets.

use egui::{Id, Margin, RichText};

use crate::{
    state::{App, SettingsTab, effort_hint, effort_label},
    theme, widgets,
};

pub fn show(ctx: &egui::Context, app: &mut App) {
    if !app.settings_open {
        return;
    }
    let modal = egui::Modal::new(Id::new("nexa-settings"))
        .backdrop_color(egui::Color32::from_black_alpha(160))
        .frame(
            egui::Frame::new()
                .fill(theme::BG_PANEL)
                .stroke(egui::Stroke::new(1.0, theme::STROKE))
                .corner_radius(egui::CornerRadius::same(14))
                .inner_margin(Margin::same(16))
                .shadow(egui::Shadow {
                    offset: [0, 12],
                    blur: 32,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(120),
                }),
        )
        .show(ctx, |ui| {
            ui.set_min_size(egui::vec2(640.0, 440.0));
            ui.set_max_size(egui::vec2(720.0, 520.0));
            egui::containers::Sides::new().height(28.0).show(
                ui,
                |ui| {
                    ui.label(
                        RichText::new("Settings")
                            .font(theme::semibold(18.0))
                            .color(theme::TEXT_PRIMARY),
                    );
                },
                |ui| {
                    if widgets::icon_button(ui, "×", "Close").clicked() {
                        app.settings_open = false;
                    }
                },
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(150.0);
                    tab_button(ui, app, SettingsTab::General, "General");
                    tab_button(ui, app, SettingsTab::Models, "Models");
                    tab_button(ui, app, SettingsTab::Presets, "Agent presets");
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.set_min_width(420.0);
                    match app.settings_tab {
                        SettingsTab::General => general_tab(ui, app),
                        SettingsTab::Models => models_tab(ui, app),
                        SettingsTab::Presets => presets_tab(ui, app),
                    }
                });
            });
        });
    if modal.should_close() {
        app.settings_open = false;
    }
}

fn tab_button(ui: &mut egui::Ui, app: &mut App, tab: SettingsTab, label: &str) {
    let selected = app.settings_tab == tab;
    let fill = if selected {
        theme::ACCENT_SOFT
    } else {
        egui::Color32::TRANSPARENT
    };
    let color = if selected {
        theme::ACCENT_BRIGHT
    } else {
        theme::TEXT_SOFT
    };
    if ui
        .add_sized(
            [ui.available_width(), 30.0],
            egui::Button::new(RichText::new(label).font(theme::medium(13.5)).color(color))
                .fill(fill)
                .stroke(egui::Stroke::NONE)
                .corner_radius(8),
        )
        .clicked()
    {
        app.settings_tab = tab;
    }
}

fn general_tab(ui: &mut egui::Ui, app: &mut App) {
    ui.label(
        RichText::new("Defaults")
            .font(theme::semibold(16.0))
            .color(theme::TEXT_PRIMARY),
    );
    ui.label(
        RichText::new(
            "Used for every new chat. Changing a picker in the composer updates these too.",
        )
        .size(12.5)
        .color(theme::TEXT_DIM),
    );
    ui.add_space(16.0);

    field_label(ui, "Agent preset", "Which tools a new chat may use.");
    preset_list(ui, app);
    ui.add_space(14.0);

    field_label(ui, "Model", "Preselected the next time Nexa starts.");
    model_list(ui, app);
    ui.add_space(14.0);

    field_label(
        ui,
        "Reasoning",
        "How much extra thinking the default model should spend.",
    );
    effort_list(ui, app);
}

fn field_label(ui: &mut egui::Ui, title: &str, hint: &str) {
    ui.label(
        RichText::new(title)
            .font(theme::medium(13.0))
            .color(theme::TEXT_PRIMARY),
    );
    ui.label(RichText::new(hint).size(12.0).color(theme::TEXT_DIM));
    ui.add_space(4.0);
}

fn preset_list(ui: &mut egui::Ui, app: &mut App) {
    egui::Frame::new()
        .fill(theme::BG_MAIN)
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
            if widgets::picker_row(
                ui,
                "standard",
                app.desktop_settings.default_preset.is_none(),
                "Standard",
                Some("Every tool"),
            )
            .clicked()
            {
                app.selected_preset = None;
                app.set_default_preset(None);
            }
            for preset in app.presets.clone() {
                let active =
                    app.desktop_settings.default_preset.as_deref() == Some(preset.name.as_str());
                let tools = if preset.tools.is_empty() {
                    "Every tool".to_owned()
                } else {
                    preset.tools.join(", ")
                };
                if widgets::picker_row(ui, &preset.name, active, &preset.name, Some(&tools))
                    .clicked()
                {
                    app.selected_preset = Some(preset.name.clone());
                    app.set_default_preset(Some(preset.name));
                }
            }
        });
}

fn model_list(ui: &mut egui::Ui, app: &mut App) {
    if app.choices.is_empty() {
        ui.label(
            RichText::new("No models configured. Run `nexa provider add`.")
                .size(12.0)
                .color(theme::TEXT_DIM),
        );
        return;
    }
    egui::Frame::new()
        .fill(theme::BG_MAIN)
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
            let mut last_provider = String::new();
            egui::ScrollArea::vertical()
                .max_height(160.0)
                .show(ui, |ui| {
                    for index in 0..app.choices.len() {
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
                        }
                    }
                });
        });
}

fn effort_list(ui: &mut egui::Ui, app: &mut App) {
    if !app.effort_picker_visible() {
        ui.label(
            RichText::new("This model does not expose a reasoning-effort setting.")
                .size(12.5)
                .color(theme::TEXT_DIM),
        );
        return;
    }
    egui::Frame::new()
        .fill(theme::BG_MAIN)
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
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
                }
            }
        });
}

fn models_tab(ui: &mut egui::Ui, app: &mut App) {
    ui.label(
        RichText::new("Models")
            .font(theme::semibold(16.0))
            .color(theme::TEXT_PRIMARY),
    );
    ui.label(
        RichText::new("Providers and models configured for this machine (`nexa provider add`).")
            .size(12.5)
            .color(theme::TEXT_DIM),
    );
    ui.add_space(10.0);

    if app.choices.is_empty() {
        ui.label(RichText::new("No providers configured yet.").color(theme::TEXT_DIM));
        return;
    }

    egui::ScrollArea::vertical().show(ui, |ui| {
        let mut last_provider = String::new();
        for choice in &app.choices {
            if choice.provider_id != last_provider {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(&choice.provider_name)
                        .font(theme::semibold(14.0))
                        .color(theme::TEXT_PRIMARY),
                );
                last_provider.clone_from(&choice.provider_id);
            }
            ui.label(
                RichText::new(&choice.model)
                    .color(theme::TEXT_SOFT)
                    .size(13.5),
            );
        }
    });
}

fn presets_tab(ui: &mut egui::Ui, app: &mut App) {
    ui.label(
        RichText::new("Agent presets")
            .font(theme::semibold(16.0))
            .color(theme::TEXT_PRIMARY),
    );
    ui.label(
        RichText::new("Named tool scopes from `$NEXA_HOME/agent-presets/`.")
            .size(12.5)
            .color(theme::TEXT_DIM),
    );
    ui.add_space(10.0);

    egui::ScrollArea::vertical().show(ui, |ui| {
        preset_row(
            ui,
            "Standard",
            "every tool",
            "",
            app.desktop_settings.default_preset.is_none(),
        );
        for preset in app.presets.clone() {
            let is_default =
                app.desktop_settings.default_preset.as_deref() == Some(preset.name.as_str());
            let tools = if preset.tools.is_empty() {
                "every tool".to_owned()
            } else {
                preset.tools.join(", ")
            };
            preset_row(ui, &preset.name, &tools, &preset.description, is_default);
        }
    });
}

fn preset_row(ui: &mut egui::Ui, name: &str, tools: &str, description: &str, is_default: bool) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(name)
                .font(theme::semibold(13.0))
                .color(theme::TEXT_PRIMARY),
        );
        if is_default {
            ui.label(
                RichText::new("default")
                    .color(theme::ACCENT_BRIGHT)
                    .size(12.0),
            );
        }
    });
    if !description.is_empty() {
        ui.label(
            RichText::new(description)
                .color(theme::TEXT_SOFT)
                .size(12.5),
        );
    }
    ui.label(
        RichText::new(format!("tools: {tools}"))
            .color(theme::TEXT_DIM)
            .size(12.0),
    );
    ui.add_space(8.0);
}
