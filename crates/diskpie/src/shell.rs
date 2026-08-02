//! Responsive native shell for the DiskPie application.

use std::f32::consts::{FRAC_PI_2, TAU};

use eframe::egui::{
    self, Align, Color32, FontId, Layout, Pos2, RichText, Sense, Shape, Stroke, ThemePreference,
    Vec2,
};

use diskpie_app::i18n::{I18n, I18nError, Locale, MessageId};

use crate::theme;

/// Preformatted static messages keep Fluent parsing and allocation off the frame loop.
struct UiStrings {
    values: Box<[String]>,
}

impl UiStrings {
    fn new(i18n: &I18n) -> Self {
        let values = MessageId::ALL
            .into_iter()
            .map(|id| {
                i18n.text(id)
                    .map(std::borrow::Cow::into_owned)
                    .unwrap_or_else(|_| id.key().to_owned())
            })
            .collect();
        Self { values }
    }

    fn get(&self, id: MessageId) -> &str {
        self.values.get(id.index()).map_or_else(|| id.key(), String::as_str)
    }
}

pub struct DiskPieShell {
    i18n: I18n,
    strings: UiStrings,
    locale: Locale,
    theme_preference: ThemePreference,
    metric: SizeMetric,
    notice: MessageId,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SizeMetric {
    Logical,
    Allocated,
}

impl DiskPieShell {
    pub fn new(creation: &eframe::CreationContext<'_>) -> Result<Self, I18nError> {
        theme::install(&creation.egui_ctx);
        creation.egui_ctx.set_theme(ThemePreference::System);
        let locale = Locale::EnglishUnitedStates;
        let i18n = I18n::new(locale)?;
        let strings = UiStrings::new(&i18n);
        Ok(Self {
            i18n,
            strings,
            locale,
            theme_preference: ThemePreference::System,
            metric: SizeMetric::Allocated,
            notice: MessageId::StatusReady,
        })
    }

    fn select_locale(&mut self, locale: Locale) {
        if locale == self.locale {
            return;
        }
        if let Ok(i18n) = I18n::new(locale) {
            self.strings = UiStrings::new(&i18n);
            self.i18n = i18n;
            self.locale = locale;
        }
    }

    fn command_rail(&mut self, ui: &mut egui::Ui) {
        let strings = &self.strings;
        let mut selected_locale = self.locale;
        let theme_preference = &mut self.theme_preference;
        let notice = &mut self.notice;

        egui::Panel::top("command-rail").exact_size(52.0).show_separator_line(true).show(
            ui,
            |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(strings.get(MessageId::AppName))
                            .strong()
                            .size(18.0)
                            .color(theme::SCAN_CURRENT),
                    );
                    ui.separator();
                    ui.add_enabled(false, egui::Button::new(strings.get(MessageId::Back)))
                        .on_disabled_hover_text(strings.get(MessageId::NoBackHistory));
                    ui.add_enabled(false, egui::Button::new(strings.get(MessageId::Parent)))
                        .on_disabled_hover_text(strings.get(MessageId::NoParent));
                    if ui.button(strings.get(MessageId::ChooseFolder)).clicked() {
                        *notice = MessageId::StatusReady;
                    }
                    if ui.available_width() > 620.0 {
                        ui.label(RichText::new(strings.get(MessageId::NoLocation)).weak());
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        egui::ComboBox::from_id_salt("theme-preference")
                            .selected_text(theme_name(*theme_preference, strings))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    theme_preference,
                                    ThemePreference::System,
                                    strings.get(MessageId::ThemeSystem),
                                );
                                ui.selectable_value(
                                    theme_preference,
                                    ThemePreference::Dark,
                                    strings.get(MessageId::ThemeDark),
                                );
                                ui.selectable_value(
                                    theme_preference,
                                    ThemePreference::Light,
                                    strings.get(MessageId::ThemeLight),
                                );
                            });
                        if ui.available_width() > 820.0 {
                            ui.label(RichText::new(strings.get(MessageId::Theme)).weak());
                        }
                        egui::ComboBox::from_id_salt("language")
                            .selected_text(selected_locale.native_name())
                            .show_ui(ui, |ui| {
                                for locale in Locale::ALL {
                                    ui.selectable_value(
                                        &mut selected_locale,
                                        locale,
                                        locale.native_name(),
                                    );
                                }
                            });
                        if ui.available_width() > 820.0 {
                            ui.label(RichText::new(strings.get(MessageId::Language)).weak());
                        }
                    });
                });
            },
        );

        ui.ctx().set_theme(*theme_preference);
        self.select_locale(selected_locale);
    }

    fn telemetry_rail(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("telemetry-rail").exact_size(44.0).show_separator_line(true).show(
            ui,
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(8.0);
                    status_pill(ui, self.strings.get(MessageId::Ready), theme::SCAN_CURRENT);
                    ui.separator();
                    metric_value(ui, self.strings.get(MessageId::Selected), "—");
                    metric_value(ui, self.strings.get(MessageId::Files), "0");
                    metric_value(ui, self.strings.get(MessageId::Folders), "0");
                    ui.separator();
                    ui.selectable_value(
                        &mut self.metric,
                        SizeMetric::Logical,
                        self.strings.get(MessageId::LogicalSize),
                    );
                    ui.selectable_value(
                        &mut self.metric,
                        SizeMetric::Allocated,
                        self.strings.get(MessageId::AllocatedData),
                    );
                });
            },
        );
    }

    fn inspector(&self, ui: &mut egui::Ui) {
        let item_count = self.item_count(0);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.heading(self.strings.get(MessageId::LargestItems));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(item_count).weak().monospace());
            });
        });
        ui.label(RichText::new(self.strings.get(MessageId::EmptyItemsHint)).weak());
        ui.add_space(12.0);
        ui.separator();

        for (label, value) in [
            (self.strings.get(MessageId::CurrentPath), self.strings.get(MessageId::NoSelection)),
            (self.strings.get(MessageId::LogicalSize), "—"),
            (self.strings.get(MessageId::AllocatedData), "—"),
            (self.strings.get(MessageId::FileCount), "0"),
            (self.strings.get(MessageId::ScanState), self.strings.get(MessageId::NotStarted)),
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
        ui.label(RichText::new(self.strings.get(MessageId::Safety)).strong());
        ui.label(RichText::new(self.strings.get(MessageId::ActionsUnavailable)).weak());
        ui.horizontal(|ui| {
            ui.add_enabled(false, egui::Button::new(self.strings.get(MessageId::Open)));
            ui.add_enabled(false, egui::Button::new(self.strings.get(MessageId::Recycle)));
            ui.add_enabled(
                false,
                egui::Button::new(self.strings.get(MessageId::DeletePermanently)),
            );
        });
    }

    fn item_count(&self, count: u64) -> String {
        self.i18n.item_count(count).unwrap_or_else(|_| count.to_string())
    }

    fn radial_lens(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size();
        let diameter = available.x.min(available.y - 56.0).clamp(220.0, 680.0);
        ui.vertical_centered(|ui| {
            let (response, painter) = ui.allocate_painter(Vec2::splat(diameter), Sense::click());
            response.clone().on_hover_text(self.strings.get(MessageId::ChartEmptyTooltip));

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
                self.strings.get(MessageId::ChooseFolderCenter),
                FontId::proportional(17.0),
                ui.visuals().strong_text_color(),
            );
            painter.text(
                center + egui::vec2(0.0, 15.0),
                egui::Align2::CENTER_CENTER,
                self.strings.get(MessageId::OrDrive),
                FontId::proportional(13.0),
                ui.visuals().weak_text_color(),
            );

            if response.clicked() {
                self.notice = MessageId::StatusReady;
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
                    ui.label(RichText::new(self.strings.get(self.notice)).weak());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(self.strings.get(MessageId::NoElevation)).weak());
                        ui.label(RichText::new("·").weak());
                        ui.label(RichText::new(self.strings.get(MessageId::StandardUser)).weak());
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

fn theme_name(preference: ThemePreference, strings: &UiStrings) -> &str {
    match preference {
        ThemePreference::System => strings.get(MessageId::ThemeSystem),
        ThemePreference::Dark => strings.get(MessageId::ThemeDark),
        ThemePreference::Light => strings.get(MessageId::ThemeLight),
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

    #[test]
    fn localized_cache_uses_the_typed_message_order() {
        let i18n = I18n::new(Locale::SpanishMexico).expect("valid embedded locale");
        let strings = UiStrings::new(&i18n);
        assert_eq!(strings.get(MessageId::Theme), "Tema");
        assert_eq!(strings.get(MessageId::Safety), "Seguridad");
    }
}
