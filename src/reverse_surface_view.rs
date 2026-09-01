//! Live GPUI host for reverse-analysis working surfaces.
//!
//! This is a view over [`crate::reverse_surface`], not another interpretation
//! or audio authority. Project/session deliveries are addressed by the host;
//! comparison selection is delegated to one shared controller per persisted
//! workspace view; every reveal, edit, and audition leaves through a typed
//! callback. A descriptor which resolves but whose document has not arrived
//! remains visibly missing and can hydrate in place later.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gpui::{
    div, prelude::*, px, rgb, rgba, App, Context, FocusHandle, Focusable, IntoElement, Render,
    SharedString, WeakEntity, Window,
};

use crate::comparison::{ComparisonId, ComparisonMetrics};
use crate::comparison_controller::{ComparisonChannel, ComparisonController};
use crate::pane_audio::result_lifecycle::{
    AnalysisActionTicket, AnalysisAuditionAvailability, AnalysisAuditionIntent,
    AnalysisDurableAction, AnalysisDurableCompletion, AnalysisDurableIntent,
    AnalysisDurableReceipt, AnalysisLifecycleError, AnalysisPresentedActionState,
    AnalysisResultController, AnalysisResultPresentation, TemporaryAnalysisResult,
};
use crate::pane_audio::{AnalysisPaneBridge, PaneAudioKind};
use crate::pane_session_binding::{PaneSessionPayload, PaneSessionSnapshot};
use crate::project_controller::ObjectRef;
use crate::reverse_surface::{
    EditAuthority, ReverseSurfaceBody, ReverseSurfaceDocument, ReverseSurfaceError,
    ReverseSurfaceLoad, ReverseSurfacePaneModel, ReverseSurfaceSnapshot, ReverseSurfaceStore,
    SurfaceActionIntent, SurfaceAuditionIntent, SurfaceChannelAvailability,
    SurfaceChannelMeasurement, SurfaceChannelSemantic, SurfaceChannelState, SurfaceEditConsequence,
    SurfaceEvidence,
};
use crate::workspace_document::{WorkspaceViewDescriptor, WorkspaceViewId};
use crate::workspace_ui::PaneRegistration;

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

pub type SharedReverseSurfaceStore = Arc<Mutex<ReverseSurfaceStore>>;
pub type SharedComparisonController = Arc<Mutex<ComparisonController>>;
pub type ReverseSurfaceViewCallback = Arc<dyn Fn(ReverseSurfaceViewEvent) + Send + Sync + 'static>;
pub type ReverseAnalysisResultCallback =
    Arc<dyn Fn(ReverseAnalysisResultEvent) + Send + Sync + 'static>;
type SharedAnalysisResults = Arc<Mutex<BTreeMap<String, AnalysisResultController>>>;
type SharedAnalysisResultCallback = Arc<Mutex<Option<ReverseAnalysisResultCallback>>>;

/// The complete mutation boundary of a reverse pane.
#[derive(Clone, Debug, PartialEq)]
pub enum ReverseSurfaceViewEvent {
    Audition {
        view: WorkspaceViewId,
        intent: SurfaceAuditionIntent,
    },
    Action {
        view: WorkspaceViewId,
        intent: SurfaceActionIntent,
    },
}

/// Optional second authority seam for temporary Finding results. Existing
/// hosts can keep constructing the factory without it; installing this
/// callback makes Keep/Apply/Compare/Make sample and semantic auditions live.
#[derive(Clone, Debug)]
pub enum ReverseAnalysisResultEvent {
    Durable {
        view: WorkspaceViewId,
        intent: AnalysisDurableIntent,
    },
    Audition {
        view: WorkspaceViewId,
        intent: AnalysisAuditionIntent,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReverseAnalysisResultError {
    Lifecycle(AnalysisLifecycleError),
    DuplicateFinding(crate::project_controller::FindingRef),
    UnknownFinding(crate::project_controller::FindingRef),
    HostAuthorityUnavailable,
    AudioAuthority(String),
}

impl std::fmt::Display for ReverseAnalysisResultError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lifecycle(error) => write!(formatter, "{error}"),
            Self::DuplicateFinding(finding) => {
                write!(
                    formatter,
                    "analysis result {:?} is already registered",
                    finding
                )
            }
            Self::UnknownFinding(finding) => {
                write!(formatter, "analysis result {:?} is not registered", finding)
            }
            Self::HostAuthorityUnavailable => {
                formatter.write_str("analysis result host authority is not connected for this pane")
            }
            Self::AudioAuthority(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for ReverseAnalysisResultError {}

impl From<AnalysisLifecycleError> for ReverseAnalysisResultError {
    fn from(value: AnalysisLifecycleError) -> Self {
        Self::Lifecycle(value)
    }
}

/// Small integration seam owned by the application shell.
///
/// Install `create_pane` as the dynamic workspace factory, route addressed
/// session payloads through `deliver`, and drain callback events through the
/// existing object-navigation / comparison-runtime controllers. Calling
/// `insert_document` upgrades every matching Missing pane without recreation.
#[derive(Clone)]
pub struct ReverseSurfaceViewFactory {
    store: SharedReverseSurfaceStore,
    callback: ReverseSurfaceViewCallback,
    views: Rc<RefCell<BTreeMap<WorkspaceViewId, WeakEntity<ReverseSurfaceView>>>>,
    controllers: Rc<RefCell<BTreeMap<WorkspaceViewId, SharedComparisonController>>>,
    analysis_results: SharedAnalysisResults,
    analysis_callback: SharedAnalysisResultCallback,
}

impl ReverseSurfaceViewFactory {
    pub fn new(store: SharedReverseSurfaceStore, callback: ReverseSurfaceViewCallback) -> Self {
        Self {
            store,
            callback,
            views: Rc::new(RefCell::new(BTreeMap::new())),
            controllers: Rc::new(RefCell::new(BTreeMap::new())),
            analysis_results: Arc::new(Mutex::new(BTreeMap::new())),
            analysis_callback: Arc::new(Mutex::new(None)),
        }
    }

    /// Install the host boundary for lifecycle effects. The pane never falls
    /// back to a local edit or player when this callback is absent.
    pub fn set_analysis_result_callback(
        &self,
        callback: ReverseAnalysisResultCallback,
        cx: &mut App,
    ) {
        *lock_unpoison(&self.analysis_callback) = Some(callback);
        self.notify_all_views(cx);
    }

    pub fn clear_analysis_result_callback(&self, cx: &mut App) {
        *lock_unpoison(&self.analysis_callback) = None;
        self.notify_all_views(cx);
    }

    /// Publish one temporary result into every pane addressing its exact
    /// Finding. Duplicate registration is explicit so an async rerun cannot
    /// silently replace a card with different generation pins.
    pub fn insert_analysis_result(
        &self,
        result: TemporaryAnalysisResult,
        cx: &mut App,
    ) -> Result<(), ReverseAnalysisResultError> {
        let finding = result.finding;
        let key = ObjectRef::Finding(finding).address();
        let mut results = lock_unpoison(&self.analysis_results);
        if results.contains_key(&key) {
            return Err(ReverseAnalysisResultError::DuplicateFinding(finding));
        }
        results.insert(key, AnalysisResultController::new(result));
        drop(results);
        self.refresh_matching_finding(finding, cx);
        Ok(())
    }

    /// Accept an authoritative completion and refresh every duplicate view of
    /// that Finding. The returned receipt is the sole durable navigation fact.
    pub fn complete_analysis_result(
        &self,
        completion: AnalysisDurableCompletion,
        cx: &mut App,
    ) -> Result<AnalysisDurableReceipt, ReverseAnalysisResultError> {
        let finding = completion.ticket().finding;
        let key = ObjectRef::Finding(finding).address();
        let mut results = lock_unpoison(&self.analysis_results);
        let controller = results
            .get_mut(&key)
            .ok_or(ReverseAnalysisResultError::UnknownFinding(finding))?;
        let receipt = controller.complete(completion)?;
        drop(results);
        self.refresh_matching_finding(finding, cx);
        Ok(receipt)
    }

    pub fn cancel_analysis_result(&self, ticket: AnalysisActionTicket, cx: &mut App) -> bool {
        let key = ObjectRef::Finding(ticket.finding).address();
        let cancelled = lock_unpoison(&self.analysis_results)
            .get_mut(&key)
            .is_some_and(|controller| controller.cancel(ticket));
        if cancelled {
            self.refresh_matching_finding(ticket.finding, cx);
        }
        cancelled
    }

    /// Explicit invalidation boundary for analysis reruns/dismissal. Removal
    /// never happens as a side effect of pane close because duplicate panes
    /// share this result and its receipts.
    pub fn invalidate_analysis_result(
        &self,
        finding: crate::project_controller::FindingRef,
        cx: &mut App,
    ) -> Result<bool, ReverseAnalysisResultError> {
        let key = ObjectRef::Finding(finding).address();
        let mut results = lock_unpoison(&self.analysis_results);
        if let Some(ticket) = results
            .get(&key)
            .and_then(AnalysisResultController::pending_ticket)
        {
            return Err(ReverseAnalysisResultError::Lifecycle(
                AnalysisLifecycleError::ActionPending(ticket),
            ));
        }
        let removed = results.remove(&key).is_some();
        drop(results);
        if removed {
            self.refresh_matching_finding(finding, cx);
        }
        Ok(removed)
    }

    pub fn create_pane(
        &self,
        descriptor: &WorkspaceViewDescriptor,
        cx: &mut App,
    ) -> Result<PaneRegistration, SharedString> {
        let model = {
            let store = lock_unpoison(&self.store);
            ReverseSurfacePaneModel::reopen(descriptor, &store)
                .map_err(|error| SharedString::from(error.to_string()))?
        };
        let controller = self.replace_controller(descriptor.id)?;
        let title = model_title(&model.snapshot(), descriptor);
        let view = cx.new(|cx| {
            ReverseSurfaceView::new_with_analysis(
                descriptor.clone(),
                model,
                Arc::clone(&self.store),
                controller,
                Arc::clone(&self.callback),
                Arc::clone(&self.analysis_results),
                Arc::clone(&self.analysis_callback),
                cx,
            )
        });
        self.views
            .borrow_mut()
            .insert(descriptor.id, view.downgrade());
        Ok(PaneRegistration::entity(title, view))
    }

    /// Returns false when the view was already released by the workspace.
    pub fn deliver(
        &self,
        recipient: WorkspaceViewId,
        payload: PaneSessionPayload,
        cx: &mut App,
    ) -> bool {
        let Some(view) = self.live_view(recipient) else {
            return false;
        };
        let _ = view.update(cx, |view, cx| view.observe_delivery(&payload, cx));
        true
    }

    /// Convenience initialization for hosts which already hold a coherent
    /// pane-session snapshot rather than its payload wrapper.
    pub fn initialize(
        &self,
        recipient: WorkspaceViewId,
        snapshot: PaneSessionSnapshot,
        cx: &mut App,
    ) -> bool {
        self.deliver(recipient, PaneSessionPayload::FullState(snapshot), cx)
    }

    pub fn controller(&self, view: WorkspaceViewId) -> Option<SharedComparisonController> {
        self.controllers.borrow().get(&view).map(Arc::clone)
    }

    /// Remove one runtime pane and return its controller so the host can stop
    /// that exact scoped-audition owner before releasing the entity.
    pub fn release(&self, view: WorkspaceViewId) -> Option<SharedComparisonController> {
        self.views.borrow_mut().remove(&view);
        self.controllers.borrow_mut().remove(&view)
    }

    pub fn refresh_controller(&self, view: WorkspaceViewId, cx: &mut App) -> bool {
        let Some(entity) = self.live_view(view) else {
            return false;
        };
        let _ = entity.update(cx, |view, cx| view.refresh_controller(cx));
        true
    }

    /// Publish semantic content, then hydrate all matching descriptors in
    /// place. Existing non-equal content remains a typed store conflict.
    pub fn insert_document(
        &self,
        document: ReverseSurfaceDocument,
        cx: &mut App,
    ) -> Result<Arc<ReverseSurfaceDocument>, ReverseSurfaceError> {
        let object = document.object.clone();
        let retained = lock_unpoison(&self.store).insert(document)?;
        let views = self
            .views
            .borrow()
            .values()
            .filter_map(WeakEntity::upgrade)
            .collect::<Vec<_>>();
        for view in views {
            if view.read(cx).object().as_ref() == Some(&object) {
                let _ = view.update(cx, |view, cx| view.refresh_document(cx));
            }
        }
        Ok(retained)
    }

    /// Project import boundary: atomically replace every hydrated semantic
    /// document, then retarget live panes without changing their workspace
    /// identities. A pane whose object is absent becomes explicitly Missing.
    pub fn replace_documents(
        &self,
        documents: impl IntoIterator<Item = ReverseSurfaceDocument>,
        cx: &mut App,
    ) -> Result<(), ReverseSurfaceError> {
        let mut replacement = ReverseSurfaceStore::new();
        for document in documents {
            replacement.insert(document)?;
        }
        *lock_unpoison(&self.store) = replacement;
        // Same-session hydration is allowed to add explanations/comparisons
        // after a temporary Finding controller was registered.  Preserve the
        // controller here; its document/publication/source pins still reject
        // stale actions.  Project close/open calls `clear_documents`, which is
        // the explicit identity boundary that discards all result state.
        self.refresh_all_documents(cx);
        Ok(())
    }

    /// Clear project-qualified reverse content during close/import. Pane
    /// entities survive and display Missing until the next hydration wave.
    pub fn clear_documents(&self, cx: &mut App) {
        lock_unpoison(&self.store).clear();
        lock_unpoison(&self.analysis_results).clear();
        self.refresh_all_documents(cx);
    }

    pub fn remove_released(&self) {
        self.views
            .borrow_mut()
            .retain(|_, entity| entity.upgrade().is_some());
        self.controllers
            .borrow_mut()
            .retain(|view, _| self.views.borrow().contains_key(view));
    }

    fn refresh_all_documents(&self, cx: &mut App) {
        let views = self
            .views
            .borrow()
            .values()
            .filter_map(WeakEntity::upgrade)
            .collect::<Vec<_>>();
        for view in views {
            let _ = view.update(cx, |view, cx| view.refresh_document(cx));
        }
        self.remove_released();
    }

    fn refresh_matching_finding(
        &self,
        finding: crate::project_controller::FindingRef,
        cx: &mut App,
    ) {
        let object = ObjectRef::Finding(finding);
        let views = self
            .views
            .borrow()
            .values()
            .filter_map(WeakEntity::upgrade)
            .collect::<Vec<_>>();
        for view in views {
            if view.read(cx).object().as_ref() == Some(&object) {
                let _ = view.update(cx, |_, cx| cx.notify());
            }
        }
        self.remove_released();
    }

    fn notify_all_views(&self, cx: &mut App) {
        let views = self
            .views
            .borrow()
            .values()
            .filter_map(WeakEntity::upgrade)
            .collect::<Vec<_>>();
        for view in views {
            let _ = view.update(cx, |_, cx| cx.notify());
        }
        self.remove_released();
    }

    fn live_view(&self, view: WorkspaceViewId) -> Option<gpui::Entity<ReverseSurfaceView>> {
        let entity = self.views.borrow().get(&view)?.upgrade();
        if entity.is_none() {
            self.views.borrow_mut().remove(&view);
            self.controllers.borrow_mut().remove(&view);
        }
        entity
    }

    fn replace_controller(
        &self,
        view: WorkspaceViewId,
    ) -> Result<SharedComparisonController, SharedString> {
        let controller = Arc::new(Mutex::new(
            ComparisonController::new(view.0)
                .map_err(|error| SharedString::from(error.to_string()))?,
        ));
        self.controllers
            .borrow_mut()
            .insert(view, Arc::clone(&controller));
        Ok(controller)
    }
}

/// One live reverse pane. Its only owned mutable state is presentation state
/// plus the UI-neutral pane model; project/audio truth stays in ProjectSession.
pub struct ReverseSurfaceView {
    descriptor: WorkspaceViewDescriptor,
    model: ReverseSurfacePaneModel,
    store: SharedReverseSurfaceStore,
    controller: SharedComparisonController,
    callback: ReverseSurfaceViewCallback,
    analysis_results: SharedAnalysisResults,
    analysis_callback: SharedAnalysisResultCallback,
    focus_handle: FocusHandle,
    feedback: Option<(bool, String)>,
}

impl ReverseSurfaceView {
    pub fn new(
        descriptor: WorkspaceViewDescriptor,
        model: ReverseSurfacePaneModel,
        store: SharedReverseSurfaceStore,
        controller: SharedComparisonController,
        callback: ReverseSurfaceViewCallback,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_analysis(
            descriptor,
            model,
            store,
            controller,
            callback,
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(Mutex::new(None)),
            cx,
        )
    }

    fn new_with_analysis(
        descriptor: WorkspaceViewDescriptor,
        model: ReverseSurfacePaneModel,
        store: SharedReverseSurfaceStore,
        controller: SharedComparisonController,
        callback: ReverseSurfaceViewCallback,
        analysis_results: SharedAnalysisResults,
        analysis_callback: SharedAnalysisResultCallback,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            descriptor,
            model,
            store,
            controller,
            callback,
            analysis_results,
            analysis_callback,
            // The reverse entity bypasses WorkspacePaneHost, so it must expose
            // one composite keyboard stop of its own.
            focus_handle: cx.focus_handle().tab_stop(true),
            feedback: None,
        }
    }

    pub fn descriptor(&self) -> &WorkspaceViewDescriptor {
        &self.descriptor
    }

    pub fn object(&self) -> Option<ObjectRef> {
        match &self.model.snapshot().load {
            ReverseSurfaceLoad::Ready(document) => Some(document.object.clone()),
            ReverseSurfaceLoad::Missing(object) => Some(object.clone()),
            ReverseSurfaceLoad::UnsupportedTarget => None,
        }
    }

    pub fn snapshot(&self) -> ReverseSurfaceSnapshot {
        self.model.snapshot()
    }

    pub fn analysis_result_presentation(&self) -> Option<AnalysisResultPresentation> {
        let key = self.object()?.address();
        lock_unpoison(&self.analysis_results)
            .get(&key)
            .map(AnalysisResultController::presentation)
    }

    pub fn observe_delivery(&mut self, payload: &PaneSessionPayload, cx: &mut Context<Self>) {
        let mut controller = lock_unpoison(&self.controller);
        self.model.observe_delivery(payload, Some(&mut controller));
        cx.notify();
    }

    pub fn refresh_controller(&mut self, cx: &mut Context<Self>) {
        let controller = lock_unpoison(&self.controller);
        self.model.observe_controller(&controller);
        cx.notify();
    }

    pub fn refresh_document(&mut self, cx: &mut Context<Self>) {
        match crate::project_controller::object_from_descriptor(&self.descriptor) {
            Ok(Some(
                object @ (ObjectRef::Finding(_)
                | ObjectRef::Explanation(_)
                | ObjectRef::Comparison(_)
                | ObjectRef::Reading(_)),
            )) => {
                {
                    let store = lock_unpoison(&self.store);
                    self.model.retarget(object, &store);
                }
                {
                    let controller = lock_unpoison(&self.controller);
                    self.model.observe_controller(&controller);
                }
                self.feedback = None;
            }
            Ok(_) => self.feedback = Some((true, "unsupported reverse-surface target".into())),
            Err(error) => self.feedback = Some((true, error.to_string())),
        }
        cx.notify();
    }

    fn select_comparison(&mut self, comparison: ComparisonId, cx: &mut Context<Self>) {
        self.feedback = match self.model.select_comparison(comparison) {
            Ok(()) => None,
            Err(error) => Some((true, error.to_string())),
        };
        cx.notify();
    }

    fn request_channel(&mut self, channel: ComparisonChannel, cx: &mut Context<Self>) {
        let result = {
            let mut controller = lock_unpoison(&self.controller);
            self.model.request_channel(channel, &mut controller)
        };
        match result {
            Ok(intent) => {
                (self.callback)(ReverseSurfaceViewEvent::Audition {
                    view: self.descriptor.id,
                    intent,
                });
                self.feedback = Some((false, channel_feedback(channel).into()));
            }
            Err(error) => self.feedback = Some((true, error.to_string())),
        }
        cx.notify();
    }

    fn reveal_evidence(&mut self, key: &str, cx: &mut Context<Self>) {
        match self.model.reveal_evidence(key) {
            Ok(intent) => {
                (self.callback)(ReverseSurfaceViewEvent::Action {
                    view: self.descriptor.id,
                    intent,
                });
                self.feedback = Some((false, "Reveal requested".into()));
            }
            Err(error) => self.feedback = Some((true, error.to_string())),
        }
        cx.notify();
    }

    fn request_edit(&mut self, key: &str, cx: &mut Context<Self>) {
        match self.model.request_edit(key) {
            Ok(intent) => {
                (self.callback)(ReverseSurfaceViewEvent::Action {
                    view: self.descriptor.id,
                    intent,
                });
                self.feedback = Some((false, "Edit consequence requested".into()));
            }
            Err(error) => self.feedback = Some((true, error.to_string())),
        }
        cx.notify();
    }

    fn request_analysis_action(&mut self, action: AnalysisDurableAction, cx: &mut Context<Self>) {
        let result = (|| {
            let callback = lock_unpoison(&self.analysis_callback)
                .clone()
                .ok_or(ReverseAnalysisResultError::HostAuthorityUnavailable)?;
            let key = self
                .object()
                .ok_or(ReverseAnalysisResultError::HostAuthorityUnavailable)?
                .address();
            let intent = lock_unpoison(&self.analysis_results)
                .get_mut(&key)
                .ok_or_else(|| {
                    self.analysis_result_finding().map_or(
                        ReverseAnalysisResultError::HostAuthorityUnavailable,
                        |finding| ReverseAnalysisResultError::UnknownFinding(finding),
                    )
                })?
                .begin(action)?;
            callback(ReverseAnalysisResultEvent::Durable {
                view: self.descriptor.id,
                intent,
            });
            Ok::<_, ReverseAnalysisResultError>(())
        })();
        self.feedback = Some(match result {
            Ok(()) => (false, format!("{} requested", action.label())),
            Err(error) => (true, error.to_string()),
        });
        cx.notify();
    }

    fn reveal_analysis_receipt(&mut self, action: AnalysisDurableAction, cx: &mut Context<Self>) {
        let result = (|| {
            let key = self
                .object()
                .ok_or(ReverseAnalysisResultError::HostAuthorityUnavailable)?
                .address();
            let reveal = lock_unpoison(&self.analysis_results)
                .get(&key)
                .and_then(|controller| controller.receipt(action))
                .map(|receipt| receipt.reveal.clone())
                .ok_or_else(|| {
                    self.analysis_result_finding().map_or(
                        ReverseAnalysisResultError::HostAuthorityUnavailable,
                        |finding| ReverseAnalysisResultError::UnknownFinding(finding),
                    )
                })?;
            (self.callback)(ReverseSurfaceViewEvent::Action {
                view: self.descriptor.id,
                intent: SurfaceActionIntent::Reveal(reveal),
            });
            Ok::<_, ReverseAnalysisResultError>(())
        })();
        self.feedback = Some(match result {
            Ok(()) => (false, "Durable result reveal requested".into()),
            Err(error) => (true, error.to_string()),
        });
        cx.notify();
    }

    fn request_analysis_audition(&mut self, kind: PaneAudioKind, cx: &mut Context<Self>) {
        let result = (|| {
            let key = self
                .object()
                .ok_or(ReverseAnalysisResultError::HostAuthorityUnavailable)?
                .address();
            let bridge = AnalysisPaneBridge::new(self.descriptor.id)
                .map_err(|error| ReverseAnalysisResultError::AudioAuthority(error.to_string()))?;
            let (intent, has_comparison) = {
                let results = lock_unpoison(&self.analysis_results);
                let controller = results.get(&key).ok_or_else(|| {
                    self.analysis_result_finding().map_or(
                        ReverseAnalysisResultError::HostAuthorityUnavailable,
                        |finding| ReverseAnalysisResultError::UnknownFinding(finding),
                    )
                })?;
                (
                    controller.audition(bridge, kind)?,
                    controller.result().bindings.comparison.is_some(),
                )
            };
            // Full-span analysis signals are the same semantic comparison
            // channels already owned by this pane's ComparisonController.
            // Route them through that path directly so there is one render,
            // transport, cancellation, and status authority. Only finite
            // previews (family medoids/templates) cross the host callback to
            // have their exact PCM resolved.
            if let Some(channel) = has_comparison
                .then(|| analysis_comparison_channel(intent.kind()))
                .flatten()
            {
                self.request_channel(channel, cx);
            } else {
                let callback = lock_unpoison(&self.analysis_callback)
                    .clone()
                    .ok_or(ReverseAnalysisResultError::HostAuthorityUnavailable)?;
                callback(ReverseAnalysisResultEvent::Audition {
                    view: self.descriptor.id,
                    intent,
                });
            }
            Ok::<_, ReverseAnalysisResultError>(())
        })();
        self.feedback = Some(match result {
            Ok(()) => (false, "Shared audition requested".into()),
            Err(error) => (true, error.to_string()),
        });
        cx.notify();
    }

    fn analysis_result_finding(&self) -> Option<crate::project_controller::FindingRef> {
        match self.object()? {
            ObjectRef::Finding(finding) => Some(finding),
            _ => None,
        }
    }

    fn render_ready(
        &self,
        document: Arc<ReverseSurfaceDocument>,
        snapshot: &ReverseSurfaceSnapshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut content = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.render_identity(&document))
            .child(self.render_body(&document));

        if let Some(presentation) = self.analysis_result_presentation() {
            content = content.child(self.render_analysis_result(&presentation, cx));
        }

        if !document.comparisons.is_empty() {
            content = content.child(self.render_comparisons(&document, snapshot, cx));
        }
        if !document.evidence.is_empty() {
            content = content.child(self.render_evidence(&document.evidence, cx));
        }
        if !document.edit_consequences.is_empty() {
            content = content.child(self.render_consequences(&document.edit_consequences, cx));
        }
        content
    }

    fn render_analysis_result(
        &self,
        presentation: &AnalysisResultPresentation,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let authority_connected = lock_unpoison(&self.analysis_callback).is_some();
        let mut actions = div().mt_3().flex().flex_wrap().gap_2();
        for presented in &presentation.actions {
            let action = presented.action;
            actions = match &presented.state {
                AnalysisPresentedActionState::Available if authority_connected => actions.child(
                    small_button(
                        format!("analysis-action-{action:?}"),
                        presented.label,
                        false,
                        LIME,
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.request_analysis_action(action, cx)),
                    ),
                ),
                AnalysisPresentedActionState::Available => actions.child(info_row(
                    format!("analysis-host-unavailable-{action:?}"),
                    format!("{} · unavailable", presented.label),
                    "Host authority is not connected; no project edit was attempted.",
                    MUTED,
                )),
                AnalysisPresentedActionState::Pending(ticket) => actions.child(info_row(
                    format!("analysis-pending-{action:?}"),
                    format!("{} · pending", presented.label),
                    format!("generation {} · awaiting authority", ticket.generation),
                    AMBER,
                )),
                AnalysisPresentedActionState::Completed {
                    primary,
                    durable_revision,
                } => actions.child(
                    row_button(
                        format!("analysis-complete-{action:?}"),
                        format!("{} · reveal", presented.label),
                        format!("{} · revision {durable_revision}", primary.address()),
                        LIME,
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.reveal_analysis_receipt(action, cx)),
                    ),
                ),
                AnalysisPresentedActionState::Refused(reason) => actions.child(info_row(
                    format!("analysis-refused-{action:?}"),
                    format!("{} · unavailable", presented.label),
                    reason.message(),
                    MUTED,
                )),
            };
        }

        let mut auditions = div().mt_3().flex().flex_wrap().gap_2();
        for audition in &presentation.auditions {
            let kind = audition.kind;
            auditions = match audition.availability {
                AnalysisAuditionAvailability::Available(_route) if authority_connected => auditions.child(
                    small_button(
                        format!("analysis-audition-{kind:?}"),
                        audition.label,
                        false,
                        CYAN,
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.request_analysis_audition(kind, cx)),
                    ),
                ),
                AnalysisAuditionAvailability::Available(_) => auditions.child(info_row(
                    format!("analysis-audition-host-unavailable-{kind:?}"),
                    format!("{} · unavailable", audition.label),
                    "Shared audition authority is not connected; this pane owns no fallback player.",
                    MUTED,
                )),
                AnalysisAuditionAvailability::Refused(reason) => auditions.child(info_row(
                    format!("analysis-audition-refused-{kind:?}"),
                    audition.label,
                    reason.message(),
                    MUTED,
                )),
            };
        }

        section("RESULT ACTIONS")
            .child(detail(
                "LIFECYCLE",
                if presentation.temporary {
                    "Temporary · Keep finding to retain this result"
                } else {
                    "Retained · every completion has an exact reveal receipt"
                }
                .into(),
            ))
            .child(actions)
            .child(auditions)
    }

    fn render_identity(&self, document: &ReverseSurfaceDocument) -> impl IntoElement {
        div()
            .p_4()
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(CYAN))
                    .child(object_kind_label(&document.object)),
            )
            .child(
                div()
                    .mt_1()
                    .text_lg()
                    .text_color(rgb(TEXT))
                    .child(document.title.clone()),
            )
            .child(
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(rgb(DIM))
                    .child(document.object.address()),
            )
    }

    fn render_body(&self, document: &ReverseSurfaceDocument) -> impl IntoElement {
        let mut body = section("INTERPRETATION");
        match &document.body {
            ReverseSurfaceBody::Finding(finding) => {
                body = body.child(detail("KIND", format!("{:?}", finding.finding.kind)));
                if let Some(extent) = finding.extent {
                    body = body.child(detail(
                        "EXTENT",
                        format!("{}..{} frames", extent.start, extent.end),
                    ));
                }
                for statement in &finding.statements {
                    body = body.child(paragraph(statement));
                }
            }
            ReverseSurfaceBody::Explanation(explanation) => {
                body = body
                    .child(detail(
                        "SCOPE",
                        format!("{:?}", explanation.definition.scope),
                    ))
                    .child(detail(
                        "EXTENT",
                        format!("{:?}", explanation.definition.extent),
                    ))
                    .child(detail(
                        "DEPENDENT NULL TESTS",
                        explanation.dependent_comparisons.len().to_string(),
                    ));
            }
            ReverseSurfaceBody::Comparison(comparison) => {
                body = body
                    .child(detail(
                        "EXPLANATION",
                        comparison.definition.explanation.0.to_string(),
                    ))
                    .child(detail(
                        "PROJECT SPAN",
                        format!(
                            "{}..{} frames",
                            comparison.definition.source.project_span.start,
                            comparison.definition.source.project_span.end
                        ),
                    ));
                if let Some(observation) = &comparison.observation {
                    body = body.child(metric_summary(observation.metrics));
                } else {
                    body = body.child(paragraph("Awaiting an exact comparison render."));
                }
            }
            ReverseSurfaceBody::Reading(reading) => {
                let verification = match &reading.verification {
                    Ok(tier) => format!("Verified: {tier:?}"),
                    Err(refusal) => format!("Unverified: {refusal:?}"),
                };
                body = body
                    .child(detail("READING", reading.reading.reading_id.to_string()))
                    .child(detail("REVISION", reading.reading.revision.to_string()))
                    .child(detail("VERIFICATION", verification))
                    .child(detail(
                        "PORTABLE SECTIONS",
                        reading.reading.sections.len().to_string(),
                    ));
            }
        }
        body
    }

    fn render_comparisons(
        &self,
        document: &ReverseSurfaceDocument,
        snapshot: &ReverseSurfaceSnapshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut choices = div().flex().flex_wrap().gap_2();
        for comparison in &document.comparisons {
            let id = comparison.definition.id;
            let selected = snapshot.selected_comparison == Some(id);
            choices = choices.child(
                small_button(
                    format!("comparison-{}", id.0),
                    comparison.definition.label.clone(),
                    selected,
                    CYAN,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.select_comparison(id, cx))),
            );
        }
        let mut channels = div().mt_3().grid().grid_cols(4).gap_2();
        for channel in &snapshot.channels {
            let request = channel.channel;
            let enabled = !matches!(
                channel.availability,
                SurfaceChannelAvailability::AwaitingObservation | SurfaceChannelAvailability::Stale
            );
            let color = channel_color(channel.semantic);
            let mut button = channel_button(channel, color);
            if enabled {
                button = button
                    .on_click(cx.listener(move |this, _, _, cx| this.request_channel(request, cx)));
            }
            channels = channels.child(button);
        }
        section("NULL TEST")
            .child(choices)
            .child(channels)
            .child(
                div()
                    .mt_3()
                    .text_xs()
                    .text_color(rgb(DIM))
                    .child("Residual is exact sample subtraction. Excess is a spectral over-explanation map and has no PCM audition."),
            )
    }

    fn render_evidence(
        &self,
        evidence: &[SurfaceEvidence],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut rows = section("EVIDENCE");
        for item in evidence {
            let key = item.key.clone();
            let has_target = item.object.is_some();
            let extent = item
                .extent
                .map(|extent| format!(" · {}..{}", extent.start, extent.end))
                .unwrap_or_default();
            let mut row = row_button(
                format!("evidence-{}", item.key),
                item.label.clone(),
                format!("{} derivation links{extent}", item.derivation.len()),
                CYAN,
            );
            if has_target {
                row =
                    row.on_click(cx.listener(move |this, _, _, cx| this.reveal_evidence(&key, cx)));
            }
            rows = rows.child(row);
        }
        rows
    }

    fn render_consequences(
        &self,
        consequences: &[SurfaceEditConsequence],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut rows = section("EDIT CONSEQUENCES");
        for consequence in consequences {
            let key = consequence.key.clone();
            let color = match consequence.authority {
                EditAuthority::None => MUTED,
                EditAuthority::InterpretationCommand => MAGENTA,
                EditAuthority::ProjectCommand => AMBER,
                EditAuthority::ReadingFork(_) => LIME,
            };
            let detail = format!(
                "{:?} · invalidates {} · retains {}",
                consequence.authority,
                consequence.invalidates.len(),
                consequence.retains_evidence.len()
            );
            rows = rows.child(
                row_button(
                    format!("consequence-{}", consequence.key),
                    consequence.label.clone(),
                    detail,
                    color,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.request_edit(&key, cx))),
            );
        }
        rows
    }
}

fn analysis_comparison_channel(kind: PaneAudioKind) -> Option<ComparisonChannel> {
    match kind {
        PaneAudioKind::HpssSource | PaneAudioKind::LoomSource => Some(ComparisonChannel::Source),
        PaneAudioKind::HpssHarmonic
        | PaneAudioKind::HpssTransient
        | PaneAudioKind::LoomConstruction
        | PaneAudioKind::RhythmConstruction => Some(ComparisonChannel::Construction),
        PaneAudioKind::HpssResidual | PaneAudioKind::LoomResidual => {
            Some(ComparisonChannel::Residual)
        }
        PaneAudioKind::ComparisonSource => Some(ComparisonChannel::Source),
        PaneAudioKind::ComparisonConstruction => Some(ComparisonChannel::Construction),
        PaneAudioKind::ComparisonResidual => Some(ComparisonChannel::Residual),
        PaneAudioKind::ComponentMagnitudeHypothesis
        | PaneAudioKind::RhythmFamilyMedoid
        | PaneAudioKind::LoomTemplate
        | PaneAudioKind::AssetOneShot
        | PaneAudioKind::PadGate => None,
    }
}

impl Focusable for ReverseSurfaceView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ReverseSurfaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.model.snapshot();
        let playhead = snapshot.audio.transport.frame.0;
        let selection = snapshot.selection.time.map_or_else(
            || "no time selection".into(),
            |span| format!("selection {}..{}", span.start, span.end),
        );
        let transport = format!(
            "{:?} · frame {playhead} · {selection} · {:?} · project {}",
            snapshot.audio.transport.mode,
            snapshot.signal,
            snapshot.publication.map_or_else(
                || "unpublished".into(),
                |value| value.revisions.aggregate.to_string()
            )
        );
        let content = match snapshot.load.clone() {
            ReverseSurfaceLoad::Ready(document) => {
                self.render_ready(document, &snapshot, cx).into_any_element()
            }
            ReverseSurfaceLoad::Missing(object) => honest_state(
                "CONTENT NOT HYDRATED",
                format!(
                    "The workspace target `{}` is valid, but its interpretation document is not loaded. The pane will hydrate in place when that content arrives.",
                    object.address()
                ),
                AMBER,
            )
            .into_any_element(),
            ReverseSurfaceLoad::UnsupportedTarget => honest_state(
                "UNSUPPORTED TARGET",
                "This descriptor does not identify a Finding, Explanation, Comparison, or Reading.",
                MAGENTA,
            )
            .into_any_element(),
        };

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
                    .h(px(38.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL_ALT))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(transport))
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
                    .id("reverse-surface-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .child(content),
            )
    }
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn model_title(
    snapshot: &ReverseSurfaceSnapshot,
    descriptor: &WorkspaceViewDescriptor,
) -> SharedString {
    if let Some(title) = &descriptor.title_override {
        return title.clone().into();
    }
    match &snapshot.load {
        ReverseSurfaceLoad::Ready(document) => document.title.clone().into(),
        ReverseSurfaceLoad::Missing(object) => object_kind_label(object).into(),
        ReverseSurfaceLoad::UnsupportedTarget => "Reverse surface".into(),
    }
}

fn object_kind_label(object: &ObjectRef) -> &'static str {
    match object {
        ObjectRef::Finding(_) => "FINDING",
        ObjectRef::Explanation(_) => "EXPLANATION",
        ObjectRef::Comparison(_) => "COMPARISON",
        ObjectRef::Reading(_) => "READING",
        _ => "REVERSE OBJECT",
    }
}

fn channel_feedback(channel: ComparisonChannel) -> &'static str {
    match channel {
        ComparisonChannel::Source => "Source audition requested",
        ComparisonChannel::Construction => "Construction audition requested",
        ComparisonChannel::Residual => "Exact residual audition requested",
        ComparisonChannel::Excess => "Spectral excess inspection requested",
    }
}

fn channel_color(semantic: SurfaceChannelSemantic) -> u32 {
    match semantic {
        SurfaceChannelSemantic::Source => CYAN,
        SurfaceChannelSemantic::Construction => LIME,
        SurfaceChannelSemantic::ExactResidual => MAGENTA,
        SurfaceChannelSemantic::SpectralExcess => AMBER,
    }
}

fn section(label: &'static str) -> gpui::Div {
    div()
        .p_4()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .child(div().mb_3().text_xs().text_color(rgb(MUTED)).child(label))
}

fn detail(label: &'static str, value: String) -> impl IntoElement {
    div()
        .mt_2()
        .flex()
        .items_start()
        .justify_between()
        .gap_4()
        .child(div().text_xs().text_color(rgb(DIM)).child(label))
        .child(div().text_sm().text_color(rgb(TEXT)).child(value))
}

fn paragraph(value: impl Into<SharedString>) -> impl IntoElement {
    div()
        .mt_2()
        .text_sm()
        .text_color(rgb(TEXT))
        .child(value.into())
}

fn metric_summary(metrics: ComparisonMetrics) -> impl IntoElement {
    div()
        .mt_3()
        .grid()
        .grid_cols(3)
        .gap_2()
        .child(metric("SOURCE ENERGY", metrics.source_energy, CYAN))
        .child(metric(
            "CONSTRUCTION ENERGY",
            metrics.construction_energy,
            LIME,
        ))
        .child(metric("RESIDUAL ENERGY", metrics.residual_energy, MAGENTA))
}

fn metric(label: &'static str, value: f64, color: u32) -> impl IntoElement {
    div()
        .p_2()
        .rounded_md()
        .bg(rgb(RAISED))
        .child(div().text_xs().text_color(rgb(DIM)).child(label))
        .child(
            div()
                .mt_1()
                .text_sm()
                .text_color(rgb(color))
                .child(format!("{value:.5}")),
        )
}

fn small_button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    selected: bool,
    color: u32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(if selected { color } else { BORDER }))
        .bg(if selected {
            rgba(0x50d8d722)
        } else {
            rgba(0x00000000)
        })
        .text_xs()
        .text_color(rgb(if selected { color } else { MUTED }))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(RAISED)).text_color(rgb(TEXT)))
        .child(label.into())
}

fn channel_button(channel: &SurfaceChannelState, color: u32) -> gpui::Stateful<gpui::Div> {
    let measurement = match channel.measurement {
        Some(SurfaceChannelMeasurement::SampleEnergy(value)) => format!("energy {value:.5}"),
        Some(SurfaceChannelMeasurement::SpectralExcess { ratio, .. }) => {
            format!("excess {:.1}%", ratio * 100.0)
        }
        None => match channel.availability {
            SurfaceChannelAvailability::AwaitingObservation => "awaiting render".into(),
            SurfaceChannelAvailability::Auditionable => "ready".into(),
            SurfaceChannelAvailability::CoverageOnly => "map only".into(),
            SurfaceChannelAvailability::Stale => "stale".into(),
        },
    };
    let label = match channel.semantic {
        SurfaceChannelSemantic::Source => "SOURCE",
        SurfaceChannelSemantic::Construction => "CONSTRUCTION",
        SurfaceChannelSemantic::ExactResidual => "RESIDUAL",
        SurfaceChannelSemantic::SpectralExcess => "EXCESS",
    };
    div()
        .id(SharedString::from(format!("channel-{label}")))
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(if channel.selected { color } else { BORDER }))
        .bg(if channel.active {
            rgba(0x50d8d722)
        } else {
            rgba(0x00000000)
        })
        .cursor_pointer()
        .hover(|style| style.bg(rgb(RAISED)))
        .child(div().text_xs().text_color(rgb(color)).child(label))
        .child(
            div()
                .mt_1()
                .text_xs()
                .text_color(rgb(DIM))
                .child(measurement),
        )
}

fn row_button(
    id: impl Into<SharedString>,
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
    color: u32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .mt_2()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(RAISED)))
        .child(div().text_sm().text_color(rgb(color)).child(title.into()))
        .child(
            div()
                .mt_1()
                .text_xs()
                .text_color(rgb(DIM))
                .child(subtitle.into()),
        )
}

fn info_row(
    id: impl Into<SharedString>,
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
    color: u32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .mt_2()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .child(div().text_sm().text_color(rgb(color)).child(title.into()))
        .child(
            div()
                .mt_1()
                .text_xs()
                .text_color(rgb(DIM))
                .child(subtitle.into()),
        )
}

fn honest_state(
    title: &'static str,
    message: impl Into<SharedString>,
    color: u32,
) -> impl IntoElement {
    div()
        .p_5()
        .rounded_md()
        .border_1()
        .border_color(rgb(color))
        .bg(rgb(PANEL))
        .child(div().text_sm().text_color(rgb(color)).child(title))
        .child(
            div()
                .mt_2()
                .text_sm()
                .text_color(rgb(MUTED))
                .child(message.into()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analysis_timeline_signals_share_comparison_channels() {
        for kind in [PaneAudioKind::HpssSource, PaneAudioKind::LoomSource] {
            assert_eq!(
                analysis_comparison_channel(kind),
                Some(ComparisonChannel::Source)
            );
        }
        for kind in [
            PaneAudioKind::HpssHarmonic,
            PaneAudioKind::HpssTransient,
            PaneAudioKind::LoomConstruction,
            PaneAudioKind::RhythmConstruction,
        ] {
            assert_eq!(
                analysis_comparison_channel(kind),
                Some(ComparisonChannel::Construction)
            );
        }
        for kind in [PaneAudioKind::HpssResidual, PaneAudioKind::LoomResidual] {
            assert_eq!(
                analysis_comparison_channel(kind),
                Some(ComparisonChannel::Residual)
            );
        }
        for kind in [
            PaneAudioKind::RhythmFamilyMedoid,
            PaneAudioKind::LoomTemplate,
            PaneAudioKind::ComponentMagnitudeHypothesis,
        ] {
            assert_eq!(analysis_comparison_channel(kind), None);
        }
    }
}
