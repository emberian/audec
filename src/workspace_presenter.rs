//! Typed presenter selection for dynamic workspace descriptors.
//!
//! The durable workspace document says *what* a pane means; GPUI owns only
//! the entity which presents it.  This module is the narrow bridge between
//! those layers.  It deliberately refuses to turn an unsupported semantic
//! lens into a visually similar analyzer: coverage, comparison, and AIR query
//! panes either reach their real presenter or produce a typed diagnostic.
//!
//! Project/session lookup stays outside the factory.  The host resolves the
//! returned deprojection target against one coherent
//! [`ProjectSession`](crate::project_session::ProjectSession)
//! snapshot, then supplies the resulting pinned request to
//! [`ExplanationWorkbenchViewFactory`].

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gpui::{App, AppContext, Entity, SharedString, WeakEntity};
use serde_json::Value;

use crate::air_query::workbench::{WORKBENCH_NAMESPACE, WORKBENCH_VIEW_NAME};
use crate::comparison::ComparisonId;
use crate::comparison_controller::ComparisonController;
use crate::explanation::ExplanationId;
use crate::explanation_workbench_view::{
    ExplanationWorkbenchCallback, ExplanationWorkbenchEvent, ExplanationWorkbenchPaneModel,
    ExplanationWorkbenchView,
};
use crate::project_controller::{object_from_descriptor, ObjectRef};
use crate::project_session::deprojection_workspace_bridge::{
    DeprojectionWorkspaceTarget, ResolvedDeprojectionWorkspaceRequest,
};
use crate::workspace_document::{
    AnalysisLensKind, EditorTarget, EditorViewState, FrameViewport, LinkFacets, LinkGroupId,
    NewWorkspaceView, ViewLinkMembership, WorkspaceItemKind, WorkspaceViewDescriptor,
    WorkspaceViewId,
};
use crate::workspace_ui::PaneRegistration;

pub const AUDEC_WORKSPACE_NAMESPACE: &str = "audec";
pub const EXPLANATION_WORKBENCH_VIEW_NAME: &str = "explanation";

/// Portable descriptor source for opening one promotable explanation.  The
/// target is encoded in the typed extension key understood by object
/// navigation; the runtime artifact payload remains session-owned.
pub fn new_explanation_workbench_view(explanation: ExplanationId) -> NewWorkspaceView {
    NewWorkspaceView {
        kind: WorkspaceItemKind::Extension {
            namespace: AUDEC_WORKSPACE_NAMESPACE.into(),
            name: EXPLANATION_WORKBENCH_VIEW_NAME.into(),
        },
        target: EditorTarget::Extension {
            namespace: AUDEC_WORKSPACE_NAMESPACE.into(),
            key: format!("explanation:{}", explanation.0),
        },
        title_override: None,
        links: unlinked(),
        state: EditorViewState::Extension { data: Value::Null },
        extensions: BTreeMap::new(),
    }
}

/// Portable descriptor source for the explained/residual/excess coverage of
/// one persistent comparison.
pub fn new_coverage_view(comparison: ComparisonId, viewport: FrameViewport) -> NewWorkspaceView {
    new_comparison_lens(AnalysisLensKind::Coverage, comparison, viewport)
}

/// Portable descriptor source for the aligned source/construction/residual
/// channels of one persistent comparison.
pub fn new_comparison_view(comparison: ComparisonId, viewport: FrameViewport) -> NewWorkspaceView {
    new_comparison_lens(AnalysisLensKind::Comparison, comparison, viewport)
}

fn new_comparison_lens(
    lens: AnalysisLensKind,
    comparison: ComparisonId,
    viewport: FrameViewport,
) -> NewWorkspaceView {
    debug_assert!(matches!(
        lens,
        AnalysisLensKind::Coverage | AnalysisLensKind::Comparison
    ));
    NewWorkspaceView {
        kind: WorkspaceItemKind::AnalysisLens { lens },
        target: EditorTarget::Render {
            comparison_id: Some(comparison.0),
        },
        title_override: None,
        links: unlinked(),
        state: EditorViewState::Analysis {
            viewport,
            follow: false,
            min_frequency_hz: None,
            max_frequency_hz: None,
            recipe_fingerprint: None,
        },
        extensions: BTreeMap::new(),
    }
}

fn unlinked() -> ViewLinkMembership {
    ViewLinkMembership {
        group: LinkGroupId::UNLINKED,
        facets: LinkFacets::NONE,
    }
}

/// Which part of the unified explain/render/null-test surface a descriptor
/// asks the presenter to foreground.  It is presentation state, not a claim
/// that the underlying candidate or comparison exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExplanationWorkbenchFocus {
    Explanation,
    Coverage,
    Comparison,
}

/// A validated, project-independent request for the explanation workbench.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExplanationWorkbenchRoute {
    pub view: WorkspaceViewId,
    pub focus: ExplanationWorkbenchFocus,
    pub object: ObjectRef,
    pub title_override: Option<String>,
}

impl ExplanationWorkbenchRoute {
    pub fn deprojection_target(&self) -> DeprojectionWorkspaceTarget {
        DeprojectionWorkspaceTarget::Object(self.object.clone())
    }

    fn accepts_resolution(&self, resolved: &ResolvedDeprojectionWorkspaceRequest) -> bool {
        match &self.object {
            ObjectRef::Explanation(id) => resolved.request.target.explanation == *id,
            ObjectRef::Comparison(id) => resolved.request.target.comparison == *id,
            _ => false,
        }
    }
}

/// Specialized dynamic presenters which must run before generic reverse-pane
/// or legacy visualizer dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpecializedWorkspacePresenter {
    ExplanationWorkbench(ExplanationWorkbenchRoute),
    ReadingQuery,
}

/// Classify one durable descriptor without touching project state or GPUI.
///
/// `Ok(None)` means the ordinary workspace factory remains authoritative.
/// A specialized-looking descriptor that cannot name its required semantic
/// object is an error; silently opening Components would lie about the pane.
pub fn resolve_specialized_presenter(
    descriptor: &WorkspaceViewDescriptor,
) -> Result<Option<SpecializedWorkspacePresenter>, WorkspacePresenterError> {
    if is_reading_query(descriptor) {
        return Ok(Some(SpecializedWorkspacePresenter::ReadingQuery));
    }

    let focus = match &descriptor.kind {
        WorkspaceItemKind::Extension { namespace, name }
            if namespace == AUDEC_WORKSPACE_NAMESPACE
                && name == EXPLANATION_WORKBENCH_VIEW_NAME =>
        {
            Some(ExplanationWorkbenchFocus::Explanation)
        }
        WorkspaceItemKind::AnalysisLens {
            lens: AnalysisLensKind::Coverage,
        } => Some(ExplanationWorkbenchFocus::Coverage),
        WorkspaceItemKind::AnalysisLens {
            lens: AnalysisLensKind::Comparison,
        }
        | WorkspaceItemKind::Render => Some(ExplanationWorkbenchFocus::Comparison),
        WorkspaceItemKind::AnalysisLens {
            lens: AnalysisLensKind::AirQuery,
        } => return Err(WorkspacePresenterError::AirQueryAlias(descriptor.id)),
        _ => None,
    };
    let Some(focus) = focus else {
        return Ok(None);
    };

    let object = object_from_descriptor(descriptor)
        .map_err(|error| WorkspacePresenterError::ObjectAddress(error.to_string()))?
        .ok_or(WorkspacePresenterError::MissingSemanticTarget {
            view: descriptor.id,
            focus,
        })?;
    if !matches!(object, ObjectRef::Explanation(_) | ObjectRef::Comparison(_)) {
        return Err(WorkspacePresenterError::WrongSemanticTarget {
            view: descriptor.id,
            focus,
            object,
        });
    }

    Ok(Some(SpecializedWorkspacePresenter::ExplanationWorkbench(
        ExplanationWorkbenchRoute {
            view: descriptor.id,
            focus,
            object,
            title_override: descriptor.title_override.clone(),
        },
    )))
}

fn is_reading_query(descriptor: &WorkspaceViewDescriptor) -> bool {
    matches!(
        &descriptor.kind,
        WorkspaceItemKind::Extension { namespace, name }
            if namespace == WORKBENCH_NAMESPACE && name == WORKBENCH_VIEW_NAME
    )
}

/// Descriptor-aware GPUI factory for the pinned explanation workbench.
///
/// The host owns asynchronous execution.  It can retrieve the live entity by
/// durable view ID and apply typed plan/promotion/render/comparison completions
/// to `model_mut()`, exactly as it already does for other workspace panes.
#[derive(Clone)]
pub struct ExplanationWorkbenchViewFactory {
    callback: WorkspaceExplanationWorkbenchCallback,
    views: Rc<RefCell<BTreeMap<WorkspaceViewId, WeakEntity<ExplanationWorkbenchView>>>>,
    controllers: Rc<RefCell<BTreeMap<WorkspaceViewId, Arc<Mutex<ComparisonController>>>>>,
}

pub type WorkspaceExplanationWorkbenchCallback =
    Arc<dyn Fn(WorkspaceViewId, ExplanationWorkbenchEvent) + Send + Sync + 'static>;

impl ExplanationWorkbenchViewFactory {
    pub fn new(callback: WorkspaceExplanationWorkbenchCallback) -> Self {
        Self {
            callback,
            views: Rc::new(RefCell::new(BTreeMap::new())),
            controllers: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }

    pub fn create_pane(
        &self,
        route: &ExplanationWorkbenchRoute,
        resolved: ResolvedDeprojectionWorkspaceRequest,
        cx: &mut App,
    ) -> Result<PaneRegistration, SharedString> {
        if !route.accepts_resolution(&resolved) {
            return Err(SharedString::from(
                WorkspacePresenterError::ResolvedObjectMismatch {
                    view: route.view,
                    expected: route.object.clone(),
                    explanation: resolved.request.target.explanation,
                    comparison: resolved.request.target.comparison,
                }
                .to_string(),
            ));
        }
        let model = ExplanationWorkbenchPaneModel::new(
            resolved.descriptor,
            Arc::clone(&resolved.payload),
            resolved.request,
        )
        .map_err(|error| SharedString::from(error.to_string()))?;
        let callback = Arc::clone(&self.callback);
        let view = route.view;
        let callback: ExplanationWorkbenchCallback = Arc::new(move |event| callback(view, event));
        let entity = cx.new(|cx| ExplanationWorkbenchView::new(model, callback, cx));
        let controller = ComparisonController::new(route.view.0)
            .map_err(|error| SharedString::from(error.to_string()))?;
        self.views
            .borrow_mut()
            .insert(route.view, entity.downgrade());
        self.controllers
            .borrow_mut()
            .insert(route.view, Arc::new(Mutex::new(controller)));
        let title = route
            .title_override
            .clone()
            .unwrap_or_else(|| match route.focus {
                ExplanationWorkbenchFocus::Explanation => "Explanation workbench".into(),
                ExplanationWorkbenchFocus::Coverage => "Explanation coverage".into(),
                ExplanationWorkbenchFocus::Comparison => "Source / construction / residual".into(),
            });
        Ok(PaneRegistration::entity(title, entity))
    }

    pub fn entity(&self, view: WorkspaceViewId) -> Option<Entity<ExplanationWorkbenchView>> {
        let entity = {
            self.views
                .borrow()
                .get(&view)
                .and_then(|entity| entity.upgrade())
        };
        if entity.is_none() {
            self.views.borrow_mut().remove(&view);
        }
        entity
    }

    pub fn release(&self, view: WorkspaceViewId) {
        self.views.borrow_mut().remove(&view);
        self.controllers.borrow_mut().remove(&view);
    }

    pub fn controller(&self, view: WorkspaceViewId) -> Option<Arc<Mutex<ComparisonController>>> {
        self.controllers.borrow().get(&view).map(Arc::clone)
    }

    pub fn remove_released(&self) {
        self.views
            .borrow_mut()
            .retain(|_, entity| entity.upgrade().is_some());
        self.controllers
            .borrow_mut()
            .retain(|view, _| self.views.borrow().contains_key(view));
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspacePresenterError {
    AirQueryAlias(WorkspaceViewId),
    MissingSemanticTarget {
        view: WorkspaceViewId,
        focus: ExplanationWorkbenchFocus,
    },
    WrongSemanticTarget {
        view: WorkspaceViewId,
        focus: ExplanationWorkbenchFocus,
        object: ObjectRef,
    },
    ObjectAddress(String),
    ResolvedObjectMismatch {
        view: WorkspaceViewId,
        expected: ObjectRef,
        explanation: ExplanationId,
        comparison: ComparisonId,
    },
}

impl fmt::Display for WorkspacePresenterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AirQueryAlias(view) => write!(
                formatter,
                "workspace view {} uses the retired analysis-lens AIR-query alias; use the reading/query workbench descriptor",
                view.0
            ),
            Self::MissingSemanticTarget { view, focus } => write!(
                formatter,
                "workspace view {} requests {focus:?} without an explanation or comparison target",
                view.0
            ),
            Self::WrongSemanticTarget {
                view,
                focus,
                object,
            } => write!(
                formatter,
                "workspace view {} requests {focus:?} for incompatible object {object:?}",
                view.0
            ),
            Self::ObjectAddress(error) => formatter.write_str(error),
            Self::ResolvedObjectMismatch {
                view,
                expected,
                explanation,
                comparison,
            } => write!(
                formatter,
                "workspace view {} expected {expected:?}, but the resolved candidate names explanation {} and comparison {}",
                view.0, explanation.0, comparison.0
            ),
        }
    }
}

impl std::error::Error for WorkspacePresenterError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_document::WorkspaceDocument;

    const NAVIGATION_OBJECT: &str = "audec.navigation_object";

    fn descriptor(
        id: u64,
        kind: WorkspaceItemKind,
        target: EditorTarget,
        state: EditorViewState,
        object: Option<&str>,
    ) -> WorkspaceViewDescriptor {
        let mut extensions = BTreeMap::new();
        if let Some(object) = object {
            extensions.insert(NAVIGATION_OBJECT.into(), Value::String(object.into()));
        }
        WorkspaceViewDescriptor {
            id: WorkspaceViewId(id),
            kind,
            target,
            title_override: None,
            links: ViewLinkMembership {
                group: LinkGroupId::UNLINKED,
                facets: LinkFacets::NONE,
            },
            state,
            extensions,
        }
    }

    fn analysis_state() -> EditorViewState {
        EditorViewState::Analysis {
            viewport: FrameViewport { start: 0, end: 1 },
            follow: false,
            min_frequency_hz: None,
            max_frequency_hz: None,
            recipe_fingerprint: None,
        }
    }

    #[test]
    fn explanation_extension_reaches_the_real_workbench() {
        let descriptor = descriptor(
            20,
            WorkspaceItemKind::Extension {
                namespace: "audec".into(),
                name: "explanation".into(),
            },
            EditorTarget::Extension {
                namespace: "audec".into(),
                key: "explanation:7".into(),
            },
            EditorViewState::Extension { data: Value::Null },
            Some("explanation:7"),
        );
        let Some(SpecializedWorkspacePresenter::ExplanationWorkbench(route)) =
            resolve_specialized_presenter(&descriptor).unwrap()
        else {
            panic!("explanation descriptor did not reach the workbench")
        };
        assert_eq!(route.focus, ExplanationWorkbenchFocus::Explanation);
        assert_eq!(route.object, ObjectRef::Explanation(ExplanationId(7)));
    }

    #[test]
    fn coverage_and_comparison_are_distinct_workbench_routes() {
        for (lens, focus) in [
            (
                AnalysisLensKind::Coverage,
                ExplanationWorkbenchFocus::Coverage,
            ),
            (
                AnalysisLensKind::Comparison,
                ExplanationWorkbenchFocus::Comparison,
            ),
        ] {
            let descriptor = descriptor(
                30 + lens as u64,
                WorkspaceItemKind::AnalysisLens { lens },
                EditorTarget::Render {
                    comparison_id: Some(11),
                },
                analysis_state(),
                None,
            );
            descriptor.validate().unwrap();
            let Some(SpecializedWorkspacePresenter::ExplanationWorkbench(route)) =
                resolve_specialized_presenter(&descriptor).unwrap()
            else {
                panic!("specialized lens fell through to a generic visualizer")
            };
            assert_eq!(route.focus, focus);
            assert_eq!(route.object, ObjectRef::Comparison(ComparisonId(11)));
        }
    }

    #[test]
    fn analysis_air_query_alias_is_refused_instead_of_showing_components() {
        let descriptor = descriptor(
            40,
            WorkspaceItemKind::AnalysisLens {
                lens: AnalysisLensKind::AirQuery,
            },
            EditorTarget::Analysis { source_id: None },
            analysis_state(),
            None,
        );
        assert_eq!(
            resolve_specialized_presenter(&descriptor),
            Err(WorkspacePresenterError::AirQueryAlias(WorkspaceViewId(40)))
        );
    }

    #[test]
    fn reading_query_extension_reaches_its_own_factory() {
        let descriptor = descriptor(
            41,
            WorkspaceItemKind::Extension {
                namespace: WORKBENCH_NAMESPACE.into(),
                name: WORKBENCH_VIEW_NAME.into(),
            },
            EditorTarget::Extension {
                namespace: WORKBENCH_NAMESPACE.into(),
                key: "query:1".into(),
            },
            EditorViewState::Extension { data: Value::Null },
            None,
        );
        assert_eq!(
            resolve_specialized_presenter(&descriptor).unwrap(),
            Some(SpecializedWorkspacePresenter::ReadingQuery)
        );
    }

    #[test]
    fn ordinary_analyzers_remain_owned_by_the_existing_factory() {
        let descriptor = descriptor(
            42,
            WorkspaceItemKind::AnalysisLens {
                lens: AnalysisLensKind::Rhythm,
            },
            EditorTarget::Analysis { source_id: None },
            analysis_state(),
            None,
        );
        assert_eq!(resolve_specialized_presenter(&descriptor).unwrap(), None);
    }

    #[test]
    fn portable_constructors_survive_document_allocation_and_resolve_honestly() {
        let mut document = WorkspaceDocument::default();
        for (view, focus, object) in [
            (
                new_explanation_workbench_view(ExplanationId(13)),
                ExplanationWorkbenchFocus::Explanation,
                ObjectRef::Explanation(ExplanationId(13)),
            ),
            (
                new_coverage_view(ComparisonId(14), FrameViewport { start: 2, end: 9 }),
                ExplanationWorkbenchFocus::Coverage,
                ObjectRef::Comparison(ComparisonId(14)),
            ),
            (
                new_comparison_view(ComparisonId(15), FrameViewport { start: 2, end: 9 }),
                ExplanationWorkbenchFocus::Comparison,
                ObjectRef::Comparison(ComparisonId(15)),
            ),
        ] {
            let id = document.create_view(view).unwrap();
            let descriptor = &document.views[&id];
            let Some(SpecializedWorkspacePresenter::ExplanationWorkbench(route)) =
                resolve_specialized_presenter(descriptor).unwrap()
            else {
                panic!("portable specialized descriptor lost its presenter")
            };
            assert_eq!(route.focus, focus);
            assert_eq!(route.object, object);
        }
    }
}
