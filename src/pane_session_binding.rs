//! Addressed, GPUI-neutral publication binding between a project session and panes.
//!
//! This module owns no project, transport, selection, link registry, workspace
//! document, or renderer. [`ProjectSession`] remains the authority for all of
//! those facts. The binding records only which runtime pane identities want
//! which session publications and the last addressed link revision each pane
//! accepted. A GPUI host can therefore translate each returned delivery into
//! one entity update without retaining a parallel project or transport model.
//!
//! View-local viewport, zoom, follow, gesture, and analysis-task state are
//! deliberately absent. Selection is delivered only through the session's
//! existing addressed link events; a plain session selection revision is not
//! silently broadcast to unlinked panes.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::aspect::SignalLayer;
use crate::project_selection::{
    ProjectSelection, SelectionAspectError as ProjectSelectionAspectError,
};
use crate::project_session::{
    ProjectAudioStatus, ProjectEventBatch, ProjectPublication, ProjectSession, ProjectSessionError,
    ProjectSessionEvent,
};
use crate::view_links::{FacetPatch, LinkedViewPatch, ViewLinkDelivery};
use crate::workspace_document::{LinkFacets, LinkGroupId, ViewLinkMembership};
use crate::workspace_items::WorkspaceViewId;

/// Session publications a pane wants to receive. The set is independent of
/// link facets: transport is global and project publication is not navigation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaneSessionTopics(u8);

impl PaneSessionTopics {
    pub const NONE: Self = Self(0);
    pub const PROJECT: Self = Self(1 << 0);
    pub const SELECTION: Self = Self(1 << 1);
    pub const AUDIO: Self = Self(1 << 2);
    pub const ALL: Self = Self(Self::PROJECT.0 | Self::SELECTION.0 | Self::AUDIO.0);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Runtime registration for one persisted workspace view identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneSessionRegistration {
    pub view: WorkspaceViewId,
    pub links: ViewLinkMembership,
    pub topics: PaneSessionTopics,
}

/// A normalized semantic selection delivered to one linked recipient.
/// Geometry and signal remain separate even when their extents are identical.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneSemanticSelection {
    pub selection: ProjectSelection,
    pub signal: SignalLayer,
    pub group: LinkGroupId,
    pub link_revision: u64,
}

/// Coherent full state used to initialize or recover one pane. A pane that is
/// created after the project was opened does not wait for a future event.
#[derive(Clone, Debug)]
pub struct PaneSessionSnapshot {
    pub project: Option<ProjectPublication>,
    pub selection: ProjectSelection,
    pub signal: SignalLayer,
    pub selection_revision: u64,
    pub audio: ProjectAudioStatus,
}

/// A typed update addressed to exactly one stable workspace view.
#[derive(Clone, Debug)]
pub struct PaneSessionDelivery {
    pub recipient: WorkspaceViewId,
    pub payload: PaneSessionPayload,
}

#[derive(Clone, Debug)]
pub enum PaneSessionPayload {
    FullState(PaneSessionSnapshot),
    ProjectPublished(ProjectPublication),
    SemanticSelection(PaneSemanticSelection),
    AudioChanged(ProjectAudioStatus),
}

#[derive(Clone, Debug)]
struct BoundPane {
    topics: PaneSessionTopics,
    /// Delivery cursor only. Link membership and group state remain owned by
    /// `ProjectSession::links`; this map is not a second routing registry.
    accepted_link_revisions: BTreeMap<LinkGroupId, u64>,
}

/// Deterministic fanout adapter over one authoritative [`ProjectSession`].
#[derive(Clone, Debug, Default)]
pub struct PaneSessionBinding {
    panes: BTreeMap<WorkspaceViewId, BoundPane>,
}

impl PaneSessionBinding {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a pane, install its persisted membership in the session's
    /// canonical link registry, and return its complete current state.
    pub fn register_pane(
        &mut self,
        session: &mut ProjectSession,
        registration: PaneSessionRegistration,
    ) -> Result<PaneSessionDelivery, PaneSessionBindingError> {
        session
            .links_mut()
            .register(registration.view, registration.links)
            .map_err(ProjectSessionError::from)?;

        let mut accepted_link_revisions = BTreeMap::new();
        let linked_selection = if registration.links.group != LinkGroupId::UNLINKED {
            session
                .links()
                .group_state(registration.links.group)
                .and_then(|(revision, state)| {
                    accepted_link_revisions.insert(registration.links.group, revision);
                    state.selection.clone()
                })
        } else {
            None
        };
        let (selection, signal) = normalize_selection(
            linked_selection.unwrap_or_else(|| session.selection().selection.clone()),
        )?;
        let snapshot = PaneSessionSnapshot {
            project: current_project_publication(session),
            selection,
            signal,
            selection_revision: session.selection().revision,
            audio: session.audio_status().clone(),
        };
        self.panes.insert(
            registration.view,
            BoundPane {
                topics: registration.topics,
                accepted_link_revisions,
            },
        );
        Ok(PaneSessionDelivery {
            recipient: registration.view,
            payload: PaneSessionPayload::FullState(snapshot),
        })
    }

    pub fn unregister_pane(&mut self, session: &mut ProjectSession, view: WorkspaceViewId) -> bool {
        let pane_removed = self.panes.remove(&view).is_some();
        session.links_mut().unregister(view) || pane_removed
    }

    pub fn contains(&self, view: WorkspaceViewId) -> bool {
        self.panes.contains_key(&view)
    }

    /// Publish one pane-originated selection through the session. The source
    /// pane has already applied its local interaction; returned session link
    /// events address only peers. Consumers should subsequently pass the
    /// polled batch to [`consume_batch`](Self::consume_batch).
    pub fn publish_semantic_selection(
        &self,
        session: &mut ProjectSession,
        source: WorkspaceViewId,
        selection: ProjectSelection,
    ) -> Result<Vec<ViewLinkDelivery>, PaneSessionBindingError> {
        if !self.panes.contains_key(&source) {
            return Err(PaneSessionBindingError::UnknownPane(source));
        }
        let membership = session
            .links()
            .membership(source)
            .ok_or(PaneSessionBindingError::UnknownPane(source))?;
        if !membership.facets.contains(LinkFacets::SELECTION) {
            return Err(PaneSessionBindingError::SelectionNotLinked(source));
        }
        let (selection, _) = normalize_selection(selection)?;
        session.replace_selection(selection.clone());
        session
            .publish_linked_view_state(
                source,
                LinkedViewPatch {
                    selection: FacetPatch::Set(selection),
                    ..LinkedViewPatch::default()
                },
            )
            .map_err(Into::into)
    }

    /// Translate a cursor-polled session batch into addressed pane updates.
    /// Replaying a batch is harmless: per-recipient link revisions suppress
    /// duplicate selection delivery.
    pub fn consume_batch(
        &mut self,
        session: &ProjectSession,
        batch: ProjectEventBatch,
    ) -> Result<Vec<PaneSessionDelivery>, PaneSessionBindingError> {
        if batch.missed_events {
            return self.full_resync(session);
        }

        let mut deliveries = Vec::new();
        for event in batch.events {
            match event {
                ProjectSessionEvent::ProjectPublished(publication) => {
                    self.fanout(
                        PaneSessionTopics::PROJECT,
                        |recipient| PaneSessionDelivery {
                            recipient,
                            payload: PaneSessionPayload::ProjectPublished(publication.clone()),
                        },
                        &mut deliveries,
                    );
                }
                ProjectSessionEvent::LinkedViews(link_deliveries) => {
                    for delivery in link_deliveries {
                        if let Some(delivery) = self.accept_link_delivery(delivery)? {
                            deliveries.push(delivery);
                        }
                    }
                }
                ProjectSessionEvent::AudioChanged(status) => {
                    self.fanout(
                        PaneSessionTopics::AUDIO,
                        |recipient| PaneSessionDelivery {
                            recipient,
                            payload: PaneSessionPayload::AudioChanged(status.clone()),
                        },
                        &mut deliveries,
                    );
                }
                // SelectionChanged updates the authoritative session snapshot.
                // Selection sharing remains explicitly addressed by LinkedViews.
                ProjectSessionEvent::SelectionChanged { .. }
                | ProjectSessionEvent::LifecycleChanged(_)
                | ProjectSessionEvent::HistoryChanged { .. }
                | ProjectSessionEvent::DiagnosticsChanged => {}
            }
        }
        Ok(deliveries)
    }

    fn accept_link_delivery(
        &mut self,
        delivery: ViewLinkDelivery,
    ) -> Result<Option<PaneSessionDelivery>, PaneSessionBindingError> {
        let Some(pane) = self.panes.get_mut(&delivery.recipient) else {
            return Ok(None);
        };
        if !pane.topics.contains(PaneSessionTopics::SELECTION)
            || !delivery.facets.contains(LinkFacets::SELECTION)
        {
            return Ok(None);
        }
        if pane
            .accepted_link_revisions
            .get(&delivery.group)
            .is_some_and(|revision| *revision >= delivery.revision)
        {
            return Ok(None);
        }
        let (selection, signal) =
            normalize_selection(delivery.state.selection.unwrap_or_default())?;
        pane.accepted_link_revisions
            .insert(delivery.group, delivery.revision);
        Ok(Some(PaneSessionDelivery {
            recipient: delivery.recipient,
            payload: PaneSessionPayload::SemanticSelection(PaneSemanticSelection {
                selection,
                signal,
                group: delivery.group,
                link_revision: delivery.revision,
            }),
        }))
    }

    fn full_resync(
        &mut self,
        session: &ProjectSession,
    ) -> Result<Vec<PaneSessionDelivery>, PaneSessionBindingError> {
        let mut deliveries = Vec::with_capacity(self.panes.len());
        for (&recipient, pane) in &mut self.panes {
            let membership = session.links().membership(recipient).unwrap_or_default();
            let linked_selection = if membership.group != LinkGroupId::UNLINKED {
                session
                    .links()
                    .group_state(membership.group)
                    .and_then(|(revision, state)| {
                        pane.accepted_link_revisions
                            .insert(membership.group, revision);
                        state.selection.clone()
                    })
            } else {
                None
            };
            let (selection, signal) = normalize_selection(
                linked_selection.unwrap_or_else(|| session.selection().selection.clone()),
            )?;
            deliveries.push(PaneSessionDelivery {
                recipient,
                payload: PaneSessionPayload::FullState(PaneSessionSnapshot {
                    project: current_project_publication(session),
                    selection,
                    signal,
                    selection_revision: session.selection().revision,
                    audio: session.audio_status().clone(),
                }),
            });
        }
        Ok(deliveries)
    }

    fn fanout(
        &self,
        topic: PaneSessionTopics,
        mut build: impl FnMut(WorkspaceViewId) -> PaneSessionDelivery,
        output: &mut Vec<PaneSessionDelivery>,
    ) {
        output.extend(
            self.panes
                .iter()
                .filter_map(|(&view, pane)| pane.topics.contains(topic).then(|| build(view))),
        );
    }
}

fn current_project_publication(session: &ProjectSession) -> Option<ProjectPublication> {
    let snapshot = session.project_snapshot().ok()?.clone();
    Some(ProjectPublication {
        generation: session.snapshot().generation,
        revisions: snapshot.revisions(),
        snapshot,
        change_set: None,
    })
}

fn normalize_selection(
    mut selection: ProjectSelection,
) -> Result<(ProjectSelection, SignalLayer), PaneSessionBindingError> {
    selection
        .normalize_aspect_signal()
        .map_err(PaneSessionBindingError::InvalidSelection)?;
    let signal = selection.selected_signal();
    Ok((selection, signal))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneSessionBindingError {
    UnknownPane(WorkspaceViewId),
    SelectionNotLinked(WorkspaceViewId),
    Session(ProjectSessionError),
    InvalidSelection(ProjectSelectionAspectError),
}

impl fmt::Display for PaneSessionBindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPane(view) => {
                write!(formatter, "workspace pane {} is not session-bound", view.0)
            }
            Self::SelectionNotLinked(view) => write!(
                formatter,
                "workspace pane {} did not opt into linked selection",
                view.0
            ),
            Self::Session(error) => error.fmt(formatter),
            Self::InvalidSelection(error) => write!(formatter, "semantic selection: {error}"),
        }
    }
}

impl Error for PaneSessionBindingError {}

impl From<ProjectSessionError> for PaneSessionBindingError {
    fn from(error: ProjectSessionError) -> Self {
        Self::Session(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use crate::aspect::{Aspect, ExplanationRef, FrameSpan};
    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        AssetRegistry, ContentFingerprint, DecodedAudioMetadata, SampleFrames,
    };
    use crate::audio::{AudioFormat, FrameRange, ProjectFrame, TransportMode, TransportSnapshot};
    use crate::daw_render::PcmAsset;
    use crate::live_project::{LiveProject, SourceMaterialMetadata};
    use crate::project_session::{ProjectEventFilter, ProjectSessionId, RenderActivity};

    const GROUP: LinkGroupId = LinkGroupId(21);
    const SOURCE: WorkspaceViewId = WorkspaceViewId(41);
    const PEER: WorkspaceViewId = WorkspaceViewId(42);

    fn registration(
        view: WorkspaceViewId,
        links: ViewLinkMembership,
        topics: PaneSessionTopics,
    ) -> PaneSessionRegistration {
        PaneSessionRegistration {
            view,
            links,
            topics,
        }
    }

    fn selection_links() -> ViewLinkMembership {
        ViewLinkMembership {
            group: GROUP,
            facets: LinkFacets::SELECTION,
        }
    }

    fn installed_session() -> ProjectSession {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/pane-session-source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "pane session source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 48_000,
                    channels: 1,
                    frame_count: SampleFrames(4),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"pane session source"),
                provenance: AssetProvenance::new(
                    1,
                    AssetOrigin::ImportedFile {
                        importer: "test".into(),
                    },
                    location,
                ),
                tags: BTreeSet::new(),
                favorite: false,
            })
            .unwrap();
        let pcm = PcmAsset::new(
            AudioFormat::new(48_000, 1).unwrap(),
            Arc::from([0.0, 0.5, -0.25, 0.0]),
        )
        .unwrap();
        let live = LiveProject::from_source_material(
            SourceMaterialMetadata::new("Pane session", "Source"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        let mut session = ProjectSession::new(ProjectSessionId(1)).unwrap();
        session.install(live, None).unwrap();
        session
    }

    fn semantic_deliveries(deliveries: &[PaneSessionDelivery]) -> usize {
        deliveries
            .iter()
            .filter(|delivery| matches!(delivery.payload, PaneSessionPayload::SemanticSelection(_)))
            .count()
    }

    #[test]
    fn linked_selection_is_delivered_exactly_once_to_linked_views() {
        let mut session = ProjectSession::new(ProjectSessionId(1)).unwrap();
        let mut binding = PaneSessionBinding::new();
        binding
            .register_pane(
                &mut session,
                registration(SOURCE, selection_links(), PaneSessionTopics::SELECTION),
            )
            .unwrap();
        binding
            .register_pane(
                &mut session,
                registration(PEER, selection_links(), PaneSessionTopics::SELECTION),
            )
            .unwrap();
        binding
            .register_pane(
                &mut session,
                registration(
                    WorkspaceViewId(43),
                    selection_links(),
                    PaneSessionTopics::SELECTION,
                ),
            )
            .unwrap();
        binding
            .register_pane(
                &mut session,
                registration(
                    WorkspaceViewId(44),
                    ViewLinkMembership::default(),
                    PaneSessionTopics::SELECTION,
                ),
            )
            .unwrap();

        let mut events =
            session.subscribe(ProjectEventFilter::SELECTION.union(ProjectEventFilter::LINKS));
        binding
            .publish_semantic_selection(
                &mut session,
                SOURCE,
                ProjectSelection {
                    aspect: Some(Aspect::Time(FrameSpan { start: 8, end: 24 })),
                    ..ProjectSelection::default()
                },
            )
            .unwrap();
        let batch = session.poll_events(&mut events);
        let first = binding.consume_batch(&session, batch.clone()).unwrap();
        assert_eq!(semantic_deliveries(&first), 2);
        assert_eq!(first[0].recipient, PEER);
        assert_eq!(first[1].recipient, WorkspaceViewId(43));

        let replay = binding.consume_batch(&session, batch).unwrap();
        assert_eq!(semantic_deliveries(&replay), 0);
    }

    #[test]
    fn residual_signal_is_independent_of_selection_geometry() {
        let mut session = ProjectSession::new(ProjectSessionId(1)).unwrap();
        let mut binding = PaneSessionBinding::new();
        for view in [SOURCE, PEER] {
            binding
                .register_pane(
                    &mut session,
                    registration(view, selection_links(), PaneSessionTopics::SELECTION),
                )
                .unwrap();
        }
        let mut events = session.subscribe(ProjectEventFilter::LINKS);
        let geometry = Aspect::Time(FrameSpan {
            start: 100,
            end: 220,
        });
        let residual = SignalLayer::Residual(ExplanationRef::Definition(7));
        binding
            .publish_semantic_selection(
                &mut session,
                SOURCE,
                ProjectSelection {
                    aspect: Some(geometry.clone()),
                    signal: Some(residual),
                    ..ProjectSelection::default()
                },
            )
            .unwrap();
        let deliveries = binding
            .consume_batch(&session, session.poll_events(&mut events))
            .unwrap();
        let PaneSessionPayload::SemanticSelection(delivered) = &deliveries[0].payload else {
            panic!("expected a semantic selection delivery")
        };
        assert_eq!(delivered.selection.aspect, Some(geometry));
        assert_eq!(delivered.selection.signal, Some(residual));
        assert_eq!(delivered.signal, residual);
    }

    #[test]
    fn one_transport_snapshot_fans_out_to_two_arrangement_views() {
        let mut session = ProjectSession::new(ProjectSessionId(1)).unwrap();
        let mut binding = PaneSessionBinding::new();
        for view in [WorkspaceViewId(51), WorkspaceViewId(52)] {
            binding
                .register_pane(
                    &mut session,
                    registration(
                        view,
                        ViewLinkMembership::default(),
                        PaneSessionTopics::AUDIO,
                    ),
                )
                .unwrap();
        }
        let mut events = session.subscribe(ProjectEventFilter::AUDIO);
        let status = ProjectAudioStatus {
            transport: TransportSnapshot {
                mode: TransportMode::Playing,
                frame: ProjectFrame(960),
                loop_region: Some(FrameRange::new(ProjectFrame(900), ProjectFrame(1_200)).unwrap()),
                loop_enabled: true,
                revision: 3,
            },
            render: RenderActivity::Ready { revision: 8 },
            preview_active: false,
            scoped_audition: None,
            diagnostic: None,
        };
        assert!(session.set_audio_status(status.clone()));
        let deliveries = binding
            .consume_batch(&session, session.poll_events(&mut events))
            .unwrap();
        assert_eq!(deliveries.len(), 2);
        assert!(deliveries.iter().all(|delivery| matches!(
            &delivery.payload,
            PaneSessionPayload::AudioChanged(received) if received == &status
        )));
    }

    #[test]
    fn late_dynamic_pane_gets_full_state_then_future_publication() {
        let mut session = installed_session();
        let mut binding = PaneSessionBinding::new();
        let late = WorkspaceViewId(WorkspaceViewId::FIRST_DYNAMIC + 9);
        let initial = binding
            .register_pane(
                &mut session,
                registration(late, ViewLinkMembership::default(), PaneSessionTopics::ALL),
            )
            .unwrap();
        let PaneSessionPayload::FullState(initial) = initial.payload else {
            panic!("late registration must receive full state")
        };
        let initial_generation = initial.project.unwrap().generation;

        let mut events = session.subscribe(ProjectEventFilter::PROJECT);
        session.refresh_published(None).unwrap();
        let deliveries = binding
            .consume_batch(&session, session.poll_events(&mut events))
            .unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].recipient, late);
        let PaneSessionPayload::ProjectPublished(publication) = &deliveries[0].payload else {
            panic!("expected a future project publication")
        };
        assert!(publication.generation > initial_generation);
    }
}
