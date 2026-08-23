//! Transcript bubbles, tool cards, and a small markdown subset for assistant text.

use egui::{Align, Color32, Layout, RichText, Sense, text::LayoutJob, text::TextFormat};

use crate::{
    state::TranscriptEntry,
    theme,
    widgets::{self, CONTENT_WIDTH},
};

pub fn render_entry(ui: &mut egui::Ui, entry: &TranscriptEntry) {
    match entry {
        TranscriptEntry::User { text } => user_bubble(ui, text),
        TranscriptEntry::Assistant {
            text, streaming, ..
        } => assistant_block(ui, text, *streaming),
        TranscriptEntry::Tool {
            call_id,
            name,
            arguments,
            result,
            failed,
        } => tool_card(ui, call_id, name, arguments, result.as_deref(), *failed),
        TranscriptEntry::Error { text } => error_card(ui, text),
    }
}

fn user_bubble(ui: &mut egui::Ui, text: &str) {
    let available = ui.available_width();
    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
        egui::Frame::new()
            .fill(theme::BUBBLE_USER)
            .corner_radius(egui::CornerRadius {
                nw: 14,
                ne: 14,
                sw: 14,
                se: 4,
            })
            .inner_margin(egui::Margin::symmetric(12, 9))
            .show(ui, |ui| {
                ui.set_max_width(available * 0.72);
                ui.label(RichText::new(text).color(theme::TEXT_PRIMARY).size(14.5));
            });
    });
    ui.add_space(10.0);
}

fn assistant_block(ui: &mut egui::Ui, text: &str, streaming: bool) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Nexa")
                .font(theme::semibold(12.5))
                .color(theme::TEXT_SOFT),
        );
        if streaming {
            ui.add_space(4.0);
            ui.spinner();
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("Copy").size(11.5).color(theme::TEXT_DIM))
                        .frame_when_inactive(false)
                        .small(),
                )
                .clicked()
            {
                ui.ctx().copy_text(text.to_owned());
            }
        });
    });
    ui.add_space(4.0);
    if text.is_empty() && streaming {
        ui.label(
            RichText::new("Thinking…")
                .italics()
                .color(theme::TEXT_DIM)
                .size(14.0),
        );
    } else {
        render_markdown(ui, text);
    }
    ui.add_space(14.0);
}

fn tool_card(
    ui: &mut egui::Ui,
    call_id: &str,
    name: &str,
    arguments: &str,
    result: Option<&str>,
    failed: bool,
) {
    let id = ui.id().with(("tool-card", call_id));
    let running = result.is_none();
    let mut open = ui
        .ctx()
        .data_mut(|data| *data.get_temp_mut_or_insert_with(id, || running));
    let (status, status_color) = if failed {
        ("Failed", theme::DANGER)
    } else if running {
        ("Running", theme::WARNING)
    } else {
        ("Done", theme::SUCCESS)
    };
    let preview = tool_preview(name, arguments);

    let header = egui::Frame::new()
        .fill(theme::BG_PANEL)
        .stroke(egui::Stroke::new(1.0, theme::STROKE))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                widgets::disclosure_icon(ui, open);
                ui.label(
                    RichText::new(&preview)
                        .font(theme::medium(12.5))
                        .color(theme::TEXT_PRIMARY),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if running {
                        ui.spinner();
                    }
                    ui.label(RichText::new(status).size(11.5).color(status_color));
                });
            });
            if open {
                ui.add_space(6.0);
                if !arguments.trim().is_empty() && arguments.trim() != "{}" {
                    ui.label(RichText::new("Input").size(11.0).color(theme::TEXT_DIM));
                    code_block(ui, arguments);
                }
                if let Some(result) = result {
                    ui.add_space(4.0);
                    ui.label(RichText::new("Output").size(11.0).color(theme::TEXT_DIM));
                    let shown = widgets::truncate(result.trim(), 2000);
                    code_block(ui, &shown);
                }
            }
        });
    let response = ui.interact(header.response.rect, id.with("hit"), Sense::click());
    if response.clicked() {
        open = !open;
        ui.ctx().data_mut(|data| data.insert_temp(id, open));
    }
    ui.add_space(8.0);
}

fn error_card(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(theme::DANGER_BG)
        .stroke(egui::Stroke::new(1.0, theme::DANGER.gamma_multiply(0.4)))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.label(
                RichText::new("Run failed")
                    .font(theme::semibold(13.0))
                    .color(theme::DANGER),
            );
            ui.label(RichText::new(text).color(theme::DANGER));
        });
    ui.add_space(10.0);
}

fn tool_preview(name: &str, arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return name.to_owned();
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(object) = value.as_object()
    {
        for key in ["path", "command", "query", "pattern", "file", "glob"] {
            if let Some(text) = object.get(key).and_then(|value| value.as_str()) {
                return format!("{name}  {}", widgets::truncate(text, 56));
            }
        }
    }
    format!(
        "{name}  {}",
        widgets::truncate(&trimmed.replace('\n', " "), 56)
    )
}

fn code_block(ui: &mut egui::Ui, code: &str) {
    egui::Frame::new()
        .fill(theme::BG_CODE)
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(
                    RichText::new(code)
                        .monospace()
                        .size(12.0)
                        .color(theme::TEXT_PRIMARY),
                )
                .selectable(true)
                .wrap(),
            );
        });
}

fn render_markdown(ui: &mut egui::Ui, text: &str) {
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        if start > 0 {
            render_blocks(ui, &rest[..start]);
        }
        rest = &rest[start + 3..];
        let newline = rest.find('\n').unwrap_or(rest.len());
        rest = rest.get(newline + 1..).unwrap_or("");
        if let Some(end) = rest.find("```") {
            code_block(ui, rest[..end].trim_end_matches('\n'));
            rest = rest.get(end + 3..).unwrap_or("");
        } else {
            code_block(ui, rest);
            return;
        }
    }
    render_blocks(ui, rest);
}

fn render_blocks(ui: &mut egui::Ui, text: &str) {
    let mut paragraph = String::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            flush_paragraph(ui, &mut paragraph);
            ui.add_space(6.0);
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix("### ") {
            flush_paragraph(ui, &mut paragraph);
            ui.label(
                RichText::new(heading)
                    .font(theme::semibold(15.0))
                    .color(theme::TEXT_PRIMARY),
            );
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix("## ") {
            flush_paragraph(ui, &mut paragraph);
            ui.label(
                RichText::new(heading)
                    .font(theme::semibold(17.0))
                    .color(theme::TEXT_PRIMARY),
            );
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix("# ") {
            flush_paragraph(ui, &mut paragraph);
            ui.label(
                RichText::new(heading)
                    .font(theme::semibold(19.0))
                    .color(theme::TEXT_PRIMARY),
            );
            continue;
        }
        if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            flush_paragraph(ui, &mut paragraph);
            ui.horizontal(|ui| {
                ui.label(RichText::new("•").color(theme::TEXT_SOFT));
                ui.add(egui::Label::new(inline_job(item, 14.5)).wrap());
            });
            continue;
        }
        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(trimmed);
    }
    flush_paragraph(ui, &mut paragraph);
}

fn flush_paragraph(ui: &mut egui::Ui, paragraph: &mut String) {
    if paragraph.is_empty() {
        return;
    }
    ui.add(egui::Label::new(inline_job(paragraph, 14.5)).wrap());
    paragraph.clear();
}

fn inline_job(text: &str, size: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(index) = rest.find("**") {
            if index > 0 {
                append_code_spans(&mut job, &rest[..index], size, false);
            }
            rest = &rest[index + 2..];
            if let Some(end) = rest.find("**") {
                append_code_spans(&mut job, &rest[..end], size, true);
                rest = &rest[end + 2..];
            } else {
                append_code_spans(&mut job, rest, size, true);
                break;
            }
        } else {
            append_code_spans(&mut job, rest, size, false);
            break;
        }
    }
    job.wrap.max_width = CONTENT_WIDTH;
    job
}

fn append_code_spans(job: &mut LayoutJob, text: &str, size: f32, bold: bool) {
    let mut rest = text;
    while let Some(index) = rest.find('`') {
        if index > 0 {
            append_span(job, &rest[..index], size, bold, false);
        }
        rest = &rest[index + 1..];
        if let Some(end) = rest.find('`') {
            append_span(job, &rest[..end], size, false, true);
            rest = &rest[end + 1..];
        } else {
            append_span(job, rest, size, false, true);
            return;
        }
    }
    append_span(job, rest, size, bold, false);
}

fn append_span(job: &mut LayoutJob, text: &str, size: f32, bold: bool, code: bool) {
    if text.is_empty() {
        return;
    }
    let font_id = if code {
        egui::FontId::monospace((size - 1.0).max(11.0))
    } else if bold {
        theme::semibold(size)
    } else {
        egui::FontId::proportional(size)
    };
    job.append(
        text,
        0.0,
        TextFormat {
            font_id,
            color: theme::TEXT_PRIMARY,
            background: if code {
                theme::BG_CODE
            } else {
                Color32::TRANSPARENT
            },
            ..Default::default()
        },
    );
}
