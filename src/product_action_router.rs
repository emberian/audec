//! Typed, toolkit-neutral product action routing.
//!
//! Dynamic panes, native menus, toolbars, accessibility, and background
//! completions all need the same application boundary.  This module records
//! that boundary without owning a GPUI entity or mutating project/workspace
//! state.  A host gives [`ProductActionRouter`] one coherent authority
//! snapshot, receives exactly one addressed [`ProductEffectEnvelope`], and
//! executes that effect against the named `ProjectSession`, workspace, audio,
//! navigation, lifecycle, or pane adapter.
//!
//! The envelope deliberately carries both stable ownership and freshness.
//! That makes a result from a closed pane or replaced document rejectable
//! before it can opportunistically update UI state.  Accepted and completed
//! results retain the same owner, generation, and receipt, and may additionally
//! carry an authoritative receipt, a reveal consequence, and diagnostics.

use std::collections::BTreeSet;

use crate::air_query::workbench::protocol::HeadlessRequest;
use crate::air_query::workbench::{AuditionTarget, QueryCancellationToken, RevealTarget};
use crate::app_controller::{ProjectWindowId, WorkspaceInstanceId};
use crate::product_input::{
    CloseChoice, CloseRequestId, ProductAction as SemanticProductAction, TimelineInputAction,
};
use crate::project_audio_controller::ProjectTransportCommand;
use crate::project_controller::{RevealPlan, RevealRecommendation, RevealRequest};
use crate::project_session::{
    ProjectEditReceipt, ProjectSessionId, RevealReceipt, RevealResolution,
};
use crate::reading_query_view::{QueryDocumentChanged, ReadingQueryViewEffect};
use crate::sample_actions::{
    SampleActionExecutionClass, SampleActionRequest, SampleDispatchReceipt,
};
use crate::ui_actions::{ActionInvocation, InvocationOrigin};
use crate::workspace::native_authority::{AcceptedWorkspaceCommand, WorkspaceLayoutCommand};
use crate::workspace_document::WorkspaceViewId;

/// Monotonic correlation identity assigned at the application boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProductActionReceiptId(pub u64);

/// Stable identity of a background job started by a routed action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProductJobId(pub u64);

/// The origin of an action.  Presentation entities are intentionally absent;
/// panes and windows are named by their durable/runtime application IDs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProductActionSource {
    Pane(WorkspaceViewId),
    NativeMenu(ProjectWindowId),
    ProjectWindow(ProjectWindowId),
    Application,
    Background {
        job: ProductJobId,
        source_view: Option<WorkspaceViewId>,
    },
}

/// Complete authority ownership attached to every action and completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProductActionOwner {
    pub session: ProjectSessionId,
    pub workspace: WorkspaceInstanceId,
    pub source: ProductActionSource,
}

impl ProductActionOwner {
    pub const fn pane(
        session: ProjectSessionId,
        workspace: WorkspaceInstanceId,
        view: WorkspaceViewId,
    ) -> Self {
        Self {
            session,
            workspace,
            source: ProductActionSource::Pane(view),
        }
    }

    pub const fn native_menu(
        session: ProjectSessionId,
        workspace: WorkspaceInstanceId,
        window: ProjectWindowId,
    ) -> Self {
        Self {
            session,
            workspace,
            source: ProductActionSource::NativeMenu(window),
        }
    }

    pub const fn source_view(&self) -> Option<WorkspaceViewId> {
        match self.source {
            ProductActionSource::Pane(view) => Some(view),
            ProductActionSource::Background { source_view, .. } => source_view,
            ProductActionSource::NativeMenu(_)
            | ProductActionSource::ProjectWindow(_)
            | ProductActionSource::Application => None,
        }
    }
}

/// Freshness copied from one coherent application read.  Document generation
/// changes only on replacement; publication generation changes on project
/// publication; workspace revision changes on accepted layout transactions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProductActionGeneration {
    pub document: u64,
    pub publication: u64,
    pub workspace: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectHistoryAction {
    Undo,
    Redo,
}

/// Actions whose only mutating authority is `ProjectSession`.
#[derive(Debug)]
pub enum ProjectSessionAction {
    Execute(crate::command::CommandEnvelope),
    History(ProjectHistoryAction),
    Sample(SampleActionRequest),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProductLifecycleAction {
    NewProject,
    OpenProject,
    OpenAudio,
    OpenRecovery,
    Save,
    SaveAs,
    ExportAudio,
    Quit,
    ResolveClose {
        request: CloseRequestId,
        choice: CloseChoice,
    },
}

/// Audio requests all target the one audio controller associated with the
/// owner session.  Reading audition still needs a semantic-to-render adapter,
/// but is no longer handled opportunistically by its pane.
#[derive(Clone, Debug, PartialEq)]
pub enum ProductAudioAction {
    Transport(ProjectTransportCommand),
    ToggleLoop,
    ReadingAudition {
        view: WorkspaceViewId,
        target: AuditionTarget,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProductNavigationAction {
    Issue(RevealRequest),
    Resolve(RevealReceipt),
    ResolveReading {
        view: WorkspaceViewId,
        target: RevealTarget,
    },
}

/// The complete outward reading/query boundary, converted from the current
/// view callback with no loss of its cancellation or document revision.
#[derive(Clone, Debug)]
pub enum ReadingQueryAction {
    Observe {
        request: HeadlessRequest,
        cancellation: QueryCancellationToken,
    },
    Execute(crate::command::CommandEnvelope),
    Audition {
        view: WorkspaceViewId,
        target: AuditionTarget,
    },
    Reveal {
        view: WorkspaceViewId,
        target: RevealTarget,
    },
    PersistDocument {
        view: WorkspaceViewId,
        change: QueryDocumentChanged,
    },
}

impl ReadingQueryAction {
    pub fn from_view_effect(view: WorkspaceViewId, effect: ReadingQueryViewEffect) -> Self {
        match effect {
            ReadingQueryViewEffect::Observation {
                request,
                cancellation,
            } => Self::Observe {
                request,
                cancellation,
            },
            ReadingQueryViewEffect::Command(envelope) => Self::Execute(envelope),
            ReadingQueryViewEffect::Render(target) => Self::Audition { view, target },
            ReadingQueryViewEffect::Reveal(target) => Self::Reveal { view, target },
            ReadingQueryViewEffect::DocumentChanged(change) => {
                Self::PersistDocument { view, change }
            }
        }
    }
}

/// One vocabulary accepted from panes and application-level action sources.
#[derive(Debug)]
pub enum RoutedProductAction {
    Project(ProjectSessionAction),
    Workspace(WorkspaceLayoutCommand),
    Audio(ProductAudioAction),
    Navigation(ProductNavigationAction),
    ReadingQuery(ReadingQueryAction),
    Lifecycle(ProductLifecycleAction),
    /// Input/AX actions retain their semantic vocabulary until this boundary;
    /// the router lowers authority-independent cases itself.
    Semantic(SemanticProductAction),
    /// Native menus, shortcuts, palette, and toolbar actions may all submit
    /// the same registry invocation instead of calling window methods.
    Invocation(ActionInvocation),
}

#[derive(Debug)]
pub struct ProductActionEnvelope {
    pub owner: ProductActionOwner,
    pub generation: ProductActionGeneration,
    pub action: RoutedProductAction,
}

impl ProductActionEnvelope {
    pub const fn new(
        owner: ProductActionOwner,
        generation: ProductActionGeneration,
        action: RoutedProductAction,
    ) -> Self {
        Self {
            owner,
            generation,
            action,
        }
    }
}

/// One coherent read of application authority state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProductRouteContext {
    pub session: ProjectSessionId,
    pub workspace: WorkspaceInstanceId,
    pub generation: ProductActionGeneration,
    pub registered_panes: BTreeSet<WorkspaceViewId>,
}

impl ProductRouteContext {
    pub fn new(
        session: ProjectSessionId,
        workspace: WorkspaceInstanceId,
        generation: ProductActionGeneration,
        registered_panes: impl IntoIterator<Item = WorkspaceViewId>,
    ) -> Self {
        Self {
            session,
            workspace,
            generation,
            registered_panes: registered_panes.into_iter().collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProductActionKind {
    Project,
    Workspace,
    Audio,
    Navigation,
    ReadingQuery,
    Lifecycle,
    Semantic,
    Invocation,
}

impl RoutedProductAction {
    pub const fn kind(&self) -> ProductActionKind {
        match self {
            Self::Project(_) => ProductActionKind::Project,
            Self::Workspace(_) => ProductActionKind::Workspace,
            Self::Audio(_) => ProductActionKind::Audio,
            Self::Navigation(_) => ProductActionKind::Navigation,
            Self::ReadingQuery(_) => ProductActionKind::ReadingQuery,
            Self::Lifecycle(_) => ProductActionKind::Lifecycle,
            Self::Semantic(_) => ProductActionKind::Semantic,
            Self::Invocation(_) => ProductActionKind::Invocation,
        }
    }
}

/// The unique authority expected to execute an effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProductAuthority {
    ProjectSession(ProjectSessionId),
    Workspace(WorkspaceInstanceId),
    Audio(ProjectSessionId),
    Navigation(ProjectSessionId),
    Observation(ProjectSessionId),
    Lifecycle(ProjectSessionId),
    Pane(WorkspaceViewId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Freshness {
    None,
    Document,
    Project,
    Workspace,
}

#[derive(Debug)]
pub enum ProductWorkspaceEffect {
    Layout(WorkspaceLayoutCommand),
    PersistReadingDocument {
        view: WorkspaceViewId,
        change: QueryDocumentChanged,
    },
    Invoke(ActionInvocation),
    Semantic(SemanticProductAction),
}

#[derive(Debug)]
pub enum ProductPaneEffect {
    Invoke(ActionInvocation),
    Semantic(SemanticProductAction),
}

#[derive(Debug)]
pub enum ProductEffect {
    Project(ProjectSessionAction),
    Workspace(ProductWorkspaceEffect),
    Audio(ProductAudioAction),
    Navigation(ProductNavigationAction),
    ObserveReading {
        request: HeadlessRequest,
        cancellation: QueryCancellationToken,
    },
    Lifecycle(ProductLifecycleAction),
    Pane {
        view: WorkspaceViewId,
        effect: ProductPaneEffect,
    },
}

/// Every delayed executor receives this whole value.  It must return the same
/// receipt rather than correlating by a pane-local cache or current focus.
#[derive(Debug)]
pub struct ProductEffectEnvelope {
    pub receipt: ProductActionReceipt,
    pub effect: ProductEffect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProductActionReceipt {
    pub id: ProductActionReceiptId,
    pub owner: ProductActionOwner,
    pub generation: ProductActionGeneration,
    pub kind: ProductActionKind,
    pub authority: ProductAuthority,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProductDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProductDiagnosticCode {
    InvalidOwner,
    WrongSession,
    WrongWorkspace,
    UnknownPane,
    MissingPaneOwner,
    StaleDocument,
    StalePublication,
    StaleWorkspace,
    UnsupportedInvocation,
    AuthorityRefused,
    Superseded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProductDiagnostic {
    pub severity: ProductDiagnosticSeverity,
    pub code: ProductDiagnosticCode,
    pub message: String,
}

impl ProductDiagnostic {
    pub fn new(
        severity: ProductDiagnosticSeverity,
        code: ProductDiagnosticCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            code,
            message: message.into(),
        }
    }

    fn error(code: ProductDiagnosticCode, message: impl Into<String>) -> Self {
        Self::new(ProductDiagnosticSeverity::Error, code, message)
    }
}

/// Receipt returned by the authority which actually performed the effect.
/// This is intentionally distinct from the router's correlation receipt.
#[derive(Clone, Debug)]
pub enum ProductAuthorityReceipt {
    Project(ProjectEditReceipt),
    Sample(SampleDispatchReceipt),
    Workspace(AcceptedWorkspaceCommand),
    Audio { control_revision: u64 },
    Navigation(RevealReceipt),
    Observation { request_id: String },
    Lifecycle { generation: u64 },
    Acknowledged,
}

#[derive(Clone, Debug)]
pub enum ProductRevealOutcome {
    Recommended(RevealRecommendation),
    Issued(RevealReceipt),
    Resolved(RevealResolution),
    Planned(RevealPlan),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProductActionResultState {
    Accepted,
    Completed,
    Rejected,
    Superseded,
}

/// Uniform action/result record suitable for status presentation, task
/// completion, provenance, and later collaboration logging.
#[derive(Clone, Debug)]
pub struct ProductActionResult {
    pub state: ProductActionResultState,
    pub owner: ProductActionOwner,
    pub generation: ProductActionGeneration,
    pub receipt: ProductActionReceipt,
    pub authority_receipt: Option<ProductAuthorityReceipt>,
    pub reveal: Option<ProductRevealOutcome>,
    pub diagnostics: Vec<ProductDiagnostic>,
}

impl ProductActionResult {
    fn accepted(receipt: ProductActionReceipt) -> Self {
        Self {
            state: ProductActionResultState::Accepted,
            owner: receipt.owner.clone(),
            generation: receipt.generation,
            receipt,
            authority_receipt: None,
            reveal: None,
            diagnostics: Vec::new(),
        }
    }

    fn rejected(receipt: ProductActionReceipt, diagnostics: Vec<ProductDiagnostic>) -> Self {
        let mut result = Self::accepted(receipt);
        result.state = ProductActionResultState::Rejected;
        result.diagnostics = diagnostics;
        result
    }

    pub fn complete(mut self, receipt: ProductAuthorityReceipt) -> Self {
        self.state = ProductActionResultState::Completed;
        self.authority_receipt = Some(receipt);
        self
    }

    pub fn with_reveal(mut self, reveal: ProductRevealOutcome) -> Self {
        self.reveal = Some(reveal);
        self
    }

    pub fn with_diagnostic(mut self, diagnostic: ProductDiagnostic) -> Self {
        self.diagnostics.push(diagnostic);
        self
    }

    pub fn superseded(mut self, message: impl Into<String>) -> Self {
        self.state = ProductActionResultState::Superseded;
        self.diagnostics.push(ProductDiagnostic::new(
            ProductDiagnosticSeverity::Info,
            ProductDiagnosticCode::Superseded,
            message,
        ));
        self
    }
}

#[derive(Debug)]
pub struct ProductRoute {
    pub result: ProductActionResult,
    pub effect: Option<ProductEffectEnvelope>,
}

/// Deterministic serial allocator and pure routing gate.  It retains no
/// project, workspace document, audio status, pane entity, or completion map.
#[derive(Clone, Debug)]
pub struct ProductActionRouter {
    next_receipt: u64,
}

impl Default for ProductActionRouter {
    fn default() -> Self {
        Self { next_receipt: 1 }
    }
}

impl ProductActionRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn route(
        &mut self,
        context: &ProductRouteContext,
        envelope: ProductActionEnvelope,
    ) -> ProductRoute {
        let kind = envelope.action.kind();
        let intended_authority = authority_for(context, &envelope.owner, &envelope.action);
        let receipt = ProductActionReceipt {
            id: self.allocate_receipt(),
            owner: envelope.owner.clone(),
            generation: envelope.generation,
            kind,
            authority: intended_authority,
        };
        let mut diagnostics = validate_owner(context, &envelope.owner);
        diagnostics.extend(validate_generation(
            context.generation,
            envelope.generation,
            freshness_for(&envelope.action),
        ));
        diagnostics.extend(validate_pane_requirement(&envelope.owner, &envelope.action));
        if !diagnostics.is_empty() {
            return ProductRoute {
                result: ProductActionResult::rejected(receipt, diagnostics),
                effect: None,
            };
        }

        let effect = lower_effect(envelope.owner.source_view(), envelope.action);
        let result = ProductActionResult::accepted(receipt.clone());
        ProductRoute {
            result,
            effect: Some(ProductEffectEnvelope { receipt, effect }),
        }
    }

    fn allocate_receipt(&mut self) -> ProductActionReceiptId {
        let id = ProductActionReceiptId(self.next_receipt);
        self.next_receipt = self.next_receipt.wrapping_add(1).max(1);
        id
    }
}

fn freshness_for(action: &RoutedProductAction) -> Freshness {
    match action {
        RoutedProductAction::Lifecycle(_) => Freshness::None,
        RoutedProductAction::Workspace(_) => Freshness::Workspace,
        RoutedProductAction::Project(_) => Freshness::Project,
        RoutedProductAction::Audio(ProductAudioAction::Transport(_))
        | RoutedProductAction::Audio(ProductAudioAction::ToggleLoop) => Freshness::Document,
        RoutedProductAction::Audio(ProductAudioAction::ReadingAudition { .. })
        | RoutedProductAction::Navigation(_)
        | RoutedProductAction::ReadingQuery(ReadingQueryAction::Observe { .. })
        | RoutedProductAction::ReadingQuery(ReadingQueryAction::Execute(_))
        | RoutedProductAction::ReadingQuery(ReadingQueryAction::Audition { .. })
        | RoutedProductAction::ReadingQuery(ReadingQueryAction::Reveal { .. }) => {
            Freshness::Project
        }
        RoutedProductAction::ReadingQuery(ReadingQueryAction::PersistDocument { .. }) => {
            Freshness::Workspace
        }
        RoutedProductAction::Semantic(action) => semantic_freshness(action),
        RoutedProductAction::Invocation(invocation) => invocation_freshness(invocation),
    }
}

fn semantic_freshness(action: &SemanticProductAction) -> Freshness {
    match action {
        SemanticProductAction::Reveal(_) | SemanticProductAction::ShowInspector(_) => {
            Freshness::Project
        }
        SemanticProductAction::Timeline {
            action:
                TimelineInputAction::PlayPause
                | TimelineInputAction::SetLoopFromSelection
                | TimelineInputAction::ToggleLoop,
            ..
        } => Freshness::Document,
        SemanticProductAction::CloseChoice { .. } => Freshness::None,
        _ => Freshness::Workspace,
    }
}

fn invocation_freshness(invocation: &ActionInvocation) -> Freshness {
    match invocation.action.0 {
        "audec.file.open" => Freshness::None,
        "audec.file.save" | "audec.file.export" => Freshness::Document,
        "audec.transport.toggle" | "audec.transport.stop" | "audec.loop.toggle" => {
            Freshness::Document
        }
        "audec.edit.undo" | "audec.edit.redo" => Freshness::Project,
        id if id.starts_with("audec.editor.") || id == "audec.palette.open" => Freshness::Workspace,
        _ => Freshness::Workspace,
    }
}

fn authority_for(
    context: &ProductRouteContext,
    owner: &ProductActionOwner,
    action: &RoutedProductAction,
) -> ProductAuthority {
    match action {
        RoutedProductAction::Project(_) => ProductAuthority::ProjectSession(context.session),
        RoutedProductAction::Workspace(_) => ProductAuthority::Workspace(context.workspace),
        RoutedProductAction::Audio(_) => ProductAuthority::Audio(context.session),
        RoutedProductAction::Navigation(_) => ProductAuthority::Navigation(context.session),
        RoutedProductAction::ReadingQuery(ReadingQueryAction::Observe { .. }) => {
            ProductAuthority::Observation(context.session)
        }
        RoutedProductAction::ReadingQuery(ReadingQueryAction::Execute(_)) => {
            ProductAuthority::ProjectSession(context.session)
        }
        RoutedProductAction::ReadingQuery(ReadingQueryAction::Audition { .. }) => {
            ProductAuthority::Audio(context.session)
        }
        RoutedProductAction::ReadingQuery(ReadingQueryAction::Reveal { .. }) => {
            ProductAuthority::Navigation(context.session)
        }
        RoutedProductAction::ReadingQuery(ReadingQueryAction::PersistDocument { .. }) => {
            ProductAuthority::Workspace(context.workspace)
        }
        RoutedProductAction::Lifecycle(_) => ProductAuthority::Lifecycle(context.session),
        RoutedProductAction::Semantic(action) => semantic_authority(context, owner, action),
        RoutedProductAction::Invocation(invocation) => {
            invocation_authority(context, owner, invocation)
        }
    }
}

fn semantic_authority(
    context: &ProductRouteContext,
    owner: &ProductActionOwner,
    action: &SemanticProductAction,
) -> ProductAuthority {
    match action {
        SemanticProductAction::Reveal(_) | SemanticProductAction::ShowInspector(_) => {
            ProductAuthority::Navigation(context.session)
        }
        SemanticProductAction::Timeline {
            action:
                TimelineInputAction::PlayPause
                | TimelineInputAction::SetLoopFromSelection
                | TimelineInputAction::ToggleLoop,
            ..
        } => ProductAuthority::Audio(context.session),
        SemanticProductAction::CloseChoice { .. } => ProductAuthority::Lifecycle(context.session),
        SemanticProductAction::SetExplorerMode(_) => ProductAuthority::Workspace(context.workspace),
        _ => owner
            .source_view()
            .map(ProductAuthority::Pane)
            .unwrap_or(ProductAuthority::Workspace(context.workspace)),
    }
}

fn invocation_authority(
    context: &ProductRouteContext,
    owner: &ProductActionOwner,
    invocation: &ActionInvocation,
) -> ProductAuthority {
    match invocation.action.0 {
        id if id.starts_with("audec.file.") => ProductAuthority::Lifecycle(context.session),
        "audec.transport.toggle" | "audec.transport.stop" | "audec.loop.toggle" => {
            ProductAuthority::Audio(context.session)
        }
        "audec.edit.undo" | "audec.edit.redo" => ProductAuthority::ProjectSession(context.session),
        id if id.starts_with("audec.editor.") || id == "audec.palette.open" => {
            ProductAuthority::Workspace(context.workspace)
        }
        _ => invocation
            .view
            .or_else(|| owner.source_view())
            .map(ProductAuthority::Pane)
            .unwrap_or(ProductAuthority::Workspace(context.workspace)),
    }
}

fn validate_owner(
    context: &ProductRouteContext,
    owner: &ProductActionOwner,
) -> Vec<ProductDiagnostic> {
    let mut diagnostics = Vec::new();
    if owner.session.0 == 0 || owner.workspace.0 == 0 {
        diagnostics.push(ProductDiagnostic::error(
            ProductDiagnosticCode::InvalidOwner,
            "product action owner contains a reserved zero identity",
        ));
    }
    if owner.session != context.session {
        diagnostics.push(ProductDiagnostic::error(
            ProductDiagnosticCode::WrongSession,
            format!(
                "action belongs to session {}, but router owns session {}",
                owner.session.0, context.session.0
            ),
        ));
    }
    if owner.workspace != context.workspace {
        diagnostics.push(ProductDiagnostic::error(
            ProductDiagnosticCode::WrongWorkspace,
            format!(
                "action belongs to workspace {}, but router owns workspace {}",
                owner.workspace.0, context.workspace.0
            ),
        ));
    }
    match owner.source {
        ProductActionSource::Pane(view) => {
            validate_registered_pane(context, view, &mut diagnostics)
        }
        ProductActionSource::NativeMenu(window) | ProductActionSource::ProjectWindow(window)
            if window.0 == 0 =>
        {
            diagnostics.push(ProductDiagnostic::error(
                ProductDiagnosticCode::InvalidOwner,
                "product action owner contains a reserved zero window identity",
            ));
        }
        ProductActionSource::Background { job, source_view } => {
            if job.0 == 0 {
                diagnostics.push(ProductDiagnostic::error(
                    ProductDiagnosticCode::InvalidOwner,
                    "product action owner contains a reserved zero job identity",
                ));
            }
            if let Some(view) = source_view {
                validate_registered_pane(context, view, &mut diagnostics);
            }
        }
        ProductActionSource::NativeMenu(_)
        | ProductActionSource::ProjectWindow(_)
        | ProductActionSource::Application => {}
    }
    diagnostics
}

fn validate_registered_pane(
    context: &ProductRouteContext,
    view: WorkspaceViewId,
    diagnostics: &mut Vec<ProductDiagnostic>,
) {
    if view.0 == 0 {
        diagnostics.push(ProductDiagnostic::error(
            ProductDiagnosticCode::InvalidOwner,
            "product action owner contains a reserved zero pane identity",
        ));
    } else if !context.registered_panes.contains(&view) {
        diagnostics.push(ProductDiagnostic::error(
            ProductDiagnosticCode::UnknownPane,
            format!("workspace pane {} is no longer registered", view.0),
        ));
    }
}

fn validate_generation(
    current: ProductActionGeneration,
    supplied: ProductActionGeneration,
    freshness: Freshness,
) -> Vec<ProductDiagnostic> {
    let mut diagnostics = Vec::new();
    if freshness == Freshness::None {
        return diagnostics;
    }
    if supplied.document != current.document {
        diagnostics.push(ProductDiagnostic::error(
            ProductDiagnosticCode::StaleDocument,
            format!(
                "action document generation {} does not match current generation {}",
                supplied.document, current.document
            ),
        ));
    }
    if freshness == Freshness::Project && supplied.publication != current.publication {
        diagnostics.push(ProductDiagnostic::error(
            ProductDiagnosticCode::StalePublication,
            format!(
                "action publication generation {} does not match current generation {}",
                supplied.publication, current.publication
            ),
        ));
    }
    if freshness == Freshness::Workspace && supplied.workspace != current.workspace {
        diagnostics.push(ProductDiagnostic::error(
            ProductDiagnosticCode::StaleWorkspace,
            format!(
                "action workspace revision {} does not match current revision {}",
                supplied.workspace, current.workspace
            ),
        ));
    }
    diagnostics
}

fn validate_pane_requirement(
    owner: &ProductActionOwner,
    action: &RoutedProductAction,
) -> Vec<ProductDiagnostic> {
    let requires_pane = match action {
        RoutedProductAction::Semantic(action) => !matches!(
            action,
            SemanticProductAction::Reveal(_)
                | SemanticProductAction::ShowInspector(_)
                | SemanticProductAction::SetExplorerMode(_)
                | SemanticProductAction::CloseChoice { .. }
                | SemanticProductAction::Timeline {
                    action: TimelineInputAction::PlayPause
                        | TimelineInputAction::SetLoopFromSelection
                        | TimelineInputAction::ToggleLoop,
                    ..
                }
        ),
        RoutedProductAction::Invocation(invocation) => {
            invocation.view.is_none()
                && owner.source_view().is_none()
                && !matches!(
                    invocation.action.0,
                    "audec.file.open"
                        | "audec.file.save"
                        | "audec.file.export"
                        | "audec.transport.toggle"
                        | "audec.transport.stop"
                        | "audec.loop.toggle"
                        | "audec.edit.undo"
                        | "audec.edit.redo"
                        | "audec.palette.open"
                )
                && !invocation.action.0.starts_with("audec.editor.")
        }
        _ => false,
    };
    if requires_pane {
        vec![ProductDiagnostic::error(
            ProductDiagnosticCode::MissingPaneOwner,
            "focused-editor action has no stable workspace pane owner",
        )]
    } else {
        Vec::new()
    }
}

fn lower_effect(
    source_view: Option<WorkspaceViewId>,
    action: RoutedProductAction,
) -> ProductEffect {
    match action {
        RoutedProductAction::Project(action) => lower_project_action(action),
        RoutedProductAction::Workspace(action) => {
            ProductEffect::Workspace(ProductWorkspaceEffect::Layout(action))
        }
        RoutedProductAction::Audio(action) => ProductEffect::Audio(action),
        RoutedProductAction::Navigation(action) => ProductEffect::Navigation(action),
        RoutedProductAction::ReadingQuery(action) => lower_reading_action(action),
        RoutedProductAction::Lifecycle(action) => ProductEffect::Lifecycle(action),
        RoutedProductAction::Semantic(action) => lower_semantic_action(source_view, action),
        RoutedProductAction::Invocation(invocation) => lower_invocation(source_view, invocation),
    }
}

fn lower_project_action(action: ProjectSessionAction) -> ProductEffect {
    match action {
        ProjectSessionAction::Sample(request)
            if request.action.execution_class()
                == SampleActionExecutionClass::BackgroundPlanning =>
        {
            // The project-session adapter captures immutable work first, then
            // prepares it off-thread and returns with this route receipt.
            ProductEffect::Project(ProjectSessionAction::Sample(request))
        }
        action => ProductEffect::Project(action),
    }
}

fn lower_reading_action(action: ReadingQueryAction) -> ProductEffect {
    match action {
        ReadingQueryAction::Observe {
            request,
            cancellation,
        } => ProductEffect::ObserveReading {
            request,
            cancellation,
        },
        ReadingQueryAction::Execute(envelope) => {
            ProductEffect::Project(ProjectSessionAction::Execute(envelope))
        }
        ReadingQueryAction::Audition { view, target } => {
            ProductEffect::Audio(ProductAudioAction::ReadingAudition { view, target })
        }
        ReadingQueryAction::Reveal { view, target } => {
            ProductEffect::Navigation(ProductNavigationAction::ResolveReading { view, target })
        }
        ReadingQueryAction::PersistDocument { view, change } => {
            ProductEffect::Workspace(ProductWorkspaceEffect::PersistReadingDocument {
                view,
                change,
            })
        }
    }
}

fn lower_semantic_action(
    source_view: Option<WorkspaceViewId>,
    action: SemanticProductAction,
) -> ProductEffect {
    match action {
        SemanticProductAction::Reveal(object) => {
            ProductEffect::Navigation(ProductNavigationAction::Issue(RevealRequest::new(
                object,
                crate::project_controller::RevealIntent::ActivateExisting,
            )))
        }
        SemanticProductAction::ShowInspector(object) => {
            ProductEffect::Navigation(ProductNavigationAction::Issue(RevealRequest::new(
                object,
                crate::project_controller::RevealIntent::ShowInspector,
            )))
        }
        SemanticProductAction::Timeline {
            action: TimelineInputAction::PlayPause,
            ..
        } => ProductEffect::Audio(ProductAudioAction::Transport(
            ProjectTransportCommand::TogglePlay,
        )),
        SemanticProductAction::Timeline {
            action: TimelineInputAction::SetLoopFromSelection,
            ..
        } => ProductEffect::Audio(ProductAudioAction::Transport(
            ProjectTransportCommand::SetLoopFromSelection,
        )),
        SemanticProductAction::Timeline {
            action: TimelineInputAction::ToggleLoop,
            ..
        } => ProductEffect::Audio(ProductAudioAction::ToggleLoop),
        SemanticProductAction::CloseChoice { request, choice } => {
            ProductEffect::Lifecycle(ProductLifecycleAction::ResolveClose { request, choice })
        }
        action @ SemanticProductAction::SetExplorerMode(_) => {
            ProductEffect::Workspace(ProductWorkspaceEffect::Semantic(action))
        }
        action => ProductEffect::Pane {
            view: source_view.expect("pane requirement validated before lowering"),
            effect: ProductPaneEffect::Semantic(action),
        },
    }
}

fn lower_invocation(
    source_view: Option<WorkspaceViewId>,
    invocation: ActionInvocation,
) -> ProductEffect {
    match invocation.action.0 {
        "audec.file.open" => ProductEffect::Lifecycle(ProductLifecycleAction::OpenProject),
        "audec.file.save" => ProductEffect::Lifecycle(ProductLifecycleAction::Save),
        "audec.file.export" => ProductEffect::Lifecycle(ProductLifecycleAction::ExportAudio),
        "audec.transport.toggle" => ProductEffect::Audio(ProductAudioAction::Transport(
            ProjectTransportCommand::TogglePlay,
        )),
        "audec.transport.stop" => {
            ProductEffect::Audio(ProductAudioAction::Transport(ProjectTransportCommand::Stop))
        }
        "audec.loop.toggle" => ProductEffect::Audio(ProductAudioAction::ToggleLoop),
        "audec.edit.undo" => {
            ProductEffect::Project(ProjectSessionAction::History(ProjectHistoryAction::Undo))
        }
        "audec.edit.redo" => {
            ProductEffect::Project(ProjectSessionAction::History(ProjectHistoryAction::Redo))
        }
        id if id.starts_with("audec.editor.") || id == "audec.palette.open" => {
            ProductEffect::Workspace(ProductWorkspaceEffect::Invoke(invocation))
        }
        _ => ProductEffect::Pane {
            view: invocation
                .view
                .or(source_view)
                .expect("pane requirement validated before lowering"),
            effect: ProductPaneEffect::Invoke(invocation),
        },
    }
}

/// Convenience constructor used by native menu adapters.  Menu origin is set
/// here once so menu listeners need only provide the stable action and target.
pub fn native_menu_invocation(
    action: crate::ui_actions::ActionId,
    view: Option<WorkspaceViewId>,
) -> ActionInvocation {
    ActionInvocation {
        action,
        origin: InvocationOrigin::Menu,
        view,
        target: None,
        modifiers: crate::ui_actions::InvocationModifiers::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::AssetId;
    use crate::product_input::ProductAction as SemanticAction;
    use crate::project_controller::{ObjectRef, RevealIntent};
    use crate::sample_actions::{
        ChopPreviewIntent, SampleAction, SampleChopIntent, SampleRequestId, SampleSelection,
    };
    use crate::ui_actions::ActionId;

    const SESSION: ProjectSessionId = ProjectSessionId(7);
    const WORKSPACE: WorkspaceInstanceId = WorkspaceInstanceId(11);
    const WINDOW: ProjectWindowId = ProjectWindowId(13);
    const VIEW: WorkspaceViewId = WorkspaceViewId(17);
    const GENERATION: ProductActionGeneration = ProductActionGeneration {
        document: 3,
        publication: 29,
        workspace: 41,
    };

    fn context() -> ProductRouteContext {
        ProductRouteContext::new(SESSION, WORKSPACE, GENERATION, [VIEW])
    }

    fn pane_owner() -> ProductActionOwner {
        ProductActionOwner::pane(SESSION, WORKSPACE, VIEW)
    }

    fn menu_owner() -> ProductActionOwner {
        ProductActionOwner::native_menu(SESSION, WORKSPACE, WINDOW)
    }

    #[test]
    fn background_sample_route_preserves_owner_generation_and_receipt() {
        let request = SampleActionRequest {
            id: SampleRequestId(23),
            action: SampleAction::PreviewChop(ChopPreviewIntent {
                source: SampleSelection::whole_asset(AssetId(31)),
                chop: SampleChopIntent::DetectOnsets {
                    analyzer: "router-test".into(),
                    sensitivity: 0.5,
                    minimum_gap_frames: 512,
                },
            }),
        };
        let mut router = ProductActionRouter::new();
        let route = router.route(
            &context(),
            ProductActionEnvelope::new(
                pane_owner(),
                GENERATION,
                RoutedProductAction::Project(ProjectSessionAction::Sample(request)),
            ),
        );

        assert_eq!(route.result.state, ProductActionResultState::Accepted);
        assert_eq!(route.result.owner, pane_owner());
        assert_eq!(route.result.generation, GENERATION);
        assert_eq!(route.result.receipt.id, ProductActionReceiptId(1));
        let effect = route.effect.unwrap();
        assert_eq!(effect.receipt, route.result.receipt);
        assert!(matches!(
            effect.effect,
            ProductEffect::Project(ProjectSessionAction::Sample(SampleActionRequest {
                id: SampleRequestId(23),
                ..
            }))
        ));
    }

    #[test]
    fn stale_project_action_is_a_correlated_diagnostic_not_an_effect() {
        let stale = ProductActionGeneration {
            publication: 28,
            ..GENERATION
        };
        let mut router = ProductActionRouter::new();
        let route = router.route(
            &context(),
            ProductActionEnvelope::new(
                pane_owner(),
                stale,
                RoutedProductAction::Navigation(ProductNavigationAction::Issue(
                    RevealRequest::new(
                        ObjectRef::Material(AssetId(31)),
                        RevealIntent::ActivateExisting,
                    ),
                )),
            ),
        );

        assert!(route.effect.is_none());
        assert_eq!(route.result.state, ProductActionResultState::Rejected);
        assert_eq!(route.result.owner, pane_owner());
        assert_eq!(route.result.generation, stale);
        assert_eq!(route.result.receipt.id, ProductActionReceiptId(1));
        assert_eq!(route.result.diagnostics.len(), 1);
        assert_eq!(
            route.result.diagnostics[0].code,
            ProductDiagnosticCode::StalePublication
        );
    }

    #[test]
    fn native_menu_and_pane_transport_share_the_audio_authority() {
        let mut router = ProductActionRouter::new();
        let menu = router.route(
            &context(),
            ProductActionEnvelope::new(
                menu_owner(),
                ProductActionGeneration {
                    publication: 1,
                    workspace: 1,
                    ..GENERATION
                },
                RoutedProductAction::Invocation(native_menu_invocation(
                    ActionId("audec.transport.toggle"),
                    None,
                )),
            ),
        );
        let pane = router.route(
            &context(),
            ProductActionEnvelope::new(
                pane_owner(),
                ProductActionGeneration {
                    publication: 2,
                    workspace: 2,
                    ..GENERATION
                },
                RoutedProductAction::Semantic(SemanticAction::Timeline {
                    controller: crate::timeline::TimelineControllerId(1),
                    action: TimelineInputAction::PlayPause,
                }),
            ),
        );

        for route in [&menu, &pane] {
            assert_eq!(
                route.result.receipt.authority,
                ProductAuthority::Audio(SESSION)
            );
            assert!(matches!(
                route.effect.as_ref().map(|effect| &effect.effect),
                Some(ProductEffect::Audio(ProductAudioAction::Transport(
                    ProjectTransportCommand::TogglePlay
                )))
            ));
        }
        assert_eq!(menu.result.receipt.id, ProductActionReceiptId(1));
        assert_eq!(pane.result.receipt.id, ProductActionReceiptId(2));
    }

    #[test]
    fn workspace_freshness_ignores_project_edits_but_rejects_layout_races() {
        let mut router = ProductActionRouter::new();
        let fresh_layout = ProductActionGeneration {
            publication: 1,
            ..GENERATION
        };
        let accepted = router.route(
            &context(),
            ProductActionEnvelope::new(
                menu_owner(),
                fresh_layout,
                RoutedProductAction::Workspace(WorkspaceLayoutCommand::FocusPane(
                    crate::workspace_session_layout::PaneInstanceId(VIEW),
                )),
            ),
        );
        assert!(accepted.effect.is_some());

        let stale_layout = ProductActionGeneration {
            publication: GENERATION.publication,
            workspace: GENERATION.workspace - 1,
            ..GENERATION
        };
        let rejected = router.route(
            &context(),
            ProductActionEnvelope::new(
                menu_owner(),
                stale_layout,
                RoutedProductAction::Workspace(WorkspaceLayoutCommand::FocusPane(
                    crate::workspace_session_layout::PaneInstanceId(VIEW),
                )),
            ),
        );
        assert!(rejected.effect.is_none());
        assert_eq!(
            rejected.result.diagnostics[0].code,
            ProductDiagnosticCode::StaleWorkspace
        );
    }

    #[test]
    fn reading_view_effects_route_to_distinct_existing_authorities() {
        let mut router = ProductActionRouter::new();
        let render = crate::air_query::workbench::AuditionTarget {
            entity: crate::interpretation_navigation::EntityRefDto::Object { id: 9 },
            extent: crate::interpretation_navigation::AspectGeometryDto {
                regions: Vec::new(),
            },
        };
        let route = router.route(
            &context(),
            ProductActionEnvelope::new(
                pane_owner(),
                GENERATION,
                RoutedProductAction::ReadingQuery(ReadingQueryAction::Audition {
                    view: VIEW,
                    target: render,
                }),
            ),
        );
        assert_eq!(
            route.result.receipt.authority,
            ProductAuthority::Audio(SESSION)
        );
        assert!(matches!(
            route.effect.unwrap().effect,
            ProductEffect::Audio(ProductAudioAction::ReadingAudition { view: VIEW, .. })
        ));

        let reveal = RevealTarget {
            entity: crate::interpretation_navigation::EntityRefDto::Object { id: 12 },
            extent: None,
        };
        let route = router.route(
            &context(),
            ProductActionEnvelope::new(
                pane_owner(),
                GENERATION,
                RoutedProductAction::ReadingQuery(ReadingQueryAction::Reveal {
                    view: VIEW,
                    target: reveal,
                }),
            ),
        );
        assert_eq!(
            route.result.receipt.authority,
            ProductAuthority::Navigation(SESSION)
        );
        assert!(matches!(
            route.effect.unwrap().effect,
            ProductEffect::Navigation(ProductNavigationAction::ResolveReading { view: VIEW, .. })
        ));
    }

    #[test]
    fn closed_pane_cannot_deliver_a_coincident_background_result() {
        let context = ProductRouteContext::new(SESSION, WORKSPACE, GENERATION, []);
        let owner = ProductActionOwner {
            session: SESSION,
            workspace: WORKSPACE,
            source: ProductActionSource::Background {
                job: ProductJobId(91),
                source_view: Some(VIEW),
            },
        };
        let mut router = ProductActionRouter::new();
        let route = router.route(
            &context,
            ProductActionEnvelope::new(
                owner.clone(),
                GENERATION,
                RoutedProductAction::Audio(ProductAudioAction::Transport(
                    ProjectTransportCommand::Play,
                )),
            ),
        );
        assert!(route.effect.is_none());
        assert_eq!(route.result.owner, owner);
        assert_eq!(
            route.result.diagnostics[0].code,
            ProductDiagnosticCode::UnknownPane
        );
    }

    #[test]
    fn completion_retains_correlation_and_adds_receipt_reveal_and_diagnostic() {
        let mut router = ProductActionRouter::new();
        let route = router.route(
            &context(),
            ProductActionEnvelope::new(
                menu_owner(),
                GENERATION,
                RoutedProductAction::Invocation(native_menu_invocation(
                    ActionId("audec.edit.undo"),
                    None,
                )),
            ),
        );
        let recommendation = RevealRecommendation {
            request: RevealRequest::new(ObjectRef::Material(AssetId(31)), RevealIntent::SelectOnly),
            diagnostics: Vec::new(),
        };
        let completed = route
            .result
            .complete(ProductAuthorityReceipt::Acknowledged)
            .with_reveal(ProductRevealOutcome::Recommended(recommendation))
            .with_diagnostic(ProductDiagnostic::new(
                ProductDiagnosticSeverity::Warning,
                ProductDiagnosticCode::AuthorityRefused,
                "test diagnostic",
            ));

        assert_eq!(completed.state, ProductActionResultState::Completed);
        assert_eq!(completed.owner, menu_owner());
        assert_eq!(completed.generation, GENERATION);
        assert_eq!(completed.receipt.id, ProductActionReceiptId(1));
        assert!(matches!(
            completed.authority_receipt,
            Some(ProductAuthorityReceipt::Acknowledged)
        ));
        assert!(matches!(
            completed.reveal,
            Some(ProductRevealOutcome::Recommended(_))
        ));
        assert_eq!(completed.diagnostics.len(), 1);
    }
}
