//! DiskPie desktop application entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

pub mod cli;
mod diagnostic_sink;
mod fonts;
#[cfg(windows)]
mod local_diagnostics;
mod resolver;
mod scan_ui;
mod shell;
pub mod storage;
pub mod sunburst_view;
mod theme;

use std::{
    process::ExitCode,
    sync::Arc,
    time::{Duration, Instant},
};

use eframe::egui;

use diagnostic_sink::DiagnosticBridge;
use diskpie_app::{runtime::RuntimeController, settings::SettingsSession};
use resolver::ResolverWorker;
use shell::{ExitHandoff, ShellExit, UiServices};
use storage::SettingsDocumentStore;

#[cfg(windows)]
use diskpie_app::{
    diagnostics::{
        AppVersion, Architecture, DiagnosticCode, DiagnosticEvent, DiagnosticField,
        DiagnosticLevel, ErrorClass, OperatingSystem, Outcome, Target,
    },
    settings::{SettingsRead, SettingsSaveOutcome, SettingsWarning},
};
#[cfg(windows)]
use diskpie_platform::{
    ProjectPathFacility, ProjectPaths, SettingsCommitOutcome, SettingsFileErrorKind,
    SettingsFileRead, ShellService, ShellServiceFinish, WindowsSettingsFile, resolve_project_paths,
};
#[cfg(windows)]
use local_diagnostics::{InitialRetentionOutcome, LocalDiagnostics};

/// Bounded wait for each UI-owned service after the event loop has returned.
const SERVICE_FINISH_TIMEOUT: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    let startup = match cli::parse_env() {
        Ok(cli::StartupAction::Run(request)) => request,
        Ok(cli::StartupAction::Help) => {
            emit_console_line(cli::HELP_TEXT);
            return ExitCode::SUCCESS;
        }
        Ok(cli::StartupAction::Version) => {
            emit_console_line(cli::VERSION_TEXT);
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            emit_console_error(&format!("diskpie: {error}\n{}", cli::USAGE));
            return ExitCode::from(2);
        }
    };

    match run(startup) {
        Ok(_receipts) => ExitCode::SUCCESS,
        Err(error) => {
            report_startup_failure(&format!("diskpie: application startup failed: {error}"));
            ExitCode::FAILURE
        }
    }
}

/// Prints CLI protocol text. Release builds are GUI-subsystem processes, so
/// the parent console is attached first; a pipe or file the parent redirected
/// is used as-is.
fn emit_console_line(text: &str) {
    #[cfg(windows)]
    let _console = diskpie_platform::attach_parent_console();
    println!("{text}");
}

fn emit_console_error(text: &str) {
    #[cfg(windows)]
    let _console = diskpie_platform::attach_parent_console();
    eprintln!("{text}");
}

/// A fatal startup failure must never disappear silently: it goes to the
/// parent console when one exists and to a native message box otherwise.
fn report_startup_failure(text: &str) {
    emit_console_error(text);
    #[cfg(windows)]
    diskpie_platform::show_startup_failure(text);
}

/// Typed receipts of the ordered shutdown performed after the event loop.
///
/// The panic-marker workstream will feed these into its shutdown-quiescence
/// proof: a clean-shutdown marker may only be produced once every owned
/// provider and worker has actually joined. This module reports the facts
/// and deliberately does not implement that proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShutdownReceipts {
    /// `RuntimeController::is_shutdown_complete` observed before disposal.
    /// It does not promise that a blocked filesystem call has returned.
    pub runtime_shutdown_complete: bool,
    /// Outcome of `ShellService::finish`, when a Shell STA had been started.
    #[cfg(windows)]
    pub shell_service: Option<ShellServiceFinish>,
    /// Whether the resolver worker thread was joined within its deadline.
    pub resolver_joined: bool,
    /// Whether the diagnostic bridge thread was joined within its deadline.
    pub diagnostic_bridge_joined: bool,
    /// Events the UI dropped because the bounded diagnostic queue was full.
    pub diagnostic_events_dropped: u64,
}

#[cfg(windows)]
fn run(startup: cli::StartupRequest) -> eframe::Result<ShutdownReceipts> {
    let mut services = WindowsStartupServices::prepare();
    let settings = SettingsSession::load(&mut services.settings_store);
    services.start_diagnostics(&settings);
    let prepared = services.prepare_ui_services(&settings)?;

    // The panic-marker workstream will supply the real value here.
    let previous_session_panicked = false;
    let (result, exit) = run_ui(
        startup,
        settings,
        services.optional_services_unavailable,
        previous_session_panicked,
        prepared.services,
    );

    let (receipts, session) = finish_ui_services(exit, prepared.resolver, prepared.bridge);
    services.publish_settings(session);
    services.finish();
    result.map(|()| receipts)
}

#[cfg(not(windows))]
fn run(startup: cli::StartupRequest) -> eframe::Result<ShutdownReceipts> {
    // ADR 0019 item 10: other desktop targets stay explicitly nonpersistent
    // until they implement an equivalent adapter.
    let mut store = SettingsDocumentStore::unavailable("settings.storage.unavailable");
    let settings = SettingsSession::load(&mut store);
    let runtime = RuntimeController::new(shell::runtime_config_from_settings(settings.settings()))
        .map_err(app_creation_error)?;
    let (resolver_client, resolver) =
        resolver::start(resolver::UnsupportedBackend).map_err(app_creation_error)?;
    let bridge = DiagnosticBridge::start(|_event| {}).map_err(app_creation_error)?;
    let services = UiServices {
        runtime,
        picker: None,
        resolver: resolver_client,
        diagnostics: bridge.sink(),
        fonts: fonts::SystemFonts::default(),
    };
    let (result, exit) = run_ui(startup, settings, true, false, services);
    let (receipts, _session) = finish_ui_services(exit, resolver, bridge);
    result.map(|()| receipts)
}

fn app_creation_error(error: impl std::error::Error + Send + Sync + 'static) -> eframe::Error {
    eframe::Error::AppCreation(Box::new(error))
}

/// Services created before the window and finished after the event loop.
struct PreparedUiServices {
    services: UiServices,
    resolver: ResolverWorker,
    bridge: DiagnosticBridge,
}

/// Ordered, bounded finish of everything the shell handed back.
///
/// The shell already requested runtime and Shell shutdown from inside the
/// event loop; this only waits within documented deadlines and never from a
/// callback. Order: runtime disposal, Shell STA finish, resolver join, then
/// the diagnostic bridge join. Settings publication and diagnostics finish
/// follow in the caller.
fn finish_ui_services(
    exit: Option<ShellExit>,
    resolver: ResolverWorker,
    bridge: DiagnosticBridge,
) -> (ShutdownReceipts, Option<SettingsSession>) {
    let (session, runtime, picker) = match exit {
        Some(exit) => (Some(exit.settings), exit.runtime, exit.picker),
        None => (None, None, None),
    };

    let runtime_shutdown_complete = runtime.as_ref().is_some_and(await_runtime_shutdown);
    // Disposal is bounded by the runtime's own contract: ownership of any
    // provider that has not returned transfers to the bounded reaper.
    drop(runtime);

    #[cfg(windows)]
    let shell_service = picker.map(|picker| picker.finish(SERVICE_FINISH_TIMEOUT));
    #[cfg(not(windows))]
    drop(picker);

    let resolver_joined = resolver.finish(SERVICE_FINISH_TIMEOUT);
    let diagnostic_events_dropped = bridge.dropped_events();
    let diagnostic_bridge_joined = bridge.finish(SERVICE_FINISH_TIMEOUT);

    (
        ShutdownReceipts {
            runtime_shutdown_complete,
            #[cfg(windows)]
            shell_service,
            resolver_joined,
            diagnostic_bridge_joined,
            diagnostic_events_dropped,
        },
        session,
    )
}

/// Polls the runtime's shutdown handoff within the service deadline.
fn await_runtime_shutdown(runtime: &RuntimeController) -> bool {
    let deadline = Instant::now() + SERVICE_FINISH_TIMEOUT;
    loop {
        if runtime.is_shutdown_complete() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn run_ui(
    startup: cli::StartupRequest,
    settings: SettingsSession,
    optional_services_unavailable: bool,
    previous_session_panicked: bool,
    services: UiServices,
) -> (eframe::Result, Option<ShellExit>) {
    let exit = ExitHandoff::default();
    let exit_for_ui = exit.clone();

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
        // application-owned settings adapter performs its I/O before and after
        // the native event loop, never from an egui callback.
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
                previous_session_panicked,
                settings,
                services,
                exit_for_ui,
            )?))
        }),
    );

    // The shell transfers policy state and service ownership only after the
    // native event loop has released it, so nothing below overlaps a callback.
    (result, exit.take())
}

/// Native services owned by the composition root for the process lifetime.
#[cfg(windows)]
struct WindowsStartupServices {
    paths: ProjectPaths,
    settings_file: Option<WindowsSettingsFile>,
    settings_unavailable: Option<ErrorClass>,
    settings_store: SettingsDocumentStore,
    diagnostics: Option<LocalDiagnostics>,
    optional_services_unavailable: bool,
}

#[cfg(windows)]
impl WindowsStartupServices {
    /// Resolves project paths and reads the bounded settings document before
    /// any window exists (ADR 0019 items 3 and 4).
    fn prepare() -> Self {
        let paths = resolve_project_paths();
        let mut unavailable = !paths.issues.is_empty();

        let (settings_file, settings_unavailable) = match prepare_settings_file(&paths) {
            Ok(file) => (Some(file), None),
            Err(class) => {
                unavailable = true;
                (None, Some(class))
            }
        };
        let initial_read = match settings_file.as_ref() {
            Some(file) => settings_read_from_native(file.read()),
            None => SettingsRead::Unavailable { code: "settings.storage.unavailable" },
        };

        Self {
            paths,
            settings_file,
            settings_unavailable,
            settings_store: SettingsDocumentStore::from_read(initial_read),
            diagnostics: None,
            optional_services_unavailable: unavailable,
        }
    }

    /// Starts local logging at the level the loaded settings request and
    /// records the startup facts that are already known.
    fn start_diagnostics(&mut self, settings: &SettingsSession) {
        let level = DiagnosticLevel::from(settings.settings().diagnostic_level);
        self.diagnostics =
            prepare_diagnostics(&self.paths, level, &mut self.optional_services_unavailable);
        if self.paths.create_directory(ProjectPathFacility::Crashes).is_err() {
            self.optional_services_unavailable = true;
        }
        if self.diagnostics.as_ref().is_some_and(|diagnostics| {
            diagnostics.initial_retention_outcome() != InitialRetentionOutcome::Succeeded
        }) {
            self.optional_services_unavailable = true;
        }

        let Some(diagnostics) = self.diagnostics.as_ref() else {
            return;
        };
        diagnostics.record(application_event(DiagnosticCode::AppStarted));
        if !self.paths.issues.is_empty() {
            diagnostics.record(DiagnosticEvent::new(DiagnosticCode::ProjectPathsUnavailable));
        }
        if let Some(class) = self.settings_unavailable {
            diagnostics.record(classified_event(DiagnosticCode::SettingsUnavailable, class));
        }
        if let Some(event) = settings_recovery_event(settings.warning()) {
            diagnostics.record(event);
        }
    }

    /// Creates the runtime, the Shell STA, the resolver worker, the diagnostic
    /// bridge, and the optional system fonts before the native window exists.
    ///
    /// A missing Shell STA only disables the native folder picker; the rest of
    /// the application, including drive selection, keeps working.
    fn prepare_ui_services(
        &mut self,
        settings: &SettingsSession,
    ) -> eframe::Result<PreparedUiServices> {
        let runtime =
            RuntimeController::new(shell::runtime_config_from_settings(settings.settings()))
                .map_err(app_creation_error)?;
        let picker = match ShellService::start() {
            Ok(service) => Some(service),
            Err(_error) => {
                self.optional_services_unavailable = true;
                self.record(classified_event(
                    DiagnosticCode::ShellActionFailed,
                    ErrorClass::Unsupported,
                ));
                None
            }
        };
        let (resolver_client, resolver) =
            resolver::start(resolver::WindowsBackend).map_err(app_creation_error)?;
        // The bridge thread records through the installed subscriber; the
        // `LocalDiagnostics` owner stays with the root for the final flush.
        let bridge = if self.diagnostics.is_some() {
            DiagnosticBridge::start(local_diagnostics::record_event)
        } else {
            DiagnosticBridge::start(|_event| {})
        }
        .map_err(app_creation_error)?;
        let fonts = fonts::load_windows_fallbacks();
        Ok(PreparedUiServices {
            services: UiServices {
                runtime,
                picker,
                resolver: resolver_client,
                diagnostics: bridge.sink(),
                fonts,
            },
            resolver,
            bridge,
        })
    }

    /// Publishes the settings session handed back by the shell through the
    /// retained-handle adapter (ADR 0019 item 8). Runs after the event loop.
    fn publish_settings(&mut self, session: Option<SettingsSession>) {
        let Some(mut session) = session else {
            return;
        };
        let pending = match session.save(&mut self.settings_store) {
            Ok(SettingsSaveOutcome::Saved) => self.settings_store.take_pending(),
            Ok(SettingsSaveOutcome::NotNeeded) => None,
            Err(_encode_error) => {
                self.record(classified_event(
                    DiagnosticCode::SettingsPublishFailed,
                    ErrorClass::InvalidData,
                ));
                None
            }
        };
        let (Some(bytes), Some(file)) = (pending, self.settings_file.as_mut()) else {
            return;
        };
        let event = match file.commit(&bytes) {
            Ok(SettingsCommitOutcome::Committed { .. }) => {
                outcome_event(DiagnosticCode::SettingsPublished, Outcome::Succeeded)
            }
            Ok(SettingsCommitOutcome::CommittedButUnverified) => {
                outcome_event(DiagnosticCode::SettingsPublished, Outcome::Degraded)
            }
            Err(error) => classified_event(
                DiagnosticCode::SettingsPublishFailed,
                error_class_from_settings_file(error.kind),
            ),
        };
        self.record(event);
    }

    /// Reports the bounded logging health observed during the run, then
    /// flushes and joins the diagnostics worker outside any UI callback.
    /// The diagnostic bridge has been finished by the caller before this.
    fn finish(self) {
        let Some(diagnostics) = self.diagnostics else {
            return;
        };
        let counters = diagnostics.counters();
        if counters.dropped_events > 0 {
            diagnostics.record(count_event(
                DiagnosticCode::DiagnosticQueueDropped,
                counters.dropped_events,
            ));
        }
        if counters.write_failures > 0 {
            diagnostics.record(count_event(
                DiagnosticCode::DiagnosticWriteFailed,
                counters.write_failures,
            ));
        }
        diagnostics.record(application_event(DiagnosticCode::AppStopped));
        let _final_outcome = diagnostics.finish();
    }

    fn record(&self, event: DiagnosticEvent) {
        if let Some(diagnostics) = self.diagnostics.as_ref() {
            diagnostics.record(event);
        }
    }
}

#[cfg(windows)]
fn prepare_settings_file(paths: &ProjectPaths) -> Result<WindowsSettingsFile, ErrorClass> {
    if paths.create_directory(ProjectPathFacility::Settings).is_err() {
        return Err(ErrorClass::NotFound);
    }
    let directory = paths.directory(ProjectPathFacility::Settings).ok_or(ErrorClass::NotFound)?;
    WindowsSettingsFile::prepare(directory)
        .map_err(|error| error_class_from_settings_file(error.kind))
}

#[cfg(windows)]
fn settings_read_from_native(read: SettingsFileRead) -> SettingsRead {
    match read {
        SettingsFileRead::Missing => SettingsRead::Missing,
        SettingsFileRead::Document { bytes } => storage::decode_document(&bytes),
        SettingsFileRead::Oversized { bytes } => SettingsRead::Oversized { bytes },
        SettingsFileRead::Unavailable { code } => SettingsRead::Unavailable { code },
    }
}

#[cfg(windows)]
fn settings_recovery_event(warning: Option<SettingsWarning>) -> Option<DiagnosticEvent> {
    let class = match warning? {
        SettingsWarning::Malformed => ErrorClass::InvalidData,
        SettingsWarning::UnsupportedVersion { .. } => ErrorClass::Unsupported,
        SettingsWarning::Oversized { .. } => ErrorClass::ResourceExhausted,
        // Unavailability is reported separately with its native error class.
        SettingsWarning::Unavailable { .. } => return None,
    };
    Some(classified_event(DiagnosticCode::SettingsRecovered, class))
}

#[cfg(windows)]
const fn error_class_from_settings_file(kind: SettingsFileErrorKind) -> ErrorClass {
    match kind {
        SettingsFileErrorKind::AccessDenied => ErrorClass::AccessDenied,
        SettingsFileErrorKind::NotFound => ErrorClass::NotFound,
        SettingsFileErrorKind::InvalidDestination
        | SettingsFileErrorKind::ReparsePoint
        | SettingsFileErrorKind::OwnedDestination
        | SettingsFileErrorKind::InvalidContent => ErrorClass::InvalidData,
        SettingsFileErrorKind::TooLarge => ErrorClass::ResourceExhausted,
        SettingsFileErrorKind::AlreadyExists => ErrorClass::Busy,
        SettingsFileErrorKind::Unsupported => ErrorClass::Unsupported,
        SettingsFileErrorKind::PartialReplacement | SettingsFileErrorKind::Io => ErrorClass::Io,
    }
}

#[cfg(windows)]
fn prepare_diagnostics(
    paths: &ProjectPaths,
    level: DiagnosticLevel,
    unavailable: &mut bool,
) -> Option<LocalDiagnostics> {
    if paths.create_directory(ProjectPathFacility::Logs).is_err() {
        *unavailable = true;
        return None;
    }
    let Some(directory) = paths.logs_directory.as_deref() else {
        *unavailable = true;
        return None;
    };
    match LocalDiagnostics::initialize(directory, level) {
        Ok(diagnostics) => Some(diagnostics),
        Err(_error) => {
            *unavailable = true;
            None
        }
    }
}

#[cfg(windows)]
fn classified_event(code: DiagnosticCode, class: ErrorClass) -> DiagnosticEvent {
    let mut event = DiagnosticEvent::new(code);
    let _field = event.push_field(DiagnosticField::ErrorClass(class));
    event
}

#[cfg(windows)]
fn outcome_event(code: DiagnosticCode, outcome: Outcome) -> DiagnosticEvent {
    let mut event = DiagnosticEvent::new(code);
    let _field = event.push_field(DiagnosticField::Outcome(outcome));
    event
}

#[cfg(windows)]
fn count_event(code: DiagnosticCode, count: u64) -> DiagnosticEvent {
    let mut event = DiagnosticEvent::new(code);
    let _field = event.push_field(DiagnosticField::ItemCount(count));
    event
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
