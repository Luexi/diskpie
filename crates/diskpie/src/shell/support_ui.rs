//! UI state for registry inspection and exact, bounded diagnostic previews.
//! Handles, filesystem inspection and writes remain in the support worker.
use super::*;
use crate::support_service::{
    ExportToken, SupportJob, SupportJobId, SupportOutcome, SupportResult,
};
use diskpie_app::{
    diagnostics::{
        AggregateMetrics, AllowlistedSettings, AppVersion, Architecture, CoarseOsVersion,
        DiagnosticExport, ExportMetadata, OperatingSystem, Target, export::ScanOutcome,
    },
    explorer_integration::{
        AmbiguityReason, Decision, ExplorerIntegrationStatus, IntegrationRequest, IntegrationState,
        decide,
    },
};
use diskpie_platform::{ShellRequest, windows::diagnostic_files::ExportDestinationKind};

const PREVIEW_PAGE_BYTES: usize = 8192;

struct PreparedDestination {
    token: ExportToken,
    display: String,
    kind: ExportDestinationKind,
    replaces_existing: bool,
    focus_cancel: bool,
}

#[derive(Default)]
pub(super) struct SupportUi {
    preferences_open: bool,
    integration: Option<ExplorerIntegrationStatus>,
    integration_job: Option<SupportJobId>,
    export_open: bool,
    include_paths: bool,
    preview: Option<DiagnosticExport>,
    preview_page: usize,
    preview_epoch: u64,
    preview_job: Option<(SupportJobId, u64)>,
    needs_preview: bool,
    save_dialog: Option<u64>,
    prepare_job: Option<(SupportJobId, u64)>,
    commit_job: Option<SupportJobId>,
    prepared: Option<PreparedDestination>,
    discard: Option<ExportToken>,
    failure: Option<String>,
}

impl SupportUi {
    /// Retains the operation result, but only a fresh successful inspection is
    /// allowed to advertise an installed integration. Unknown is displayed
    /// explicitly and is not saved as an enabled preference.
    fn settle_integration(&mut self, outcome: SupportOutcome) -> bool {
        self.integration_job = None;
        self.failure = None;
        self.integration = match outcome {
            SupportOutcome::Integration { status, report } => {
                self.failure = report.as_ref().and_then(|result| match result {
                    Err(error) => Some(error.to_string()),
                    Ok(report) => report.incomplete_cleanup.map(|error| error.to_string()),
                });
                match status {
                    Ok(status) => Some(status),
                    Err(error) => {
                        self.failure = Some(match report {
                            Some(Ok(report)) => format!(
                                "integration {:?}; status refresh failed: {error}",
                                report.outcome
                            ),
                            Some(Err(operation)) => {
                                format!("{operation}; status refresh failed: {error}")
                            }
                            None => error.to_string(),
                        });
                        None
                    }
                }
            }
            SupportOutcome::Failed(error) => {
                self.failure = Some(error.to_string());
                None
            }
            _ => None,
        };
        self.integration.as_ref().is_some_and(ExplorerIntegrationStatus::is_installed)
    }

    pub(super) fn needs_repaint(&self) -> bool {
        self.save_dialog.is_some()
            || self.preview_job.is_some()
            || self.prepare_job.is_some()
            || self.commit_job.is_some()
            || self.integration_job.is_some()
            || self.discard.is_some()
    }
}

impl DiskPieShell {
    pub(super) fn tools_menu(&mut self, ui: &mut egui::Ui) {
        let title = self.strings.get(MessageId::Tools).to_owned();
        ui.menu_button(title, |ui| {
            if ui.button(self.strings.get(MessageId::Preferences)).clicked() {
                self.support_ui.preferences_open = true;
                self.inspect_integration();
                ui.close();
            }
            if ui
                .add_enabled(
                    self.support.is_some(),
                    egui::Button::new(self.strings.get(MessageId::ExportDiagnostics)),
                )
                .clicked()
            {
                self.support_ui.export_open = true;
                self.support_ui.include_paths = false;
                self.invalidate_export();
                ui.close();
            }
            if ui
                .add_enabled(
                    self.picker.is_some(),
                    egui::Button::new(self.strings.get(MessageId::RecycleBin)),
                )
                .clicked()
            {
                self.open_bin();
                ui.close();
            }
            if ui
                .add_enabled(
                    self.picker.is_some(),
                    egui::Button::new(self.strings.get(MessageId::InstalledApps)),
                )
                .clicked()
            {
                self.installed_apps();
                ui.close();
            }
        });
    }

    fn inspect_integration(&mut self) {
        if self.support_ui.integration_job.is_some() {
            return;
        }
        self.support_ui.integration_job = self.submit_support(SupportJob::InspectIntegration);
    }

    pub(super) fn submit_support(&mut self, job: SupportJob) -> Option<SupportJobId> {
        let result = self.support.as_mut().and_then(|support| support.submit(job).ok());
        if result.is_none() {
            self.notice = Notice::Message(MessageId::SupportUnavailable);
        }
        result
    }

    fn invalidate_export(&mut self) {
        self.support_ui.preview_epoch = self.support_ui.preview_epoch.wrapping_add(1);
        self.support_ui.preview = None;
        self.support_ui.preview_page = 0;
        self.support_ui.needs_preview = true;
        self.discard_destination();
    }

    fn discard_destination(&mut self) {
        if let Some(prepared) = self.support_ui.prepared.take() {
            self.support_ui.discard = Some(prepared.token);
        }
    }

    pub(super) fn drain_support_results(&mut self) {
        for _ in 0..crate::support_service::SUPPORT_JOB_CAPACITY {
            let Some(result) = self.support.as_mut().and_then(|service| service.try_recv()) else {
                break;
            };
            if let SupportOutcome::Failed(error) = &result.outcome {
                self.support_ui.failure = Some(error.to_string());
            }
            let Some(result) = self.handle_action_support(result) else {
                continue;
            };
            self.handle_support_result(result);
        }
        if self.support.as_ref().is_some_and(|service| service.is_stopped()) {
            self.support = None;
            self.support_ui.integration_job = None;
            self.support_ui.integration = None;
            let mut settings = self.settings.settings().clone();
            settings.explorer_integration = false;
            self.settings.replace(settings);
            self.support_ui.preview_job = None;
            self.support_ui.prepare_job = None;
            self.support_ui.commit_job = None;
            self.support_ui.prepared = None;
            self.support_ui.discard = None;
            self.support_ui.needs_preview = false;
            self.action_support_stopped();
            self.notice = Notice::Message(MessageId::SupportUnavailable);
        }
        // A full queue cannot make us lose a prepared destination's handle.
        if let Some(token) = self.support_ui.discard
            && self
                .support
                .as_mut()
                .is_some_and(|service| service.submit(SupportJob::DiscardExport { token }).is_ok())
        {
            self.support_ui.discard = None;
        }
        if self.support_ui.export_open
            && self.support_ui.needs_preview
            && self.support_ui.preview_job.is_none()
            && self.support_ui.prepare_job.is_none()
            && self.support_ui.commit_job.is_none()
            && self.support_ui.discard.is_none()
        {
            self.build_preview();
        }
    }

    fn build_preview(&mut self) {
        self.synchronize_settings();
        let aggregates =
            last_scan_metrics(self.runtime.as_ref().and_then(RuntimeController::settled_summary));
        let architecture = if cfg!(target_arch = "x86_64") {
            Architecture::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Architecture::Aarch64
        } else if cfg!(target_arch = "x86") {
            Architecture::X86
        } else {
            Architecture::Other
        };
        let metadata = ExportMetadata {
            app_version: AppVersion::new(
                env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0),
                env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0),
                env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0),
            ),
            target: Target::new(OperatingSystem::Windows, architecture),
            // Unknown rather than inventing an OS version from a compatibility manifest.
            coarse_os_version: CoarseOsVersion::default(),
            panic_marker_present: self.previous_session_panicked,
        };
        let job = SupportJob::BuildExport {
            metadata,
            settings: AllowlistedSettings::from(self.settings.settings()),
            aggregates,
            counters: self
                .diagnostic_counters
                .as_ref()
                .map(|reader| reader.snapshot())
                .unwrap_or_default(),
            include_paths: self.support_ui.include_paths,
        };
        self.support_ui.needs_preview = false;
        if let Some(id) = self.submit_support(job) {
            self.support_ui.preview_job = Some((id, self.support_ui.preview_epoch));
        }
    }

    fn handle_support_result(&mut self, result: SupportResult) {
        let id = result.id;
        if self.support_ui.integration_job == Some(id) {
            let installed = self.support_ui.settle_integration(result.outcome);
            let mut settings = self.settings.settings().clone();
            settings.explorer_integration = installed;
            self.settings.replace(settings);
            if self.support_ui.failure.is_some() || self.support_ui.integration.is_none() {
                self.notice = Notice::Message(MessageId::SupportUnavailable);
            }
        } else if self.support_ui.preview_job.is_some_and(|(job, _)| job == id) {
            let (_, epoch) = self.support_ui.preview_job.take().expect("matched preview job");
            if epoch != self.support_ui.preview_epoch || !self.support_ui.export_open {
                return;
            }
            match result.outcome {
                SupportOutcome::Export(export) => {
                    debug_assert!(export.preview_is_exact_artifact());
                    self.support_ui.preview = Some(export);
                    let mut event = DiagnosticEvent::new(DiagnosticCode::ExportPrepared);
                    let _ = event.push_field(DiagnosticField::RequestId(id.get()));
                    self.diagnostics.emit(event);
                }
                _ => self.notice = Notice::Message(MessageId::ExportUnavailable),
            }
        } else if self.support_ui.prepare_job.is_some_and(|(job, _)| job == id) {
            let (_, epoch) = self.support_ui.prepare_job.take().expect("matched destination job");
            match result.outcome {
                SupportOutcome::ExportPrepared { token, destination, kind, replaces_existing } => {
                    if epoch != self.support_ui.preview_epoch || !self.support_ui.export_open {
                        self.support_ui.discard = Some(token);
                    } else {
                        self.support_ui.prepared = Some(PreparedDestination {
                            token,
                            display: format_path_for_display(&destination),
                            kind,
                            replaces_existing,
                            focus_cancel: true,
                        });
                    }
                }
                _ => self.notice = Notice::Message(MessageId::ExportUnavailable),
            }
        } else if self.support_ui.commit_job == Some(id) {
            self.support_ui.commit_job = None;
            self.notice =
                Notice::Message(if matches!(result.outcome, SupportOutcome::ExportWritten(_)) {
                    MessageId::ExportSaved
                } else {
                    MessageId::ExportUnavailable
                });
        } else if let SupportOutcome::ExportPrepared { token, .. } = result.outcome {
            self.support_ui.discard = Some(token);
        }
    }

    fn choose_export_destination(&mut self) {
        if self.support_ui.save_dialog.is_some() || self.support_ui.preview.is_none() {
            return;
        }
        self.discard_destination();
        let Some(owner) = self.owner else {
            self.notice = Notice::Message(MessageId::DialogFailed);
            return;
        };
        let id = self
            .picker
            .as_ref()
            .and_then(|picker| picker.submit(ShellRequest::save_export(owner)).ok());
        if let Some(id) = id {
            self.support_ui.save_dialog = Some(id.get());
        } else {
            self.notice = Notice::Message(MessageId::DialogFailed);
        }
    }

    pub(super) fn handle_export_dialog(&mut self, event: DialogEvent) {
        if self.support_ui.save_dialog != Some(event.request_id().get()) {
            return;
        }
        self.support_ui.save_dialog = None;
        if !self.support_ui.export_open {
            return;
        }
        match event {
            DialogEvent::Selected { path, .. } => {
                let Some(export) = self.support_ui.preview.as_ref() else {
                    return;
                };
                let artifact = export.artifact().clone();
                if let Some(id) =
                    self.submit_support(SupportJob::PrepareExport { destination: path, artifact })
                {
                    self.support_ui.prepare_job = Some((id, self.support_ui.preview_epoch));
                }
            }
            DialogEvent::Failed { .. } => self.notice = Notice::Message(MessageId::DialogFailed),
            DialogEvent::Cancelled { .. } => {}
        }
    }

    pub(super) fn export_service_stopped(&mut self) {
        if self.support_ui.save_dialog.take().is_some() {
            self.notice = Notice::Message(MessageId::DialogFailed);
        }
    }

    pub(super) fn render_support(&mut self, ui: &mut egui::Ui) {
        self.render_preferences(ui);
        self.render_export(ui);
    }

    fn render_preferences(&mut self, ui: &mut egui::Ui) {
        let mut open = self.support_ui.preferences_open;
        if !open {
            return;
        }
        let mut request = None;
        let mut refresh = false;
        egui::Window::new(self.strings.get(MessageId::Preferences))
            .id(egui::Id::new("preferences"))
            .open(&mut open)
            .resizable(true)
            .show(ui.ctx(), |ui| {
                self.appearance_controls(ui);
                ui.separator();
                ui.heading(self.strings.get(MessageId::ExplorerIntegration));
                if let Some(error) = &self.support_ui.failure {
                    ui.label(error);
                }
                ui.label(self.strings.get(MessageId::IntegrationClassic));
                if self.support.is_none() {
                    ui.label(self.strings.get(MessageId::SupportUnavailable));
                    return;
                }
                let busy = self.support_ui.integration_job.is_some();
                if let Some(status) = &self.support_ui.integration {
                    ui.label(self.strings.get(integration_message(status.state())));
                    if let IntegrationState::OwnedStale { stored_executable, current_executable } =
                        status.state()
                    {
                        ui.label(format_path_for_display(stored_executable));
                        ui.label(format_path_for_display(current_executable));
                    }
                    if !status.strays().is_empty() {
                        ui.label(self.strings.get(MessageId::IntegrationCleanup));
                    }
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(
                                !busy
                                    && integration_action_enabled(
                                        status,
                                        IntegrationRequest::Install,
                                    ),
                                egui::Button::new(self.strings.get(MessageId::Install)),
                            )
                            .clicked()
                        {
                            request = Some(IntegrationRequest::Install);
                        }
                        if ui
                            .add_enabled(
                                !busy
                                    && integration_action_enabled(
                                        status,
                                        IntegrationRequest::Repair,
                                    ),
                                egui::Button::new(self.strings.get(MessageId::Repair)),
                            )
                            .clicked()
                        {
                            request = Some(IntegrationRequest::Repair);
                        }
                        if ui
                            .add_enabled(
                                !busy
                                    && integration_action_enabled(
                                        status,
                                        IntegrationRequest::Remove,
                                    ),
                                egui::Button::new(self.strings.get(MessageId::Remove)),
                            )
                            .clicked()
                        {
                            request = Some(IntegrationRequest::Remove);
                        }
                    });
                } else {
                    ui.label(self.strings.get(MessageId::IntegrationUnknown));
                }
                if busy {
                    ui.spinner();
                }
                refresh = ui
                    .add_enabled(!busy, egui::Button::new(self.strings.get(MessageId::Refresh)))
                    .clicked();
            });
        self.support_ui.preferences_open = open;
        if let Some(request) = request {
            self.support_ui.integration_job =
                self.submit_support(SupportJob::ApplyIntegration(request));
        } else if refresh {
            self.inspect_integration();
        }
    }

    fn render_export(&mut self, ui: &mut egui::Ui) {
        let mut open = self.support_ui.export_open;
        if !open {
            return;
        }
        let mut rebuild = false;
        let mut choose = false;
        let mut commit = false;
        let mut discard = false;
        let busy = self.support_ui.save_dialog.is_some()
            || self.support_ui.prepare_job.is_some()
            || self.support_ui.commit_job.is_some();
        egui::Window::new(self.strings.get(MessageId::ExportDiagnostics))
            .id(egui::Id::new("diagnostic-export"))
            .open(&mut open)
            .default_width(700.0)
            .resizable(true)
            .show(ui.ctx(), |ui| {
                rebuild = ui
                    .add_enabled(
                        !busy,
                        egui::Checkbox::new(
                            &mut self.support_ui.include_paths,
                            self.strings.get(MessageId::IncludePaths),
                        ),
                    )
                    .changed();
                ui.label(self.strings.get(MessageId::ExportPreview));
                if let Some(error) = &self.support_ui.failure {
                    ui.label(error);
                }
                if let Some(export) = &self.support_ui.preview {
                    let text = export.preview().text();
                    let pages = text.len().div_ceil(PREVIEW_PAGE_BYTES).max(1);
                    self.support_ui.preview_page = self.support_ui.preview_page.min(pages - 1);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(self.support_ui.preview_page > 0, egui::Button::new("←"))
                            .clicked()
                        {
                            self.support_ui.preview_page -= 1;
                        }
                        ui.label(format!(
                            "{} / {} · {} bytes",
                            self.support_ui.preview_page + 1,
                            pages,
                            text.len()
                        ));
                        if ui
                            .add_enabled(
                                self.support_ui.preview_page + 1 < pages,
                                egui::Button::new("→"),
                            )
                            .clicked()
                        {
                            self.support_ui.preview_page += 1;
                        }
                    });
                    let mut page = preview_page(text, self.support_ui.preview_page);
                    egui::ScrollArea::both().max_height(ui.available_height().min(340.0)).show(
                        ui,
                        |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut page)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(f32::INFINITY),
                            );
                        },
                    );
                } else if self.support_ui.preview_job.is_some() {
                    ui.spinner();
                } else if ui.button(self.strings.get(MessageId::Refresh)).clicked() {
                    rebuild = true;
                }
                choose = ui
                    .add_enabled(
                        !busy
                            && self.support_ui.preview.is_some()
                            && self.support_ui.discard.is_none(),
                        egui::Button::new(self.strings.get(MessageId::ChooseDestination)),
                    )
                    .clicked();
                if let Some(prepared) = &mut self.support_ui.prepared {
                    ui.separator();
                    ui.label(&prepared.display);
                    if prepared.replaces_existing {
                        ui.label(self.strings.get(MessageId::ExportExistingDestination));
                    }
                    if prepared.kind == ExportDestinationKind::Unc {
                        ui.label(self.strings.get(MessageId::RemoteDestination));
                    }
                    ui.label(self.strings.get(MessageId::ConfirmDestination));
                    ui.horizontal(|ui| {
                        let cancel = ui.button(self.strings.get(MessageId::Cancel));
                        if prepared.focus_cancel {
                            cancel.request_focus();
                            prepared.focus_cancel = false;
                        }
                        discard = cancel.clicked();
                        commit = ui
                            .add_enabled(
                                !busy && !prepared.replaces_existing,
                                egui::Button::new(self.strings.get(MessageId::SaveExport)),
                            )
                            .clicked();
                    });
                }
                if busy {
                    ui.spinner();
                }
            });
        self.support_ui.export_open = open;
        if !open {
            self.support_ui.needs_preview = false;
            self.support_ui.preview_epoch = self.support_ui.preview_epoch.wrapping_add(1);
            self.discard_destination();
        } else if rebuild {
            self.invalidate_export();
        } else if discard {
            self.discard_destination();
        } else if choose {
            self.choose_export_destination();
        } else if commit && let Some(prepared) = self.support_ui.prepared.take() {
            self.support_ui.commit_job =
                self.submit_support(SupportJob::CommitExport { token: prepared.token });
            // Rejection consumes this UI attempt but the worker still owns the token.
            if self.support_ui.commit_job.is_none() {
                self.support_ui.discard = Some(prepared.token);
            }
        }
    }
}

fn integration_message(state: &IntegrationState) -> MessageId {
    match state {
        IntegrationState::NotInstalled => MessageId::IntegrationAbsent,
        IntegrationState::OwnedCurrent => MessageId::IntegrationCurrent,
        IntegrationState::OwnedStale { .. } => MessageId::IntegrationStale,
        IntegrationState::Ambiguous(AmbiguityReason::PartialInstallation) => {
            MessageId::IntegrationPartial
        }
        IntegrationState::Foreign | IntegrationState::Ambiguous(_) => {
            MessageId::IntegrationConflict
        }
    }
}

fn integration_action_enabled(
    status: &ExplorerIntegrationStatus,
    request: IntegrationRequest,
) -> bool {
    matches!(decide(request, status), Decision::Perform(_))
}

fn last_scan_metrics(summary: Option<diskpie_app::runtime::ScanSummary>) -> AggregateMetrics {
    let Some(summary) = summary else {
        return AggregateMetrics::default();
    };
    use diskpie_app::session::SessionPhase;
    let outcome = match summary.phase {
        SessionPhase::Complete => ScanOutcome::Complete,
        SessionPhase::Partial => ScanOutcome::Partial,
        SessionPhase::Cancelled => ScanOutcome::Cancelled,
        SessionPhase::Failed(_) => ScanOutcome::Failed,
        SessionPhase::AwaitingStart | SessionPhase::Running => return AggregateMetrics::default(),
    };
    let counts = summary.progress.effective();
    AggregateMetrics {
        generation: Some(summary.generation.get()),
        outcome: Some(outcome),
        items_observed: Some(counts.entries),
        // ScanSummary does not retain measured bytes or elapsed time. In a
        // branch rescan the displayed tree may also be a different scope.
        // Its scanner counters exclude resolver omissions, so they cannot
        // establish the total omission count either.
        ..AggregateMetrics::default()
    }
}

/// Every UTF-8 byte belongs to exactly one page; boundaries never split a scalar.
fn preview_page(text: &str, page: usize) -> &str {
    let mut start = page.saturating_mul(PREVIEW_PAGE_BYTES).min(text.len());
    let mut end = start.saturating_add(PREVIEW_PAGE_BYTES).min(text.len());
    while !text.is_char_boundary(start) {
        start += 1;
    }
    while !text.is_char_boundary(end) {
        end += 1;
    }
    &text[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_app::explorer_integration::{
        IntegrationError, IntegrationOutcome, IntegrationReport, KeyState, RegistryError,
        RegistryErrorKind, RegistryOperation, RollbackReport,
    };

    fn integration_status(keys: [KeyState; 2]) -> ExplorerIntegrationStatus {
        ExplorerIntegrationStatus::from_key_states(
            std::path::Path::new(r"C:\DiskPie\diskpie.exe"),
            keys,
        )
    }

    #[test]
    fn partial_integration_can_be_completed_in_either_direction() {
        for keys in [
            [KeyState::OwnedCurrent, KeyState::NotInstalled],
            [KeyState::NotInstalled, KeyState::OwnedCurrent],
        ] {
            let status = integration_status(keys);
            assert!(integration_action_enabled(&status, IntegrationRequest::Install));
            assert!(integration_action_enabled(&status, IntegrationRequest::Repair));
            assert_eq!(integration_message(status.state()), MessageId::IntegrationPartial);
        }
        let conflict = integration_status([KeyState::Foreign, KeyState::NotInstalled]);
        for request in
            [IntegrationRequest::Install, IntegrationRequest::Repair, IntegrationRequest::Remove]
        {
            assert!(!integration_action_enabled(&conflict, request));
        }
    }

    #[test]
    fn failed_attempts_replace_old_verified_state_and_keep_mutation_evidence() {
        let mut state = SupportUi {
            integration: Some(integration_status([KeyState::OwnedCurrent, KeyState::OwnedCurrent])),
            ..Default::default()
        };
        let error = IntegrationError::Registry {
            error: RegistryError::new(RegistryOperation::Delete, RegistryErrorKind::AccessDenied),
            rollback: RollbackReport::RemovalIncomplete { removed_subtrees: 1 },
        };
        let installed = state.settle_integration(SupportOutcome::Integration {
            status: Ok(integration_status([KeyState::NotInstalled, KeyState::OwnedCurrent])),
            report: Some(Err(error.clone())),
        });
        assert!(!installed, "the saved preference must reflect the fresh partial state");
        assert_eq!(
            state.integration.as_ref().unwrap().state(),
            &IntegrationState::Ambiguous(AmbiguityReason::PartialInstallation)
        );
        assert!(state.failure.as_ref().unwrap().contains("RemovalIncomplete"));
        let installed = state.settle_integration(SupportOutcome::Integration {
            status: Err(error),
            report: Some(Ok(IntegrationReport {
                outcome: IntegrationOutcome::Installed,
                incomplete_cleanup: None,
            })),
        });
        assert!(!installed);
        assert!(state.integration.is_none(), "a failed verification invalidates the old state");
        assert!(
            state.failure.as_ref().unwrap().contains("Installed"),
            "a failed inspection must not erase the successful mutation"
        );
    }

    #[test]
    fn exported_metrics_use_the_last_generation_and_leave_unmeasured_fields_unknown() {
        use diskpie_app::{
            runtime::ScanSummary,
            session::{SessionPhase, SessionProgress},
        };
        assert_eq!(last_scan_metrics(None), AggregateMetrics::default());
        let metrics = last_scan_metrics(Some(ScanSummary {
            generation: diskpie_scan::ScanGeneration::new(41),
            phase: SessionPhase::Partial,
            progress: SessionProgress::default(),
        }));
        assert_eq!(metrics.generation, Some(41));
        assert_eq!(metrics.outcome, Some(ScanOutcome::Partial));
        assert_eq!(metrics.items_observed, Some(0));
        assert_eq!(metrics.bytes_observed, None);
        assert_eq!(metrics.scan_omissions, None);
        assert_eq!(metrics.scan_duration_millis, None);
    }

    #[test]
    fn exact_preview_pages_preserve_all_unicode_bytes() {
        let text = "á文件🙂\r\n".repeat(1900);
        let joined = (0..text.len().div_ceil(PREVIEW_PAGE_BYTES))
            .map(|page| preview_page(&text, page))
            .collect::<String>();
        assert_eq!(joined.as_bytes(), text.as_bytes());
    }

    #[test]
    fn every_export_starts_without_paths_or_destination() {
        let ui = SupportUi::default();
        assert!(!ui.include_paths);
        assert!(ui.prepared.is_none());
        assert!(ui.save_dialog.is_none());
    }

    #[test]
    fn integration_states_have_distinct_localized_labels() {
        assert_eq!(
            integration_message(&IntegrationState::NotInstalled),
            MessageId::IntegrationAbsent
        );
        assert_eq!(
            integration_message(&IntegrationState::OwnedCurrent),
            MessageId::IntegrationCurrent
        );
        assert_eq!(integration_message(&IntegrationState::Foreign), MessageId::IntegrationConflict);
        assert_eq!(
            integration_message(&IntegrationState::OwnedStale {
                stored_executable: "old.exe".into(),
                current_executable: "new.exe".into()
            }),
            MessageId::IntegrationStale
        );
    }
}
