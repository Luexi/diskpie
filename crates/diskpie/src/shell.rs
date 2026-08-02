//! Responsive native shell for the DiskPie application.

use std::f32::consts::{FRAC_PI_2, TAU};

use eframe::egui::{
    self, Align, Color32, FontId, Layout, Pos2, RichText, Sense, Shape, Stroke, ThemePreference,
    Vec2,
};

use crate::theme;

pub struct DiskPieShell {
    theme_preference: ThemePreference,
    metric: SizeMetric,
    notice: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SizeMetric {
    Logical,
    Allocated,
}

impl DiskPieShell {
    pub fn new(creation: &eframe::CreationContext<'_>) -> Self {
        theme::install(&creation.egui_ctx);
        creation.egui_ctx.set_theme(ThemePreference::System);
        Self {
            theme_preference: ThemePreference::System,
            metric: SizeMetric::Allocated,
            notice: "Ready — choose a folder or drive to begin",
        }
    }

    fn command_rail(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("command-rail").exact_size(52.0).show_separator_line(true).show(
            ui,
            |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("DiskPie").strong().size(18.0).color(theme::SCAN_CURRENT),
                    );
                    ui.separator();
                    ui.add_enabled(false, egui::Button::new("Back"))
                        .on_disabled_hover_text("No navigation history yet");
                    ui.add_enabled(false, egui::Button::new("Parent"))
                        .on_disabled_hover_text("No parent folder yet");
                    if ui.button("Choose folder…").clicked() {
                        self.notice = "Folder picker is not connected yet";
                    }
                    ui.label(RichText::new("No location selected").weak());

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        egui::ComboBox::from_id_salt("theme-preference")
                            .selected_text(theme_name(self.theme_preference))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.theme_preference,
                                    ThemePreference::System,
                                    "System theme",
                                );
                                ui.selectable_value(
                                    &mut self.theme_preference,
                                    ThemePreference::Dark,
                                    "Dark theme",
                                );
                                ui.selectable_value(
                                    &mut self.theme_preference,
                                    ThemePreference::Light,
                                    "Light theme",
                                );
                            });
                        ui.label(RichText::new("Theme").weak());
                    });
                });
            },
        );
        ui.ctx().set_theme(self.theme_preference);
    }

    fn telemetry_rail(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("telemetry-rail").exact_size(44.0).show_separator_line(true).show(
            ui,
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(8.0);
                    status_pill(ui, "READY", theme::SCAN_CURRENT);
                    ui.separator();
                    metric_value(ui, "Selected", "—");
                    metric_value(ui, "Files", "0");
                    metric_value(ui, "Folders", "0");
                    ui.separator();
                    ui.selectable_value(&mut self.metric, SizeMetric::Logical, "Logical size");
                    ui.selectable_value(&mut self.metric, SizeMetric::Allocated, "Allocated data");
                });
            },
        );
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.heading("Largest items");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new("0 items").weak().monospace());
            });
        });
        ui.label(
            RichText::new("A synchronized keyboard-friendly list will appear as results arrive.")
                .weak(),
        );
        ui.add_space(12.0);
        ui.separator();

        for (label, value) in [
            ("Current path", "No selection"),
            ("Logical size", "—"),
            ("Allocated data", "—"),
            ("File count", "0"),
            ("Scan state", "Not started"),
        ] {
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).weak());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(value).monospace());
                });
            });
        }

        ui.add_space(16.0);
        ui.separator();
        ui.label(RichText::new("Safety").strong());
        ui.label(
            RichText::new("Filesystem actions stay unavailable until an exact path is selected.")
                .weak(),
        );
        ui.horizontal(|ui| {
            ui.add_enabled(false, egui::Button::new("Open"));
            ui.add_enabled(false, egui::Button::new("Recycle"));
            ui.add_enabled(false, egui::Button::new("Delete permanently"));
        });
    }

    fn radial_lens(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size();
        let diameter = available.x.min(available.y - 56.0).clamp(220.0, 680.0);
        ui.vertical_centered(|ui| {
            let (response, painter) = ui.allocate_painter(Vec2::splat(diameter), Sense::click());
            response.clone().on_hover_text("The disk map will grow here while scanning");

            let center = response.rect.center();
            let radius = diameter * 0.43;
            let active_theme = ui.ctx().theme();
            let bed = if active_theme == egui::Theme::Dark {
                Color32::from_rgb(26, 30, 36)
            } else {
                Color32::from_rgb(229, 235, 239)
            };
            let quiet = if active_theme == egui::Theme::Dark {
                Color32::from_gray(74)
            } else {
                Color32::from_gray(188)
            };

            painter.circle_filled(center, radius, bed);
            for ring in [0.40_f32, 0.62, 0.82, 1.0] {
                painter.circle_stroke(center, radius * ring, Stroke::new(1.0, quiet));
            }
            for (start, end, radius_factor, color, width) in [
                (-0.20, 1.08, 1.0, theme::SCAN_CURRENT, 8.0),
                (1.22, 2.14, 0.82, theme::SECTOR_EMBER, 7.0),
                (2.32, 3.48, 0.62, theme::SCAN_CURRENT.gamma_multiply(0.72), 6.0),
                (3.62, 4.54, 0.82, theme::PLATTER_MIDNIGHT, 7.0),
            ] {
                painter.add(Shape::line(
                    arc_points(center, radius * radius_factor, start, end, 36),
                    Stroke::new(width, color),
                ));
            }
            painter.circle_filled(center, radius * 0.30, ui.visuals().panel_fill);
            painter.circle_stroke(
                center,
                radius * 0.30,
                Stroke::new(1.5, theme::SCAN_CURRENT.gamma_multiply(0.65)),
            );
            painter.text(
                center + egui::vec2(0.0, -8.0),
                egui::Align2::CENTER_CENTER,
                "Choose a folder",
                FontId::proportional(17.0),
                ui.visuals().strong_text_color(),
            );
            painter.text(
                center + egui::vec2(0.0, 15.0),
                egui::Align2::CENTER_CENTER,
                "or drive",
                FontId::proportional(13.0),
                ui.visuals().weak_text_color(),
            );

            if response.clicked() {
                self.notice = "Folder picker is not connected yet";
            }
        });
    }

    fn workspace(&mut self, ui: &mut egui::Ui) {
        let wide = ui.available_width() >= 900.0;
        if wide {
            egui::Panel::right("inspection-rail")
                .resizable(true)
                .default_size(360.0)
                .min_size(300.0)
                .max_size(440.0)
                .show_separator_line(true)
                .show(ui, |ui| self.inspector(ui));
            egui::CentralPanel::default().show(ui, |ui| self.radial_lens(ui));
        } else {
            egui::CentralPanel::default().show(ui, |ui| {
                self.radial_lens(ui);
                ui.separator();
                self.inspector(ui);
            });
        }
    }

    fn status_rail(&self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status-rail").exact_size(34.0).show_separator_line(true).show(
            ui,
            |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new(self.notice).weak());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new("Standard user · No elevation").weak());
                    });
                });
            },
        );
    }
}

impl eframe::App for DiskPieShell {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.command_rail(ui);
        self.telemetry_rail(ui);
        self.status_rail(ui);
        self.workspace(ui);
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }
}

fn status_pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    let text = RichText::new(text).strong().monospace().color(color);
    ui.label(text);
}

fn metric_value(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).weak());
        ui.label(RichText::new(value).monospace().strong());
    });
}

fn theme_name(preference: ThemePreference) -> &'static str {
    match preference {
        ThemePreference::System => "System",
        ThemePreference::Dark => "Dark",
        ThemePreference::Light => "Light",
    }
}

fn arc_points(center: Pos2, radius: f32, start: f32, end: f32, segments: usize) -> Vec<Pos2> {
    let span = (end - start).rem_euclid(TAU);
    let start = start - FRAC_PI_2;
    (0..=segments)
        .map(|index| {
            let angle = start + span * index as f32 / segments as f32;
            center + radius * egui::vec2(angle.cos(), angle.sin())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_includes_requested_endpoints() {
        let points = arc_points(Pos2::ZERO, 10.0, 0.0, FRAC_PI_2, 4);
        assert_eq!(points.len(), 5);
        assert!((points[0].x - 0.0).abs() < 0.001);
        assert!((points[0].y + 10.0).abs() < 0.001);
        assert!((points[4].x - 10.0).abs() < 0.001);
        assert!((points[4].y - 0.0).abs() < 0.001);
    }
}
