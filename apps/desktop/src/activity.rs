//! Right-hand activity inspector for the current chat.

use egui::RichText;

use crate::{
    state::{ActivityKind, App},
    theme, widgets,
};

pub fn show(ui: &mut egui::Ui, app: &mut App) {
    egui::containers::Sides::new().height(28.0).show(
        ui,
        |ui| {
            ui.label(
                RichText::new("Activity")
                    .font(theme::semibold(14.0))
                    .color(theme::TEXT_PRIMARY),
            );
        },
        |ui| {
            if widgets::icon_button(ui, "×", "Close").clicked() {
                app.activity_open = false;
            }
        },
    );
    ui.label(
        RichText::new("Everything this chat has done, in order.")
            .size(11.5)
            .color(theme::TEXT_DIM),
    );
    ui.add_space(8.0);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            if app.activity.is_empty() {
                ui.label(
                    RichText::new("Activity from this chat will show up here.")
                        .color(theme::TEXT_DIM)
                        .italics(),
                );
                return;
            }
            for line in &app.activity {
                let (label, color) = match line.kind {
                    ActivityKind::User => ("Prompt", theme::ACCENT_BRIGHT),
                    ActivityKind::Run => ("Run", theme::TEXT_SOFT),
                    ActivityKind::Tool => ("Tool", theme::WARNING),
                    ActivityKind::Done => ("Done", theme::SUCCESS),
                    ActivityKind::Error => ("Error", theme::DANGER),
                };
                ui.add_space(4.0);
                ui.label(RichText::new(label).font(theme::medium(11.0)).color(color));
                ui.label(
                    RichText::new(&line.text)
                        .size(12.5)
                        .color(theme::TEXT_PRIMARY),
                );
            }
        });
}
