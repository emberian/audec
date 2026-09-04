//! Targeted GPUI workbench for artifact-backed explain/promote/compare flows.
//!
//! The view owns presentation state only. It never manufactures a candidate,
//! PCM buffer, project edit, renderer, or comparison observation. Every
//! authoritative operation leaves through [`ExplanationWorkbenchEvent`] and
//! every visible result comes back as an immutable typed object produced by
//! the artifact/promotion/comparison controllers.

use std::fmt;
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, rgb, App, Context, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Window,
};

use crate::artifact_catalog::comparison_hydration::{
    ArtifactComparisonPayload, ArtifactComparisonPin,
};
use crate::artifact_catalog::{ArtifactDescriptor, ArtifactId, ContentDigest};
use crate::artifact_promotion_bridge::{
    ArtifactPromotionBridgeError, ArtifactPromotionComparisonPlan,
    ArtifactPromotionComparisonRequest, ArtifactPromotionComparisonResult,
};
use crate::comparison::ComparisonMetrics;
use crate::comparison_controller::ComparisonChannel;
use crate::comparison_runtime::executor::ComparisonProductCompletion;
use crate::daw_project::ProjectRevisions;
use crate::deprojection_execution::promotion::{CreatedObject, PromotionRefusal};
use crate::deprojection_program::EvidenceRef;
use crate::reverse_navigation::ReverseTargetDescriptor;

const BACKGROUND: u32 = 0x090b10;
const PANEL: u32 = 0x10141d;
const PANEL_ALT: u32 = 0x0d1118;
const RAISED: u32 = 0x171d28;
const BORDER: u32 = 0x252c38;
const TEXT: u32 = 0xe8edf5;
const MUTED: u32 = 0x8c98a9;
const DIM: u32 = 0x596579;
const CYAN: u32 = 0x50d8d7;
const MAGENTA: u32 = 0xf172b6;
const AMBER: u32 = 0xf6b760;
const LIME: u32 = 0xa7d877;

pub type ExplanationWorkbenchCallback =
    Arc<dyn Fn(ExplanationWorkbenchEvent) + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkbenchActionId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkbenchOperation {
    Plan,
    Execute,
    Render,
    Capture(ComparisonChannel),
    Undo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkbenchPhase {
    Draft,
    Planning,
    PlanReady,
    Refused,
    Applying,
    Promoted,
    Rendering,
    RenderReady,
    Capturing(ComparisonChannel),
    ComparisonReady,
    Undoing,
    Undone,
    Cancelling,
    Cancelled,
    Stale,
    Failed,
}

/// Tiny pure reducer used by the GPUI model and by headless transition tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkbenchTransitionModel {
    pub phase: WorkbenchPhase,
    pub in_flight: Option<WorkbenchOperation>,
}

impl Default for WorkbenchTransitionModel {
    fn default() -> Self {
        Self {
            phase: WorkbenchPhase::Draft,
            in_flight: None,
        }
    }
}

impl WorkbenchTransitionModel {
    pub fn request(&mut self, operation: WorkbenchOperation) -> Result<(), WorkbenchModelError> {
        if self.in_flight.is_some() {
            return Err(WorkbenchModelError::OperationInFlight);
        }
        let phase = match (self.phase, operation) {
            (
                WorkbenchPhase::Draft
                | WorkbenchPhase::Refused
                | WorkbenchPhase::Cancelled
                | WorkbenchPhase::Stale
                | WorkbenchPhase::Failed
                | WorkbenchPhase::Undone,
                WorkbenchOperation::Plan,
            ) => WorkbenchPhase::Planning,
            (WorkbenchPhase::PlanReady, WorkbenchOperation::Execute) => WorkbenchPhase::Applying,
            (WorkbenchPhase::Promoted, WorkbenchOperation::Render) => WorkbenchPhase::Rendering,
            (
                WorkbenchPhase::RenderReady | WorkbenchPhase::ComparisonReady,
                WorkbenchOperation::Capture(channel),
            ) => WorkbenchPhase::Capturing(channel),
            (
                WorkbenchPhase::Promoted
                | WorkbenchPhase::RenderReady
                | WorkbenchPhase::ComparisonReady,
                WorkbenchOperation::Undo,
            ) => WorkbenchPhase::Undoing,
            _ => {
                return Err(WorkbenchModelError::InvalidTransition {
                    phase: self.phase,
                    operation,
                });
            }
        };
        self.phase = phase;
        self.in_flight = Some(operation);
        Ok(())
    }

    pub fn complete(&mut self, operation: WorkbenchOperation) -> Result<(), WorkbenchModelError> {
        if self.in_flight != Some(operation) {
            return Err(WorkbenchModelError::StaleCompletion {
                expected: self.in_flight,
                actual: operation,
            });
        }
        self.phase = match operation {
            WorkbenchOperation::Plan => WorkbenchPhase::PlanReady,
            WorkbenchOperation::Execute => WorkbenchPhase::Promoted,
            WorkbenchOperation::Render => WorkbenchPhase::RenderReady,
            WorkbenchOperation::Capture(_) => WorkbenchPhase::ComparisonReady,
            WorkbenchOperation::Undo => WorkbenchPhase::Undone,
        };
        self.in_flight = None;
        Ok(())
    }

    pub fn refuse_plan(&mut self) -> Result<(), WorkbenchModelError> {
        if self.in_flight != Some(WorkbenchOperation::Plan) {
            return Err(WorkbenchModelError::StaleCompletion {
                expected: self.in_flight,
                actual: WorkbenchOperation::Plan,
            });
        }
        self.phase = WorkbenchPhase::Refused;
        self.in_flight = None;
        Ok(())
    }

    pub fn begin_cancel(&mut self) -> Result<WorkbenchOperation, WorkbenchModelError> {
        let operation = self.in_flight.ok_or(WorkbenchModelError::NothingToCancel)?;
        self.phase = WorkbenchPhase::Cancelling;
        Ok(operation)
    }

    pub fn cancelled(&mut self, operation: WorkbenchOperation) -> Result<(), WorkbenchModelError> {
        if self.in_flight != Some(operation) {
            return Err(WorkbenchModelError::StaleCompletion {
                expected: self.in_flight,
                actual: operation,
            });
        }
        self.phase = WorkbenchPhase::Cancelled;
        self.in_flight = None;
        Ok(())
    }

    pub fn stale(&mut self) {
        self.phase = WorkbenchPhase::Stale;
        self.in_flight = None;
    }

    pub fn failed(&mut self) {
        self.phase = WorkbenchPhase::Failed;
        self.in_flight = None;
    }
}

/// The pane's verbs that change comparison or project state. These are not
/// reveals and no longer share an enum with one.
#[derive(Clone, Debug)]
pub enum WorkbenchCommand {
    Plan {
        action: WorkbenchActionId,
        request: ArtifactPromotionComparisonRequest,
    },
    Execute {
        action: WorkbenchActionId,
        plan: Arc<ArtifactPromotionComparisonPlan>,
    },
    Render {
        action: WorkbenchActionId,
        result: Arc<ArtifactPromotionComparisonResult>,
    },
    Capture {
        action: WorkbenchActionId,
        result: Arc<ArtifactPromotionComparisonResult>,
        channel: ComparisonChannel,
    },
    Undo {
        action: WorkbenchActionId,
        result: Arc<ArtifactPromotionComparisonResult>,
    },
    Cancel {
        action: WorkbenchActionId,
        operation: WorkbenchOperation,
    },
}

/// Complete authority boundary for the pane.
///
/// A reveal names a reverse identity and nothing else; lowering it to an
/// [`ObjectRef`](crate::project_controller::ObjectRef) or to a named refusal
/// is `reverse_navigation`'s job, not the pane's and not the host's.
#[derive(Clone, Debug)]
pub enum ExplanationWorkbenchEvent {
    Command(WorkbenchCommand),
    Reveal(ReverseTargetDescriptor),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkbenchDiagnosticKind {
    Information,
    Cancelled,
    StaleRevision,
    StalePublication,
    StaleDocument,
    StaleSelection,
    StaleCatalog,
    IsolationRequired,
    Failure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkbenchDiagnostic {
    pub kind: WorkbenchDiagnosticKind,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct ExplanationWorkbenchSnapshot {
    pub phase: WorkbenchPhase,
    pub descriptor: ArtifactDescriptor,
    pub pin: ArtifactComparisonPin,
    pub request: ArtifactPromotionComparisonRequest,
    pub signal_count: usize,
    pub plan: Option<Arc<ArtifactPromotionComparisonPlan>>,
    pub result: Option<Arc<ArtifactPromotionComparisonResult>>,
    pub completion: Option<Arc<ComparisonProductCompletion>>,
    pub selected_channel: ComparisonChannel,
    pub refusals: Vec<PromotionRefusal>,
    pub diagnostics: Vec<WorkbenchDiagnostic>,
}

pub struct ExplanationWorkbenchPaneModel {
    descriptor: ArtifactDescriptor,
    payload: Arc<ArtifactComparisonPayload>,
    request: ArtifactPromotionComparisonRequest,
    transition: WorkbenchTransitionModel,
    next_action: u64,
    active_action: Option<WorkbenchActionId>,
    plan: Option<Arc<ArtifactPromotionComparisonPlan>>,
    result: Option<Arc<ArtifactPromotionComparisonResult>>,
    completion: Option<Arc<ComparisonProductCompletion>>,
    selected_channel: ComparisonChannel,
    refusals: Vec<PromotionRefusal>,
    diagnostics: Vec<WorkbenchDiagnostic>,
}

impl ExplanationWorkbenchPaneModel {
    pub fn new(
        descriptor: ArtifactDescriptor,
        payload: Arc<ArtifactComparisonPayload>,
        request: ArtifactPromotionComparisonRequest,
    ) -> Result<Self, WorkbenchModelError> {
        descriptor
            .validate()
            .map_err(|error| WorkbenchModelError::InvalidInput(error.to_string()))?;
        if descriptor.id != request.artifact
            || descriptor.id != request.artifact_pin.artifact
            || descriptor.source_digest != request.artifact_pin.source_digest
            || descriptor.recipe_digest != request.artifact_pin.recipe_digest
            || payload.pin != request.artifact_pin
        {
            return Err(WorkbenchModelError::ArtifactPinMismatch);
        }
        if payload.signals().len() == 0 {
            return Err(WorkbenchModelError::EmptyPayload);
        }
        Ok(Self {
            descriptor,
            payload,
            request,
            transition: WorkbenchTransitionModel::default(),
            next_action: 1,
            active_action: None,
            plan: None,
            result: None,
            completion: None,
            selected_channel: ComparisonChannel::Residual,
            refusals: Vec::new(),
            diagnostics: Vec::new(),
        })
    }

    pub fn snapshot(&self) -> ExplanationWorkbenchSnapshot {
        ExplanationWorkbenchSnapshot {
            phase: self.transition.phase,
            descriptor: self.descriptor.clone(),
            pin: self.payload.pin,
            request: self.request.clone(),
            signal_count: self.payload.signals().len(),
            plan: self.plan.clone(),
            result: self.result.clone(),
            completion: self.completion.clone(),
            selected_channel: self.selected_channel,
            refusals: self.refusals.clone(),
            diagnostics: self.diagnostics.clone(),
        }
    }

    pub fn request_plan(&mut self) -> Result<ExplanationWorkbenchEvent, WorkbenchModelError> {
        self.transition.request(WorkbenchOperation::Plan)?;
        self.plan = None;
        self.result = None;
        self.completion = None;
        self.refusals.clear();
        self.diagnostics.clear();
        let action = self.begin_action()?;
        Ok(ExplanationWorkbenchEvent::Command(WorkbenchCommand::Plan {
            action,
            request: self.request.clone(),
        }))
    }

    pub fn accept_plan(
        &mut self,
        action: WorkbenchActionId,
        plan: Arc<ArtifactPromotionComparisonPlan>,
    ) -> Result<(), WorkbenchModelError> {
        self.require_action(action)?;
        if plan.descriptor() != &self.descriptor
            || plan.payload().pin != self.payload.pin
            || plan.base_revisions() != self.request.artifact_pin.project_revisions
            || plan.workspace_pin() != self.request.workspace_pin
        {
            return Err(WorkbenchModelError::PlanMismatch);
        }
        self.transition.complete(WorkbenchOperation::Plan)?;
        self.active_action = None;
        self.plan = Some(plan);
        self.diagnostics.push(WorkbenchDiagnostic {
            kind: WorkbenchDiagnosticKind::Information,
            message: "Promotion preview compiled without mutation".into(),
        });
        Ok(())
    }

    pub fn accept_refusals(
        &mut self,
        action: WorkbenchActionId,
        refusals: Vec<PromotionRefusal>,
    ) -> Result<(), WorkbenchModelError> {
        self.require_action(action)?;
        if refusals.is_empty() {
            return Err(WorkbenchModelError::EmptyRefusal);
        }
        self.transition.refuse_plan()?;
        self.active_action = None;
        self.refusals = refusals;
        Ok(())
    }

    pub fn request_execute(&mut self) -> Result<ExplanationWorkbenchEvent, WorkbenchModelError> {
        let plan = self.plan.clone().ok_or(WorkbenchModelError::MissingPlan)?;
        self.transition.request(WorkbenchOperation::Execute)?;
        let action = self.begin_action()?;
        Ok(ExplanationWorkbenchEvent::Command(
            WorkbenchCommand::Execute { action, plan },
        ))
    }

    pub fn accept_promotion(
        &mut self,
        action: WorkbenchActionId,
        result: Arc<ArtifactPromotionComparisonResult>,
    ) -> Result<(), WorkbenchModelError> {
        self.require_action(action)?;
        if result.descriptor != self.descriptor
            || result.artifact_pin != self.payload.pin
            || result.candidate.id != self.request.candidate.id
        {
            return Err(WorkbenchModelError::PromotionMismatch);
        }
        self.transition.complete(WorkbenchOperation::Execute)?;
        self.active_action = None;
        self.result = Some(result);
        self.diagnostics.push(WorkbenchDiagnostic {
            kind: WorkbenchDiagnosticKind::Information,
            message: "Atomic promotion committed; shared render is required".into(),
        });
        Ok(())
    }

    pub fn request_render(&mut self) -> Result<ExplanationWorkbenchEvent, WorkbenchModelError> {
        let result = self
            .result
            .clone()
            .ok_or(WorkbenchModelError::MissingPromotion)?;
        self.transition.request(WorkbenchOperation::Render)?;
        let action = self.begin_action()?;
        Ok(ExplanationWorkbenchEvent::Command(
            WorkbenchCommand::Render { action, result },
        ))
    }

    pub fn accept_render(
        &mut self,
        action: WorkbenchActionId,
        revisions: ProjectRevisions,
        publication_generation: u64,
    ) -> Result<(), WorkbenchModelError> {
        self.require_action(action)?;
        let result = self
            .result
            .as_ref()
            .ok_or(WorkbenchModelError::MissingPromotion)?;
        if result.promoted_revisions() != revisions
            || result.promoted_publication_generation() != publication_generation
        {
            return Err(WorkbenchModelError::RenderPinMismatch);
        }
        self.transition.complete(WorkbenchOperation::Render)?;
        self.active_action = None;
        self.diagnostics.push(WorkbenchDiagnostic {
            kind: WorkbenchDiagnosticKind::Information,
            message: "Authoritative shared schedule is ready".into(),
        });
        Ok(())
    }

    pub fn request_channel(
        &mut self,
        channel: ComparisonChannel,
    ) -> Result<ExplanationWorkbenchEvent, WorkbenchModelError> {
        let result = self
            .result
            .clone()
            .ok_or(WorkbenchModelError::MissingPromotion)?;
        self.transition
            .request(WorkbenchOperation::Capture(channel))?;
        self.selected_channel = channel;
        let action = self.begin_action()?;
        Ok(ExplanationWorkbenchEvent::Command(
            WorkbenchCommand::Capture {
                action,
                result,
                channel,
            },
        ))
    }

    pub fn accept_comparison(
        &mut self,
        action: WorkbenchActionId,
        completion: Arc<ComparisonProductCompletion>,
    ) -> Result<(), WorkbenchModelError> {
        self.require_action(action)?;
        let operation = self
            .transition
            .in_flight
            .ok_or(WorkbenchModelError::OperationInFlight)?;
        let WorkbenchOperation::Capture(channel) = operation else {
            return Err(WorkbenchModelError::InvalidTransition {
                phase: self.transition.phase,
                operation,
            });
        };
        let result = self
            .result
            .as_ref()
            .ok_or(WorkbenchModelError::MissingPromotion)?;
        if completion.request.comparison != self.request.target.comparison
            || completion.request.explanation != self.request.target.explanation
            || completion.request.channel != channel
            || completion.producing_revision != result.promoted_revisions()
        {
            return Err(WorkbenchModelError::ComparisonMismatch);
        }
        self.transition.complete(operation)?;
        self.active_action = None;
        self.selected_channel = channel;
        self.completion = Some(completion);
        Ok(())
    }

    pub fn request_undo(&mut self) -> Result<ExplanationWorkbenchEvent, WorkbenchModelError> {
        let result = self
            .result
            .clone()
            .ok_or(WorkbenchModelError::MissingPromotion)?;
        self.transition.request(WorkbenchOperation::Undo)?;
        let action = self.begin_action()?;
        Ok(ExplanationWorkbenchEvent::Command(WorkbenchCommand::Undo {
            action,
            result,
        }))
    }

    pub fn accept_undo(&mut self, action: WorkbenchActionId) -> Result<(), WorkbenchModelError> {
        self.require_action(action)?;
        self.transition.complete(WorkbenchOperation::Undo)?;
        self.active_action = None;
        self.plan = None;
        self.result = None;
        self.completion = None;
        self.diagnostics.push(WorkbenchDiagnostic {
            kind: WorkbenchDiagnosticKind::Information,
            message: "Atomic promotion removed by one undo".into(),
        });
        Ok(())
    }

    pub fn request_cancel(&mut self) -> Result<ExplanationWorkbenchEvent, WorkbenchModelError> {
        let operation = self.transition.begin_cancel()?;
        let action = self
            .active_action
            .ok_or(WorkbenchModelError::NothingToCancel)?;
        Ok(ExplanationWorkbenchEvent::Command(
            WorkbenchCommand::Cancel { action, operation },
        ))
    }

    pub fn accept_cancelled(
        &mut self,
        action: WorkbenchActionId,
    ) -> Result<(), WorkbenchModelError> {
        self.require_action(action)?;
        let operation = self
            .transition
            .in_flight
            .ok_or(WorkbenchModelError::NothingToCancel)?;
        self.transition.cancelled(operation)?;
        self.active_action = None;
        self.diagnostics.push(WorkbenchDiagnostic {
            kind: WorkbenchDiagnosticKind::Cancelled,
            message: format!("{operation:?} cancelled before publication"),
        });
        Ok(())
    }

    pub fn reject(
        &mut self,
        action: WorkbenchActionId,
        error: ArtifactPromotionBridgeError,
    ) -> Result<(), WorkbenchModelError> {
        self.require_action(action)?;
        if let ArtifactPromotionBridgeError::PromotionCompile(
            crate::deprojection_execution::promotion::PromotionCompileError::Refused(refusals),
        ) = error
        {
            return self.accept_refusals(action, refusals);
        }
        let (phase, kind) = match &error {
            ArtifactPromotionBridgeError::Cancelled => (
                WorkbenchPhase::Cancelled,
                WorkbenchDiagnosticKind::Cancelled,
            ),
            ArtifactPromotionBridgeError::StaleArtifactRevision { .. } => (
                WorkbenchPhase::Stale,
                WorkbenchDiagnosticKind::StaleRevision,
            ),
            ArtifactPromotionBridgeError::PublicationSuperseded { .. } => (
                WorkbenchPhase::Stale,
                WorkbenchDiagnosticKind::StalePublication,
            ),
            ArtifactPromotionBridgeError::DocumentSuperseded { .. } => (
                WorkbenchPhase::Stale,
                WorkbenchDiagnosticKind::StaleDocument,
            ),
            ArtifactPromotionBridgeError::SelectionSuperseded { .. } => (
                WorkbenchPhase::Stale,
                WorkbenchDiagnosticKind::StaleSelection,
            ),
            ArtifactPromotionBridgeError::ArtifactCatalogSuperseded { .. } => {
                (WorkbenchPhase::Stale, WorkbenchDiagnosticKind::StaleCatalog)
            }
            ArtifactPromotionBridgeError::IsolationBackendRequired(_) => (
                WorkbenchPhase::Failed,
                WorkbenchDiagnosticKind::IsolationRequired,
            ),
            _ => (WorkbenchPhase::Failed, WorkbenchDiagnosticKind::Failure),
        };
        self.transition.phase = phase;
        self.transition.in_flight = None;
        self.active_action = None;
        self.diagnostics.push(WorkbenchDiagnostic {
            kind,
            message: error.to_string(),
        });
        Ok(())
    }

    pub fn reveal_artifact(&self) -> ExplanationWorkbenchEvent {
        ExplanationWorkbenchEvent::Reveal(ReverseTargetDescriptor::Artifact(self.descriptor.id))
    }

    /// Evidence carries the artifact whose content minted it, which is the
    /// scope a finding needs; without it a reveal could only guess.
    pub fn reveal_evidence(&self, evidence: EvidenceRef) -> ExplanationWorkbenchEvent {
        ExplanationWorkbenchEvent::Reveal(ReverseTargetDescriptor::Evidence {
            artifact: Some(self.descriptor.id),
            evidence,
        })
    }

    pub fn reveal_created(&self, object: CreatedObject) -> ExplanationWorkbenchEvent {
        ExplanationWorkbenchEvent::Reveal(ReverseTargetDescriptor::Created(object))
    }

    fn begin_action(&mut self) -> Result<WorkbenchActionId, WorkbenchModelError> {
        let action = WorkbenchActionId(self.next_action);
        self.next_action = self
            .next_action
            .checked_add(1)
            .ok_or(WorkbenchModelError::ActionExhausted)?;
        self.active_action = Some(action);
        Ok(action)
    }

    fn require_action(&self, action: WorkbenchActionId) -> Result<(), WorkbenchModelError> {
        if self.active_action == Some(action) {
            Ok(())
        } else {
            Err(WorkbenchModelError::StaleAction {
                expected: self.active_action,
                actual: action,
            })
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkbenchModelError {
    InvalidInput(String),
    ArtifactPinMismatch,
    EmptyPayload,
    OperationInFlight,
    InvalidTransition {
        phase: WorkbenchPhase,
        operation: WorkbenchOperation,
    },
    StaleCompletion {
        expected: Option<WorkbenchOperation>,
        actual: WorkbenchOperation,
    },
    StaleAction {
        expected: Option<WorkbenchActionId>,
        actual: WorkbenchActionId,
    },
    NothingToCancel,
    ActionExhausted,
    EmptyRefusal,
    MissingPlan,
    MissingPromotion,
    PlanMismatch,
    PromotionMismatch,
    RenderPinMismatch,
    ComparisonMismatch,
}

impl fmt::Display for WorkbenchModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "explanation workbench: {self:?}")
    }
}

impl std::error::Error for WorkbenchModelError {}

pub struct ExplanationWorkbenchView {
    model: ExplanationWorkbenchPaneModel,
    callback: ExplanationWorkbenchCallback,
    focus_handle: FocusHandle,
    feedback: Option<(bool, String)>,
}

impl ExplanationWorkbenchView {
    pub fn new(
        model: ExplanationWorkbenchPaneModel,
        callback: ExplanationWorkbenchCallback,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            model,
            callback,
            focus_handle: cx.focus_handle().tab_stop(true),
            feedback: None,
        }
    }

    pub fn snapshot(&self) -> ExplanationWorkbenchSnapshot {
        self.model.snapshot()
    }

    pub fn model(&self) -> &ExplanationWorkbenchPaneModel {
        &self.model
    }

    pub fn model_mut(&mut self) -> &mut ExplanationWorkbenchPaneModel {
        &mut self.model
    }

    /// Host adapters call this after applying one typed model completion.
    pub fn notify_model_changed(&mut self, cx: &mut Context<Self>) {
        self.feedback = None;
        cx.notify();
    }

    /// Publish a host-side typed refusal (for example, a retained evidence
    /// reference which has no product-level workspace address) in the pane
    /// that initiated it. This prevents callback handoff from looking like a
    /// successful reveal when navigation could not honestly be performed.
    pub fn report_host_diagnostic(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.feedback = Some((true, message.into()));
        cx.notify();
    }

    fn emit_result(
        &mut self,
        result: Result<ExplanationWorkbenchEvent, WorkbenchModelError>,
        success: &'static str,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(event) => {
                (self.callback)(event);
                self.feedback = Some((false, success.into()));
            }
            Err(error) => self.feedback = Some((true, error.to_string())),
        }
        cx.notify();
    }

    fn plan(&mut self, cx: &mut Context<Self>) {
        let result = self.model.request_plan();
        self.emit_result(result, "Promotion preview requested", cx);
    }

    fn execute(&mut self, cx: &mut Context<Self>) {
        let result = self.model.request_execute();
        self.emit_result(result, "Atomic promotion requested", cx);
    }

    fn render_shared(&mut self, cx: &mut Context<Self>) {
        let result = self.model.request_render();
        self.emit_result(result, "Authoritative render requested", cx);
    }

    fn capture(&mut self, channel: ComparisonChannel, cx: &mut Context<Self>) {
        let result = self.model.request_channel(channel);
        self.emit_result(result, channel_feedback(channel), cx);
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let result = self.model.request_undo();
        self.emit_result(result, "Atomic undo requested", cx);
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        let result = self.model.request_cancel();
        self.emit_result(result, "Cancellation requested", cx);
    }

    fn reveal_created(&mut self, object: CreatedObject, cx: &mut Context<Self>) {
        (self.callback)(self.model.reveal_created(object));
        self.feedback = Some((false, "Promoted object reveal requested".into()));
        cx.notify();
    }

    fn render_identity(
        &self,
        snapshot: &ExplanationWorkbenchSnapshot,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        section("ARTIFACT / GENERATION PINS")
            .child(detail("ARTIFACT", digest_label(snapshot.descriptor.id.0)))
            .child(detail(
                "SOURCE",
                digest_label(snapshot.descriptor.source_digest),
            ))
            .child(detail(
                "RECIPE",
                digest_label(snapshot.descriptor.recipe_digest),
            ))
            .child(detail(
                "OUTPUT",
                digest_label(snapshot.descriptor.output_digest),
            ))
            .child(detail(
                "GEOMETRY",
                format!(
                    "{}..{} · {} Hz · {} ch · {} exact signal(s)",
                    snapshot.descriptor.extent.start,
                    snapshot.descriptor.extent.end,
                    snapshot.descriptor.sample_rate,
                    snapshot.descriptor.channels,
                    snapshot.signal_count
                ),
            ))
            .child(detail(
                "PROJECT PIN",
                revision_label(snapshot.pin.project_revisions),
            ))
            .child(detail(
                "GENERATIONS",
                format!(
                    "publication {} · catalog {} · document {}",
                    snapshot.pin.publication_generation,
                    snapshot.pin.catalog_generation,
                    snapshot.request.workspace_pin.document_generation
                ),
            ))
            .child(paragraph(
                "Evidence artifact only · no standalone workspace address",
            ))
    }

    fn render_candidate(
        &self,
        snapshot: &ExplanationWorkbenchSnapshot,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let candidate = &snapshot.request.candidate;
        let mut body = section("SOURCE PROGRAM / EVIDENCE")
            .child(detail("CANDIDATE", digest_label(candidate.id.0)))
            .child(detail("LABEL", candidate.label.clone()))
            .child(detail("ORIGIN", format!("{:?}", candidate.origin)))
            .child(detail(
                "MATERIAL",
                format!(
                    "{} +{} frames · {} Hz · {} ch",
                    candidate.program.source.start_frame,
                    candidate.program.source.frame_count,
                    candidate.program.source.sample_rate_hz,
                    candidate.program.source.channels
                ),
            ))
            .child(detail(
                "PROGRAM",
                format!(
                    "{} roots · {} terms · {} source claims",
                    candidate.program.roots.len(),
                    candidate.program.terms.len(),
                    candidate.source_claims.len()
                ),
            ))
            .child(detail(
                "STRUCTURAL SCORE",
                format!(
                    "{} bytes · {} free parameters · {} exact-audio terms",
                    candidate.structural_score.description_bytes,
                    candidate.structural_score.free_parameters,
                    candidate.structural_score.exact_audio_terms
                ),
            ));
        for (root_index, root) in candidate.program.roots.iter().enumerate() {
            if let Some(term) = candidate.program.terms.get(root) {
                body = body.child(
                    div()
                        .mt_2()
                        .p_3()
                        .rounded_md()
                        .bg(rgb(RAISED))
                        .child(detail("ROOT", digest_label(root.0)))
                        .child(detail("TERM", format!("{:?}", term.kind)))
                        .child(detail("DERIVATION", term.derivation.rule.clone())),
                );
                for (evidence_index, evidence) in term.evidence.iter().enumerate() {
                    let label = format!("{:?}", evidence);
                    body = body.child(info_row(
                        format!("candidate-evidence-{root_index}-{evidence_index}"),
                        label,
                        "Evidence-only reference · no standalone workspace address",
                        CYAN,
                    ));
                }
            }
        }
        for caveat in &candidate.caveats {
            body = body.child(paragraph(caveat));
        }
        body
    }

    fn render_promotion(
        &self,
        snapshot: &ExplanationWorkbenchSnapshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let bindings = &snapshot.request.bindings;
        let mut body = section("PROMOTION PREVIEW / RETAINED EXPRESSION")
            .child(detail("PHASE", format!("{:?}", snapshot.phase)))
            .child(detail(
                "PLACEMENT",
                format!(
                    "frame {} · cycle {} ticks · curve step {} frames",
                    snapshot.request.placement.start_frame,
                    snapshot.request.placement.cycle.0,
                    snapshot.request.placement.curve_resolution_frames
                ),
            ))
            .child(detail(
                "EXPLICIT BINDINGS",
                format!(
                    "{} source · {} preset · {} curve · note instrument {}",
                    bindings.source_assets.len(),
                    bindings.preset_instruments.len(),
                    bindings.curve_targets.len(),
                    if bindings.note_instrument.is_some() {
                        "yes"
                    } else {
                        "no"
                    }
                ),
            ));
        if let Some(plan) = &snapshot.plan {
            body = body.child(detail(
                "COMPILED PREVIEW",
                format!(
                    "base {} · publication {} · no mutation yet",
                    plan.base_revisions().aggregate,
                    plan.base_publication_generation()
                ),
            ));
        }
        for refusal in &snapshot.refusals {
            body = body.child(
                div()
                    .mt_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(MAGENTA))
                    .text_xs()
                    .text_color(rgb(MAGENTA))
                    .child(format!("{refusal:?}")),
            );
        }
        if let Some(result) = &snapshot.result {
            body = body
                .child(detail(
                    "PROMOTED REVISION",
                    revision_label(result.promoted_revisions()),
                ))
                .child(detail(
                    "ATOMIC RECEIPT",
                    format!(
                        "publication {} · {} created object(s)",
                        result.promoted_publication_generation(),
                        result.promotion.created.len()
                    ),
                ));
            for provenance in result.promotion.provenance.values() {
                body = body.child(
                    div()
                        .mt_2()
                        .p_3()
                        .rounded_md()
                        .bg(rgb(RAISED))
                        .child(detail("ROLE", format!("{:?}", provenance.role)))
                        .child(detail(
                            "RETAINED",
                            provenance.expression.as_ref().map_or_else(
                                || "evidence-only term".into(),
                                |expression| format!("{expression:?}"),
                            ),
                        ))
                        .child(detail(
                            "EVIDENCE",
                            format!("{} retained references", provenance.evidence.len()),
                        )),
                );
            }
            for (index, object) in result.promotion.created.iter().enumerate() {
                let object = object.clone();
                body = if created_object_has_workspace_address(&object) {
                    body.child(
                        row_button(
                            format!("promoted-object-{index}"),
                            format!("{object:?}"),
                            "Reveal ordinary project object",
                            LIME,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.reveal_created(object.clone(), cx)
                        })),
                    )
                } else {
                    body.child(info_row(
                        format!("promoted-object-{index}"),
                        format!("{object:?}"),
                        "Created subordinate object · reveal its durable parent instead",
                        MUTED,
                    ))
                };
            }
        }
        body.child(self.render_primary_actions(snapshot, cx))
    }

    fn render_primary_actions(
        &self,
        snapshot: &ExplanationWorkbenchSnapshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut actions = div().mt_3().flex().flex_wrap().gap_2();
        match snapshot.phase {
            WorkbenchPhase::Draft
            | WorkbenchPhase::Refused
            | WorkbenchPhase::Cancelled
            | WorkbenchPhase::Stale
            | WorkbenchPhase::Failed
            | WorkbenchPhase::Undone => {
                actions = actions.child(
                    small_button("plan-promotion", "Preview promotion", false, CYAN)
                        .on_click(cx.listener(|this, _, _, cx| this.plan(cx))),
                );
            }
            WorkbenchPhase::PlanReady => {
                actions = actions.child(
                    small_button("apply-promotion", "Apply atomically", false, LIME)
                        .on_click(cx.listener(|this, _, _, cx| this.execute(cx))),
                );
            }
            WorkbenchPhase::Promoted => {
                actions = actions.child(
                    small_button("render-promotion", "Render shared schedule", false, CYAN)
                        .on_click(cx.listener(|this, _, _, cx| this.render_shared(cx))),
                );
            }
            WorkbenchPhase::Planning
            | WorkbenchPhase::Applying
            | WorkbenchPhase::Rendering
            | WorkbenchPhase::Capturing(_)
            | WorkbenchPhase::Undoing => {
                actions = actions.child(
                    small_button("cancel-operation", "Cancel", false, MAGENTA)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                );
            }
            WorkbenchPhase::RenderReady | WorkbenchPhase::ComparisonReady => {}
            WorkbenchPhase::Cancelling => {
                actions = actions.child(paragraph("Waiting for typed cancellation receipt."));
            }
        }
        if matches!(
            snapshot.phase,
            WorkbenchPhase::Promoted
                | WorkbenchPhase::RenderReady
                | WorkbenchPhase::ComparisonReady
        ) {
            actions = actions.child(
                small_button("undo-promotion", "Undo promotion", false, AMBER)
                    .on_click(cx.listener(|this, _, _, cx| this.undo(cx))),
            );
        }
        actions
    }

    fn render_comparison(
        &self,
        snapshot: &ExplanationWorkbenchSnapshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let enabled = matches!(
            snapshot.phase,
            WorkbenchPhase::RenderReady | WorkbenchPhase::ComparisonReady
        );
        let mut channels = div().grid().grid_cols(4).gap_2();
        for channel in [
            ComparisonChannel::Source,
            ComparisonChannel::Construction,
            ComparisonChannel::Residual,
            ComparisonChannel::Excess,
        ] {
            let selected = snapshot.selected_channel == channel;
            let color = channel_color(channel);
            let mut button = small_button(
                format!("comparison-{channel:?}"),
                channel_label(channel),
                selected,
                color,
            );
            if enabled {
                button =
                    button.on_click(cx.listener(move |this, _, _, cx| this.capture(channel, cx)));
            }
            channels = channels.child(button);
        }
        let mut body = section("ALIGNED NULL TEST")
            .child(channels)
            .child(paragraph(
                "Residual is exact source − construction. Excess is a spectral coverage field and never fabricated PCM.",
            ));
        if let Some(completion) = &snapshot.completion {
            let observation = &completion.execution.observation;
            body = body
                .child(metric_summary(observation.metrics))
                .child(detail(
                    "SOURCE DIGEST",
                    digest_label(observation.source_digest.0),
                ))
                .child(detail(
                    "CONSTRUCTION DIGEST",
                    digest_label(observation.construction_digest.0),
                ))
                .child(detail(
                    "RESIDUAL DIGEST",
                    digest_label(observation.residual_digest.0),
                ))
                .child(detail(
                    "COVERAGE FIELD",
                    format!(
                        "{} columns × {} bins × {} channels",
                        completion.execution.coverage.columns,
                        completion.execution.coverage.bins,
                        completion.execution.coverage.channels
                    ),
                ))
                .child(detail(
                    "SPECTRAL EXCESS",
                    format!(
                        "{:.3}% · source power {:.6} · construction power {:.6}",
                        completion.execution.coverage.summary.excess_energy_ratio * 100.0,
                        completion.execution.coverage.summary.source_power,
                        completion.execution.coverage.summary.construction_power
                    ),
                ));
        } else {
            body = body.child(paragraph(match snapshot.phase {
                WorkbenchPhase::Promoted | WorkbenchPhase::Rendering => {
                    "Waiting for the promoted revision's shared schedule."
                }
                WorkbenchPhase::RenderReady => "Choose a comparison channel to capture products.",
                WorkbenchPhase::Capturing(_) => "Capturing exact aligned comparison products.",
                _ => "No exact comparison completion has been published.",
            }));
        }
        body
    }

    fn render_diagnostics(&self, snapshot: &ExplanationWorkbenchSnapshot) -> impl IntoElement {
        let mut body = section("DIAGNOSTICS");
        if snapshot.diagnostics.is_empty() {
            body = body.child(paragraph(
                "No cancellation, stale-pin, or execution diagnostics.",
            ));
        }
        for diagnostic in &snapshot.diagnostics {
            let color = match diagnostic.kind {
                WorkbenchDiagnosticKind::Information => LIME,
                WorkbenchDiagnosticKind::Cancelled => AMBER,
                WorkbenchDiagnosticKind::StaleRevision
                | WorkbenchDiagnosticKind::StalePublication
                | WorkbenchDiagnosticKind::StaleDocument
                | WorkbenchDiagnosticKind::StaleSelection
                | WorkbenchDiagnosticKind::StaleCatalog
                | WorkbenchDiagnosticKind::IsolationRequired
                | WorkbenchDiagnosticKind::Failure => MAGENTA,
            };
            body = body.child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(rgb(color))
                    .child(format!("{:?}: {}", diagnostic.kind, diagnostic.message)),
            );
        }
        body
    }
}

impl Focusable for ExplanationWorkbenchView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExplanationWorkbenchView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.model.snapshot();
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .h(px(40.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL_ALT))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child("EXPLANATION / COMPARISON WORKBENCH"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(phase_color(snapshot.phase)))
                            .child(format!("{:?}", snapshot.phase)),
                    ),
            )
            .child(
                div()
                    .h(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_4()
                    .bg(rgb(PANEL))
                    .when_some(self.feedback.clone(), |this, (is_error, message)| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(rgb(if is_error { MAGENTA } else { LIME }))
                                .child(message),
                        )
                    }),
            )
            .child(
                div()
                    .id("explanation-workbench-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(self.render_identity(&snapshot, cx))
                    .child(self.render_candidate(&snapshot, cx))
                    .child(self.render_promotion(&snapshot, cx))
                    .child(self.render_comparison(&snapshot, cx))
                    .child(self.render_diagnostics(&snapshot)),
            )
    }
}

fn section(label: &'static str) -> gpui::Div {
    div()
        .p_4()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL))
        .child(div().text_xs().text_color(rgb(MUTED)).child(label))
}

fn detail(label: &'static str, value: impl Into<SharedString>) -> gpui::Div {
    div()
        .mt_2()
        .flex()
        .items_start()
        .justify_between()
        .gap_4()
        .child(div().text_xs().text_color(rgb(DIM)).child(label))
        .child(
            div()
                .text_xs()
                .text_color(rgb(TEXT))
                .text_right()
                .child(value.into()),
        )
}

fn paragraph(value: impl Into<SharedString>) -> gpui::Div {
    div()
        .mt_2()
        .text_xs()
        .text_color(rgb(MUTED))
        .child(value.into())
}

fn small_button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    selected: bool,
    accent: u32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(if selected { accent } else { BORDER }))
        .bg(rgb(if selected { RAISED } else { PANEL_ALT }))
        .text_xs()
        .text_color(rgb(if selected { accent } else { TEXT }))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(RAISED)))
        .child(label.into())
}

fn row_button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    accent: u32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .mt_2()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(RAISED)))
        .child(div().text_xs().text_color(rgb(accent)).child(label.into()))
        .child(
            div()
                .mt_1()
                .text_xs()
                .text_color(rgb(DIM))
                .child(description.into()),
        )
}

fn info_row(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    accent: u32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .mt_2()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .child(div().text_xs().text_color(rgb(accent)).child(label.into()))
        .child(
            div()
                .mt_1()
                .text_xs()
                .text_color(rgb(DIM))
                .child(description.into()),
        )
}

fn created_object_has_workspace_address(object: &CreatedObject) -> bool {
    !matches!(
        object,
        CreatedObject::SequencerPatternClip(_)
            | CreatedObject::SequencerLane(_)
            | CreatedObject::SamplePad(_)
    )
}

fn metric_summary(metrics: ComparisonMetrics) -> gpui::Div {
    div()
        .mt_3()
        .grid()
        .grid_cols(3)
        .gap_2()
        .child(metric("SOURCE ENERGY", metrics.source_energy))
        .child(metric("CONSTRUCTION", metrics.construction_energy))
        .child(metric("RESIDUAL", metrics.residual_energy))
        .child(metric(
            "EXPLAINED",
            metrics.clamped_explained_energy * 100.0,
        ))
        .child(metric("EXCESS", metrics.excess_energy_ratio * 100.0))
        .child(metric("CORRELATION", metrics.correlation))
}

fn metric(label: &'static str, value: f64) -> gpui::Div {
    div()
        .p_2()
        .rounded_md()
        .bg(rgb(PANEL_ALT))
        .child(div().text_xs().text_color(rgb(DIM)).child(label))
        .child(
            div()
                .mt_1()
                .text_sm()
                .text_color(rgb(TEXT))
                .child(format!("{value:.6}")),
        )
}

fn digest_label(digest: ContentDigest) -> String {
    let prefix = digest
        .bytes
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{:?}:{prefix}…", digest.algorithm)
}

fn revision_label(revisions: ProjectRevisions) -> String {
    format!(
        "aggregate {} · arrangement {} · sequencer {} · automation {} · assets {} · mixer {}",
        revisions.aggregate,
        revisions.arrangement,
        revisions.sequencer,
        revisions.automation,
        revisions.assets,
        revisions.mixer
    )
}

fn channel_label(channel: ComparisonChannel) -> &'static str {
    match channel {
        ComparisonChannel::Source => "Source",
        ComparisonChannel::Construction => "Construction",
        ComparisonChannel::Residual => "Residual",
        ComparisonChannel::Excess => "Excess / coverage",
    }
}

fn channel_feedback(channel: ComparisonChannel) -> &'static str {
    match channel {
        ComparisonChannel::Source => "Source capture requested",
        ComparisonChannel::Construction => "Construction capture requested",
        ComparisonChannel::Residual => "Exact residual capture requested",
        ComparisonChannel::Excess => "Spectral excess capture requested",
    }
}

fn channel_color(channel: ComparisonChannel) -> u32 {
    match channel {
        ComparisonChannel::Source => CYAN,
        ComparisonChannel::Construction => LIME,
        ComparisonChannel::Residual => MAGENTA,
        ComparisonChannel::Excess => AMBER,
    }
}

fn phase_color(phase: WorkbenchPhase) -> u32 {
    match phase {
        WorkbenchPhase::PlanReady
        | WorkbenchPhase::Promoted
        | WorkbenchPhase::RenderReady
        | WorkbenchPhase::ComparisonReady => LIME,
        WorkbenchPhase::Refused
        | WorkbenchPhase::Stale
        | WorkbenchPhase::Failed
        | WorkbenchPhase::Cancelled => MAGENTA,
        WorkbenchPhase::Planning
        | WorkbenchPhase::Applying
        | WorkbenchPhase::Rendering
        | WorkbenchPhase::Capturing(_)
        | WorkbenchPhase::Undoing
        | WorkbenchPhase::Cancelling => AMBER,
        WorkbenchPhase::Draft | WorkbenchPhase::Undone => MUTED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reducer_accepts_only_ordered_authoritative_completions() {
        let mut model = WorkbenchTransitionModel::default();
        model.request(WorkbenchOperation::Plan).unwrap();
        assert_eq!(model.phase, WorkbenchPhase::Planning);
        assert!(matches!(
            model.complete(WorkbenchOperation::Execute),
            Err(WorkbenchModelError::StaleCompletion { .. })
        ));
        model.complete(WorkbenchOperation::Plan).unwrap();
        model.request(WorkbenchOperation::Execute).unwrap();
        model.complete(WorkbenchOperation::Execute).unwrap();
        model.request(WorkbenchOperation::Render).unwrap();
        model.complete(WorkbenchOperation::Render).unwrap();
        model
            .request(WorkbenchOperation::Capture(ComparisonChannel::Residual))
            .unwrap();
        model
            .complete(WorkbenchOperation::Capture(ComparisonChannel::Residual))
            .unwrap();
        model.request(WorkbenchOperation::Undo).unwrap();
        model.complete(WorkbenchOperation::Undo).unwrap();
        assert_eq!(model.phase, WorkbenchPhase::Undone);
        assert_eq!(model.in_flight, None);
    }

    #[test]
    fn reducer_cancellation_and_stale_state_never_publish_readiness() {
        let mut model = WorkbenchTransitionModel::default();
        model.request(WorkbenchOperation::Plan).unwrap();
        assert_eq!(model.begin_cancel().unwrap(), WorkbenchOperation::Plan);
        model.cancelled(WorkbenchOperation::Plan).unwrap();
        assert_eq!(model.phase, WorkbenchPhase::Cancelled);
        assert_eq!(model.in_flight, None);

        model.request(WorkbenchOperation::Plan).unwrap();
        model.stale();
        assert_eq!(model.phase, WorkbenchPhase::Stale);
        assert_eq!(model.in_flight, None);
        assert!(matches!(
            model.complete(WorkbenchOperation::Plan),
            Err(WorkbenchModelError::StaleCompletion { .. })
        ));
    }

    #[test]
    fn reducer_refusal_can_only_complete_the_matching_plan() {
        let mut model = WorkbenchTransitionModel::default();
        assert!(model.refuse_plan().is_err());
        model.request(WorkbenchOperation::Plan).unwrap();
        model.refuse_plan().unwrap();
        assert_eq!(model.phase, WorkbenchPhase::Refused);
        assert!(matches!(
            model.request(WorkbenchOperation::Execute),
            Err(WorkbenchModelError::InvalidTransition { .. })
        ));
        model.request(WorkbenchOperation::Plan).unwrap();
        assert_eq!(model.phase, WorkbenchPhase::Planning);
    }
}
