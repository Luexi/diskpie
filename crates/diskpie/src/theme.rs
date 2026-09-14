//! DiskPie visual tokens adapted to egui's light and dark themes.

use eframe::egui::{self, Color32, Stroke, Theme, Visuals};

pub const SCAN_CURRENT: Color32 = Color32::from_rgb(70, 142, 191);
pub const SECTOR_EMBER: Color32 = Color32::from_rgb(247, 154, 30);
pub const PLATTER_MIDNIGHT: Color32 = Color32::from_rgb(6, 47, 87);
pub const SEPARATOR_PORCELAIN: Color32 = Color32::from_rgb(254, 251, 240);
pub const CHASSIS_SLATE: Color32 = Color32::from_rgb(32, 33, 36);
pub const DANGER_STOP: Color32 = Color32::from_rgb(217, 91, 103);

pub fn install(context: &egui::Context) {
    context.style_mut_of(Theme::Dark, |style| {
        apply_spacing(style);
        style.visuals = dark_visuals();
    });
    context.style_mut_of(Theme::Light, |style| {
        apply_spacing(style);
        style.visuals = light_visuals();
    });
}

fn apply_spacing(style: &mut egui::Style) {
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.interact_size = egui::vec2(28.0, 28.0);
    style.spacing.menu_margin = egui::Margin::same(8);
    for text_style in [egui::TextStyle::Body, egui::TextStyle::Button] {
        style.text_styles.insert(text_style, egui::FontId::proportional(14.0));
    }
    style.text_styles.insert(egui::TextStyle::Small, egui::FontId::proportional(13.0));
    style.text_styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(13.0));
    style.text_styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(18.0));
}

fn dark_visuals() -> Visuals {
    let mut visuals = Visuals::dark();
    visuals.panel_fill = CHASSIS_SLATE;
    visuals.window_fill = Color32::from_rgb(38, 40, 43);
    visuals.extreme_bg_color = Color32::from_rgb(26, 27, 29);
    visuals.faint_bg_color = Color32::from_rgb(48, 50, 55);
    visuals.override_text_color = Some(Color32::from_rgb(236, 237, 239));
    visuals.weak_text_color = Some(Color32::from_rgb(175, 180, 186));
    visuals.selection.bg_fill = Color32::from_rgb(41, 60, 75);
    visuals.selection.stroke = Stroke::new(1.5, Color32::from_rgb(128, 191, 255));
    visuals.hyperlink_color = Color32::from_rgb(128, 191, 255);
    visuals.warn_fg_color = SECTOR_EMBER;
    visuals.error_fg_color = DANGER_STOP;
    visuals.widgets.active.bg_fill = Color32::from_rgb(41, 60, 75);
    visuals.widgets.active.fg_stroke = Stroke::new(1.5, SEPARATOR_PORCELAIN);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, SCAN_CURRENT.gamma_multiply(0.7));
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(64, 67, 73));
    visuals
}

fn light_visuals() -> Visuals {
    let mut visuals = Visuals::light();
    visuals.panel_fill = Color32::WHITE;
    visuals.window_fill = Color32::WHITE;
    visuals.extreme_bg_color = Color32::from_rgb(241, 242, 243);
    visuals.faint_bg_color = Color32::from_rgb(246, 246, 246);
    visuals.override_text_color = Some(Color32::from_rgb(32, 33, 36));
    visuals.weak_text_color = Some(Color32::from_rgb(98, 102, 107));
    visuals.selection.bg_fill = Color32::from_rgb(232, 242, 250);
    visuals.selection.stroke = Stroke::new(1.5, Color32::from_rgb(8, 105, 179));
    visuals.hyperlink_color = Color32::from_rgb(8, 105, 179);
    visuals.warn_fg_color = Color32::from_rgb(158, 88, 0);
    visuals.error_fg_color = Color32::from_rgb(164, 45, 58);
    visuals.widgets.active.bg_fill = Color32::from_rgb(232, 242, 250);
    visuals.widgets.active.fg_stroke = Stroke::new(1.5, PLATTER_MIDNIGHT);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, SCAN_CURRENT);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(222, 223, 225));
    visuals
}
