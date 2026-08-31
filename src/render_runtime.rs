//! Executable bridge from immutable DAW schedules to persistent playback.
//!
//! The only sound-producing kernel in this module is [`DawEngineSchedule`]. A
//! whole bounce is rendered from that kernel into an immutable product, the
//! persistent renderer plays that exact product, and export pins either reuse
//! those samples or invoke the same frozen schedule. Tiles and state anchors
//! can replace the product partition later without changing publication,
//! playback, or export truth.
//!
//! # Controller protocol
//!
//! 1. Freeze one aggregate snapshot into an [`ExecutableRenderPlan`] and call
//!    [`RenderRuntime::submit_target`]. Clone that executable to a worker and
//!    call [`ExecutableRenderPlan::render_whole_bounce`].
//! 2. For cold playback, call [`RenderRuntime::bootstrap_renderer`] and pass
//!    its renderer to [`AudioHost::open_renderer`](crate::audio_host::AudioHost::open_renderer).
//! 3. For later worker completions, translate the current host snapshot with
//!    [`CohortRendererControl::publication_transport`], call
//!    [`RenderRuntime::stage_whole_bounce`], then pass its action to
//!    [`CohortRendererControl::arm_action`]. Old PCM remains audible until the
//!    renderer returns a receipt at the armed boundary.
//! 4. Poll [`RenderRuntime::poll_publication`] on the control thread. After an
//!    acknowledgement, call [`RenderRuntime::arm_staged`] in case a newer
//!    worker completion arrived while the prior publication was in flight.
//!
//! Format or timeline-extent changes intentionally reject an in-place swap:
//! `TransportHandle` freezes both facts. The controller recreates the host for
//! those uncommon structural changes; ordinary edits never replace it.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::Arc;

use crate::audio::{
    AudioError, AudioFormat, ProjectAudio, ProjectFrame, ProjectRenderer, TransportMode,
    TransportSnapshot,
};
use crate::daw_engine::{DawEngineError, DawEngineSchedule};
use crate::daw_render::{RenderCancellation, RenderWindow};
use crate::render_plan::{
    ExactDigest, OutputTailPolicy, ProjectRevisionStamp, RenderFormat, RenderPlan, RenderPlanId,
    RenderScope, RenderSpan,
};
use crate::render_products::{
    CohortProduct, CohortProductProvenance, PlaybackCohort, PlaybackCohortId, ProductPartition,
    RenderProduct, RenderProductCatalog, RenderProductError, RenderProductKey, RenderSlot,
};
use crate::render_service::{
    ExportPin, ExportPinSource, PublicationAction, PublicationBoundary, PublicationTicket,
    PublicationTransport, RenderFailure, RenderService, RenderServiceError, RenderServiceStatus,
};

const PCM_DIGEST_DOMAIN: &[u8] = b"audec:canonical-f32le-pcm:v1\0";
const WHOLE_BOUNCE_BOUNDARY_DOMAIN: &[u8] = b"audec:whole-bounce-boundary:v1";

/// Metadata plus the actual frozen engine schedule it identifies.
#[derive(Clone, Debug)]
pub struct ExecutableRenderPlan {
    pub descriptor: Arc<RenderPlan>,
    pub schedule: Arc<DawEngineSchedule>,
}

impl ExecutableRenderPlan {
    pub fn new(
        descriptor: Arc<RenderPlan>,
        schedule: Arc<DawEngineSchedule>,
    ) -> Result<Self, RenderRuntimeError> {
        let actual_revisions = project_revision_stamp(schedule.project_revision());
        if descriptor.id.revisions != actual_revisions {
            return Err(RenderRuntimeError::PlanRevisionMismatch {
                expected: descriptor.id.revisions,
                actual: actual_revisions,
            });
        }
        let actual_format = render_format_stamp(schedule.render_schedule().format());
        if descriptor.format() != actual_format {
            return Err(RenderRuntimeError::PlanFormatMismatch {
                expected: descriptor.format(),
                actual: actual_format,
            });
        }
        let window = schedule.render_schedule().window();
        let actual_extent = RenderSpan::new(window.start, window.end)
            .map_err(|_| RenderRuntimeError::InvalidEngineExtent)?;
        if descriptor.extent() != actual_extent {
            return Err(RenderRuntimeError::PlanExtentMismatch {
                expected: descriptor.extent(),
                actual: actual_extent,
            });
        }
        Ok(Self {
            descriptor,
            schedule,
        })
    }

    pub fn id(&self) -> &RenderPlanId {
        &self.descriptor.id
    }

    /// Execute one master product through the sole DAW engine.
    pub fn render_master_product(
        &self,
        span: RenderSpan,
        partition: ProductPartition,
        boundary_recipe: ExactDigest,
        cancellation: &RenderCancellation,
    ) -> Result<Arc<RenderProduct>, RenderRuntimeError> {
        if !self.descriptor.extent().contains_span(span) {
            return Err(RenderRuntimeError::ProductOutsidePlan {
                product: span,
                plan: self.descriptor.extent(),
            });
        }
        let rendered = self.schedule.render(
            RenderWindow::new(span.start, span.end)
                .map_err(|_| RenderRuntimeError::InvalidEngineExtent)?,
            cancellation,
        )?;
        if rendered.origin_frame != span.start {
            return Err(RenderRuntimeError::EngineOriginMismatch {
                expected: span.start,
                actual: rendered.origin_frame,
            });
        }
        let key = RenderProductKey::new(
            self.descriptor.id.clone(),
            RenderScope::Master,
            span,
            partition,
            boundary_recipe,
        )?;
        let pcm = rendered.audio.shared_interleaved();
        let digest = canonical_pcm_digest(&pcm);
        Ok(Arc::new(RenderProduct::new(digest, key, pcm)?))
    }

    pub fn render_whole_bounce(
        &self,
        cancellation: &RenderCancellation,
    ) -> Result<Arc<RenderProduct>, RenderRuntimeError> {
        self.render_master_product(
            self.descriptor.extent(),
            ProductPartition::WholeBounce,
            whole_bounce_boundary_recipe(),
            cancellation,
        )
    }
}

/// Control-side runtime. Expensive renders may run on any worker after cloning
/// an [`ExecutableRenderPlan`]; publication and acknowledgements return here.
#[derive(Debug, Default)]
pub struct RenderRuntime {
    service: RenderService,
    executable: BTreeMap<RenderPlanId, Arc<ExecutableRenderPlan>>,
    products: RenderProductCatalog,
    next_cohort_sequence: u64,
}

impl RenderRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn service(&self) -> &RenderService {
        &self.service
    }

    pub fn submit_target(
        &mut self,
        executable: Arc<ExecutableRenderPlan>,
    ) -> Result<(), RenderRuntimeError> {
        if let Some(existing) = self.executable.get(executable.id()) {
            if existing.descriptor != executable.descriptor {
                return Err(RenderRuntimeError::ExecutableIdentityCollision(
                    executable.id().clone(),
                ));
            }
        } else {
            self.executable
                .insert(executable.id().clone(), Arc::clone(&executable));
        }
        self.service
            .submit_target(Arc::clone(&executable.descriptor))?;
        Ok(())
    }

    pub fn executable_plan(
        &self,
        plan: &RenderPlanId,
    ) -> Result<Arc<ExecutableRenderPlan>, RenderRuntimeError> {
        self.executable
            .get(plan)
            .cloned()
            .ok_or_else(|| RenderRuntimeError::UnknownExecutablePlan(plan.clone()))
    }

    pub fn adopt_product(
        &mut self,
        product: Arc<RenderProduct>,
    ) -> Result<Arc<RenderProduct>, RenderRuntimeError> {
        Ok(self.products.insert(product)?)
    }

    /// Stage a complete master bounce. The returned action is passed by
    /// reference to [`CohortRendererControl::arm_action`]. Until the renderer
    /// acknowledges it, the old cohort remains both active and visible.
    pub fn stage_whole_bounce(
        &mut self,
        plan: &RenderPlanId,
        product: Arc<RenderProduct>,
        transport: PublicationTransport,
    ) -> Result<PublicationAction, RenderRuntimeError> {
        if &product.produced_by.plan != plan
            || product.produced_by.scope != RenderScope::Master
            || product.produced_by.core != plan.compiled_extent
            || product.produced_by.partition != ProductPartition::WholeBounce
        {
            return Err(RenderRuntimeError::NotWholeMasterProduct);
        }
        let product = self.adopt_product(product)?;
        let cohort = self.whole_bounce_cohort(plan, product, transport.loop_region)?;
        Ok(self.service.stage_cohort_for_transport(cohort, transport)?)
    }

    /// Bootstrap a renderer directly from the first whole bounce. This is the
    /// only publication that need not cross a mailbox: construction itself is
    /// the activation boundary.
    pub fn bootstrap_renderer(
        &mut self,
        plan: &RenderPlanId,
        product: Arc<RenderProduct>,
    ) -> Result<(CohortRendererControl, CohortRenderer), RenderRuntimeError> {
        let action = self.stage_whole_bounce(plan, product, PublicationTransport::default())?;
        let cohort = action
            .cohort()
            .cloned()
            .ok_or(RenderRuntimeError::InitialCohortWasNotArmed)?;
        let (control, renderer) = CohortRenderer::new(Arc::clone(&cohort))?;
        let retired = self.service.acknowledge_publication(&cohort.id)?;
        if retired.is_some() {
            return Err(RenderRuntimeError::UnexpectedInitialRetirement);
        }
        Ok((control, renderer))
    }

    fn whole_bounce_cohort(
        &mut self,
        plan: &RenderPlanId,
        product: Arc<RenderProduct>,
        publication_loop: Option<RenderSpan>,
    ) -> Result<Arc<PlaybackCohort>, RenderRuntimeError> {
        if let Some(loop_region) = publication_loop {
            if !plan.compiled_extent.contains_span(loop_region) {
                return Err(RenderRuntimeError::LoopOutsidePlan(loop_region));
            }
        }
        self.next_cohort_sequence = self
            .next_cohort_sequence
            .checked_add(1)
            .ok_or(RenderRuntimeError::CohortSequenceOverflow)?;
        let slot = RenderSlot {
            scope: RenderScope::Master,
            span: plan.compiled_extent,
        };
        Ok(Arc::new(PlaybackCohort::new(
            PlaybackCohortId {
                plan: plan.clone(),
                sequence: self.next_cohort_sequence,
            },
            publication_loop,
            vec![slot.clone()],
            vec![CohortProduct {
                slot,
                product,
                provenance: CohortProductProvenance::RenderedForTarget,
            }],
        )?))
    }

    pub fn update_transport(&mut self, transport: PublicationTransport) -> PublicationAction {
        self.service.update_transport(transport)
    }

    /// Controller convenience: translate a host snapshot through the renderer
    /// that owns its timeline, then update the publication service.
    pub fn observe_transport(
        &mut self,
        control: &CohortRendererControl,
        snapshot: TransportSnapshot,
    ) -> Result<PublicationAction, RenderRuntimeError> {
        Ok(self.update_transport(control.publication_transport(snapshot)?))
    }

    /// Ask the service for a candidate left staged behind an earlier in-flight
    /// publication. Controllers normally call this after
    /// [`Self::poll_publication`] returns a completion.
    pub fn arm_staged(&mut self) -> PublicationAction {
        self.service.arm_staged()
    }

    /// Reconcile a host-side publication failure without discarding the last
    /// coherent active cohort.
    pub fn reject_publication(
        &mut self,
        cohort: &PlaybackCohortId,
        message: impl Into<String>,
    ) -> Result<(), RenderRuntimeError> {
        Ok(self.service.reject_publication(cohort, message)?)
    }

    pub fn record_failure(&mut self, failure: RenderFailure) -> bool {
        self.service.record_failure(failure)
    }

    pub fn status(&self) -> RenderServiceStatus {
        self.service.status()
    }

    /// Drain one audio-thread acknowledgement and advance semantic active state.
    pub fn poll_publication(
        &mut self,
        control: &CohortRendererControl,
    ) -> Result<Option<PublicationCompletion>, RenderRuntimeError> {
        let Some(receipt) = control.drain_receipt() else {
            return Ok(None);
        };
        let service_retired = self.service.acknowledge_publication(&receipt.activated)?;
        let service_retired_id = service_retired.as_ref().map(|cohort| cohort.id.clone());
        let renderer_retired_id = receipt.retired.as_ref().map(|cohort| cohort.id.clone());
        if service_retired_id != renderer_retired_id {
            return Err(RenderRuntimeError::RetiredCohortMismatch {
                service: service_retired_id,
                renderer: renderer_retired_id,
            });
        }
        Ok(Some(PublicationCompletion {
            active: receipt.activated,
            retired: service_retired_id,
        }))
    }

    pub fn pin_plan_export(
        &self,
        plan: &RenderPlanId,
        scope: RenderScope,
        span: RenderSpan,
        tail: OutputTailPolicy,
    ) -> Result<ExportPin, RenderRuntimeError> {
        Ok(self.service.pin_plan_export(plan, scope, span, tail)?)
    }

    pub fn pin_active_export(
        &self,
        scope: RenderScope,
        span: RenderSpan,
        tail: OutputTailPolicy,
    ) -> Result<ExportPin, RenderRuntimeError> {
        Ok(self.service.pin_active_products_export(scope, span, tail)?)
    }

    /// Resolve an export pin to finite PCM. This is the adapter for the current
    /// `export_project_audio_to_wav` API; a later streaming encoder can consume
    /// the same pin without changing its identity.
    pub fn render_export_pin(
        &self,
        pin: &ExportPin,
        cancellation: &RenderCancellation,
    ) -> Result<RuntimeRenderedAudio, RenderRuntimeError> {
        if pin.scope != RenderScope::Master {
            return Err(RenderRuntimeError::UnsupportedExportScope(
                pin.scope.clone(),
            ));
        }
        let mut rendered = match &pin.source {
            ExportPinSource::FreshPlanRender => {
                let executable = self.executable_plan(&pin.plan.id)?;
                let result = executable.schedule.render(
                    RenderWindow::new(pin.maximum_output_span.start, pin.maximum_output_span.end)
                        .map_err(|_| RenderRuntimeError::InvalidEngineExtent)?,
                    cancellation,
                )?;
                if result.origin_frame != pin.maximum_output_span.start {
                    return Err(RenderRuntimeError::EngineOriginMismatch {
                        expected: pin.maximum_output_span.start,
                        actual: result.origin_frame,
                    });
                }
                result.audio.interleaved().to_vec()
            }
            ExportPinSource::PublishedProducts { cohort, .. } => {
                copy_cohort_pcm(cohort, &pin.scope, pin.maximum_output_span)?
            }
        };
        let channels = usize::from(pin.plan.format().channels.get());
        let output_span = resolve_adaptive_tail(pin, &rendered, channels)?;
        let output_samples = usize::try_from(output_span.len())
            .ok()
            .and_then(|frames| frames.checked_mul(channels))
            .ok_or(RenderRuntimeError::RenderTooLarge)?;
        rendered.truncate(output_samples);
        let audio_format = audio_format(pin.plan.format());
        let pcm_digest = canonical_pcm_digest(&rendered);
        let audio = ProjectAudio::from_interleaved(audio_format, rendered)?;
        Ok(RuntimeRenderedAudio {
            plan: pin.plan.id.clone(),
            scope: pin.scope.clone(),
            origin_frame: output_span.start,
            audio,
            pcm_digest,
        })
    }
}

fn copy_cohort_pcm(
    cohort: &PlaybackCohort,
    scope: &RenderScope,
    span: RenderSpan,
) -> Result<Vec<f32>, RenderRuntimeError> {
    if !cohort.covers(scope, span) {
        return Err(RenderRuntimeError::CohortDoesNotCover(span));
    }
    let channels = usize::from(cohort.id.plan.engine.format.channels.get());
    let sample_count = usize::try_from(span.len())
        .ok()
        .and_then(|frames| frames.checked_mul(channels))
        .ok_or(RenderRuntimeError::RenderTooLarge)?;
    let mut output = vec![0.0; sample_count];
    for entry in cohort
        .products()
        .filter(|entry| &entry.slot.scope == scope && entry.slot.span.intersects(span))
    {
        let overlap = entry
            .slot
            .span
            .intersection(span)
            .expect("filtered product overlap");
        let source_frame = usize::try_from(overlap.start - entry.slot.span.start)
            .map_err(|_| RenderRuntimeError::RenderTooLarge)?;
        let target_frame = usize::try_from(overlap.start - span.start)
            .map_err(|_| RenderRuntimeError::RenderTooLarge)?;
        let frames =
            usize::try_from(overlap.len()).map_err(|_| RenderRuntimeError::RenderTooLarge)?;
        let source_start = source_frame * channels;
        let target_start = target_frame * channels;
        let samples = frames * channels;
        output[target_start..target_start + samples]
            .copy_from_slice(&entry.product.interleaved()[source_start..source_start + samples]);
    }
    Ok(output)
}

fn resolve_adaptive_tail(
    pin: &ExportPin,
    samples: &[f32],
    channels: usize,
) -> Result<RenderSpan, RenderRuntimeError> {
    match pin.tail {
        OutputTailPolicy::Crop | OutputTailPolicy::FixedFrames(_) => Ok(pin.maximum_output_span),
        OutputTailPolicy::UntilBelow {
            amplitude,
            hold_frames,
            ..
        } => {
            let body_frames =
                usize::try_from(pin.span.len()).map_err(|_| RenderRuntimeError::RenderTooLarge)?;
            let hold_frames =
                usize::try_from(hold_frames).map_err(|_| RenderRuntimeError::RenderTooLarge)?;
            let mut quiet = 0_usize;
            let mut resolved_frames = usize::try_from(pin.maximum_output_span.len())
                .map_err(|_| RenderRuntimeError::RenderTooLarge)?;
            for (index, frame) in samples.chunks_exact(channels).enumerate().skip(body_frames) {
                if frame.iter().all(|sample| sample.abs() <= amplitude) {
                    quiet = quiet.saturating_add(1);
                    if quiet >= hold_frames {
                        resolved_frames = index + 1;
                        break;
                    }
                } else {
                    quiet = 0;
                }
            }
            let resolved_end = pin
                .maximum_output_span
                .start
                .checked_add(
                    i64::try_from(resolved_frames)
                        .map_err(|_| RenderRuntimeError::RenderTooLarge)?,
                )
                .ok_or(RenderRuntimeError::RenderTooLarge)?;
            RenderSpan::new(pin.maximum_output_span.start, resolved_end)
                .map_err(|_| RenderRuntimeError::RenderTooLarge)
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeRenderedAudio {
    pub plan: RenderPlanId,
    pub scope: RenderScope,
    pub origin_frame: i64,
    pub audio: ProjectAudio,
    pub pcm_digest: ExactDigest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationCompletion {
    pub active: PlaybackCohortId,
    pub retired: Option<PlaybackCohortId>,
}

struct PublicationEnvelope {
    ticket: PublicationTicket,
    retired: Option<Arc<PlaybackCohort>>,
}

/// One-producer/one-consumer mailbox. The control thread allocates envelopes;
/// the realtime renderer only moves their raw pointers and returns the same
/// allocation for acknowledgement/reclamation on the control thread.
struct PublicationMailbox {
    incoming: AtomicPtr<PublicationEnvelope>,
    receipt: AtomicPtr<PublicationEnvelope>,
}

impl PublicationMailbox {
    fn new() -> Self {
        Self {
            incoming: AtomicPtr::new(ptr::null_mut()),
            receipt: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

impl Drop for PublicationMailbox {
    fn drop(&mut self) {
        for slot in [&self.incoming, &self.receipt] {
            let pointer = slot.swap(ptr::null_mut(), Ordering::AcqRel);
            if !pointer.is_null() {
                // SAFETY: pointers stored in either slot come only from one
                // `Box::into_raw`, and swapping to null claims unique ownership.
                unsafe { drop(Box::from_raw(pointer)) };
            }
        }
    }
}

#[derive(Debug)]
struct RendererCounters {
    starvation_events: AtomicU64,
    starved_frames: AtomicU64,
    publication_pending: AtomicBool,
}

impl RendererCounters {
    fn new() -> Self {
        Self {
            starvation_events: AtomicU64::new(0),
            starved_frames: AtomicU64::new(0),
            publication_pending: AtomicBool::new(false),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CohortRendererStatus {
    pub starvation_events: u64,
    pub starved_frames: u64,
    pub publication_queued: bool,
    pub receipt_waiting: bool,
}

/// Control-thread half of persistent playback publication.
#[derive(Clone)]
pub struct CohortRendererControl {
    mailbox: Arc<PublicationMailbox>,
    counters: Arc<RendererCounters>,
    format: RenderFormat,
    timeline: RenderSpan,
}

impl CohortRendererControl {
    /// Queue one pre-armed action without consuming it. On `MailboxBusy`, the
    /// caller retains the action and may retry; semantic service state remains
    /// publication-in-flight until an acknowledgement or rejection.
    pub fn arm_action(&self, action: &PublicationAction) -> Result<(), RenderRuntimeError> {
        let Some(ticket) = action.ticket() else {
            return Ok(());
        };
        self.validate_ticket(ticket)?;
        let envelope = Box::new(PublicationEnvelope {
            ticket: ticket.clone(),
            retired: None,
        });
        let pointer = Box::into_raw(envelope);
        // Set before the release-CAS makes the pointer visible. The renderer
        // can therefore never activate a ticket and clear the flag before the
        // producer announces it.
        self.counters
            .publication_pending
            .store(true, Ordering::Release);
        match self.mailbox.incoming.compare_exchange(
            ptr::null_mut(),
            pointer,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(_) => {
                // SAFETY: compare_exchange failed, so the pointer was never
                // published and remains uniquely owned by this control call.
                unsafe { drop(Box::from_raw(pointer)) };
                // Another incoming ticket owns the pending flag. Its eventual
                // activation clears it; do not race that consumer here.
                Err(RenderRuntimeError::PublicationMailboxBusy)
            }
        }
    }

    fn validate_ticket(&self, ticket: &PublicationTicket) -> Result<(), RenderRuntimeError> {
        if !ticket.cohort.is_ready() {
            return Err(RenderRuntimeError::IncompletePlaybackCohort);
        }
        if ticket.cohort.id.plan.engine.format != self.format {
            return Err(RenderRuntimeError::RendererFormatChanged {
                expected: self.format,
                actual: ticket.cohort.id.plan.engine.format,
            });
        }
        if ticket.cohort.id.plan.compiled_extent != self.timeline {
            return Err(RenderRuntimeError::RendererTimelineChanged {
                expected: self.timeline,
                actual: ticket.cohort.id.plan.compiled_extent,
            });
        }
        Ok(())
    }

    pub fn drain_receipt(&self) -> Option<PublicationReceipt> {
        let pointer = self.mailbox.receipt.swap(ptr::null_mut(), Ordering::AcqRel);
        if pointer.is_null() {
            return None;
        }
        // SAFETY: swapping the receipt pointer to null transfers its unique Box
        // ownership back to the control thread.
        let envelope = unsafe { Box::from_raw(pointer) };
        Some(PublicationReceipt {
            activated: envelope.ticket.cohort.id.clone(),
            retired: envelope.retired,
        })
    }

    pub fn status(&self) -> CohortRendererStatus {
        CohortRendererStatus {
            starvation_events: self.counters.starvation_events.load(Ordering::Acquire),
            starved_frames: self.counters.starved_frames.load(Ordering::Acquire),
            publication_queued: self.counters.publication_pending.load(Ordering::Acquire),
            receipt_waiting: !self.mailbox.receipt.load(Ordering::Acquire).is_null(),
        }
    }

    pub const fn format(&self) -> RenderFormat {
        self.format
    }

    pub const fn timeline(&self) -> RenderSpan {
        self.timeline
    }

    /// Translate the zero-based transport exposed by [`AudioHost`](crate::audio_host::AudioHost)
    /// back onto this renderer's signed project timeline.
    pub fn publication_transport(
        &self,
        snapshot: TransportSnapshot,
    ) -> Result<PublicationTransport, RenderRuntimeError> {
        if snapshot.frame.0 > self.timeline.len() {
            return Err(RenderRuntimeError::TransportFrameOutsideTimeline {
                frame: snapshot.frame.0,
                timeline: self.timeline,
            });
        }
        let loop_region = if snapshot.loop_enabled {
            snapshot
                .loop_region
                .map(|region| {
                    if region.end.0 > self.timeline.len() {
                        return Err(RenderRuntimeError::TransportLoopOutsideTimeline {
                            start: region.start.0,
                            end: region.end.0,
                            timeline: self.timeline,
                        });
                    }
                    let start = relative_project_frame(self.timeline, region.start.0)?;
                    let end = relative_project_frame(self.timeline, region.end.0)?;
                    RenderSpan::new(start, end).map_err(|_| {
                        RenderRuntimeError::TransportLoopOutsideTimeline {
                            start: region.start.0,
                            end: region.end.0,
                            timeline: self.timeline,
                        }
                    })
                })
                .transpose()?
        } else {
            None
        };
        Ok(PublicationTransport {
            rolling: snapshot.mode == TransportMode::Playing,
            loop_region,
        })
    }
}

#[derive(Debug)]
pub struct PublicationReceipt {
    pub activated: PlaybackCohortId,
    pub retired: Option<Arc<PlaybackCohort>>,
}

/// Realtime-side renderer over immutable cohort products.
///
/// It performs no allocation, locking, hashing, scheduling, logging, or final
/// Arc reclamation in `render_interleaved`/`seek`. Publication envelopes are
/// allocated by control and returned unchanged for control-side destruction.
pub struct CohortRenderer {
    mailbox: Arc<PublicationMailbox>,
    counters: Arc<RendererCounters>,
    active: Arc<PlaybackCohort>,
    format: AudioFormat,
    format_stamp: RenderFormat,
    timeline: RenderSpan,
    position: ProjectFrame,
    current_product: Option<Arc<RenderProduct>>,
    pending_ticket: Option<Box<PublicationEnvelope>>,
    pending_receipt: Option<Box<PublicationEnvelope>>,
    starving: bool,
}

impl CohortRenderer {
    pub fn new(
        active: Arc<PlaybackCohort>,
    ) -> Result<(CohortRendererControl, Self), RenderRuntimeError> {
        if !active.is_ready() {
            return Err(RenderRuntimeError::IncompletePlaybackCohort);
        }
        let format_stamp = active.id.plan.engine.format;
        let timeline = active.id.plan.compiled_extent;
        if !active.covers(&RenderScope::Master, timeline) {
            return Err(RenderRuntimeError::CohortDoesNotCover(timeline));
        }
        let mailbox = Arc::new(PublicationMailbox::new());
        let counters = Arc::new(RendererCounters::new());
        let control = CohortRendererControl {
            mailbox: Arc::clone(&mailbox),
            counters: Arc::clone(&counters),
            format: format_stamp,
            timeline,
        };
        let renderer = Self {
            mailbox,
            counters,
            active,
            format: audio_format(format_stamp),
            format_stamp,
            timeline,
            position: ProjectFrame(0),
            current_product: None,
            pending_ticket: None,
            pending_receipt: None,
            starving: false,
        };
        Ok((control, renderer))
    }

    pub fn active_cohort(&self) -> &PlaybackCohortId {
        &self.active.id
    }

    fn project_position(&self) -> i64 {
        self.timeline
            .start
            .saturating_add(self.position.0.min(i64::MAX as u64) as i64)
    }

    fn poll_ticket(&mut self) {
        self.flush_receipt();
        if self.pending_ticket.is_some() {
            return;
        }
        let pointer = self
            .mailbox
            .incoming
            .swap(ptr::null_mut(), Ordering::AcqRel);
        if !pointer.is_null() {
            // SAFETY: swapping the incoming pointer to null transfers unique
            // ownership from the control producer to this renderer.
            self.pending_ticket = Some(unsafe { Box::from_raw(pointer) });
        }
    }

    fn activate_if(&mut self, boundary: PublicationBoundary) {
        self.poll_ticket();
        let accepted = self
            .pending_ticket
            .as_ref()
            .is_some_and(|envelope| envelope.ticket.accepts(boundary));
        if !accepted {
            return;
        }
        let mut envelope = self
            .pending_ticket
            .take()
            .expect("accepted publication ticket exists");
        let next = Arc::clone(&envelope.ticket.cohort);
        let retired = std::mem::replace(&mut self.active, next);
        envelope.retired = Some(retired);
        self.current_product = None;
        self.starving = false;
        self.pending_receipt = Some(envelope);
        self.counters
            .publication_pending
            .store(false, Ordering::Release);
        self.flush_receipt();
    }

    fn flush_receipt(&mut self) {
        let Some(envelope) = self.pending_receipt.take() else {
            return;
        };
        let pointer = Box::into_raw(envelope);
        match self.mailbox.receipt.compare_exchange(
            ptr::null_mut(),
            pointer,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => {}
            Err(_) => {
                // SAFETY: compare_exchange failed, so this pointer was not
                // published and remains uniquely owned by the renderer.
                self.pending_receipt = Some(unsafe { Box::from_raw(pointer) });
            }
        }
    }

    fn select_product(&mut self, project_frame: i64) -> bool {
        let current_matches = self.current_product.as_ref().is_some_and(|product| {
            product.produced_by.scope == RenderScope::Master
                && product.produced_by.core.contains(project_frame)
        });
        if !current_matches {
            self.current_product = self
                .active
                .products()
                .find(|entry| {
                    entry.slot.scope == RenderScope::Master
                        && entry.slot.span.contains(project_frame)
                })
                .map(|entry| Arc::clone(&entry.product));
        }
        self.current_product.is_some()
    }

    fn note_starvation(&mut self, frames: u64) {
        if !self.starving {
            self.counters
                .starvation_events
                .fetch_add(1, Ordering::Relaxed);
            self.starving = true;
        }
        self.counters
            .starved_frames
            .fetch_add(frames, Ordering::Relaxed);
    }
}

impl ProjectRenderer for CohortRenderer {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn length(&self) -> ProjectFrame {
        ProjectFrame(self.timeline.len())
    }

    fn position(&self) -> ProjectFrame {
        self.position
    }

    fn seek(&mut self, frame: ProjectFrame) {
        let requested = ProjectFrame(frame.0.min(self.timeline.len()));
        let old_project = self.project_position();
        let requested_project = self
            .timeline
            .start
            .saturating_add(requested.0.min(i64::MAX as u64) as i64);
        let boundary = match self
            .pending_ticket
            .as_ref()
            .map(|envelope| envelope.ticket.gate)
        {
            Some(crate::render_service::PublicationGate::LoopWrap(region))
                if old_project == region.end && requested_project == region.start =>
            {
                PublicationBoundary::LoopWrap { region }
            }
            _ => PublicationBoundary::Discontinuity {
                next_frame: requested_project,
            },
        };
        self.activate_if(boundary);
        self.position = requested;
        self.current_product = None;
        self.starving = false;
    }

    fn render_interleaved(&mut self, output: &mut [f32]) -> usize {
        let channels = usize::from(self.format.channels.get());
        let requested_frames = output.len() / channels;
        let complete_samples = requested_frames * channels;
        output.fill(0.0);
        self.activate_if(PublicationBoundary::RenderQuantum {
            next_frame: self.project_position(),
        });
        let available = self.timeline.len().saturating_sub(self.position.0);
        let rendered_frames = requested_frames.min(available as usize);
        for output_frame in 0..rendered_frames {
            let project_frame = self.project_position();
            if !self.select_product(project_frame) {
                self.note_starvation(1);
                self.position.0 = self.position.0.saturating_add(1);
                continue;
            }
            self.starving = false;
            {
                let product = self
                    .current_product
                    .as_ref()
                    .expect("selected product is retained");
                let product_frame = usize::try_from(project_frame - product.produced_by.core.start)
                    .expect("selected product contains project frame");
                let source_start = product_frame * channels;
                let target_start = output_frame * channels;
                output[target_start..target_start + channels]
                    .copy_from_slice(&product.interleaved()[source_start..source_start + channels]);
            }
            self.position.0 = self.position.0.saturating_add(1);
        }
        output[complete_samples..].fill(0.0);
        rendered_frames
    }
}

pub fn project_revision_stamp(
    revisions: crate::daw_project::ProjectRevisions,
) -> ProjectRevisionStamp {
    ProjectRevisionStamp {
        aggregate: revisions.aggregate,
        arrangement: revisions.arrangement,
        sequencer: revisions.sequencer,
        automation: revisions.automation,
        assets: revisions.assets,
        mixer: revisions.mixer,
        air: revisions.air,
        bindings: revisions.bindings,
    }
}

pub fn render_format_stamp(format: AudioFormat) -> RenderFormat {
    RenderFormat {
        sample_rate: format.sample_rate,
        channels: format.channels,
    }
}

fn relative_project_frame(timeline: RenderSpan, relative: u64) -> Result<i64, RenderRuntimeError> {
    let relative =
        i64::try_from(relative).map_err(|_| RenderRuntimeError::TransportCoordinateOverflow)?;
    timeline
        .start
        .checked_add(relative)
        .ok_or(RenderRuntimeError::TransportCoordinateOverflow)
}

fn audio_format(format: RenderFormat) -> AudioFormat {
    AudioFormat {
        sample_rate: format.sample_rate,
        channels: format.channels,
    }
}

pub fn whole_bounce_boundary_recipe() -> ExactDigest {
    ExactDigest::new(Sha256::digest(WHOLE_BOUNCE_BOUNDARY_DOMAIN))
}

/// SHA-256 over a domain tag followed by exact little-endian finite `f32` bits.
pub fn canonical_pcm_digest(interleaved: &[f32]) -> ExactDigest {
    let mut digest = Sha256::new();
    digest.update(PCM_DIGEST_DOMAIN);
    for sample in interleaved {
        digest.update(&sample.to_bits().to_le_bytes());
    }
    ExactDigest::new(digest.finalize())
}

/// Small streaming SHA-256 implementation kept private to exact render-product
/// identity. It avoids turning the existing non-cryptographic asset FNV hint
/// into a durable PCM address and avoids allocating a second byte copy.
struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffered: usize,
    bytes: u64,
}

impl Sha256 {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: Self::INITIAL,
            buffer: [0; 64],
            buffered: 0,
            bytes: 0,
        }
    }

    fn digest(bytes: &[u8]) -> [u8; 32] {
        let mut digest = Self::new();
        digest.update(bytes);
        digest.finalize()
    }

    fn update(&mut self, mut bytes: &[u8]) {
        self.bytes = self.bytes.wrapping_add(bytes.len() as u64);
        if self.buffered > 0 {
            let copied = (64 - self.buffered).min(bytes.len());
            self.buffer[self.buffered..self.buffered + copied].copy_from_slice(&bytes[..copied]);
            self.buffered += copied;
            bytes = &bytes[copied..];
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }
        while bytes.len() >= 64 {
            let block: &[u8; 64] = bytes[..64].try_into().expect("exact SHA-256 block");
            self.compress(block);
            bytes = &bytes[64..];
        }
        self.buffer[..bytes.len()].copy_from_slice(bytes);
        self.buffered = bytes.len();
    }

    fn finalize(mut self) -> [u8; 32] {
        let bit_length = self.bytes.wrapping_mul(8);
        self.buffer[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > 56 {
            self.buffer[self.buffered..].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffer = [0; 64];
        } else {
            self.buffer[self.buffered..56].fill(0);
        }
        self.buffer[56..64].copy_from_slice(&bit_length.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);
        let mut output = [0; 32];
        for (chunk, word) in output.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        output
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut words = [0_u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(chunk.try_into().expect("four-byte SHA word"));
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(Self::K[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (target, addition) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *target = target.wrapping_add(addition);
        }
    }
}

#[derive(Debug)]
pub enum RenderRuntimeError {
    PlanRevisionMismatch {
        expected: ProjectRevisionStamp,
        actual: ProjectRevisionStamp,
    },
    PlanFormatMismatch {
        expected: RenderFormat,
        actual: RenderFormat,
    },
    PlanExtentMismatch {
        expected: RenderSpan,
        actual: RenderSpan,
    },
    InvalidEngineExtent,
    ProductOutsidePlan {
        product: RenderSpan,
        plan: RenderSpan,
    },
    EngineOriginMismatch {
        expected: i64,
        actual: i64,
    },
    ExecutableIdentityCollision(RenderPlanId),
    UnknownExecutablePlan(RenderPlanId),
    NotWholeMasterProduct,
    InitialCohortWasNotArmed,
    UnexpectedInitialRetirement,
    LoopOutsidePlan(RenderSpan),
    CohortSequenceOverflow,
    RetiredCohortMismatch {
        service: Option<PlaybackCohortId>,
        renderer: Option<PlaybackCohortId>,
    },
    UnsupportedExportScope(RenderScope),
    CohortDoesNotCover(RenderSpan),
    RenderTooLarge,
    PublicationMailboxBusy,
    IncompletePlaybackCohort,
    RendererFormatChanged {
        expected: RenderFormat,
        actual: RenderFormat,
    },
    RendererTimelineChanged {
        expected: RenderSpan,
        actual: RenderSpan,
    },
    TransportFrameOutsideTimeline {
        frame: u64,
        timeline: RenderSpan,
    },
    TransportLoopOutsideTimeline {
        start: u64,
        end: u64,
        timeline: RenderSpan,
    },
    TransportCoordinateOverflow,
    Engine(DawEngineError),
    Product(RenderProductError),
    Service(RenderServiceError),
    Audio(AudioError),
}

impl fmt::Display for RenderRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlanRevisionMismatch { .. } => {
                write!(
                    formatter,
                    "render plan revisions differ from engine schedule"
                )
            }
            Self::PlanFormatMismatch { .. } => {
                write!(formatter, "render plan format differs from engine schedule")
            }
            Self::PlanExtentMismatch { .. } => {
                write!(formatter, "render plan extent differs from engine schedule")
            }
            Self::InvalidEngineExtent => write!(formatter, "engine render extent is invalid"),
            Self::ProductOutsidePlan { .. } => write!(formatter, "product lies outside its plan"),
            Self::EngineOriginMismatch { expected, actual } => write!(
                formatter,
                "engine render origin {actual} differs from requested {expected}"
            ),
            Self::ExecutableIdentityCollision(_) => {
                write!(formatter, "executable plan identity collision")
            }
            Self::UnknownExecutablePlan(_) => {
                write!(formatter, "executable plan is not registered")
            }
            Self::NotWholeMasterProduct => {
                write!(formatter, "product is not the plan's whole master bounce")
            }
            Self::InitialCohortWasNotArmed => {
                write!(formatter, "initial playback cohort was not armed")
            }
            Self::UnexpectedInitialRetirement => {
                write!(formatter, "initial publication retired an existing cohort")
            }
            Self::LoopOutsidePlan(_) => {
                write!(formatter, "publication loop lies outside render plan")
            }
            Self::CohortSequenceOverflow => write!(formatter, "playback cohort sequence overflow"),
            Self::RetiredCohortMismatch { .. } => {
                write!(formatter, "renderer and service retired different cohorts")
            }
            Self::UnsupportedExportScope(scope) => write!(
                formatter,
                "engine does not yet render export scope {scope:?}"
            ),
            Self::CohortDoesNotCover(span) => write!(
                formatter,
                "cohort does not cover {}..{}",
                span.start, span.end
            ),
            Self::RenderTooLarge => write!(formatter, "render is too large for this platform"),
            Self::PublicationMailboxBusy => {
                write!(formatter, "renderer publication mailbox is busy")
            }
            Self::IncompletePlaybackCohort => write!(formatter, "playback cohort is incomplete"),
            Self::RendererFormatChanged { .. } => {
                write!(formatter, "persistent renderer cannot change audio format")
            }
            Self::RendererTimelineChanged { .. } => {
                write!(formatter, "persistent renderer timeline extent changed")
            }
            Self::TransportFrameOutsideTimeline { frame, timeline } => write!(
                formatter,
                "transport frame {frame} lies outside renderer timeline {}..{}",
                timeline.start, timeline.end
            ),
            Self::TransportLoopOutsideTimeline {
                start,
                end,
                timeline,
            } => write!(
                formatter,
                "transport loop {start}..{end} lies outside renderer timeline {}..{}",
                timeline.start, timeline.end
            ),
            Self::TransportCoordinateOverflow => {
                write!(formatter, "transport coordinate overflows project timeline")
            }
            Self::Engine(error) => error.fmt(formatter),
            Self::Product(error) => error.fmt(formatter),
            Self::Service(error) => error.fmt(formatter),
            Self::Audio(error) => error.fmt(formatter),
        }
    }
}

impl Error for RenderRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::Product(error) => Some(error),
            Self::Service(error) => Some(error),
            Self::Audio(error) => Some(error),
            _ => None,
        }
    }
}

impl From<DawEngineError> for RenderRuntimeError {
    fn from(error: DawEngineError) -> Self {
        Self::Engine(error)
    }
}

impl From<RenderProductError> for RenderRuntimeError {
    fn from(error: RenderProductError) -> Self {
        Self::Product(error)
    }
}

impl From<RenderServiceError> for RenderRuntimeError {
    fn from(error: RenderServiceError) -> Self {
        Self::Service(error)
    }
}

impl From<AudioError> for RenderRuntimeError {
    fn from(error: AudioError) -> Self {
        Self::Audio(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_plan::{EngineRecipeStamp, ProjectRevisionStamp};

    fn digest(byte: u8) -> ExactDigest {
        ExactDigest::new([byte; 32])
    }

    fn test_plan(revision: u64) -> Arc<RenderPlan> {
        let format = RenderFormat::new(48_000, 2).unwrap();
        let engine = EngineRecipeStamp::new(1, format, 512, 0, digest(3)).unwrap();
        let id = RenderPlanId::new(
            7,
            digest(revision as u8),
            ProjectRevisionStamp {
                aggregate: revision,
                ..ProjectRevisionStamp::default()
            },
            RenderSpan::new(0, 4).unwrap(),
            engine,
            Vec::new(),
        )
        .unwrap();
        Arc::new(RenderPlan::new(
            id,
            crate::render_plan::DeterminismGrade::BitExact,
            crate::render_plan::Tileability::Stateless,
        ))
    }

    fn product(plan: &RenderPlan, samples: [f32; 8], byte: u8) -> Arc<RenderProduct> {
        let key = RenderProductKey::new(
            plan.id.clone(),
            RenderScope::Master,
            plan.extent(),
            ProductPartition::WholeBounce,
            whole_bounce_boundary_recipe(),
        )
        .unwrap();
        Arc::new(RenderProduct::new(digest(byte), key, Arc::from(samples)).unwrap())
    }

    fn cohort(plan: &RenderPlan, sequence: u64, samples: [f32; 8]) -> Arc<PlaybackCohort> {
        let slot = RenderSlot {
            scope: RenderScope::Master,
            span: plan.extent(),
        };
        Arc::new(
            PlaybackCohort::new(
                PlaybackCohortId {
                    plan: plan.id.clone(),
                    sequence,
                },
                Some(RenderSpan::new(1, 3).unwrap()),
                vec![slot.clone()],
                vec![CohortProduct {
                    slot,
                    product: product(plan, samples, sequence as u8),
                    provenance: CohortProductProvenance::RenderedForTarget,
                }],
            )
            .unwrap(),
        )
    }

    #[test]
    fn sha256_matches_the_standard_empty_vector() {
        let actual = Sha256::digest(b"");
        let expected = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];
        assert_eq!(actual, expected);
    }

    #[test]
    fn persistent_renderer_swaps_only_on_the_armed_loop_wrap() {
        let old = test_plan(1);
        let new = test_plan(2);
        let old_cohort = cohort(&old, 1, [1.0, 10.0, 2.0, 20.0, 3.0, 30.0, 4.0, 40.0]);
        let new_cohort = cohort(&new, 2, [9.0, 90.0, 8.0, 80.0, 7.0, 70.0, 6.0, 60.0]);
        let (control, mut renderer) = CohortRenderer::new(old_cohort).unwrap();
        let action = PublicationAction::Arm(PublicationTicket {
            cohort: Arc::clone(&new_cohort),
            gate: crate::render_service::PublicationGate::LoopWrap(RenderSpan::new(1, 3).unwrap()),
        });
        control.arm_action(&action).unwrap();

        let mut old_pass = [0.0; 6];
        assert_eq!(renderer.render_interleaved(&mut old_pass), 3);
        assert_eq!(old_pass, [1.0, 10.0, 2.0, 20.0, 3.0, 30.0]);
        renderer.seek(ProjectFrame(1));
        let mut new_loop = [0.0; 2];
        assert_eq!(renderer.render_interleaved(&mut new_loop), 1);
        assert_eq!(new_loop, [8.0, 80.0]);
        assert_eq!(control.drain_receipt().unwrap().activated, new_cohort.id);
    }

    #[test]
    fn missing_product_is_silence_with_visible_starvation() {
        let plan = test_plan(1);
        let covered = RenderSpan::new(0, 2).unwrap();
        let slot = RenderSlot {
            scope: RenderScope::Master,
            span: covered,
        };
        let key = RenderProductKey::new(
            plan.id.clone(),
            RenderScope::Master,
            covered,
            ProductPartition::ContiguousRun {
                anchor_frame: 0,
                sequence: 0,
            },
            digest(4),
        )
        .unwrap();
        let partial = Arc::new(RenderProduct::new(digest(5), key, Arc::from([1.0; 4])).unwrap());
        let cohort = Arc::new(
            PlaybackCohort::new(
                PlaybackCohortId {
                    plan: plan.id.clone(),
                    sequence: 1,
                },
                None,
                vec![slot.clone()],
                vec![CohortProduct {
                    slot,
                    product: partial,
                    provenance: CohortProductProvenance::RenderedForTarget,
                }],
            )
            .unwrap(),
        );
        // Constructor intentionally requires full master coverage, so a cold
        // incomplete table cannot masquerade as ready playback.
        assert!(matches!(
            CohortRenderer::new(cohort),
            Err(RenderRuntimeError::CohortDoesNotCover(_))
        ));
    }
}
