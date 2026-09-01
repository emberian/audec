//! GPUI presentation and interaction adapter for the reading/query workbench.
//!
//! This view owns only draft/presentation state. Query and reading observations
//! leave as [`HeadlessRequest`] values suitable for [`HeadlessSessionAdapter`];
//! project edits leave as [`CommandEnvelope`] values; reveal and audition leave
//! as their exact typed targets; persisted query state leaves as a typed
//! document-change publication. Nothing in this module can apply a project
//! command or claim that an external render/import succeeded.

use std::collections::BTreeSet;
use std::rc::Rc;

use gpui::{
    actions, div, prelude::*, px, rgb, App, Context, FocusHandle, Focusable, IntoElement,
    KeyBinding, Render, SharedString, Window,
};

use crate::air_query::workbench::protocol::{
    ForeignHypothesisAllocationDto, HeadlessDispatch, HeadlessOperation, HeadlessProtocolError,
    HeadlessRequest, HeadlessResponseBody, HypothesisSetAllocationDto, ReadingInputDto,
    HEADLESS_PROTOCOL,
};
use crate::air_query::workbench::{
    diff_readings, merge_as_coexisting_hypotheses, plan_reading_import, residual_guide,
    AuditionTarget, FactKindDto, QueryCancellationToken, QueryDocument, QueryExecutionProvenance,
    QueryPageRequest, QueryTermDto, ReadingDiff, ReadingImportOptions, ReadingImportPlan,
    ReadingMergePlan, ResidualGuide, RevealTarget, UnknownSectionPolicy, WorkbenchPaneFactory,
    WorkbenchPaneModel,
};
use crate::command::CommandEnvelope;
use crate::coverage::CoverageField;
use crate::reading::QualifiedEntityId;
use crate::workspace_document::{EditorViewState, WorkspaceItemKind, WorkspaceViewDescriptor};
use crate::workspace_ui::PaneRegistration;

actions!(
    audec_reading_query_view,
    [
        RunReadingQuery,
        CancelReadingQuery,
        NextReadingQueryPage,
        PreviousReadingQueryRow,
        NextReadingQueryRow,
        RevealReadingQueryRow,
        AuditionReadingQueryRow
    ]
);

pub const READING_QUERY_KEY_CONTEXT: &str = "AudecReadingQuery";

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

/// Install once with the application's other key bindings.
pub fn bind_reading_query_view_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(
            "cmd-enter",
            RunReadingQuery,
            Some(READING_QUERY_KEY_CONTEXT),
        ),
        KeyBinding::new(
            "escape",
            CancelReadingQuery,
            Some(READING_QUERY_KEY_CONTEXT),
        ),
        KeyBinding::new(
            "cmd-]",
            NextReadingQueryPage,
            Some(READING_QUERY_KEY_CONTEXT),
        ),
        KeyBinding::new(
            "up",
            PreviousReadingQueryRow,
            Some(READING_QUERY_KEY_CONTEXT),
        ),
        KeyBinding::new("down", NextReadingQueryRow, Some(READING_QUERY_KEY_CONTEXT)),
        KeyBinding::new(
            "enter",
            RevealReadingQueryRow,
            Some(READING_QUERY_KEY_CONTEXT),
        ),
        KeyBinding::new(
            "a",
            AuditionReadingQueryRow,
            Some(READING_QUERY_KEY_CONTEXT),
        ),
    ]);
}

/// The complete outward authority boundary of the pane.
pub enum ReadingQueryViewEffect {
    /// A host should run this with `HeadlessSessionAdapter` and return its
    /// `HeadlessDispatch` through `accept_dispatch`.
    Observation {
        request: HeadlessRequest,
        cancellation: QueryCancellationToken,
    },
    /// Must be submitted to the aggregate command executor. The view never
    /// applies it and therefore never reports an import as committed.
    Command(CommandEnvelope),
    /// A typed audition/render request.
    Render(AuditionTarget),
    /// A typed semantic-navigation request.
    Reveal(RevealTarget),
    /// Authoritative replacement for the durable descriptor state. Emitted
    /// after, and only after, the view's accepted QueryDocument changes.
    DocumentChanged(QueryDocumentChanged),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryDocumentChangeReason {
    ResidualGuideInstalled,
    QueryPageObserved,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryDocumentChanged {
    pub document: QueryDocument,
    pub reason: QueryDocumentChangeReason,
}

pub type ReadingQueryViewCallback = Rc<dyn Fn(ReadingQueryViewEffect) + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadingQueryOperation {
    QueryFirstPage,
    QueryNextPage,
    VerifyReading,
    PlanReadingImport,
    ImportHypotheses,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadingQueryPaneNotice {
    Ready,
    Pending {
        operation: ReadingQueryOperation,
        request_id: String,
    },
    Empty(String),
    MissingSource(String),
    Cancelled(String),
    Refused(String),
    Observed(String),
    CommandAwaitingExecutor(String),
}

impl ReadingQueryPaneNotice {
    fn label(&self) -> (&'static str, u32, String) {
        match self {
            Self::Ready => ("READY", MUTED, "No operation is pending".into()),
            Self::Pending {
                operation,
                request_id,
            } => ("RUNNING", CYAN, format!("{operation:?} · {request_id}")),
            Self::Empty(message) => ("EMPTY", DIM, message.clone()),
            Self::MissingSource(message) => ("SOURCE MISSING", AMBER, message.clone()),
            Self::Cancelled(message) => ("CANCELLED", AMBER, message.clone()),
            Self::Refused(message) => ("REFUSED", MAGENTA, message.clone()),
            Self::Observed(message) => ("OBSERVED", LIME, message.clone()),
            Self::CommandAwaitingExecutor(message) => ("COMMAND PLANNED", AMBER, message.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReadingQueryViewInputs {
    pub query_provenance: Option<QueryExecutionProvenance>,
    pub readings: Vec<ReadingInputDto>,
    pub existing_entities: Vec<QualifiedEntityId>,
    pub unknown_sections: UnknownSectionPolicy,
    pub base_revision: Option<u64>,
    pub hypothesis_allocations: Vec<ForeignHypothesisAllocationDto>,
    pub set_allocations: Vec<HypothesisSetAllocationDto>,
}

impl Default for ReadingQueryViewInputs {
    fn default() -> Self {
        Self {
            query_provenance: None,
            readings: Vec::new(),
            existing_entities: Vec::new(),
            unknown_sections: UnknownSectionPolicy::PreserveOpaque,
            base_revision: None,
            hypothesis_allocations: Vec::new(),
            set_allocations: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryWrapOperator {
    And,
    Or,
    Not,
    Related,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryBuilderRefusal {
    UnknownPath(Vec<usize>),
    ExpectedKind(Vec<usize>),
    ExpectedList(Vec<usize>),
    ExpectedProposal(Vec<usize>),
    CannotRemoveRoot,
    InvalidProposalId,
}

/// Pure editable tree state. Paths are child indices from the root; unary
/// operators expose their child at index zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryBuilderState {
    root: QueryTermDto,
    selected_path: Vec<usize>,
    dirty: bool,
}

impl QueryBuilderState {
    pub fn new(root: QueryTermDto) -> Self {
        Self {
            root,
            selected_path: Vec::new(),
            dirty: false,
        }
    }

    pub fn root(&self) -> &QueryTermDto {
        &self.root
    }

    pub fn selected_path(&self) -> &[usize] {
        &self.selected_path
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn select(&mut self, path: Vec<usize>) -> Result<(), QueryBuilderRefusal> {
        term_at(&self.root, &path).ok_or_else(|| QueryBuilderRefusal::UnknownPath(path.clone()))?;
        self.selected_path = path;
        Ok(())
    }

    pub fn cycle_selected_kind(&mut self) -> Result<(), QueryBuilderRefusal> {
        let path = self.selected_path.clone();
        let term = term_at_mut(&mut self.root, &path)
            .ok_or_else(|| QueryBuilderRefusal::UnknownPath(path.clone()))?;
        let QueryTermDto::Kind { kind } = term else {
            return Err(QueryBuilderRefusal::ExpectedKind(path));
        };
        *kind = match kind {
            FactKindDto::Object => FactKindDto::Source,
            FactKindDto::Source => FactKindDto::Parameter,
            FactKindDto::Parameter => FactKindDto::Hypothesis,
            FactKindDto::Hypothesis => FactKindDto::Object,
        };
        self.dirty = true;
        Ok(())
    }

    pub fn wrap_selected(
        &mut self,
        operator: QueryWrapOperator,
    ) -> Result<(), QueryBuilderRefusal> {
        let path = self.selected_path.clone();
        let term = term_at_mut(&mut self.root, &path)
            .ok_or_else(|| QueryBuilderRefusal::UnknownPath(path.clone()))?;
        let previous = term.clone();
        *term = match operator {
            QueryWrapOperator::And => QueryTermDto::And {
                terms: vec![previous],
            },
            QueryWrapOperator::Or => QueryTermDto::Or {
                terms: vec![previous],
            },
            QueryWrapOperator::Not => QueryTermDto::Not {
                term: Box::new(previous),
            },
            QueryWrapOperator::Related => QueryTermDto::Related {
                to: Box::new(previous),
            },
        };
        self.selected_path.push(0);
        self.dirty = true;
        Ok(())
    }

    pub fn append_kind(&mut self, kind: FactKindDto) -> Result<(), QueryBuilderRefusal> {
        let path = self.selected_path.clone();
        let term = term_at_mut(&mut self.root, &path)
            .ok_or_else(|| QueryBuilderRefusal::UnknownPath(path.clone()))?;
        let terms = match term {
            QueryTermDto::And { terms } | QueryTermDto::Or { terms } => terms,
            _ => return Err(QueryBuilderRefusal::ExpectedList(path)),
        };
        terms.push(QueryTermDto::Kind { kind });
        self.selected_path.push(terms.len() - 1);
        self.dirty = true;
        Ok(())
    }

    pub fn adjust_proposal_id(&mut self, delta: i64) -> Result<(), QueryBuilderRefusal> {
        let path = self.selected_path.clone();
        let term = term_at_mut(&mut self.root, &path)
            .ok_or_else(|| QueryBuilderRefusal::UnknownPath(path.clone()))?;
        let QueryTermDto::NotExplainedBy { proposal_id } = term else {
            return Err(QueryBuilderRefusal::ExpectedProposal(path));
        };
        let next = if delta.is_negative() {
            proposal_id.checked_sub(delta.unsigned_abs())
        } else {
            proposal_id.checked_add(delta as u64)
        }
        .filter(|value| *value != 0)
        .ok_or(QueryBuilderRefusal::InvalidProposalId)?;
        *proposal_id = next;
        self.dirty = true;
        Ok(())
    }

    pub fn remove_selected(&mut self) -> Result<(), QueryBuilderRefusal> {
        if self.selected_path.is_empty() {
            return Err(QueryBuilderRefusal::CannotRemoveRoot);
        }
        let mut parent_path = self.selected_path.clone();
        let index = parent_path.pop().expect("non-empty path checked above");
        let parent = term_at_mut(&mut self.root, &parent_path)
            .ok_or_else(|| QueryBuilderRefusal::UnknownPath(parent_path.clone()))?;
        let terms = match parent {
            QueryTermDto::And { terms } | QueryTermDto::Or { terms } => terms,
            _ => return Err(QueryBuilderRefusal::ExpectedList(parent_path)),
        };
        if index >= terms.len() {
            return Err(QueryBuilderRefusal::UnknownPath(self.selected_path.clone()));
        }
        terms.remove(index);
        self.selected_path = parent_path;
        self.dirty = true;
        Ok(())
    }

    fn accept(&mut self, root: QueryTermDto) {
        self.root = root;
        self.selected_path.clear();
        self.dirty = false;
    }
}

fn term_at<'a>(term: &'a QueryTermDto, path: &[usize]) -> Option<&'a QueryTermDto> {
    let Some((&head, tail)) = path.split_first() else {
        return Some(term);
    };
    let child = match term {
        QueryTermDto::Related { to } | QueryTermDto::Not { term: to } if head == 0 => to,
        QueryTermDto::And { terms } | QueryTermDto::Or { terms } => terms.get(head)?,
        _ => return None,
    };
    term_at(child, tail)
}

fn term_at_mut<'a>(term: &'a mut QueryTermDto, path: &[usize]) -> Option<&'a mut QueryTermDto> {
    let Some((&head, tail)) = path.split_first() else {
        return Some(term);
    };
    let child = match term {
        QueryTermDto::Related { to } | QueryTermDto::Not { term: to } if head == 0 => to,
        QueryTermDto::And { terms } | QueryTermDto::Or { terms } => terms.get_mut(head)?,
        _ => return None,
    };
    term_at_mut(child, tail)
}

#[derive(Clone, Debug)]
struct PendingRequest {
    operation: ReadingQueryOperation,
    request_id: String,
    cancellation: QueryCancellationToken,
}

/// Snapshot useful to shell chrome, tests, and accessibility adapters without
/// asking them to inspect GPUI elements.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadingQueryPresentation {
    pub notice: ReadingQueryPaneNotice,
    pub query_label: String,
    pub query_dirty: bool,
    pub result_rows: usize,
    pub selected_row: Option<usize>,
    pub result_pages: usize,
    pub reading_count: usize,
    pub selected_reading: Option<usize>,
    pub has_diff: bool,
    pub merged_hypotheses: usize,
    pub residual_targets: usize,
}

pub struct ReadingQueryView {
    model: WorkbenchPaneModel,
    builder: QueryBuilderState,
    inputs: ReadingQueryViewInputs,
    selected_reading: Option<usize>,
    diff: Option<ReadingDiff>,
    merge: Option<ReadingMergePlan>,
    reading_plans: Vec<ReadingImportPlan>,
    residual: Option<ResidualGuide>,
    pending: Option<PendingRequest>,
    notice: ReadingQueryPaneNotice,
    next_request: u64,
    callback: ReadingQueryViewCallback,
    focus_handle: FocusHandle,
}

impl ReadingQueryView {
    pub fn new(
        document: QueryDocument,
        callback: ReadingQueryViewCallback,
        cx: &mut Context<Self>,
    ) -> Result<Self, crate::air_query::workbench::WorkbenchError> {
        let model = WorkbenchPaneFactory::model(document)?;
        Ok(Self::from_model(model, callback, cx))
    }

    pub fn from_model(
        model: WorkbenchPaneModel,
        callback: ReadingQueryViewCallback,
        cx: &mut Context<Self>,
    ) -> Self {
        let builder = QueryBuilderState::new(model.document().query.clone());
        let notice = if model.rows().is_empty() {
            ReadingQueryPaneNotice::Empty("The query has no executed result page".into())
        } else {
            ReadingQueryPaneNotice::Ready
        };
        Self {
            model,
            builder,
            inputs: ReadingQueryViewInputs::default(),
            selected_reading: None,
            diff: None,
            merge: None,
            reading_plans: Vec::new(),
            residual: None,
            pending: None,
            notice,
            next_request: 1,
            callback,
            focus_handle: cx.focus_handle().tab_stop(true),
        }
    }

    pub fn presentation(&self) -> ReadingQueryPresentation {
        ReadingQueryPresentation {
            notice: self.notice.clone(),
            query_label: self.builder.root.stable_label(),
            query_dirty: self.builder.dirty,
            result_rows: self.model.rows().len(),
            selected_row: self.model.selected_row(),
            result_pages: self.model.document().results.len(),
            reading_count: self.inputs.readings.len(),
            selected_reading: self.selected_reading,
            has_diff: self.diff.is_some(),
            merged_hypotheses: self
                .merge
                .as_ref()
                .map_or(0, |merge| merge.hypothesis_groups.len()),
            residual_targets: self
                .residual
                .as_ref()
                .map_or(0, |guide| guide.auditions.len()),
        }
    }

    pub fn model(&self) -> &WorkbenchPaneModel {
        &self.model
    }

    pub fn builder(&self) -> &QueryBuilderState {
        &self.builder
    }

    pub fn observe_inputs(&mut self, inputs: ReadingQueryViewInputs, cx: &mut Context<Self>) {
        self.inputs = inputs;
        self.selected_reading = (!self.inputs.readings.is_empty()).then_some(0);
        self.diff = None;
        self.merge = None;
        self.reading_plans.clear();
        self.notice = if self.inputs.query_provenance.is_none() {
            ReadingQueryPaneNotice::MissingSource(
                "No fact snapshot provenance is attached; query execution is disabled".into(),
            )
        } else if self.inputs.readings.is_empty() {
            ReadingQueryPaneNotice::Empty("No portable readings are loaded".into())
        } else if self
            .inputs
            .readings
            .iter()
            .any(|reading| reading.local_source.is_none())
        {
            ReadingQueryPaneNotice::MissingSource(
                "At least one reading has no local source match; it remains graph-only".into(),
            )
        } else {
            ReadingQueryPaneNotice::Ready
        };
        cx.notify();
    }

    pub fn install_residual_guide(
        &mut self,
        document_id: crate::air_query::workbench::QueryDocumentId,
        title: impl Into<String>,
        field: &CoverageField,
        comparison_id: u64,
        proposal_id: u64,
        limit: usize,
        cx: &mut Context<Self>,
    ) {
        match residual_guide(document_id, title, field, comparison_id, proposal_id, limit) {
            Ok(guide) => match WorkbenchPaneFactory::model(guide.query_document.clone()) {
                Ok(model) => {
                    self.builder.accept(guide.query_document.query.clone());
                    self.model = model;
                    self.publish_document_changed(
                        QueryDocumentChangeReason::ResidualGuideInstalled,
                    );
                    self.notice = if guide.auditions.is_empty() {
                        ReadingQueryPaneNotice::Empty(
                            "Residual coverage contains no ranked hotspot".into(),
                        )
                    } else {
                        ReadingQueryPaneNotice::Observed(format!(
                            "{} residual hotspot targets prepared",
                            guide.auditions.len()
                        ))
                    };
                    self.residual = Some(guide);
                }
                Err(error) => self.refuse(error.to_string()),
            },
            Err(error) => self.refuse(error.to_string()),
        }
        cx.notify();
    }

    pub fn select_query_term(&mut self, path: Vec<usize>, cx: &mut Context<Self>) {
        if let Err(error) = self.builder.select(path) {
            self.refuse(format!("query builder: {error:?}"));
        }
        cx.notify();
    }

    pub fn cycle_selected_kind(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.builder.cycle_selected_kind() {
            self.refuse(format!("query builder: {error:?}"));
        }
        cx.notify();
    }

    pub fn wrap_selected(&mut self, operator: QueryWrapOperator, cx: &mut Context<Self>) {
        if let Err(error) = self.builder.wrap_selected(operator) {
            self.refuse(format!("query builder: {error:?}"));
        }
        cx.notify();
    }

    pub fn append_kind(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.builder.append_kind(FactKindDto::Object) {
            self.refuse(format!("query builder: {error:?}"));
        }
        cx.notify();
    }

    pub fn remove_selected_term(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.builder.remove_selected() {
            self.refuse(format!("query builder: {error:?}"));
        }
        cx.notify();
    }

    pub fn adjust_selected_proposal(&mut self, delta: i64, cx: &mut Context<Self>) {
        if let Err(error) = self.builder.adjust_proposal_id(delta) {
            self.refuse(format!("query builder: {error:?}"));
        }
        cx.notify();
    }

    pub fn run_first_page(&mut self, cx: &mut Context<Self>) {
        self.run_page(None, ReadingQueryOperation::QueryFirstPage, cx);
    }

    pub fn request_next_page(&mut self, cx: &mut Context<Self>) {
        if self.builder.dirty {
            self.refuse("The edited query must run from its first page before paging".into());
            cx.notify();
            return;
        }
        let cursor = self
            .model
            .document()
            .latest_result()
            .and_then(|result| result.page.next_cursor.clone());
        let Some(cursor) = cursor else {
            self.notice =
                ReadingQueryPaneNotice::Empty("The current result has no additional page".into());
            cx.notify();
            return;
        };
        self.run_page(Some(cursor), ReadingQueryOperation::QueryNextPage, cx);
    }

    fn run_page(
        &mut self,
        cursor: Option<String>,
        operation: ReadingQueryOperation,
        cx: &mut Context<Self>,
    ) {
        let Some(provenance) = self.inputs.query_provenance.clone() else {
            self.notice = ReadingQueryPaneNotice::MissingSource(
                "A fact snapshot digest/revision is required to run this query".into(),
            );
            cx.notify();
            return;
        };
        let mut document = self.model.document().clone();
        if document.query != *self.builder.root() {
            if let Err(error) = document.replace_query(self.builder.root().clone()) {
                self.refuse(error.to_string());
                cx.notify();
                return;
            }
        }
        self.start_observation(
            operation,
            HeadlessOperation::QueryPage {
                document,
                provenance,
                page: QueryPageRequest { limit: 100, cursor },
            },
            cx,
        );
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else {
            self.notice =
                ReadingQueryPaneNotice::Refused("There is no in-flight work to cancel".into());
            cx.notify();
            return;
        };
        pending.cancellation.cancel();
        self.notice = ReadingQueryPaneNotice::Cancelled(format!(
            "{} was cancelled; late results will be ignored",
            pending.request_id
        ));
        cx.notify();
    }

    pub fn accept_dispatch(&mut self, dispatch: HeadlessDispatch, cx: &mut Context<Self>) {
        let request_id = dispatch.response().request_id.clone();
        if !self.finish_pending(&request_id) {
            self.refuse(format!("unknown or late response {request_id}"));
            cx.notify();
            return;
        }
        match dispatch {
            HeadlessDispatch::Observation(response) => match response.body {
                HeadlessResponseBody::QueryPage { document } => {
                    match WorkbenchPaneFactory::model(document.clone()) {
                        Ok(model) => {
                            let count = model.rows().len();
                            self.model = model;
                            self.builder.accept(document.query);
                            self.publish_document_changed(
                                QueryDocumentChangeReason::QueryPageObserved,
                            );
                            self.notice = if count == 0 {
                                ReadingQueryPaneNotice::Empty(
                                    "The query completed with no matching facts".into(),
                                )
                            } else {
                                ReadingQueryPaneNotice::Observed(format!(
                                    "Query page contains {count} provenance-bearing rows"
                                ))
                            };
                        }
                        Err(error) => self.refuse(error.to_string()),
                    }
                }
                HeadlessResponseBody::ReadingVerified { tier } => {
                    self.notice = ReadingQueryPaneNotice::Observed(format!(
                        "Reading verification tier: {tier:?}"
                    ));
                }
                HeadlessResponseBody::ImportPlanned { plan } => {
                    self.notice = ReadingQueryPaneNotice::Observed(format!(
                        "Import plan: {} qualified entities, {} preserved sections, {:?}",
                        plan.entities.len(),
                        plan.preserved_sections.len(),
                        plan.verification
                    ));
                }
                HeadlessResponseBody::CommandPlanned { .. }
                | HeadlessResponseBody::RenderPlanned { .. } => self.refuse(
                    "The headless adapter returned an effect body as an observation".into(),
                ),
            },
            HeadlessDispatch::Command { envelope, .. } => {
                (self.callback)(ReadingQueryViewEffect::Command(envelope));
                self.notice = ReadingQueryPaneNotice::CommandAwaitingExecutor(
                    "The aggregate executor must validate, journal, and apply this envelope".into(),
                );
            }
            HeadlessDispatch::Render { target, .. } => {
                (self.callback)(ReadingQueryViewEffect::Render(target));
                self.notice = ReadingQueryPaneNotice::Observed(
                    "Render intent handed to the audio host".into(),
                );
            }
        }
        cx.notify();
    }

    pub fn accept_refusal(
        &mut self,
        request_id: &str,
        error: HeadlessProtocolError,
        cx: &mut Context<Self>,
    ) {
        if self.finish_pending(request_id) {
            self.refuse(error.to_string());
        } else {
            self.refuse(format!("unknown or late refusal {request_id}: {error}"));
        }
        cx.notify();
    }

    /// Complete one known observation after a project/session worker failed
    /// before producing a headless dispatch. Unknown or cancelled request IDs
    /// are ignored so a late worker cannot replace the pane's current status.
    pub fn complete_external_failure(
        &mut self,
        request_id: &str,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.finish_pending(request_id) {
            return false;
        }
        self.refuse(message.into());
        cx.notify();
        true
    }

    pub fn select_result_row(&mut self, row: usize, cx: &mut Context<Self>) {
        match self.model.select_row(row) {
            Ok(()) => self.notice = ReadingQueryPaneNotice::Ready,
            Err(error) => self.refuse(format!("{error:?}")),
        }
        cx.notify();
    }

    pub fn select_relative_row(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.model.rows().len();
        if count == 0 {
            self.notice = ReadingQueryPaneNotice::Empty("There are no result rows".into());
            cx.notify();
            return;
        }
        let current = self.model.selected_row().unwrap_or(0);
        let next = current.saturating_add_signed(delta).min(count - 1);
        self.select_result_row(next, cx);
    }

    pub fn request_reveal(&mut self, cx: &mut Context<Self>) {
        match self.model.reveal_target() {
            Ok(target) => {
                (self.callback)(ReadingQueryViewEffect::Reveal(target));
                self.notice = ReadingQueryPaneNotice::Observed("Reveal intent handed off".into());
            }
            Err(error) => self.refuse(format!("{error:?}")),
        }
        cx.notify();
    }

    pub fn request_audition(&mut self, cx: &mut Context<Self>) {
        match self.model.audition_target() {
            Ok(target) => {
                (self.callback)(ReadingQueryViewEffect::Render(target));
                self.notice = ReadingQueryPaneNotice::Observed("Audition intent handed off".into());
            }
            Err(error) => self.refuse(format!("{error:?}")),
        }
        cx.notify();
    }

    pub fn request_residual_audition(&mut self, index: usize, cx: &mut Context<Self>) {
        let target = self
            .residual
            .as_ref()
            .and_then(|guide| guide.auditions.get(index))
            .cloned();
        match target {
            Some(target) => {
                (self.callback)(ReadingQueryViewEffect::Render(target));
                self.notice = ReadingQueryPaneNotice::Observed(format!(
                    "Residual hotspot {} handed to the render host",
                    index + 1
                ));
            }
            None => self.refuse(format!("unknown residual hotspot {index}")),
        }
        cx.notify();
    }

    pub fn select_reading(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.inputs.readings.len() {
            self.refuse(format!("unknown reading row {index}"));
        } else {
            self.selected_reading = Some(index);
            self.notice = ReadingQueryPaneNotice::Ready;
        }
        cx.notify();
    }

    pub fn request_verify_reading(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.selected_reading_input().cloned() else {
            self.notice = ReadingQueryPaneNotice::Empty("No reading is selected".into());
            cx.notify();
            return;
        };
        self.start_observation(
            ReadingQueryOperation::VerifyReading,
            HeadlessOperation::VerifyReading {
                reading: input.reading,
                local_source: input.local_source,
            },
            cx,
        );
    }

    pub fn request_plan_reading_import(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.selected_reading_input().cloned() else {
            self.notice = ReadingQueryPaneNotice::Empty("No reading is selected".into());
            cx.notify();
            return;
        };
        self.start_observation(
            ReadingQueryOperation::PlanReadingImport,
            HeadlessOperation::PlanReadingImport {
                reading: input.reading,
                local_source: input.local_source,
                existing: self.inputs.existing_entities.clone(),
                unknown_sections: self.inputs.unknown_sections,
            },
            cx,
        );
    }

    pub fn preview_reading_diff(&mut self, cx: &mut Context<Self>) {
        let Some(left_index) = self.selected_reading else {
            self.notice = ReadingQueryPaneNotice::Empty("No reading is selected".into());
            cx.notify();
            return;
        };
        let right_index = (0..self.inputs.readings.len()).find(|index| *index != left_index);
        let Some(right_index) = right_index else {
            self.notice =
                ReadingQueryPaneNotice::Empty("A semantic diff needs two reading versions".into());
            cx.notify();
            return;
        };
        match diff_readings(
            &self.inputs.readings[left_index].reading,
            &self.inputs.readings[right_index].reading,
        ) {
            Ok(diff) => {
                let changed = diff
                    .sections
                    .iter()
                    .filter(|change| {
                        change.kind != crate::air_query::workbench::SectionChangeKind::Unchanged
                    })
                    .count()
                    + diff
                        .entities
                        .iter()
                        .filter(|change| {
                            change.kind != crate::air_query::workbench::SectionChangeKind::Unchanged
                        })
                        .count();
                self.diff = Some(diff);
                self.notice = ReadingQueryPaneNotice::Observed(format!(
                    "Semantic diff contains {changed} changed qualified items"
                ));
            }
            Err(error) => self.refuse(format!("{error:?}")),
        }
        cx.notify();
    }

    pub fn preview_reading_merge(&mut self, cx: &mut Context<Self>) {
        match self.build_reading_plans().and_then(|plans| {
            merge_as_coexisting_hypotheses(&plans)
                .map(|merge| (plans, merge))
                .map_err(|error| format!("{error:?}"))
        }) {
            Ok((plans, merge)) => {
                let alternatives = merge
                    .hypothesis_groups
                    .iter()
                    .map(|group| group.alternatives.len())
                    .sum::<usize>();
                self.reading_plans = plans;
                self.merge = Some(merge);
                self.notice = ReadingQueryPaneNotice::Observed(format!(
                    "Merge preview keeps {alternatives} reading-qualified alternatives"
                ));
            }
            Err(error) => self.refuse(error),
        }
        cx.notify();
    }

    pub fn request_hypothesis_import(&mut self, cx: &mut Context<Self>) {
        if self.inputs.readings.is_empty() {
            self.notice = ReadingQueryPaneNotice::Empty("No readings are loaded".into());
            cx.notify();
            return;
        }
        let Some(base_revision) = self.inputs.base_revision else {
            self.notice = ReadingQueryPaneNotice::MissingSource(
                "A project base revision and explicit ID allocations are required".into(),
            );
            cx.notify();
            return;
        };
        self.start_observation(
            ReadingQueryOperation::ImportHypotheses,
            HeadlessOperation::ImportHypotheses {
                readings: self.inputs.readings.clone(),
                existing: self.inputs.existing_entities.clone(),
                unknown_sections: self.inputs.unknown_sections,
                base_revision,
                hypothesis_allocations: self.inputs.hypothesis_allocations.clone(),
                set_allocations: self.inputs.set_allocations.clone(),
            },
            cx,
        );
    }

    pub fn request_reading_reveal(&mut self, id: &QualifiedEntityId, cx: &mut Context<Self>) {
        let result = self
            .reading_plans
            .iter()
            .find(|plan| plan.reading_id == id.reading)
            .map_or_else(
                || Err(format!("unknown planned reading entity {id:?}")),
                |plan| plan.reveal_target(id).map_err(|error| format!("{error:?}")),
            );
        match result {
            Ok(target) => (self.callback)(ReadingQueryViewEffect::Reveal(target)),
            Err(error) => self.refuse(error),
        }
        cx.notify();
    }

    pub fn request_reading_audition(&mut self, id: &QualifiedEntityId, cx: &mut Context<Self>) {
        let result = self
            .reading_plans
            .iter()
            .find(|plan| plan.reading_id == id.reading)
            .map_or_else(
                || Err(format!("unknown planned reading entity {id:?}")),
                |plan| {
                    plan.audition_target(id)
                        .map_err(|error| format!("{error:?}"))
                },
            );
        match result {
            Ok(target) => (self.callback)(ReadingQueryViewEffect::Render(target)),
            Err(error) => self.refuse(error),
        }
        cx.notify();
    }

    fn selected_reading_input(&self) -> Option<&ReadingInputDto> {
        self.selected_reading
            .and_then(|index| self.inputs.readings.get(index))
    }

    fn build_reading_plans(&self) -> Result<Vec<ReadingImportPlan>, String> {
        if self.inputs.readings.is_empty() {
            return Err("No readings are loaded".into());
        }
        let existing = self
            .inputs
            .existing_entities
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        self.inputs
            .readings
            .iter()
            .map(|input| {
                let local = input.local_source.map(Into::into);
                plan_reading_import(
                    &input.reading,
                    local.as_ref(),
                    &existing,
                    ReadingImportOptions {
                        unknown_sections: self.inputs.unknown_sections,
                        require_entity_section: true,
                    },
                )
                .map_err(|error| format!("{error:?}"))
            })
            .collect()
    }

    fn start_observation(
        &mut self,
        operation: ReadingQueryOperation,
        operation_body: HeadlessOperation,
        cx: &mut Context<Self>,
    ) {
        if let Some(pending) = &self.pending {
            self.refuse(format!(
                "{} is still pending; cancel it before starting another operation",
                pending.request_id
            ));
            cx.notify();
            return;
        }
        let request_id = format!(
            "pane-query-{}-{}",
            self.model.document().id.0,
            self.next_request
        );
        self.next_request = self.next_request.saturating_add(1);
        let cancellation = QueryCancellationToken::default();
        let request = HeadlessRequest {
            protocol: HEADLESS_PROTOCOL.into(),
            request_id: request_id.clone(),
            operation: operation_body,
        };
        self.pending = Some(PendingRequest {
            operation,
            request_id: request_id.clone(),
            cancellation: cancellation.clone(),
        });
        self.notice = ReadingQueryPaneNotice::Pending {
            operation,
            request_id,
        };
        (self.callback)(ReadingQueryViewEffect::Observation {
            request,
            cancellation,
        });
        cx.notify();
    }

    fn finish_pending(&mut self, request_id: &str) -> bool {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
        {
            self.pending = None;
            true
        } else {
            false
        }
    }

    fn refuse(&mut self, message: String) {
        self.notice = ReadingQueryPaneNotice::Refused(message);
    }

    fn publish_document_changed(&self, reason: QueryDocumentChangeReason) {
        (self.callback)(ReadingQueryViewEffect::DocumentChanged(
            QueryDocumentChanged {
                document: self.model.document().clone(),
                reason,
            },
        ));
    }

    fn on_run(&mut self, _: &RunReadingQuery, _: &mut Window, cx: &mut Context<Self>) {
        self.run_first_page(cx);
    }

    fn on_cancel(&mut self, _: &CancelReadingQuery, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel(cx);
    }

    fn on_next_page(&mut self, _: &NextReadingQueryPage, _: &mut Window, cx: &mut Context<Self>) {
        self.request_next_page(cx);
    }

    fn on_previous_row(
        &mut self,
        _: &PreviousReadingQueryRow,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_relative_row(-1, cx);
    }

    fn on_next_row(&mut self, _: &NextReadingQueryRow, _: &mut Window, cx: &mut Context<Self>) {
        self.select_relative_row(1, cx);
    }

    fn on_reveal(&mut self, _: &RevealReadingQueryRow, _: &mut Window, cx: &mut Context<Self>) {
        self.request_reveal(cx);
    }

    fn on_audition(&mut self, _: &AuditionReadingQueryRow, _: &mut Window, cx: &mut Context<Self>) {
        self.request_audition(cx);
    }

    fn render_header(&self) -> impl IntoElement {
        let (status, color, detail) = self.notice.label();
        div()
            .h(px(48.0))
            .flex_none()
            .px_4()
            .flex()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .child(
                div()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(self.model.document().title.clone()),
                    )
                    .child(div().text_xs().text_color(rgb(DIM)).child(format!(
                        "document {} · revision {} · {} pages",
                        self.model.document().id.0,
                        self.model.document().revision,
                        self.model.document().results.len()
                    ))),
            )
            .child(
                div()
                    .text_right()
                    .child(div().text_xs().text_color(rgb(color)).child(status))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(detail)),
            )
    }

    fn render_query_builder(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut tree = div().flex().flex_col().gap_1();
        for (path, depth, label) in flatten_terms(self.builder.root()) {
            let selected = path == self.builder.selected_path();
            let id = SharedString::from(format!("reading-query-term-{}", path_label(&path)));
            tree = tree.child(
                div()
                    .id(id)
                    .ml(px(depth as f32 * 14.0))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(if selected { CYAN } else { BORDER }))
                    .bg(rgb(if selected { RAISED } else { PANEL_ALT }))
                    .cursor_pointer()
                    .text_xs()
                    .text_color(rgb(if selected { TEXT } else { MUTED }))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.select_query_term(path.clone(), cx)),
                    )
                    .child(label),
            );
        }
        section("QUERY BUILDER")
            .child(
                div()
                    .mb_2()
                    .text_xs()
                    .text_color(rgb(if self.builder.dirty { AMBER } else { DIM }))
                    .child(if self.builder.dirty {
                        "Draft changed · next run creates a new query-document revision"
                    } else {
                        "Draft matches the persisted query revision"
                    }),
            )
            .child(tree)
            .child(
                div()
                    .mt_3()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        action_button("rq-cycle-kind", "CYCLE KIND", CYAN)
                            .on_click(cx.listener(|this, _, _, cx| this.cycle_selected_kind(cx))),
                    )
                    .child(action_button("rq-wrap-and", "WRAP AND", MUTED).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.wrap_selected(QueryWrapOperator::And, cx)
                        }),
                    ))
                    .child(action_button("rq-wrap-or", "WRAP OR", MUTED).on_click(
                        cx.listener(|this, _, _, cx| this.wrap_selected(QueryWrapOperator::Or, cx)),
                    ))
                    .child(action_button("rq-wrap-not", "WRAP NOT", MUTED).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.wrap_selected(QueryWrapOperator::Not, cx)
                        }),
                    ))
                    .child(action_button("rq-wrap-related", "RELATED", MUTED).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.wrap_selected(QueryWrapOperator::Related, cx)
                        }),
                    ))
                    .child(
                        action_button("rq-add-kind", "+ KIND", LIME)
                            .on_click(cx.listener(|this, _, _, cx| this.append_kind(cx))),
                    )
                    .child(
                        action_button("rq-proposal-down", "PROPOSAL −", MUTED).on_click(
                            cx.listener(|this, _, _, cx| this.adjust_selected_proposal(-1, cx)),
                        ),
                    )
                    .child(
                        action_button("rq-proposal-up", "PROPOSAL +", MUTED).on_click(
                            cx.listener(|this, _, _, cx| this.adjust_selected_proposal(1, cx)),
                        ),
                    )
                    .child(
                        action_button("rq-remove-term", "REMOVE", MAGENTA)
                            .on_click(cx.listener(|this, _, _, cx| this.remove_selected_term(cx))),
                    ),
            )
            .child(
                div()
                    .mt_3()
                    .flex()
                    .gap_2()
                    .child(
                        action_button("rq-run", "RUN · ⌘↩", CYAN)
                            .on_click(cx.listener(|this, _, _, cx| this.run_first_page(cx))),
                    )
                    .child(
                        action_button("rq-next-page", "NEXT PAGE · ⌘]", LIME)
                            .on_click(cx.listener(|this, _, _, cx| this.request_next_page(cx))),
                    )
                    .child(
                        action_button("rq-cancel", "CANCEL · ESC", AMBER)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    ),
            )
    }

    fn render_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = section("RESULTS + DERIVATION");
        if self.model.rows().is_empty() {
            return body
                .child(honest_state(
                    "NO RESULT ROWS",
                    "Run the query against an attached fact snapshot. Empty results remain explicit.",
                    DIM,
                ))
                .into_any_element();
        }
        for (index, hit) in self.model.rows().iter().enumerate() {
            let selected = self.model.selected_row() == Some(index);
            let extent = hit.extent.as_ref().map_or_else(
                || "no audition geometry".into(),
                |extent| format!("{} regions", extent.regions.len()),
            );
            let premise_count = hit.derivation.premises.len();
            body = body.child(
                div()
                    .id(("reading-query-result", index))
                    .mt_2()
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(if selected { CYAN } else { BORDER }))
                    .bg(rgb(if selected { RAISED } else { PANEL_ALT }))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.select_result_row(index, cx)))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(format!("{:?}", hit.fact)),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                        "rule {} · {premise_count} premises · {extent}",
                        hit.derivation.rule
                    ))),
            );
        }
        body.child(
            div()
                .mt_3()
                .flex()
                .gap_2()
                .child(
                    action_button("rq-reveal", "REVEAL · ↩", CYAN)
                        .on_click(cx.listener(|this, _, _, cx| this.request_reveal(cx))),
                )
                .child(
                    action_button("rq-audition", "AUDITION · A", MAGENTA)
                        .on_click(cx.listener(|this, _, _, cx| this.request_audition(cx))),
                ),
        )
        .into_any_element()
    }

    fn render_provenance(&self) -> impl IntoElement {
        let mut body = section("PERSISTED QUERY PROVENANCE");
        if self.model.document().results.is_empty() {
            return body.child(honest_state(
                "NO EXECUTION HISTORY",
                "No result provenance has been persisted for this query document.",
                DIM,
            ));
        }
        for result in self.model.document().results.iter().rev() {
            let digest = short_digest(&result.content_address.bytes);
            let fact_digest = short_digest(&result.provenance.fact_base_digest.bytes);
            body = body.child(
                div()
                    .mt_2()
                    .p_2()
                    .rounded_sm()
                    .bg(rgb(PANEL_ALT))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(format!(
                        "doc r{} · facts r{} {fact_digest} · address {digest} · offset {} · {} hits",
                        result.document_revision,
                        result.provenance.fact_base_revision,
                        result.page_start,
                        result.page.hits.len()
                    ))
                    .when_some(result.provenance.source_revision.clone(), |this, source| {
                        this.child(
                            div()
                                .mt_1()
                                .text_color(rgb(DIM))
                                .child(format!("source revision {source}")),
                        )
                    }),
            );
        }
        body
    }

    fn render_residual(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = section("RESIDUAL-GUIDED ASPECTS");
        let Some(guide) = &self.residual else {
            return body.child(honest_state(
                "NO COVERAGE FIELD",
                "Attach measured coverage to derive honest time-frequency residual targets.",
                AMBER,
            ));
        };
        if guide.auditions.is_empty() {
            return body.child(honest_state(
                "NO RESIDUAL HOTSPOT",
                "The coverage field yielded no non-empty hotspot band.",
                DIM,
            ));
        }
        for (index, target) in guide.auditions.iter().enumerate() {
            let regions = target.extent.regions.len();
            body = body.child(
                action_button(
                    SharedString::from(format!("rq-residual-{index}")),
                    SharedString::from(format!(
                        "AUDITION HOTSPOT {} · {regions} REGION",
                        index + 1
                    )),
                    MAGENTA,
                )
                .mt_2()
                .on_click(
                    cx.listener(move |this, _, _, cx| this.request_residual_audition(index, cx)),
                ),
            );
        }
        body
    }

    fn render_readings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = section("PORTABLE READINGS");
        if self.inputs.readings.is_empty() {
            return body.child(honest_state(
                "NO READINGS",
                "Load portable readings through the host; the pane does not invent demo content.",
                DIM,
            ));
        }
        for (index, input) in self.inputs.readings.iter().enumerate() {
            let selected = self.selected_reading == Some(index);
            let source = if input.local_source.is_some() {
                "local source supplied"
            } else {
                "source absent · graph-only until matched"
            };
            body = body.child(
                div()
                    .id(("rq-reading", index))
                    .mt_2()
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(if selected { CYAN } else { BORDER }))
                    .bg(rgb(PANEL_ALT))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.select_reading(index, cx)))
                    .child(div().text_sm().text_color(rgb(TEXT)).child(format!(
                        "{} · revision {}",
                        input.reading.reading_id, input.reading.revision
                    )))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(if input.local_source.is_some() {
                                LIME
                            } else {
                                AMBER
                            }))
                            .child(source),
                    ),
            );
        }
        body = body.child(
            div()
                .mt_3()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(
                    action_button("rq-verify-reading", "VERIFY", CYAN)
                        .on_click(cx.listener(|this, _, _, cx| this.request_verify_reading(cx))),
                )
                .child(
                    action_button("rq-plan-import", "PLAN IMPORT", LIME).on_click(
                        cx.listener(|this, _, _, cx| this.request_plan_reading_import(cx)),
                    ),
                )
                .child(
                    action_button("rq-diff", "SEMANTIC DIFF", MUTED)
                        .on_click(cx.listener(|this, _, _, cx| this.preview_reading_diff(cx))),
                )
                .child(
                    action_button("rq-merge", "PREVIEW COEXISTENCE", MUTED)
                        .on_click(cx.listener(|this, _, _, cx| this.preview_reading_merge(cx))),
                )
                .child(
                    action_button("rq-import", "PLAN ONE COMMAND", AMBER)
                        .on_click(cx.listener(|this, _, _, cx| this.request_hypothesis_import(cx))),
                ),
        );
        if let Some(diff) = &self.diff {
            body = body.child(div().mt_3().text_xs().text_color(rgb(MUTED)).child(format!(
                "diff {} r{} ↔ {} r{} · source changed {} · {} sections · {} qualified entities",
                diff.left.0,
                diff.left.1,
                diff.right.0,
                diff.right.1,
                diff.source_changed,
                diff.sections.len(),
                diff.entities.len()
            )));
        }
        if let Some(merge) = &self.merge {
            body = body.child(div().mt_3().text_xs().text_color(rgb(AMBER)).child(format!(
                "{} readings · {} entities · {} coexisting hypothesis groups · no winner selected",
                merge.readings.len(),
                merge.entities.len(),
                merge.hypothesis_groups.len()
            )));
        }
        body
    }
}

impl Focusable for ReadingQueryView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ReadingQueryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context(READING_QUERY_KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_run))
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_next_page))
            .on_action(cx.listener(Self::on_previous_row))
            .on_action(cx.listener(Self::on_next_row))
            .on_action(cx.listener(Self::on_reveal))
            .on_action(cx.listener(Self::on_audition))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .child(self.render_header())
            .child(
                div()
                    .id("reading-query-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(self.render_query_builder(cx))
                    .child(self.render_results(cx))
                    .child(self.render_provenance())
                    .child(self.render_residual(cx))
                    .child(self.render_readings(cx)),
            )
    }
}

/// Descriptor-aware factory for the dynamic workspace registry.
#[derive(Clone)]
pub struct ReadingQueryViewFactory {
    callback: ReadingQueryViewCallback,
}

impl ReadingQueryViewFactory {
    pub fn new(callback: ReadingQueryViewCallback) -> Self {
        Self { callback }
    }

    pub fn create_entity(
        &self,
        document: QueryDocument,
        cx: &mut App,
    ) -> Result<gpui::Entity<ReadingQueryView>, SharedString> {
        let model = WorkbenchPaneFactory::model(document)
            .map_err(|error| SharedString::from(error.to_string()))?;
        let callback = Rc::clone(&self.callback);
        Ok(cx.new(|cx| ReadingQueryView::from_model(model, callback, cx)))
    }

    pub fn create_pane(
        &self,
        descriptor: &WorkspaceViewDescriptor,
        cx: &mut App,
    ) -> Result<PaneRegistration, SharedString> {
        match &descriptor.kind {
            WorkspaceItemKind::Extension { namespace, name }
                if namespace == crate::air_query::workbench::WORKBENCH_NAMESPACE
                    && name == crate::air_query::workbench::WORKBENCH_VIEW_NAME => {}
            _ => return Err("descriptor is not a reading/query workbench view".into()),
        }
        let EditorViewState::Extension { data } = &descriptor.state else {
            return Err("reading/query descriptor has non-extension state".into());
        };
        let document = serde_json::from_value::<QueryDocument>(data.clone())
            .map_err(|error| SharedString::from(error.to_string()))?;
        let title = descriptor
            .title_override
            .clone()
            .unwrap_or_else(|| document.title.clone());
        let entity = self.create_entity(document, cx)?;
        Ok(PaneRegistration::entity(title, entity))
    }
}

fn flatten_terms(root: &QueryTermDto) -> Vec<(Vec<usize>, usize, String)> {
    fn visit(
        term: &QueryTermDto,
        path: &mut Vec<usize>,
        depth: usize,
        rows: &mut Vec<(Vec<usize>, usize, String)>,
    ) {
        rows.push((path.clone(), depth, term.stable_label()));
        match term {
            QueryTermDto::Related { to } | QueryTermDto::Not { term: to } => {
                path.push(0);
                visit(to, path, depth + 1, rows);
                path.pop();
            }
            QueryTermDto::And { terms } | QueryTermDto::Or { terms } => {
                for (index, child) in terms.iter().enumerate() {
                    path.push(index);
                    visit(child, path, depth + 1, rows);
                    path.pop();
                }
            }
            QueryTermDto::Kind { .. }
            | QueryTermDto::Within { .. }
            | QueryTermDto::NotExplainedBy { .. } => {}
        }
    }
    let mut rows = Vec::new();
    visit(root, &mut Vec::new(), 0, &mut rows);
    rows
}

fn path_label(path: &[usize]) -> String {
    if path.is_empty() {
        "root".into()
    } else {
        path.iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join("-")
    }
}

fn short_digest(bytes: &[u8; 32]) -> String {
    bytes[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn section(label: &'static str) -> gpui::Div {
    div()
        .p_4()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL))
        .child(div().mb_2().text_xs().text_color(rgb(CYAN)).child(label))
}

fn action_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    color: u32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .text_xs()
        .text_color(rgb(color))
        .cursor_pointer()
        .child(label.into())
}

fn honest_state(title: &'static str, detail: &'static str, color: u32) -> impl IntoElement {
    div()
        .mt_2()
        .p_3()
        .rounded_sm()
        .border_1()
        .border_color(rgb(color))
        .bg(rgb(PANEL_ALT))
        .child(div().text_xs().text_color(rgb(color)).child(title))
        .child(div().mt_1().text_xs().text_color(rgb(MUTED)).child(detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(kind: FactKindDto) -> QueryTermDto {
        QueryTermDto::Kind { kind }
    }

    #[test]
    fn query_builder_edits_selected_paths_without_rewriting_siblings() {
        let mut builder = QueryBuilderState::new(QueryTermDto::And {
            terms: vec![kind(FactKindDto::Object), kind(FactKindDto::Parameter)],
        });
        builder.select(vec![1]).unwrap();
        builder.cycle_selected_kind().unwrap();
        builder.wrap_selected(QueryWrapOperator::Not).unwrap();

        assert_eq!(
            builder.root(),
            &QueryTermDto::And {
                terms: vec![
                    kind(FactKindDto::Object),
                    QueryTermDto::Not {
                        term: Box::new(kind(FactKindDto::Hypothesis))
                    }
                ]
            }
        );
        assert_eq!(builder.selected_path(), &[1, 0]);
        assert!(builder.is_dirty());
    }

    #[test]
    fn query_builder_refuses_root_removal_and_wrong_editor_shape() {
        let mut builder = QueryBuilderState::new(kind(FactKindDto::Object));
        assert_eq!(
            builder.remove_selected(),
            Err(QueryBuilderRefusal::CannotRemoveRoot)
        );
        assert_eq!(
            builder.append_kind(FactKindDto::Source),
            Err(QueryBuilderRefusal::ExpectedList(Vec::new()))
        );
        assert_eq!(
            builder.adjust_proposal_id(-1),
            Err(QueryBuilderRefusal::ExpectedProposal(Vec::new()))
        );
    }

    #[test]
    fn flattening_and_paths_are_stable_for_keyboard_or_accessibility_adapters() {
        let root = QueryTermDto::Or {
            terms: vec![
                kind(FactKindDto::Object),
                QueryTermDto::Related {
                    to: Box::new(kind(FactKindDto::Source)),
                },
            ],
        };
        let rows = flatten_terms(&root);
        assert_eq!(
            rows.iter().map(|row| row.0.clone()).collect::<Vec<_>>(),
            vec![vec![], vec![0], vec![1], vec![1, 0]]
        );
        assert_eq!(path_label(&[]), "root");
        assert_eq!(path_label(&[1, 0]), "1-0");
    }

    #[test]
    fn pane_notice_never_conflates_planned_command_with_success() {
        let notice = ReadingQueryPaneNotice::CommandAwaitingExecutor(
            "aggregate executor has not replied".into(),
        );
        let (label, _, detail) = notice.label();
        assert_eq!(label, "COMMAND PLANNED");
        assert!(detail.contains("not replied"));
    }
}
