//! Responsive native shell for the DiskPie application.
//!
//! The shell owns UI-only state plus the handles the composition root gave it:
//! the runtime controller, the optional Shell STA service, the resolver client,
//! and a bounded diagnostic sink. Every frame runs one bounded logic phase
//! (drain shell events, drain resolver results, tick the runtime) and then
//! renders the committed immutable frame. Nothing in this module performs
//! filesystem, Shell, COM, layout, settings, or diagnostics I/O, and nothing
//! here joins a thread.

use std::{
    f32::consts::{FRAC_PI_2, TAU},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Color32, FontId, Key, Layout, Modifiers, Pos2, RichText, Sense, Shape, Stroke,
    ThemePreference, Vec2,
};

use diskpie_app::{
    diagnostics::{DiagnosticCode, DiagnosticEvent, DiagnosticField, ErrorClass},
    format::{format_iec_bytes, format_path_for_display},
    i18n::{I18n, I18nError, Locale, MessageId},
    navigation::{CommandAvailability, NavigationAction, UnavailableReason},
    runtime::{RuntimeConfig, RuntimeController, RuntimeError, RuntimeState},
    session::SessionProgress,
    settings::{
        Settings, SettingsSession, SizePreference, UiLocale as SettingsLocale,
        UiTheme as SettingsTheme,
    },
};
use diskpie_core::{
    EntryKind, GenerationId, NodeId, NodeRecord, TreeSnapshot,
    sunburst::{LayoutOptions, SizeBasis},
};
use diskpie_scan::{CancelToken, ScanFs, ScanRoot};

use crate::{
    diagnostic_sink::DiagnosticSink,
    fonts::SystemFonts,
    resolver::{
        ResolveJob, ResolveJobId, ResolveOutcome, ResolveResult, ResolvedLaunch, ResolverClient,
        SubmitJobError, VolumeList,
    },
    scan_ui::{
        DialogOutcome, FailureClass, LaunchOutcome, RepaintRequest, RuntimeObservation, ScanFlow,
        ScanUi, repaint_policy,
    },
    sunburst_view::{
        NodeChange, SunburstHitKind, SunburstMeshCache, SunburstMeshOptions, SunburstTooltip,
        SunburstView,
    },
    theme,
};

#[cfg(windows)]
use diskpie_platform::{DialogEvent, OwnerWindow, ShellService, SubmitError};

/// Largest-children rows offered by the synchronized list.
const LARGEST_CHILDREN_LIMIT: usize = 200;
/// Shell events drained per frame.
const SHELL_EVENT_BUDGET: usize = 4;
/// Resolver results drained per frame.
const RESOLVER_RESULT_BUDGET: usize = 4;
/// Frames a launch may wait for retirement capacity before it is reported.
const MAX_DEFERRED_LAUNCH_FRAMES: u32 = 900;
/// Frames a navigation command may wait for a frame transition to settle.
const MAX_DEFERRED_ACTION_FRAMES: u32 = 120;
/// Poll interval while a dialog or resolver job is outstanding.
const BACKGROUND_POLL_INTERVAL: Duration = Duration::from_millis(33);

/// Preformatted static messages keep Fluent parsing and allocation off the frame loop.
///
/// Messages that interpolate arguments cannot be resolved ahead of time; they
/// are left out of the cache and must be formatted through [`I18n`] at the
/// moment their value is known (see [`DiskPieShell::item_count`]).
struct UiStrings {
    values: Box<[Option<String>]>,
}

impl UiStrings {
    fn new(i18n: &I18n) -> Self {
        let values = MessageId::ALL
            .into_iter()
            .map(|id| {
                if id.requires_arguments() {
                    None
                } else {
                    i18n.text(id).ok().map(std::borrow::Cow::into_owned)
                }
            })
            .collect();
        Self { values }
    }

    /// Returns the cached text. A message that needs arguments is a caller
    /// error and yields its stable key so the defect is visible, never a
    /// half-formatted sentence.
    fn get(&self, id: MessageId) -> &str {
        debug_assert!(!id.requires_arguments(), "{} needs arguments", id.key());
        self.values.get(id.index()).and_then(Option::as_deref).unwrap_or_else(|| id.key())
    }
}

/// Native folder picker. On Windows this is the dedicated Shell STA service.
#[cfg(windows)]
pub type NativeFolderPicker = ShellService;

/// Placeholder on targets without a native picker; never constructed.
#[cfg(not(windows))]
pub struct NativeFolderPicker {
    _private: (),
}

/// Handles the composition root prepares before the native window exists.
pub struct UiServices {
    pub runtime: RuntimeController,
    pub picker: Option<NativeFolderPicker>,
    pub resolver: ResolverClient,
    pub diagnostics: DiagnosticSink,
    pub fonts: SystemFonts,
}

/// Ownership handed back to the composition root after eframe drops the app.
pub struct ShellExit {
    pub settings: SettingsSession,
    pub runtime: Option<RuntimeController>,
    pub picker: Option<NativeFolderPicker>,
}

/// Single handoff from the dropped eframe application to the composition root.
#[derive(Clone, Default)]
pub(crate) struct ExitHandoff(Arc<Mutex<Option<ShellExit>>>);

impl ExitHandoff {
    #[must_use]
    pub(crate) fn take(&self) -> Option<ShellExit> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()
    }

    fn publish(&self, exit: ShellExit) {
        *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(exit);
    }
}

/// A launch waiting for retirement capacity; retried on a later frame.
struct DeferredLaunch {
    fs: Arc<dyn ScanFs>,
    roots: Vec<ScanRoot>,
    cancel: CancelToken,
    displays: Vec<String>,
    issues: Vec<String>,
    frames: u32,
}

/// User-facing status line. Free text is display-only and never logged.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Notice {
    Message(MessageId),
    Text(String),
}

/// Typed commands the widgets emit; dispatched after rendering borrows end.
#[derive(Clone, Debug, PartialEq)]
pub enum ShellCommand {
    Navigate(NavigationAction),
    ChooseFolder,
    ScanPaths(Vec<PathBuf>),
    ScanAllVolumes,
    RefreshVolumes,
    Cancel,
    SetMetric(SizeBasis),
    Focus(Option<NodeId>),
}

/// Cached largest-children ranking for the committed frame.
#[derive(Default)]
struct ListCache {
    key: Option<(u64, NodeId, SizeBasis)>,
    rows: Vec<NodeId>,
}

pub struct DiskPieShell {
    i18n: I18n,
    strings: UiStrings,
    locale: Locale,
    theme_preference: ThemePreference,
    metric: SizeBasis,
    show_both_sizes: bool,
    settings: SettingsSession,
    exit: ExitHandoff,
    runtime: Option<RuntimeController>,
    mesh_cache: SunburstMeshCache,
    mesh_options: SunburstMeshOptions,
    picker: Option<NativeFolderPicker>,
    resolver: ResolverClient,
    diagnostics: DiagnosticSink,
    flow: ScanFlow,
    volumes: Option<VolumeList>,
    volumes_job: Option<ResolveJobId>,
    deferred_launch: Option<DeferredLaunch>,
    deferred_action: Option<(NavigationAction, u32)>,
    roots_display: Vec<String>,
    resolve_issues: Vec<String>,
    focused: Option<NodeId>,
    context_node: Option<NodeId>,
    notice: Notice,
    previous_session_panicked: bool,
    optional_services_unavailable: bool,
    started: Instant,
    shutdown_requested: bool,
    list_cache: ListCache,
    path_scratch: PathBuf,
    last_selected: Option<NodeId>,
    scroll_to_selection: bool,
    #[cfg(windows)]
    owner: Option<OwnerWindow>,
}

impl DiskPieShell {
    /// Builds the shell inside the eframe creation callback.
    pub fn new(
        creation: &eframe::CreationContext<'_>,
        startup_path: Option<PathBuf>,
        optional_services_unavailable: bool,
        previous_session_panicked: bool,
        settings: SettingsSession,
        services: UiServices,
        exit: ExitHandoff,
    ) -> Result<Self, I18nError> {
        Self::with_context(
            &creation.egui_ctx,
            startup_path,
            optional_services_unavailable,
            previous_session_panicked,
            settings,
            services,
            exit,
        )
    }

    /// Builds the shell for any egui context, including a headless one in tests.
    pub fn with_context(
        ctx: &egui::Context,
        startup_path: Option<PathBuf>,
        optional_services_unavailable: bool,
        previous_session_panicked: bool,
        settings: SettingsSession,
        services: UiServices,
        exit: ExitHandoff,
    ) -> Result<Self, I18nError> {
        theme::install(ctx);
        if !services.fonts.is_empty() {
            ctx.set_fonts(services.fonts.apply_to(egui::FontDefinitions::default()));
        }
        let locale = locale_from_setting(settings.settings().locale);
        let theme_preference = theme_from_setting(settings.settings().theme);
        let metric = basis_from_setting(settings.settings().size_preference);
        ctx.set_theme(theme_preference);
        if let Some(scale) = settings.settings().ui_scale {
            ctx.set_zoom_factor(scale);
        }
        let i18n = I18n::new(locale)?;
        let strings = UiStrings::new(&i18n);
        let notice = if optional_services_unavailable || settings.warning().is_some() {
            Notice::Message(MessageId::OptionalServicesUnavailable)
        } else {
            Notice::Message(MessageId::StatusReady)
        };
        let UiServices { runtime, picker, resolver, diagnostics, fonts: _ } = services;
        let mut shell = Self {
            i18n,
            strings,
            locale,
            theme_preference,
            metric,
            show_both_sizes: settings.settings().show_both_sizes,
            settings,
            exit,
            runtime: Some(runtime),
            mesh_cache: SunburstMeshCache::new(),
            mesh_options: SunburstMeshOptions::default(),
            picker,
            resolver,
            diagnostics,
            flow: ScanFlow::new(),
            volumes: None,
            volumes_job: None,
            deferred_launch: None,
            deferred_action: None,
            roots_display: Vec::new(),
            resolve_issues: Vec::new(),
            focused: None,
            context_node: None,
            notice,
            previous_session_panicked,
            optional_services_unavailable,
            started: Instant::now(),
            shutdown_requested: false,
            list_cache: ListCache::default(),
            path_scratch: PathBuf::new(),
            last_selected: None,
            scroll_to_selection: false,
            #[cfg(windows)]
            owner: None,
        };
        shell.request_volumes();
        if let Some(path) = startup_path {
            shell.request_scan_paths(vec![path]);
        }
        Ok(shell)
    }

    /// The state rendered this frame.
    #[cfg(test)]
    #[must_use]
    pub fn scan_state(&self) -> ScanUi {
        self.flow.state()
    }

    /// Per-frame bounded logic: poll services and the runtime, never wait.
    pub fn logic(&mut self, ctx: &egui::Context) {
        self.drain_shell_events();
        self.drain_resolver_results();
        self.retry_deferred_launch();
        self.tick_runtime();
        self.retry_deferred_action();
        self.sync_size_basis();
        self.observe_runtime();

        let runtime_needs_repaint =
            self.runtime.as_ref().is_some_and(RuntimeController::needs_repaint);
        match repaint_policy(runtime_needs_repaint, &self.flow) {
            RepaintRequest::Now => ctx.request_repaint(),
            RepaintRequest::Soon => ctx.request_repaint_after(BACKGROUND_POLL_INTERVAL),
            RepaintRequest::None => {}
        }
    }

    /// Asks every owned service to stop without waiting. Idempotent.
    pub fn begin_shutdown(&mut self) {
        if self.shutdown_requested {
            return;
        }
        self.shutdown_requested = true;
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.request_shutdown();
        }
        #[cfg(windows)]
        if let Some(picker) = self.picker.as_ref() {
            picker.request_shutdown();
        }
        self.resolver.request_stop();
    }

    fn drain_shell_events(&mut self) {
        #[cfg(windows)]
        for _ in 0..SHELL_EVENT_BUDGET {
            let Some(picker) = self.picker.as_ref() else {
                break;
            };
            match picker.try_recv() {
                Ok(Some(event)) => self.handle_dialog_event(event),
                Ok(None) => break,
                Err(_stopped) => {
                    self.picker = None;
                    if let Some(request) = self.flow.dialog_outstanding() {
                        self.flow.dialog_finished(request, DialogOutcome::Failed);
                        self.notice = Notice::Message(MessageId::DialogFailed);
                    }
                    break;
                }
            }
        }
    }

    #[cfg(windows)]
    fn handle_dialog_event(&mut self, event: DialogEvent) {
        let request_id = event.request_id().get();
        match event {
            DialogEvent::Selected { path, .. } => {
                self.flow.dialog_finished(request_id, DialogOutcome::Selected);
                self.request_scan_paths(vec![path]);
            }
            DialogEvent::Cancelled { .. } => {
                self.flow.dialog_finished(request_id, DialogOutcome::Cancelled);
            }
            DialogEvent::Failed { .. } => {
                self.flow.dialog_finished(request_id, DialogOutcome::Failed);
                self.notice = Notice::Message(MessageId::DialogFailed);
                self.diagnostics.emit(shell_failed_event(request_id, ErrorClass::Internal));
            }
        }
    }

    fn drain_resolver_results(&mut self) {
        for _ in 0..RESOLVER_RESULT_BUDGET {
            let Some(result) = self.resolver.try_recv() else {
                break;
            };
            self.handle_resolve_result(result);
        }
    }

    fn handle_resolve_result(&mut self, result: ResolveResult) {
        let ResolveResult { id, outcome } = result;
        if self.volumes_job == Some(id) {
            self.volumes_job = None;
            match outcome {
                ResolveOutcome::Volumes(list) => self.volumes = Some(list),
                ResolveOutcome::Failed(failure) => {
                    self.volumes =
                        Some(VolumeList { volumes: Vec::new(), errors: vec![failure.detail] });
                }
                ResolveOutcome::Launch(_) | ResolveOutcome::Abandoned => {}
            }
            return;
        }
        match outcome {
            ResolveOutcome::Launch(launch) => {
                self.flow.resolve_finished(Ok(()));
                self.launch(launch);
            }
            ResolveOutcome::Failed(failure) => {
                self.flow.resolve_finished(Err(failure.class));
                self.notice = Notice::Text(failure.detail);
                self.diagnostics.emit(classified_event(
                    DiagnosticCode::ScanFailed,
                    failure.class.error_class(),
                ));
            }
            ResolveOutcome::Volumes(list) => {
                self.flow.resolve_finished(Ok(()));
                self.volumes = Some(list);
            }
            ResolveOutcome::Abandoned => self.flow.resolve_finished(Ok(())),
        }
    }

    fn launch(&mut self, launch: ResolvedLaunch) {
        let ResolvedLaunch { fs, roots, cancel, displays, issues } = launch;
        self.try_launch(DeferredLaunch { fs, roots, cancel, displays, issues, frames: 0 });
    }

    fn try_launch(&mut self, launch: DeferredLaunch) {
        let DeferredLaunch { fs, roots, cancel, displays, issues, frames } = launch;
        let Some(runtime) = self.runtime.as_mut() else {
            self.flow.launch_attempted(LaunchOutcome::Rejected(FailureClass::Other));
            return;
        };
        let root_count = u64::try_from(roots.len()).unwrap_or(u64::MAX);
        match runtime.request_scan(fs, roots, cancel) {
            Ok(receipt) => {
                self.flow.launch_attempted(LaunchOutcome::Accepted);
                self.roots_display = displays;
                self.resolve_issues = issues;
                self.focused = None;
                self.notice = Notice::Message(MessageId::Scanning);
                let mut event = DiagnosticEvent::new(DiagnosticCode::ScanRequested);
                let _generation =
                    event.push_field(DiagnosticField::GenerationId(receipt.generation.get()));
                let _roots = event.push_field(DiagnosticField::ItemCount(root_count));
                self.diagnostics.emit(event);
            }
            Err(rejected) => {
                let (error, fs, roots, cancel) = rejected.into_parts();
                if error == RuntimeError::RetirementBackpressure
                    && frames < MAX_DEFERRED_LAUNCH_FRAMES
                {
                    self.flow.launch_attempted(LaunchOutcome::Deferred);
                    self.deferred_launch = Some(DeferredLaunch {
                        fs,
                        roots,
                        cancel,
                        displays,
                        issues,
                        frames: frames.saturating_add(1),
                    });
                } else {
                    let class = FailureClass::from_runtime_error(&error);
                    self.flow.launch_attempted(LaunchOutcome::Rejected(class));
                    self.notice = Notice::Message(failure_message(class));
                    self.diagnostics
                        .emit(classified_event(DiagnosticCode::ScanFailed, class.error_class()));
                }
            }
        }
    }

    fn retry_deferred_launch(&mut self) {
        if let Some(launch) = self.deferred_launch.take() {
            self.try_launch(launch);
        }
    }

    fn tick_runtime(&mut self) {
        let elapsed = self.started.elapsed();
        let Some(runtime) = self.runtime.as_mut() else {
            return;
        };
        match runtime.tick(|| elapsed) {
            Ok(report) => {
                if let Some(generation) = report.scan_started {
                    let mut event = DiagnosticEvent::new(DiagnosticCode::ScanStarted);
                    let _generation =
                        event.push_field(DiagnosticField::GenerationId(generation.get()));
                    self.diagnostics.emit(event);
                }
                if let Some(generation) = report.scan_finished {
                    let summary = runtime
                        .settled_summary()
                        .filter(|summary| summary.generation == generation);
                    if let Some(summary) = summary {
                        let counters = summary.progress.effective();
                        let (code, class, notice) = match &summary.phase {
                            diskpie_app::session::SessionPhase::Cancelled => {
                                (DiagnosticCode::ScanCancelled, None, MessageId::Cancelled)
                            }
                            diskpie_app::session::SessionPhase::Failed(failure) => {
                                let class = FailureClass::from_scan_failure(failure);
                                (
                                    DiagnosticCode::ScanFailed,
                                    Some(class.error_class()),
                                    failure_message(class),
                                )
                            }
                            diskpie_app::session::SessionPhase::Partial => {
                                (DiagnosticCode::ScanCompleted, None, MessageId::PartialSettled)
                            }
                            _ => (DiagnosticCode::ScanCompleted, None, MessageId::Complete),
                        };
                        self.notice = Notice::Message(notice);
                        let mut event = DiagnosticEvent::new(code);
                        let _generation =
                            event.push_field(DiagnosticField::GenerationId(generation.get()));
                        let _files = event.push_field(DiagnosticField::FileCount(counters.files));
                        let _directories =
                            event.push_field(DiagnosticField::DirectoryCount(counters.directories));
                        let _omissions =
                            event.push_field(DiagnosticField::OmissionCount(counters.omissions));
                        if let Some(class) = class {
                            let _class = event.push_field(DiagnosticField::ErrorClass(class));
                        }
                        self.diagnostics.emit(event);
                        if counters.omissions > 0 {
                            let mut omission = DiagnosticEvent::new(DiagnosticCode::ScanOmission);
                            let _generation = omission
                                .push_field(DiagnosticField::GenerationId(generation.get()));
                            let _count = omission
                                .push_field(DiagnosticField::OmissionCount(counters.omissions));
                            self.diagnostics.emit(omission);
                        }
                    }
                }
            }
            Err(error) => {
                let class = FailureClass::from_runtime_error(&error);
                self.notice = Notice::Message(failure_message(class));
                runtime.clear_error();
            }
        }
    }

    /// Re-applies the user's metric after the first frame of a generation
    /// commits with the navigation default.
    fn sync_size_basis(&mut self) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        let Some(navigation) = runtime.navigation() else {
            return;
        };
        if navigation.size_basis() != self.metric && self.deferred_action.is_none() {
            let metric = self.metric;
            self.navigate(NavigationAction::SetSizeBasis(metric));
        }
    }

    fn observe_runtime(&mut self) {
        let Some(runtime) = self.runtime.as_ref() else {
            self.flow.observe_runtime(RuntimeObservation::ShuttingDown, false);
            return;
        };
        let state = runtime.state();
        let current_generation = match &state {
            RuntimeState::Active { generation, .. }
            | RuntimeState::Cancelling { generation, .. }
            | RuntimeState::Settled { generation, .. } => Some(GenerationId::from(*generation)),
            RuntimeState::Idle
            | RuntimeState::ShuttingDown { .. }
            | RuntimeState::ShutdownComplete => None,
        };
        let has_results = runtime
            .snapshot()
            .is_some_and(|snapshot| Some(snapshot.generation()) == current_generation);
        self.flow.observe_runtime(RuntimeObservation::from_state(&state), has_results);

        let selected = runtime.navigation().and_then(|navigation| navigation.selected());
        if selected != self.last_selected {
            self.last_selected = selected;
            self.scroll_to_selection = selected.is_some();
        }
    }

    fn request_volumes(&mut self) {
        if self.volumes_job.is_some() {
            return;
        }
        match self.resolver.submit(ResolveJob::Volumes) {
            Ok(id) => self.volumes_job = Some(id),
            Err(SubmitJobError::QueueFull | SubmitJobError::WorkerStopped) => {}
        }
    }

    fn request_scan_paths(&mut self, paths: Vec<PathBuf>) {
        match self.resolver.submit(ResolveJob::Paths(paths)) {
            Ok(_id) => {
                self.flow.resolve_started();
                self.notice = Notice::Message(MessageId::Resolving);
            }
            Err(SubmitJobError::QueueFull) => {
                self.notice = Notice::Message(MessageId::ResolverBusy)
            }
            Err(SubmitJobError::WorkerStopped) => {
                self.notice = Notice::Message(failure_message(FailureClass::Other));
            }
        }
    }

    fn choose_folder(&mut self) {
        #[cfg(windows)]
        {
            let Some(picker) = self.picker.as_ref() else {
                self.notice = Notice::Message(MessageId::FolderPickerUnavailable);
                return;
            };
            let Some(owner) = self.owner else {
                self.notice = Notice::Message(MessageId::DialogFailed);
                return;
            };
            match picker.submit_folder(owner) {
                Ok(id) => {
                    self.flow.dialog_submitted(id.get());
                    self.notice = Notice::Message(MessageId::Choosing);
                    let mut event = DiagnosticEvent::new(DiagnosticCode::ShellActionRequested);
                    let _request = event.push_field(DiagnosticField::RequestId(id.get()));
                    self.diagnostics.emit(event);
                }
                Err(SubmitError::QueueFull | SubmitError::ModalOutstanding) => {
                    self.notice = Notice::Message(MessageId::DialogBusy);
                }
                Err(SubmitError::ServiceUnavailable) => {
                    self.notice = Notice::Message(MessageId::FolderPickerUnavailable);
                    self.diagnostics.emit(shell_failed_event(0, ErrorClass::Unsupported));
                }
            }
        }
        #[cfg(not(windows))]
        {
            self.notice = Notice::Message(MessageId::FolderPickerUnavailable);
        }
    }

    /// Routes one typed navigation action through the runtime.
    fn navigate(&mut self, action: NavigationAction) {
        let Some(runtime) = self.runtime.as_mut() else {
            return;
        };
        let command = match runtime.navigation_command(action) {
            Ok(command) => command,
            Err(RuntimeError::NoNavigation | RuntimeError::ShuttingDown) => return,
            Err(_error) => {
                self.notice = Notice::Message(MessageId::ScanFailedOther);
                return;
            }
        };
        if let Some(navigation) = runtime.navigation()
            && let CommandAvailability::Unavailable(reason) = navigation.availability(command)
        {
            self.notice = Notice::Message(reason_message(reason));
            return;
        }
        match runtime.execute_navigation(command) {
            Ok(report) => {
                if let Some(request) = report.rescan_request() {
                    let paths = request
                        .full_roots()
                        .iter()
                        .map(|target| target.path().to_path_buf())
                        .collect::<Vec<_>>();
                    if paths.is_empty() {
                        // Branch rescans have no engine path yet; availability
                        // already reports it, this is defensive.
                        self.notice = Notice::Message(MessageId::RescanBranchUnavailable);
                    } else {
                        self.request_scan_paths(paths);
                    }
                }
            }
            Err(RuntimeError::FrameTransitionPending) => {
                self.deferred_action = Some((action, 0));
            }
            Err(RuntimeError::Navigation(error)) => {
                if let diskpie_app::navigation::NavigationError::CommandUnavailable {
                    reason, ..
                } = error
                {
                    self.notice = Notice::Message(reason_message(reason));
                }
                runtime.clear_error();
            }
            Err(_error) => {
                self.notice = Notice::Message(MessageId::ScanFailedOther);
                runtime.clear_error();
            }
        }
    }

    fn retry_deferred_action(&mut self) {
        let Some((action, frames)) = self.deferred_action.take() else {
            return;
        };
        if frames >= MAX_DEFERRED_ACTION_FRAMES {
            return;
        }
        let Some(runtime) = self.runtime.as_mut() else {
            return;
        };
        let Ok(command) = runtime.navigation_command(action) else {
            return;
        };
        match runtime.execute_navigation(command) {
            Ok(report) => {
                if let Some(request) = report.rescan_request() {
                    let paths = request
                        .full_roots()
                        .iter()
                        .map(|target| target.path().to_path_buf())
                        .collect::<Vec<_>>();
                    if !paths.is_empty() {
                        self.request_scan_paths(paths);
                    }
                }
            }
            Err(RuntimeError::FrameTransitionPending) => {
                self.deferred_action = Some((action, frames.saturating_add(1)));
            }
            Err(_error) => runtime.clear_error(),
        }
    }

    /// Executes commands emitted by the widgets this frame.
    pub fn dispatch(&mut self, command: ShellCommand) {
        match command {
            ShellCommand::Navigate(action) => self.navigate(action),
            ShellCommand::ChooseFolder => self.choose_folder(),
            ShellCommand::ScanPaths(paths) => self.request_scan_paths(paths),
            ShellCommand::ScanAllVolumes => {
                let paths: Vec<PathBuf> = self
                    .volumes
                    .as_ref()
                    .map(|list| list.volumes.iter().map(|volume| volume.path.clone()).collect())
                    .unwrap_or_default();
                if !paths.is_empty() {
                    self.request_scan_paths(paths);
                }
            }
            ShellCommand::RefreshVolumes => self.request_volumes(),
            ShellCommand::Cancel => {
                if let Some(runtime) = self.runtime.as_mut()
                    && runtime.cancel_active()
                {
                    self.notice = Notice::Message(MessageId::Cancelling);
                }
            }
            ShellCommand::SetMetric(basis) => {
                self.metric = basis;
                self.navigate(NavigationAction::SetSizeBasis(basis));
            }
            ShellCommand::Focus(node) => self.focused = node,
        }
    }

    fn availability(&self, action: NavigationAction) -> CommandAvailability {
        let Some(navigation) = self.runtime.as_ref().and_then(RuntimeController::navigation) else {
            return CommandAvailability::Unavailable(UnavailableReason::NoRescanRoots);
        };
        navigation.availability(navigation.command(action))
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

    fn synchronize_settings(&mut self) {
        let mut updated = self.settings.settings().clone();
        updated.locale = setting_from_locale(self.locale);
        updated.theme = setting_from_theme(self.theme_preference);
        updated.size_preference = setting_from_basis(self.metric);
        if updated != *self.settings.settings() {
            self.settings.replace(updated);
        }
    }

    fn item_count(&self, count: u64) -> String {
        self.i18n.item_count(count).unwrap_or_else(|_| count.to_string())
    }

    fn phase_message(&self) -> MessageId {
        phase_message(self.flow.state())
    }

    fn notice_text(&self) -> &str {
        match &self.notice {
            Notice::Message(id) => self.strings.get(*id),
            Notice::Text(text) => text,
        }
    }

    // ----------------------------------------------------------------------
    // Rendering
    // ----------------------------------------------------------------------

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let can_cancel = self.flow.state().can_cancel();
        let mut commands = Vec::new();
        ctx.input_mut(|input| {
            for (modifiers, key) in SHORTCUTS {
                if let Some(command) = shortcut_command(modifiers, key, can_cancel)
                    && input.consume_key(modifiers, key)
                {
                    commands.push(command);
                }
            }
        });
        for command in commands {
            self.dispatch(command);
        }
    }

    fn command_rail(&mut self, ui: &mut egui::Ui) {
        let mut commands = Vec::new();
        let state = self.flow.state();
        let back = self.availability(NavigationAction::Back);
        let parent = self.availability(NavigationAction::Parent);
        let rescan = self.availability(NavigationAction::RescanAll);
        let summary = self.availability(NavigationAction::ShowSummary);
        let is_summary_view = self
            .runtime
            .as_ref()
            .and_then(RuntimeController::navigation)
            .is_some_and(diskpie_app::navigation::NavigationState::is_summary_view);
        let picker_available = self.picker.is_some();
        let location = if self.roots_display.is_empty() {
            None
        } else {
            Some(self.roots_display.join(" · "))
        };
        let strings = &self.strings;
        let mut selected_locale = self.locale;
        let theme_preference = &mut self.theme_preference;

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
                    if command_button(ui, strings, MessageId::Back, back, "Backspace · Alt+←") {
                        commands.push(ShellCommand::Navigate(NavigationAction::Back));
                    }
                    if command_button(ui, strings, MessageId::Parent, parent, "Alt+↑") {
                        commands.push(ShellCommand::Navigate(NavigationAction::Parent));
                    }
                    let choose = ui
                        .add_enabled(
                            picker_available && state != ScanUi::Choosing,
                            egui::Button::new(strings.get(MessageId::ChooseFolder)),
                        )
                        .on_hover_text("Ctrl+O")
                        .on_disabled_hover_text(if picker_available {
                            strings.get(MessageId::Choosing)
                        } else {
                            strings.get(MessageId::FolderPickerUnavailable)
                        });
                    if choose.clicked() {
                        commands.push(ShellCommand::ChooseFolder);
                    }
                    if command_button(ui, strings, MessageId::Rescan, rescan, "F5") {
                        commands.push(ShellCommand::Navigate(NavigationAction::RescanAll));
                    }
                    let cancel = ui
                        .add_enabled(
                            state.can_cancel(),
                            egui::Button::new(strings.get(MessageId::Cancel)),
                        )
                        .on_hover_text("Esc");
                    if cancel.clicked() {
                        commands.push(ShellCommand::Cancel);
                    }
                    if (summary.is_available() || is_summary_view)
                        && command_button(ui, strings, MessageId::ShowSummary, summary, "")
                    {
                        commands.push(ShellCommand::Navigate(NavigationAction::ShowSummary));
                    }
                    // The trailing preference cluster is laid out left to right
                    // inside a right-aligned region so keyboard focus travels in
                    // reading order instead of mirroring the visual order.
                    let show_labels = ui.available_width() > 820.0;
                    let theme_label = strings.get(MessageId::Theme);
                    let language_label = strings.get(MessageId::Language);
                    let spacing = ui.spacing().item_spacing.x;
                    let label_width = |ui: &egui::Ui, text: &str| {
                        if show_labels {
                            ui.painter()
                                .layout_no_wrap(
                                    text.to_owned(),
                                    egui::TextStyle::Body.resolve(ui.style()),
                                    ui.visuals().text_color(),
                                )
                                .size()
                                .x
                                + spacing
                        } else {
                            0.0
                        }
                    };
                    let cluster_width = 2.0 * (PREFERENCE_COMBO_WIDTH + spacing)
                        + label_width(ui, theme_label)
                        + label_width(ui, language_label);

                    // The scan-root display is truncated so a long path can
                    // never push the preference cluster out of the rail.
                    let location_width = ui.available_width() - cluster_width - 16.0;
                    if location_width > 120.0 {
                        ui.scope(|ui| {
                            ui.set_max_width(location_width);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(
                                        location
                                            .as_deref()
                                            .unwrap_or_else(|| strings.get(MessageId::NoLocation)),
                                    )
                                    .weak()
                                    .monospace(),
                                )
                                .truncate(),
                            );
                        });
                    }
                    ui.add_space((ui.available_width() - cluster_width - 8.0).max(0.0));

                    let theme_combo = egui::ComboBox::from_id_salt("theme-preference")
                        .width(PREFERENCE_COMBO_WIDTH)
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
                    label_combo(ui, theme_combo.response, theme_label, show_labels);

                    let language_combo = egui::ComboBox::from_id_salt("language")
                        .width(PREFERENCE_COMBO_WIDTH)
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
                    label_combo(ui, language_combo.response, language_label, show_labels);
                });
            },
        );

        ui.ctx().set_theme(*theme_preference);
        self.select_locale(selected_locale);
        for command in commands {
            self.dispatch(command);
        }
    }

    fn telemetry_rail(&mut self, ui: &mut egui::Ui) {
        let mut commands = Vec::new();
        let state = self.flow.state();
        let phase = self.strings.get(self.phase_message());
        let phase_color = phase_color(state, ui.visuals().dark_mode);
        let progress = self.runtime.as_ref().and_then(RuntimeController::progress);
        let counters = progress.map(SessionProgress::effective).unwrap_or_default();
        let selected_bytes = self
            .selected_node_record()
            .map(|record| format_iec_bytes(node_known_bytes(record, self.metric), self.locale));
        let unknown = self.view_root_record().map_or(0, |record| match self.metric {
            SizeBasis::Logical => record.aggregate().logical().unknown_entries(),
            SizeBasis::Allocated => record.aggregate().allocated().unknown_entries(),
        });
        let hidden = self
            .runtime
            .as_ref()
            .and_then(RuntimeController::navigation)
            .map_or(0, diskpie_app::navigation::NavigationState::hidden_branch_count);
        let restore_all = self.availability(NavigationAction::RestoreAllBranches);
        let items = self.item_count(counters.entries);
        let hidden_label = if hidden > 0 {
            self.i18n
                .hidden_group(u64::try_from(hidden).unwrap_or(u64::MAX))
                .unwrap_or_else(|_| hidden.to_string())
        } else {
            String::new()
        };
        let files = counters.files.to_string();
        let folders = counters.directories.to_string();
        let omissions = counters.omissions.to_string();
        let unknown = unknown.to_string();
        let metric = self.metric;
        let strings = &self.strings;

        egui::Panel::top("telemetry-rail").exact_size(44.0).show_separator_line(true).show(
            ui,
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(8.0);
                    status_pill(ui, phase, phase_color);
                    ui.separator();
                    metric_value(
                        ui,
                        strings.get(MessageId::Selected),
                        selected_bytes.as_deref().unwrap_or("—"),
                    );
                    metric_value(ui, strings.get(MessageId::Files), &files);
                    metric_value(ui, strings.get(MessageId::Folders), &folders);
                    metric_value(ui, strings.get(MessageId::OmittedItems), &omissions);
                    metric_value(ui, strings.get(MessageId::UnknownSize), &unknown);
                    ui.label(RichText::new(&items).weak().monospace());
                    ui.separator();
                    let logical = ui.add(
                        egui::Button::new(strings.get(MessageId::LogicalSize))
                            .selected(metric == SizeBasis::Logical),
                    );
                    if logical.clicked() {
                        commands.push(ShellCommand::SetMetric(SizeBasis::Logical));
                    }
                    let allocated = ui.add(
                        egui::Button::new(strings.get(MessageId::AllocatedData))
                            .selected(metric == SizeBasis::Allocated),
                    );
                    if allocated.clicked() {
                        commands.push(ShellCommand::SetMetric(SizeBasis::Allocated));
                    }
                    if hidden > 0 {
                        ui.separator();
                        ui.label(RichText::new(&hidden_label).weak().monospace());
                        if command_button(
                            ui,
                            strings,
                            MessageId::RestoreAllBranches,
                            restore_all,
                            "",
                        ) {
                            commands
                                .push(ShellCommand::Navigate(NavigationAction::RestoreAllBranches));
                        }
                    }
                });
            },
        );
        for command in commands {
            self.dispatch(command);
        }
    }

    fn selected_node_record(&self) -> Option<&NodeRecord> {
        let runtime = self.runtime.as_ref()?;
        let selected = runtime.navigation()?.selected()?;
        runtime.snapshot()?.node(selected)
    }

    fn view_root_record(&self) -> Option<&NodeRecord> {
        let runtime = self.runtime.as_ref()?;
        let root = runtime.navigation()?.view_root();
        runtime.snapshot()?.node(root)
    }

    fn workspace(&mut self, ui: &mut egui::Ui) {
        let wide = ui.available_width() >= 900.0;
        if wide {
            egui::Panel::right("inspection-rail")
                .resizable(true)
                .default_size(360.0)
                .min_size(300.0)
                .max_size(480.0)
                .show_separator_line(true)
                .show(ui, |ui| self.inspector(ui));
            egui::CentralPanel::default().show(ui, |ui| self.lens(ui));
        } else {
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("narrow-workspace").show(ui, |ui| {
                    self.lens(ui);
                    ui.separator();
                    self.inspector(ui);
                });
            });
        }
    }

    /// The radial lens: calibration rings while empty, the real chart otherwise.
    fn lens(&mut self, ui: &mut egui::Ui) {
        let state = self.flow.state();
        let has_layout = self.runtime.as_ref().is_some_and(|runtime| runtime.layout().is_some());
        // Earlier results stay visible while a dialog or resolution is
        // outstanding; the phase pill and status rail carry the state text.
        if state == ScanUi::Empty || !has_layout {
            self.calibration_lens(ui, state);
        } else {
            self.chart_lens(ui, state);
        }
    }

    fn calibration_lens(&mut self, ui: &mut egui::Ui, state: ScanUi) {
        let mut commands = Vec::new();
        let available = ui.available_size();
        let diameter = available.x.min(available.y - 180.0).clamp(180.0, 520.0);
        let (center_text, sub_text) = match state {
            ScanUi::Choosing => (MessageId::Choosing, MessageId::ChartEmptyTooltip),
            ScanUi::Resolving => (MessageId::Resolving, MessageId::ChartEmptyTooltip),
            ScanUi::Scanning | ScanUi::Partial { .. } | ScanUi::Cancelling => {
                (MessageId::Scanning, MessageId::ChartEmptyTooltip)
            }
            ScanUi::Failed { .. } => (MessageId::Failed, MessageId::ScanFailedTitle),
            ScanUi::Empty | ScanUi::Complete | ScanUi::CancelledWithResults => {
                (MessageId::ChooseFolderCenter, MessageId::OrDrive)
            }
        };
        ui.vertical_centered(|ui| {
            let (response, painter) = ui.allocate_painter(Vec2::splat(diameter), Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Other,
                    true,
                    self.strings.get(MessageId::ChartAccessibilityLabel),
                )
            });
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
                self.strings.get(center_text),
                FontId::proportional(17.0),
                ui.visuals().strong_text_color(),
            );
            painter.text(
                center + egui::vec2(0.0, 15.0),
                egui::Align2::CENTER_CENTER,
                self.strings.get(sub_text),
                FontId::proportional(13.0),
                ui.visuals().weak_text_color(),
            );

            if response.clicked() && state == ScanUi::Empty && self.picker.is_some() {
                commands.push(ShellCommand::ChooseFolder);
            }
        });

        if let ScanUi::Failed { class } = state {
            ui.add_space(8.0);
            ui.label(RichText::new(self.strings.get(failure_message(class))).strong());
            if let Notice::Text(text) = &self.notice {
                ui.label(RichText::new(text).weak().monospace());
            }
        }
        if state == ScanUi::Choosing || self.picker.is_none() {
            ui.add_space(8.0);
            ui.label(RichText::new(self.strings.get(MessageId::FolderPickerUnavailable)).weak());
        }
        ui.add_space(12.0);
        self.volume_list(ui, &mut commands, state);
        for command in commands {
            self.dispatch(command);
        }
    }

    fn volume_list(&self, ui: &mut egui::Ui, commands: &mut Vec<ShellCommand>, state: ScanUi) {
        let busy = state == ScanUi::Choosing || state == ScanUi::Resolving;
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.heading(self.strings.get(MessageId::Volumes));
            let refresh = ui.add_enabled(
                self.volumes_job.is_none(),
                egui::Button::new(self.strings.get(MessageId::RefreshVolumes)),
            );
            if refresh.clicked() {
                commands.push(ShellCommand::RefreshVolumes);
            }
            let has_volumes = self.volumes.as_ref().is_some_and(|list| list.volumes.len() > 1);
            let all = ui.add_enabled(
                has_volumes && !busy,
                egui::Button::new(self.strings.get(MessageId::ScanAllVolumes)),
            );
            if all.clicked() {
                commands.push(ShellCommand::ScanAllVolumes);
            }
        });
        let Some(list) = self.volumes.as_ref() else {
            ui.label(RichText::new(self.strings.get(MessageId::Resolving)).weak());
            return;
        };
        if list.volumes.is_empty() {
            ui.label(RichText::new(self.strings.get(MessageId::NoVolumesFound)).weak());
        }
        for volume in &list.volumes {
            let mut text = volume.display.clone();
            if let Some(label) = volume.label.as_deref().filter(|label| !label.is_empty()) {
                text.push_str("  ");
                text.push_str(label);
            }
            if let Some(filesystem) = volume.filesystem.as_deref() {
                text.push_str("  ·  ");
                text.push_str(filesystem);
            }
            if let (Some(free), Some(total)) = (volume.free_bytes, volume.total_bytes) {
                text.push_str("  ·  ");
                text.push_str(&format_iec_bytes(u128::from(free), self.locale));
                text.push(' ');
                text.push_str(self.strings.get(MessageId::Free));
                text.push_str("  ·  ");
                text.push_str(&format_iec_bytes(u128::from(total), self.locale));
                text.push(' ');
                text.push_str(self.strings.get(MessageId::Total));
            }
            let response = ui.add_enabled(
                !busy,
                egui::Button::new(RichText::new(text).monospace()).min_size(Vec2::new(360.0, 0.0)),
            );
            if response.clicked() {
                commands.push(ShellCommand::ScanPaths(vec![volume.path.clone()]));
            }
            for issue in volume.issues.iter().take(3) {
                ui.label(RichText::new(issue).weak().small());
            }
        }
        if !list.errors.is_empty() {
            ui.add_space(4.0);
            ui.label(RichText::new(self.strings.get(MessageId::DiscoveryIssues)).weak());
            for error in list.errors.iter().take(8) {
                ui.label(RichText::new(error).weak().small());
            }
        }
    }

    fn chart_lens(&mut self, ui: &mut egui::Ui, state: ScanUi) {
        let mut commands = Vec::new();
        {
            let Self {
                runtime,
                mesh_cache,
                mesh_options,
                i18n,
                strings,
                locale,
                focused,
                context_node,
                path_scratch,
                metric,
                ..
            } = self;
            let Some(runtime) = runtime.as_ref() else {
                return;
            };
            let (Some(layout), Some(presentation), Some(navigation), Some(snapshot)) = (
                runtime.layout(),
                runtime.presentation(),
                runtime.navigation(),
                runtime.snapshot(),
            ) else {
                return;
            };
            let selected = navigation.selected();
            let color_key = |node: NodeId| presentation.path_color_key(node).unwrap_or(0);
            let available = ui.available_size();
            let side = available.x.min(available.y).max(120.0);
            let phase_text = strings.get(phase_message(state));
            let tooltip_context = TooltipContext {
                snapshot,
                i18n,
                strings,
                locale: *locale,
                basis: *metric,
                phase_text,
            };
            let response = ui
                .vertical_centered(|ui| {
                    SunburstView::new(layout, mesh_cache)
                        .selected(selected)
                        .focused(*focused)
                        .desired_size(Vec2::splat(side))
                        .mesh_options(*mesh_options)
                        .color_key(&color_key)
                        .accessibility_label(strings.get(MessageId::ChartAccessibilityLabel))
                        .show_with_tooltip(ui, |ui, tooltip| {
                            chart_tooltip(ui, tooltip, &tooltip_context, path_scratch);
                        })
                })
                .inner;

            let requests = response.requests;
            match requests.selection {
                Some(NodeChange::Set(node)) => {
                    commands.push(ShellCommand::Navigate(NavigationAction::Select(node)));
                }
                Some(NodeChange::Clear) => {
                    commands.push(ShellCommand::Navigate(NavigationAction::ClearSelection));
                }
                None => {}
            }
            if let Some(node) = requests.zoom {
                commands.push(ShellCommand::Navigate(NavigationAction::Activate(node)));
            }
            match requests.focus {
                Some(NodeChange::Set(node)) => commands.push(ShellCommand::Focus(Some(node))),
                Some(NodeChange::Clear) => commands.push(ShellCommand::Focus(None)),
                None => {}
            }
            // A secondary click opens egui's context menu by itself; the
            // keyboard request (Shift+F10) opens the same popup explicitly.
            if let Some(node) = requests.context_menu {
                *context_node = Some(node);
                if !response.widget.secondary_clicked() {
                    egui::Popup::open_id(
                        ui.ctx(),
                        egui::Popup::default_response_id(&response.widget),
                    );
                }
            }
            let menu_node = *context_node;
            let menu_response = response.widget.context_menu(|ui| {
                if let Some(node) = menu_node {
                    branch_menu(ui, strings, navigation, node, &mut commands);
                } else {
                    ui.close();
                }
            });
            if menu_response.is_none() {
                // The node is remembered only while its menu is visible.
                *context_node = None;
            }

            if let ScanUi::Failed { class } = state {
                ui.label(RichText::new(strings.get(failure_message(class))).strong());
            }
        }
        for command in commands {
            self.dispatch(command);
        }
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        let mut commands = Vec::new();
        let state = self.flow.state();
        let counters = self
            .runtime
            .as_ref()
            .and_then(RuntimeController::progress)
            .map(SessionProgress::effective)
            .unwrap_or_default();
        let item_count = self.item_count(counters.entries);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.heading(self.strings.get(MessageId::LargestItems));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(item_count).weak().monospace());
            });
        });
        self.largest_list(ui, &mut commands);
        ui.add_space(12.0);
        ui.separator();
        self.details(ui, state, &mut commands);
        ui.add_space(16.0);
        ui.separator();
        ui.label(RichText::new(self.strings.get(MessageId::Safety)).strong());
        ui.label(RichText::new(self.strings.get(MessageId::ActionsUnavailable)).weak());
        ui.horizontal_wrapped(|ui| {
            for id in [
                MessageId::Open,
                MessageId::Reveal,
                MessageId::Recycle,
                MessageId::DeletePermanently,
            ] {
                ui.add_enabled(false, egui::Button::new(self.strings.get(id)))
                    .on_disabled_hover_text(self.strings.get(MessageId::ActionsUnavailable));
            }
        });
        for command in commands {
            self.dispatch(command);
        }
    }

    fn refresh_list_cache(&mut self) {
        let Some(runtime) = self.runtime.as_ref() else {
            self.list_cache = ListCache::default();
            return;
        };
        let (Some(snapshot), Some(navigation), Some(revision)) =
            (runtime.snapshot(), runtime.navigation(), runtime.displayed_revision())
        else {
            self.list_cache = ListCache::default();
            return;
        };
        let key = (revision, navigation.view_root(), navigation.size_basis());
        if self.list_cache.key == Some(key) {
            return;
        }
        let rows = diskpie_app::presentation::largest_children(
            snapshot,
            navigation.view_root(),
            navigation.size_basis(),
            LARGEST_CHILDREN_LIMIT,
        )
        .unwrap_or_default();
        self.list_cache = ListCache { key: Some(key), rows };
    }

    /// Keyboard-operable, virtualized companion list synchronized with the chart.
    fn largest_list(&mut self, ui: &mut egui::Ui, commands: &mut Vec<ShellCommand>) {
        self.refresh_list_cache();
        let Some(runtime) = self.runtime.as_ref() else {
            ui.label(RichText::new(self.strings.get(MessageId::EmptyItemsHint)).weak());
            return;
        };
        let (Some(snapshot), Some(navigation)) = (runtime.snapshot(), runtime.navigation()) else {
            ui.label(RichText::new(self.strings.get(MessageId::EmptyItemsHint)).weak());
            return;
        };
        if self.list_cache.rows.is_empty() {
            ui.label(RichText::new(self.strings.get(MessageId::EmptyItemsHint)).weak());
            return;
        }
        let basis = navigation.size_basis();
        let selected = navigation.selected();
        let root_bytes = snapshot
            .node(navigation.view_root())
            .map_or(0, |record| node_known_bytes(record, basis));
        let rows = &self.list_cache.rows;
        let strings = &self.strings;
        let locale = self.locale;
        let scroll_to_selection = std::mem::take(&mut self.scroll_to_selection);
        let hint = strings.get(MessageId::ListKeyboardHint);
        ui.label(RichText::new(hint).weak().small());
        let row_height = ui.spacing().interact_size.y;
        let list_height = (ui.available_height() * 0.45).clamp(120.0, 420.0);
        let enter = ui.input(|input| input.key_pressed(Key::Enter));
        egui::ScrollArea::vertical()
            .id_salt("largest-children")
            .max_height(list_height)
            .auto_shrink([false, true])
            .show_rows(ui, row_height, rows.len(), |ui, range| {
                // Justified rows fill the width and keep the text left-aligned
                // so names, sizes, and shares line up as columns.
                ui.with_layout(Layout::top_down_justified(Align::Min), |ui| {
                    for index in range {
                        let node = rows[index];
                        let Some(record) = snapshot.node(node) else {
                            continue;
                        };
                        let bytes = node_known_bytes(record, basis);
                        let size = format_iec_bytes(bytes, locale);
                        let percent = format_percent(fraction(bytes, root_bytes));
                        let name = display_name(record);
                        let is_selected = selected == Some(node);
                        let label = format!("{name}  {size}  {percent}");
                        let response = ui.add(
                            egui::Button::selectable(
                                is_selected,
                                RichText::new(&label).monospace(),
                            )
                            .min_size(Vec2::new(0.0, row_height)),
                        );
                        response.widget_info(|| {
                            egui::WidgetInfo::selected(
                                egui::WidgetType::SelectableLabel,
                                true,
                                is_selected,
                                &label,
                            )
                        });
                        if is_selected && scroll_to_selection {
                            response.scroll_to_me(Some(Align::Center));
                        }
                        let zoom = response.has_focus() && enter;
                        if zoom || response.double_clicked() {
                            commands.push(ShellCommand::Navigate(NavigationAction::Activate(node)));
                        } else if response.clicked() {
                            commands.push(ShellCommand::Navigate(NavigationAction::Select(node)));
                            commands.push(ShellCommand::Focus(Some(node)));
                        }
                    }
                });
            });
    }

    fn details(&self, ui: &mut egui::Ui, state: ScanUi, commands: &mut Vec<ShellCommand>) {
        let strings = &self.strings;
        let runtime = self.runtime.as_ref();
        let snapshot = runtime.and_then(RuntimeController::snapshot);
        let navigation = runtime.and_then(RuntimeController::navigation);
        let subject = navigation
            .and_then(|navigation| navigation.selected().or_else(|| Some(navigation.view_root())));
        let record = subject.and_then(|node| snapshot.and_then(|snapshot| snapshot.node(node)));
        let path = match (snapshot, subject) {
            (Some(snapshot), Some(node)) => snapshot
                .path(node)
                .ok()
                .filter(|path| !path.as_os_str().is_empty())
                .map(|path| format_path_for_display(&path))
                .or_else(|| record.map(display_name)),
            _ => None,
        };
        let path = path.unwrap_or_else(|| strings.get(MessageId::NoSelection).to_owned());
        let aggregate = record.map(NodeRecord::aggregate);
        let logical = aggregate.map_or_else(
            || "—".to_owned(),
            |aggregate| format_iec_bytes(aggregate.logical().known_bytes(), self.locale),
        );
        let allocated = aggregate.map_or_else(
            || "—".to_owned(),
            |aggregate| format_iec_bytes(aggregate.allocated().known_bytes(), self.locale),
        );
        let files = aggregate.map_or_else(|| "0".to_owned(), |a| a.file_count().to_string());
        let folders = aggregate.map_or_else(|| "0".to_owned(), |a| a.directory_count().to_string());
        let scan_state = if runtime.and_then(RuntimeController::snapshot).is_none() {
            strings.get(MessageId::NotStarted)
        } else {
            strings.get(phase_message(state))
        };

        let mut rows: Vec<(&str, &str)> = vec![(strings.get(MessageId::CurrentPath), &path)];
        match (self.show_both_sizes, self.metric) {
            (true, _) => {
                rows.push((strings.get(MessageId::LogicalSize), &logical));
                rows.push((strings.get(MessageId::AllocatedData), &allocated));
            }
            (false, SizeBasis::Logical) => {
                rows.push((strings.get(MessageId::LogicalSize), &logical))
            }
            (false, SizeBasis::Allocated) => {
                rows.push((strings.get(MessageId::AllocatedData), &allocated));
            }
        }
        rows.push((strings.get(MessageId::FileCount), &files));
        rows.push((strings.get(MessageId::FolderCount), &folders));
        rows.push((strings.get(MessageId::ScanState), scan_state));
        for (label, value) in rows {
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).weak());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(value).monospace()).truncate());
                });
            });
        }
        if aggregate.is_some_and(|aggregate| !aggregate.has_unique_allocation_precision()) {
            ui.label(RichText::new(strings.get(MessageId::AllocationEstimate)).weak());
        }
        if !self.resolve_issues.is_empty() {
            ui.add_space(4.0);
            ui.label(RichText::new(strings.get(MessageId::DiscoveryIssues)).weak());
            for issue in self.resolve_issues.iter().take(4) {
                ui.label(RichText::new(issue).weak().small());
            }
        }

        if let (Some(node), Some(navigation)) = (subject, navigation) {
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                let hide =
                    navigation.availability(navigation.command(NavigationAction::HideBranch(node)));
                if command_button(ui, strings, MessageId::HideBranch, hide, "") {
                    commands.push(ShellCommand::Navigate(NavigationAction::HideBranch(node)));
                }
                let restore = navigation
                    .availability(navigation.command(NavigationAction::RestoreBranch(node)));
                if command_button(ui, strings, MessageId::RestoreBranch, restore, "") {
                    commands.push(ShellCommand::Navigate(NavigationAction::RestoreBranch(node)));
                }
                ui.add_enabled(false, egui::Button::new(strings.get(MessageId::RescanBranch)))
                    .on_disabled_hover_text(strings.get(MessageId::RescanBranchUnavailable));
            });
        }
    }

    fn status_rail(&self, ui: &mut egui::Ui) {
        let height = if self.previous_session_panicked || self.optional_services_unavailable {
            56.0
        } else {
            34.0
        };
        egui::Panel::bottom("status-rail").exact_size(height).show_separator_line(true).show(
            ui,
            |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.add(
                            egui::Label::new(RichText::new(self.notice_text()).weak()).truncate(),
                        );
                        if self.previous_session_panicked {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(
                                        self.strings
                                            .get(MessageId::PreviousSessionEndedUnexpectedly),
                                    )
                                    .color(ui.visuals().warn_fg_color),
                                )
                                .truncate(),
                            );
                        } else if self.optional_services_unavailable {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(
                                        self.strings.get(MessageId::OptionalServicesUnavailable),
                                    )
                                    .weak(),
                                )
                                .truncate(),
                            );
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(self.strings.get(MessageId::NoElevation)).weak());
                        ui.label(RichText::new("·").weak());
                        ui.label(RichText::new(self.strings.get(MessageId::StandardUser)).weak());
                    });
                });
            },
        );
    }

    #[cfg(windows)]
    fn refresh_owner(&mut self, frame: &eframe::Frame) {
        if self.owner.is_some() {
            return;
        }
        self.owner = owner_window(frame);
    }
}

/// Owner HWND obtained through eframe's safe raw-window-handle API.
#[cfg(windows)]
fn owner_window(frame: &eframe::Frame) -> Option<OwnerWindow> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => OwnerWindow::from_raw(handle.hwnd.get()),
        _ => None,
    }
}

impl eframe::App for DiskPieShell {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        #[cfg(windows)]
        self.refresh_owner(frame);
        #[cfg(not(windows))]
        let _ = frame;
        let ctx = ui.ctx().clone();
        if ctx.input(|input| input.viewport().close_requested()) {
            self.begin_shutdown();
        }
        self.logic(&ctx);
        self.handle_shortcuts(&ctx);
        self.command_rail(ui);
        self.telemetry_rail(ui);
        self.synchronize_settings();
        self.status_rail(ui);
        self.workspace(ui);
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.begin_shutdown();
    }
}

impl Drop for DiskPieShell {
    fn drop(&mut self) {
        self.begin_shutdown();
        self.exit.publish(ShellExit {
            settings: self.settings.clone(),
            runtime: self.runtime.take(),
            picker: self.picker.take(),
        });
    }
}

// --------------------------------------------------------------------------
// Pure helpers
// --------------------------------------------------------------------------

/// Builds the runtime policy from validated settings.
#[must_use]
pub fn runtime_config_from_settings(settings: &Settings) -> RuntimeConfig {
    let mut config = RuntimeConfig::default();
    if settings.scan_workers > 0 {
        config.scan_options.worker_count = usize::from(settings.scan_workers);
    }
    let mut layout = LayoutOptions::new(NodeId::from_raw(0));
    layout.size_basis = basis_from_setting(settings.size_preference);
    layout.max_depth = u32::from(settings.layout_max_depth);
    layout.minimum_sweep = settings.layout_minimum_sweep;
    layout.max_sectors = usize::try_from(settings.layout_max_sectors).unwrap_or(usize::MAX);
    config.layout_options = layout;
    config
}

/// Keyboard shortcuts owned by the shell (the chart handles its own keys).
pub const SHORTCUTS: [(Modifiers, Key); 6] = [
    (Modifiers::NONE, Key::Backspace),
    (Modifiers::ALT, Key::ArrowLeft),
    (Modifiers::ALT, Key::ArrowUp),
    (Modifiers::NONE, Key::F5),
    (Modifiers::NONE, Key::Escape),
    (Modifiers::COMMAND, Key::O),
];

/// Maps one shortcut to a command. Escape is claimed only while cancelling
/// makes sense so the chart keeps it for clearing its selection otherwise.
#[must_use]
pub fn shortcut_command(modifiers: Modifiers, key: Key, can_cancel: bool) -> Option<ShellCommand> {
    match (modifiers, key) {
        (Modifiers::NONE, Key::Backspace) | (Modifiers::ALT, Key::ArrowLeft) => {
            Some(ShellCommand::Navigate(NavigationAction::Back))
        }
        (Modifiers::ALT, Key::ArrowUp) => Some(ShellCommand::Navigate(NavigationAction::Parent)),
        (Modifiers::NONE, Key::F5) => Some(ShellCommand::Navigate(NavigationAction::RescanAll)),
        (Modifiers::NONE, Key::Escape) => can_cancel.then_some(ShellCommand::Cancel),
        (Modifiers::COMMAND, Key::O) => Some(ShellCommand::ChooseFolder),
        _ => None,
    }
}

/// Phase text for each visible state.
#[must_use]
pub const fn phase_message(state: ScanUi) -> MessageId {
    match state {
        ScanUi::Empty => MessageId::Ready,
        ScanUi::Choosing => MessageId::Choosing,
        ScanUi::Resolving => MessageId::Resolving,
        ScanUi::Scanning => MessageId::Scanning,
        ScanUi::Partial { settled: false } => MessageId::PartialResults,
        ScanUi::Partial { settled: true } => MessageId::PartialSettled,
        ScanUi::Cancelling => MessageId::Cancelling,
        ScanUi::Complete => MessageId::Complete,
        ScanUi::CancelledWithResults => MessageId::Cancelled,
        ScanUi::Failed { .. } => MessageId::Failed,
    }
}

/// Explanation text for a typed failure class.
#[must_use]
pub const fn failure_message(class: FailureClass) -> MessageId {
    match class {
        FailureClass::PermissionDenied => MessageId::PermissionDenied,
        FailureClass::PathUnavailable => MessageId::PathUnavailable,
        FailureClass::Other => MessageId::ScanFailedOther,
    }
}

/// Hover text for every typed unavailability reason.
#[must_use]
pub const fn reason_message(reason: UnavailableReason) -> MessageId {
    match reason {
        UnavailableReason::StaleGeneration { .. } => MessageId::ReasonStaleGeneration,
        UnavailableReason::UnknownNode { .. } => MessageId::ReasonUnknownNode,
        UnavailableReason::SyntheticTarget { .. } => MessageId::ReasonSyntheticTarget,
        UnavailableReason::NotNavigable { .. } => MessageId::ReasonNotNavigable,
        UnavailableReason::NotFilesystemRoot { .. } => MessageId::ReasonNotFilesystemRoot,
        UnavailableReason::NoHistory => MessageId::NoBackHistory,
        UnavailableReason::NoParent => MessageId::NoParent,
        UnavailableReason::NoSummary => MessageId::ReasonNoSummary,
        UnavailableReason::NoSelection => MessageId::NoSelection,
        UnavailableReason::FilesystemRootCannotBeHidden { .. } => {
            MessageId::ReasonRootCannotBeHidden
        }
        UnavailableReason::ViewRootCannotBeHidden { .. } => MessageId::ReasonViewRootCannotBeHidden,
        UnavailableReason::AlreadyHidden { .. } => MessageId::ReasonAlreadyHidden,
        UnavailableReason::NotHidden { .. } => MessageId::ReasonNotHidden,
        UnavailableReason::NoHiddenBranches => MessageId::ReasonNoHiddenBranches,
        UnavailableReason::HiddenStateBusy { .. } => MessageId::ReasonHiddenStateBusy,
        UnavailableReason::HiddenTarget { .. } => MessageId::ReasonHiddenTarget,
        UnavailableReason::NoRescanRoots => MessageId::ReasonNoRescanRoots,
        UnavailableReason::NoRealPath { .. } => MessageId::ReasonNoRealPath,
    }
}

fn phase_color(state: ScanUi, dark_mode: bool) -> Color32 {
    match state {
        ScanUi::Empty | ScanUi::Choosing | ScanUi::Resolving | ScanUi::Scanning => {
            theme::SCAN_CURRENT
        }
        ScanUi::Partial { .. } | ScanUi::Cancelling | ScanUi::CancelledWithResults => {
            theme::SECTOR_EMBER
        }
        ScanUi::Complete => {
            if dark_mode {
                theme::SEPARATOR_PORCELAIN
            } else {
                theme::PLATTER_MIDNIGHT
            }
        }
        ScanUi::Failed { .. } => theme::DANGER_STOP,
    }
}

/// Node bytes in the requested basis (known part only; unknown is not zero
/// and is reported separately).
fn node_known_bytes(record: &NodeRecord, basis: SizeBasis) -> u128 {
    match basis {
        SizeBasis::Logical => record.aggregate().logical().known_bytes(),
        SizeBasis::Allocated => record.aggregate().allocated().known_bytes(),
    }
}

fn fraction(part: u128, whole: u128) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    let scaled = part.saturating_mul(10_000) / whole;
    u32::try_from(scaled).map_or(1.0, |value| f64::from(value) / 10_000.0)
}

/// Formats a fraction of one as a percentage with one decimal.
#[must_use]
pub fn format_percent(fraction: f64) -> String {
    let clamped = if fraction.is_finite() { fraction.clamp(0.0, 1.0) } else { 0.0 };
    let tenths = (clamped * 1_000.0).round() as u32;
    format!("{}.{} %", tenths / 10, tenths % 10)
}

/// Display name of a node. A filesystem root displays as its path spelling.
fn display_name(record: &NodeRecord) -> String {
    if record.kind() == EntryKind::Root {
        format_path_for_display(std::path::Path::new(record.name()))
    } else {
        record.name().to_string_lossy().into_owned()
    }
}

struct TooltipContext<'a> {
    snapshot: &'a TreeSnapshot,
    i18n: &'a I18n,
    strings: &'a UiStrings,
    locale: Locale,
    basis: SizeBasis,
    phase_text: &'a str,
}

fn chart_tooltip(
    ui: &mut egui::Ui,
    tooltip: SunburstTooltip,
    context: &TooltipContext<'_>,
    scratch: &mut PathBuf,
) {
    let strings = context.strings;
    let share = tooltip.root_fraction.map(format_percent);
    match tooltip.sector.kind {
        SunburstHitKind::Real { node_id } => {
            let Some(record) = context.snapshot.node(node_id) else {
                return;
            };
            let path = if context.snapshot.path_into(node_id, scratch).is_ok()
                && !scratch.as_os_str().is_empty()
            {
                format_path_for_display(scratch)
            } else {
                display_name(record)
            };
            let aggregate = record.aggregate();
            ui.label(RichText::new(path).strong().monospace());
            ui.label(format!(
                "{}: {}",
                strings.get(MessageId::LogicalSize),
                format_iec_bytes(aggregate.logical().known_bytes(), context.locale)
            ));
            ui.label(format!(
                "{}: {}",
                strings.get(MessageId::AllocatedData),
                format_iec_bytes(aggregate.allocated().known_bytes(), context.locale)
            ));
            ui.label(format!("{}: {}", strings.get(MessageId::FileCount), aggregate.file_count()));
            ui.label(format!(
                "{}: {}",
                strings.get(MessageId::FolderCount),
                aggregate.directory_count()
            ));
            let unknown = match context.basis {
                SizeBasis::Logical => aggregate.logical().unknown_entries(),
                SizeBasis::Allocated => aggregate.allocated().unknown_entries(),
            };
            if aggregate.omission_count() > 0 || unknown > 0 {
                ui.label(format!(
                    "{}: {}  ·  {}: {}",
                    strings.get(MessageId::OmittedItems),
                    aggregate.omission_count(),
                    strings.get(MessageId::UnknownSize),
                    unknown
                ));
            }
            if !aggregate.has_unique_allocation_precision() {
                ui.label(RichText::new(strings.get(MessageId::AllocationEstimate)).weak());
            }
        }
        SunburstHitKind::UnavailableReal => {
            ui.label(strings.get(MessageId::ReasonUnknownNode));
        }
        SunburstHitKind::Hidden { member_count } => {
            let count = u64::try_from(member_count).unwrap_or(u64::MAX);
            ui.label(RichText::new(context.i18n.hidden_group(count).unwrap_or_default()).strong());
        }
        SunburstHitKind::Other { member_count, .. } => {
            let count = u64::try_from(member_count).unwrap_or(u64::MAX);
            ui.label(RichText::new(context.i18n.other_group(count).unwrap_or_default()).strong());
        }
    }
    if let Some(share) = share {
        ui.label(format!("{share} {}", strings.get(MessageId::ShareOfView)));
    }
    ui.label(
        RichText::new(format!("{}: {}", strings.get(MessageId::ScanState), context.phase_text))
            .weak(),
    );
}

fn branch_menu(
    ui: &mut egui::Ui,
    strings: &UiStrings,
    navigation: &diskpie_app::navigation::NavigationState,
    node: NodeId,
    commands: &mut Vec<ShellCommand>,
) {
    ui.set_min_width(200.0);
    let hide = navigation.availability(navigation.command(NavigationAction::HideBranch(node)));
    if command_button(ui, strings, MessageId::HideBranch, hide, "") {
        commands.push(ShellCommand::Navigate(NavigationAction::HideBranch(node)));
        ui.close();
    }
    let restore =
        navigation.availability(navigation.command(NavigationAction::RestoreBranch(node)));
    if command_button(ui, strings, MessageId::RestoreBranch, restore, "") {
        commands.push(ShellCommand::Navigate(NavigationAction::RestoreBranch(node)));
        ui.close();
    }
    ui.add_enabled(false, egui::Button::new(strings.get(MessageId::RescanBranch)))
        .on_disabled_hover_text(strings.get(MessageId::RescanBranchUnavailable));
    ui.separator();
    for id in [MessageId::Open, MessageId::Reveal, MessageId::Recycle, MessageId::DeletePermanently]
    {
        ui.add_enabled(false, egui::Button::new(strings.get(id)))
            .on_disabled_hover_text(strings.get(MessageId::ActionsUnavailable));
    }
}

/// A button whose enabled state and disabled hover text follow availability.
fn command_button(
    ui: &mut egui::Ui,
    strings: &UiStrings,
    label: MessageId,
    availability: CommandAvailability,
    shortcut: &str,
) -> bool {
    let mut response =
        ui.add_enabled(availability.is_available(), egui::Button::new(strings.get(label)));
    if !shortcut.is_empty() {
        response = response.on_hover_text(shortcut);
    }
    if let Some(reason) = availability.reason() {
        response = response.on_disabled_hover_text(strings.get(reason_message(reason)));
    }
    response.clicked()
}

#[cfg(windows)]
fn shell_failed_event(request_id: u64, class: ErrorClass) -> DiagnosticEvent {
    let mut event = DiagnosticEvent::new(DiagnosticCode::ShellActionFailed);
    let _request = event.push_field(DiagnosticField::RequestId(request_id));
    let _class = event.push_field(DiagnosticField::ErrorClass(class));
    event
}

fn classified_event(code: DiagnosticCode, class: ErrorClass) -> DiagnosticEvent {
    let mut event = DiagnosticEvent::new(code);
    let _class = event.push_field(DiagnosticField::ErrorClass(class));
    event
}

const fn locale_from_setting(locale: SettingsLocale) -> Locale {
    match locale {
        SettingsLocale::EnglishUnitedStates => Locale::EnglishUnitedStates,
        SettingsLocale::SpanishMexico => Locale::SpanishMexico,
    }
}

const fn setting_from_locale(locale: Locale) -> SettingsLocale {
    match locale {
        Locale::EnglishUnitedStates => SettingsLocale::EnglishUnitedStates,
        Locale::SpanishMexico => SettingsLocale::SpanishMexico,
    }
}

const fn theme_from_setting(theme: SettingsTheme) -> ThemePreference {
    match theme {
        SettingsTheme::System => ThemePreference::System,
        SettingsTheme::Dark => ThemePreference::Dark,
        SettingsTheme::Light => ThemePreference::Light,
    }
}

const fn setting_from_theme(theme: ThemePreference) -> SettingsTheme {
    match theme {
        ThemePreference::System => SettingsTheme::System,
        ThemePreference::Dark => SettingsTheme::Dark,
        ThemePreference::Light => SettingsTheme::Light,
    }
}

const fn basis_from_setting(preference: SizePreference) -> SizeBasis {
    match preference {
        SizePreference::Logical => SizeBasis::Logical,
        SizePreference::Allocated => SizeBasis::Allocated,
    }
}

const fn setting_from_basis(basis: SizeBasis) -> SizePreference {
    match basis {
        SizeBasis::Logical => SizePreference::Logical,
        SizeBasis::Allocated => SizePreference::Allocated,
    }
}

/// Fixed width keeps the preference cluster measurable for right alignment.
const PREFERENCE_COMBO_WIDTH: f32 = 132.0;

/// Gives a combo box an accessible name and, when there is room, a visible
/// label after it in egui's usual control-then-label order.
fn label_combo(ui: &mut egui::Ui, combo: egui::Response, label: &str, show_label: bool) {
    combo.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, ui.is_enabled(), label)
    });
    if show_label {
        let label_response = ui.label(RichText::new(label).weak());
        let _labelled = combo.labelled_by(label_response.id);
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
    use crate::resolver::{ResolveBackend, ResolveFailure, ResolvedRoot};
    use std::path::Path;

    struct RejectingBackend;

    impl ResolveBackend for RejectingBackend {
        fn resolve_root(&self, _path: &Path) -> Result<ResolvedRoot, ResolveFailure> {
            Err(ResolveFailure {
                class: FailureClass::PathUnavailable,
                detail: "fixture".to_owned(),
            })
        }

        fn discover_volumes(&self) -> VolumeList {
            VolumeList::default()
        }

        fn filesystem(&self, _cancel: CancelToken) -> Arc<dyn ScanFs> {
            Arc::new(crate::resolver::UnsupportedBackend).filesystem(CancelToken::new())
        }
    }

    fn headless_shell(startup_path: Option<PathBuf>) -> (DiskPieShell, egui::Context) {
        let ctx = egui::Context::default();
        let mut store = crate::storage::SettingsDocumentStore::from_bytes(None);
        let settings = SettingsSession::load(&mut store);
        let runtime = RuntimeController::new(runtime_config_from_settings(settings.settings()))
            .expect("runtime starts");
        let (resolver, _worker) = crate::resolver::start(RejectingBackend).expect("resolver");
        let services = UiServices {
            runtime,
            picker: None,
            resolver,
            diagnostics: DiagnosticSink::disconnected(),
            fonts: SystemFonts::default(),
        };
        let shell = DiskPieShell::with_context(
            &ctx,
            startup_path,
            false,
            false,
            settings,
            services,
            ExitHandoff::default(),
        )
        .expect("shell builds headlessly");
        (shell, ctx)
    }

    fn settle(shell: &mut DiskPieShell, ctx: &egui::Context) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while shell.resolver.outstanding() > 0 {
            assert!(Instant::now() < deadline, "resolver did not settle");
            shell.logic(ctx);
            std::thread::sleep(Duration::from_millis(2));
        }
        shell.logic(ctx);
    }

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

    #[test]
    fn preference_adapters_round_trip_every_closed_variant() {
        for locale in [SettingsLocale::EnglishUnitedStates, SettingsLocale::SpanishMexico] {
            assert_eq!(setting_from_locale(locale_from_setting(locale)), locale);
        }
        for theme in [SettingsTheme::System, SettingsTheme::Dark, SettingsTheme::Light] {
            assert_eq!(setting_from_theme(theme_from_setting(theme)), theme);
        }
        for metric in [SizePreference::Logical, SizePreference::Allocated] {
            assert_eq!(setting_from_basis(basis_from_setting(metric)), metric);
        }
    }

    #[test]
    fn exit_handoff_survives_a_poisoned_mutex() {
        let exit = ExitHandoff::default();
        let poisoned = exit.clone();
        let _ = std::panic::catch_unwind(move || {
            let _guard = poisoned.0.lock().expect("initial lock");
            panic!("poison handoff fixture");
        });
        let mut store = crate::storage::SettingsDocumentStore::from_bytes(None);
        let session = SettingsSession::load(&mut store);

        exit.publish(ShellExit { settings: session.clone(), runtime: None, picker: None });
        let taken = exit.take().expect("handoff present");
        assert_eq!(taken.settings, session);
        assert!(taken.runtime.is_none());
    }

    #[test]
    fn runtime_config_honours_the_layout_and_worker_settings() {
        let settings = Settings {
            scan_workers: 3,
            layout_max_depth: 5,
            layout_max_sectors: 2_048,
            layout_minimum_sweep: 0.01,
            size_preference: SizePreference::Logical,
            ..Settings::default()
        };
        let config = runtime_config_from_settings(&settings);
        assert_eq!(config.scan_options.worker_count, 3);
        assert_eq!(config.layout_options.max_depth, 5);
        assert_eq!(config.layout_options.max_sectors, 2_048);
        assert!((config.layout_options.minimum_sweep - 0.01).abs() < f64::EPSILON);
        assert_eq!(config.layout_options.size_basis, SizeBasis::Logical);

        let automatic = runtime_config_from_settings(&Settings::default());
        assert_eq!(
            automatic.scan_options.worker_count,
            RuntimeConfig::default().scan_options.worker_count
        );
        assert!(RuntimeController::new(config).is_ok());
    }

    #[test]
    fn shortcuts_map_to_typed_commands_and_escape_only_cancels_while_scanning() {
        assert_eq!(
            shortcut_command(Modifiers::NONE, Key::Backspace, false),
            Some(ShellCommand::Navigate(NavigationAction::Back))
        );
        assert_eq!(
            shortcut_command(Modifiers::ALT, Key::ArrowLeft, false),
            Some(ShellCommand::Navigate(NavigationAction::Back))
        );
        assert_eq!(
            shortcut_command(Modifiers::ALT, Key::ArrowUp, false),
            Some(ShellCommand::Navigate(NavigationAction::Parent))
        );
        assert_eq!(
            shortcut_command(Modifiers::NONE, Key::F5, false),
            Some(ShellCommand::Navigate(NavigationAction::RescanAll))
        );
        assert_eq!(shortcut_command(Modifiers::NONE, Key::Escape, false), None);
        assert_eq!(
            shortcut_command(Modifiers::NONE, Key::Escape, true),
            Some(ShellCommand::Cancel)
        );
        assert_eq!(
            shortcut_command(Modifiers::COMMAND, Key::O, false),
            Some(ShellCommand::ChooseFolder)
        );
        assert_eq!(shortcut_command(Modifiers::NONE, Key::ArrowLeft, false), None);
        for (modifiers, key) in SHORTCUTS {
            assert!(shortcut_command(modifiers, key, true).is_some());
        }
    }

    #[test]
    fn every_unavailable_reason_has_a_message_and_every_state_a_phase() {
        let node = NodeId::from_raw(1);
        let generation = GenerationId::new(1);
        let reasons = [
            UnavailableReason::StaleGeneration { current: generation, command: generation },
            UnavailableReason::UnknownNode { node },
            UnavailableReason::SyntheticTarget { node },
            UnavailableReason::NotNavigable { node, kind: EntryKind::File },
            UnavailableReason::NotFilesystemRoot { node, kind: EntryKind::File },
            UnavailableReason::NoHistory,
            UnavailableReason::NoParent,
            UnavailableReason::NoSummary,
            UnavailableReason::NoSelection,
            UnavailableReason::FilesystemRootCannotBeHidden { node },
            UnavailableReason::ViewRootCannotBeHidden { node },
            UnavailableReason::AlreadyHidden { node },
            UnavailableReason::NotHidden { node },
            UnavailableReason::NoHiddenBranches,
            UnavailableReason::HiddenStateBusy { max_delta_depth: 64 },
            UnavailableReason::HiddenTarget { node },
            UnavailableReason::NoRescanRoots,
            UnavailableReason::NoRealPath { node },
        ];
        let i18n = I18n::new(Locale::EnglishUnitedStates).expect("locale");
        let strings = UiStrings::new(&i18n);
        for reason in reasons {
            let id = reason_message(reason);
            assert!(!id.requires_arguments());
            assert!(!strings.get(id).is_empty());
        }
        for state in [
            ScanUi::Empty,
            ScanUi::Choosing,
            ScanUi::Resolving,
            ScanUi::Scanning,
            ScanUi::Partial { settled: false },
            ScanUi::Partial { settled: true },
            ScanUi::Cancelling,
            ScanUi::Complete,
            ScanUi::CancelledWithResults,
            ScanUi::Failed { class: FailureClass::Other },
        ] {
            assert!(!strings.get(phase_message(state)).is_empty());
        }
        assert_eq!(
            reason_message(UnavailableReason::HiddenStateBusy { max_delta_depth: 64 }),
            MessageId::ReasonHiddenStateBusy
        );
    }

    #[test]
    fn percent_formatting_is_bounded() {
        assert_eq!(format_percent(0.1234), "12.3 %");
        assert_eq!(format_percent(1.5), "100.0 %");
        assert_eq!(format_percent(f64::NAN), "0.0 %");
        assert_eq!(format_percent(fraction(1, 0)), "0.0 %");
        assert_eq!(format_percent(fraction(1, 4)), "25.0 %");
    }

    #[test]
    fn idle_logic_phase_does_not_request_repaint() {
        let (mut shell, ctx) = headless_shell(None);
        settle(&mut shell, &ctx);
        assert_eq!(shell.scan_state(), ScanUi::Empty);

        // egui itself may schedule warm-up repaints during its first passes,
        // so the assertion is about the shell: no repaint cause may originate
        // in this file while the application is idle.
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let ctx = ui.ctx().clone();
            shell.logic(&ctx);
            let causes = ctx.repaint_causes();
            assert!(
                causes.iter().all(|cause| !cause.file.ends_with("shell.rs")),
                "idle logic phase requested repaint: {causes:?}"
            );
        });

        let mut choosing = ScanFlow::new();
        choosing.dialog_submitted(1);
        assert_eq!(repaint_policy(false, &choosing), RepaintRequest::Soon);
    }

    #[test]
    fn startup_path_goes_through_the_resolver_and_reports_typed_failure() {
        let (mut shell, ctx) = headless_shell(Some(PathBuf::from(r"C:\fixture\missing")));
        assert_eq!(shell.scan_state(), ScanUi::Resolving);
        settle(&mut shell, &ctx);
        assert_eq!(shell.scan_state(), ScanUi::Failed { class: FailureClass::PathUnavailable });
        assert_eq!(shell.notice, Notice::Text("fixture".to_owned()));

        shell.dispatch(ShellCommand::ChooseFolder);
        assert_eq!(shell.notice, Notice::Message(MessageId::FolderPickerUnavailable));
        assert!(!shell.flow.background_work_outstanding());
    }

    #[test]
    fn navigation_without_a_frame_is_ignored_and_availability_is_typed() {
        let (mut shell, ctx) = headless_shell(None);
        settle(&mut shell, &ctx);
        assert!(!shell.availability(NavigationAction::Back).is_available());
        shell.dispatch(ShellCommand::Navigate(NavigationAction::Back));
        shell.dispatch(ShellCommand::Cancel);
        shell.dispatch(ShellCommand::SetMetric(SizeBasis::Logical));
        assert_eq!(shell.metric, SizeBasis::Logical);
        shell.synchronize_settings();
        assert_eq!(shell.settings.settings().size_preference, SizePreference::Logical);
        shell.begin_shutdown();
        shell.begin_shutdown();
        assert!(shell.shutdown_requested);
    }
}
