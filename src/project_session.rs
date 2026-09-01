//! GPUI-independent project-session contracts and published interaction state.
//!
//! [`ProjectSession`] is the sole runtime owner of one [`LiveProject`] and its
//! analysis context. It owns no editor entities or windows. A GPUI host should
//! wrap this value in `Entity<ProjectSession>`, emit the recorded events, and
//! keep hardware/audio-task handles in a sibling controller. Editors read the
//! cached snapshot and submit command envelopes through this session's owned
//! controller; they do not retain mutable domain mirrors.
//!
//! This module does not claim that an analysis is project truth. `DawProject`
//! remains the constructive/AIR aggregate; retained analysis is evidence and
//! task input attached to that aggregate.

#[path = "project_reading_query_session.rs"]
pub mod reading_query;

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::analysis::Analysis;
use crate::assets::{AssetId, AssetRegistration};
use crate::audio::{FrameRange, ProjectFrame, TransportMode, TransportSnapshot};
use crate::change_set::ChangeSet;
use crate::command::{CommandBatch, CommandEnvelope};
use crate::command_journal::{CommandJournalRecord, CommandOperation};
use crate::command_record::CoalesceToken;
use crate::constructive::ConstructiveEditPlan;
use crate::control_views::control_actions::{
    ControlAction, ControlSessionAdapter, ControlSessionOperation, HistoryDirection,
};
use crate::daw_engine::AssetPcmMap;
use crate::daw_project::ProjectRevisions;
use crate::daw_render::PcmAsset;
use crate::live_project::{
    AssetImportDisposition, LiveProject, LiveProjectSnapshot, ProjectController,
    ProjectControllerError, ProjectControllerUpdate, ProjectGesture, ProjectJournalCheckpoint,
    ProjectJournalDelta,
};
use crate::project_controller::{
    ConstructiveOutcome, ObjectRef, PatternWorkflowIntent, PatternWorkflowOutcome,
    SampleActionOutcome, WorkbenchSampleIntent, WorkbenchSampleOutcome,
    WorkbenchSampleWorkflowOutcome, WorkbenchSamplingError,
};
use crate::project_selection::{
    EditCursor, ObjectSelection, ProjectSelection, ProjectSelectionState, SelectionDocumentId,
    SelectionGuard, SelectionGuardError, SelectionProvenance, SelectionReconcileReport,
};
use crate::render_plan::RenderSpan;
use crate::render_runtime::{AuditionMix, AuditionOwner, AuditionSubject, TimelineAuditionId};
use crate::sample_actions::{MaterialPoolSnapshot, SampleAction, SampleWorkflowSpec};
use crate::sample_material::CanonicalPcmIdentity;
use crate::view_links::{
    LinkedViewPatch, ViewLinkDelivery, ViewLinkError, ViewLinkMembership, ViewLinkRegistry,
};
use crate::workspace_items::WorkspaceViewId;

#[path = "deprojection_workspace_bridge.rs"]
pub mod deprojection_workspace_bridge;
use deprojection_workspace_bridge::DeprojectionWorkspaceBridge;

#[path = "project_reveal.rs"]
mod reveal;
#[allow(unused_imports)]
pub use reveal::{
    ProjectRevealError, RevealDisposition, RevealFallback, RevealFreshness, RevealGuard,
    RevealReceipt, RevealRejection, RevealResolution, WorkspaceRevealTargetIssue,
    WorkspaceRevealTargetIssueReason, WorkspaceTargetResolution,
};

#[path = "project_session_lifecycle.rs"]
mod lifecycle;
#[allow(unused_imports)]
pub use lifecycle::{
    ProjectDocumentDiagnostics, ProjectDocumentLifecycle, ProjectDocumentOrigin,
    ProjectExportRequest, ProjectJournalDiagnostic, ProjectJournalDiagnosticKind,
    ProjectJournalPersistenceState, ProjectJournalRecoveryState, ProjectJournalReplayState,
    ProjectLifecycleError, ProjectOpenCompletion, ProjectOpenOutcome, ProjectOpenRequest,
    ProjectReplacementDisposition, ProjectSaveCompletion, ProjectSaveOutcome, ProjectSaveRequest,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectSessionId(pub u64);

fn selection_document_id(
    session: ProjectSessionId,
    document_generation: u64,
) -> SelectionDocumentId {
    // Stable FNV-1a over the two typed identity components. This token is an
    // interaction freshness guard, never a persisted project-object ID.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for value in [session.0, document_generation] {
        for byte in value.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    SelectionDocumentId(hash)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectLifecycle {
    Empty,
    Loading { source: PathBuf },
    Ready,
    Failed { message: String },
}

/// Cached read publication. It is replaced only after a coherent aggregate
/// snapshot has validated, so views never observe a partially reconciled edit.
#[derive(Clone, Debug)]
pub struct ProjectReadSnapshot {
    pub generation: u64,
    pub lifecycle: ProjectLifecycle,
    pub project: Option<LiveProjectSnapshot>,
    pub analysis: Option<Arc<Analysis>>,
}

impl ProjectReadSnapshot {
    pub fn revisions(&self) -> Option<ProjectRevisions> {
        self.project.as_ref().map(LiveProjectSnapshot::revisions)
    }
}

impl Default for ProjectReadSnapshot {
    fn default() -> Self {
        Self {
            generation: 0,
            lifecycle: ProjectLifecycle::Empty,
            project: None,
            analysis: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderActivity {
    Idle,
    Rendering {
        generation: u64,
        revision: u64,
    },
    /// An older coherent revision remains audible while `revision` renders or
    /// waits for its publication boundary.
    Updating {
        generation: u64,
        revision: u64,
        audible_revision: u64,
        candidate_ready: bool,
        publication_in_flight: bool,
    },
    Ready {
        revision: u64,
    },
    Failed {
        generation: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopedAuditionPhase {
    Pending,
    Active,
}

/// Visible truth about a pane-scoped signal heard through the project
/// renderer. It shares the global transport; it is not the one-shot preview
/// bus used for pads and browser samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopedAuditionStatus {
    pub id: TimelineAuditionId,
    pub owner: AuditionOwner,
    pub subject: AuditionSubject,
    pub mix: AuditionMix,
    pub span: RenderSpan,
    pub phase: ScopedAuditionPhase,
}

/// Cheap UI-facing state owned by the eventual audio controller. It carries
/// exact frame values and status only, never PCM or a second transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectAudioStatus {
    pub transport: TransportSnapshot,
    pub render: RenderActivity,
    pub preview_active: bool,
    pub scoped_audition: Option<ScopedAuditionStatus>,
    pub diagnostic: Option<String>,
}

impl Default for ProjectAudioStatus {
    fn default() -> Self {
        Self {
            transport: TransportSnapshot {
                mode: TransportMode::Stopped,
                frame: ProjectFrame(0),
                loop_region: None,
                loop_enabled: false,
                revision: 0,
            },
            render: RenderActivity::Idle,
            preview_active: false,
            scoped_audition: None,
            diagnostic: None,
        }
    }
}

impl ProjectAudioStatus {
    pub fn with_loop(mut self, range: Option<FrameRange>, enabled: bool) -> Self {
        self.transport.loop_region = range;
        self.transport.loop_enabled = enabled && range.is_some();
        self
    }
}

/// Exact project publication consumed by analysis, render, and persistence
/// controllers. The snapshot is Arc-backed and remains coherent even if a
/// subscriber handles several rapid publications after the session advances.
#[derive(Clone, Debug)]
pub struct ProjectPublication {
    pub generation: u64,
    pub revisions: ProjectRevisions,
    pub snapshot: LiveProjectSnapshot,
    /// `None` is an initial/load publication rather than an edit.
    pub change_set: Option<ChangeSet>,
}

#[derive(Clone, Debug)]
pub enum ProjectSessionEvent {
    LifecycleChanged(ProjectLifecycle),
    ProjectPublished(ProjectPublication),
    HistoryChanged {
        can_undo: bool,
        can_redo: bool,
        undo_label: Option<String>,
        redo_label: Option<String>,
        journal_sequence: u64,
    },
    SelectionChanged {
        revision: u64,
    },
    LinkedViews(Vec<ViewLinkDelivery>),
    AudioChanged(ProjectAudioStatus),
    DiagnosticsChanged,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProjectEventFilter(u16);

impl ProjectEventFilter {
    pub const LIFECYCLE: Self = Self(1 << 0);
    pub const PROJECT: Self = Self(1 << 1);
    pub const SELECTION: Self = Self(1 << 2);
    pub const LINKS: Self = Self(1 << 3);
    pub const AUDIO: Self = Self(1 << 4);
    pub const DIAGNOSTICS: Self = Self(1 << 5);
    pub const HISTORY: Self = Self(1 << 6);
    pub const ALL: Self = Self(
        Self::LIFECYCLE.0
            | Self::PROJECT.0
            | Self::SELECTION.0
            | Self::LINKS.0
            | Self::AUDIO.0
            | Self::DIAGNOSTICS.0
            | Self::HISTORY.0,
    );

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn admits(self, event: &ProjectSessionEvent) -> bool {
        self.contains(match event {
            ProjectSessionEvent::LifecycleChanged(_) => Self::LIFECYCLE,
            ProjectSessionEvent::ProjectPublished(_) => Self::PROJECT,
            ProjectSessionEvent::HistoryChanged { .. } => Self::HISTORY,
            ProjectSessionEvent::SelectionChanged { .. } => Self::SELECTION,
            ProjectSessionEvent::LinkedViews(_) => Self::LINKS,
            ProjectSessionEvent::AudioChanged(_) => Self::AUDIO,
            ProjectSessionEvent::DiagnosticsChanged => Self::DIAGNOSTICS,
        })
    }
}

#[derive(Clone, Debug)]
struct StampedEvent {
    sequence: u64,
    event: ProjectSessionEvent,
}

/// Cursor-based subscription contract. The GPUI wrapper can keep one cursor
/// per pane and translate batches into `cx.notify()` without callbacks that
/// re-enter the session during mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectEventSubscription {
    pub filter: ProjectEventFilter,
    next_sequence: u64,
}

#[derive(Clone, Debug)]
pub struct ProjectEventBatch {
    pub events: Vec<ProjectSessionEvent>,
    /// True when the subscriber fell behind the bounded log and should refresh
    /// its complete read snapshot before applying incremental events.
    pub missed_events: bool,
}

#[derive(Clone, Debug)]
struct ProjectEventLog {
    next_sequence: u64,
    capacity: usize,
    events: VecDeque<StampedEvent>,
}

impl ProjectEventLog {
    fn new(capacity: usize) -> Self {
        Self {
            next_sequence: 1,
            capacity: capacity.max(1),
            events: VecDeque::new(),
        }
    }

    fn push(&mut self, event: ProjectSessionEvent) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);
        self.events.push_back(StampedEvent { sequence, event });
        while self.events.len() > self.capacity {
            self.events.pop_front();
        }
    }

    fn subscribe(&self, filter: ProjectEventFilter) -> ProjectEventSubscription {
        ProjectEventSubscription {
            filter,
            next_sequence: self.next_sequence,
        }
    }

    fn poll(&self, subscription: &mut ProjectEventSubscription) -> ProjectEventBatch {
        let oldest = self
            .events
            .front()
            .map_or(self.next_sequence, |event| event.sequence);
        let missed_events = subscription.next_sequence < oldest;
        let start = subscription.next_sequence.max(oldest);
        let events = self
            .events
            .iter()
            .filter(|event| event.sequence >= start)
            .filter(|event| subscription.filter.admits(&event.event))
            .map(|event| event.event.clone())
            .collect();
        subscription.next_sequence = self.next_sequence;
        ProjectEventBatch {
            events,
            missed_events,
        }
    }
}

/// Pure session core. The GPUI entity wrapper is responsible for background
/// jobs, hardware handles, and calling `cx.emit` after these methods record an
/// event. Every durable edit crosses this boundary as a command envelope.
pub struct ProjectSession {
    id: ProjectSessionId,
    document_generation: u64,
    controller: Option<ProjectController>,
    published: ProjectReadSnapshot,
    selection: ProjectSelectionState,
    links: ViewLinkRegistry,
    audio: ProjectAudioStatus,
    diagnostics: Vec<String>,
    events: ProjectEventLog,
    deprojection_workspace: DeprojectionWorkspaceBridge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectHistoryStatus {
    pub can_undo: bool,
    pub can_redo: bool,
    pub undo_label: Option<String>,
    pub redo_label: Option<String>,
}

/// Typed result of one authoritative aggregate mutation. This is the direct
/// return path for editors which need publication/change-set/history facts
/// without polling and correlating separate session events.
#[derive(Clone, Debug)]
pub struct ProjectEditReceipt {
    pub operation: CommandOperation,
    pub publication: ProjectPublication,
    pub change_set: ChangeSet,
    pub history: ProjectHistoryStatus,
    pub journal_sequence: u64,
}

/// Installed-project asset import result. A proven exact-content reuse has no
/// edit receipt because it did not mutate project truth; creation carries the
/// same authoritative receipt as every other aggregate edit.
#[derive(Clone, Debug)]
pub struct ProjectAssetImportReceipt {
    pub asset: AssetId,
    pub disposition: AssetImportDisposition,
    pub decoded_pcm: CanonicalPcmIdentity,
    pub duplicate_predecessors: Vec<AssetId>,
    pub edit: Option<ProjectEditReceipt>,
}

impl ProjectSession {
    pub fn new(id: ProjectSessionId) -> Result<Self, ProjectSessionError> {
        if id.0 == 0 {
            return Err(ProjectSessionError::ZeroId);
        }
        Ok(Self {
            id,
            document_generation: 0,
            controller: None,
            published: ProjectReadSnapshot::default(),
            selection: ProjectSelectionState::default(),
            links: ViewLinkRegistry::default(),
            audio: ProjectAudioStatus::default(),
            diagnostics: Vec::new(),
            events: ProjectEventLog::new(512),
            deprojection_workspace: DeprojectionWorkspaceBridge::new(),
        })
    }

    pub const fn id(&self) -> ProjectSessionId {
        self.id
    }

    /// Monotonic identity for the installed document. Aggregate edits do not
    /// change it; replacing/opening a document does.
    pub const fn document_generation(&self) -> u64 {
        self.document_generation
    }

    pub fn snapshot(&self) -> &ProjectReadSnapshot {
        &self.published
    }

    /// Coherent aggregate/runtime publication for editors, render, and save.
    pub fn project_snapshot(&self) -> Result<&LiveProjectSnapshot, ProjectSessionError> {
        self.published
            .project
            .as_ref()
            .ok_or(ProjectSessionError::NoProject)
    }

    pub fn is_dirty(&self) -> Result<bool, ProjectSessionError> {
        Ok(self.project_snapshot()?.is_dirty())
    }

    pub fn history_status(&self) -> Result<ProjectHistoryStatus, ProjectSessionError> {
        let controller = self
            .controller
            .as_ref()
            .ok_or(ProjectSessionError::NoProject)?;
        Ok(ProjectHistoryStatus {
            can_undo: controller.can_undo(),
            can_redo: controller.can_redo(),
            undo_label: controller.undo_label().map(str::to_owned),
            redo_label: controller.redo_label().map(str::to_owned),
        })
    }

    pub fn live_project(&self) -> Option<&LiveProject> {
        self.controller
            .as_ref()
            .map(ProjectController::live_project)
    }

    pub fn project_controller(&self) -> Option<&ProjectController> {
        self.controller.as_ref()
    }

    pub fn project_controller_mut(&mut self) -> Option<&mut ProjectController> {
        self.controller.as_mut()
    }

    pub fn selection(&self) -> &ProjectSelectionState {
        &self.selection
    }

    pub fn links(&self) -> &ViewLinkRegistry {
        &self.links
    }

    /// Register or update one durable workspace view in the session's sole
    /// semantic-link router. Workspace/layout hosts persist membership, but
    /// must not construct a second runtime router from that description.
    pub fn register_linked_view(
        &mut self,
        view: WorkspaceViewId,
        membership: ViewLinkMembership,
    ) -> Result<(), ProjectSessionError> {
        self.links.register(view, membership).map_err(Into::into)
    }

    /// Remove one runtime view identity from semantic routing. This does not
    /// alter its durable workspace descriptor; a recreated pane can register
    /// the persisted membership again and receive the group's current state.
    pub fn unregister_linked_view(&mut self, view: WorkspaceViewId) -> bool {
        self.links.unregister(view)
    }

    pub fn audio_status(&self) -> &ProjectAudioStatus {
        &self.audio
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub fn begin_loading(&mut self, source: PathBuf) {
        self.deprojection_workspace.clear();
        self.controller = None;
        self.published.project = None;
        self.published.analysis = None;
        self.published.generation = self.published.generation.wrapping_add(1);
        self.published.lifecycle = ProjectLifecycle::Loading { source };
        self.events.push(ProjectSessionEvent::LifecycleChanged(
            self.published.lifecycle.clone(),
        ));
    }

    pub fn install(
        &mut self,
        live: LiveProject,
        analysis: Option<Arc<Analysis>>,
    ) -> Result<ProjectRevisions, ProjectSessionError> {
        self.deprojection_workspace.clear();
        let controller = ProjectController::new(live)?;
        let snapshot = controller.snapshot().clone();
        let revisions = snapshot.revisions();
        self.document_generation = self.document_generation.wrapping_add(1);
        if self.document_generation == 0 {
            self.document_generation = 1;
        }
        self.controller = Some(controller);
        self.published = ProjectReadSnapshot {
            generation: self.published.generation.wrapping_add(1),
            lifecycle: ProjectLifecycle::Ready,
            project: Some(snapshot),
            analysis,
        };
        self.events.push(ProjectSessionEvent::LifecycleChanged(
            ProjectLifecycle::Ready,
        ));
        self.events
            .push(ProjectSessionEvent::ProjectPublished(ProjectPublication {
                generation: self.published.generation,
                revisions,
                snapshot: self
                    .published
                    .project
                    .as_ref()
                    .expect("installed project snapshot exists")
                    .clone(),
                change_set: None,
            }));
        self.events.push(ProjectSessionEvent::HistoryChanged {
            can_undo: false,
            can_redo: false,
            undo_label: None,
            redo_label: None,
            journal_sequence: 0,
        });
        Ok(revisions)
    }

    /// Replace the immutable analysis sidecar without publishing a project
    /// edit. Deferred analysis products do not mutate DAW domains, revisions,
    /// history, transport, or document dirtiness.
    pub fn replace_analysis_snapshot(&mut self, analysis: Arc<Analysis>) {
        self.published.analysis = Some(analysis);
    }

    /// Compatibility publication hook. Once installed in a session, all
    /// aggregate domains are command-owned, so this republishes the cached
    /// controller snapshot and never deep-diffs editor mirrors.
    pub fn refresh_published(
        &mut self,
        change_set: Option<ChangeSet>,
    ) -> Result<ProjectRevisions, ProjectSessionError> {
        let controller = self
            .controller
            .as_ref()
            .ok_or(ProjectSessionError::NoProject)?;
        let snapshot = controller.snapshot().clone();
        let revisions = snapshot.revisions();
        self.published.project = Some(snapshot);
        self.published.generation = self.published.generation.wrapping_add(1);
        self.published.lifecycle = ProjectLifecycle::Ready;
        self.events
            .push(ProjectSessionEvent::ProjectPublished(ProjectPublication {
                generation: self.published.generation,
                revisions,
                snapshot: self
                    .published
                    .project
                    .as_ref()
                    .expect("refreshed project snapshot exists")
                    .clone(),
                change_set,
            }));
        Ok(revisions)
    }

    pub fn execute(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<ProjectRevisions, ProjectSessionError> {
        Ok(self.execute_envelope(envelope)?.publication.revisions)
    }

    pub fn execute_envelope(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<ProjectEditReceipt, ProjectSessionError> {
        let update = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .execute(envelope)?;
        Ok(self.publish_controller_receipt(update))
    }

    /// Execute the complete pattern workflow against the session-owned
    /// controller. Mutating outcomes are republished through the same receipt
    /// path as a raw envelope; navigation, preview, audition, and gesture-only
    /// outcomes remain non-project results.
    pub fn execute_pattern_workflow(
        &mut self,
        intent: PatternWorkflowIntent,
    ) -> Result<PatternWorkflowOutcome, ProjectSessionError> {
        let outcome = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .execute_pattern_workflow(intent)
            .map_err(|error| ProjectSessionError::Action(error.to_string()))?;
        let update = match &outcome {
            PatternWorkflowOutcome::Published { update, .. } => Some(update.clone()),
            PatternWorkflowOutcome::History(Some(update)) => Some(update.clone()),
            PatternWorkflowOutcome::History(None)
            | PatternWorkflowOutcome::Targeted(_)
            | PatternWorkflowOutcome::Navigate(_)
            | PatternWorkflowOutcome::Preview(_)
            | PatternWorkflowOutcome::Audition(_)
            | PatternWorkflowOutcome::GestureBegan(_)
            | PatternWorkflowOutcome::GestureEnded => None,
        };
        if let Some(update) = update {
            self.publish_controller_receipt(update);
        }
        Ok(outcome)
    }

    /// Atomically install durable media metadata and its validated decoded
    /// PCM. Exact content deduplication is decided inside the controller from
    /// both the persisted fingerprint hint and bit-exact canonical PCM.
    pub fn import_asset(
        &mut self,
        expected_revision: u64,
        registration: AssetRegistration,
        pcm: PcmAsset,
    ) -> Result<ProjectAssetImportReceipt, ProjectSessionError> {
        let outcome = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .import_asset(expected_revision, registration, pcm)?;
        let edit = outcome
            .update
            .map(|update| self.publish_controller_receipt(update));
        Ok(ProjectAssetImportReceipt {
            asset: outcome.asset,
            disposition: outcome.disposition,
            decoded_pcm: outcome.decoded_pcm,
            duplicate_predecessors: outcome.duplicate_predecessors,
            edit,
        })
    }

    /// Replay a hydrated journal asset mutation without exposing an
    /// intermediate metadata-only publication.
    pub fn replay_record_with_asset_pcm(
        &mut self,
        record: &CommandJournalRecord,
        asset_pcm: AssetPcmMap,
    ) -> Result<ProjectEditReceipt, ProjectSessionError> {
        let update = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .replay_record_with_asset_pcm(record, asset_pcm)?;
        Ok(self.publish_controller_receipt(update))
    }

    pub fn execute_batch(
        &mut self,
        base_revision: u64,
        batch: CommandBatch,
    ) -> Result<ProjectRevisions, ProjectSessionError> {
        Ok(self
            .execute_batch_with_receipt(base_revision, batch)?
            .publication
            .revisions)
    }

    pub fn execute_batch_with_receipt(
        &mut self,
        base_revision: u64,
        batch: CommandBatch,
    ) -> Result<ProjectEditReceipt, ProjectSessionError> {
        let update = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .execute_batch(crate::command_record::CommandAttempt {
                base_revision,
                batch,
            })?;
        Ok(self.publish_controller_receipt(update))
    }

    /// Start a deterministic gesture boundary. Use `execute_gesture` for each
    /// commit emitted during the gesture, then `end_gesture` even if only one
    /// envelope was produced.
    pub fn begin_gesture(
        &mut self,
        coalesce: CoalesceToken,
    ) -> Result<ProjectGesture, ProjectSessionError> {
        Ok(self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .begin_gesture(coalesce))
    }

    pub fn execute_gesture(
        &mut self,
        gesture: &ProjectGesture,
        envelope: CommandEnvelope,
    ) -> Result<ProjectEditReceipt, ProjectSessionError> {
        let update = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .execute_in_gesture(gesture, envelope)?;
        Ok(self.publish_controller_receipt(update))
    }

    pub fn end_gesture(&mut self, gesture: &ProjectGesture) -> Result<(), ProjectSessionError> {
        self.controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .end_gesture(gesture)?;
        Ok(())
    }

    /// Route one sampler action through the owned controller and mirror its
    /// publication into the session event stream. Ephemeral audition,
    /// inspection, preview, and workspace outcomes do not dirty the project.
    pub fn execute_sample_action(
        &mut self,
        action: SampleAction,
    ) -> Result<SampleActionOutcome, ProjectSessionError> {
        let outcome = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .execute_sample_action(action)
            .map_err(|error| ProjectSessionError::Action(error.to_string()))?;
        if let SampleActionOutcome::Published(published) = &outcome {
            self.publish_controller_update(published.update.clone());
        }
        Ok(outcome)
    }

    /// Publish one already-planned constructive operation through the
    /// session's sole controller and event/history stream. Planning remains
    /// pure; the plan's aggregate revision is checked again at commit time.
    pub fn execute_constructive_plan(
        &mut self,
        plan: ConstructiveEditPlan,
    ) -> Result<ConstructiveOutcome, ProjectSessionError> {
        let outcome = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .execute_constructive_plan(plan)
            .map_err(|error| ProjectSessionError::Action(error.to_string()))?;
        self.publish_controller_update(outcome.update.clone());
        Ok(outcome)
    }

    /// Capture the current immutable project/PCM publication for a heavy
    /// sampler request. This method is intentionally cheap; callers run
    /// `SampleActionBackgroundWork::prepare` on their background executor.
    pub fn capture_sample_action_work(
        &self,
        request: crate::sample_actions::SampleActionRequest,
    ) -> Result<crate::project_controller::SampleActionBackgroundWork, ProjectSessionError> {
        self.controller
            .as_ref()
            .ok_or(ProjectSessionError::NoProject)?
            .capture_sample_action_work(request)
            .map_err(|error| ProjectSessionError::Action(error.to_string()))
    }

    /// Publish already-prepared sampler work at one short authoritative
    /// boundary. A concurrent edit is rejected by the prepared base revision;
    /// successful constructive commits enter the ordinary session event stream.
    pub fn commit_prepared_sample_action(
        &mut self,
        prepared: crate::project_controller::PreparedSampleAction,
    ) -> Result<SampleActionOutcome, ProjectSessionError> {
        let outcome = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .commit_prepared_sample_action(prepared)
            .map_err(|error| ProjectSessionError::Action(error.to_string()))?;
        if let SampleActionOutcome::Published(published) = &outcome {
            self.publish_controller_update(published.update.clone());
        }
        Ok(outcome)
    }

    pub fn publish_primary_workbench_range(
        &mut self,
        range: crate::session::SampleRange,
        intent: WorkbenchSampleIntent,
    ) -> Result<WorkbenchSampleOutcome, ProjectSessionError> {
        let outcome = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .publish_primary_workbench_range(range, intent)
            .map_err(ProjectSessionError::from)?;
        self.publish_controller_update(outcome.constructive.update.clone());
        Ok(outcome)
    }

    /// Product-facing selection/loop workflow. Unlike the compatibility entry
    /// point, this type guarantees the caller receives the named sample rows,
    /// explicit landing, and a revision-matched material-pool snapshot.
    pub fn publish_primary_sample_workflow(
        &mut self,
        range: crate::session::SampleRange,
        spec: SampleWorkflowSpec,
    ) -> Result<WorkbenchSampleWorkflowOutcome, ProjectSessionError> {
        let outcome = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .publish_primary_sample_workflow(range, spec)
            .map_err(ProjectSessionError::from)?;
        self.publish_controller_update(outcome.constructive.update.clone());
        Ok(outcome)
    }

    pub fn material_pool_snapshot(&self) -> Result<MaterialPoolSnapshot, ProjectSessionError> {
        Ok(self
            .controller
            .as_ref()
            .ok_or(ProjectSessionError::NoProject)?
            .material_pool_snapshot())
    }

    pub fn publish_workbench_range(
        &mut self,
        asset: crate::assets::AssetId,
        range: crate::session::SampleRange,
        intent: WorkbenchSampleIntent,
    ) -> Result<WorkbenchSampleOutcome, ProjectSessionError> {
        let outcome = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .publish_workbench_range(asset, range, intent)
            .map_err(ProjectSessionError::from)?;
        self.publish_controller_update(outcome.constructive.update.clone());
        Ok(outcome)
    }

    /// Mixer and automation views submit their typed optimistic intents here.
    /// History requests share the aggregate undo stack instead of invoking a
    /// domain-local history.
    pub fn execute_control_action(
        &mut self,
        action: ControlAction,
    ) -> Result<Option<ProjectRevisions>, ProjectSessionError> {
        self.execute_control_action_for_editor(0, action)
    }

    /// Execute a mixer/automation intent for one stable editor instance.
    ///
    /// `editor_session` separates otherwise-identical numeric and pointer
    /// series originating in different panes. The compatibility entry point
    /// above uses session `0`; dynamic pane hosts should pass their durable
    /// workspace-view identity here.
    pub fn execute_control_action_for_editor(
        &mut self,
        editor_session: u64,
        action: ControlAction,
    ) -> Result<Option<ProjectRevisions>, ProjectSessionError> {
        let operation = {
            let controller = self
                .controller
                .as_ref()
                .ok_or(ProjectSessionError::NoProject)?;
            let snapshot = controller.snapshot();
            let domains = &snapshot.project.state().domains;
            ControlSessionAdapter::new(
                controller.revisions().aggregate,
                editor_session,
                &domains.mixer,
                &domains.automation,
            )
            .adapt(&action)
            .map_err(|error| ProjectSessionError::Action(error.to_string()))?
        };

        match operation {
            ControlSessionOperation::Execute(envelope) => self.execute(envelope).map(Some),
            ControlSessionOperation::History {
                expected_aggregate_revision,
                direction,
            } => {
                let actual = self.project_snapshot()?.revisions().aggregate;
                if expected_aggregate_revision != actual {
                    return Err(ProjectSessionError::RevisionConflict {
                        expected: expected_aggregate_revision,
                        actual,
                    });
                }
                match direction {
                    HistoryDirection::Undo => self.undo(),
                    HistoryDirection::Redo => self.redo(),
                }
            }
        }
    }

    pub fn undo(&mut self) -> Result<Option<ProjectRevisions>, ProjectSessionError> {
        Ok(self
            .undo_with_receipt()?
            .map(|receipt| receipt.publication.revisions))
    }

    pub fn undo_with_receipt(&mut self) -> Result<Option<ProjectEditReceipt>, ProjectSessionError> {
        let update = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .undo()?;
        Ok(update.map(|update| self.publish_controller_receipt(update)))
    }

    pub fn redo(&mut self) -> Result<Option<ProjectRevisions>, ProjectSessionError> {
        Ok(self
            .redo_with_receipt()?
            .map(|receipt| receipt.publication.revisions))
    }

    pub fn redo_with_receipt(&mut self) -> Result<Option<ProjectEditReceipt>, ProjectSessionError> {
        let update = self
            .controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .redo()?;
        Ok(update.map(|update| self.publish_controller_receipt(update)))
    }

    pub fn commit_gesture(&mut self) -> Result<(), ProjectSessionError> {
        self.controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .commit_gesture();
        Ok(())
    }

    pub fn journal_records(&self) -> Result<&[CommandJournalRecord], ProjectSessionError> {
        Ok(self
            .controller
            .as_ref()
            .ok_or(ProjectSessionError::NoProject)?
            .journal_records())
    }

    pub fn journal_checkpoint(&self) -> Result<ProjectJournalCheckpoint, ProjectSessionError> {
        Ok(self
            .controller
            .as_ref()
            .ok_or(ProjectSessionError::NoProject)?
            .journal_checkpoint())
    }

    /// Seed the durable cursor before replaying a compacted journal suffix.
    /// This is a lifecycle-only boundary: it cannot manufacture history or a
    /// publication and is rejected once this installed controller has edited.
    pub fn begin_journal_replay(&mut self, next_sequence: u64) -> Result<(), ProjectSessionError> {
        self.controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .begin_journal_replay(next_sequence)?;
        Ok(())
    }

    pub fn capture_autosave_journal(
        &self,
    ) -> Result<Option<ProjectJournalDelta>, ProjectSessionError> {
        Ok(self
            .controller
            .as_ref()
            .ok_or(ProjectSessionError::NoProject)?
            .pending_journal_delta())
    }

    pub fn acknowledge_autosave_journal(
        &mut self,
        delta: &ProjectJournalDelta,
    ) -> Result<(), ProjectSessionError> {
        self.controller
            .as_mut()
            .ok_or(ProjectSessionError::NoProject)?
            .acknowledge_journal_delta(delta)?;
        Ok(())
    }

    pub fn mark_saved_if_revision(&mut self, revision: u64) -> Result<bool, ProjectSessionError> {
        let (marked, snapshot, revisions) = {
            let controller = self
                .controller
                .as_mut()
                .ok_or(ProjectSessionError::NoProject)?;
            let marked = controller.mark_saved_if_revision(revision)?;
            (
                marked,
                controller.snapshot().clone(),
                controller.revisions(),
            )
        };
        if marked {
            self.published.project = Some(snapshot);
            self.published.generation = self.published.generation.wrapping_add(1);
            self.events
                .push(ProjectSessionEvent::ProjectPublished(ProjectPublication {
                    generation: self.published.generation,
                    revisions,
                    snapshot: self
                        .published
                        .project
                        .as_ref()
                        .expect("saved project snapshot exists")
                        .clone(),
                    change_set: None,
                }));
        }
        Ok(marked)
    }

    fn publish_controller_update(&mut self, update: ProjectControllerUpdate) -> ProjectRevisions {
        self.publish_controller_receipt(update)
            .publication
            .revisions
    }

    fn publish_controller_receipt(
        &mut self,
        update: ProjectControllerUpdate,
    ) -> ProjectEditReceipt {
        let operation = update.operation;
        let journal_sequence = update.journal_sequence;
        let revisions = update.revisions();
        self.published.project = Some(update.snapshot);
        self.published.generation = self.published.generation.wrapping_add(1);
        self.published.lifecycle = ProjectLifecycle::Ready;
        let change_set = update.change_set;
        let publication = ProjectPublication {
            generation: self.published.generation,
            revisions,
            snapshot: self
                .published
                .project
                .as_ref()
                .expect("controller publication snapshot exists")
                .clone(),
            change_set: Some(change_set.clone()),
        };
        self.events
            .push(ProjectSessionEvent::ProjectPublished(publication.clone()));
        let controller = self
            .controller
            .as_ref()
            .expect("controller update requires an installed controller");
        let history = ProjectHistoryStatus {
            can_undo: controller.can_undo(),
            can_redo: controller.can_redo(),
            undo_label: controller.undo_label().map(str::to_owned),
            redo_label: controller.redo_label().map(str::to_owned),
        };
        self.events.push(ProjectSessionEvent::HistoryChanged {
            can_undo: history.can_undo,
            can_redo: history.can_redo,
            undo_label: history.undo_label.clone(),
            redo_label: history.redo_label.clone(),
            journal_sequence,
        });
        ProjectEditReceipt {
            operation,
            publication,
            change_set,
            history,
            journal_sequence,
        }
    }

    pub fn fail(&mut self, message: impl Into<String>) {
        let lifecycle = ProjectLifecycle::Failed {
            message: message.into(),
        };
        self.published.lifecycle = lifecycle.clone();
        self.published.generation = self.published.generation.wrapping_add(1);
        self.events
            .push(ProjectSessionEvent::LifecycleChanged(lifecycle));
    }

    pub fn replace_selection(&mut self, selection: ProjectSelection) -> bool {
        let changed = self.selection.replace(selection);
        self.publish_selection_change(changed);
        changed
    }

    /// Current document/project token for delayed reveal and Inspector work.
    /// The document component is stable across aggregate edits and changes
    /// when either the owning session or installed document changes.
    pub fn current_selection_guard(&self) -> Result<SelectionGuard, ProjectSessionError> {
        let project_revision = self
            .published
            .revisions()
            .ok_or(ProjectSessionError::NoProject)?
            .aggregate;
        Ok(SelectionGuard {
            document: selection_document_id(self.id, self.document_generation),
            project_revision,
        })
    }

    /// Replace a complete selection prepared against a prior session
    /// snapshot. This is the delayed reveal path: its embedded guard must
    /// still exactly match the installed document and aggregate revision.
    pub fn replace_guarded_selection(
        &mut self,
        selection: ProjectSelection,
    ) -> Result<bool, ProjectSessionError> {
        let guard = self.current_selection_guard()?;
        let changed =
            self.selection
                .replace_guarded(selection, guard.document, guard.project_revision)?;
        self.publish_selection_change(changed);
        Ok(changed)
    }

    /// Publish an exact primary/secondary object set from a synchronous view
    /// interaction. The session supplies the current freshness guard and the
    /// caller supplies explicit provenance, including its source view.
    /// Existing time/aspect geometry remains independent and is preserved.
    pub fn replace_object_selection(
        &mut self,
        mut objects: ObjectSelection,
        provenance: SelectionProvenance,
    ) -> Result<bool, ProjectSessionError> {
        let guard = self.current_selection_guard()?;
        objects.guard = Some(guard);
        objects.provenance = provenance;
        let mut selection = if let Some(primary) = objects.primary.clone() {
            let mut selection = ProjectSelection::from_reveal(
                primary,
                objects.secondary.clone(),
                guard,
                provenance.source_view,
            );
            selection.objects.provenance = provenance;
            selection
        } else {
            let mut selection = ProjectSelection::default();
            selection.objects = objects;
            selection
        };
        selection.time = self.selection.selection.time;
        selection.aspect = self.selection.selection.aspect.clone();
        selection.signal = self.selection.selection.signal;
        self.replace_guarded_selection(selection)
    }

    /// Reconcile the current exact object selection against the current
    /// immutable project snapshot. The caller owns object lookup policy; the
    /// session owns document/revision freshness and event publication.
    pub fn reconcile_guarded_selection(
        &mut self,
        exists: impl FnMut(&ObjectRef) -> bool,
    ) -> Result<SelectionReconcileReport, ProjectSessionError> {
        let guard = self.current_selection_guard()?;
        let revision_before = self.selection.revision;
        let report =
            self.selection
                .reconcile_guarded(guard.document, guard.project_revision, exists)?;
        self.publish_selection_change(self.selection.revision != revision_before);
        Ok(report)
    }

    fn publish_selection_change(&mut self, changed: bool) {
        if changed {
            self.events.push(ProjectSessionEvent::SelectionChanged {
                revision: self.selection.revision,
            });
        }
    }

    pub fn set_edit_cursor(&mut self, cursor: EditCursor) -> bool {
        let changed = self.selection.set_edit_cursor(cursor);
        self.publish_selection_change(changed);
        changed
    }

    pub fn publish_linked_view_state(
        &mut self,
        source: WorkspaceViewId,
        patch: LinkedViewPatch,
    ) -> Result<Vec<ViewLinkDelivery>, ProjectSessionError> {
        let deliveries = self.links.publish(source, patch)?;
        if !deliveries.is_empty() {
            self.events
                .push(ProjectSessionEvent::LinkedViews(deliveries.clone()));
        }
        Ok(deliveries)
    }

    pub fn set_audio_status(&mut self, status: ProjectAudioStatus) -> bool {
        if self.audio == status {
            return false;
        }
        self.audio = status.clone();
        self.events.push(ProjectSessionEvent::AudioChanged(status));
        true
    }

    pub fn replace_diagnostics(&mut self, diagnostics: Vec<String>) -> bool {
        if self.diagnostics == diagnostics {
            return false;
        }
        self.diagnostics = diagnostics;
        self.events.push(ProjectSessionEvent::DiagnosticsChanged);
        true
    }

    pub fn subscribe(&self, filter: ProjectEventFilter) -> ProjectEventSubscription {
        self.events.subscribe(filter)
    }

    pub fn poll_events(&self, subscription: &mut ProjectEventSubscription) -> ProjectEventBatch {
        self.events.poll(subscription)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectSessionError {
    ZeroId,
    NoProject,
    Project(String),
    Controller(String),
    Action(String),
    RevisionConflict { expected: u64, actual: u64 },
    Selection(SelectionGuardError),
    Links(ViewLinkError),
}

impl fmt::Display for ProjectSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroId => formatter.write_str("project session ID zero is reserved"),
            Self::NoProject => formatter.write_str("the project session has no live project"),
            Self::Project(message) => write!(formatter, "project session: {message}"),
            Self::Controller(message) => write!(formatter, "project controller: {message}"),
            Self::Action(message) => write!(formatter, "project action: {message}"),
            Self::RevisionConflict { expected, actual } => write!(
                formatter,
                "project action revision conflict: expected {expected}, actual {actual}"
            ),
            Self::Selection(error) => error.fmt(formatter),
            Self::Links(error) => error.fmt(formatter),
        }
    }
}

impl Error for ProjectSessionError {}

impl From<ViewLinkError> for ProjectSessionError {
    fn from(error: ViewLinkError) -> Self {
        Self::Links(error)
    }
}

impl From<SelectionGuardError> for ProjectSessionError {
    fn from(error: SelectionGuardError) -> Self {
        Self::Selection(error)
    }
}

impl From<ProjectControllerError> for ProjectSessionError {
    fn from(error: ProjectControllerError) -> Self {
        Self::Controller(error.to_string())
    }
}

impl From<WorkbenchSamplingError> for ProjectSessionError {
    fn from(error: WorkbenchSamplingError) -> Self {
        Self::Action(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        AssetRegistry, ContentFingerprint, DecodedAudioMetadata, SampleFrames,
    };
    use crate::audio::AudioFormat;
    use crate::command::DomainCommand;
    use crate::control_views::control_actions::{
        ControlEdit, ControlHistoryIntent, ControlSurface, MixerAction, MixerActionIntent,
    };
    use crate::daw_render::PcmAsset;
    use crate::mixer::MixerCommand;
    use crate::sample_actions::SampleKitDestination;
    use crate::session::{Sample, SampleRange};

    fn installed_session() -> ProjectSession {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/session-source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "session source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 48_000,
                    channels: 1,
                    frame_count: SampleFrames(6),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"session source"),
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
            Arc::from([0.0, 0.8, 0.2, 0.0, 0.4, 0.0]),
        )
        .unwrap();
        let live = LiveProject::from_source_material(
            crate::live_project::SourceMaterialMetadata::new("Session", "Source"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        let mut session = ProjectSession::new(ProjectSessionId(1)).unwrap();
        session.install(live, None).unwrap();
        session
    }

    fn imported_registration(
        name: &str,
        content: ContentFingerprint,
        frames: u64,
    ) -> AssetRegistration {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse(format!("/audio/{name}.wav")).unwrap()),
            None,
        )
        .unwrap();
        AssetRegistration {
            name: name.into(),
            location: location.clone(),
            metadata: DecodedAudioMetadata {
                sample_rate_hz: 48_000,
                channels: 1,
                frame_count: SampleFrames(frames),
                container: Some("wav".into()),
                codec: Some("pcm_f32le".into()),
                bit_depth: Some(32),
            },
            content,
            provenance: AssetProvenance::new(
                2,
                AssetOrigin::ImportedFile {
                    importer: "test import".into(),
                },
                location,
            ),
            tags: BTreeSet::new(),
            favorite: false,
        }
    }

    #[test]
    fn subscriptions_filter_and_do_not_reenter_the_session() {
        let mut session = ProjectSession::new(ProjectSessionId(1)).unwrap();
        let mut selection = session.subscribe(ProjectEventFilter::SELECTION);
        session.set_edit_cursor(EditCursor { frame: 12 });
        session.set_audio_status(ProjectAudioStatus {
            preview_active: true,
            ..ProjectAudioStatus::default()
        });
        let batch = session.poll_events(&mut selection);
        assert!(!batch.missed_events);
        assert_eq!(batch.events.len(), 1);
        assert!(matches!(
            batch.events[0],
            ProjectSessionEvent::SelectionChanged { revision: 1 }
        ));
    }

    #[test]
    fn selection_changes_do_not_change_project_generation() {
        let mut session = ProjectSession::new(ProjectSessionId(1)).unwrap();
        let generation = session.snapshot().generation;
        session.set_edit_cursor(EditCursor { frame: 99 });
        assert_eq!(session.snapshot().generation, generation);
    }

    #[test]
    fn exact_object_selection_is_stamped_and_publishes_one_selection_event() {
        let mut session = installed_session();
        let track = session.live_project().unwrap().source_ids().track;
        let clip = session.live_project().unwrap().source_ids().clip;
        let provenance = SelectionProvenance {
            source: crate::project_selection::SelectionSource::Arrangement,
            source_view: Some(WorkspaceViewId(41)),
        };
        let objects = ObjectSelection {
            primary: Some(ObjectRef::AudioClip(clip)),
            secondary: vec![ObjectRef::Track(track)],
            ..ObjectSelection::default()
        };
        let mut events = session.subscribe(ProjectEventFilter::SELECTION);

        assert!(session
            .replace_object_selection(objects.clone(), provenance)
            .unwrap());
        assert!(!session
            .replace_object_selection(objects, provenance)
            .unwrap());

        let selection = &session.selection().selection;
        assert_eq!(selection.objects.primary, Some(ObjectRef::AudioClip(clip)));
        assert_eq!(selection.objects.secondary, vec![ObjectRef::Track(track)]);
        assert_eq!(selection.objects.provenance, provenance);
        assert_eq!(
            selection.objects.guard,
            Some(session.current_selection_guard().unwrap())
        );
        let batch = session.poll_events(&mut events);
        assert_eq!(batch.events.len(), 1);
        assert!(matches!(
            batch.events[0],
            ProjectSessionEvent::SelectionChanged { revision: 1 }
        ));
    }

    #[test]
    fn delayed_selection_with_stale_guard_is_refused_without_an_event() {
        let mut session = installed_session();
        let track = session.live_project().unwrap().source_ids().track;
        let current = session.current_selection_guard().unwrap();
        let stale = SelectionGuard {
            project_revision: current.project_revision.wrapping_add(1),
            ..current
        };
        let selection = ProjectSelection::from_reveal(
            ObjectRef::Track(track),
            [],
            stale,
            Some(WorkspaceViewId(43)),
        );
        let mut events = session.subscribe(ProjectEventFilter::SELECTION);

        assert!(matches!(
            session.replace_guarded_selection(selection),
            Err(ProjectSessionError::Selection(
                SelectionGuardError::ProjectRevisionConflict { .. }
            ))
        ));
        assert_eq!(session.selection().revision, 0);
        assert!(session.poll_events(&mut events).events.is_empty());
    }

    #[test]
    fn guarded_reconciliation_promotes_a_live_related_object_once() {
        let mut session = installed_session();
        let track = session.live_project().unwrap().source_ids().track;
        let clip = session.live_project().unwrap().source_ids().clip;
        session
            .replace_object_selection(
                ObjectSelection {
                    primary: Some(ObjectRef::AudioClip(clip)),
                    secondary: vec![ObjectRef::Track(track)],
                    ..ObjectSelection::default()
                },
                SelectionProvenance {
                    source: crate::project_selection::SelectionSource::Reveal,
                    source_view: Some(WorkspaceViewId(47)),
                },
            )
            .unwrap();
        let mut events = session.subscribe(ProjectEventFilter::SELECTION);

        let report = session
            .reconcile_guarded_selection(|object| matches!(object, ObjectRef::Track(_)))
            .unwrap();

        assert_eq!(report.removed, 1);
        assert!(report.primary_removed);
        assert_eq!(
            session.selection().selection.objects.primary,
            Some(ObjectRef::Track(track))
        );
        assert!(session.selection().selection.objects.secondary.is_empty());
        let batch = session.poll_events(&mut events);
        assert_eq!(batch.events.len(), 1);
        assert!(matches!(
            batch.events[0],
            ProjectSessionEvent::SelectionChanged { revision: 2 }
        ));
    }

    #[test]
    fn selection_document_guard_changes_on_document_replacement() {
        let mut session = installed_session();
        let first = session.current_selection_guard().unwrap();
        assert_eq!(first, session.current_selection_guard().unwrap());
        let replacement = installed_session().live_project().unwrap().clone();

        session.install(replacement, None).unwrap();

        let second = session.current_selection_guard().unwrap();
        assert_ne!(first.document, second.document);
        assert_eq!(
            first.document,
            selection_document_id(ProjectSessionId(1), 1)
        );
        assert_ne!(
            first.document,
            selection_document_id(ProjectSessionId(2), 1)
        );
    }

    #[test]
    fn session_dispatch_publishes_workbench_control_save_and_history_from_one_controller() {
        let mut session = installed_session();
        let mut events =
            session.subscribe(ProjectEventFilter::PROJECT.union(ProjectEventFilter::HISTORY));
        let sampled = session
            .publish_primary_workbench_range(
                SampleRange::new(Sample::new(1), Sample::new(5)),
                WorkbenchSampleIntent::OneShot {
                    kit: SampleKitDestination::NewKit,
                    target_bus: None,
                },
            )
            .unwrap();
        assert!(sampled.constructive.publication.pattern.is_none());
        assert!(session.is_dirty().unwrap());
        assert_eq!(session.project_snapshot().unwrap().sample_pcm.len(), 1);
        assert!(session.history_status().unwrap().can_undo);

        let (mixer_revision, bus) = {
            let snapshot = session.project_snapshot().unwrap();
            (
                snapshot.project.state().domains.mixer.revision(),
                session.live_project().unwrap().source_ids().bus,
            )
        };
        session
            .execute_control_action(ControlAction::Mixer(MixerActionIntent::new(
                mixer_revision,
                MixerAction::SetGainDb { bus, gain_db: -3.0 },
            )))
            .unwrap();
        assert_eq!(
            session
                .project_snapshot()
                .unwrap()
                .project
                .state()
                .domains
                .mixer
                .bus(bus)
                .unwrap()
                .fader()
                .gain_db(),
            -3.0
        );

        session.undo().unwrap().unwrap();
        assert_eq!(
            session
                .project_snapshot()
                .unwrap()
                .project
                .state()
                .domains
                .mixer
                .bus(bus)
                .unwrap()
                .fader()
                .gain_db(),
            0.0
        );
        session.redo().unwrap().unwrap();
        let pending_journal = session
            .capture_autosave_journal()
            .unwrap()
            .expect("unwritten authoritative commands remain pending");
        let saved_revision = session.project_snapshot().unwrap().revisions().aggregate;
        assert!(session.mark_saved_if_revision(saved_revision).unwrap());
        assert!(!session.is_dirty().unwrap());
        assert_eq!(
            session.capture_autosave_journal().unwrap(),
            Some(pending_journal),
            "marking the project explicitly saved must not silently acknowledge an unwritten journal"
        );

        let batch = session.poll_events(&mut events);
        assert!(!batch.missed_events);
        assert!(batch.events.iter().any(|event| matches!(
            event,
            ProjectSessionEvent::ProjectPublished(publication)
                if publication.change_set.is_some()
        )));
        assert!(batch.events.iter().any(|event| matches!(
            event,
            ProjectSessionEvent::HistoryChanged { can_undo: true, .. }
        )));
    }

    #[test]
    fn control_session_adapter_preserves_coalescing_and_authoritative_history() {
        let mut session = installed_session();
        let bus = session.live_project().unwrap().source_ids().bus;
        for gain_db in [-3.0, -9.0] {
            let revision = session
                .project_snapshot()
                .unwrap()
                .project
                .state()
                .domains
                .mixer
                .revision();
            session
                .execute_control_action_for_editor(
                    77,
                    ControlAction::Mixer(
                        MixerActionIntent::new(revision, MixerAction::SetGainDb { bus, gain_db })
                            .with_edit(ControlEdit::Numeric),
                    ),
                )
                .unwrap();
        }
        let mixer_revision = session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .revision();
        session
            .execute_control_action_for_editor(
                77,
                ControlAction::History(ControlHistoryIntent {
                    surface: ControlSurface::Mixer,
                    expected_revision: mixer_revision,
                    direction: HistoryDirection::Undo,
                }),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            session
                .project_snapshot()
                .unwrap()
                .project
                .state()
                .domains
                .mixer
                .bus(bus)
                .unwrap()
                .fader()
                .gain_db(),
            0.0,
            "both numeric entries must undo as one editor-scoped series"
        );
    }

    #[test]
    fn typed_edit_receipt_and_autosave_cursor_share_one_session_path() {
        let mut session = installed_session();
        let (base_revision, command) = {
            let snapshot = session.project_snapshot().unwrap();
            let bus = session.live_project().unwrap().source_ids().bus;
            let command = MixerCommand::build(
                "receipt gain",
                &snapshot.project.state().domains.mixer,
                |mixer| mixer.set_gain_db(bus, -4.5),
            )
            .unwrap();
            (snapshot.revisions().aggregate, command)
        };
        let commands = vec![DomainCommand::Mixer(command)];
        let receipt = session
            .execute_envelope(CommandEnvelope {
                label: "receipt gain".into(),
                base_revision,
                coalesce: None,
                id_claims: crate::command::claims_for_commands(&commands),
                commands,
            })
            .unwrap();
        assert_eq!(receipt.operation, CommandOperation::Execute);
        assert_eq!(receipt.journal_sequence, 1);
        assert!(receipt.history.can_undo);
        assert!(receipt
            .publication
            .change_set
            .as_ref()
            .unwrap()
            .domains
            .contains(&crate::daw_project::ProjectDomain::Mixer));

        let delta = session.capture_autosave_journal().unwrap().unwrap();
        assert_eq!(delta.records.len(), 1);
        session.acknowledge_autosave_journal(&delta).unwrap();
        assert!(session.capture_autosave_journal().unwrap().is_none());

        let undo = session.undo_with_receipt().unwrap().unwrap();
        assert_eq!(undo.operation, CommandOperation::Undo);
        assert_eq!(undo.journal_sequence, 2);
        assert!(session.capture_autosave_journal().unwrap().is_some());
    }

    #[test]
    fn constructive_journal_replay_rematerializes_the_audible_pcm_cohort() {
        let mut session = installed_session();
        let checkpoint = session.project_snapshot().unwrap().clone();
        session
            .publish_primary_workbench_range(
                SampleRange::new(Sample::new(1), Sample::new(5)),
                WorkbenchSampleIntent::OneShot {
                    kit: SampleKitDestination::NewKit,
                    target_bus: None,
                },
            )
            .unwrap();
        let record = session.journal_records().unwrap()[0].clone();
        let direct = session.project_snapshot().unwrap().clone();

        let replay_live = LiveProject::from_project(
            checkpoint.project.as_ref().clone(),
            checkpoint.pcm.as_ref().clone(),
        )
        .unwrap();
        let mut replay = ProjectController::new(replay_live).unwrap();
        replay.replay_record(&record).unwrap();
        assert_eq!(replay.revisions(), direct.revisions());
        assert_eq!(replay.snapshot().sample_pcm.len(), direct.sample_pcm.len());
        for (target, expected) in direct.sample_pcm.iter() {
            let actual = &replay.snapshot().sample_pcm[target];
            assert_eq!(actual.format, expected.format);
            assert_eq!(actual.samples.as_ref(), expected.samples.as_ref());
        }
        assert!(replay.pending_journal_delta().is_none());
    }

    #[test]
    fn installed_asset_import_is_atomic_undoable_redoable_and_hydrated_on_replay() {
        let mut session = installed_session();
        let checkpoint = session.project_snapshot().unwrap().clone();
        let imported_pcm = PcmAsset::new(
            AudioFormat::new(48_000, 1).unwrap(),
            Arc::from([0.25, -0.5, 0.75, 0.0]),
        )
        .unwrap();
        let registration = imported_registration(
            "atomic-import",
            ContentFingerprint::from_bytes(b"atomic import bytes"),
            4,
        );
        let expected_revision = checkpoint.revisions().aggregate;
        let imported = session
            .import_asset(expected_revision, registration, imported_pcm.clone())
            .unwrap();
        assert_eq!(imported.disposition, AssetImportDisposition::Created);
        let edit = imported.edit.as_ref().expect("creation is a project edit");
        assert_eq!(edit.operation, CommandOperation::Execute);
        assert_eq!(
            edit.change_set.domains,
            BTreeSet::from([crate::daw_project::ProjectDomain::Assets])
        );
        assert!(edit
            .publication
            .snapshot
            .project
            .state()
            .domains
            .assets
            .get(imported.asset)
            .is_some());
        assert_eq!(
            edit.publication.snapshot.pcm[&imported.asset]
                .samples
                .as_ref(),
            imported_pcm.samples.as_ref()
        );
        let record = session.journal_records().unwrap()[0].clone();

        let undo = session.undo_with_receipt().unwrap().unwrap();
        assert!(undo
            .publication
            .snapshot
            .project
            .state()
            .domains
            .assets
            .get(imported.asset)
            .is_none());
        assert!(!undo.publication.snapshot.pcm.contains_key(&imported.asset));
        let redo = session.redo_with_receipt().unwrap().unwrap();
        assert!(redo
            .publication
            .snapshot
            .project
            .state()
            .domains
            .assets
            .get(imported.asset)
            .is_some());
        assert_eq!(
            redo.publication.snapshot.pcm[&imported.asset]
                .samples
                .as_ref(),
            imported_pcm.samples.as_ref()
        );
        let history_records = session.journal_records().unwrap().to_vec();

        let replay_live = LiveProject::from_project(
            checkpoint.project.as_ref().clone(),
            checkpoint.pcm.as_ref().clone(),
        )
        .unwrap();
        let mut replay = ProjectController::new(replay_live).unwrap();
        let missing = replay.replay_record(&record).unwrap_err();
        assert!(matches!(
            missing,
            ProjectControllerError::Project(
                crate::live_project::LiveProjectError::MissingImportedAssetPcm(asset)
            ) if asset == imported.asset
        ));
        assert_eq!(replay.revisions(), checkpoint.revisions());
        replay
            .replay_record_with_asset_pcm(
                &record,
                AssetPcmMap::from([(imported.asset, imported_pcm.clone())]),
            )
            .unwrap();
        assert!(replay
            .snapshot()
            .project
            .state()
            .domains
            .assets
            .get(imported.asset)
            .is_some());
        assert_eq!(
            replay.snapshot().pcm[&imported.asset].samples.as_ref(),
            imported_pcm.samples.as_ref()
        );

        let replay_history_live = LiveProject::from_project(
            checkpoint.project.as_ref().clone(),
            checkpoint.pcm.as_ref().clone(),
        )
        .unwrap();
        let mut replay_history = ProjectController::new(replay_history_live).unwrap();
        for (index, record) in history_records.iter().enumerate() {
            let hydration = if index == 1 {
                AssetPcmMap::new()
            } else {
                AssetPcmMap::from([(imported.asset, imported_pcm.clone())])
            };
            replay_history
                .replay_record_with_asset_pcm(record, hydration)
                .unwrap();
        }
        assert_eq!(
            replay_history.revisions(),
            session.project_snapshot().unwrap().revisions()
        );
        assert_eq!(
            replay_history.snapshot().pcm[&imported.asset]
                .samples
                .as_ref(),
            imported_pcm.samples.as_ref()
        );
    }

    #[test]
    fn asset_import_deduplicates_only_after_exact_decoded_pcm_proof() {
        let mut session = installed_session();
        let initial_revision = session.project_snapshot().unwrap().revisions().aggregate;
        let same_pcm = PcmAsset::new(
            AudioFormat::new(48_000, 1).unwrap(),
            Arc::from([0.0, 0.8, 0.2, 0.0, 0.4, 0.0]),
        )
        .unwrap();
        let reused = session
            .import_asset(
                initial_revision,
                imported_registration(
                    "same-content",
                    ContentFingerprint::from_bytes(b"session source"),
                    6,
                ),
                same_pcm,
            )
            .unwrap();
        assert_eq!(
            reused.disposition,
            AssetImportDisposition::ReusedExactContent
        );
        assert_eq!(reused.asset, AssetId(1));
        assert!(reused.edit.is_none());
        assert_eq!(
            session.project_snapshot().unwrap().revisions().aggregate,
            initial_revision
        );
        assert!(session.journal_records().unwrap().is_empty());

        let collision_pcm = PcmAsset::new(
            AudioFormat::new(48_000, 1).unwrap(),
            Arc::from([0.0, 0.8, 0.2, 0.0, 0.5, 0.0]),
        )
        .unwrap();
        let collision = session
            .import_asset(
                initial_revision,
                imported_registration(
                    "fingerprint-collision",
                    ContentFingerprint::from_bytes(b"session source"),
                    6,
                ),
                collision_pcm,
            )
            .unwrap();
        assert_eq!(collision.disposition, AssetImportDisposition::Created);
        assert_ne!(collision.asset, AssetId(1));
        assert_eq!(collision.duplicate_predecessors, vec![AssetId(1)]);
        assert!(collision.edit.is_some());
    }
}
