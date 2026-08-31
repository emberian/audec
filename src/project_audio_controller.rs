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

use crate::audio::{FrameRange, ProjectFrame, TransportMode, TransportSnapshot};
use crate::audio_host::{AudioHost, AudioHostSnapshot};
use crate::change_set::ChangeSet;
use crate::daw_engine::{compile_daw_engine, DawEngineConfig};
use crate::daw_render::{RenderCancellation, RenderWindow};
use crate::project_session::{
    ProjectAudioStatus, ProjectPublication, ProjectSession, ProjectSessionEvent, RenderActivity,
    ScopedAuditionPhase, ScopedAuditionStatus,
};
use crate::render_plan::{
    DeterminismGrade, EngineRecipeStamp, ExactDigest, OutputTailPolicy, RenderDependencyStamp,
    RenderPlan, RenderPlanId, RenderScope, RenderSpan, Tileability,
};
use crate::render_products::RenderProduct;
use crate::render_runtime::{
    project_revision_stamp, render_format_stamp, AuditionOwner, CohortRenderer,
    CohortRendererControl, ExecutableRenderPlan, PublicationCompletion,
    PublicationCompletionOutcome, RenderRuntime, RenderRuntimeError, RuntimeRenderedAudio,
    TimelineAudition,
};
use crate::render_service::{
    ExportPin, PublicationAction, RenderAvailability, RenderFailure, RenderFailureStage,
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

/// Cloneable work item suitable for a normal thread/task pool.
#[derive(Clone, Debug)]
pub struct ProjectAudioRenderJob {
    publication: ProjectPublication,
    recipe: ProjectAudioRenderRecipe,
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

    /// Compile and bounce through the sole DAW engine.
    pub fn execute(
        &self,
        cancellation: &RenderCancellation,
    ) -> Result<ProjectAudioRenderCompletion, ProjectAudioControllerError> {
        if self.recipe.stamp.snapshot.is_zero() {
            return Err(ProjectAudioControllerError::MissingSnapshotDigest);
        }
        if self.recipe.stamp.engine_configuration.is_zero() {
            return Err(ProjectAudioControllerError::MissingEngineConfigurationDigest);
        }
        let window = RenderWindow::new(self.recipe.extent.start, self.recipe.extent.end)
            .map_err(|error| ProjectAudioControllerError::Plan(error.to_string()))?;
        let schedule = Arc::new(compile_daw_engine(
            &self.publication.snapshot.project,
            &self.publication.snapshot.pcm,
            window,
            &self.recipe.engine,
            cancellation,
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
        let product = executable.render_whole_bounce(cancellation)?;
        let diagnostics = executable
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
            .collect();
        Ok(ProjectAudioRenderCompletion {
            generation: self.publication.generation,
            revision: self.publication.revisions.aggregate,
            change_set: self.publication.change_set.clone(),
            executable,
            product,
            diagnostics,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ProjectAudioRenderCompletion {
    pub generation: u64,
    pub revision: u64,
    pub change_set: Option<ChangeSet>,
    pub executable: Arc<ExecutableRenderPlan>,
    pub product: Arc<RenderProduct>,
    pub diagnostics: Vec<String>,
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
    desired: Option<DesiredTarget>,
    observation: ProjectAudioHostObservation,
    pending_action: Option<PublicationAction>,
    plan_generations: BTreeMap<RenderPlanId, u64>,
    audible_generation: Option<u64>,
    scoped_audition: Option<ScopedAuditionStatus>,
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
            desired: None,
            observation: ProjectAudioHostObservation::default(),
            pending_action: None,
            plan_generations: BTreeMap::new(),
            audible_generation: None,
            scoped_audition: None,
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
        self.desired = Some(DesiredTarget {
            generation: publication.generation,
            revision: publication.revisions.aggregate,
            change_set: publication.change_set.clone(),
        });
        self.local_failure = None;
        ProjectAudioRenderJob {
            publication,
            recipe,
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
        self.local_failure = None;
        self.diagnostics = completion.diagnostics.clone();

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
            let (control, renderer) = self
                .runtime
                .bootstrap_renderer(completion.executable.id(), Arc::clone(&completion.product))?;
            self.renderer_control = Some(control);
            self.audible_generation = Some(completion.generation);
            return Ok(ProjectAudioControllerEffect::OpenHost(renderer));
        }

        let control = self
            .renderer_control
            .as_ref()
            .expect("checked persistent renderer control")
            .clone();
        let transport = control.publication_transport(self.observation.transport)?;
        let action = self.runtime.stage_whole_bounce(
            completion.executable.id(),
            completion.product,
            transport,
        )?;
        self.queue_action(action)?;
        Ok(ProjectAudioControllerEffect::None)
    }

    fn replace_host(
        &mut self,
        completion: ProjectAudioRenderCompletion,
    ) -> Result<ProjectAudioControllerEffect, ProjectAudioControllerError> {
        let mut runtime = RenderRuntime::new();
        runtime.submit_target(Arc::clone(&completion.executable))?;
        let (control, renderer) = runtime
            .bootstrap_renderer(completion.executable.id(), Arc::clone(&completion.product))?;
        self.runtime = runtime;
        self.renderer_control = Some(control);
        self.pending_action = None;
        self.plan_generations.clear();
        self.plan_generations
            .insert(completion.executable.id().clone(), completion.generation);
        self.audible_generation = Some(completion.generation);
        Ok(ProjectAudioControllerEffect::ReplaceHost(renderer))
    }

    /// Record a newest-generation worker error while preserving old audio.
    pub fn fail_render(&mut self, generation: u64, message: impl Into<String>) -> bool {
        let Some(desired) = &self.desired else {
            return false;
        };
        if desired.generation != generation {
            return false;
        }
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
        self.observation = observation;
    }

    /// Apply a pane's transport intent to the one project transport. Panes
    /// receive this callback; they never retain a competing handle.
    pub fn apply_transport_intent(
        &mut self,
        host: &AudioHost,
        intent: ProjectTransportIntent,
    ) -> Result<(), ProjectAudioControllerError> {
        let transport = host.transport();
        match intent {
            ProjectTransportIntent::Play => transport.play(),
            ProjectTransportIntent::Pause => transport.pause(),
            ProjectTransportIntent::Stop => transport.stop(),
            ProjectTransportIntent::TogglePlay => {
                if transport.snapshot().mode == TransportMode::Playing {
                    transport.pause();
                } else {
                    transport.play();
                }
            }
            ProjectTransportIntent::Seek(frame) => transport.seek(frame),
            ProjectTransportIntent::SetLoop { range, enabled } => {
                transport.set_loop_region(Some(range))?;
                transport.set_loop_enabled(enabled);
            }
            ProjectTransportIntent::ClearLoop => transport.set_loop_region(None)?,
        }
        self.observation = host.snapshot().into();
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
        let control = self
            .renderer_control
            .as_ref()
            .ok_or(ProjectAudioControllerError::NoPersistentRenderer)?;
        control.set_timeline_audition(Arc::clone(&audition))?;
        self.scoped_audition = Some(ScopedAuditionStatus {
            id: audition.id,
            owner: audition.id.owner,
            subject: audition.subject,
            mix: audition.mix,
            span: audition.span,
            phase: ScopedAuditionPhase::Pending,
        });
        let range = relative_audio_range(control.timeline(), audition.span)?;
        let transport = host.transport();
        match alignment {
            AuditionAlignment::PreserveTransport => {}
            AuditionAlignment::SeekToStart { play } => {
                transport.seek(range.start);
                if play {
                    transport.play();
                }
            }
            AuditionAlignment::LoopSpan { play } => {
                transport.set_loop_region(Some(range))?;
                transport.set_loop_enabled(true);
                transport.seek(range.start);
                if play {
                    transport.play();
                }
            }
        }
        self.observation = host.snapshot().into();
        Ok(())
    }

    /// Stop only this owner. A stale pane cannot clear a newer pane's scoped
    /// audition or alter project transport as a side effect.
    pub fn stop_scoped_audition(
        &mut self,
        owner: AuditionOwner,
    ) -> Result<(), ProjectAudioControllerError> {
        let control = self
            .renderer_control
            .as_ref()
            .ok_or(ProjectAudioControllerError::NoPersistentRenderer)?;
        control.clear_timeline_audition(owner)?;
        if self
            .scoped_audition
            .is_some_and(|status| status.owner == owner)
        {
            if let Some(status) = &mut self.scoped_audition {
                status.phase = ScopedAuditionPhase::Pending;
            }
        }
        Ok(())
    }

    /// Drive receipt acknowledgement and the next staged publication. This is
    /// cheap enough for a UI timer; it never renders or waits.
    pub fn tick(
        &mut self,
        observation: ProjectAudioHostObservation,
    ) -> Result<Option<PublicationCompletion>, ProjectAudioControllerError> {
        self.observation = observation;
        let Some(control) = self.renderer_control.as_ref().cloned() else {
            return Ok(None);
        };

        if let Some(receipt) = control.drain_audition_receipt() {
            match receipt.active {
                Some(active) => {
                    if let Some(status) = &mut self.scoped_audition {
                        if status.id == active {
                            status.phase = ScopedAuditionPhase::Active;
                        }
                    }
                }
                None => self.scoped_audition = None,
            }
        }

        self.retry_pending(&control)?;
        let completion = self.runtime.poll_publication(&control)?;
        if let Some(PublicationCompletion {
            outcome: PublicationCompletionOutcome::Activated { active, .. },
        }) = &completion
        {
            self.audible_generation = self.plan_generations.get(&active.plan).copied();
        }
        let action = self
            .runtime
            .observe_transport(&control, self.observation.transport)?;
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
            transport: self.observation.transport,
            render,
            preview_active: self.observation.preview_active,
            scoped_audition: self.scoped_audition,
            diagnostic: self.diagnostics.last().cloned(),
        }
    }

    /// Publish cheap status and diagnostics into the session event stream.
    pub fn publish_session_state(&self, session: &mut ProjectSession) {
        session.set_audio_status(self.status());
        session.replace_diagnostics(self.diagnostics.clone());
    }

    /// Pin exactly the cohort currently audible, even while a newer revision
    /// is rendering or staged.
    pub fn pin_audible_export(
        &self,
        scope: RenderScope,
        span: RenderSpan,
        tail: OutputTailPolicy,
    ) -> Result<ExportPin, ProjectAudioControllerError> {
        Ok(self.runtime.pin_active_export(scope, span, tail)?)
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
    MissingSnapshotDigest,
    MissingEngineConfigurationDigest,
    NoDesiredTarget,
    ControllerActionAlreadyPending,
    NoPersistentRenderer,
    AuditionOutsideTimeline {
        audition: RenderSpan,
        timeline: RenderSpan,
    },
    TransportCoordinateOverflow,
    Plan(String),
    Audio(crate::audio::AudioError),
    Engine(crate::daw_engine::DawEngineError),
    Runtime(RenderRuntimeError),
}

impl fmt::Display for ProjectAudioControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyArrangement => formatter.write_str("project arrangement is empty"),
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
            Self::AuditionOutsideTimeline { .. } => {
                formatter.write_str("scoped audition lies outside project playback")
            }
            Self::TransportCoordinateOverflow => {
                formatter.write_str("scoped audition coordinate overflows project transport")
            }
            Self::Plan(message) => write!(formatter, "render plan: {message}"),
            Self::Audio(error) => error.fmt(formatter),
            Self::Engine(error) => error.fmt(formatter),
            Self::Runtime(error) => error.fmt(formatter),
        }
    }
}

impl Error for ProjectAudioControllerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
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

    use crate::audio::{FrameRange, ProjectRenderer};
    use crate::daw_engine::AssetPcmMap;
    use crate::daw_project::{DawProject, ProjectDomain};
    use crate::live_project::LiveProjectSnapshot;
    use crate::mixer::BusKind;
    use crate::render_products::{ProductPartition, RenderProductKey};
    use crate::render_runtime::whole_bounce_boundary_recipe;

    fn digest(byte: u8) -> ExactDigest {
        ExactDigest::new([byte; 32])
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
        completion.product =
            Arc::new(RenderProduct::new(digest(identity_byte), key, samples.into()).unwrap());
        completion
    }

    #[test]
    fn rapid_edits_discard_obsolete_worker_completions() {
        let mut controller = ProjectAudioController::new();
        let first = request(&mut controller, 1, project(1), 1);
        let second = request(&mut controller, 2, project(2), 2);

        let obsolete = controller
            .complete_render(completion(&first, 0.1, 11))
            .unwrap();
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
}
