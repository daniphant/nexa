//! Shared chrome: pills, picker popovers, rows, and layout helpers.

use egui::{
    Color32, CornerRadius, Frame, Id, Margin, PopupCloseBehavior, PopupKind, Pos2, RectAlign,
    Response, RichText, Sense, Shadow, Shape, Stroke, Vec2, pos2, vec2,
};

use crate::theme;

pub const CONTENT_WIDTH: f32 = 760.0;
pub const PICKER_WIDTH: f32 = 300.0;

pub fn popover_frame() -> Frame {
    Frame::new()
        .fill(theme::BG_POPOVER)
        .stroke(Stroke::new(1.0, theme::STROKE))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(8))
        .shadow(Shadow {
            offset: [0, 10],
            blur: 28,
            spread: 0,
            color: Color32::from_black_alpha(110),
        })
}

fn picker_style(style: &mut egui::Style) {
    style.spacing.item_spacing = vec2(2.0, 2.0);
    style.spacing.button_padding = vec2(8.0, 5.0);
    style.visuals.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    style.visuals.widgets.inactive.bg_fill = Color32::TRANSPARENT;
    style.visuals.widgets.inactive.bg_stroke = Stroke::NONE;
    style.visuals.widgets.hovered.bg_stroke = Stroke::NONE;
    style.visuals.widgets.active.bg_stroke = Stroke::NONE;
    style.visuals.widgets.open.bg_stroke = Stroke::NONE;
}

/// Compact rounded trigger used by the composer pickers.
pub fn pill(ui: &mut egui::Ui, label: &str, open: bool) -> Response {
    let fill = if open {
        theme::BG_HOVER
    } else {
        theme::BG_FIELD
    };
    let stroke = if open {
        Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.7))
    } else {
        Stroke::new(1.0, theme::STROKE)
    };
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_owned(), theme::medium(12.5), theme::TEXT_PRIMARY);
    let padding = vec2(10.0, 6.0);
    let chevron = 7.0;
    let gap = 6.0;
    let height = (galley.size().y + padding.y * 2.0).max(28.0);
    let width = galley.size().x + padding.x * 2.0 + gap + chevron;
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    let fill = if response.hovered() && !open {
        theme::BG_HOVER
    } else {
        fill
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(16),
        fill,
        stroke,
        egui::StrokeKind::Inside,
    );
    let text_pos = pos2(
        rect.left() + padding.x,
        rect.center().y - galley.size().y * 0.5,
    );
    ui.painter().galley(text_pos, galley, theme::TEXT_PRIMARY);
    let chevron_center = pos2(rect.right() - padding.x - chevron * 0.5, rect.center().y);
    paint_chevron(ui, chevron_center, open, theme::TEXT_SOFT);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn paint_chevron(ui: &egui::Ui, center: Pos2, open: bool, color: Color32) {
    let w = 3.6;
    let h = 2.4;
    let points = if open {
        vec![
            pos2(center.x, center.y - h),
            pos2(center.x + w, center.y + h),
            pos2(center.x - w, center.y + h),
        ]
    } else {
        vec![
            pos2(center.x - w, center.y - h),
            pos2(center.x + w, center.y - h),
            pos2(center.x, center.y + h),
        ]
    };
    ui.painter()
        .add(Shape::convex_polygon(points, color, Stroke::NONE));
}

/// Paints the small disclosure marker used by expandable rows.
pub fn paint_disclosure(ui: &egui::Ui, center: Pos2, open: bool, color: Color32) {
    let w = 3.5;
    let h = 3.0;
    let points = if open {
        vec![
            pos2(center.x - w, center.y - h),
            pos2(center.x + w, center.y - h),
            pos2(center.x, center.y + h),
        ]
    } else {
        vec![
            pos2(center.x - h, center.y - w),
            pos2(center.x - h, center.y + w),
            pos2(center.x + h, center.y),
        ]
    };
    ui.painter()
        .add(Shape::convex_polygon(points, color, Stroke::NONE));
}

/// Allocates and paints a disclosure marker inside a horizontal layout.
pub fn disclosure_icon(ui: &mut egui::Ui, open: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(14.0, 18.0), Sense::hover());
    paint_disclosure(ui, rect.center(), open, theme::TEXT_DIM);
}

pub fn picker_id(ctx: &egui::Context, salt: &'static str) -> (Id, bool) {
    let id = Id::new(salt);
    (id, egui::Popup::is_id_open(ctx, id))
}

/// A picker anchored to a pill. Composer-docked pickers prefer opening upward
/// so they stay on screen; the empty-state composer prefers opening downward.
pub fn picker(
    button: &Response,
    popup_id: Id,
    width: f32,
    prefer_up: bool,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    let (primary, alternatives) = if prefer_up {
        (
            RectAlign::TOP_START,
            [
                RectAlign::TOP_START,
                RectAlign::TOP_END,
                RectAlign::BOTTOM_START,
                RectAlign::BOTTOM_END,
            ],
        )
    } else {
        (
            RectAlign::BOTTOM_START,
            [
                RectAlign::BOTTOM_START,
                RectAlign::BOTTOM_END,
                RectAlign::TOP_START,
                RectAlign::TOP_END,
            ],
        )
    };
    egui::Popup::from_toggle_button_response(button)
        .id(popup_id)
        .kind(PopupKind::Menu)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .align(primary)
        .align_alternatives(&alternatives)
        .gap(6.0)
        .width(width)
        .frame(popover_frame())
        .style(picker_style)
        .show(|ui| {
            ui.set_min_width(width);
            ui.set_max_width(width);
            add_contents(ui);
        });
}

pub fn picker_title(ui: &mut egui::Ui, title: &str) {
    ui.label(
        RichText::new(title)
            .font(theme::semibold(12.0))
            .color(theme::TEXT_SOFT),
    );
    ui.add_space(4.0);
}

pub fn picker_section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(
        RichText::new(title)
            .font(theme::medium(11.0))
            .color(theme::TEXT_DIM),
    );
    ui.add_space(2.0);
}

pub fn picker_search(ui: &mut egui::Ui, filter: &mut String, hint: &str) -> Response {
    let inner = Frame::new()
        .fill(theme::BG_FIELD)
        .stroke(Stroke::new(1.0, theme::STROKE))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(filter)
                    .hint_text(RichText::new(hint).color(theme::TEXT_DIM))
                    .frame(egui::Frame::NONE)
                    .desired_width(f32::INFINITY)
                    .font(egui::FontId::proportional(13.0)),
            )
        });
    inner.inner
}

pub fn picker_list(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::ScrollArea::vertical()
        .max_height(320.0)
        .auto_shrink([false, true])
        .show(ui, add_contents);
}

/// Two-line selectable row used inside pickers. `id_salt` must be unique in
/// the current picker.
pub fn picker_row(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    selected: bool,
    title: &str,
    subtitle: Option<&str>,
) -> Response {
    let id = ui.id().with(id_salt);
    let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
    let fill = if selected {
        theme::ACCENT_SOFT
    } else if hovered {
        theme::BG_HOVER
    } else {
        Color32::TRANSPARENT
    };
    let title_color = if selected {
        theme::ACCENT_BRIGHT
    } else {
        theme::TEXT_PRIMARY
    };
    let shown = Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin {
            left: 10,
            right: 8,
            top: 7,
            bottom: 7,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::containers::Sides::new()
                .height(if subtitle.is_some() { 34.0 } else { 18.0 })
                .show(
                    ui,
                    |ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(title)
                                    .font(theme::medium(13.0))
                                    .color(title_color),
                            );
                            if let Some(subtitle) = subtitle {
                                ui.label(RichText::new(subtitle).size(11.5).color(theme::TEXT_DIM));
                            }
                        });
                    },
                    |ui| {
                        if selected {
                            ui.label(
                                RichText::new("✓")
                                    .font(theme::semibold(14.0))
                                    .color(theme::ACCENT_BRIGHT),
                            );
                        }
                    },
                );
        });
    ui.interact(shown.response.rect, id, Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn icon_button(ui: &mut egui::Ui, icon: &str, tip: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(icon).size(15.0).color(theme::TEXT_SOFT))
            .frame_when_inactive(false)
            .min_size(vec2(32.0, 32.0))
            .corner_radius(CornerRadius::same(8)),
    )
    .on_hover_text(tip)
}

pub fn ghost_button(ui: &mut egui::Ui, label: &str, selected: bool) -> Response {
    let fill = if selected {
        theme::ACCENT_SOFT
    } else {
        Color32::TRANSPARENT
    };
    let color = if selected {
        theme::ACCENT_BRIGHT
    } else {
        theme::TEXT_SOFT
    };
    ui.add(
        egui::Button::new(RichText::new(label).font(theme::medium(12.5)).color(color))
            .fill(fill)
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(8)),
    )
}

pub fn send_button(ui: &mut egui::Ui, enabled: bool) -> Response {
    let size = vec2(34.0, 34.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let fill = if !enabled {
        theme::BG_HOVER
    } else if response.hovered() {
        theme::ACCENT_BRIGHT
    } else {
        theme::ACCENT
    };
    let arrow = if enabled {
        Color32::WHITE
    } else {
        theme::TEXT_DIM
    };
    ui.painter().circle_filled(rect.center(), 17.0, fill);
    let c = rect.center();
    ui.painter().add(Shape::convex_polygon(
        vec![
            pos2(c.x, c.y - 6.0),
            pos2(c.x + 5.5, c.y + 3.5),
            pos2(c.x - 5.5, c.y + 3.5),
        ],
        arrow,
        Stroke::NONE,
    ));
    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

pub fn centered_column(ui: &mut egui::Ui, max_width: f32, add: impl FnOnce(&mut egui::Ui)) {
    ui.vertical_centered(|ui| {
        let width = ui.available_width().min(max_width);
        ui.set_width(width);
        add(ui);
    });
}

pub fn search_field(ui: &mut egui::Ui, filter: &mut String, hint: &str) {
    Frame::new()
        .fill(theme::BG_MAIN)
        .stroke(Stroke::new(1.0, theme::STROKE))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(filter)
                    .hint_text(RichText::new(hint).color(theme::TEXT_DIM))
                    .frame(egui::Frame::NONE)
                    .desired_width(f32::INFINITY)
                    .font(egui::FontId::proportional(13.0)),
            );
        });
}

pub fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let taken: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{taken}…")
    } else {
        taken
    }
}

pub fn title_from_message(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text)
        .trim();
    truncate(line, 42)
}

pub fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

/// Left accent bar + fill for the active sidebar session.
pub fn paint_row_bg(ui: &egui::Ui, rect: egui::Rect, selected: bool, hovered: bool) {
    let fill = if selected {
        theme::BG_HOVER
    } else if hovered {
        theme::BG_FIELD
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    if selected {
        let bar = egui::Rect::from_min_max(
            rect.left_top() + vec2(0.0, 8.0),
            rect.left_bottom() + vec2(3.0, -8.0),
        );
        ui.painter()
            .rect_filled(bar, CornerRadius::same(2), theme::ACCENT);
    }
}
