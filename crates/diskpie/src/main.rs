//! DiskPie desktop application entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

pub mod cli;
#[cfg(windows)]
mod local_diagnostics;
pub mod panic_marker;
mod shell;
pub mod storage;
pub mod sunburst_view;
mod theme;

use std::{process::ExitCode, sync::Arc};

use eframe::egui;

use diskpie_app::settings::SettingsSession;
use panic_marker::{CrashMarkerError, FinishCleanOutcome};
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
    SettingsFileRead, WindowsSettingsFile, resolve_project_paths,
};
#[cfg(windows)]
use local_diagnostics::{InitialRetentionOutcome, LocalDiagnostics};
#[cfg(windows)]
use panic_marker::{
    CrashMarkerErrorKind, CrashRuntime, RuntimeQuiescenceReceipt, ShutdownQuiescenceProof,
};

/// What happened to this session's crash marker at exit.
///
/// Diagnostics are already finished when the value is known, so it is not
/// logged; it is returned from `run` for a future explicit export.
#[derive(Debug)]
#[allow(dead_code, reason = "retained for a future diagnostics export")]
enum CrashMarkerFinish {
    /// No process hook was installed in this session.
    NotInstalled,
    /// No quiescence proof could be minted; the runtime was dropped and every
    /// marker state was preserved.
    PreservedWithoutProof,
    /// Clean-session reconciliation ran with a proof.
    Reconciled(Result<FinishCleanOutcome, CrashMarkerError>),
}

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

    let (result, _crash_marker_finish) = run(startup);
    match result {
        Ok(()) => ExitCode::SUCCESS,
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

#[cfg(windows)]
fn run(startup: cli::StartupRequest) -> (eframe::Result, CrashMarkerFinish) {
    let mut services = WindowsStartupServices::prepare();
    let settings = SettingsSession::load(&mut services.settings_store);
    services.start_diagnostics(&settings);

    let (result, session) = run_ui(startup, settings, services.optional_services_unavailable);

    // Exit order: settings publication, diagnostics stop/finish, then crash
    // marker reconciliation with the proof minted from that finish.
    services.publish_settings(session);
    let crash_marker_finish = services.finish();
    (result, crash_marker_finish)
}

#[cfg(not(windows))]
fn run(startup: cli::StartupRequest) -> (eframe::Result, CrashMarkerFinish) {
    // ADR 0019 item 10: other desktop targets stay explicitly nonpersistent
    // until they implement an equivalent adapter.
    let mut store = SettingsDocumentStore::unavailable("settings.storage.unavailable");
    let settings = SettingsSession::load(&mut store);
    (run_ui(startup, settings, true).0, CrashMarkerFinish::NotInstalled)
}

fn run_ui(
    startup: cli::StartupRequest,
    settings: SettingsSession,
    optional_services_unavailable: bool,
) -> (eframe::Result, Option<SettingsSession>) {
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
                settings,
                settings_exit_for_ui,
            )?))
        }),
    );

    // The shell transfers policy state only after the native event loop has
    // released it, so publication never overlaps an egui callback.
    (result, settings_exit.take())
}

/// Native services owned by the composition root for the process lifetime.
#[cfg(windows)]
struct WindowsStartupServices {
    paths: ProjectPaths,
    settings_file: Option<WindowsSettingsFile>,
    settings_unavailable: Option<ErrorClass>,
    settings_store: SettingsDocumentStore,
    diagnostics: Option<LocalDiagnostics>,
    crash_runtime: Option<CrashRuntime>,
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
            crash_runtime: None,
            optional_services_unavailable: unavailable,
        }
    }

    /// Starts local logging at the level the loaded settings request, records
    /// the startup facts that are already known, and then installs the crash
    /// marker hook before any UI, scan, or Shell thread exists.
    fn start_diagnostics(&mut self, settings: &SettingsSession) {
        let level = DiagnosticLevel::from(settings.settings().diagnostic_level);
        self.diagnostics =
            prepare_diagnostics(&self.paths, level, &mut self.optional_services_unavailable);
        let crashes_ready = self.paths.create_directory(ProjectPathFacility::Crashes).is_ok();
        if !crashes_ready {
            self.optional_services_unavailable = true;
        }
        if self.diagnostics.as_ref().is_some_and(|diagnostics| {
            diagnostics.initial_retention_outcome() != InitialRetentionOutcome::Succeeded
        }) {
            self.optional_services_unavailable = true;
        }

        if let Some(diagnostics) = self.diagnostics.as_ref() {
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

        self.install_crash_runtime(crashes_ready);
    }

    /// Installs the process panic hook through the retained-handle session
    /// (ADR 0017 item 1). Failure is nonfatal: the marker facility is reported
    /// unavailable and startup continues without a hook.
    fn install_crash_runtime(&mut self, crashes_ready: bool) {
        let directory =
            crashes_ready.then(|| self.paths.directory(ProjectPathFacility::Crashes)).flatten();
        let Some(directory) = directory else {
            self.optional_services_unavailable = true;
            self.record(classified_event(
                DiagnosticCode::PanicMarkerUnavailable,
                ErrorClass::NotFound,
            ));
            return;
        };
        let runtime = match CrashRuntime::install(directory) {
            Ok(runtime) => runtime,
            Err(error) => {
                self.optional_services_unavailable = true;
                self.record(classified_event(
                    DiagnosticCode::PanicMarkerUnavailable,
                    error_class_from_crash_marker(error.kind),
                ));
                return;
            }
        };
        for failure in runtime.preparation_failures() {
            self.record(classified_event(
                DiagnosticCode::PanicMarkerUnavailable,
                error_class_from_crash_marker(failure.kind),
            ));
        }
        // ADR 0017 item 8: presence is the only fact recorded; no cause is
        // inferred. A read failure was already reported above.
        if matches!(runtime.prepared_previous_marker(), Ok(Some(_marker))) {
            self.record(DiagnosticEvent::new(DiagnosticCode::PanicMarkerFound));
        }
        self.crash_runtime = Some(runtime);
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

    /// Reports the bounded logging health observed during the run, flushes
    /// and joins the diagnostics worker outside any UI callback, and only
    /// then reconciles the crash marker with a proof minted from that join.
    ///
    /// Without a completed diagnostics finish there is no proof: the runtime
    /// is dropped, which preserves every marker state by construction. That
    /// deliberately includes a session whose diagnostics never started: no
    /// worker exists to join, but there is also no receipt to certify it, so
    /// a marker from a caught worker panic stays on disk and the next start
    /// records `panic.marker_found`. A "diagnostics never started" receipt
    /// would be mintable without any join and is therefore not offered.
    fn finish(self) -> CrashMarkerFinish {
        let Self { diagnostics, crash_runtime, .. } = self;
        let diagnostics_receipt = diagnostics.and_then(|diagnostics| {
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
            // The receipt is minted inside `LocalDiagnostics::finish` from
            // the worker's own completion message and cannot be built here.
            diagnostics.finish().quiescence
        });

        let Some(runtime) = crash_runtime else {
            return CrashMarkerFinish::NotInstalled;
        };
        let Some(diagnostics_receipt) = diagnostics_receipt else {
            drop(runtime);
            return CrashMarkerFinish::PreservedWithoutProof;
        };
        let proof = ShutdownQuiescenceProof::from_receipts(
            diagnostics_receipt,
            RuntimeQuiescenceReceipt::no_runtime_composed(),
        );
        CrashMarkerFinish::Reconciled(runtime.finish_clean(proof))
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
const fn error_class_from_crash_marker(kind: CrashMarkerErrorKind) -> ErrorClass {
    match kind {
        CrashMarkerErrorKind::AccessDenied => ErrorClass::AccessDenied,
        CrashMarkerErrorKind::NotFound => ErrorClass::NotFound,
        CrashMarkerErrorKind::InvalidData
        | CrashMarkerErrorKind::InvalidMarker
        | CrashMarkerErrorKind::OwnershipConflict => ErrorClass::InvalidData,
        CrashMarkerErrorKind::Busy | CrashMarkerErrorKind::PreservedSibling => ErrorClass::Busy,
        CrashMarkerErrorKind::StorageFull | CrashMarkerErrorKind::MarkerTooLarge => {
            ErrorClass::ResourceExhausted
        }
        CrashMarkerErrorKind::Unsupported => ErrorClass::Unsupported,
        CrashMarkerErrorKind::HookAlreadyInstalled | CrashMarkerErrorKind::PanickingThread => {
            ErrorClass::Internal
        }
        CrashMarkerErrorKind::Other => ErrorClass::Io,
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
