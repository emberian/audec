//! One session-facing authority for pane selection, transport, audition, and
//! constructive-material consequences.
//!
//! This service owns no project, audio host, or workspace document. It joins
//! their existing authorities at one typed boundary so panes cannot create a
//! private transport, let a drag seek, leave an extracted sample as status
//! text, or publish stale analysis PCM. UI adapters apply returned navigation
//! and transport effects; the session remains project truth.

use std::error::Error;
use std::fmt;

use crate::audio::ProjectFrame;
use crate::audio_host::AudioHost;
use crate::pane_audio::{
    workspace_audition_owner, PaneAudioError, PaneAuditionContext, PaneTimelineEffect, PreviewBus,
    PreviewController, PreviewStatus, SamplePanePreviewEffect, SamplePanePreviewOutcome,
};
use crate::pane_session_binding::{
    PaneSessionBinding, PaneSessionBindingError, PaneSessionDelivery, PaneSessionRegistration,
};
use crate::project_audio_controller::ProjectAudioController;
use crate::project_controller::{recommend_sample_result, RevealRecommendation};
use crate::project_selection::{
    ObjectSelection, ProjectSelection, SelectionProvenance, SelectionSource,
};
use crate::project_session::{ProjectEventBatch, ProjectSession, ProjectSessionError};
use crate::render_runtime::AuditionOwner;
use crate::sample_actions::SamplePublishedResult;
use crate::transport_handoff_controller::{
    TransportEndpoint, TransportHandoffError, WorkspaceTransportAuthority,
    WorkspaceTransportEffects,
};
use crate::workspace_document::LinkFacets;
use crate::workspace_items::WorkspaceViewId;

#[derive(Clone, Debug)]
pub struct PaneSelectionPublication {
    pub selection_revision: u64,
    /// Immediate addressed deliveries. Re-consuming the session event batch
    /// is harmless because link revisions suppress echoes.
    pub linked: Vec<PaneSessionDelivery>,
    /// Selection-only by law; never contains Seek, Play, or loop mutation.
    pub transport: WorkspaceTransportEffects,
}

#[derive(Clone, Debug)]
pub struct PaneMaterialPublication {
    pub recommendation: RevealRecommendation,
    pub selection_revision: u64,
    pub linked: Vec<PaneSessionDelivery>,
    pub transport: WorkspaceTransportEffects,
}

/// Composition root for pane-facing, UI-neutral interaction policy.
#[derive(Clone, Debug, Default)]
pub struct PaneCohesionAuthority {
    sessions: PaneSessionBinding,
    transport: WorkspaceTransportAuthority,
    previews: PreviewController,
}

impl PaneCohesionAuthority {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_pane(
        &mut self,
        session: &mut ProjectSession,
        registration: PaneSessionRegistration,
    ) -> Result<PaneSessionDelivery, PaneCohesionError> {
        Ok(self.sessions.register_pane(session, registration)?)
    }

    /// Teardown cancels only the finite preview owned by this persisted view.
    /// Timeline audition ownership is cleared by the project audio controller
    /// at its ordinary scoped-audition boundary.
    pub fn unregister_pane<B: PreviewBus>(
        &mut self,
        session: &mut ProjectSession,
        view: WorkspaceViewId,
        preview_bus: &B,
    ) -> Result<bool, PaneCohesionError> {
        let owner = workspace_audition_owner(view)?;
        self.previews.cancel_owner(preview_bus, owner);
        Ok(self.sessions.unregister_pane(session, view))
    }

    pub fn consume_batch(
        &mut self,
        session: &ProjectSession,
        batch: ProjectEventBatch,
    ) -> Result<Vec<PaneSessionDelivery>, PaneCohesionError> {
        Ok(self.sessions.consume_batch(session, batch)?)
    }

    /// Commit one pane selection to the project session and prepare its sole
    /// transport-side consequence. Applying the returned command updates the
    /// loop *candidate* only; it does not locate or start playback.
    pub fn publish_selection(
        &mut self,
        session: &mut ProjectSession,
        source: WorkspaceViewId,
        endpoint: TransportEndpoint,
        selection: ProjectSelection,
    ) -> Result<PaneSelectionPublication, PaneCohesionError> {
        let linked = self
            .sessions
            .publish_semantic_selection(session, source, selection)?;
        let linked = self.sessions.accept_link_deliveries(linked)?;
        let revision = session.selection().revision;
        let transport =
            self.transport
                .selection_changed(endpoint, revision, &session.selection().selection)?;
        Ok(PaneSelectionPublication {
            selection_revision: revision,
            linked,
            transport,
        })
    }

    /// Explicit Set Loop snapshots the current selection. A later
    /// `publish_selection` changes only the candidate and leaves the adoption
    /// receipt intact until this method is called again.
    pub fn set_loop_from_selection(
        &mut self,
    ) -> Result<WorkspaceTransportEffects, PaneCohesionError> {
        Ok(self.transport.set_loop_from_selection()?)
    }

    pub fn clear_loop(&mut self) -> WorkspaceTransportEffects {
        self.transport.clear_loop()
    }

    pub fn set_loop_enabled(&self, enabled: bool) -> WorkspaceTransportEffects {
        self.transport.set_loop_enabled(enabled)
    }

    pub fn locate(&self, frame: ProjectFrame) -> WorkspaceTransportEffects {
        self.transport.locate(frame)
    }

    pub fn preview_status(&self) -> PreviewStatus {
        self.previews.status()
    }

    pub fn previews_mut(&mut self) -> &mut PreviewController {
        &mut self.previews
    }

    /// Apply a finite-preview effect through the one shared preview arbiter.
    /// This path cannot seek or mutate project transport.
    pub fn apply_preview<B: PreviewBus>(
        &mut self,
        bus: &B,
        effect: SamplePanePreviewEffect,
    ) -> SamplePanePreviewOutcome {
        effect.apply(&mut self.previews, bus)
    }

    /// Apply aligned harmonic/transient/construction/residual PCM through the
    /// sole project renderer and transport. Ownership must resolve to a pane
    /// still registered in this authority.
    pub fn apply_timeline_audition(
        &self,
        controller: &mut ProjectAudioController,
        host: &AudioHost,
        current: &PaneAuditionContext,
        effect: &PaneTimelineEffect,
    ) -> Result<(), PaneCohesionError> {
        self.require_registered_owner(effect.audition.id.owner)?;
        effect.apply(controller, host, current)?;
        Ok(())
    }

    /// Turn a successful sample/chop/beat publication into ordinary semantic
    /// selection plus a typed reveal request. The new kit, pad, pattern,
    /// occurrence, track, bus, and source material stay related, so extraction
    /// cannot terminate as a toast with no usable destination.
    pub fn publish_material_result(
        &mut self,
        session: &mut ProjectSession,
        source: WorkspaceViewId,
        endpoint: TransportEndpoint,
        result: &SamplePublishedResult,
    ) -> Result<PaneMaterialPublication, PaneCohesionError> {
        if !self.sessions.contains(source) {
            return Err(PaneCohesionError::UnknownPane(source));
        }
        let actual_revision = session.project_snapshot()?.revisions().aggregate;
        if actual_revision != result.revision {
            return Err(PaneCohesionError::StaleMaterialPublication {
                published: result.revision,
                current: actual_revision,
            });
        }
        let mut recommendation = recommend_sample_result(result);
        recommendation.request.current_view = Some(source);
        session.replace_object_selection(
            ObjectSelection {
                primary: Some(recommendation.request.object.clone()),
                secondary: recommendation.request.related.clone(),
                ..ObjectSelection::default()
            },
            SelectionProvenance {
                source: SelectionSource::Reveal,
                source_view: Some(source),
            },
        )?;

        let linked = if session
            .links()
            .membership(source)
            .is_some_and(|membership| membership.facets.contains(LinkFacets::SELECTION))
        {
            let selection = session.selection().selection.clone();
            session.publish_linked_view_state(
                source,
                crate::view_links::LinkedViewPatch {
                    selection: crate::view_links::FacetPatch::Set(selection),
                    ..crate::view_links::LinkedViewPatch::default()
                },
            )?
        } else {
            Vec::new()
        };
        let linked = self.sessions.accept_link_deliveries(linked)?;
        let selection_revision = session.selection().revision;
        let transport = self.transport.selection_changed(
            endpoint,
            selection_revision,
            &session.selection().selection,
        )?;
        Ok(PaneMaterialPublication {
            recommendation,
            selection_revision,
            linked,
            transport,
        })
    }

    fn require_registered_owner(&self, owner: AuditionOwner) -> Result<(), PaneCohesionError> {
        let view = WorkspaceViewId(owner.local);
        if workspace_audition_owner(view)? != owner || !self.sessions.contains(view) {
            return Err(PaneCohesionError::UnknownAuditionOwner(owner));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum PaneCohesionError {
    Binding(PaneSessionBindingError),
    Session(ProjectSessionError),
    Transport(TransportHandoffError),
    Audio(PaneAudioError),
    UnknownPane(WorkspaceViewId),
    UnknownAuditionOwner(AuditionOwner),
    StaleMaterialPublication { published: u64, current: u64 },
}

impl fmt::Display for PaneCohesionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binding(error) => error.fmt(formatter),
            Self::Session(error) => error.fmt(formatter),
            Self::Transport(error) => error.fmt(formatter),
            Self::Audio(error) => error.fmt(formatter),
            Self::UnknownPane(view) => write!(formatter, "workspace pane {} is not registered", view.0),
            Self::UnknownAuditionOwner(owner) => write!(formatter, "audition owner {owner:?} does not name a registered pane"),
            Self::StaleMaterialPublication { published, current } => write!(formatter, "sample publication revision {published} is stale against project revision {current}"),
        }
    }
}

impl Error for PaneCohesionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Binding(error) => Some(error),
            Self::Session(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Audio(error) => Some(error),
            _ => None,
        }
    }
}
impl From<PaneSessionBindingError> for PaneCohesionError {
    fn from(value: PaneSessionBindingError) -> Self {
        Self::Binding(value)
    }
}
impl From<ProjectSessionError> for PaneCohesionError {
    fn from(value: ProjectSessionError) -> Self {
        Self::Session(value)
    }
}
impl From<TransportHandoffError> for PaneCohesionError {
    fn from(value: TransportHandoffError) -> Self {
        Self::Transport(value)
    }
}
impl From<PaneAudioError> for PaneCohesionError {
    fn from(value: PaneAudioError) -> Self {
        Self::Audio(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aspect::{Aspect, FrameSpan};
    use crate::pane_session_binding::{PaneSessionPayload, PaneSessionTopics};
    use crate::project_audio_controller::ProjectTransportCommand;
    use crate::project_session::{ProjectEventFilter, ProjectSessionId};
    use crate::render_plan::{RenderFormat, RenderSpan};
    use crate::workspace_document::ViewLinkMembership;

    fn registration(view: WorkspaceViewId) -> PaneSessionRegistration {
        PaneSessionRegistration {
            view,
            links: ViewLinkMembership::default(),
            topics: PaneSessionTopics::ALL,
        }
    }

    fn selected(start: i64, end: i64) -> ProjectSelection {
        let span = FrameSpan { start, end };
        ProjectSelection {
            time: Some(span),
            aspect: Some(Aspect::Time(span)),
            ..ProjectSelection::default()
        }
    }

    #[test]
    fn one_selection_authority_fans_out_without_locating_and_loop_adoption_is_explicit() {
        let source = WorkspaceViewId(401);
        let peer = WorkspaceViewId(402);
        let mut session = ProjectSession::new(ProjectSessionId(73)).unwrap();
        let mut authority = PaneCohesionAuthority::new();
        authority
            .register_pane(&mut session, registration(source))
            .unwrap();
        authority
            .register_pane(&mut session, registration(peer))
            .unwrap();
        let endpoint = TransportEndpoint {
            timeline: RenderSpan::new(0, 1_000).unwrap(),
            format: RenderFormat::new(48_000, 2).unwrap(),
        };
        let mut subscription = session.subscribe(ProjectEventFilter::SELECTION);

        let first = authority
            .publish_selection(&mut session, source, endpoint, selected(100, 200))
            .unwrap();
        assert!(first.linked.is_empty());
        assert_eq!(
            first.transport.commands,
            vec![ProjectTransportCommand::ReplaceSelection(Some(
                crate::audio::FrameRange::new(ProjectFrame(100), ProjectFrame(200)).unwrap()
            ))]
        );
        assert!(first.transport.commands.iter().all(|command| !matches!(
            command,
            ProjectTransportCommand::Seek(_)
                | ProjectTransportCommand::Play
                | ProjectTransportCommand::TogglePlay
                | ProjectTransportCommand::SetLoopFromSelection
        )));
        let deliveries = authority
            .consume_batch(&session, session.poll_events(&mut subscription))
            .unwrap();
        assert_eq!(deliveries.len(), 2);
        assert!(deliveries.iter().all(|delivery| matches!(
            delivery.payload,
            PaneSessionPayload::AuthoritativeSelection(_)
        )));

        let adopted = authority.set_loop_from_selection().unwrap();
        assert!(matches!(
            adopted.commands.as_slice(),
            [
                ProjectTransportCommand::ReplaceSelection(Some(_)),
                ProjectTransportCommand::SetLoopFromSelection
            ]
        ));
        let second = authority
            .publish_selection(&mut session, source, endpoint, selected(400, 500))
            .unwrap();
        assert_eq!(
            second.transport.commands,
            vec![ProjectTransportCommand::ReplaceSelection(Some(
                crate::audio::FrameRange::new(ProjectFrame(400), ProjectFrame(500)).unwrap()
            ))]
        );
        assert_eq!(
            second.transport.loop_adoption.unwrap().project_span,
            FrameSpan {
                start: 100,
                end: 200
            }
        );
        assert_eq!(
            authority.set_loop_enabled(true).commands,
            vec![ProjectTransportCommand::SetLoopEnabled(true)]
        );
    }
}
