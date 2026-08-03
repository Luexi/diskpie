//! DiskPie desktop application entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

pub mod cli;
mod shell;
pub mod storage;
pub mod sunburst_view;
mod theme;

use std::{process::ExitCode, sync::Arc};

use eframe::egui;

fn main() -> ExitCode {
    let startup = match cli::parse_env() {
        Ok(cli::StartupAction::Run(request)) => request,
        Ok(cli::StartupAction::Help) => {
            println!("{}", cli::HELP_TEXT);
            return ExitCode::SUCCESS;
        }
        Ok(cli::StartupAction::Version) => {
            println!("{}", cli::VERSION_TEXT);
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("diskpie: {error}\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };

    match run(startup) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("diskpie: application startup failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(startup: cli::StartupRequest) -> eframe::Result {
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
        Box::new(move |creation| {
            Ok(Box::new(shell::DiskPieShell::new(creation, startup.into_initial_path())?))
        }),
    )
}
