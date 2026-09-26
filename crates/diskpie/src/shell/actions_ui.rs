//! Composition of exact-target confirmation, single-use dispatch, and reconciliation.
//! All native work is submitted to the bounded support worker or Shell STA.

#![cfg(windows)]

use super::{DiskPieShell, MessageId, Notice};
use crate::support_service::{SupportJob, SupportJobId, SupportOutcome, SupportResult};
use diskpie_app::{
    actions::{
        ActionKind, ActionOutcome, ActionPurpose, ActionReport, ConfirmationFlow,
        ConfirmationPresentation, DestructiveOutcome, EmptyBinOutcome, FailureStage,
        FilesystemTarget, FlowStatus, OpenItem, PendingObligation, PostActionObligation,
        RevealItem, SizeSummary,
    },
    diagnostics::{DiagnosticCode, DiagnosticEvent, DiagnosticField},
    format::format_iec_bytes,
    navigation::NavigationAction,
    runtime::RuntimeState,
    session::SessionPhase,
};
use diskpie_core::{GenerationId, NodeId};
use diskpie_platform::{DialogRequestId, DispatchOutcome, ShellEvent, ShellRequest, SubmitError};
use eframe::egui::{self, RichText};

const MAX_DISPATCHES: usize = 4;

/// Item actions reachable from the interface. Cleanup is recycle-only: the
/// interface offers no permanent deletion and no Recycle Bin emptying.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileAction {
    Open,
    Reveal,
    Recycle,
}

impl FileAction {
    const fn destructive(self) -> bool {
        matches!(self, Self::Recycle)
    }
    const fn purpose(self) -> ActionPurpose {
        match self {
            Self::Open => ActionPurpose::Open,
            Self::Reveal => ActionPurpose::Reveal,
            Self::Recycle => ActionPurpose::Destructive,
        }
    }
}

struct Preparing {
    id: SupportJobId,
    action: FileAction,
    generation: GenerationId,
    node: NodeId,
    verifying: bool,
}

/// Root-owned shutdown handoff. Unknown shutdown outcomes still carry the
/// mandatory rescan obligation and cannot be interpreted as successful deletion.
pub(crate) struct PendingAction {
    request_id: u64,
    obligation: PendingObligation,
}

impl PendingAction {
    pub(crate) fn matches(&self, event: &ShellEvent) -> bool {
        self.request_id == event.request_id().get()
    }

    pub(crate) fn settle(self, event: Option<ShellEvent>) -> ActionReport {
        let outcome = event
            .filter(|event| self.matches(event))
            .and_then(|event| destructive_event(event, self.obligation.kind()))
            .unwrap_or_else(|| unknown_outcome(self.obligation.kind()));
        self.obligation.settle(outcome)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RefreshKind {
    Branch,
    All,
}

enum Reconciliation {
    Needed(PostActionObligation),
    Resolving { kind: RefreshKind, before: u64 },
    Scanning { kind: RefreshKind, generation: GenerationId },
}

pub(super) struct ActionUi {
    flow: ConfirmationFlow,
    preparing: Option<Preparing>,
    subject: Option<FilesystemTarget>,
    pending: Option<PendingAction>,
    mutation_cancel: Option<DialogRequestId>,
    dispatches: Vec<DialogRequestId>,
    focus_cancel: bool,
    reconciliation: Option<Reconciliation>,
    stale: Option<GenerationId>,
    halted: bool,
    report: Option<ActionReport>,
    report_rejected: bool,
}

impl Default for ActionUi {
    fn default() -> Self {
        Self {
            flow: ConfirmationFlow::new(),
            preparing: None,
            subject: None,
            pending: None,
            mutation_cancel: None,
            dispatches: Vec::new(),
            focus_cancel: false,
            reconciliation: None,
            stale: None,
            halted: false,
            report: None,
            report_rejected: false,
        }
    }
}

impl ActionUi {
    pub(super) fn needs_repaint(&self) -> bool {
        self.preparing.is_some()
            || self.pending.is_some()
            || !self.dispatches.is_empty()
            || self.reconciliation.is_some()
    }

    /// A preparation or a confirmation already occupies the action flow, so a
    /// new file action could only be refused; the controls mirror this.
    pub(super) fn busy(&self) -> bool {
        self.preparing.is_some() || self.flow.status() != FlowStatus::Idle
    }

    pub(super) fn take_pending(&mut self) -> Option<PendingAction> {
        self.mutation_cancel = None;
        self.pending.take()
    }

    fn complete(&mut self, request_id: u64, outcome: ActionOutcome) -> bool {
        if self.pending.as_ref().is_none_or(|pending| pending.request_id != request_id) {
            return false;
        }
        let pending = self.pending.take().expect("matching destructive request");
        self.mutation_cancel = None;
        self.record(pending.obligation.settle(outcome));
        true
    }

    fn record(&mut self, report: ActionReport) {
        self.halted |= report.requires_halt();
        self.reconciliation = Some(Reconciliation::Needed(report.obligation.clone()));
        self.report = Some(report);
        self.report_rejected = false;
    }

    fn cancel_confirmation(&mut self) {
        self.flow.cancel();
        self.subject = None;
        // Outstanding preparation keeps its slot until the result arrives.
        // Marking it nonverifying is insufficient: remove it and its late
        // result is ignored by the support event router without issuing work.
        self.preparing = None;
    }
}

impl DiskPieShell {
    pub(super) fn destructive_busy(&self) -> bool {
        self.actions.preparing.as_ref().is_some_and(|preparing| preparing.action.destructive())
            || self.actions.flow.status() != FlowStatus::Idle
            || self.actions.pending.is_some()
            || self.actions.reconciliation.is_some()
    }

    fn action_scan_settled(&self) -> bool {
        !self.shutdown_requested
            && self.scan_job.is_none()
            && self.branch_job.is_none()
            && self.deferred_launch.is_none()
            && self.runtime.as_ref().is_some_and(|runtime| {
                matches!(runtime.state(), RuntimeState::Idle | RuntimeState::Settled { .. })
                    && runtime.pending_generation().is_none()
                    && runtime.layout_request_id().is_none()
            })
    }

    fn action_generation(&self) -> GenerationId {
        self.runtime
            .as_ref()
            .and_then(|runtime| runtime.snapshot())
            .map_or(GenerationId::new(0), |snapshot| snapshot.generation())
    }

    fn current_target(&self, target: &FilesystemTarget) -> bool {
        self.runtime.as_ref().and_then(|runtime| runtime.navigation()).is_some_and(|navigation| {
            navigation.generation() == target.generation()
                && !navigation.is_hidden(target.node())
                && navigation.snapshot().node(target.node()).is_some()
        })
    }

    pub(super) fn start_file_action(&mut self, node: NodeId, action: FileAction) {
        if self.actions.preparing.is_some() || self.actions.flow.status() != FlowStatus::Idle {
            self.notice = Notice::Message(MessageId::ActionWaitForScan);
            return;
        }
        if action.destructive()
            && (self.destructive_busy()
                || !self.action_scan_settled()
                || self.actions.stale.is_some()
                || self.actions.halted)
        {
            self.notice = Notice::Message(if self.actions.halted {
                MessageId::ActionHalted
            } else if self.actions.stale.is_some() {
                MessageId::ResultsStale
            } else {
                MessageId::ActionWaitForScan
            });
            return;
        }
        self.prepare_file_action(node, action, false);
    }

    fn prepare_file_action(&mut self, node: NodeId, action: FileAction, verifying: bool) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        let (Some(snapshot), Some(navigation)) = (runtime.snapshot_handle(), runtime.navigation())
        else {
            return;
        };
        let generation = snapshot.generation();
        let hidden = navigation.is_hidden(node);
        let Some(support) = self.support.as_mut() else {
            self.notice = Notice::Message(MessageId::SupportUnavailable);
            self.actions.cancel_confirmation();
            return;
        };
        match support.submit(SupportJob::PrepareTarget {
            snapshot,
            generation,
            node,
            hidden,
            purpose: action.purpose(),
        }) {
            Ok(id) => {
                self.actions.preparing =
                    Some(Preparing { id, action, generation, node, verifying });
                self.notice = Notice::Message(MessageId::ActionPreparing);
            }
            Err(_) => {
                self.actions.cancel_confirmation();
                self.notice = Notice::Message(MessageId::ActionRejected);
            }
        }
    }

    pub(super) fn handle_action_support(&mut self, result: SupportResult) -> Option<SupportResult> {
        if self.actions.preparing.as_ref().is_none_or(|pending| pending.id != result.id) {
            return Some(result);
        }
        let preparing = self.actions.preparing.take().expect("matching preparation");
        let SupportOutcome::Target { target, recycle_supported } = result.outcome else {
            self.actions.cancel_confirmation();
            self.notice = Notice::Message(MessageId::ActionRejected);
            return None;
        };
        if target.generation() != preparing.generation
            || target.node() != preparing.node
            || !self.current_target(&target)
            || (preparing.action.destructive()
                && (!self.action_scan_settled() || self.actions.halted))
        {
            self.actions.cancel_confirmation();
            self.notice = Notice::Message(MessageId::ActionTargetChanged);
            return None;
        }
        if preparing.action == FileAction::Recycle && !recycle_supported {
            self.actions.cancel_confirmation();
            self.notice = Notice::Message(MessageId::RecycleUnsupported);
            return None;
        }
        if preparing.verifying {
            self.dispatch_confirmed_target(target, preparing.action);
        } else {
            match preparing.action {
                FileAction::Open => {
                    if let Some(owner) = self.owner {
                        self.submit_dispatch(ShellRequest::open(owner, &OpenItem::new(target)));
                    } else {
                        self.notice = Notice::Message(MessageId::ActionRejected);
                    }
                }
                FileAction::Reveal => {
                    self.submit_dispatch(ShellRequest::reveal(&RevealItem::new(target)))
                }
                FileAction::Recycle => {
                    self.actions.subject = Some(target.clone());
                    if self.actions.flow.begin_recycle(target).is_ok() {
                        self.actions.focus_cancel = true;
                    } else {
                        self.notice = Notice::Message(MessageId::ActionRejected);
                    }
                }
            }
        }
        None
    }

    fn dispatch_confirmed_target(&mut self, target: FilesystemTarget, action: FileAction) {
        let Some(owner) = self.owner else {
            self.actions.cancel_confirmation();
            self.notice = Notice::Message(MessageId::ActionRejected);
            return;
        };
        let request = match action {
            FileAction::Recycle => self.actions.flow.take_recycle().ok().and_then(|capability| {
                capability.binding().verify_target(&target).ok()?;
                Some(ShellRequest::recycle(owner, capability))
            }),
            FileAction::Open | FileAction::Reveal => None,
        };
        self.actions.subject = None;
        match request {
            Some((request, obligation)) => self.submit_mutation(request, obligation),
            None => {
                self.actions.cancel_confirmation();
                self.notice = Notice::Message(MessageId::ActionTargetChanged);
            }
        }
    }

    fn submit_dispatch(&mut self, request: ShellRequest) {
        if self.actions.dispatches.len() >= MAX_DISPATCHES {
            self.notice = Notice::Message(MessageId::ActionRejected);
            return;
        }
        match self
            .picker
            .as_ref()
            .ok_or(SubmitError::ServiceUnavailable)
            .and_then(|picker| picker.submit(request))
        {
            Ok(id) => {
                self.actions.dispatches.push(id);
                self.action_requested(id);
            }
            Err(_) => self.notice = Notice::Message(MessageId::ActionRejected),
        }
    }

    fn submit_mutation(&mut self, request: ShellRequest, obligation: PendingObligation) {
        self.actions.stale = Some(self.action_generation());
        let kind = obligation.kind();
        match self
            .picker
            .as_ref()
            .ok_or(SubmitError::ServiceUnavailable)
            .and_then(|picker| picker.submit(request))
        {
            Ok(id) => {
                self.actions.pending = Some(PendingAction { request_id: id.get(), obligation });
                self.actions.mutation_cancel = Some(id);
                self.notice = Notice::Message(MessageId::ActionWorking);
                self.action_requested(id);
            }
            Err(_) => {
                // The capability is consumed even when enqueue fails. Its
                // obligation is settled exactly once; never retry this request.
                let outcome = match kind {
                    ActionKind::EmptyRecycleBin => {
                        ActionOutcome::RecycleBin(EmptyBinOutcome::CancelledBeforeMutation)
                    }
                    _ => ActionOutcome::Filesystem(DestructiveOutcome::CancelledBeforeMutation),
                };
                self.actions.record(obligation.settle(outcome));
                self.actions.report_rejected = true;
                self.notice = Notice::Message(MessageId::ActionRejected);
            }
        }
    }

    fn action_requested(&self, id: DialogRequestId) {
        let mut event = DiagnosticEvent::new(DiagnosticCode::ShellActionRequested);
        let _ = event.push_field(DiagnosticField::RequestId(id.get()));
        self.diagnostics.emit(event);
    }

    pub(super) fn installed_apps(&mut self) {
        if let Some(owner) = self.owner {
            self.submit_dispatch(ShellRequest::installed_apps(owner));
        } else {
            self.notice = Notice::Message(MessageId::ActionRejected);
        }
    }

    pub(super) fn handle_action_shell(&mut self, event: ShellEvent) -> Option<ShellEvent> {
        let id = event.request_id();
        if self.actions.pending.as_ref().is_some_and(|pending| pending.request_id == id.get()) {
            let kind = self.actions.pending.as_ref().expect("matching request").obligation.kind();
            let outcome = destructive_event(event, kind).unwrap_or_else(|| unknown_outcome(kind));
            let message = outcome_message(&outcome);
            self.actions.complete(id.get(), outcome);
            self.notice = Notice::Message(if self.actions.halted {
                MessageId::ActionHalted
            } else {
                message
            });
            return None;
        }
        if let Some(index) = self.actions.dispatches.iter().position(|request| *request == id) {
            self.actions.dispatches.swap_remove(index);
            self.notice = Notice::Message(match event {
                ShellEvent::Open { outcome: DispatchOutcome::Dispatched, .. }
                | ShellEvent::Reveal { outcome: DispatchOutcome::Dispatched, .. }
                | ShellEvent::InstalledApps { outcome: DispatchOutcome::Dispatched, .. } => {
                    MessageId::ActionDispatched
                }
                _ => MessageId::ActionFailed,
            });
            return None;
        }
        Some(event)
    }

    pub(super) fn render_actions(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if self.actions.report.is_some()
            || self.actions.halted
            || self.actions.stale.is_some()
            || self.actions.pending.is_some()
            || self.actions.preparing.is_some()
            || self.actions.reconciliation.is_some()
        {
            egui::Window::new("DiskPie")
                .id(egui::Id::new("diskpie-action-status"))
                .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(12.0, -12.0))
                .title_bar(false)
                .resizable(false)
                .collapsible(false)
                .show(&ctx, |ui| {
                    ui.set_max_width((ctx.content_rect().width() - 48.0).clamp(180.0, 440.0));
                    self.render_action_status(ui);
                });
        }
        self.render_confirmation(&ctx);
    }

    /// Native screenshot fixture for the real post-mutation pending-refresh UI.
    /// The fixture does not dispatch an action or touch any filesystem target.
    #[cfg(test)]
    pub(super) fn set_visual_pending_refresh(&mut self) {
        self.actions.stale = Some(self.action_generation());
        self.notice = Notice::Message(MessageId::ResultsStale);
    }

    fn render_action_status(&mut self, ui: &mut egui::Ui) {
        if let Some(report) = &self.actions.report {
            let message = if self.actions.report_rejected {
                MessageId::ActionRejected
            } else {
                outcome_message(&report.outcome)
            };
            ui.label(self.strings.get(message));
            if self.actions.stale.is_none()
                && !self.destructive_busy()
                && ui.button(self.strings.get(MessageId::Close)).clicked()
            {
                self.actions.report = None;
            }
        }
        if self.actions.halted {
            ui.colored_label(ui.visuals().warn_fg_color, self.strings.get(MessageId::ActionHalted));
        }
        if self.actions.stale.is_some() {
            ui.label(self.strings.get(MessageId::ResultsStale));
            if !self.destructive_busy() && ui.button(self.strings.get(MessageId::Rescan)).clicked()
            {
                self.start_action_full_refresh(self.runtime_scan_sequence());
            }
        }
        if self.actions.pending.is_some() {
            ui.label(self.strings.get(MessageId::ActionWorking));
            if ui.button(self.strings.get(MessageId::Cancel)).clicked()
                && let (Some(picker), Some(id)) = (&self.picker, self.actions.mutation_cancel)
            {
                picker.cancel(id);
            }
        } else if self.actions.preparing.is_some() {
            ui.label(self.strings.get(MessageId::ActionPreparing));
            if ui.button(self.strings.get(MessageId::Cancel)).clicked() {
                self.actions.cancel_confirmation();
            }
        } else if self.actions.reconciliation.is_some() {
            ui.label(self.strings.get(MessageId::ActionRefreshing));
        }
    }

    /// Called only after the STA receiver is drained and reports disconnection.
    pub(super) fn action_service_stopped(&mut self) {
        if let Some(pending) = self.actions.take_pending() {
            self.actions.record(pending.settle(None));
            self.notice = Notice::Message(MessageId::ActionUnknown);
        }
        self.actions.cancel_confirmation();
        self.actions.dispatches.clear();
    }

    pub(super) fn action_support_stopped(&mut self) {
        if self.actions.preparing.is_some() {
            self.actions.cancel_confirmation();
            self.notice = Notice::Message(MessageId::ActionRejected);
        }
    }

    /// A full scan may also be requested from the ordinary location controls.
    /// Only a full replacement can reconcile a failed refresh of another branch.
    pub(super) fn action_full_scan_accepted(&mut self, generation: diskpie_scan::ScanGeneration) {
        if self.actions.stale.is_some()
            && self.actions.pending.is_none()
            && self.actions.report.as_ref().is_some_and(|report| {
                !matches!(report.obligation, PostActionObligation::RescanRecycleBin { .. })
            })
        {
            self.actions.reconciliation = Some(Reconciliation::Scanning {
                kind: RefreshKind::All,
                generation: generation.into(),
            });
        }
    }

    fn render_confirmation(&mut self, ctx: &egui::Context) {
        let Some(presentation) = self.actions.flow.presentation().cloned() else {
            return;
        };
        // Only recycling is offered. A flow in any other kind cannot come from
        // this interface; drop it rather than render a permanent-delete or
        // empty-bin confirmation.
        let ConfirmationPresentation::Filesystem { target, .. } = &presentation else {
            self.actions.cancel_confirmation();
            return;
        };
        if presentation.kind() != ActionKind::Recycle {
            self.actions.cancel_confirmation();
            return;
        }
        let mut cancel = false;
        let mut accept = false;
        let response =
            egui::Modal::new(egui::Id::new("diskpie-action-confirmation")).show(ctx, |ui| {
                ui.set_max_width(560.0_f32.min(ctx.content_rect().width() - 32.0).max(220.0));
                egui::ScrollArea::vertical()
                    .max_height((ctx.content_rect().height() - 120.0).max(160.0))
                    .show(ui, |ui| {
                        ui.heading(self.strings.get(MessageId::ConfirmRecycleTitle));
                        ui.label(self.strings.get(MessageId::ReviewTarget));
                        if self
                            .actions
                            .subject
                            .as_ref()
                            .is_some_and(|subject| subject.identity().is_none())
                        {
                            ui.label(self.strings.get(MessageId::IdentityUnavailable));
                        }
                        ui.add(egui::Label::new(RichText::new(&target.display).monospace()).wrap());
                        if let Some(escaped) = &target.escaped_utf16 {
                            ui.add(egui::Label::new(RichText::new(escaped).monospace()).wrap());
                        }
                        if ui.button(self.strings.get(MessageId::CopyPath)).clicked() {
                            ctx.copy_text(target.escaped_utf16.clone().unwrap_or_else(|| {
                                target.exact_path.to_string_lossy().into_owned()
                            }));
                        }
                        ui.label(format!(
                            "{}: {}",
                            self.strings.get(MessageId::LogicalSize),
                            self.action_size(target.logical)
                        ));
                        ui.label(format!(
                            "{}: {}",
                            self.strings.get(MessageId::AllocatedData),
                            self.action_size(target.allocated)
                        ));
                        if target.reparse_note {
                            ui.label(self.strings.get(MessageId::ReparseAction));
                        }
                        ui.label(self.strings.get(MessageId::RecycleOnlyNote));
                        ui.separator();
                        ui.horizontal(|ui| {
                            let response = ui.button(self.strings.get(MessageId::Cancel));
                            if self.actions.focus_cancel {
                                response.request_focus();
                                self.actions.focus_cancel = false;
                            }
                            cancel = response.clicked();
                            accept = ui.button(self.strings.get(MessageId::Recycle)).clicked();
                        });
                    });
            });
        if cancel || response.should_close() {
            self.actions.cancel_confirmation();
            return;
        }
        if accept
            && let Ok(FlowStatus::Confirmed { .. }) = self.actions.flow.accept_review()
            && let Some(subject) = &self.actions.subject
        {
            self.prepare_file_action(subject.node(), FileAction::Recycle, true);
        }
    }

    fn action_size(&self, size: SizeSummary) -> String {
        if size.unknown_entries == 0 {
            format_iec_bytes(size.known_bytes, self.locale)
        } else if size.known_bytes == 0 {
            self.strings.get(MessageId::UnknownSize).to_owned()
        } else {
            format!(
                "{} {}",
                self.strings.get(MessageId::AtLeast),
                format_iec_bytes(size.known_bytes, self.locale)
            )
        }
    }

    pub(super) fn drive_reconciliation(&mut self) {
        if self.shutdown_requested {
            return;
        }
        let Some(refresh) = self.actions.reconciliation.take() else {
            return;
        };
        match refresh {
            Reconciliation::Needed(PostActionObligation::RescanRecycleBin { .. }) => {
                // No interface flow mutates the Recycle Bin, so nothing scanned
                // by DiskPie is stale because of it.
                self.actions.stale = None;
            }
            Reconciliation::Needed(obligation) => {
                if !self.action_scan_settled() {
                    self.actions.reconciliation = Some(Reconciliation::Needed(obligation));
                    return;
                }
                let before = self.runtime_scan_sequence();
                let parent = match obligation {
                    PostActionObligation::RescanParent { generation, parent, parent_path } => self
                        .runtime
                        .as_ref()
                        .and_then(|runtime| runtime.snapshot())
                        .filter(|snapshot| {
                            snapshot.generation() == generation
                                && snapshot.path(parent).ok().as_ref() == Some(&parent_path)
                        })
                        .map(|_| parent),
                    _ => None,
                };
                if let Some(parent) = parent {
                    self.request_branch(parent);
                    if self.branch_job.is_some() {
                        self.actions.reconciliation =
                            Some(Reconciliation::Resolving { kind: RefreshKind::Branch, before });
                    } else {
                        self.start_action_full_refresh(before);
                    }
                } else {
                    self.start_action_full_refresh(before);
                }
            }
            Reconciliation::Resolving { kind, before } => {
                let current = self.runtime_scan_sequence();
                if current > before {
                    self.actions.reconciliation = Some(Reconciliation::Scanning {
                        kind,
                        generation: GenerationId::new(current),
                    });
                } else if self.branch_job.is_some()
                    || self.scan_job.is_some()
                    || self.deferred_launch.is_some()
                {
                    self.actions.reconciliation = Some(Reconciliation::Resolving { kind, before });
                } else if kind == RefreshKind::Branch {
                    self.start_action_full_refresh(before);
                } else {
                    self.notice = Notice::Message(MessageId::ResultsRetained);
                }
            }
            Reconciliation::Scanning { kind, generation } => {
                let Some(runtime) = &self.runtime else {
                    return;
                };
                if matches!(runtime.state(), RuntimeState::Cancelling { generation: cancelled, .. }
                    if GenerationId::from(cancelled) == generation)
                {
                    self.branch_running = false;
                    self.notice = Notice::Message(MessageId::ResultsRetained);
                    return;
                }
                let terminal = runtime
                    .settled_summary()
                    .filter(|summary| GenerationId::from(summary.generation) == generation);
                if let Some(summary) = terminal {
                    if matches!(summary.phase, SessionPhase::Complete | SessionPhase::Partial)
                        && runtime
                            .snapshot()
                            .is_some_and(|snapshot| snapshot.generation() == generation)
                    {
                        self.actions.stale = None;
                        self.branch_running = false;
                    } else if matches!(summary.phase, SessionPhase::Cancelled) {
                        self.branch_running = false;
                        self.notice = Notice::Message(MessageId::ResultsRetained);
                    } else if matches!(summary.phase, SessionPhase::Failed(_))
                        || (!runtime.needs_repaint() && runtime.last_error().is_some())
                    {
                        self.branch_running = false;
                        if kind == RefreshKind::Branch {
                            self.start_action_full_refresh(generation.get());
                        } else {
                            self.notice = Notice::Message(MessageId::ResultsRetained);
                        }
                    } else {
                        self.actions.reconciliation =
                            Some(Reconciliation::Scanning { kind, generation });
                    }
                } else {
                    self.actions.reconciliation =
                        Some(Reconciliation::Scanning { kind, generation });
                }
            }
        }
    }

    fn start_action_full_refresh(&mut self, before: u64) {
        self.navigate(NavigationAction::RescanAll);
        if self.scan_job.is_some() || self.deferred_launch.is_some() {
            self.actions.reconciliation =
                Some(Reconciliation::Resolving { kind: RefreshKind::All, before });
        } else {
            self.notice = Notice::Message(MessageId::ResultsRetained);
        }
    }

    fn runtime_scan_sequence(&self) -> u64 {
        self.runtime.as_ref().map_or(0, |runtime| match runtime.state() {
            RuntimeState::Active { generation, .. }
            | RuntimeState::Settled { generation, .. }
            | RuntimeState::Cancelling { generation, .. } => generation.get(),
            _ => 0,
        })
    }
}

fn destructive_event(event: ShellEvent, kind: ActionKind) -> Option<ActionOutcome> {
    match (event, kind) {
        (ShellEvent::Recycle { outcome, .. }, ActionKind::Recycle)
        | (ShellEvent::DeletePermanently { outcome, .. }, ActionKind::DeletePermanently) => {
            Some(outcome.into())
        }
        (ShellEvent::EmptyRecycleBin { outcome, .. }, ActionKind::EmptyRecycleBin) => {
            Some(outcome.into())
        }
        _ => None,
    }
}

fn unknown_outcome(kind: ActionKind) -> ActionOutcome {
    match kind {
        ActionKind::EmptyRecycleBin => EmptyBinOutcome::UnknownMayHaveMutated {
            stage: FailureStage::ServiceShutdown,
            code: None,
            before: None,
            after: None,
        }
        .into(),
        _ => DestructiveOutcome::UnknownMayHaveMutated {
            stage: FailureStage::ServiceShutdown,
            code: None,
        }
        .into(),
    }
}

fn outcome_message(outcome: &ActionOutcome) -> MessageId {
    match outcome {
        ActionOutcome::Filesystem(DestructiveOutcome::Completed { .. })
        | ActionOutcome::RecycleBin(EmptyBinOutcome::Completed { .. }) => MessageId::ActionDone,
        ActionOutcome::Filesystem(DestructiveOutcome::CancelledBeforeMutation)
        | ActionOutcome::RecycleBin(EmptyBinOutcome::CancelledBeforeMutation) => {
            MessageId::ActionCancelled
        }
        ActionOutcome::Filesystem(DestructiveOutcome::Partial { .. }) => MessageId::ActionPartial,
        ActionOutcome::Filesystem(DestructiveOutcome::Failed { .. })
        | ActionOutcome::RecycleBin(EmptyBinOutcome::Failed { .. }) => MessageId::ActionFailed,
        ActionOutcome::Filesystem(DestructiveOutcome::UnknownMayHaveMutated { .. })
        | ActionOutcome::RecycleBin(EmptyBinOutcome::UnknownMayHaveMutated { .. }) => {
            MessageId::ActionUnknown
        }
        ActionOutcome::Filesystem(
            DestructiveOutcome::SafetyViolationUnexpectedPermanentDelete { .. },
        ) => MessageId::ActionHalted,
        ActionOutcome::Filesystem(DestructiveOutcome::TargetChanged { .. }) => {
            MessageId::ActionTargetChanged
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_app::actions::TargetValidator;
    use diskpie_core::{NodeSpec, OwnMetrics, TreeBuilder};

    fn target() -> FilesystemTarget {
        let mut builder = TreeBuilder::new(GenerationId::new(1));
        let root = builder.add_root(NodeSpec::root("C:\\fixture")).unwrap();
        let node =
            builder.add_child(root, NodeSpec::file("item", OwnMetrics::ZERO_BY_POLICY)).unwrap();
        let snapshot = builder.freeze().unwrap();
        TargetValidator::new(&snapshot, Some(std::path::Path::new("C:\\DiskPie\\diskpie.exe")))
            .validate(node, GenerationId::new(1), false, ActionPurpose::Destructive)
            .unwrap()
    }

    fn obligation() -> PendingObligation {
        let target = target();
        let mut flow = ConfirmationFlow::new();
        flow.begin_recycle(target).unwrap();
        flow.accept_review().unwrap();
        flow.take_recycle().unwrap().consume().1
    }

    #[test]
    fn late_or_duplicate_terminal_never_settles_a_different_attempt() {
        let mut state = ActionUi {
            pending: Some(PendingAction { request_id: 7, obligation: obligation() }),
            ..ActionUi::default()
        };
        assert!(!state.complete(6, DestructiveOutcome::CancelledBeforeMutation.into()));
        assert!(state.pending.is_some());
        assert!(state.complete(7, DestructiveOutcome::CancelledBeforeMutation.into()));
        assert!(!state.complete(7, DestructiveOutcome::CancelledBeforeMutation.into()));
        assert!(matches!(
            state.reconciliation,
            Some(Reconciliation::Needed(PostActionObligation::RescanParent { .. }))
        ));
    }

    #[test]
    fn shutdown_handoff_keeps_obligation_and_reports_uncertainty() {
        let mut state = ActionUi {
            pending: Some(PendingAction { request_id: 7, obligation: obligation() }),
            ..ActionUi::default()
        };
        let pending = state.take_pending().unwrap();
        assert!(state.take_pending().is_none());
        let report = pending.settle(None);
        assert!(matches!(
            report.outcome,
            ActionOutcome::Filesystem(DestructiveOutcome::UnknownMayHaveMutated {
                stage: FailureStage::ServiceShutdown,
                ..
            })
        ));
        assert!(matches!(report.obligation, PostActionObligation::RescanParent { .. }));
    }

    #[test]
    fn safety_violation_blocks_later_mutations_but_retains_refresh_obligation() {
        let mut state = ActionUi {
            pending: Some(PendingAction { request_id: 7, obligation: obligation() }),
            ..ActionUi::default()
        };
        state.complete(
            7,
            DestructiveOutcome::SafetyViolationUnexpectedPermanentDelete { items: Vec::new() }
                .into(),
        );
        assert!(state.halted);
        assert!(state.reconciliation.is_some());
        assert_eq!(
            outcome_message(&state.report.as_ref().unwrap().outcome),
            MessageId::ActionHalted
        );
    }

    fn frame(
        shell: &mut DiskPieShell,
        ctx: &egui::Context,
        width: f32,
        enter: bool,
    ) -> egui::FullOutput {
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 640.0),
            )),
            ..Default::default()
        };
        if enter {
            input.events.push(egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            });
        }
        ctx.run_ui(input, |ui| {
            shell.render_actions(ui);
        })
    }

    fn painted_text(output: &egui::FullOutput) -> String {
        fn collect(shape: &egui::Shape, text: &mut String) {
            match shape {
                egui::Shape::Text(shape) => {
                    text.push_str(shape.galley.text());
                    text.push('\n');
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, text);
                    }
                }
                _ => {}
            }
        }
        let mut text = String::new();
        for shape in &output.shapes {
            collect(&shape.shape, &mut text);
        }
        text
    }

    #[test]
    fn exact_target_renders_in_both_locales_and_cancel_has_default_keyboard_focus() {
        use diskpie_app::i18n::{I18n, Locale};
        for locale in Locale::ALL {
            for width in [420.0, 1100.0] {
                let (mut shell, ctx) = super::super::tests::headless_shell(None);
                shell.locale = locale;
                shell.i18n = I18n::new(locale).unwrap();
                shell.strings = super::super::UiStrings::new(&shell.i18n);
                let target = target();
                shell.actions.subject = Some(target.clone());
                shell.actions.flow.begin_recycle(target).unwrap();
                shell.actions.focus_cancel = true;
                let _ = frame(&mut shell, &ctx, width, false);
                let output = frame(&mut shell, &ctx, width, false);
                let text = painted_text(&output);
                assert!(text.contains("C:\\fixture\\item"), "exact target missing: {text}");
                assert!(text.contains(shell.strings.get(MessageId::IdentityUnavailable)));
                assert!(text.contains(shell.strings.get(MessageId::ConfirmRecycleTitle)));
                assert!(text.contains(shell.strings.get(MessageId::RecycleOnlyNote)));
                let _ = frame(&mut shell, &ctx, width, true);
                assert_eq!(
                    shell.actions.flow.status(),
                    FlowStatus::Idle,
                    "Enter must activate Cancel by default"
                );
                assert!(shell.actions.pending.is_none());
            }
        }
    }

    #[test]
    fn permanent_delete_and_empty_bin_flows_are_never_rendered() {
        let (mut shell, ctx) = super::super::tests::headless_shell(None);
        let target = target();
        shell.actions.subject = Some(target.clone());
        shell.actions.flow.begin_delete(target).unwrap();
        let _ = frame(&mut shell, &ctx, 800.0, false);
        assert_eq!(shell.actions.flow.status(), FlowStatus::Idle);
        assert!(shell.actions.subject.is_none());
        shell
            .actions
            .flow
            .begin_empty_recycle_bin(
                diskpie_app::actions::RecycleBinScope::AllDrives,
                GenerationId::new(1),
                None,
            )
            .unwrap();
        let output = frame(&mut shell, &ctx, 800.0, false);
        assert_eq!(shell.actions.flow.status(), FlowStatus::Idle);
        assert!(!painted_text(&output).contains(shell.strings.get(MessageId::ReviewTarget)));
    }

    #[test]
    fn service_loss_settles_once_and_status_remains_visible_without_panel_space() {
        let (mut shell, ctx) = super::super::tests::headless_shell(None);
        shell.actions.pending = Some(PendingAction { request_id: 9, obligation: obligation() });
        shell.actions.stale = Some(GenerationId::new(1));
        shell.action_service_stopped();
        shell.action_service_stopped();
        assert!(shell.actions.pending.is_none());
        assert!(matches!(
            shell.actions.report.as_ref().unwrap().outcome,
            ActionOutcome::Filesystem(DestructiveOutcome::UnknownMayHaveMutated { .. })
        ));
        let _ = frame(&mut shell, &ctx, 420.0, false);
        let output = frame(&mut shell, &ctx, 420.0, false);
        let text = painted_text(&output);
        assert!(text.contains(shell.strings.get(MessageId::ActionUnknown)));
        assert!(text.contains(shell.strings.get(MessageId::ResultsStale)));
        assert!(shell.actions.needs_repaint());
    }
}
