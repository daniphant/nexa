//! Palette, typography, and style setup for the desktop client.
//!
//! Colors follow DeepSeek Harness's dark tokens (`design-platform.css`).
//! Fonts are Inter and JetBrains Mono with a bundled symbol fallback (see
//! `assets/fonts/README.md`).

use std::sync::Arc;

use egui::{
    Color32, FontData, FontDefinitions, FontFamily, FontTweak, Shadow, Stroke,
    epaint::text::VariationCoords,
};

pub const ACCENT: Color32 = Color32::from_rgb(86, 134, 254);
pub const ACCENT_BRIGHT: Color32 = Color32::from_rgb(103, 158, 254);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(40, 52, 86);
pub const BG_MAIN: Color32 = Color32::from_rgb(21, 21, 23);
pub const BG_PANEL: Color32 = Color32::from_rgb(27, 27, 28);
pub const BG_FIELD: Color32 = Color32::from_rgb(44, 44, 46);
pub const BG_HOVER: Color32 = Color32::from_rgb(53, 54, 56);
pub const BG_POPOVER: Color32 = Color32::from_rgb(34, 34, 36);
pub const BG_CODE: Color32 = Color32::from_rgb(30, 30, 32);
pub const BUBBLE_USER: Color32 = Color32::from_rgb(44, 44, 46);
pub const STROKE: Color32 = Color32::from_rgb(58, 58, 62);
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(235, 238, 242);
pub const TEXT_SOFT: Color32 = Color32::from_rgb(151, 157, 166);
pub const TEXT_DIM: Color32 = Color32::from_rgb(97, 102, 107);
pub const DANGER: Color32 = Color32::from_rgb(242, 90, 90);
pub const DANGER_BG: Color32 = Color32::from_rgb(42, 20, 20);
pub const SUCCESS: Color32 = Color32::from_rgb(110, 198, 146);
pub const WARNING: Color32 = Color32::from_rgb(232, 186, 92);

pub const FONT_INTER_REGULAR: &str = "Inter-Regular";
pub const FONT_INTER_MEDIUM: &str = "Inter-Medium";
pub const FONT_INTER_SEMIBOLD: &str = "Inter-SemiBold";
const FONT_HACK: &str = "Hack";
const FONT_SYMBOLS: &str = "NotoSansSymbols2";
const FONT_UBUNTU: &str = "Ubuntu-Light";
const FONT_NOTO_EMOJI: &str = "NotoEmoji-Regular";
const FONT_EMOJI_ICON: &str = "emoji-icon-font";
const FONT_MONO: &str = "JetBrainsMono";

/// A `FontId` in the semibold Inter face, for pill labels and section titles.
pub fn semibold(size: f32) -> egui::FontId {
    egui::FontId::new(size, FontFamily::Name(FONT_INTER_SEMIBOLD.into()))
}

/// A `FontId` in the medium Inter face, for secondary emphasis.
pub fn medium(size: f32) -> egui::FontId {
    egui::FontId::new(size, FontFamily::Name(FONT_INTER_MEDIUM.into()))
}

/// Installs the bundled fonts and dark palette. Call once from the
/// `eframe::CreationContext`; egui caches the rasterized glyph atlas per
/// `FontId`, so re-installing every frame would be wasteful.
pub fn configure(ctx: &egui::Context) {
    install_fonts(ctx);

    ctx.set_theme(egui::Theme::Dark);
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.dark_mode = true;
    style.visuals.override_text_color = Some(TEXT_PRIMARY);
    style.visuals.panel_fill = BG_PANEL;
    style.visuals.window_fill = BG_POPOVER;
    style.visuals.window_stroke = Stroke::new(1.0, STROKE);
    style.visuals.window_corner_radius = egui::CornerRadius::same(12);
    style.visuals.menu_corner_radius = egui::CornerRadius::same(12);
    style.visuals.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(90),
    };
    style.visuals.popup_shadow = Shadow {
        offset: [0, 10],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(110),
    };
    style.visuals.extreme_bg_color = BG_FIELD;
    style.visuals.faint_bg_color = BG_HOVER;
    style.visuals.code_bg_color = BG_CODE;
    style.visuals.hyperlink_color = ACCENT;
    style.visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    style.visuals.selection.bg_fill = ACCENT_SOFT;
    style.visuals.warn_fg_color = WARNING;
    style.visuals.error_fg_color = DANGER;
    style.visuals.widgets.noninteractive.bg_fill = BG_PANEL;
    style.visuals.widgets.noninteractive.weak_bg_fill = BG_PANEL;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, STROKE);
    style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    style.visuals.widgets.inactive.bg_fill = BG_FIELD;
    style.visuals.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    style.visuals.widgets.inactive.bg_stroke = Stroke::NONE;
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    style.visuals.widgets.hovered.bg_fill = BG_HOVER;
    style.visuals.widgets.hovered.weak_bg_fill = BG_HOVER;
    style.visuals.widgets.hovered.bg_stroke = Stroke::NONE;
    style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    style.visuals.widgets.active.bg_fill = BG_FIELD;
    style.visuals.widgets.active.weak_bg_fill = BG_FIELD;
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    style.visuals.widgets.active.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    style.visuals.widgets.open.bg_fill = BG_HOVER;
    style.visuals.widgets.open.weak_bg_fill = BG_HOVER;
    style.visuals.widgets.open.bg_stroke = Stroke::new(1.0, STROKE);
    style.visuals.widgets.open.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    for widget in [
        &mut style.visuals.widgets.noninteractive,
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(8);
        widget.expansion = 0.0;
    }
    style.visuals.indent_has_left_vline = false;
    style.visuals.collapsing_header_frame = false;
    style.visuals.striped = false;
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.menu_margin = egui::Margin::same(8);
    style.spacing.combo_width = 220.0;
    style.spacing.interact_size = egui::vec2(40.0, 28.0);
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(13.5));
    style
        .text_styles
        .insert(egui::TextStyle::Monospace, egui::FontId::monospace(13.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, semibold(20.0));
    ctx.set_style_of(egui::Theme::Dark, style);
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let inter: &'static [u8] = include_bytes!("../assets/fonts/Inter.ttf");
    let mono: &'static [u8] = include_bytes!("../assets/fonts/JetBrainsMono.ttf");
    let symbols: &'static [u8] = include_bytes!("../assets/fonts/NotoSansSymbols2-Regular.ttf");

    // Keep the fallback chain independent of egui's `default_fonts` feature.
    // Noto Sans Symbols 2 covers the broad Unicode symbol range, while the
    // bundled Hack and emoji faces fill in glyphs that it does not contain.
    for (name, data) in [
        (FONT_SYMBOLS, symbols),
        (FONT_HACK, epaint_default_fonts::HACK_REGULAR),
        (FONT_UBUNTU, epaint_default_fonts::UBUNTU_LIGHT),
        (FONT_NOTO_EMOJI, epaint_default_fonts::NOTO_EMOJI_REGULAR),
        (FONT_EMOJI_ICON, epaint_default_fonts::EMOJI_ICON),
    ] {
        fonts
            .font_data
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(FontData::from_static(data)));
    }

    for (name, weight) in [
        (FONT_INTER_REGULAR, 400.0_f32),
        (FONT_INTER_MEDIUM, 500.0),
        (FONT_INTER_SEMIBOLD, 600.0),
    ] {
        let data = FontData::from_static(inter).tweak(FontTweak {
            coords: VariationCoords::new([(b"wght", weight)]),
            ..FontTweak::default()
        });
        fonts.font_data.insert(name.to_owned(), Arc::new(data));
    }
    fonts
        .font_data
        .insert(FONT_MONO.to_owned(), Arc::new(FontData::from_static(mono)));

    let existing_proportional = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut proportional_fallbacks = vec![FONT_SYMBOLS.to_owned(), FONT_HACK.to_owned()];
    proportional_fallbacks.extend(
        existing_proportional
            .into_iter()
            .filter(|name| name != FONT_SYMBOLS && name != FONT_HACK && name != FONT_INTER_REGULAR),
    );

    let mut proportional = vec![FONT_INTER_REGULAR.to_owned()];
    proportional.extend(proportional_fallbacks.iter().cloned());
    fonts
        .families
        .insert(FontFamily::Proportional, proportional);

    let mut monospace = vec![FONT_MONO.to_owned()];
    monospace.extend(proportional_fallbacks.iter().cloned());
    fonts.families.insert(FontFamily::Monospace, monospace);

    // Every named Inter weight needs the same fallback chain as the default
    // proportional family. Without this, RichText with an explicit weight
    // falls straight through to egui's replacement glyph.
    for name in [FONT_INTER_REGULAR, FONT_INTER_MEDIUM, FONT_INTER_SEMIBOLD] {
        let mut family = vec![name.to_owned()];
        family.extend(proportional_fallbacks.iter().cloned());
        fonts.families.insert(FontFamily::Name(name.into()), family);
    }

    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::{
        FONT_EMOJI_ICON, FONT_HACK, FONT_INTER_MEDIUM, FONT_INTER_REGULAR, FONT_INTER_SEMIBOLD,
        FONT_MONO, FONT_NOTO_EMOJI, FONT_SYMBOLS, FONT_UBUNTU, install_fonts,
    };

    #[test]
    fn every_text_family_has_the_bundled_fallback_chain() {
        let ctx = egui::Context::default();
        install_fonts(&ctx);

        let families = [
            egui::FontFamily::Proportional,
            egui::FontFamily::Monospace,
            egui::FontFamily::Name(FONT_INTER_REGULAR.into()),
            egui::FontFamily::Name(FONT_INTER_MEDIUM.into()),
            egui::FontFamily::Name(FONT_INTER_SEMIBOLD.into()),
        ];
        let fallback_names = [
            FONT_SYMBOLS,
            FONT_HACK,
            FONT_UBUNTU,
            FONT_NOTO_EMOJI,
            FONT_EMOJI_ICON,
        ];

        ctx.begin_pass(egui::RawInput::default());
        ctx.fonts(|fonts| {
            for family in families {
                let chain = fonts
                    .definitions()
                    .families
                    .get(&family)
                    .expect("text family should be configured");
                assert_eq!(
                    chain.iter().skip(1).map(String::as_str).collect::<Vec<_>>(),
                    fallback_names.to_vec()
                );
            }
            for name in fallback_names {
                assert!(fonts.definitions().font_data.contains_key(name));
            }
            assert!(fonts.definitions().font_data.contains_key(FONT_MONO));
        });
        let mut output = ctx.end_pass();
        output.textures_delta.clear();
    }
}
