//! GPUI-neutral orchestration from project publications to persistent audio.
//!
//! A [`ProjectSession`](crate::project_session::ProjectSession) publishes an
//! immutable snapshot. This controller turns it into a cancellable job, keeps
//! only the newest completed generation, and opens a renderer exactly once.
//! Later bounces are coherent cohort publications inside that renderer; they
//! do not replace the device, transport, or audition bus.
//!
//! The controller deliberately does not invent a snapshot hash. Callers must
//! supply exact snapshot/dependency/configuration digests in
//! [`ProjectAudioPlanStamp`]. That keeps a session-local revision counter from
//! masquerading as portable content identity.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::audio::{
    FrameRange, ProjectFrame, TransportHandle, TransportMode, TransportSessionId, TransportSnapshot,
};
use crate::audio_host::{AudioHost, AudioHostSnapshot};
use crate::change_set::ChangeSet;
use crate::daw_engine::{compile_daw_engine, DawEngineConfig, DawEngineRender, EngineDiagnostic};
use crate::daw_render::{RenderCancellation, RenderDiagnostic, RenderWindow};
use crate::project_session::{
    ProjectAudioStatus, ProjectPublication, ProjectSession, ProjectSessionEvent, RenderActivity,
    ScopedAuditionPhase, ScopedAuditionStatus,
};
use crate::render_plan::{
    DeterminismGrade, EngineRecipeStamp, ExactDigest, OutputTailPolicy, RenderDependencyStamp,
    RenderPlan, RenderPlanId, RenderScope, RenderSpan, Tileability,
};
use crate::render_products::{PlaybackCohort, PlaybackCohortId, RenderProduct, TileGrid};
use crate::render_runtime::{
    canonical_pcm_digest, project_revision_stamp, render_format_stamp, AuditionMix, AuditionOwner,
    AuditionSubject, CohortRenderer, CohortRendererControl, CohortRendererStatus,
    ExecutableRenderPlan, PublicationCompletion, PublicationCompletionOutcome, RenderRuntime,
    RenderRuntimeError, RuntimeRenderedAudio, TimelineAudition, TimelineAuditionId,
};
use crate::render_service::{
    AuditionPin, ExportPin, PublicationAction, RenderAvailability, RenderFailure,
    RenderFailureStage,
};
use crate::render_tiles::{
    canonical_reuse_receipt, RenderTileError, TileCohortDraft, TileLayout, TileRenderBatch,
    TileRenderBatchStatus, TileRenderCompletion, TileRenderPolicy, TileReuseProof, TileWorkPlan,
    DEFAULT_TILE_FRAMES,
};

/// Identity facts whose canonical bytes are owned outside the render engine.
#[derive(Clone, Debug)]
pub struct ProjectAudioPlanStamp {
    pub project_namespace: u128,
    pub snapshot: ExactDigest,
    pub engine_abi: u32,
    pub engine_configuration: ExactDigest,
    pub dependencies: Vec<RenderDependencyStamp>,
    pub determinism: DeterminismGrade,
    pub tileability: Tileability,
}

/// Frozen settings for one render job.
#[derive(Clone, Debug)]
pub struct ProjectAudioRenderRecipe {
    pub extent: RenderSpan,
    pub engine: Arc<DawEngineConfig>,
    pub stamp: ProjectAudioPlanStamp,
}

impl ProjectAudioRenderRecipe {
    /// Use the complete occupied arrangement range while retaining leading
    /// zero-based silence. A caller may instead provide a stable larger extent
    /// when it wants edits that lengthen the arrangement to keep one host.
    pub fn audition(
        publication: &ProjectPublication,
        engine: Arc<DawEngineConfig>,
        stamp: ProjectAudioPlanStamp,
    ) -> Result<Self, ProjectAudioControllerError> {
        let range = publication
            .snapshot
            .project
            .state()
            .domains
            .arrangement
            .project_range()
            .ok_or(ProjectAudioControllerError::EmptyArrangement)?;
        let extent = RenderSpan::new(range.start.get().min(0), range.end.get())
            .map_err(|error| ProjectAudioControllerError::Plan(error.to_string()))?;
        Ok(Self {
            extent,
            engine,
            stamp,
        })
    }
}

/// Incremental-bounce policy owned by the project controller. Unsupported or
/// insufficiently described graphs fall back to whole bounce automatically.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectAudioTilePolicy {
    pub grid: TileGrid,
    pub maximum_context_frames: u64,
}

impl Default for ProjectAudioTilePolicy {
    fn default() -> Self {
        Self {
            grid: TileGrid::new(DEFAULT_TILE_FRAMES)
                .expect("default render tile size is a power of two"),
            maximum_context_frames: DEFAULT_TILE_FRAMES as u64,
        }
    }
}

#[derive(Clone, Debug)]
struct ProjectAudioTileSeed {
    previous_cohort: Arc<PlaybackCohort>,
    previous_plan: Arc<RenderPlan>,
    policy: ProjectAudioTilePolicy,
    publication_loop: Option<RenderSpan>,
    playhead: i64,
}

/// Cloneable work item suitable for a normal thread/task pool.
#[derive(Clone, Debug)]
pub struct ProjectAudioRenderJob {
    publication: ProjectPublication,
    recipe: ProjectAudioRenderRecipe,
    controller_cancellation: RenderCancellation,
    tile_seed: Option<ProjectAudioTileSeed>,
}

impl ProjectAudioRenderJob {
    pub const fn generation(&self) -> u64 {
        self.publication.generation
    }

    pub const fn revision(&self) -> u64 {
        self.publication.revisions.aggregate
    }

    pub fn change_set(&self) -> Option<&ChangeSet> {
        self.publication.change_set.as_ref()
    }

    /// Controller-owned cancellation shared with every tile worker. UI and
    /// headless adapters may retain this token instead of inventing a second
    /// cancellation lifetime.
    pub fn cancellation(&self) -> RenderCancellation {
        self.controller_cancellation.clone()
    }

    /// Compile and bounce through the sole DAW engine. Eligible updates render
    /// only missing tiles; cold/structural/unsupported updates retain whole
    /// bounce as the correctness fallback.
    pub fn execute(
        &self,
        cancellation: &RenderCancellation,
    ) -> Result<ProjectAudioRenderCompletion, ProjectAudioControllerError> {
        self.execute_with_progress(cancellation, |_| {})
    }

    /// Execute while reporting immutable, cheap scheduler snapshots. Adapters
    /// may forward these to a UI/task model; the callback must not render,
    /// publish, or mutate the project. PCM identity is independent of callback
    /// timing and worker order.
    pub fn execute_with_progress(
        &self,
        cancellation: &RenderCancellation,
        observe: impl FnMut(ProjectAudioRenderProgress),
    ) -> Result<ProjectAudioRenderCompletion, ProjectAudioControllerError> {
        self.execute_with_live_cursor(cancellation, || None, observe)
    }

    /// Variant for a live host adapter. The cursor callback is sampled before
    /// each claim, allowing not-yet-issued dirty tiles to follow a moving
    /// playhead or a newly edited loop without restarting the deterministic
    /// batch. Already-issued jobs remain valid because priority is not an input.
    pub fn execute_with_live_cursor(
        &self,
        cancellation: &RenderCancellation,
        mut cursor: impl FnMut() -> Option<ProjectAudioRenderCursor>,
        mut observe: impl FnMut(ProjectAudioRenderProgress),
    ) -> Result<ProjectAudioRenderCompletion, ProjectAudioControllerError> {
        observe(ProjectAudioRenderProgress {
            generation: self.generation(),
            phase: ProjectAudioRenderPhase::Compiling,
        });
        if self.recipe.stamp.snapshot.is_zero() {
            return Err(ProjectAudioControllerError::MissingSnapshotDigest);
        }
        if self.recipe.stamp.engine_configuration.is_zero() {
            return Err(ProjectAudioControllerError::MissingEngineConfigurationDigest);
        }
        if cancellation.is_cancelled() {
            self.controller_cancellation.cancel();
        }
        if self.controller_cancellation.is_cancelled() {
            return Err(ProjectAudioControllerError::Cancelled);
        }
        let window = RenderWindow::new(self.recipe.extent.start, self.recipe.extent.end)
            .map_err(|error| ProjectAudioControllerError::Plan(error.to_string()))?;
        let schedule = Arc::new(compile_daw_engine(
            &self.publication.snapshot.project,
            &self.publication.snapshot.pcm,
            window,
            &self.recipe.engine,
            &self.controller_cancellation,
        )?);
        let format = render_format_stamp(schedule.render_schedule().format());
        let engine = EngineRecipeStamp::new(
            self.recipe.stamp.engine_abi,
            format,
            self.recipe.engine.block_frames,
            self.recipe.engine.performance_seed,
            self.recipe.stamp.engine_configuration,
        )
        .map_err(|error| ProjectAudioControllerError::Plan(error.to_string()))?;
        let id = RenderPlanId::new(
            self.recipe.stamp.project_namespace,
            self.recipe.stamp.snapshot,
            project_revision_stamp(self.publication.revisions),
            self.recipe.extent,
            engine,
            self.recipe.stamp.dependencies.clone(),
        )
        .map_err(|error| ProjectAudioControllerError::Plan(error.to_string()))?;
        let descriptor = Arc::new(RenderPlan::new(
            id,
            self.recipe.stamp.determinism,
            self.recipe.stamp.tileability,
        ));
        let executable = Arc::new(ExecutableRenderPlan::new(descriptor, schedule)?);
        let mut diagnostics = executable
            .schedule
            .engine_diagnostics()
            .iter()
            .map(|diagnostic| format!("engine: {diagnostic:?}"))
            .chain(
                executable
                    .schedule
                    .render_diagnostics()
                    .iter()
                    .map(|diagnostic| format!("render: {diagnostic:?}")),
            )
            .collect::<Vec<_>>();
        let products =
            match self.try_render_tiles(&executable, cancellation, &mut cursor, &mut observe) {
                Ok(Some(products)) => products,
                Ok(None) => {
                    observe(ProjectAudioRenderProgress {
                        generation: self.generation(),
                        phase: ProjectAudioRenderPhase::RenderingWhole,
                    });
                    ProjectAudioRenderProducts::Whole {
                        product: executable.render_whole_bounce(&self.controller_cancellation)?,
                    }
                }
                Err(ProjectAudioControllerError::TileUnsupported(message)) => {
                    diagnostics.push(format!("incremental bounce fallback: {message}"));
                    observe(ProjectAudioRenderProgress {
                        generation: self.generation(),
                        phase: ProjectAudioRenderPhase::RenderingWhole,
                    });
                    ProjectAudioRenderProducts::Whole {
                        product: executable.render_whole_bounce(&self.controller_cancellation)?,
                    }
                }
                Err(error) => return Err(error),
            };
        let completion = ProjectAudioRenderCompletion {
            generation: self.publication.generation,
            revision: self.publication.revisions.aggregate,
            change_set: self.publication.change_set.clone(),
            executable,
            products,
            diagnostics,
        };
        observe(ProjectAudioRenderProgress {
            generation: self.generation(),
            phase: ProjectAudioRenderPhase::Complete,
        });
        Ok(completion)
    }

    fn try_render_tiles(
        &self,
        executable: &Arc<ExecutableRenderPlan>,
        external_cancellation: &RenderCancellation,
        cursor: &mut impl FnMut() -> Option<ProjectAudioRenderCursor>,
        observe: &mut impl FnMut(ProjectAudioRenderProgress),
    ) -> Result<Option<ProjectAudioRenderProducts>, ProjectAudioControllerError> {
        let Some(seed) = &self.tile_seed else {
            return Ok(None);
        };
        let Some(changes) = self.publication.change_set.as_ref() else {
            return Err(ProjectAudioControllerError::TileUnsupported(
                "project publication has no exact ChangeSet receipt".into(),
            ));
        };
        let target = &executable.descriptor;
        if target.determinism != DeterminismGrade::BitExact {
            return Err(ProjectAudioControllerError::TileUnsupported(
                "render plan does not promise bit-exact partition equivalence".into(),
            ));
        }
        if seed.previous_plan.extent() != target.extent()
            || seed.previous_plan.format() != target.format()
        {
            return Ok(None);
        }
        let policy = TileRenderPolicy::new(
            seed.policy.grid,
            seed.policy.maximum_context_frames,
            target.tileability,
        )?;
        let layout = match TileLayout::new(target, policy) {
            Ok(layout) => layout,
            Err(RenderTileError::CheckpointRequired) => {
                return Err(ProjectAudioControllerError::TileUnsupported(
                    "graph requires state checkpoints".into(),
                ));
            }
            Err(RenderTileError::SequentialOnly) => {
                return Err(ProjectAudioControllerError::TileUnsupported(
                    "graph is sequential-only".into(),
                ));
            }
            Err(RenderTileError::ContextCeilingExceeded { required, ceiling }) => {
                return Err(ProjectAudioControllerError::TileUnsupported(format!(
                    "graph needs {required} context frames; policy allows {ceiling}"
                )));
            }
            Err(error) => return Err(error.into()),
        };
        let proof = TileReuseProof::new(
            canonical_reuse_receipt(&seed.previous_plan.id, &target.id, changes),
            changes.clone(),
        )?;
        let work = TileWorkPlan::derive(
            &seed.previous_cohort,
            &seed.previous_plan,
            target,
            &layout,
            seed.publication_loop,
            &proof,
        )?;
        let rendered_tiles = work.render_count();
        let reused_tiles = work.reuse_count();
        let mut batch = TileRenderBatch::with_cancellation(
            self.publication.generation,
            work,
            self.controller_cancellation.clone(),
        );
        let default_cursor = ProjectAudioRenderCursor {
            loop_region: seed.publication_loop,
            playhead: seed.playhead,
        };
        loop {
            let current = cursor().unwrap_or(default_cursor);
            observe(ProjectAudioRenderProgress {
                generation: self.generation(),
                phase: ProjectAudioRenderPhase::RenderingTiles(
                    batch.status(current.loop_region, current.playhead),
                ),
            });
            let Some(job) = batch.take_next_job(current.loop_region, current.playhead) else {
                break;
            };
            if external_cancellation.is_cancelled() {
                batch.cancel();
            }
            if batch.is_cancelled() {
                return Err(ProjectAudioControllerError::Cancelled);
            }
            let product = executable.render_tile(&job.spec, &job.cancellation)?;
            if external_cancellation.is_cancelled() {
                batch.cancel();
                return Err(ProjectAudioControllerError::Cancelled);
            }
            batch.accept(TileRenderCompletion {
                generation: job.generation,
                target: job.target,
                index: job.spec.index,
                product,
            })?;
            observe(ProjectAudioRenderProgress {
                generation: self.generation(),
                phase: ProjectAudioRenderPhase::RenderingTiles(
                    batch.status(current.loop_region, current.playhead),
                ),
            });
        }
        Ok(Some(ProjectAudioRenderProducts::Tiles {
            draft: batch.finish()?,
            rendered_tiles,
            reused_tiles,
        }))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectAudioRenderCursor {
    pub loop_region: Option<RenderSpan>,
    pub playhead: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectAudioRenderProgress {
    pub generation: u64,
    pub phase: ProjectAudioRenderPhase,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectAudioRenderPhase {
    Compiling,
    RenderingWhole,
    RenderingTiles(TileRenderBatchStatus),
    Complete,
}

#[derive(Clone, Debug)]
pub enum ProjectAudioRenderProducts {
    Whole {
        product: Arc<RenderProduct>,
    },
    Tiles {
        draft: TileCohortDraft,
        rendered_tiles: usize,
        reused_tiles: usize,
    },
}

#[derive(Clone, Debug)]
pub struct ProjectAudioRenderCompletion {
    pub generation: u64,
    pub revision: u64,
    pub change_set: Option<ChangeSet>,
    pub executable: Arc<ExecutableRenderPlan>,
    pub products: ProjectAudioRenderProducts,
    pub diagnostics: Vec<String>,
}

/// Immutable worker payload for exporting the exact controller target that
/// existed when the user requested the operation. It owns the frozen schedule
/// and never touches transport or the older audible-product cache.
#[derive(Clone, Debug)]
pub struct ProjectAudioExportJob {
    generation: u64,
    revision: u64,
    pin: ExportPin,
    executable: Arc<ExecutableRenderPlan>,
}

#[derive(Clone, Debug)]
pub struct ProjectAudioExportDiagnostics {
    pub engine: Arc<[EngineDiagnostic]>,
    pub render: Arc<[RenderDiagnostic]>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProjectAudioExportIssue {
    Engine(EngineDiagnostic),
    Render(RenderDiagnostic),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProjectAudioExportIntegrity {
    Verified,
    Refused {
        issues: Arc<[ProjectAudioExportIssue]>,
    },
}

impl ProjectAudioExportIntegrity {
    pub fn issues(&self) -> &[ProjectAudioExportIssue] {
        match self {
            Self::Verified => &[],
            Self::Refused { issues } => issues,
        }
    }
}

impl ProjectAudioExportJob {
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn execute(
        &self,
        cancellation: &RenderCancellation,
    ) -> Result<ProjectAudioExportCompletion, ProjectAudioControllerError> {
        let diagnosed = self
            .executable
            .render_fresh_export_pin_with_diagnostics(&self.pin, cancellation)?;
        let diagnostics = ProjectAudioExportDiagnostics {
            engine: diagnosed.engine_diagnostics,
            render: diagnosed.render_diagnostics,
        };
        let integrity = export_integrity(&diagnostics);
        Ok(ProjectAudioExportCompletion {
            generation: self.generation,
            revision: self.revision,
            rendered: diagnosed.rendered,
            diagnostics,
            integrity,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ProjectAudioExportCompletion {
    generation: u64,
    revision: u64,
    rendered: RuntimeRenderedAudio,
    diagnostics: ProjectAudioExportDiagnostics,
    integrity: ProjectAudioExportIntegrity,
}

impl ProjectAudioExportCompletion {
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn diagnostics(&self) -> &ProjectAudioExportDiagnostics {
        &self.diagnostics
    }

    pub fn integrity(&self) -> &ProjectAudioExportIntegrity {
        &self.integrity
    }
}

fn export_integrity(diagnostics: &ProjectAudioExportDiagnostics) -> ProjectAudioExportIntegrity {
    let engine = diagnostics
        .engine
        .iter()
        .filter(|diagnostic| material_engine_diagnostic(diagnostic))
        .cloned()
        .map(ProjectAudioExportIssue::Engine);
    let render = diagnostics
        .render
        .iter()
        .filter(|diagnostic| material_render_diagnostic(diagnostic))
        .cloned()
        .map(ProjectAudioExportIssue::Render);
    let issues: Arc<[_]> = engine.chain(render).collect::<Vec<_>>().into();
    if issues.is_empty() {
        ProjectAudioExportIntegrity::Verified
    } else {
        ProjectAudioExportIntegrity::Refused { issues }
    }
}

fn material_engine_diagnostic(diagnostic: &EngineDiagnostic) -> bool {
    match diagnostic {
        EngineDiagnostic::RegistryAssetOffline { .. }
        | EngineDiagnostic::PcmNotSupplied { .. }
        | EngineDiagnostic::PcmMetadataMismatch { .. }
        | EngineDiagnostic::ClipBusOverrideUnsupported { .. }
        | EngineDiagnostic::InstrumentBusMissing { .. }
        | EngineDiagnostic::IdentityFreeNoteEvents { .. }
        | EngineDiagnostic::InstrumentNotSupplied { .. }
        | EngineDiagnostic::UnroutableSequencerEvents { .. }
        | EngineDiagnostic::SamplerRuntime(_)
        | EngineDiagnostic::DuplicateSamplerConsumerSuppressed { .. } => true,
    }
}

fn material_render_diagnostic(diagnostic: &RenderDiagnostic) -> bool {
    match diagnostic {
        // An unbound arrangement track is authored to use the documented
        // default master destination; no signal or processing is discarded.
        RenderDiagnostic::TrackRoutedToMaster { .. } => false,
        RenderDiagnostic::MissingMixerBus { .. }
        | RenderDiagnostic::UnsupportedTimeTransform { .. }
        | RenderDiagnostic::ArrangementPatternNeedsInstrument { .. }
        | RenderDiagnostic::ArrangementAutomationRegionExternal { .. }
        | RenderDiagnostic::PluginUnavailable { .. }
        | RenderDiagnostic::PluginBypassedByReferenceRenderer { .. }
        | RenderDiagnostic::MissingAsset { .. }
        | RenderDiagnostic::InvalidAssetFormat { .. }
        | RenderDiagnostic::SequencerEventsNeedInstrument { .. } => true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectAudioHostObservation {
    pub transport: TransportSnapshot,
    pub preview_active: bool,
}

impl Default for ProjectAudioHostObservation {
    fn default() -> Self {
        Self {
            transport: TransportSnapshot {
                mode: TransportMode::Stopped,
                frame: ProjectFrame(0),
                loop_region: None,
                loop_enabled: false,
                revision: 0,
            },
            preview_active: false,
        }
    }
}

impl From<AudioHostSnapshot> for ProjectAudioHostObservation {
    fn from(snapshot: AudioHostSnapshot) -> Self {
        Self {
            transport: snapshot.transport,
            preview_active: snapshot.preview_active,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectTransportIntent {
    Play,
    Pause,
    Stop,
    TogglePlay,
    Seek(ProjectFrame),
    SetLoop { range: FrameRange, enabled: bool },
    ClearLoop,
}

/// Shared follow authority for project-time presentations. Individual panes
/// retain their own viewport geometry, but they subscribe to this policy
/// instead of inventing a second playhead clock.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProjectTransportFollowPolicy {
    Off,
    #[default]
    Playhead,
}

/// Semantic commands accepted by the one project transport session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectTransportCommand {
    Play,
    Pause,
    Stop,
    TogglePlay,
    /// A click locate. If it lies outside an enabled loop, the loop bounds are
    /// retained for later editing but disabled in the same backend write as
    /// the seek, so Play cannot jump to a stale loop start.
    Seek(ProjectFrame),
    ReplaceSelection(Option<FrameRange>),
    /// One drag commit that updates selection and replaces the active loop.
    ReplaceSelectionAndLoop(FrameRange),
    SetLoopFromSelection,
    ReplaceLoop {
        range: FrameRange,
        enabled: bool,
        locate_start: bool,
    },
    ClearLoop,
    SetLoopEnabled(bool),
    SetFollow(ProjectTransportFollowPolicy),
}

/// Authoritative, PCM-free session state consumed by arrangement and analysis
/// panes. Selection is intentionally separate from the transport loop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectTransportSessionSnapshot {
    pub transport: TransportSnapshot,
    pub selection: Option<FrameRange>,
    pub follow: ProjectTransportFollowPolicy,
    pub desired_revision: Option<u64>,
    pub audible_cohort: Option<PlaybackCohortId>,
    pub scoped_audition: Option<ScopedAuditionStatus>,
    pub host: Option<TransportSessionId>,
    pub host_handoff_pending: bool,
    /// Changes for semantic commands, host observations, cohort swaps, and
    /// owner-scoped audition handoffs.
    pub revision: u64,
}

/// Backend state machine for the single project transport. It owns no device;
/// commands are applied to the `TransportHandle` owned by `AudioHost`.
#[derive(Clone, Debug)]
pub struct ProjectTransportSession {
    snapshot: ProjectTransportSessionSnapshot,
    clearing_audition: Option<TimelineAuditionId>,
    retired_host: Option<TransportSessionId>,
    /// The control-side revision produced by the newest command whose
    /// frame/mode publication has not crossed the realtime boundary yet.
    /// Host observations at or behind this revision may contribute loop
    /// control state, but must not roll the semantic playhead back to the
    /// audio thread's older publication.
    pending_transport_ack: Option<u64>,
}

impl Default for ProjectTransportSession {
    fn default() -> Self {
        Self {
            snapshot: ProjectTransportSessionSnapshot {
                transport: ProjectAudioHostObservation::default().transport,
                selection: None,
                follow: ProjectTransportFollowPolicy::default(),
                desired_revision: None,
                audible_cohort: None,
                scoped_audition: None,
                host: None,
                host_handoff_pending: false,
                revision: 0,
            },
            clearing_audition: None,
            retired_host: None,
            pending_transport_ack: None,
        }
    }
}

impl ProjectTransportSession {
    pub fn snapshot(&self) -> ProjectTransportSessionSnapshot {
        self.snapshot.clone()
    }

    pub fn observe(&mut self, mut observation: TransportSnapshot) {
        if let Some(command_revision) = self.pending_transport_ack {
            if revision_advanced_after(observation.revision, command_revision) {
                self.pending_transport_ack = None;
            } else if observation.revision != command_revision {
                // A snapshot captured before the newest control transaction
                // contains neither its loop tuple nor its locate. Ignore the
                // whole stale observation instead of assembling a hybrid.
                return;
            } else {
                // `TransportHandle::snapshot` deliberately reports the audio
                // publication for frame/mode and the control snapshot for
                // loop state. Until the audio thread acknowledges the command,
                // retain the session's requested locate/mode so a UI polling
                // tick cannot resurrect an old loop start.
                observation.frame = self.snapshot.transport.frame;
                observation.mode = self.snapshot.transport.mode;
            }
        }
        if self.snapshot.transport != observation {
            self.snapshot.transport = observation;
            self.bump_revision();
        }
    }

    pub fn bind_host(
        &mut self,
        transport: &TransportHandle,
    ) -> Result<(), ProjectAudioControllerError> {
        let identity = transport.session_id();
        if self.snapshot.host_handoff_pending && self.retired_host == Some(identity) {
            return Err(ProjectAudioControllerError::TransportHostRetired(identity));
        }
        if let Some(bound) = self.snapshot.host {
            if bound != identity {
                return Err(ProjectAudioControllerError::TransportHostMismatch {
                    expected: bound,
                    actual: identity,
                });
            }
        }
        self.snapshot.host = Some(identity);
        self.snapshot.host_handoff_pending = false;
        self.retired_host = None;
        self.pending_transport_ack = None;
        self.snapshot.transport = transport.snapshot();
        self.bump_revision();
        Ok(())
    }

    fn begin_host_handoff(&mut self) {
        self.retired_host = self.snapshot.host;
        self.snapshot.host = None;
        self.snapshot.host_handoff_pending = true;
        self.snapshot.scoped_audition = None;
        self.clearing_audition = None;
        self.pending_transport_ack = None;
        self.bump_revision();
    }

    fn require_host(&self, transport: &TransportHandle) -> Result<(), ProjectAudioControllerError> {
        if self.snapshot.host_handoff_pending {
            return Err(ProjectAudioControllerError::TransportHostHandoffPending);
        }
        let actual = transport.session_id();
        match self.snapshot.host {
            Some(expected) if expected != actual => {
                Err(ProjectAudioControllerError::TransportHostMismatch { expected, actual })
            }
            Some(_) => Ok(()),
            None => Err(ProjectAudioControllerError::TransportHostNotBound),
        }
    }

    pub fn apply(
        &mut self,
        transport: &TransportHandle,
        command: ProjectTransportCommand,
    ) -> Result<(), ProjectAudioControllerError> {
        self.require_host(transport)?;
        let before = self.snapshot.clone();
        let mut touched_transport = true;
        match command {
            ProjectTransportCommand::Play => {
                transport.play();
                if self.snapshot.transport.mode == TransportMode::Ended {
                    self.snapshot.transport.frame = ProjectFrame(0);
                }
                self.snapshot.transport.mode = TransportMode::Playing;
            }
            ProjectTransportCommand::Pause => {
                transport.pause();
                if self.snapshot.transport.mode != TransportMode::Stopped {
                    self.snapshot.transport.mode = TransportMode::Paused;
                }
            }
            ProjectTransportCommand::Stop => {
                transport.stop();
                self.snapshot.transport.mode = TransportMode::Stopped;
                self.snapshot.transport.frame = ProjectFrame(0);
            }
            ProjectTransportCommand::TogglePlay => {
                transport.toggle();
                if self.snapshot.transport.mode == TransportMode::Playing {
                    self.snapshot.transport.mode = TransportMode::Paused;
                } else {
                    if self.snapshot.transport.mode == TransportMode::Ended {
                        self.snapshot.transport.frame = ProjectFrame(0);
                    }
                    self.snapshot.transport.mode = TransportMode::Playing;
                }
            }
            ProjectTransportCommand::Seek(frame) => {
                let frame = ProjectFrame(frame.0.min(transport.length().0));
                let loop_outside = self.snapshot.transport.loop_enabled
                    && self
                        .snapshot
                        .transport
                        .loop_region
                        .is_some_and(|range| frame < range.start || frame >= range.end);
                if loop_outside {
                    transport.set_loop_state(
                        self.snapshot.transport.loop_region,
                        false,
                        Some(frame),
                    )?;
                    self.snapshot.transport.loop_enabled = false;
                } else {
                    transport.seek(frame);
                }
                self.record_locate(frame);
            }
            ProjectTransportCommand::ReplaceSelection(selection) => {
                touched_transport = false;
                self.validate_selection(transport, selection)?;
                self.snapshot.selection = selection;
            }
            ProjectTransportCommand::ReplaceSelectionAndLoop(range) => {
                self.validate_selection(transport, Some(range))?;
                transport.set_loop_state(Some(range), true, Some(range.start))?;
                self.snapshot.selection = Some(range);
                self.snapshot.transport.loop_region = Some(range);
                self.snapshot.transport.loop_enabled = true;
                self.record_locate(range.start);
            }
            ProjectTransportCommand::SetLoopFromSelection => {
                let range = self
                    .snapshot
                    .selection
                    .ok_or(ProjectAudioControllerError::NoTransportSelection)?;
                transport.set_loop_state(Some(range), true, Some(range.start))?;
                self.snapshot.transport.loop_region = Some(range);
                self.snapshot.transport.loop_enabled = true;
                self.record_locate(range.start);
            }
            ProjectTransportCommand::ReplaceLoop {
                range,
                enabled,
                locate_start,
            } => {
                self.validate_selection(transport, Some(range))?;
                transport.set_loop_state(
                    Some(range),
                    enabled,
                    locate_start.then_some(range.start),
                )?;
                self.snapshot.transport.loop_region = Some(range);
                self.snapshot.transport.loop_enabled = enabled;
                if locate_start {
                    self.record_locate(range.start);
                }
            }
            ProjectTransportCommand::ClearLoop => {
                transport.set_loop_state(None, false, None)?;
                self.snapshot.transport.loop_region = None;
                self.snapshot.transport.loop_enabled = false;
            }
            ProjectTransportCommand::SetLoopEnabled(enabled) => {
                transport.set_loop_state(self.snapshot.transport.loop_region, enabled, None)?;
                self.snapshot.transport.loop_enabled =
                    enabled && self.snapshot.transport.loop_region.is_some();
            }
            ProjectTransportCommand::SetFollow(follow) => self.snapshot.follow = follow,
        }
        if matches!(command, ProjectTransportCommand::SetFollow(_)) {
            touched_transport = false;
        }
        if touched_transport {
            self.pending_transport_ack = Some(transport.snapshot().revision);
        }
        if self.snapshot != before {
            self.snapshot.revision = before.revision.wrapping_add(1);
        }
        Ok(())
    }

    fn validate_selection(
        &self,
        transport: &TransportHandle,
        selection: Option<FrameRange>,
    ) -> Result<(), ProjectAudioControllerError> {
        if let Some(range) = selection {
            if range.end > transport.length() {
                return Err(
                    ProjectAudioControllerError::TransportSelectionOutsideTimeline {
                        selection: range,
                        length: transport.length(),
                    },
                );
            }
        }
        Ok(())
    }

    fn record_locate(&mut self, frame: ProjectFrame) {
        self.snapshot.transport.frame = frame;
        self.snapshot.transport.mode = match self.snapshot.transport.mode {
            TransportMode::Playing => TransportMode::Playing,
            TransportMode::Stopped if frame == ProjectFrame(0) => TransportMode::Stopped,
            _ => TransportMode::Paused,
        };
    }

    fn set_desired_revision(&mut self, revision: Option<u64>) {
        if self.snapshot.desired_revision != revision {
            self.snapshot.desired_revision = revision;
            self.bump_revision();
        }
    }

    fn set_audible_cohort(&mut self, cohort: Option<PlaybackCohortId>) {
        if self.snapshot.audible_cohort != cohort {
            self.snapshot.audible_cohort = cohort;
            self.bump_revision();
        }
    }

    fn set_scoped_audition(&mut self, audition: Option<ScopedAuditionStatus>) {
        if self.snapshot.scoped_audition != audition {
            self.snapshot.scoped_audition = audition;
            if audition.is_some() {
                self.clearing_audition = None;
            }
            self.bump_revision();
        }
    }

    fn bump_revision(&mut self) {
        self.snapshot.revision = self.snapshot.revision.wrapping_add(1);
    }
}

/// Transport revisions are wrapping monotonic counters. A delta in the lower
/// half of the integer space is forward progress; equality and the upper half
/// denote an unacknowledged or older observation.
const fn revision_advanced_after(observed: u64, baseline: u64) -> bool {
    let delta = observed.wrapping_sub(baseline);
    delta != 0 && delta < (1_u64 << 63)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditionAlignment {
    /// Change only the signal heard at the existing project position.
    PreserveTransport,
    /// Seek the sole project transport to the audition's first frame.
    SeekToStart { play: bool },
    /// Adopt the audition span as the sole project loop, seek, and optionally
    /// play. Every pane observes the same resulting transport state.
    LoopSpan { play: bool },
}

/// Minimal adoption receipt shared by exact recipe adapters. Pattern, notation,
/// and future analytical recipes retain their richer provenance themselves;
/// the audio controller needs only the project revision and aligned PCM span.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectAudioAuditionPin {
    pub revision: u64,
    pub span: RenderSpan,
}

/// The only effects that require an owner outside this pure controller.
pub enum ProjectAudioControllerEffect {
    None,
    /// Open `AudioHost::open_renderer(renderer)` once for a cold project.
    OpenHost(CohortRenderer),
    /// Format or timeline extent changed; preserving one host is impossible
    /// because the current transport freezes both facts.
    ReplaceHost(CohortRenderer),
    Superseded {
        generation: u64,
        desired_generation: u64,
    },
}

#[derive(Clone, Debug)]
struct DesiredTarget {
    generation: u64,
    revision: u64,
    change_set: Option<ChangeSet>,
}

/// Main/control-thread state. Worker jobs contain no references back here.
pub struct ProjectAudioController {
    runtime: RenderRuntime,
    renderer_control: Option<CohortRendererControl>,
    tile_policy: Option<ProjectAudioTilePolicy>,
    active_render_cancellation: Option<RenderCancellation>,
    desired: Option<DesiredTarget>,
    transport_session: ProjectTransportSession,
    preview_active: bool,
    pending_action: Option<PublicationAction>,
    plan_generations: BTreeMap<RenderPlanId, u64>,
    audible_generation: Option<u64>,
    diagnostics: Vec<String>,
    local_failure: Option<(u64, String)>,
}

impl Default for ProjectAudioController {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectAudioController {
    pub fn new() -> Self {
        Self {
            runtime: RenderRuntime::new(),
            renderer_control: None,
            tile_policy: Some(ProjectAudioTilePolicy::default()),
            active_render_cancellation: None,
            desired: None,
            transport_session: ProjectTransportSession::default(),
            preview_active: false,
            pending_action: None,
            plan_generations: BTreeMap::new(),
            audible_generation: None,
            diagnostics: Vec::new(),
            local_failure: None,
        }
    }

    pub fn runtime(&self) -> &RenderRuntime {
        &self.runtime
    }

    pub fn renderer_control(&self) -> Option<&CohortRendererControl> {
        self.renderer_control.as_ref()
    }

    /// Realtime health is separate from bounce readiness. Dirty target tiles
    /// mean "old coherent audio remains audible"; starvation means the active
    /// published table itself failed to supply a requested master frame.
    pub fn renderer_status(&self) -> Option<CohortRendererStatus> {
        self.renderer_control
            .as_ref()
            .map(CohortRendererControl::status)
    }

    pub const fn tile_policy(&self) -> Option<ProjectAudioTilePolicy> {
        self.tile_policy
    }

    pub fn set_tile_policy(&mut self, policy: Option<ProjectAudioTilePolicy>) {
        self.tile_policy = policy;
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Consume the project half of a session event without coupling the event
    /// log to a worker implementation.
    pub fn request_from_event(
        &mut self,
        event: &ProjectSessionEvent,
        recipe: ProjectAudioRenderRecipe,
    ) -> Option<ProjectAudioRenderJob> {
        let ProjectSessionEvent::ProjectPublished(publication) = event else {
            return None;
        };
        Some(self.request_render(publication.clone(), recipe))
    }

    pub fn request_render(
        &mut self,
        publication: ProjectPublication,
        recipe: ProjectAudioRenderRecipe,
    ) -> ProjectAudioRenderJob {
        if let Some(cancellation) = self.active_render_cancellation.take() {
            cancellation.cancel();
        }
        let controller_cancellation = RenderCancellation::new();
        self.active_render_cancellation = Some(controller_cancellation.clone());
        let tile_seed = self.tile_policy.and_then(|policy| {
            let control = self.renderer_control.as_ref()?;
            let previous_cohort = self.runtime.service().active_cohort()?;
            let previous = self
                .runtime
                .executable_plan(&previous_cohort.id.plan)
                .ok()?;
            let transport = control
                .publication_transport(self.transport_session.snapshot.transport)
                .ok()?;
            let relative = self
                .transport_session
                .snapshot
                .transport
                .frame
                .0
                .min(control.timeline().len());
            let playhead = control
                .timeline()
                .start
                .saturating_add(relative.min(i64::MAX as u64) as i64);
            Some(ProjectAudioTileSeed {
                previous_cohort,
                previous_plan: Arc::clone(&previous.descriptor),
                policy,
                publication_loop: transport.loop_region,
                playhead,
            })
        });
        self.desired = Some(DesiredTarget {
            generation: publication.generation,
            revision: publication.revisions.aggregate,
            change_set: publication.change_set.clone(),
        });
        self.transport_session
            .set_desired_revision(Some(publication.revisions.aggregate));
        self.invalidate_revision_bound_audition(publication.revisions.aggregate);
        self.local_failure = None;
        ProjectAudioRenderJob {
            publication,
            recipe,
            controller_cancellation,
            tile_seed,
        }
    }

    /// Scoped analysis/pattern PCM is revision-bound. A newer project request
    /// retires it explicitly so a stale Replace audition cannot mask the next
    /// coherent project cohort.
    fn invalidate_revision_bound_audition(&mut self, revision: u64) {
        let Some(status) = self.transport_session.snapshot.scoped_audition else {
            return;
        };
        if status.id.revision == revision {
            return;
        }
        if let Some(control) = &self.renderer_control {
            if let Err(error) = control.clear_timeline_audition(status.id) {
                self.diagnostics.push(error.to_string());
                return;
            }
            self.transport_session.clearing_audition = Some(status.id);
            if let Some(current) = &mut self.transport_session.snapshot.scoped_audition {
                current.phase = ScopedAuditionPhase::Pending;
            }
        } else {
            self.transport_session.set_scoped_audition(None);
        }
    }

    /// Accept one worker completion. Obsolete completions publish nothing.
    pub fn complete_render(
        &mut self,
        completion: ProjectAudioRenderCompletion,
    ) -> Result<ProjectAudioControllerEffect, ProjectAudioControllerError> {
        let desired = self
            .desired
            .as_ref()
            .ok_or(ProjectAudioControllerError::NoDesiredTarget)?;
        if completion.generation != desired.generation || completion.revision != desired.revision {
            return Ok(ProjectAudioControllerEffect::Superseded {
                generation: completion.generation,
                desired_generation: desired.generation,
            });
        }
        self.active_render_cancellation = None;
        self.local_failure = None;
        self.diagnostics = completion.diagnostics.clone();
        if let ProjectAudioRenderProducts::Tiles {
            rendered_tiles,
            reused_tiles,
            ..
        } = &completion.products
        {
            self.diagnostics.push(format!(
                "incremental bounce: rendered {rendered_tiles} tiles, reused {reused_tiles}"
            ));
        }

        if let Some(control) = &self.renderer_control {
            let next = &completion.executable.descriptor;
            if control.format() != next.format() || control.timeline() != next.extent() {
                return self.replace_host(completion);
            }
            if let Some(inflight) = self.runtime.status().publication_in_flight {
                if inflight.plan != next.id {
                    control.cancel_publication(&inflight);
                }
            }
        }

        self.runtime
            .submit_target(Arc::clone(&completion.executable))?;
        self.plan_generations
            .insert(completion.executable.id().clone(), completion.generation);
        if self.renderer_control.is_none() {
            let ProjectAudioRenderProducts::Whole { product } = &completion.products else {
                return Err(ProjectAudioControllerError::ColdTileBootstrap);
            };
            let (control, renderer) = self
                .runtime
                .bootstrap_renderer(completion.executable.id(), Arc::clone(product))?;
            self.renderer_control = Some(control);
            self.audible_generation = Some(completion.generation);
            self.sync_audible_cohort();
            self.transport_session.begin_host_handoff();
            return Ok(ProjectAudioControllerEffect::OpenHost(renderer));
        }

        let control = self
            .renderer_control
            .as_ref()
            .expect("checked persistent renderer control")
            .clone();
        let transport = control.publication_transport(self.transport_session.snapshot.transport)?;
        let action = match completion.products {
            ProjectAudioRenderProducts::Whole { product } => {
                self.runtime
                    .stage_whole_bounce(completion.executable.id(), product, transport)?
            }
            ProjectAudioRenderProducts::Tiles { draft, .. } => {
                self.runtime.stage_tile_cohort(draft, transport)?
            }
        };
        self.queue_action(action)?;
        Ok(ProjectAudioControllerEffect::None)
    }

    fn replace_host(
        &mut self,
        completion: ProjectAudioRenderCompletion,
    ) -> Result<ProjectAudioControllerEffect, ProjectAudioControllerError> {
        let ProjectAudioRenderProducts::Whole { product } = &completion.products else {
            return Err(ProjectAudioControllerError::StructuralTileReplacement);
        };
        let mut runtime = RenderRuntime::new();
        runtime.submit_target(Arc::clone(&completion.executable))?;
        let (control, renderer) =
            runtime.bootstrap_renderer(completion.executable.id(), Arc::clone(product))?;
        self.runtime = runtime;
        self.renderer_control = Some(control);
        self.active_render_cancellation = None;
        self.pending_action = None;
        self.plan_generations.clear();
        self.plan_generations
            .insert(completion.executable.id().clone(), completion.generation);
        self.audible_generation = Some(completion.generation);
        self.sync_audible_cohort();
        // A structural host replacement creates a fresh renderer; it cannot
        // carry the old renderer's pane-scoped signal. Keep UI/session status
        // honest rather than reporting an audition that is no longer audible.
        self.transport_session.set_scoped_audition(None);
        self.transport_session.begin_host_handoff();
        Ok(ProjectAudioControllerEffect::ReplaceHost(renderer))
    }

    fn sync_audible_cohort(&mut self) {
        let active = self
            .runtime
            .service()
            .active_cohort()
            .map(|cohort| cohort.id.clone());
        self.transport_session.set_audible_cohort(active);
    }

    /// Record a newest-generation worker error while preserving old audio.
    pub fn fail_render(&mut self, generation: u64, message: impl Into<String>) -> bool {
        let Some(desired) = &self.desired else {
            return false;
        };
        if desired.generation != generation {
            return false;
        }
        self.active_render_cancellation = None;
        let message = message.into();
        self.local_failure = Some((generation, message.clone()));
        self.diagnostics = vec![message];
        if let Some(target) = self.runtime.service().target_plan() {
            self.runtime.record_failure(RenderFailure::new(
                target.id.clone(),
                RenderFailureStage::ProductRender,
                self.diagnostics[0].clone(),
            ));
        }
        true
    }

    pub fn observe_host(&mut self, observation: ProjectAudioHostObservation) {
        self.transport_session.observe(observation.transport);
        self.preview_active = observation.preview_active;
    }

    pub fn transport_session(&self) -> &ProjectTransportSession {
        &self.transport_session
    }

    /// Complete an `OpenHost`/`ReplaceHost` effect before exposing transport
    /// controls again. Commands carrying the retired host identity are
    /// rejected across the handoff interval.
    pub fn bind_audio_host(
        &mut self,
        host: &AudioHost,
    ) -> Result<ProjectTransportSessionSnapshot, ProjectAudioControllerError> {
        self.transport_session.bind_host(&host.transport())?;
        self.preview_active = host.preview_active();
        Ok(self.transport_session.snapshot())
    }

    /// Apply the authoritative backend command. Timeline and analysis panes
    /// send commands here and observe [`Self::transport_session`]; they do not
    /// retain or operate their own transport handles.
    pub fn apply_transport_command(
        &mut self,
        host: &AudioHost,
        command: ProjectTransportCommand,
    ) -> Result<ProjectTransportSessionSnapshot, ProjectAudioControllerError> {
        self.transport_session.apply(&host.transport(), command)?;
        self.preview_active = host.preview_active();
        Ok(self.transport_session.snapshot())
    }

    /// Apply a pane's transport intent to the one project transport. Panes
    /// receive this callback; they never retain a competing handle.
    pub fn apply_transport_intent(
        &mut self,
        host: &AudioHost,
        intent: ProjectTransportIntent,
    ) -> Result<(), ProjectAudioControllerError> {
        let command = match intent {
            ProjectTransportIntent::Play => ProjectTransportCommand::Play,
            ProjectTransportIntent::Pause => ProjectTransportCommand::Pause,
            ProjectTransportIntent::Stop => ProjectTransportCommand::Stop,
            ProjectTransportIntent::TogglePlay => ProjectTransportCommand::TogglePlay,
            ProjectTransportIntent::Seek(frame) => ProjectTransportCommand::Seek(frame),
            ProjectTransportIntent::SetLoop { range, enabled } => {
                ProjectTransportCommand::ReplaceLoop {
                    range,
                    enabled,
                    locate_start: false,
                }
            }
            ProjectTransportIntent::ClearLoop => ProjectTransportCommand::ClearLoop,
        };
        self.apply_transport_command(host, command)?;
        Ok(())
    }

    /// Publish a timeline-aligned pane audition into the existing project
    /// renderer, optionally adopting its span as the global transport loop.
    pub fn start_scoped_audition(
        &mut self,
        host: &AudioHost,
        audition: Arc<TimelineAudition>,
        alignment: AuditionAlignment,
    ) -> Result<(), ProjectAudioControllerError> {
        self.transport_session.require_host(&host.transport())?;
        let control = self
            .renderer_control
            .as_ref()
            .ok_or(ProjectAudioControllerError::NoPersistentRenderer)?;
        let range = relative_audio_range(control.timeline(), audition.span)?;
        control.set_timeline_audition(Arc::clone(&audition))?;
        self.transport_session
            .set_scoped_audition(Some(ScopedAuditionStatus {
                id: audition.id,
                owner: audition.id.owner,
                subject: audition.subject,
                mix: audition.mix,
                span: audition.span,
                phase: ScopedAuditionPhase::Pending,
            }));
        match alignment {
            AuditionAlignment::PreserveTransport => {}
            AuditionAlignment::SeekToStart { play } => {
                self.transport_session.apply(
                    &host.transport(),
                    ProjectTransportCommand::Seek(range.start),
                )?;
                if play {
                    self.transport_session
                        .apply(&host.transport(), ProjectTransportCommand::Play)?;
                }
            }
            AuditionAlignment::LoopSpan { play } => {
                self.transport_session.apply(
                    &host.transport(),
                    ProjectTransportCommand::ReplaceLoop {
                        range,
                        enabled: true,
                        locate_start: true,
                    },
                )?;
                if play {
                    self.transport_session
                        .apply(&host.transport(), ProjectTransportCommand::Play)?;
                }
            }
        }
        self.preview_active = host.preview_active();
        Ok(())
    }

    /// Adoption boundary for the shared pattern-audition recipe adapter. The
    /// adapter already compiled and rendered through `DawEngineSchedule`; this
    /// method only verifies the pinned revision/window and publishes its PCM
    /// into the existing transport-scoped audition mailbox.
    pub fn adopt_frozen_engine_audition(
        &mut self,
        host: &AudioHost,
        pin: ProjectAudioAuditionPin,
        render: &DawEngineRender,
        owner: AuditionOwner,
        subject: AuditionSubject,
        mix: AuditionMix,
        alignment: AuditionAlignment,
    ) -> Result<Arc<TimelineAudition>, ProjectAudioControllerError> {
        let expected_revision = self.desired.as_ref().map(|target| target.revision);
        let actual_revision = pin.revision;
        if expected_revision != Some(actual_revision) {
            return Err(
                ProjectAudioControllerError::PatternAuditionRevisionMismatch {
                    expected: expected_revision,
                    actual: actual_revision,
                },
            );
        }
        if render.origin_frame != pin.span.start {
            return Err(ProjectAudioControllerError::PatternAuditionOriginMismatch {
                expected: pin.span.start,
                actual: render.origin_frame,
            });
        }
        let pcm = render.audio.shared_interleaved();
        let audition = Arc::new(TimelineAudition::new(
            TimelineAuditionId {
                owner,
                revision: actual_revision,
                content: canonical_pcm_digest(&pcm),
            },
            subject,
            mix,
            pin.span,
            render_format_stamp(render.audio.format()),
            pcm,
        )?);
        self.start_scoped_audition(host, Arc::clone(&audition), alignment)?;
        Ok(audition)
    }

    /// Stop only this owner. A stale pane cannot clear a newer pane's scoped
    /// audition or alter project transport as a side effect.
    pub fn stop_scoped_audition(
        &mut self,
        owner: AuditionOwner,
    ) -> Result<(), ProjectAudioControllerError> {
        let Some(status) = self.transport_session.snapshot.scoped_audition else {
            return Ok(());
        };
        if status.owner != owner {
            return Ok(());
        }
        self.stop_scoped_audition_exact(status.id)?;
        Ok(())
    }

    /// Compare-and-clear the exact audition token. This is the preferred pane
    /// teardown hook; an obsolete completion from the same owner cannot clear
    /// that owner's newer audition generation.
    pub fn stop_scoped_audition_exact(
        &mut self,
        audition: TimelineAuditionId,
    ) -> Result<bool, ProjectAudioControllerError> {
        let Some(status) = self.transport_session.snapshot.scoped_audition else {
            return Ok(false);
        };
        if status.id != audition {
            return Ok(false);
        }
        let control = self
            .renderer_control
            .as_ref()
            .ok_or(ProjectAudioControllerError::NoPersistentRenderer)?;
        control.clear_timeline_audition(audition)?;
        self.transport_session.clearing_audition = Some(audition);
        if let Some(status) = &mut self.transport_session.snapshot.scoped_audition {
            status.phase = ScopedAuditionPhase::Pending;
        }
        Ok(true)
    }

    /// Drive receipt acknowledgement and the next staged publication. This is
    /// cheap enough for a UI timer; it never renders or waits.
    pub fn tick(
        &mut self,
        observation: ProjectAudioHostObservation,
    ) -> Result<Option<PublicationCompletion>, ProjectAudioControllerError> {
        self.observe_host(observation);
        let Some(control) = self.renderer_control.as_ref().cloned() else {
            return Ok(None);
        };

        if let Some(receipt) = control.drain_audition_receipt() {
            match receipt.active {
                Some(active) => {
                    if let Some(status) = &mut self.transport_session.snapshot.scoped_audition {
                        if status.id == active {
                            status.phase = ScopedAuditionPhase::Active;
                            self.transport_session.clearing_audition = None;
                        }
                    }
                }
                None => {
                    let clears_current = self
                        .transport_session
                        .snapshot
                        .scoped_audition
                        .is_some_and(|status| {
                            self.transport_session.clearing_audition == Some(status.id)
                        });
                    if clears_current {
                        self.transport_session.set_scoped_audition(None);
                        self.transport_session.clearing_audition = None;
                    }
                }
            }
        }

        self.retry_pending(&control)?;
        let completion = self.runtime.poll_publication(&control)?;
        if let Some(PublicationCompletion {
            outcome: PublicationCompletionOutcome::Activated { active, .. },
        }) = &completion
        {
            self.audible_generation = self.plan_generations.get(&active.plan).copied();
            self.transport_session
                .set_audible_cohort(Some(active.clone()));
        }
        let action = self
            .runtime
            .observe_transport(&control, self.transport_session.snapshot.transport)?;
        self.queue_action(action)?;
        Ok(completion)
    }

    fn retry_pending(
        &mut self,
        control: &CohortRendererControl,
    ) -> Result<(), ProjectAudioControllerError> {
        let Some(action) = self.pending_action.take() else {
            return Ok(());
        };
        if let Err(error) = control.arm_action(&action) {
            if matches!(error, RenderRuntimeError::PublicationMailboxBusy) {
                self.pending_action = Some(action);
                return Ok(());
            }
            if let Some(cohort) = action.cohort() {
                self.runtime
                    .reject_publication(&cohort.id, error.to_string())?;
            }
            return Err(error.into());
        }
        Ok(())
    }

    fn queue_action(
        &mut self,
        action: PublicationAction,
    ) -> Result<(), ProjectAudioControllerError> {
        if matches!(action, PublicationAction::None) {
            return Ok(());
        }
        if self.pending_action.is_some() {
            return Err(ProjectAudioControllerError::ControllerActionAlreadyPending);
        }
        self.pending_action = Some(action);
        if let Some(control) = self.renderer_control.as_ref().cloned() {
            self.retry_pending(&control)?;
        }
        Ok(())
    }

    pub fn status(&self) -> ProjectAudioStatus {
        let desired = self.desired.as_ref();
        let service = self.runtime.status();
        let render = if let Some((generation, _)) = &self.local_failure {
            RenderActivity::Failed {
                generation: *generation,
            }
        } else {
            match service.availability {
                RenderAvailability::Empty => {
                    desired.map_or(RenderActivity::Idle, |target| RenderActivity::Rendering {
                        generation: target.generation,
                        revision: target.revision,
                    })
                }
                RenderAvailability::Priming { target } => RenderActivity::Rendering {
                    generation: desired.map_or(0, |target| target.generation),
                    revision: target.revisions.aggregate,
                },
                RenderAvailability::Ready { active }
                    if desired.is_some_and(|target| {
                        self.audible_generation != Some(target.generation)
                    }) =>
                {
                    let target = desired.expect("guard requires desired target");
                    RenderActivity::Updating {
                        generation: target.generation,
                        revision: target.revision,
                        audible_revision: active.plan.revisions.aggregate,
                        candidate_ready: false,
                        publication_in_flight: false,
                    }
                }
                RenderAvailability::Ready { active } => RenderActivity::Ready {
                    revision: active.plan.revisions.aggregate,
                },
                RenderAvailability::Stale {
                    active,
                    target,
                    candidate_ready,
                    publication_in_flight,
                } => RenderActivity::Updating {
                    generation: desired.map_or(0, |target| target.generation),
                    revision: target.revisions.aggregate,
                    audible_revision: active.plan.revisions.aggregate,
                    candidate_ready,
                    publication_in_flight,
                },
                RenderAvailability::Updating {
                    active,
                    candidate_ready,
                    publication_in_flight,
                } => RenderActivity::Updating {
                    generation: desired.map_or(0, |target| target.generation),
                    revision: desired
                        .map_or(active.plan.revisions.aggregate, |target| target.revision),
                    audible_revision: active.plan.revisions.aggregate,
                    candidate_ready,
                    publication_in_flight,
                },
                RenderAvailability::Failed {
                    active: _,
                    target: _,
                    failure: _,
                } => RenderActivity::Failed {
                    generation: desired.map_or(0, |target| target.generation),
                },
            }
        };
        ProjectAudioStatus {
            transport: self.transport_session.snapshot.transport,
            render,
            preview_active: self.preview_active,
            scoped_audition: self.transport_session.snapshot.scoped_audition,
            diagnostic: self.diagnostics.last().cloned(),
        }
    }

    /// Publish cheap status and diagnostics into the session event stream.
    pub fn publish_session_state(&self, session: &mut ProjectSession) {
        session.set_audio_status(self.status());
        session.replace_diagnostics(self.diagnostics.clone());
    }

    /// Pin the last controller-acknowledged audible cohort. Interactive callers
    /// should prefer [`Self::reconcile_and_pin_audible_export`] so a realtime
    /// swap whose receipt is waiting cannot be mistaken for the old cohort.
    pub fn pin_audible_export(
        &self,
        scope: RenderScope,
        span: RenderSpan,
        tail: OutputTailPolicy,
    ) -> Result<ExportPin, ProjectAudioControllerError> {
        Ok(self.runtime.pin_active_export(scope, span, tail)?)
    }

    /// Sample the owned host once, drain publication/audition receipts, then
    /// pin the cohort that is actually audible after reconciliation.
    pub fn reconcile_and_pin_audible_export(
        &mut self,
        host: &AudioHost,
        scope: RenderScope,
        span: RenderSpan,
        tail: OutputTailPolicy,
    ) -> Result<ExportPin, ProjectAudioControllerError> {
        self.transport_session.require_host(&host.transport())?;
        self.tick(host.snapshot().into())?;
        self.sync_audible_cohort();
        Ok(self.runtime.pin_active_export(scope, span, tail)?)
    }

    /// Pin a named immutable plan for a fresh master, bus, or track-stem
    /// execution. This is distinct from [`Self::pin_audible_export`]: callers
    /// must choose explicitly whether export follows the audible cohort or a
    /// newer compiled target waiting for loop-wrap publication.
    pub fn pin_plan_export(
        &self,
        plan: &RenderPlanId,
        scope: RenderScope,
        span: RenderSpan,
        tail: OutputTailPolicy,
    ) -> Result<ExportPin, ProjectAudioControllerError> {
        Ok(self.runtime.pin_plan_export(plan, scope, span, tail)?)
    }

    /// Pin the newest compiled target without requiring it to have played or
    /// crossed a loop publication boundary. UI adapters pass the generation
    /// and revision captured with their async request; late completions are
    /// rejected instead of exporting the older audible cohort.
    pub fn pin_current_export(
        &self,
        expected_generation: u64,
        expected_revision: u64,
        scope: RenderScope,
        span: RenderSpan,
        tail: OutputTailPolicy,
    ) -> Result<ExportPin, ProjectAudioControllerError> {
        let desired = self
            .desired
            .as_ref()
            .ok_or(ProjectAudioControllerError::NoDesiredTarget)?;
        if desired.generation != expected_generation || desired.revision != expected_revision {
            return Err(ProjectAudioControllerError::StaleExportRequest {
                expected_generation: desired.generation,
                actual_generation: expected_generation,
                expected_revision: desired.revision,
                actual_revision: expected_revision,
            });
        }
        let target = self
            .runtime
            .service()
            .target_plan()
            .filter(|plan| plan.id.revisions.aggregate == expected_revision)
            .filter(|plan| self.plan_generations.get(&plan.id) == Some(&expected_generation))
            .ok_or(
                ProjectAudioControllerError::CurrentExportTargetNotCompiled {
                    generation: expected_generation,
                    revision: expected_revision,
                },
            )?;
        Ok(self
            .runtime
            .pin_plan_export(&target.id, scope, span, tail)?)
    }

    /// Capture a self-contained worker job for the newest compiled project
    /// target. Playback need not have started and the target need not yet be
    /// audible. Call [`ProjectAudioExportJob::execute`] off-thread, then pass
    /// its result to [`Self::complete_current_export`] on the controller thread.
    pub fn request_current_export(
        &self,
        scope: RenderScope,
        span: RenderSpan,
        tail: OutputTailPolicy,
    ) -> Result<ProjectAudioExportJob, ProjectAudioControllerError> {
        let desired = self
            .desired
            .as_ref()
            .ok_or(ProjectAudioControllerError::NoDesiredTarget)?;
        let pin =
            self.pin_current_export(desired.generation, desired.revision, scope, span, tail)?;
        let executable = self.runtime.executable_plan(&pin.plan.id)?;
        Ok(ProjectAudioExportJob {
            generation: desired.generation,
            revision: desired.revision,
            pin,
            executable,
        })
    }

    /// Accept only a worker completion that still names the current project
    /// generation and revision. A render finishing after an edit is rejected
    /// and cannot be mislabeled or written as the newer revision.
    pub fn complete_current_export(
        &self,
        completion: ProjectAudioExportCompletion,
    ) -> Result<RuntimeRenderedAudio, ProjectAudioControllerError> {
        let desired = self
            .desired
            .as_ref()
            .ok_or(ProjectAudioControllerError::NoDesiredTarget)?;
        if desired.generation != completion.generation || desired.revision != completion.revision {
            return Err(ProjectAudioControllerError::StaleExportRequest {
                expected_generation: desired.generation,
                actual_generation: completion.generation,
                expected_revision: desired.revision,
                actual_revision: completion.revision,
            });
        }
        if !matches!(&completion.integrity, ProjectAudioExportIntegrity::Verified) {
            return Err(ProjectAudioControllerError::ExportIntegrityRefused(
                completion.integrity,
            ));
        }
        Ok(completion.rendered)
    }

    /// Pin exactly the products audible at request time for a later scoped
    /// audition. The returned Arc graph remains valid across target renders and
    /// loop-boundary cohort swaps.
    pub fn pin_audible_audition(
        &self,
        scope: RenderScope,
        span: RenderSpan,
    ) -> Result<AuditionPin, ProjectAudioControllerError> {
        Ok(self.runtime.pin_active_audition(scope, span)?)
    }

    pub fn reconcile_and_pin_audible_audition(
        &mut self,
        host: &AudioHost,
        scope: RenderScope,
        span: RenderSpan,
    ) -> Result<AuditionPin, ProjectAudioControllerError> {
        self.transport_session.require_host(&host.transport())?;
        self.tick(host.snapshot().into())?;
        self.sync_audible_cohort();
        Ok(self.runtime.pin_active_audition(scope, span)?)
    }

    pub fn render_pinned_audition(
        &self,
        pin: &AuditionPin,
        owner: AuditionOwner,
        subject: AuditionSubject,
        mix: AuditionMix,
    ) -> Result<Arc<TimelineAudition>, ProjectAudioControllerError> {
        Ok(self.runtime.render_audition_pin(pin, owner, subject, mix)?)
    }

    pub fn render_export(
        &self,
        pin: &ExportPin,
        cancellation: &RenderCancellation,
    ) -> Result<RuntimeRenderedAudio, ProjectAudioControllerError> {
        Ok(self.runtime.render_export_pin(pin, cancellation)?)
    }

    pub fn desired_change_set(&self) -> Option<&ChangeSet> {
        self.desired
            .as_ref()
            .and_then(|target| target.change_set.as_ref())
    }
}

fn relative_audio_range(
    timeline: RenderSpan,
    span: RenderSpan,
) -> Result<FrameRange, ProjectAudioControllerError> {
    if !timeline.contains_span(span) {
        return Err(ProjectAudioControllerError::AuditionOutsideTimeline {
            audition: span,
            timeline,
        });
    }
    let start = u64::try_from(span.start - timeline.start)
        .map_err(|_| ProjectAudioControllerError::TransportCoordinateOverflow)?;
    let end = u64::try_from(span.end - timeline.start)
        .map_err(|_| ProjectAudioControllerError::TransportCoordinateOverflow)?;
    FrameRange::new(ProjectFrame(start), ProjectFrame(end)).map_err(Into::into)
}

#[derive(Debug)]
pub enum ProjectAudioControllerError {
    EmptyArrangement,
    Cancelled,
    MissingSnapshotDigest,
    MissingEngineConfigurationDigest,
    NoDesiredTarget,
    ControllerActionAlreadyPending,
    NoPersistentRenderer,
    ColdTileBootstrap,
    StructuralTileReplacement,
    PatternAuditionRevisionMismatch {
        expected: Option<u64>,
        actual: u64,
    },
    PatternAuditionOriginMismatch {
        expected: i64,
        actual: i64,
    },
    StaleExportRequest {
        expected_generation: u64,
        actual_generation: u64,
        expected_revision: u64,
        actual_revision: u64,
    },
    CurrentExportTargetNotCompiled {
        generation: u64,
        revision: u64,
    },
    NoTransportSelection,
    TransportSelectionOutsideTimeline {
        selection: FrameRange,
        length: ProjectFrame,
    },
    TransportHostNotBound,
    TransportHostHandoffPending,
    TransportHostMismatch {
        expected: TransportSessionId,
        actual: TransportSessionId,
    },
    TransportHostRetired(TransportSessionId),
    ExportIntegrityRefused(ProjectAudioExportIntegrity),
    TileUnsupported(String),
    AuditionOutsideTimeline {
        audition: RenderSpan,
        timeline: RenderSpan,
    },
    TransportCoordinateOverflow,
    Plan(String),
    Audio(crate::audio::AudioError),
    Engine(crate::daw_engine::DawEngineError),
    Tile(RenderTileError),
    Runtime(RenderRuntimeError),
}

impl fmt::Display for ProjectAudioControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyArrangement => formatter.write_str("project arrangement is empty"),
            Self::Cancelled => formatter.write_str("project audio render was cancelled"),
            Self::MissingSnapshotDigest => {
                formatter.write_str("render publication has no exact snapshot digest")
            }
            Self::MissingEngineConfigurationDigest => {
                formatter.write_str("render publication has no exact engine configuration digest")
            }
            Self::NoDesiredTarget => formatter.write_str("audio controller has no desired target"),
            Self::ControllerActionAlreadyPending => {
                formatter.write_str("audio controller already has a pending host action")
            }
            Self::NoPersistentRenderer => {
                formatter.write_str("audio controller has no persistent renderer")
            }
            Self::ColdTileBootstrap => {
                formatter.write_str("cold project playback requires a whole bounce")
            }
            Self::StructuralTileReplacement => {
                formatter.write_str("structural host replacement requires a whole bounce")
            }
            Self::PatternAuditionRevisionMismatch { expected, actual } => write!(
                formatter,
                "pattern audition revision {actual} does not match controller target {expected:?}"
            ),
            Self::PatternAuditionOriginMismatch { expected, actual } => write!(
                formatter,
                "pattern audition origin {actual} differs from pinned loop start {expected}"
            ),
            Self::StaleExportRequest {
                expected_generation,
                actual_generation,
                expected_revision,
                actual_revision,
            } => write!(
                formatter,
                "export request generation/revision {actual_generation}/{actual_revision} is stale; current target is {expected_generation}/{expected_revision}"
            ),
            Self::CurrentExportTargetNotCompiled {
                generation,
                revision,
            } => write!(
                formatter,
                "export target generation/revision {generation}/{revision} is not compiled yet"
            ),
            Self::NoTransportSelection => {
                formatter.write_str("project transport has no selection to adopt as a loop")
            }
            Self::TransportSelectionOutsideTimeline { selection, length } => write!(
                formatter,
                "transport selection {}..{} exceeds timeline length {}",
                selection.start.0, selection.end.0, length.0
            ),
            Self::TransportHostNotBound => {
                formatter.write_str("project transport session has no bound audio host")
            }
            Self::TransportHostHandoffPending => formatter
                .write_str("project transport host replacement has not been bound yet"),
            Self::TransportHostMismatch { expected, actual } => write!(
                formatter,
                "audio host {actual:?} does not match project transport host {expected:?}"
            ),
            Self::TransportHostRetired(host) => write!(
                formatter,
                "audio host {host:?} was retired by the pending transport handoff"
            ),
            Self::ExportIntegrityRefused(integrity) => write!(
                formatter,
                "export refused because {} material diagnostic(s) would make the output incomplete",
                integrity.issues().len()
            ),
            Self::TileUnsupported(message) => {
                write!(formatter, "incremental bounce unavailable: {message}")
            }
            Self::AuditionOutsideTimeline { .. } => {
                formatter.write_str("scoped audition lies outside project playback")
            }
            Self::TransportCoordinateOverflow => {
                formatter.write_str("scoped audition coordinate overflows project transport")
            }
            Self::Plan(message) => write!(formatter, "render plan: {message}"),
            Self::Audio(error) => error.fmt(formatter),
            Self::Engine(error) => error.fmt(formatter),
            Self::Tile(error) => error.fmt(formatter),
            Self::Runtime(error) => error.fmt(formatter),
        }
    }
}

impl Error for ProjectAudioControllerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::Tile(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::Audio(error) => Some(error),
            _ => None,
        }
    }
}

impl From<crate::daw_engine::DawEngineError> for ProjectAudioControllerError {
    fn from(error: crate::daw_engine::DawEngineError) -> Self {
        Self::Engine(error)
    }
}

impl From<RenderTileError> for ProjectAudioControllerError {
    fn from(error: RenderTileError) -> Self {
        Self::Tile(error)
    }
}

impl From<crate::audio::AudioError> for ProjectAudioControllerError {
    fn from(error: crate::audio::AudioError) -> Self {
        Self::Audio(error)
    }
}

impl From<RenderRuntimeError> for ProjectAudioControllerError {
    fn from(error: RenderRuntimeError) -> Self {
        Self::Runtime(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    use crate::audio::{
        AudioFormat, FrameRange, PcmRenderer, ProjectAudio, ProjectRenderer, TransportSource,
    };
    use crate::daw_engine::AssetPcmMap;
    use crate::daw_project::{DawProject, ProjectDomain};
    use crate::live_project::LiveProjectSnapshot;
    use crate::mixer::BusKind;
    use crate::render_products::{ProductPartition, RenderProductKey};
    use crate::render_runtime::whole_bounce_boundary_recipe;

    fn digest(byte: u8) -> ExactDigest {
        ExactDigest::new([byte; 32])
    }

    fn transport_fixture() -> (
        ProjectTransportSession,
        TransportHandle,
        TransportSource<PcmRenderer>,
    ) {
        let format = AudioFormat::new(48_000, 1).unwrap();
        let audio = ProjectAudio::from_interleaved(format, vec![0.0; 16]).unwrap();
        let (handle, source) = TransportSource::new(PcmRenderer::new(audio));
        let mut session = ProjectTransportSession::default();
        session.bind_host(&handle).unwrap();
        (session, handle, source)
    }

    #[test]
    fn transport_selection_loop_and_click_locate_have_distinct_atomic_semantics() {
        let (mut session, handle, mut source) = transport_fixture();
        let old = FrameRange::new(ProjectFrame(2), ProjectFrame(5)).unwrap();
        session
            .apply(
                &handle,
                ProjectTransportCommand::ReplaceSelectionAndLoop(old),
            )
            .unwrap();
        assert_eq!(session.snapshot().selection, Some(old));
        assert_eq!(session.snapshot().transport.loop_region, Some(old));

        session
            .apply(&handle, ProjectTransportCommand::Seek(ProjectFrame(9)))
            .unwrap();
        let located = session.snapshot();
        assert_eq!(located.selection, Some(old));
        assert_eq!(located.transport.loop_region, Some(old));
        assert!(!located.transport.loop_enabled);
        assert_eq!(located.transport.frame, ProjectFrame(9));

        let replacement = FrameRange::new(ProjectFrame(10), ProjectFrame(14)).unwrap();
        session
            .apply(
                &handle,
                ProjectTransportCommand::ReplaceSelectionAndLoop(replacement),
            )
            .unwrap();
        session
            .apply(&handle, ProjectTransportCommand::Play)
            .unwrap();
        assert_eq!(source.next(), Some(0.0));
        assert_eq!(handle.snapshot().frame, ProjectFrame(11));
    }

    #[test]
    fn stale_host_poll_cannot_roll_back_a_pending_semantic_locate() {
        let (mut session, handle, mut source) = transport_fixture();
        session
            .apply(&handle, ProjectTransportCommand::Play)
            .unwrap();
        source.next();
        session.observe(handle.snapshot());

        let captured_before_command = handle.snapshot();
        let range = FrameRange::new(ProjectFrame(8), ProjectFrame(12)).unwrap();
        session
            .apply(
                &handle,
                ProjectTransportCommand::ReplaceSelectionAndLoop(range),
            )
            .unwrap();

        session.observe(captured_before_command);
        assert_eq!(session.snapshot().transport.frame, range.start);
        assert_eq!(session.snapshot().transport.loop_region, Some(range));

        // Control changes are visible immediately, while the realtime
        // publication still reports the preceding frame. Polling that mixed
        // snapshot must preserve the requested range start and Playing mode.
        let before_ack = handle.snapshot();
        assert_ne!(before_ack.frame, range.start);
        session.observe(before_ack);
        assert_eq!(session.snapshot().transport.frame, range.start);
        assert_eq!(session.snapshot().transport.mode, TransportMode::Playing);
        assert_eq!(session.snapshot().transport.loop_region, Some(range));

        assert_eq!(source.next(), Some(0.0));
        session.observe(handle.snapshot());
        assert_eq!(session.snapshot().transport.frame, ProjectFrame(9));
        assert_eq!(session.snapshot().transport.mode, TransportMode::Playing);
    }

    #[test]
    fn stale_host_poll_cannot_resurrect_an_old_loop_start_after_click_locate() {
        let (mut session, handle, mut source) = transport_fixture();
        let old = FrameRange::new(ProjectFrame(2), ProjectFrame(5)).unwrap();
        session
            .apply(
                &handle,
                ProjectTransportCommand::ReplaceSelectionAndLoop(old),
            )
            .unwrap();
        session
            .apply(&handle, ProjectTransportCommand::Play)
            .unwrap();
        source.next();
        session.observe(handle.snapshot());

        session
            .apply(&handle, ProjectTransportCommand::Seek(ProjectFrame(11)))
            .unwrap();
        session.observe(handle.snapshot());
        let pending = session.snapshot().transport;
        assert_eq!(pending.frame, ProjectFrame(11));
        assert_eq!(pending.mode, TransportMode::Playing);
        assert_eq!(pending.loop_region, Some(old));
        assert!(!pending.loop_enabled);

        assert_eq!(source.next(), Some(0.0));
        session.observe(handle.snapshot());
        assert_eq!(session.snapshot().transport.frame, ProjectFrame(12));
        assert_eq!(session.snapshot().transport.mode, TransportMode::Playing);
    }

    #[test]
    fn wrapping_transport_revision_order_is_explicit() {
        assert!(!revision_advanced_after(7, 7));
        assert!(revision_advanced_after(8, 7));
        assert!(!revision_advanced_after(6, 7));
        assert!(revision_advanced_after(0, u64::MAX));
    }

    #[test]
    fn desired_state_toggle_and_host_handoff_are_race_safe() {
        let (mut session, handle, mut source) = transport_fixture();
        session
            .apply(&handle, ProjectTransportCommand::Play)
            .unwrap();
        session
            .apply(&handle, ProjectTransportCommand::TogglePlay)
            .unwrap();
        assert_eq!(source.next(), Some(0.0));
        assert_eq!(handle.snapshot().mode, TransportMode::Paused);

        let format = AudioFormat::new(48_000, 1).unwrap();
        let replacement_audio = ProjectAudio::from_interleaved(format, vec![0.0; 16]).unwrap();
        let (replacement, _source) = TransportSource::new(PcmRenderer::new(replacement_audio));
        assert!(matches!(
            session.bind_host(&replacement),
            Err(ProjectAudioControllerError::TransportHostMismatch { .. })
        ));
        session.begin_host_handoff();
        assert!(matches!(
            session.apply(&handle, ProjectTransportCommand::Play),
            Err(ProjectAudioControllerError::TransportHostHandoffPending)
        ));
        assert!(matches!(
            session.bind_host(&handle),
            Err(ProjectAudioControllerError::TransportHostRetired(_))
        ));
        session.bind_host(&replacement).unwrap();
    }

    fn project(revision: u64) -> DawProject {
        let mut project = DawProject::new("controller test", 48_000, 120.0).unwrap();
        for index in 0..revision {
            project
                .transact(
                    format!("revision {index}"),
                    project.revisions().aggregate,
                    BTreeSet::from([ProjectDomain::Mixer]),
                    |state| {
                        state
                            .domains
                            .mixer
                            .add_bus(BusKind::Group, format!("Group {index}"))
                            .map(|_| ())
                            .map_err(|error| error.to_string())
                    },
                )
                .unwrap();
        }
        project
    }

    fn request(
        controller: &mut ProjectAudioController,
        generation: u64,
        project: DawProject,
        identity_byte: u8,
    ) -> ProjectAudioRenderJob {
        let revisions = project.revisions();
        let publication = ProjectPublication {
            generation,
            revisions,
            snapshot: LiveProjectSnapshot {
                project: Arc::new(project),
                pcm: Arc::new(AssetPcmMap::new()),
                sample_pcm: Arc::new(BTreeMap::new()),
            },
            change_set: Some(ChangeSet::default()),
        };
        controller.request_render(
            publication,
            ProjectAudioRenderRecipe {
                extent: RenderSpan::new(0, 4).unwrap(),
                engine: Arc::new(DawEngineConfig::default()),
                stamp: ProjectAudioPlanStamp {
                    project_namespace: 77,
                    snapshot: digest(identity_byte),
                    engine_abi: 1,
                    engine_configuration: digest(240),
                    dependencies: Vec::new(),
                    determinism: DeterminismGrade::BitExact,
                    tileability: Tileability::Stateless,
                },
            },
        )
    }

    fn completion(
        job: &ProjectAudioRenderJob,
        sample: f32,
        identity_byte: u8,
    ) -> ProjectAudioRenderCompletion {
        let cancellation = RenderCancellation::new();
        let mut completion = job.execute(&cancellation).unwrap();
        let descriptor = &completion.executable.descriptor;
        let key = RenderProductKey::new(
            descriptor.id.clone(),
            RenderScope::Master,
            descriptor.extent(),
            ProductPartition::WholeBounce,
            whole_bounce_boundary_recipe(),
        )
        .unwrap();
        let samples = vec![sample; descriptor.extent().len() as usize * 2];
        completion.products = ProjectAudioRenderProducts::Whole {
            product: Arc::new(
                RenderProduct::new(digest(identity_byte), key, samples.into()).unwrap(),
            ),
        };
        completion
    }

    fn request_with_changes(
        controller: &mut ProjectAudioController,
        generation: u64,
        project: DawProject,
        identity_byte: u8,
        changes: ChangeSet,
    ) -> ProjectAudioRenderJob {
        let mut job = request(controller, generation, project, identity_byte);
        job.publication.change_set = Some(changes.clone());
        if let Some(desired) = &mut controller.desired {
            desired.change_set = Some(changes);
        }
        job
    }

    #[test]
    fn rapid_edits_discard_obsolete_worker_completions() {
        let mut controller = ProjectAudioController::new();
        let first = request(&mut controller, 1, project(1), 1);
        let first_completion = completion(&first, 0.1, 11);
        let second = request(&mut controller, 2, project(2), 2);

        let obsolete = controller.complete_render(first_completion).unwrap();
        assert!(matches!(
            obsolete,
            ProjectAudioControllerEffect::Superseded {
                generation: 1,
                desired_generation: 2
            }
        ));
        let current = controller
            .complete_render(completion(&second, 0.2, 12))
            .unwrap();
        assert!(matches!(current, ProjectAudioControllerEffect::OpenHost(_)));
        assert!(matches!(
            controller.status().render,
            RenderActivity::Ready { revision: 2 }
        ));
    }

    #[test]
    fn newer_project_revision_invalidates_revision_bound_scoped_audition() {
        let mut controller = ProjectAudioController::new();
        let first = request(&mut controller, 1, project(1), 1);
        controller
            .complete_render(completion(&first, 0.1, 11))
            .unwrap();
        let id = TimelineAuditionId {
            owner: AuditionOwner {
                namespace: 8,
                local: 3,
            },
            revision: 1,
            content: digest(44),
        };
        controller
            .transport_session
            .set_scoped_audition(Some(ScopedAuditionStatus {
                id,
                owner: id.owner,
                subject: AuditionSubject::Residual,
                mix: AuditionMix::Replace,
                span: RenderSpan::new(0, 4).unwrap(),
                phase: ScopedAuditionPhase::Active,
            }));

        let _second = request(&mut controller, 2, project(2), 2);
        assert_eq!(controller.transport_session.clearing_audition, Some(id));
        assert_eq!(
            controller
                .transport_session
                .snapshot
                .scoped_audition
                .unwrap()
                .phase,
            ScopedAuditionPhase::Pending
        );
    }

    #[test]
    fn current_export_needs_no_playback_and_rejects_late_worker_completion() {
        let mut controller = ProjectAudioController::new();
        let first = request(&mut controller, 1, project(1), 1);
        let span = RenderSpan::new(0, 4).unwrap();
        assert!(matches!(
            controller.request_current_export(RenderScope::Master, span, OutputTailPolicy::Crop),
            Err(
                ProjectAudioControllerError::CurrentExportTargetNotCompiled {
                    generation: 1,
                    revision: 1
                }
            )
        ));

        // Accepting the compiled target opens a renderer for the host adapter,
        // but this test never renders a transport frame or publishes a loop.
        assert!(matches!(
            controller
                .complete_render(completion(&first, 0.1, 11))
                .unwrap(),
            ProjectAudioControllerEffect::OpenHost(_)
        ));
        let accepted_job = controller
            .request_current_export(RenderScope::Master, span, OutputTailPolicy::Crop)
            .unwrap();
        let accepted_completion = accepted_job.execute(&RenderCancellation::new()).unwrap();
        assert_eq!(
            accepted_completion.integrity(),
            &ProjectAudioExportIntegrity::Verified
        );
        let accepted = controller
            .complete_current_export(accepted_completion)
            .unwrap();
        assert_eq!(accepted.plan.revisions.aggregate, 1);
        assert_eq!(accepted.origin_frame, span.start);

        let stale_job = controller
            .request_current_export(RenderScope::Master, span, OutputTailPolicy::Crop)
            .unwrap();
        let stale_completion = stale_job.execute(&RenderCancellation::new()).unwrap();
        let _second = request(&mut controller, 2, project(2), 2);
        assert!(matches!(
            controller.complete_current_export(stale_completion),
            Err(ProjectAudioControllerError::StaleExportRequest {
                expected_generation: 2,
                actual_generation: 1,
                expected_revision: 2,
                actual_revision: 1
            })
        ));
    }

    #[test]
    fn missing_or_rate_mismatched_material_cannot_complete_as_successful_export() {
        let mut controller = ProjectAudioController::new();
        let first = request(&mut controller, 1, project(1), 1);
        controller
            .complete_render(completion(&first, 0.1, 11))
            .unwrap();
        let job = controller
            .request_current_export(
                RenderScope::Master,
                RenderSpan::new(0, 4).unwrap(),
                OutputTailPolicy::Crop,
            )
            .unwrap();
        let valid = job.execute(&RenderCancellation::new()).unwrap();

        let refused = |diagnostic| {
            let mut completion = valid.clone();
            completion.diagnostics = ProjectAudioExportDiagnostics {
                engine: Arc::from([diagnostic]),
                render: Arc::from([]),
            };
            completion.integrity = export_integrity(&completion.diagnostics);
            completion
        };
        let missing = refused(EngineDiagnostic::PcmNotSupplied {
            asset: crate::assets::AssetId(9),
            arrangement_alias: crate::arrangement::AssetId::from_raw(9),
        });
        assert!(matches!(
            controller.complete_current_export(missing),
            Err(ProjectAudioControllerError::ExportIntegrityRefused(_))
        ));

        let mismatch = refused(EngineDiagnostic::PcmMetadataMismatch {
            asset: crate::assets::AssetId(9),
            arrangement_alias: crate::arrangement::AssetId::from_raw(9),
            registry_sample_rate: 48_000,
            pcm_sample_rate: 44_100,
            registry_channels: 2,
            pcm_channels: 2,
            registry_frames: 4,
            pcm_frames: 4,
        });
        assert!(matches!(
            controller.complete_current_export(mismatch),
            Err(ProjectAudioControllerError::ExportIntegrityRefused(_))
        ));

        assert!(controller.complete_current_export(valid).is_ok());
    }

    #[test]
    fn export_integrity_policy_refuses_ignored_authored_audio_but_allows_default_master_route() {
        assert!(!material_render_diagnostic(
            &RenderDiagnostic::TrackRoutedToMaster {
                track: crate::arrangement::TrackId::from_raw(1),
            }
        ));
        assert!(material_render_diagnostic(
            &RenderDiagnostic::ArrangementAutomationRegionExternal {
                clip: crate::arrangement::ClipId::from_raw(2),
                parameter: 3,
            }
        ));
        assert!(material_engine_diagnostic(
            &EngineDiagnostic::ClipBusOverrideUnsupported {
                clip: crate::arrangement::ClipId::from_raw(2),
                requested: crate::mixer::BusId::from_raw(4),
                rendered_to: crate::mixer::BusId::from_raw(5),
            }
        ));
        // Duplicate consumers are deterministic, but suppression discards an
        // authored instrument route and therefore cannot be export-verified.
        assert!(material_engine_diagnostic(
            &EngineDiagnostic::DuplicateSamplerConsumerSuppressed {
                sample_alias: 6,
                retained_instrument: 7,
                suppressed_instrument: 8,
            }
        ));
    }

    #[test]
    fn controller_adopts_tiles_then_reuses_unaffected_ranges() {
        let mut controller = ProjectAudioController::new();
        controller.set_tile_policy(Some(ProjectAudioTilePolicy {
            grid: TileGrid::new(2).unwrap(),
            maximum_context_frames: 0,
        }));
        let first = request(&mut controller, 1, project(1), 1);
        let mut renderer = match controller
            .complete_render(completion(&first, 0.1, 51))
            .unwrap()
        {
            ProjectAudioControllerEffect::OpenHost(renderer) => renderer,
            _ => panic!("first render must bootstrap whole-bounce playback"),
        };

        let mut full_change = ChangeSet::default();
        full_change.touch(ProjectDomain::Mixer).invalidate_range(
            crate::mixer::BusId::from_raw(1),
            crate::change_set::AudioRange::new(0, 4).unwrap(),
        );
        let second = request_with_changes(&mut controller, 2, project(2), 2, full_change);
        let second_completion = second.execute(&second.cancellation()).unwrap();
        assert!(matches!(
            &second_completion.products,
            ProjectAudioRenderProducts::Tiles {
                rendered_tiles: 2,
                reused_tiles: 0,
                ..
            }
        ));
        controller.complete_render(second_completion).unwrap();
        let observation = ProjectAudioHostObservation::default();
        let mut frame = [0.0; 2];
        renderer.render_interleaved(&mut frame);
        controller.tick(observation).unwrap();

        let mut local_change = ChangeSet::default();
        local_change.touch(ProjectDomain::Mixer).invalidate_range(
            crate::mixer::BusId::from_raw(1),
            crate::change_set::AudioRange::new(0, 2).unwrap(),
        );
        let third = request_with_changes(&mut controller, 3, project(3), 3, local_change);
        let third_completion = third.execute(&third.cancellation()).unwrap();
        assert!(matches!(
            &third_completion.products,
            ProjectAudioRenderProducts::Tiles {
                rendered_tiles: 1,
                reused_tiles: 1,
                ..
            }
        ));
        controller.complete_render(third_completion).unwrap();
        renderer.render_interleaved(&mut frame);
        controller.tick(observation).unwrap();
        assert!(controller
            .diagnostics()
            .iter()
            .any(|line| line.contains("rendered 1 tiles, reused 1")));

        let pin = controller
            .pin_audible_export(
                RenderScope::Master,
                RenderSpan::new(0, 4).unwrap(),
                OutputTailPolicy::Crop,
            )
            .unwrap();
        assert_eq!(pin.plan.id.revisions.aggregate, 3);
        let exported = controller
            .render_export(&pin, &RenderCancellation::new())
            .unwrap();
        assert_eq!(exported.audio.frame_count(), ProjectFrame(4));
    }

    #[test]
    fn worker_progress_reports_dirty_target_without_claiming_audio_starvation() {
        let mut controller = ProjectAudioController::new();
        controller.set_tile_policy(Some(ProjectAudioTilePolicy {
            grid: TileGrid::new(2).unwrap(),
            maximum_context_frames: 0,
        }));
        let first = request(&mut controller, 1, project(1), 1);
        assert!(matches!(
            controller
                .complete_render(completion(&first, 0.1, 71))
                .unwrap(),
            ProjectAudioControllerEffect::OpenHost(_)
        ));
        let second = request(&mut controller, 2, project(2), 2);
        let mut progress = Vec::new();
        let completion = second
            .execute_with_progress(&second.cancellation(), |update| progress.push(update))
            .unwrap();
        assert!(matches!(
            completion.products,
            ProjectAudioRenderProducts::Tiles {
                rendered_tiles: 2,
                ..
            }
        ));
        let tile_states = progress
            .iter()
            .filter_map(|update| match &update.phase {
                ProjectAudioRenderPhase::RenderingTiles(status) => Some(status),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tile_states.first().unwrap().rendered_tiles, 0);
        assert_eq!(tile_states.last().unwrap().remaining_tiles, 0);
        assert_eq!(controller.renderer_status().unwrap().starvation_events, 0);
    }

    #[test]
    fn requesting_a_new_generation_cancels_the_controller_owned_job() {
        let mut controller = ProjectAudioController::new();
        let first = request(&mut controller, 1, project(1), 1);
        let first_cancellation = first.cancellation();
        assert!(!first_cancellation.is_cancelled());
        let _second = request(&mut controller, 2, project(2), 2);
        assert!(first_cancellation.is_cancelled());
        assert!(matches!(
            first.execute(&first_cancellation),
            Err(ProjectAudioControllerError::Cancelled)
        ));
    }

    #[test]
    fn rolling_loop_keeps_old_pcm_until_wrap_then_acks_new_revision() {
        let mut controller = ProjectAudioController::new();
        let first = request(&mut controller, 1, project(1), 1);
        let mut renderer = match controller
            .complete_render(completion(&first, 0.1, 21))
            .unwrap()
        {
            ProjectAudioControllerEffect::OpenHost(renderer) => renderer,
            _ => panic!("first render must bootstrap the host"),
        };
        let observation = ProjectAudioHostObservation {
            transport: TransportSnapshot {
                mode: TransportMode::Playing,
                frame: ProjectFrame(0),
                loop_region: Some(FrameRange::new(ProjectFrame(1), ProjectFrame(3)).unwrap()),
                loop_enabled: true,
                revision: 1,
            },
            preview_active: false,
        };
        controller.observe_host(observation);

        let second = request(&mut controller, 2, project(2), 2);
        controller
            .complete_render(completion(&second, 0.9, 22))
            .unwrap();
        assert!(matches!(
            controller.status().render,
            RenderActivity::Updating {
                revision: 2,
                audible_revision: 1,
                publication_in_flight: true,
                ..
            }
        ));

        let mut old_pass = [0.0; 6];
        assert_eq!(renderer.render_interleaved(&mut old_pass), 3);
        assert_eq!(old_pass, [0.1; 6]);
        renderer.seek(ProjectFrame(1));
        let mut new_pass = [0.0; 2];
        assert_eq!(renderer.render_interleaved(&mut new_pass), 1);
        assert_eq!(new_pass, [0.9; 2]);

        let receipt = controller.tick(observation).unwrap();
        assert!(receipt.is_some());
        assert!(matches!(
            controller.status().render,
            RenderActivity::Ready { revision: 2 }
        ));
    }

    #[test]
    fn newer_ready_edit_cancels_an_armed_intermediate_revision() {
        let mut controller = ProjectAudioController::new();
        let first = request(&mut controller, 1, project(1), 1);
        let mut renderer = match controller
            .complete_render(completion(&first, 0.1, 31))
            .unwrap()
        {
            ProjectAudioControllerEffect::OpenHost(renderer) => renderer,
            _ => panic!("first render must bootstrap the host"),
        };
        let observation = ProjectAudioHostObservation {
            transport: TransportSnapshot {
                mode: TransportMode::Playing,
                frame: ProjectFrame(0),
                loop_region: Some(FrameRange::new(ProjectFrame(1), ProjectFrame(3)).unwrap()),
                loop_enabled: true,
                revision: 1,
            },
            preview_active: false,
        };
        controller.observe_host(observation);

        let second = request(&mut controller, 2, project(2), 2);
        controller
            .complete_render(completion(&second, 0.5, 32))
            .unwrap();
        let third = request(&mut controller, 3, project(3), 3);
        controller
            .complete_render(completion(&third, 0.9, 33))
            .unwrap();

        // Any renderer call observes cancellation of revision 2 without
        // changing active PCM. Its receipt lets the controller arm revision 3.
        let mut first_frame = [0.0; 2];
        renderer.render_interleaved(&mut first_frame);
        assert_eq!(first_frame, [0.1; 2]);
        let cancelled = controller.tick(observation).unwrap().unwrap();
        assert!(matches!(
            cancelled.outcome,
            PublicationCompletionOutcome::Cancelled { .. }
        ));

        let mut rest_of_old_loop = [0.0; 4];
        renderer.render_interleaved(&mut rest_of_old_loop);
        assert_eq!(rest_of_old_loop, [0.1; 4]);
        renderer.seek(ProjectFrame(1));
        let mut next_loop = [0.0; 2];
        renderer.render_interleaved(&mut next_loop);
        assert_eq!(next_loop, [0.9; 2]);
    }

    #[test]
    fn structural_host_replacement_retires_unpublishable_scoped_audition_status() {
        let mut controller = ProjectAudioController::new();
        let first = request(&mut controller, 1, project(1), 1);
        assert!(matches!(
            controller
                .complete_render(completion(&first, 0.1, 41))
                .unwrap(),
            ProjectAudioControllerEffect::OpenHost(_)
        ));
        controller
            .transport_session
            .set_scoped_audition(Some(ScopedAuditionStatus {
                id: crate::render_runtime::TimelineAuditionId {
                    owner: AuditionOwner {
                        namespace: 7,
                        local: 8,
                    },
                    revision: 1,
                    content: digest(99),
                },
                owner: AuditionOwner {
                    namespace: 7,
                    local: 8,
                },
                subject: crate::render_runtime::AuditionSubject::Residual,
                mix: crate::render_runtime::AuditionMix::Replace,
                span: RenderSpan::new(0, 4).unwrap(),
                phase: ScopedAuditionPhase::Active,
            }));

        let mut second = request(&mut controller, 2, project(2), 2);
        second.recipe.extent = RenderSpan::new(0, 6).unwrap();
        assert!(matches!(
            controller
                .complete_render(completion(&second, 0.2, 42))
                .unwrap(),
            ProjectAudioControllerEffect::ReplaceHost(_)
        ));
        assert_eq!(controller.status().scoped_audition, None);
    }
}
