//! GPUI-neutral shared selection and aspect-link coordination.
//!
//! This service owns only semantic attention shared between panes: typed
//! object selections plus an [`Aspect`] and an explicit [`SignalLayer`].  It
//! deliberately refuses to own a renderer, a transport, a viewport, follow
//! behavior, or a native-window handle. Those facts remain local to their
//! respective hosts. The service adapts semantic attention to the existing
//! typed [`ViewLinkRegistry`] patch protocol and records received delivery
//! stamps so an echoed delivery cannot form a link loop.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::aspect::{normalize, Aspect, ExplanationRef, SignalLayer};
use crate::project_selection::{
    ProjectSelection, SelectionAspectError as ProjectSelectionAspectError,
};
use crate::view_links::{
    FacetPatch, LinkedViewPatch, ViewLinkDelivery, ViewLinkError, ViewLinkRegistry,
};
use crate::workspace_document::{
    LinkFacets, LinkGroupId, ViewLinkMembership, WorkspaceDocument, WorkspaceViewId,
};

/// An intentional audio-layer selection. It is an action chosen by a pane or
/// command, never an inference made by link propagation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AspectSignalSwitch {
    Source,
    Construction(ExplanationRef),
    Residual(ExplanationRef),
}

impl From<AspectSignalSwitch> for SignalLayer {
    fn from(value: AspectSignalSwitch) -> Self {
        match value {
            AspectSignalSwitch::Source => SignalLayer::Source,
            AspectSignalSwitch::Construction(reference) => SignalLayer::Explanation(reference),
            AspectSignalSwitch::Residual(reference) => SignalLayer::Residual(reference),
        }
    }
}

/// Per-pane state held by the coordination service. No viewport/playhead or
/// follow flag appears here because those are deliberately independent view
/// choices even when panes share an aspect.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SelectionAspectState {
    pub selection: ProjectSelection,
    /// Last accepted group revision, used to ignore a duplicate/echoed
    /// delivery. Revisions are monotonic within one link group.
    accepted_revisions: BTreeMap<LinkGroupId, u64>,
}

/// Result of applying a typed link delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryDisposition {
    Applied,
    /// The delivery was already accepted (or is older) by this recipient.
    SuppressedLoop,
    /// A link may carry other facets; this service deliberately ignores one
    /// without a selection payload.
    NoSelectionFacet,
}

/// A deterministic semantic-link service. `BTreeMap` ordering makes both
/// membership restoration and delivery side effects stable across runs.
#[derive(Clone, Debug, Default)]
pub struct SelectionAspectService {
    links: ViewLinkRegistry,
    panes: BTreeMap<WorkspaceViewId, SelectionAspectState>,
}

impl SelectionAspectService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one pane with its persisted link membership. Registering an
    /// existing pane updates its membership but preserves local selection.
    pub fn register_view(
        &mut self,
        view: WorkspaceViewId,
        membership: ViewLinkMembership,
    ) -> Result<(), SelectionAspectError> {
        self.links.register(view, membership)?;
        self.panes.entry(view).or_default();
        Ok(())
    }

    pub fn unregister_view(&mut self, view: WorkspaceViewId) -> bool {
        let removed = self.panes.remove(&view).is_some();
        self.links.unregister(view) || removed
    }

    /// Rebuild router membership from durable workspace descriptors. The
    /// document already owns persistence/unknown-field preservation; this
    /// service only consumes its typed `ViewLinkMembership` values.
    pub fn register_workspace_document(
        &mut self,
        document: &WorkspaceDocument,
    ) -> Result<(), SelectionAspectError> {
        document
            .validate()
            .map_err(|error| SelectionAspectError::Workspace(error.to_string()))?;
        for descriptor in document.views.values() {
            self.register_view(descriptor.id, descriptor.links)?;
        }
        Ok(())
    }

    pub fn state(&self, view: WorkspaceViewId) -> Option<&SelectionAspectState> {
        self.panes.get(&view)
    }

    pub fn membership(&self, view: WorkspaceViewId) -> Option<ViewLinkMembership> {
        self.links.membership(view)
    }

    /// Replace local semantic selection without publishing. This is useful for
    /// a pane's own pointer/keyboard interaction and avoids accidental
    /// rebroadcasts while a delivery is being rendered.
    pub fn replace_local_selection(
        &mut self,
        view: WorkspaceViewId,
        mut selection: ProjectSelection,
    ) -> Result<bool, SelectionAspectError> {
        normalize_selection_aspect(&mut selection)?;
        let state = self
            .panes
            .get_mut(&view)
            .ok_or(SelectionAspectError::MissingTarget(view))?;
        if state.selection == selection {
            return Ok(false);
        }
        state.selection = selection;
        Ok(true)
    }

    /// Set a shared aspect while preserving the pane's object selection and
    /// explicit signal layer.
    pub fn set_aspect(
        &mut self,
        view: WorkspaceViewId,
        aspect: Option<Aspect>,
    ) -> Result<bool, SelectionAspectError> {
        let state = self
            .panes
            .get_mut(&view)
            .ok_or(SelectionAspectError::MissingTarget(view))?;
        let mut candidate = state.selection.clone();
        candidate.aspect = aspect.map(normalize);
        normalize_selection_aspect(&mut candidate)?;
        if state.selection == candidate {
            return Ok(false);
        }
        state.selection = candidate;
        Ok(true)
    }

    /// Change only the selected signal layer. It does not synthesize an
    /// `ExplainedBy`/`ResidualOf` term or reinterpret geometry; callers opt
    /// into source/construction/residual explicitly.
    pub fn switch_signal_layer(
        &mut self,
        view: WorkspaceViewId,
        switch: AspectSignalSwitch,
    ) -> Result<bool, SelectionAspectError> {
        let state = self
            .panes
            .get_mut(&view)
            .ok_or(SelectionAspectError::MissingTarget(view))?;
        let signal = SignalLayer::from(switch);
        if state.selection.signal == Some(signal) {
            return Ok(false);
        }
        state.selection.signal = Some(signal);
        Ok(true)
    }

    /// Send only the typed selection facet. The service never publishes a
    /// playhead, transport command, viewport, frequency viewport, or follow
    /// decision; a host that wants those links can use `ViewLinkRegistry`
    /// directly with its own local-state policy.
    pub fn publish_selection(
        &mut self,
        source: WorkspaceViewId,
    ) -> Result<Vec<ViewLinkDelivery>, SelectionAspectError> {
        let selection = self
            .panes
            .get(&source)
            .ok_or(SelectionAspectError::MissingTarget(source))?
            .selection
            .clone();
        let patch = LinkedViewPatch {
            selection: FacetPatch::Set(selection),
            ..LinkedViewPatch::default()
        };
        self.links.publish(source, patch).map_err(Into::into)
    }

    /// Apply an addressed delivery returned by [`publish_selection`]. A
    /// duplicate or older group revision is deliberately ignored, which makes
    /// delivery idempotent and prevents receiver-side event handlers from
    /// echoing a selection around the group.
    pub fn accept_delivery(
        &mut self,
        delivery: &ViewLinkDelivery,
    ) -> Result<DeliveryDisposition, SelectionAspectError> {
        let state = self
            .panes
            .get_mut(&delivery.recipient)
            .ok_or(SelectionAspectError::MissingTarget(delivery.recipient))?;
        if !delivery.facets.contains(LinkFacets::SELECTION) {
            return Ok(DeliveryDisposition::NoSelectionFacet);
        }
        if state
            .accepted_revisions
            .get(&delivery.group)
            .is_some_and(|revision| *revision >= delivery.revision)
        {
            return Ok(DeliveryDisposition::SuppressedLoop);
        }
        match &delivery.state.selection {
            Some(selection) => {
                let mut selection = selection.clone();
                normalize_selection_aspect(&mut selection)?;
                state.selection = selection;
            }
            None => state.selection = ProjectSelection::default(),
        }
        state
            .accepted_revisions
            .insert(delivery.group, delivery.revision);
        Ok(DeliveryDisposition::Applied)
    }

    /// A pane receiving a delivery must not publish that same revision again.
    /// Hosts can ask this before translating their own UI event back into a
    /// local `publish_selection` call.
    pub fn is_delivery_echo(
        &self,
        view: WorkspaceViewId,
        group: LinkGroupId,
        revision: u64,
    ) -> Result<bool, SelectionAspectError> {
        let state = self
            .panes
            .get(&view)
            .ok_or(SelectionAspectError::MissingTarget(view))?;
        Ok(state.accepted_revisions.get(&group) == Some(&revision))
    }
}

fn normalize_selection_aspect(
    selection: &mut ProjectSelection,
) -> Result<(), SelectionAspectError> {
    selection
        .normalize_aspect_signal()
        .map(|_| ())
        .map_err(SelectionAspectError::InvalidSelection)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectionAspectError {
    MissingTarget(WorkspaceViewId),
    Links(ViewLinkError),
    Workspace(String),
    InvalidSelection(ProjectSelectionAspectError),
}

impl fmt::Display for SelectionAspectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTarget(view) => {
                write!(
                    formatter,
                    "selection/aspect target view {} is not registered",
                    view.0
                )
            }
            Self::Links(error) => write!(formatter, "view link: {error}"),
            Self::Workspace(error) => write!(formatter, "workspace links: {error}"),
            Self::InvalidSelection(error) => write!(formatter, "selection/aspect: {error}"),
        }
    }
}

impl Error for SelectionAspectError {}

impl From<ViewLinkError> for SelectionAspectError {
    fn from(error: ViewLinkError) -> Self {
        Self::Links(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aspect::{BandSpan, ChannelMask, FrameSpan};
    use crate::ontology::ObjectId;
    use std::collections::BTreeSet;

    const GROUP: LinkGroupId = LinkGroupId(7);
    const A: WorkspaceViewId = WorkspaceViewId(11);
    const B: WorkspaceViewId = WorkspaceViewId(12);
    const C: WorkspaceViewId = WorkspaceViewId(13);

    fn service() -> SelectionAspectService {
        let mut service = SelectionAspectService::new();
        service
            .register_view(
                A,
                ViewLinkMembership {
                    group: GROUP,
                    facets: LinkFacets::SELECTION,
                },
            )
            .unwrap();
        service
            .register_view(
                B,
                ViewLinkMembership {
                    group: GROUP,
                    facets: LinkFacets::SELECTION,
                },
            )
            .unwrap();
        service
            .register_view(
                C,
                ViewLinkMembership {
                    group: GROUP,
                    facets: LinkFacets::TIME,
                },
            )
            .unwrap();
        service
    }

    #[test]
    fn union_regions_objects_channels_and_signal_propagate_exactly() {
        let mut service = service();
        let geometry = Aspect::Union(vec![
            Aspect::Intersect(vec![
                Aspect::Time(FrameSpan { start: 10, end: 20 }),
                Aspect::Band(BandSpan::new(80.0, 300.0).unwrap()),
                Aspect::Channels(ChannelMask(0b01)),
            ]),
            Aspect::Intersect(vec![
                Aspect::Time(FrameSpan { start: 40, end: 55 }),
                Aspect::Channels(ChannelMask(0b10)),
            ]),
        ]);
        let selection = ProjectSelection {
            time: Some(FrameSpan { start: 10, end: 55 }),
            air: BTreeSet::from([crate::project_selection::AirSelection::Object(
                ObjectId::new(4),
            )]),
            aspect: Some(geometry.clone()),
            signal: Some(SignalLayer::Residual(ExplanationRef::Definition(99))),
            ..ProjectSelection::default()
        };
        service.replace_local_selection(A, selection).unwrap();
        let deliveries = service.publish_selection(A).unwrap();

        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].recipient, B);
        assert_eq!(deliveries[0].facets, LinkFacets::SELECTION);
        assert_eq!(
            service.accept_delivery(&deliveries[0]).unwrap(),
            DeliveryDisposition::Applied
        );
        let received = &service.state(B).unwrap().selection;
        assert_eq!(
            received.signal,
            Some(SignalLayer::Residual(ExplanationRef::Definition(99)))
        );
        assert_eq!(
            received.air,
            BTreeSet::from([crate::project_selection::AirSelection::Object(
                ObjectId::new(4)
            )])
        );
        assert_eq!(received.aspect, Some(normalize(geometry)));
    }

    #[test]
    fn delivery_is_idempotent_and_echo_guard_stops_link_loop() {
        let mut service = service();
        service
            .set_aspect(A, Some(Aspect::Time(FrameSpan { start: 1, end: 2 })))
            .unwrap();
        let delivery = service.publish_selection(A).unwrap().pop().unwrap();
        assert_eq!(
            service.accept_delivery(&delivery).unwrap(),
            DeliveryDisposition::Applied
        );
        assert!(service
            .is_delivery_echo(B, delivery.group, delivery.revision)
            .unwrap());
        assert_eq!(
            service.accept_delivery(&delivery).unwrap(),
            DeliveryDisposition::SuppressedLoop
        );
    }

    #[test]
    fn missing_target_is_explicit_and_non_selection_recipient_is_skipped() {
        let mut service = service();
        assert!(matches!(
            service.set_aspect(WorkspaceViewId(404), Some(Aspect::All)),
            Err(SelectionAspectError::MissingTarget(WorkspaceViewId(404)))
        ));
        service
            .set_aspect(A, Some(Aspect::Time(FrameSpan { start: 1, end: 2 })))
            .unwrap();
        let deliveries = service.publish_selection(A).unwrap();
        assert!(deliveries.iter().all(|delivery| delivery.recipient != C));
    }

    #[test]
    fn signal_switch_is_explicit_and_does_not_rewrite_geometry() {
        let mut service = service();
        let geometry = Aspect::Time(FrameSpan { start: 20, end: 30 });
        service.set_aspect(A, Some(geometry.clone())).unwrap();
        service
            .switch_signal_layer(
                A,
                AspectSignalSwitch::Construction(ExplanationRef::Definition(8)),
            )
            .unwrap();
        let state = service.state(A).unwrap();
        assert_eq!(state.selection.aspect, Some(geometry));
        assert_eq!(
            state.selection.signal,
            Some(SignalLayer::Explanation(ExplanationRef::Definition(8)))
        );
    }

    #[test]
    fn persisted_workspace_link_memberships_restore_without_runtime_ids() {
        let mut document = WorkspaceDocument::default();
        let view = *document.views.keys().next().unwrap();
        let group = document
            .create_link_group(crate::workspace_document::LinkGroupDescriptor::default())
            .unwrap();
        document.views.get_mut(&view).unwrap().links = ViewLinkMembership {
            group,
            facets: LinkFacets::SELECTION,
        };

        let mut service = SelectionAspectService::new();
        service.register_workspace_document(&document).unwrap();
        assert_eq!(
            service.membership(view),
            Some(ViewLinkMembership {
                group,
                facets: LinkFacets::SELECTION,
            })
        );
    }
}
