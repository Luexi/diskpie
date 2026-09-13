//! DiskPie desktop application entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

pub mod cli;
mod diagnostic_sink;
mod fonts;
#[cfg(windows)]
mod local_diagnostics;
pub mod panic_marker;
mod resolver;
mod scan_ui;
mod shell;
pub mod storage;
pub mod sunburst_view;
#[cfg(windows)]
mod support_service;
mod theme;

use std::{
    process::ExitCode,
    sync::Arc,
    time::{Duration, Instant},
};

use eframe::egui;

use diagnostic_sink::DiagnosticBridge;
use diskpie_app::item_list_service::{ItemListFinish, ItemListService};
use diskpie_app::{runtime::RuntimeController, settings::SettingsSession};
use panic_marker::{CrashMarkerError, FinishCleanOutcome};
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
#[cfg(windows)]
use panic_marker::{
    CrashMarkerErrorKind, CrashRuntime, RuntimeQuiescenceReceipt, RuntimeShutdownEvidence,
    ShutdownQuiescenceProof,
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

    let (result, _crash_marker_finish) = run(startup);
    match result {
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
/// The crash-marker reconciliation turns these into its shutdown-quiescence
/// proof: a clean-shutdown marker may only be produced once every owned
/// provider and worker has actually stopped. This struct reports the facts;
/// [`RuntimeQuiescenceReceipt::from_shutdown`] decides whether they suffice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShutdownReceipts {
    /// `RuntimeController::is_shutdown_complete` observed before disposal.
    /// It does not promise that a blocked filesystem call has returned.
    pub runtime_shutdown_complete: bool,
    /// Scan sessions still held by background retirement when the runtime
    /// reported shutdown complete; zero means no provider was left behind.
    pub runtime_retiring_scans: usize,
    /// Whether a Shell STA was started for this session at all.
    pub shell_service_started: bool,
    /// Outcome of `ShellService::finish`, when the shell handed the STA back.
    #[cfg(windows)]
    pub shell_service: Option<ShellServiceFinish>,
    /// Whether the resolver worker thread was joined within its deadline.
    pub resolver_joined: bool,
    /// Whether the diagnostic bridge thread was joined within its deadline.
    pub diagnostic_bridge_joined: bool,
    /// The item-list service was absent or joined before its deadline.
    pub item_list_joined: bool,
    pub support_joined: bool,
    /// Events the UI dropped because the bounded diagnostic queue was full.
    pub diagnostic_events_dropped: u64,
}

#[cfg(windows)]
impl ShutdownReceipts {
    /// Whether the Shell STA is known to have exited: either none was ever
    /// started, or `finish` joined it within its deadline. A handed-off or
    /// reaper-unavailable STA, and a started STA that was never handed back
    /// for finishing, both count as still possibly alive.
    fn shell_stopped(&self) -> bool {
        match (self.shell_service_started, self.shell_service) {
            (false, _) => true,
            (true, Some(ShellServiceFinish::Exited { .. })) => true,
            (true, _) => false,
        }
    }

    fn runtime_quiescence(&self) -> Option<RuntimeQuiescenceReceipt> {
        RuntimeQuiescenceReceipt::from_shutdown(RuntimeShutdownEvidence {
            runtime_shutdown_complete: self.runtime_shutdown_complete,
            retiring_scans: self.runtime_retiring_scans,
            shell_stopped: self.shell_stopped(),
            resolver_joined: self.resolver_joined,
            bridge_joined: self.diagnostic_bridge_joined,
            item_list_joined: self.item_list_joined,
            support_joined: self.support_joined,
        })
    }
}

#[cfg(windows)]
fn run(startup: cli::StartupRequest) -> (eframe::Result<ShutdownReceipts>, CrashMarkerFinish) {
    let mut services = WindowsStartupServices::prepare();
    let settings = SettingsSession::load(&mut services.settings_store);
    services.start_diagnostics(&settings);
    let prepared = match services.prepare_ui_services(&settings) {
        Ok(prepared) => prepared,
        Err(error) => {
            // Preparation can fail after starting some services. Without
            // explicit join receipts their shutdown is unverified, even though
            // their nonblocking Drops have requested retirement.
            let crash_marker_finish = services.finish(None);
            return (Err(error), crash_marker_finish);
        }
    };
    let shell_service_started = prepared.services.picker.is_some();
    let previous_session_panicked = services.previous_session_panicked();

    let (result, exit) = run_ui(
        startup,
        settings,
        services.optional_services_unavailable,
        previous_session_panicked,
        prepared.services,
    );

    // Exit order: UI-owned services (runtime disposal, Shell STA finish,
    // resolver and bridge joins), settings publication, diagnostics
    // stop/finish, then crash-marker reconciliation with a proof minted from
    // the diagnostics join and the receipts collected above.
    let (receipts, session) = finish_ui_services(
        exit,
        prepared.resolver,
        prepared.bridge,
        shell_service_started,
        prepared.support,
    );
    services.publish_settings(session);
    let crash_marker_finish = services.finish(Some(receipts));
    (result.map(|()| receipts), crash_marker_finish)
}

#[cfg(not(windows))]
fn run(startup: cli::StartupRequest) -> (eframe::Result<ShutdownReceipts>, CrashMarkerFinish) {
    // ADR 0019 item 10: other desktop targets stay explicitly nonpersistent
    // until they implement an equivalent adapter.
    let mut store = SettingsDocumentStore::unavailable("settings.storage.unavailable");
    let settings = SettingsSession::load(&mut store);
    let prepared = (|| -> eframe::Result<PreparedUiServices> {
        let runtime =
            RuntimeController::new(shell::runtime_config_from_settings(settings.settings()))
                .map_err(app_creation_error)?;
        let (resolver_client, resolver) =
            resolver::start(resolver::UnsupportedBackend).map_err(app_creation_error)?;
        let bridge = DiagnosticBridge::start(|_event| {}).map_err(app_creation_error)?;
        Ok(PreparedUiServices {
            services: UiServices {
                runtime,
                picker: None,
                resolver: resolver_client,
                diagnostics: bridge.sink(),
                fonts: fonts::SystemFonts::default(),
                item_list: ItemListService::new().ok(),
            },
            resolver,
            bridge,
        })
    })();
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return (Err(error), CrashMarkerFinish::NotInstalled),
    };
    let (result, exit) = run_ui(startup, settings, true, false, prepared.services);
    let (receipts, _session) = finish_ui_services(exit, prepared.resolver, prepared.bridge, false);
    (result.map(|()| receipts), CrashMarkerFinish::NotInstalled)
}

fn app_creation_error(error: impl std::error::Error + Send + Sync + 'static) -> eframe::Error {
    eframe::Error::AppCreation(Box::new(error))
}

/// Services created before the window and finished after the event loop.
struct PreparedUiServices {
    services: UiServices,
    resolver: ResolverWorker,
    bridge: DiagnosticBridge,
    #[cfg(windows)]
    support: Option<support_service::SupportWorker>,
}

/// Ordered, bounded finish of everything the shell handed back.
///
/// The shell already requested runtime and Shell shutdown from inside the
/// event loop; this only waits within documented deadlines and never from a
/// callback. Order: runtime disposal, Shell STA finish, resolver join, then
/// the diagnostic bridge join. Settings publication and diagnostics finish
/// follow in the caller.
fn finish_ui_services(
    mut exit: Option<ShellExit>,
    resolver: ResolverWorker,
    bridge: DiagnosticBridge,
    shell_service_started: bool,
    #[cfg(windows)] support: Option<support_service::SupportWorker>,
) -> (ShutdownReceipts, Option<SettingsSession>) {
    #[cfg(windows)]
    let pending_action = exit.as_mut().and_then(|exit| exit.pending_action.take());
    // Rejected snapshot/provider ownership is released after the event loop.
    if let Some(exit) = exit.as_mut() {
        exit.retired_resources.clear();
    }
    let (session, mut runtime, picker, clock_origin, item_list) = match exit {
        Some(exit) => {
            (Some(exit.settings), exit.runtime, exit.picker, exit.clock_origin, exit.item_list)
        }
        None => (None, None, None, Instant::now(), None),
    };

    let runtime_shutdown_complete =
        runtime.as_mut().is_some_and(|runtime| await_runtime_shutdown(runtime, clock_origin));
    let runtime_retiring_scans =
        runtime.as_ref().map_or(usize::MAX, RuntimeController::retiring_scans);
    // Disposal is bounded by the runtime's own contract: ownership of any
    // provider that has not returned transfers to the bounded reaper.
    drop(runtime);

    #[cfg(windows)]
    if let Some(pending) = pending_action {
        let deadline = Instant::now() + SERVICE_FINISH_TIMEOUT;
        let event = loop {
            let Some(picker) = picker.as_ref() else {
                break None;
            };
            match picker.try_recv() {
                Ok(Some(event)) if pending.matches(&event) => break Some(event),
                Ok(Some(_)) => continue,
                Err(_) => break None,
                Ok(None) if Instant::now() >= deadline => break None,
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        let report = pending.settle(event);
        let mut event = DiagnosticEvent::new(DiagnosticCode::ShellActionFailed);
        let outcome = if report.requires_halt() { Outcome::Failed } else { Outcome::Degraded };
        let _ = event.push_field(DiagnosticField::Outcome(outcome));
        bridge.sink().emit(event);
        // No snapshot survives exit. Its next use always requires a new scan,
        // satisfying the retained obligation without mutating the closed UI.
    }

    #[cfg(windows)]
    let shell_service = picker.map(|picker| picker.finish(SERVICE_FINISH_TIMEOUT));
    #[cfg(not(windows))]
    drop(picker);

    let item_list_joined = item_list
        .is_none_or(|list| matches!(list.finish(SERVICE_FINISH_TIMEOUT), ItemListFinish::Joined));
    #[cfg(windows)]
    let support_joined = !support_service::has_quarantined_worker()
        && support.is_none_or(|worker| {
            matches!(worker.finish(SERVICE_FINISH_TIMEOUT), support_service::SupportFinish::Joined)
        });
    #[cfg(not(windows))]
    let support_joined = true;

    let resolver_joined = resolver.finish(SERVICE_FINISH_TIMEOUT);
    let diagnostic_events_dropped = bridge.dropped_events();
    let diagnostic_bridge_joined = bridge.finish(SERVICE_FINISH_TIMEOUT);

    (
        ShutdownReceipts {
            runtime_shutdown_complete,
            runtime_retiring_scans,
            shell_service_started,
            #[cfg(windows)]
            shell_service,
            resolver_joined,
            diagnostic_bridge_joined,
            item_list_joined,
            support_joined,
            diagnostic_events_dropped,
        },
        session,
    )
}

/// Polls the runtime's shutdown handoff within the service deadline.
///
/// Each poll also ticks the runtime with the shell's clock: shutdown
/// completion requires the supervisor's output slots to be empty, and a
/// terminal report that arrives after the last frame would otherwise sit
/// there undrained until the deadline. Ticking here is not a callback and
/// never blocks; a tick error (for example `ShuttingDown`) is expected and
/// carries no work of its own.
fn await_runtime_shutdown(runtime: &mut RuntimeController, clock_origin: Instant) -> bool {
    let deadline = Instant::now() + SERVICE_FINISH_TIMEOUT;
    loop {
        if runtime.is_shutdown_complete() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let _ = runtime.tick(|| clock_origin.elapsed());
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

    /// Whether the installed crash session retained a valid marker from a
    /// previous run. The shell shows only that fact (ADR 0017 item 8).
    fn previous_session_panicked(&self) -> bool {
        self.crash_runtime
            .as_ref()
            .is_some_and(|runtime| matches!(runtime.prepared_previous_marker(), Ok(Some(_marker))))
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
        let mut owned_files = self.paths.settings_file.iter().cloned().collect::<Vec<_>>();
        if let Some(directory) = &self.paths.crashes_directory {
            owned_files.push(directory.join("last-panic.txt"));
            owned_files.push(directory.join("last-panic.write"));
        }
        let (support_client, support) =
            match support_service::start(support_service::SupportConfig {
                executable: std::env::current_exe().ok(),
                logs_directory: self.paths.logs_directory.clone(),
                owned_files,
            }) {
                Ok((client, worker)) => (Some(client), Some(worker)),
                Err(_) => {
                    self.optional_services_unavailable = true;
                    (None, None)
                }
            };
        Ok(PreparedUiServices {
            services: UiServices {
                runtime,
                picker,
                resolver: resolver_client,
                diagnostics: bridge.sink(),
                fonts,
                item_list: ItemListService::new().ok(),
                support: support_client,
                diagnostic_counters: self
                    .diagnostics
                    .as_ref()
                    .map(LocalDiagnostics::counter_reader),
            },
            resolver,
            bridge,
            support,
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

    /// Reports the bounded logging health observed during the run, flushes
    /// and joins the diagnostics worker outside any UI callback, and only
    /// then reconciles the crash marker with a proof minted from that join
    /// and from the UI-service shutdown receipts.
    ///
    /// Without a completed diagnostics finish, or with any UI-owned worker
    /// that did not provably stop, there is no proof: the runtime is dropped,
    /// which preserves every marker state by construction. That deliberately
    /// includes a session whose diagnostics never started: no worker exists
    /// to join, but there is also no receipt to certify it, so a marker from
    /// a caught worker panic stays on disk and the next start records
    /// `panic.marker_found`. Missing UI receipts also cover partial startup
    /// failures; they must never assert that no runtime was composed.
    fn finish(self, receipts: Option<ShutdownReceipts>) -> CrashMarkerFinish {
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
            if let Some(receipts) = receipts
                && receipts.diagnostic_events_dropped > 0
            {
                diagnostics.record(count_event(
                    DiagnosticCode::DiagnosticQueueDropped,
                    receipts.diagnostic_events_dropped,
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
        let runtime_receipt = verified_ui_quiescence(receipts);
        let Some(runtime_receipt) = runtime_receipt else {
            drop(runtime);
            return CrashMarkerFinish::PreservedWithoutProof;
        };
        let proof = ShutdownQuiescenceProof::from_receipts(diagnostics_receipt, runtime_receipt);
        CrashMarkerFinish::Reconciled(runtime.finish_clean(proof))
    }

    fn record(&self, event: DiagnosticEvent) {
        if let Some(diagnostics) = self.diagnostics.as_ref() {
            diagnostics.record(event);
        }
    }
}

/// A missing exit/receipt set is not evidence that startup created no workers.
#[cfg(windows)]
fn verified_ui_quiescence(receipts: Option<ShutdownReceipts>) -> Option<RuntimeQuiescenceReceipt> {
    receipts.and_then(|receipts| receipts.runtime_quiescence())
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

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    /// The `runtime_shutdown_complete` receipt is a real observation: a
    /// runtime whose shutdown was requested from the frame loop hands its
    /// ownership to background retirement within the service deadline, and
    /// disposal after that returns without waiting on a callback.
    #[test]
    fn runtime_shutdown_receipt_is_true_after_a_requested_shutdown() {
        let mut runtime = RuntimeController::new(shell::runtime_config_from_settings(
            &diskpie_app::settings::Settings::default(),
        ))
        .expect("runtime starts");
        assert!(!runtime.is_shutdown_complete(), "nothing was requested yet");
        runtime.request_shutdown();
        let started = Instant::now();
        assert!(await_runtime_shutdown(&mut runtime, started));
        assert!(started.elapsed() < SERVICE_FINISH_TIMEOUT);
        drop(runtime);
    }

    #[test]
    fn finishing_without_a_shell_exit_still_joins_the_root_owned_workers() {
        let (client, resolver) =
            resolver::start(resolver::UnsupportedBackend).expect("resolver starts");
        let bridge = DiagnosticBridge::start(|_event| {}).expect("bridge starts");
        client.request_stop();
        drop(client);
        let (receipts, session) = finish_ui_services(
            None,
            resolver,
            bridge,
            false,
            #[cfg(windows)]
            None,
        );
        assert!(session.is_none());
        assert!(!receipts.runtime_shutdown_complete, "no runtime was handed back");
        assert_eq!(receipts.runtime_retiring_scans, usize::MAX, "unknown without a runtime");
        assert!(receipts.resolver_joined);
        assert!(receipts.diagnostic_bridge_joined);
        assert_eq!(receipts.diagnostic_events_dropped, 0);
    }

    #[cfg(windows)]
    #[test]
    fn quiescence_needs_every_ui_worker_to_have_stopped() {
        let (client, resolver) =
            resolver::start(resolver::UnsupportedBackend).expect("resolver starts");
        let bridge = DiagnosticBridge::start(|_event| {}).expect("bridge starts");
        client.request_stop();
        drop(client);
        let (receipts, _session) = finish_ui_services(None, resolver, bridge, true, None);
        assert!(!receipts.shell_stopped(), "a started STA without a finish is not stopped");
        assert!(receipts.runtime_quiescence().is_none());

        let clean = ShutdownReceipts {
            runtime_shutdown_complete: true,
            runtime_retiring_scans: 0,
            shell_service_started: true,
            shell_service: Some(ShellServiceFinish::Exited { panicked: false }),
            resolver_joined: true,
            diagnostic_bridge_joined: true,
            item_list_joined: true,
            support_joined: true,
            diagnostic_events_dropped: 0,
        };
        assert!(clean.runtime_quiescence().is_some());
        assert!(verified_ui_quiescence(Some(clean)).is_some());
        assert!(
            ShutdownReceipts { shell_service_started: false, shell_service: None, ..clean }
                .runtime_quiescence()
                .is_some(),
            "a session that never started the STA needs no STA receipt"
        );
        for degraded in [
            ShutdownReceipts {
                shell_service: Some(ShellServiceFinish::TimedOutHandedToReaper),
                ..clean
            },
            ShutdownReceipts {
                shell_service: Some(ShellServiceFinish::TimedOutReaperUnavailable),
                ..clean
            },
            ShutdownReceipts { runtime_retiring_scans: 1, ..clean },
            ShutdownReceipts { runtime_shutdown_complete: false, ..clean },
            ShutdownReceipts { resolver_joined: false, ..clean },
            ShutdownReceipts { diagnostic_bridge_joined: false, ..clean },
            ShutdownReceipts { item_list_joined: false, ..clean },
            ShutdownReceipts { support_joined: false, ..clean },
        ] {
            assert!(degraded.runtime_quiescence().is_none(), "{degraded:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn startup_failure_after_starting_a_worker_has_no_clean_shutdown_proof() {
        // Model an error at the next fallible startup step while an earlier
        // worker is demonstrably alive. No native settings or marker is touched.
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let worker = std::thread::spawn(move || {
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let failed_startup: Result<ShutdownReceipts, std::io::Error> =
            Err(std::io::Error::other("injected next-service startup failure"));
        let receipts = failed_startup.ok();
        assert!(!worker.is_finished());
        assert!(verified_ui_quiescence(receipts).is_none());
        drop(release_tx);
        worker.join().unwrap();
    }
}
