//! DiskPie desktop application entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

pub mod cli;
#[cfg(windows)]
mod local_diagnostics;
mod shell;
pub mod storage;
pub mod sunburst_view;
mod theme;

use std::{path::PathBuf, process::ExitCode, sync::Arc};

use eframe::egui;

use diskpie_app::settings::SettingsSession;

#[cfg(windows)]
use diskpie_app::diagnostics::{
    AppVersion, Architecture, DiagnosticCode, DiagnosticEvent, DiagnosticField, DiagnosticLevel,
    OperatingSystem, Target,
};
#[cfg(windows)]
use diskpie_platform::{ProjectPathFacility, ProjectPaths, resolve_project_paths};
#[cfg(windows)]
use local_diagnostics::{InitialRetentionOutcome, LocalDiagnostics};

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

#[cfg(windows)]
fn run(startup: cli::StartupRequest) -> eframe::Result {
    let mut services = WindowsStartupServices::prepare();
    if let Some(diagnostics) = services.diagnostics.as_ref() {
        diagnostics.record(application_event(DiagnosticCode::AppStarted));
        if services.optional_services_unavailable {
            diagnostics.record(DiagnosticEvent::new(DiagnosticCode::ProjectPathsUnavailable));
        }
    }
    let result =
        run_ui(startup, services.settings_path.take(), services.optional_services_unavailable);
    if let Some(diagnostics) = services.diagnostics.take() {
        diagnostics.record(application_event(DiagnosticCode::AppStopped));
        let _final_counters = diagnostics.finish();
    }
    result
}

#[cfg(not(windows))]
fn run(startup: cli::StartupRequest) -> eframe::Result {
    run_ui(startup, None, true)
}

fn run_ui(
    startup: cli::StartupRequest,
    settings_path: Option<PathBuf>,
    optional_services_unavailable: bool,
) -> eframe::Result {
    // The native file adapter is deliberately the only component allowed to
    // turn this explicit path into bytes. Until that adapter has produced a
    // bounded source document, the policy layer remains unavailable and must
    // not fall back to eframe's platform storage.
    let unavailable_code = if settings_path.is_some() {
        "settings.native.not_connected"
    } else {
        "settings.storage.unavailable"
    };
    let mut settings_store = storage::SettingsDocumentStore::unavailable(unavailable_code);
    let settings = SettingsSession::load(&mut settings_store);
    let settings_exit = shell::SettingsExit::default();
    let settings_exit_for_ui = settings_exit.clone();

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
        // ADR 0019: eframe native persistence is compile-time disabled. The
        // application-owned settings adapter receives the explicit path and
        // performs no I/O from an egui callback.
        persist_window: false,
        persistence_path: None,
        ..Default::default()
    };

    let result = eframe::run_native(
        diskpie_app::PRODUCT_NAME,
        options,
        Box::new(move |creation| {
            Ok(Box::new(shell::DiskPieShell::new(
                creation,
                startup.into_initial_path(),
                optional_services_unavailable,
                settings,
                settings_exit_for_ui,
            )?))
        }),
    );

    // The shell transfers policy state only after the native event loop has
    // released it. Publication is intentionally deferred to the native file
    // adapter, so no filesystem operation can occur in an egui callback.
    if let Some(mut settings) = settings_exit.take() {
        let _staged_result = settings.save(&mut settings_store);
        let _staged_document = settings_store.take_pending();
    }
    result
}

#[cfg(windows)]
struct WindowsStartupServices {
    settings_path: Option<PathBuf>,
    diagnostics: Option<LocalDiagnostics>,
    optional_services_unavailable: bool,
}

#[cfg(windows)]
impl WindowsStartupServices {
    fn prepare() -> Self {
        let paths = resolve_project_paths();
        let mut unavailable = !paths.issues.is_empty();

        let settings_path = prepare_settings(&paths, &mut unavailable);
        let diagnostics = prepare_diagnostics(&paths, &mut unavailable);
        if paths.create_directory(ProjectPathFacility::Crashes).is_err() {
            unavailable = true;
        }
        if diagnostics.as_ref().is_some_and(|diagnostics| {
            diagnostics.initial_retention_outcome() != InitialRetentionOutcome::Succeeded
        }) {
            unavailable = true;
        }

        Self { settings_path, diagnostics, optional_services_unavailable: unavailable }
    }
}

#[cfg(windows)]
fn prepare_settings(paths: &ProjectPaths, unavailable: &mut bool) -> Option<PathBuf> {
    if paths.create_directory(ProjectPathFacility::Settings).is_err() {
        *unavailable = true;
        return None;
    }
    paths.settings_file.clone()
}

#[cfg(windows)]
fn prepare_diagnostics(paths: &ProjectPaths, unavailable: &mut bool) -> Option<LocalDiagnostics> {
    if paths.create_directory(ProjectPathFacility::Logs).is_err() {
        *unavailable = true;
        return None;
    }
    let Some(directory) = paths.logs_directory.as_deref() else {
        *unavailable = true;
        return None;
    };
    match LocalDiagnostics::initialize(directory, DiagnosticLevel::Info) {
        Ok(diagnostics) => Some(diagnostics),
        Err(_error) => {
            *unavailable = true;
            None
        }
    }
}

#[cfg(windows)]
fn application_event(code: DiagnosticCode) -> DiagnosticEvent {
    let mut event = DiagnosticEvent::new(code);
    let version = AppVersion::new(
        env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0),
        env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0),
        env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0),
    );
    let architecture = if cfg!(target_arch = "x86_64") {
        Architecture::X86_64
    } else if cfg!(target_arch = "x86") {
        Architecture::X86
    } else if cfg!(target_arch = "aarch64") {
        Architecture::Aarch64
    } else if cfg!(target_arch = "arm") {
        Architecture::Arm
    } else {
        Architecture::Other
    };
    let _version = event.push_field(DiagnosticField::AppVersion(version));
    let _target = event
        .push_field(DiagnosticField::Target(Target::new(OperatingSystem::Windows, architecture)));
    event
}
