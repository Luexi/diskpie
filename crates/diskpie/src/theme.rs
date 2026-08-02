//! DiskPie visual tokens adapted to egui's light and dark themes.

use eframe::egui::{self, Color32, Stroke, Theme, Visuals};

pub const SCAN_CURRENT: Color32 = Color32::from_rgb(5, 188, 235);
pub const SECTOR_EMBER: Color32 = Color32::from_rgb(247, 154, 30);
pub const PLATTER_MIDNIGHT: Color32 = Color32::from_rgb(6, 47, 87);
pub const SEPARATOR_PORCELAIN: Color32 = Color32::from_rgb(254, 251, 240);
pub const CHASSIS_SLATE: Color32 = Color32::from_rgb(32, 36, 43);
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
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(8.0, 6.0);
    style.spacing.interact_size = egui::vec2(36.0, 32.0);
    style.spacing.menu_margin = egui::Margin::same(8);
}

fn dark_visuals() -> Visuals {
    let mut visuals = Visuals::dark();
    visuals.panel_fill = CHASSIS_SLATE;
    visuals.window_fill = Color32::from_rgb(38, 43, 51);
    visuals.extreme_bg_color = Color32::from_rgb(26, 30, 36);
    visuals.faint_bg_color = Color32::from_rgb(42, 47, 55);
    visuals.selection.bg_fill = PLATTER_MIDNIGHT;
    visuals.selection.stroke = Stroke::new(1.5, SCAN_CURRENT);
    visuals.hyperlink_color = SCAN_CURRENT;
    visuals.warn_fg_color = SECTOR_EMBER;
    visuals.error_fg_color = DANGER_STOP;
    visuals.widgets.active.bg_fill = PLATTER_MIDNIGHT;
    visuals.widgets.active.fg_stroke = Stroke::new(1.5, SEPARATOR_PORCELAIN);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, SCAN_CURRENT.gamma_multiply(0.7));
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_gray(58));
    visuals
}

fn light_visuals() -> Visuals {
    let mut visuals = Visuals::light();
    visuals.panel_fill = Color32::from_rgb(244, 247, 249);
    visuals.window_fill = Color32::WHITE;
    visuals.extreme_bg_color = Color32::from_rgb(229, 235, 239);
    visuals.faint_bg_color = Color32::from_rgb(236, 241, 244);
    visuals.selection.bg_fill = SCAN_CURRENT.gamma_multiply(0.24);
    visuals.selection.stroke = Stroke::new(1.5, PLATTER_MIDNIGHT);
    visuals.hyperlink_color = PLATTER_MIDNIGHT;
    visuals.warn_fg_color = Color32::from_rgb(158, 88, 0);
    visuals.error_fg_color = Color32::from_rgb(164, 45, 58);
    visuals.widgets.active.bg_fill = SCAN_CURRENT.gamma_multiply(0.28);
    visuals.widgets.active.fg_stroke = Stroke::new(1.5, PLATTER_MIDNIGHT);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, SCAN_CURRENT);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_gray(204));
    visuals
}
