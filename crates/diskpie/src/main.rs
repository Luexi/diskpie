//! DiskPie desktop application entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

mod shell;
mod sunburst_view;
mod theme;

use std::sync::Arc;

use eframe::egui;

fn main() -> eframe::Result {
    let icon =
        eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/brand/diskpie-icon.png"))
            .ok()
            .map(Arc::new);

    let mut viewport = egui::ViewportBuilder::default()
        .with_app_id("io.github.luexi.diskpie")
        .with_title(diskpie_app::PRODUCT_NAME)
        .with_inner_size([1_180.0, 760.0])
        .with_min_inner_size([680.0, 520.0])
        .with_resizable(true);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        centered: true,
        persist_window: true,
        ..Default::default()
    };

    eframe::run_native(
        diskpie_app::PRODUCT_NAME,
        options,
        Box::new(|creation| Ok(Box::new(shell::DiskPieShell::new(creation)?))),
    )
}
