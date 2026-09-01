//! Executable bridge from immutable DAW schedules to persistent playback.
//!
//! The only sound-producing kernel in this module is [`DawEngineSchedule`]. A
//! whole bounce is rendered from that kernel into an immutable product, the
//! persistent renderer plays that exact product, and export pins either reuse
//! those samples or invoke the same frozen schedule. Tiles change only the
//! product partition, never publication, playback, or export truth.
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
//!    [`RenderRuntime::stage_whole_bounce`] or [`RenderRuntime::stage_tile_cohort`], then pass its action to
//!    [`CohortRendererControl::arm_action`]. Old PCM remains audible until the
//!    renderer returns a receipt at the armed boundary.
//! 4. Poll [`RenderRuntime::poll_publication`] on the control thread. After an
//!    acknowledgement, call [`RenderRuntime::arm_staged`] in case a newer
//!    worker completion arrived while the prior publication was in flight.
//!
//! Partial target coverage remains scheduler state and never enters realtime
//! playback. Format or timeline-extent changes intentionally reject an in-place swap:
//! `TransportHandle` freezes both facts. The controller recreates the host for
//! those uncommon structural changes; ordinary edits never replace it.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::audio::{
    AudioError, AudioFormat, ProjectAudio, ProjectFrame, ProjectRenderer, TransportMode,
    TransportSnapshot,
};
use crate::daw_engine::{DawEngineError, DawEngineSchedule};
use crate::daw_render::RenderCancellation;
use crate::render_plan::{
    ExactDigest, OutputTailPolicy, ProjectRevisionStamp, RenderFormat, RenderPlan, RenderPlanId,
    RenderScope, RenderSpan,
};
use crate::render_products::{
    CohortProduct, CohortProductProvenance, PlaybackCohort, PlaybackCohortId, ProductPartition,
    RenderProduct, RenderProductCatalog, RenderProductError, RenderProductKey, RenderSlot,
};
use crate::render_service::{
    AuditionPin, ExportPin, ExportPinSource, PublicationAction, PublicationBoundary,
    PublicationTicket, PublicationTransport, RenderFailure, RenderService, RenderServiceError,
    RenderServiceStatus,
};
use crate::render_tiles::{RenderTileError, TileCohortDraft, TileRenderSpec};

const PCM_DIGEST_DOMAIN: &[u8] = b"audec:canonical-f32le-pcm:v1\0";
const WHOLE_BOUNCE_BOUNDARY_DOMAIN: &[u8] = b"audec:whole-bounce-boundary:v1";

/// Stable session-local owner of a scoped audition. Workspace views, coverage
/// comparisons, and headless clients map their typed IDs into this pair.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AuditionOwner {
    pub namespace: u128,
    pub local: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AuditionSubject {
    Source,
    Construction,
    Residual,
    Excess,
    Harmonic,
    Transient,
    Custom { namespace: u128, local: u64 },
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AuditionMix {
    /// The scoped product is the only audible signal inside its span.
    Replace,
    /// Add the scoped product to the normal master inside its span.
    Overlay,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TimelineAuditionId {
    pub owner: AuditionOwner,
    pub revision: u64,
    pub content: ExactDigest,
}

/// Immutable PCM aligned to signed project frames. This is a derived audition
/// product, not another transport or render graph.
#[derive(Clone, Debug)]
pub struct TimelineAudition {
    pub id: TimelineAuditionId,
    pub subject: AuditionSubject,
    pub mix: AuditionMix,
    pub span: RenderSpan,
    pub format: RenderFormat,
    source_cohort: Option<PlaybackCohortId>,
    interleaved: Arc<[f32]>,
}

impl TimelineAudition {
    pub fn new(
        id: TimelineAuditionId,
        subject: AuditionSubject,
        mix: AuditionMix,
        span: RenderSpan,
        format: RenderFormat,
        interleaved: Arc<[f32]>,
    ) -> Result<Self, RenderRuntimeError> {
        let channels = usize::from(format.channels.get());
        let expected = usize::try_from(span.len())
            .ok()
            .and_then(|frames| frames.checked_mul(channels))
            .ok_or(RenderRuntimeError::RenderTooLarge)?;
        if interleaved.len() != expected {
            return Err(RenderRuntimeError::AuditionSampleCount {
                expected,
                actual: interleaved.len(),
            });
        }
        if let Some(index) = interleaved.iter().position(|sample| !sample.is_finite()) {
            return Err(RenderRuntimeError::AuditionNonFiniteSample { index });
        }
        Ok(Self {
            id,
            subject,
            mix,
            span,
            format,
            source_cohort: None,
            interleaved,
        })
    }

    pub fn interleaved(&self) -> &[f32] {
        &self.interleaved
    }

    /// Exact published cohort retained by runtime-materialized auditions.
    /// `None` identifies caller-supplied analysis PCM created with [`Self::new`].
    pub fn source_cohort(&self) -> Option<&PlaybackCohortId> {
        self.source_cohort.as_ref()
    }
}

/// Metadata plus the actual frozen engine schedule it identifies.
#[derive(Clone, Debug)]
pub struct ExecutableRenderPlan {
    pub descriptor: Arc<RenderPlan>,
    pub schedule: Arc<DawEngineSchedule>,
    native_graph: Arc<crate::compiled_audio_graph::NativeDawGraph>,
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
        let native_graph = Arc::new(schedule.compile_native_graph(Arc::clone(&descriptor))?);
        Ok(Self {
            descriptor,
            schedule,
            native_graph,
        })
    }

    pub fn id(&self) -> &RenderPlanId {
        &self.descriptor.id
    }

    pub fn graph_diagnostics(&self) -> &[crate::compiled_audio_graph::GraphDiagnostic] {
        self.native_graph.graph().diagnostics()
    }

    pub fn graph_render_diagnostics(&self) -> &[crate::daw_render::RenderDiagnostic] {
        self.native_graph.render_diagnostics()
    }

    /// Execute one semantic product through the sole DAW engine.
    pub fn render_product(
        &self,
        scope: RenderScope,
        span: RenderSpan,
        partition: ProductPartition,
        boundary_recipe: ExactDigest,
        cancellation: &RenderCancellation,
    ) -> Result<Arc<RenderProduct>, RenderRuntimeError> {
        self.render_products(
            &[scope.clone()],
            span,
            partition,
            boundary_recipe,
            cancellation,
        )?
        .remove(&scope)
        .ok_or(RenderRuntimeError::MissingScopedEngineOutput(scope))
    }

    /// Render several scopes from one frozen source/mixer traversal.
    pub fn render_products(
        &self,
        scopes: &[RenderScope],
        span: RenderSpan,
        partition: ProductPartition,
        boundary_recipe: ExactDigest,
        cancellation: &RenderCancellation,
    ) -> Result<BTreeMap<RenderScope, Arc<RenderProduct>>, RenderRuntimeError> {
        if !self.descriptor.extent().contains_span(span) {
            return Err(RenderRuntimeError::ProductOutsidePlan {
                product: span,
                plan: self.descriptor.extent(),
            });
        }
        let rendered = self
            .native_graph
            .render_scopes(span, scopes, cancellation)?;
        let mut products = BTreeMap::new();
        for scope in scopes {
            let key = RenderProductKey::new(
                self.descriptor.id.clone(),
                scope.clone(),
                span,
                partition.clone(),
                boundary_recipe,
            )?;
            let pcm = rendered
                .outputs
                .get(scope)
                .cloned()
                .ok_or_else(|| RenderRuntimeError::MissingScopedEngineOutput(scope.clone()))?;
            let digest = canonical_pcm_digest(&pcm);
            products.insert(
                scope.clone(),
                Arc::new(RenderProduct::new(digest, key, pcm)?),
            );
        }
        Ok(products)
    }

    pub fn render_master_product(
        &self,
        span: RenderSpan,
        partition: ProductPartition,
        boundary_recipe: ExactDigest,
        cancellation: &RenderCancellation,
    ) -> Result<Arc<RenderProduct>, RenderRuntimeError> {
        self.render_product(
            RenderScope::Master,
            span,
            partition,
            boundary_recipe,
            cancellation,
        )
    }

    /// Materialize a fresh export pin without consulting mutable runtime
    /// publication state. This is the worker-side half of a controller-owned
    /// current-export job: the pin and executable are immutable, while the
    /// controller validates the captured generation again on completion.
    pub fn render_fresh_export_pin(
        &self,
        pin: &ExportPin,
        cancellation: &RenderCancellation,
    ) -> Result<RuntimeRenderedAudio, RenderRuntimeError> {
        Ok(self
            .render_fresh_export_pin_with_diagnostics(pin, cancellation)?
            .rendered)
    }

    /// Fresh export plus the exact diagnostics emitted by that same engine
    /// traversal. Controller/UI integrity gates use this variant so successful
    /// finite silence cannot hide missing or mismatched project material.
    pub fn render_fresh_export_pin_with_diagnostics(
        &self,
        pin: &ExportPin,
        cancellation: &RenderCancellation,
    ) -> Result<RuntimeDiagnosedExport, RenderRuntimeError> {
        if pin.plan.id != *self.id() || !matches!(pin.source, ExportPinSource::FreshPlanRender) {
            return Err(RenderRuntimeError::ExportPinMismatch);
        }
        validate_export_pin(pin)?;
        let result = self.native_graph.render_scopes(
            pin.maximum_output_span,
            std::slice::from_ref(&pin.scope),
            cancellation,
        )?;
        let engine_diagnostics = self.schedule.engine_diagnostics().to_vec().into();
        let render_diagnostics = self.native_graph.render_diagnostics().to_vec().into();
        let graph_diagnostics = self.native_graph.graph().diagnostics().to_vec().into();
        let rendered = result
            .outputs
            .get(&pin.scope)
            .cloned()
            .ok_or_else(|| RenderRuntimeError::MissingScopedEngineOutput(pin.scope.clone()))?
            .to_vec();
        Ok(RuntimeDiagnosedExport {
            rendered: finish_export(pin, rendered)?,
            engine_diagnostics,
            render_diagnostics,
            graph_diagnostics,
        })
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

    /// Execute one planned tile through the same frozen engine schedule used
    /// by whole bounce. The explicit context is rendered first and cropped to
    /// the immutable core only after the engine returns; no tile-local DSP
    /// path or boundary approximation is permitted here.
    pub fn render_tile(
        &self,
        spec: &TileRenderSpec,
        cancellation: &RenderCancellation,
    ) -> Result<Arc<RenderProduct>, RenderRuntimeError> {
        if spec.plan != self.descriptor.id {
            return Err(RenderRuntimeError::TilePlanMismatch {
                expected: self.descriptor.id.clone(),
                actual: spec.plan.clone(),
            });
        }
        if !self.descriptor.extent().contains_span(spec.context) {
            return Err(RenderRuntimeError::TileContextOutsidePlan {
                context: spec.context,
                plan: self.descriptor.extent(),
            });
        }
        if !spec.context.contains_span(spec.core) {
            return Err(RenderRuntimeError::TileContextDoesNotCoverCore {
                context: spec.context,
                core: spec.core,
            });
        }
        let rendered = self.native_graph.render_scopes(
            spec.context,
            std::slice::from_ref(&spec.scope),
            cancellation,
        )?;
        let channels = usize::from(self.descriptor.format().channels.get());
        let source_frame = usize::try_from(spec.core.start - spec.context.start)
            .map_err(|_| RenderRuntimeError::RenderTooLarge)?;
        let core_frames =
            usize::try_from(spec.core.len()).map_err(|_| RenderRuntimeError::RenderTooLarge)?;
        let source_start = source_frame
            .checked_mul(channels)
            .ok_or(RenderRuntimeError::RenderTooLarge)?;
        let sample_count = core_frames
            .checked_mul(channels)
            .ok_or(RenderRuntimeError::RenderTooLarge)?;
        let source_end = source_start
            .checked_add(sample_count)
            .ok_or(RenderRuntimeError::RenderTooLarge)?;
        let source = rendered
            .outputs
            .get(&spec.scope)
            .cloned()
            .ok_or_else(|| RenderRuntimeError::MissingScopedEngineOutput(spec.scope.clone()))?;
        let core_pcm: Arc<[f32]> = source
            .get(source_start..source_end)
            .ok_or(RenderRuntimeError::TileEngineOutputTooShort)?
            .to_vec()
            .into();
        let key = spec.product_key()?;
        let digest = canonical_pcm_digest(&core_pcm);
        Ok(Arc::new(RenderProduct::new(digest, key, core_pcm)?))
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

    /// Stage a complete tiled master manifest through the same cohort service
    /// as whole bounce. Reused and freshly rendered entries retain their exact
    /// derivation receipts while the catalog shares identical PCM allocations.
    /// Export pins this cohort verbatim after publication.
    pub fn stage_tile_cohort(
        &mut self,
        draft: TileCohortDraft,
        transport: PublicationTransport,
    ) -> Result<PublicationAction, RenderRuntimeError> {
        if let Some(loop_region) = transport.loop_region {
            if !draft.plan.compiled_extent.contains_span(loop_region) {
                return Err(RenderRuntimeError::LoopOutsidePlan(loop_region));
            }
        }
        let mut products = Vec::with_capacity(draft.products.len());
        for entry in draft.products {
            products.push(CohortProduct {
                slot: entry.slot,
                product: self.adopt_product(entry.product)?,
                provenance: entry.provenance,
            });
        }
        self.next_cohort_sequence = self
            .next_cohort_sequence
            .checked_add(1)
            .ok_or(RenderRuntimeError::CohortSequenceOverflow)?;
        let cohort = Arc::new(PlaybackCohort::new(
            PlaybackCohortId {
                plan: draft.plan.clone(),
                sequence: self.next_cohort_sequence,
            },
            // Tile priority was chosen from the loop observed when work began,
            // but every cohort covers the entire plan. Publication therefore
            // follows the loop observed at completion; otherwise a harmless
            // loop edit could leave a complete candidate staged forever.
            transport.loop_region,
            draft.required,
            products,
        )?);
        if !cohort.covers(&RenderScope::Master, draft.plan.compiled_extent) {
            return Err(RenderRuntimeError::CohortDoesNotCover(
                draft.plan.compiled_extent,
            ));
        }
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
        self.bootstrap_staged_action(action)
    }

    /// Bootstrap the first renderer from one complete tiled cohort. This is
    /// the cold/restart counterpart to [`Self::bootstrap_renderer`]: cached and
    /// freshly rendered tiles still pass through the ordinary cohort service,
    /// coverage validation, catalog adoption, and renderer constructor.
    pub fn bootstrap_tile_renderer(
        &mut self,
        draft: TileCohortDraft,
    ) -> Result<(CohortRendererControl, CohortRenderer), RenderRuntimeError> {
        let action = self.stage_tile_cohort(draft, PublicationTransport::default())?;
        self.bootstrap_staged_action(action)
    }

    fn bootstrap_staged_action(
        &mut self,
        action: PublicationAction,
    ) -> Result<(CohortRendererControl, CohortRenderer), RenderRuntimeError> {
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
        match receipt.outcome {
            EnvelopeOutcome::Pending => Err(RenderRuntimeError::UnexpectedPendingReceipt),
            EnvelopeOutcome::Cancelled => {
                if receipt.retired.is_some() {
                    return Err(RenderRuntimeError::CancelledPublicationRetiredCohort);
                }
                self.service
                    .reject_publication(&receipt.cohort, "superseded before activation")?;
                Ok(Some(PublicationCompletion {
                    outcome: PublicationCompletionOutcome::Cancelled {
                        cohort: receipt.cohort,
                    },
                }))
            }
            EnvelopeOutcome::Activated => {
                let service_retired = self.service.acknowledge_publication(&receipt.cohort)?;
                let service_retired_id = service_retired.as_ref().map(|cohort| cohort.id.clone());
                let renderer_retired_id = receipt.retired.as_ref().map(|cohort| cohort.id.clone());
                if service_retired_id != renderer_retired_id {
                    return Err(RenderRuntimeError::RetiredCohortMismatch {
                        service: service_retired_id,
                        renderer: renderer_retired_id,
                    });
                }
                Ok(Some(PublicationCompletion {
                    outcome: PublicationCompletionOutcome::Activated {
                        active: receipt.cohort,
                        retired: service_retired_id,
                    },
                }))
            }
        }
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

    pub fn pin_active_audition(
        &self,
        scope: RenderScope,
        span: RenderSpan,
    ) -> Result<AuditionPin, RenderRuntimeError> {
        Ok(self.service.pin_active_audition(scope, span)?)
    }

    /// Materialize PCM from an immutable published-product pin. The returned
    /// audition carries the pinned cohort identity and a digest of the exact
    /// copied samples, so a later loop-boundary swap cannot relabel its source.
    pub fn render_audition_pin(
        &self,
        pin: &AuditionPin,
        owner: AuditionOwner,
        subject: AuditionSubject,
        mix: AuditionMix,
    ) -> Result<Arc<TimelineAudition>, RenderRuntimeError> {
        if pin.plan.id != pin.cohort.id.plan {
            return Err(RenderRuntimeError::AuditionPinMismatch);
        }
        let expected = pin
            .cohort
            .product_ids_covering(&pin.scope, pin.span)
            .ok_or(RenderRuntimeError::CohortDoesNotCover(pin.span))?;
        if expected.as_slice() != pin.products.as_ref() {
            return Err(RenderRuntimeError::AuditionPinMismatch);
        }
        let interleaved: Arc<[f32]> = copy_cohort_pcm(&pin.cohort, &pin.scope, pin.span)?.into();
        let mut audition = TimelineAudition::new(
            TimelineAuditionId {
                owner,
                revision: pin.plan.id.revisions.aggregate,
                content: canonical_pcm_digest(&interleaved),
            },
            subject,
            mix,
            pin.span,
            pin.plan.format(),
            interleaved,
        )?;
        audition.source_cohort = Some(pin.cohort.id.clone());
        Ok(Arc::new(audition))
    }

    /// Resolve an export pin to finite PCM. This is the adapter for the current
    /// `export_project_audio_to_wav` API; a later streaming encoder can consume
    /// the same pin without changing its identity.
    pub fn render_export_pin(
        &self,
        pin: &ExportPin,
        cancellation: &RenderCancellation,
    ) -> Result<RuntimeRenderedAudio, RenderRuntimeError> {
        validate_export_pin(pin)?;
        let rendered = match &pin.source {
            ExportPinSource::FreshPlanRender => {
                let executable = self.executable_plan(&pin.plan.id)?;
                return executable.render_fresh_export_pin(pin, cancellation);
            }
            ExportPinSource::PublishedProducts { cohort, products } => {
                if cohort.id.plan != pin.plan.id
                    || cohort
                        .product_ids_covering(&pin.scope, pin.maximum_output_span)
                        .as_deref()
                        != Some(products.as_ref())
                {
                    return Err(RenderRuntimeError::ExportPinMismatch);
                }
                copy_cohort_pcm(cohort, &pin.scope, pin.maximum_output_span)?
            }
        };
        finish_export(pin, rendered)
    }
}

fn validate_export_pin(pin: &ExportPin) -> Result<(), RenderRuntimeError> {
    let expected_maximum = pin
        .tail
        .maximum_output_span(pin.span)
        .map_err(|_| RenderRuntimeError::ExportPinMismatch)?;
    if expected_maximum != pin.maximum_output_span
        || !pin.plan.extent().contains_span(pin.maximum_output_span)
    {
        return Err(RenderRuntimeError::ExportPinMismatch);
    }
    Ok(())
}

fn finish_export(
    pin: &ExportPin,
    mut rendered: Vec<f32>,
) -> Result<RuntimeRenderedAudio, RenderRuntimeError> {
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

#[derive(Clone, Debug)]
pub struct RuntimeDiagnosedExport {
    pub rendered: RuntimeRenderedAudio,
    pub engine_diagnostics: Arc<[crate::daw_engine::EngineDiagnostic]>,
    pub render_diagnostics: Arc<[crate::daw_render::RenderDiagnostic]>,
    pub graph_diagnostics: Arc<[crate::compiled_audio_graph::GraphDiagnostic]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationCompletion {
    pub outcome: PublicationCompletionOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicationCompletionOutcome {
    Activated {
        active: PlaybackCohortId,
        retired: Option<PlaybackCohortId>,
    },
    Cancelled {
        cohort: PlaybackCohortId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnvelopeOutcome {
    Pending,
    Activated,
    Cancelled,
}

struct PublicationEnvelope {
    ticket: PublicationTicket,
    retired: Option<Arc<PlaybackCohort>>,
    outcome: EnvelopeOutcome,
}

enum AuditionCommand {
    Set(Option<Arc<TimelineAudition>>),
    ClearIfActive(TimelineAuditionId),
}

struct AuditionEnvelope {
    command: AuditionCommand,
    applied: bool,
    active: Option<TimelineAuditionId>,
    retired: Option<Arc<TimelineAudition>>,
}

/// One-producer/one-consumer mailbox. The control thread allocates envelopes;
/// the realtime renderer only moves their raw pointers and returns the same
/// allocation for acknowledgement/reclamation on the control thread.
struct PublicationMailbox {
    incoming: AtomicPtr<PublicationEnvelope>,
    receipt: AtomicPtr<PublicationEnvelope>,
    cancel_through_sequence: AtomicU64,
    audition_incoming: AtomicPtr<AuditionEnvelope>,
    audition_receipt: AtomicPtr<AuditionEnvelope>,
    /// Control-side desired token. The realtime thread never locks this; it
    /// prevents an obsolete exact-clear from replacing a newer pending Set in
    /// the single-slot mailbox.
    desired_audition: Mutex<Option<TimelineAuditionId>>,
}

impl PublicationMailbox {
    fn new() -> Self {
        Self {
            incoming: AtomicPtr::new(ptr::null_mut()),
            receipt: AtomicPtr::new(ptr::null_mut()),
            cancel_through_sequence: AtomicU64::new(0),
            audition_incoming: AtomicPtr::new(ptr::null_mut()),
            audition_receipt: AtomicPtr::new(ptr::null_mut()),
            desired_audition: Mutex::new(None),
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
        for slot in [&self.audition_incoming, &self.audition_receipt] {
            let pointer = slot.swap(ptr::null_mut(), Ordering::AcqRel);
            if !pointer.is_null() {
                // SAFETY: identical one-owner mailbox discipline to the
                // publication slots above.
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
    currently_starving: AtomicBool,
}

impl RendererCounters {
    fn new() -> Self {
        Self {
            starvation_events: AtomicU64::new(0),
            starved_frames: AtomicU64::new(0),
            publication_pending: AtomicBool::new(false),
            currently_starving: AtomicBool::new(false),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CohortRendererStatus {
    pub starvation_events: u64,
    pub starved_frames: u64,
    pub publication_queued: bool,
    pub receipt_waiting: bool,
    /// Current quantum ended without a master product at the renderer position.
    /// Lifetime counters remain available after recovery.
    pub currently_starving: bool,
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
            outcome: EnvelopeOutcome::Pending,
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
            cohort: envelope.ticket.cohort.id.clone(),
            outcome: envelope.outcome,
            retired: envelope.retired,
        })
    }

    /// Mark an armed cohort obsolete without mutating active audio. The
    /// realtime renderer returns a cancellation receipt at its next call; the
    /// control side then rejects the service publication and arms the newest
    /// staged cohort.
    pub fn cancel_publication(&self, cohort: &PlaybackCohortId) {
        self.mailbox
            .cancel_through_sequence
            .fetch_max(cohort.sequence, Ordering::Release);
    }

    pub fn set_timeline_audition(
        &self,
        audition: Arc<TimelineAudition>,
    ) -> Result<(), RenderRuntimeError> {
        if audition.format != self.format {
            return Err(RenderRuntimeError::AuditionFormatChanged {
                expected: self.format,
                actual: audition.format,
            });
        }
        if !self.timeline.contains_span(audition.span) {
            return Err(RenderRuntimeError::AuditionOutsideTimeline {
                audition: audition.span,
                timeline: self.timeline,
            });
        }
        *self
            .mailbox
            .desired_audition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(audition.id);
        self.queue_audition(AuditionCommand::Set(Some(audition)))
    }

    /// Clear only the requesting pane's audition. A stale pane cannot silence
    /// a newer audition owned by another pane.
    pub fn clear_timeline_audition(
        &self,
        audition: TimelineAuditionId,
    ) -> Result<(), RenderRuntimeError> {
        let mut desired = self
            .mailbox
            .desired_audition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *desired != Some(audition) {
            return Ok(());
        }
        *desired = None;
        drop(desired);
        self.queue_audition(AuditionCommand::ClearIfActive(audition))
    }

    fn queue_audition(&self, command: AuditionCommand) -> Result<(), RenderRuntimeError> {
        let pointer = Box::into_raw(Box::new(AuditionEnvelope {
            command,
            applied: false,
            active: None,
            retired: None,
        }));
        let replaced = self
            .mailbox
            .audition_incoming
            .swap(pointer, Ordering::AcqRel);
        if !replaced.is_null() {
            // SAFETY: swap transferred the not-yet-observed prior request back
            // to control. Latest pane intent wins before the audio side sees it.
            unsafe { drop(Box::from_raw(replaced)) };
        }
        Ok(())
    }

    pub fn drain_audition_receipt(&self) -> Option<TimelineAuditionReceipt> {
        let pointer = self
            .mailbox
            .audition_receipt
            .swap(ptr::null_mut(), Ordering::AcqRel);
        if pointer.is_null() {
            return None;
        }
        // SAFETY: swapping to null transfers unique ownership to control.
        let envelope = unsafe { Box::from_raw(pointer) };
        Some(TimelineAuditionReceipt {
            applied: envelope.applied,
            active: envelope.active,
            retired: envelope.retired.map(|audition| audition.id),
        })
    }

    pub fn status(&self) -> CohortRendererStatus {
        CohortRendererStatus {
            starvation_events: self.counters.starvation_events.load(Ordering::Acquire),
            starved_frames: self.counters.starved_frames.load(Ordering::Acquire),
            publication_queued: self.counters.publication_pending.load(Ordering::Acquire),
            receipt_waiting: !self.mailbox.receipt.load(Ordering::Acquire).is_null(),
            currently_starving: self.counters.currently_starving.load(Ordering::Acquire),
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
    pub cohort: PlaybackCohortId,
    outcome: EnvelopeOutcome,
    pub retired: Option<Arc<PlaybackCohort>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineAuditionReceipt {
    pub applied: bool,
    pub active: Option<TimelineAuditionId>,
    pub retired: Option<TimelineAuditionId>,
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
    active_audition: Option<Arc<TimelineAudition>>,
    pending_ticket: Option<Box<PublicationEnvelope>>,
    pending_receipt: Option<Box<PublicationEnvelope>>,
    pending_audition_receipt: Option<Box<AuditionEnvelope>>,
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
            active_audition: None,
            pending_ticket: None,
            pending_receipt: None,
            pending_audition_receipt: None,
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
        let cancelled = self.pending_ticket.as_ref().is_some_and(|envelope| {
            envelope.ticket.cohort.id.sequence
                <= self.mailbox.cancel_through_sequence.load(Ordering::Acquire)
        });
        if cancelled {
            let mut envelope = self
                .pending_ticket
                .take()
                .expect("cancelled publication ticket exists");
            envelope.outcome = EnvelopeOutcome::Cancelled;
            self.pending_receipt = Some(envelope);
            self.counters
                .publication_pending
                .store(false, Ordering::Release);
            self.flush_receipt();
            return;
        }
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
        envelope.outcome = EnvelopeOutcome::Activated;
        self.current_product = None;
        self.starving = false;
        self.counters
            .currently_starving
            .store(false, Ordering::Release);
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
            .currently_starving
            .store(true, Ordering::Release);
        self.counters
            .starved_frames
            .fetch_add(frames, Ordering::Relaxed);
    }

    fn apply_audition_command(&mut self) {
        self.flush_audition_receipt();
        if self.pending_audition_receipt.is_some() {
            return;
        }
        let pointer = self
            .mailbox
            .audition_incoming
            .swap(ptr::null_mut(), Ordering::AcqRel);
        if pointer.is_null() {
            return;
        }
        // SAFETY: swapping to null transfers the unique envelope allocation.
        let mut envelope = unsafe { Box::from_raw(pointer) };
        match &mut envelope.command {
            AuditionCommand::Set(next) => {
                let next = next.take();
                envelope.retired = std::mem::replace(&mut self.active_audition, next);
                envelope.applied = true;
            }
            AuditionCommand::ClearIfActive(audition) => {
                if self
                    .active_audition
                    .as_ref()
                    .is_some_and(|active| active.id == *audition)
                {
                    envelope.retired = self.active_audition.take();
                    envelope.applied = true;
                }
            }
        }
        envelope.active = self.active_audition.as_ref().map(|audition| audition.id);
        self.pending_audition_receipt = Some(envelope);
        self.flush_audition_receipt();
    }

    fn flush_audition_receipt(&mut self) {
        let Some(envelope) = self.pending_audition_receipt.take() else {
            return;
        };
        let pointer = Box::into_raw(envelope);
        if self
            .mailbox
            .audition_receipt
            .compare_exchange(
                ptr::null_mut(),
                pointer,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_err()
        {
            // SAFETY: failed publication leaves unique ownership here.
            self.pending_audition_receipt = Some(unsafe { Box::from_raw(pointer) });
        }
    }

    fn mix_audition_frame(&self, project_frame: i64, output: &mut [f32]) {
        let Some(audition) = self
            .active_audition
            .as_ref()
            .filter(|audition| audition.span.contains(project_frame))
        else {
            return;
        };
        let channels = usize::from(self.format.channels.get());
        let frame = usize::try_from(project_frame - audition.span.start)
            .expect("audition span contains project frame");
        let start = frame * channels;
        let source = &audition.interleaved()[start..start + channels];
        match audition.mix {
            AuditionMix::Replace => output.copy_from_slice(source),
            AuditionMix::Overlay => {
                for (target, addition) in output.iter_mut().zip(source) {
                    let mixed = *target + *addition;
                    *target = if mixed.is_finite() { mixed } else { 0.0 };
                }
            }
        }
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
        self.apply_audition_command();
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
        self.counters
            .currently_starving
            .store(false, Ordering::Release);
    }

    fn control_boundary(&mut self, mode: TransportMode) {
        self.apply_audition_command();
        if mode != TransportMode::Playing {
            // Paused/stopped transport still produces silent adapter frames.
            // Treat that nonrolling boundary as coherent for every gate so a
            // loop-wrap ticket cannot remain stranded until the next Play.
            self.activate_if(PublicationBoundary::Stopped);
        } else {
            // Pull cancellation/new-ticket state promptly; the subsequent
            // render quantum or seek still decides the audible swap boundary.
            self.poll_ticket();
        }
    }

    fn render_interleaved(&mut self, output: &mut [f32]) -> usize {
        self.apply_audition_command();
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
            if self.select_product(project_frame) {
                self.starving = false;
                self.counters
                    .currently_starving
                    .store(false, Ordering::Release);
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
            } else {
                self.note_starvation(1);
            }
            let target_start = output_frame * channels;
            self.mix_audition_frame(
                project_frame,
                &mut output[target_start..target_start + channels],
            );
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
        sample_kits: revisions.sample_kits,
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
            } else {
                return;
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
    TilePlanMismatch {
        expected: RenderPlanId,
        actual: RenderPlanId,
    },
    UnsupportedTileScope(RenderScope),
    MissingScopedEngineOutput(RenderScope),
    TileContextOutsidePlan {
        context: RenderSpan,
        plan: RenderSpan,
    },
    TileContextDoesNotCoverCore {
        context: RenderSpan,
        core: RenderSpan,
    },
    TileEngineOutputTooShort,
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
    UnexpectedPendingReceipt,
    CancelledPublicationRetiredCohort,
    UnsupportedExportScope(RenderScope),
    ExportPinMismatch,
    CohortDoesNotCover(RenderSpan),
    RenderTooLarge,
    AuditionSampleCount {
        expected: usize,
        actual: usize,
    },
    AuditionNonFiniteSample {
        index: usize,
    },
    AuditionFormatChanged {
        expected: RenderFormat,
        actual: RenderFormat,
    },
    AuditionOutsideTimeline {
        audition: RenderSpan,
        timeline: RenderSpan,
    },
    AuditionMailboxBusy,
    AuditionPinMismatch,
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
    Graph(crate::compiled_audio_graph::GraphExecutionError),
    Tile(RenderTileError),
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
            Self::TilePlanMismatch { .. } => {
                write!(formatter, "tile plan differs from executable render plan")
            }
            Self::UnsupportedTileScope(scope) => {
                write!(formatter, "engine does not yet render tile scope {scope:?}")
            }
            Self::MissingScopedEngineOutput(scope) => {
                write!(formatter, "engine omitted requested render scope {scope:?}")
            }
            Self::TileContextOutsidePlan { .. } => {
                write!(formatter, "tile context lies outside its render plan")
            }
            Self::TileContextDoesNotCoverCore { .. } => {
                write!(formatter, "tile context does not cover its core")
            }
            Self::TileEngineOutputTooShort => {
                write!(formatter, "engine returned too little PCM for tile core")
            }
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
            Self::UnexpectedPendingReceipt => {
                write!(
                    formatter,
                    "renderer returned a pending publication as a receipt"
                )
            }
            Self::CancelledPublicationRetiredCohort => {
                write!(
                    formatter,
                    "cancelled publication unexpectedly retired active audio"
                )
            }
            Self::UnsupportedExportScope(scope) => write!(
                formatter,
                "engine does not yet render export scope {scope:?}"
            ),
            Self::ExportPinMismatch => {
                write!(
                    formatter,
                    "export pin does not match its immutable source receipt"
                )
            }
            Self::CohortDoesNotCover(span) => write!(
                formatter,
                "cohort does not cover {}..{}",
                span.start, span.end
            ),
            Self::RenderTooLarge => write!(formatter, "render is too large for this platform"),
            Self::AuditionSampleCount { expected, actual } => write!(
                formatter,
                "timeline audition has {actual} samples, expected {expected}"
            ),
            Self::AuditionNonFiniteSample { index } => {
                write!(
                    formatter,
                    "timeline audition has non-finite sample at {index}"
                )
            }
            Self::AuditionFormatChanged { .. } => {
                write!(
                    formatter,
                    "timeline audition format differs from project playback"
                )
            }
            Self::AuditionOutsideTimeline { .. } => {
                write!(formatter, "timeline audition lies outside project playback")
            }
            Self::AuditionMailboxBusy => write!(formatter, "timeline audition mailbox is busy"),
            Self::AuditionPinMismatch => {
                write!(
                    formatter,
                    "timeline audition pin does not match its retained cohort"
                )
            }
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
            Self::Graph(error) => error.fmt(formatter),
            Self::Tile(error) => error.fmt(formatter),
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
            Self::Graph(error) => Some(error),
            Self::Tile(error) => Some(error),
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

impl From<crate::compiled_audio_graph::GraphExecutionError> for RenderRuntimeError {
    fn from(error: crate::compiled_audio_graph::GraphExecutionError) -> Self {
        Self::Graph(error)
    }
}

impl From<RenderTileError> for RenderRuntimeError {
    fn from(error: RenderTileError) -> Self {
        Self::Tile(error)
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
    use std::collections::BTreeSet;

    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        AssetRegistry, ContentFingerprint, DecodedAudioMetadata, SampleFrames,
    };
    use crate::daw_engine::DawEngineConfig;
    use crate::daw_render::PcmAsset;
    use crate::live_project::{LiveProject, SourceMaterialMetadata};
    use crate::render_plan::{EngineRecipeStamp, ProjectRevisionStamp};
    use crate::render_products::TileGrid;
    use crate::render_tiles::{
        TileLayout, TileRenderBatch, TileRenderCompletion, TileRenderPolicy, TileWorkPlan,
    };

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

    fn executable_source_plan() -> Arc<ExecutableRenderPlan> {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/tile-source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "tile source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 48_000,
                    channels: 1,
                    frame_count: SampleFrames(7),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"tile source fixture"),
                provenance: AssetProvenance::new(
                    1,
                    AssetOrigin::ImportedFile {
                        importer: "render tile test".into(),
                    },
                    location,
                ),
                tags: BTreeSet::new(),
                favorite: false,
            })
            .unwrap();
        let pcm = PcmAsset::new(
            AudioFormat::new(48_000, 1).unwrap(),
            Arc::from([0.25, -0.5, 0.75, -1.0, 0.125, 0.625, -0.375]),
        )
        .unwrap();
        let live = LiveProject::from_source_material(
            SourceMaterialMetadata::new("Tile null", "Source"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        let cancellation = RenderCancellation::new();
        let config = DawEngineConfig::default();
        let schedule = Arc::new(live.compile_audition(&config, &cancellation).unwrap());
        let window = schedule.render_schedule().window();
        let extent = RenderSpan::new(window.start, window.end).unwrap();
        let engine = EngineRecipeStamp::new(
            1,
            render_format_stamp(schedule.render_schedule().format()),
            config.block_frames,
            config.performance_seed,
            digest(203),
        )
        .unwrap();
        let id = RenderPlanId::new(
            71,
            digest(202),
            project_revision_stamp(schedule.project_revision()),
            extent,
            engine,
            Vec::new(),
        )
        .unwrap();
        let descriptor = Arc::new(RenderPlan::new(
            id,
            crate::render_plan::DeterminismGrade::BitExact,
            crate::render_plan::Tileability::Stateless,
        ));
        Arc::new(ExecutableRenderPlan::new(descriptor, schedule).unwrap())
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

        let mut segmented = Sha256::new();
        segmented.update(b"a");
        segmented.update(b"b");
        segmented.update(b"c");
        assert_eq!(
            segmented.finalize(),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
    }

    #[test]
    fn tiled_engine_publication_and_export_null_bitwise_with_whole_bounce() {
        let executable = executable_source_plan();
        let cancellation = RenderCancellation::new();
        let whole = executable.render_whole_bounce(&cancellation).unwrap();
        let layout = TileLayout::new(
            &executable.descriptor,
            TileRenderPolicy::new(
                TileGrid::new(4).unwrap(),
                0,
                crate::render_plan::Tileability::Stateless,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            layout.tiles().last().unwrap().core,
            RenderSpan::new(4, 7).unwrap()
        );

        let work = TileWorkPlan::cold(&layout, None);
        let mut batch = TileRenderBatch::new(1, work);
        let mut assembled = Vec::new();
        for job in batch.jobs(None, 0) {
            let product = executable
                .render_tile(&job.spec, &job.cancellation)
                .unwrap();
            assembled.extend_from_slice(product.interleaved());
            batch
                .accept(TileRenderCompletion {
                    generation: job.generation,
                    target: job.target,
                    index: job.spec.index,
                    product,
                })
                .unwrap();
        }
        assert_eq!(assembled.len(), whole.interleaved().len());
        assert!(assembled
            .iter()
            .zip(whole.interleaved())
            .all(|(tile, oracle)| tile.to_bits() == oracle.to_bits()));

        let draft = batch.finish().unwrap();
        let mut cold_runtime = RenderRuntime::new();
        cold_runtime.submit_target(Arc::clone(&executable)).unwrap();
        let (_cold_control, mut cold_renderer) =
            cold_runtime.bootstrap_tile_renderer(draft.clone()).unwrap();
        let mut cold_pcm = vec![0.0; whole.interleaved().len()];
        assert_eq!(
            cold_renderer.render_interleaved(&mut cold_pcm),
            whole.id.frames as usize
        );
        assert!(cold_pcm
            .iter()
            .zip(whole.interleaved())
            .all(|(tile, oracle)| tile.to_bits() == oracle.to_bits()));

        let mut runtime = RenderRuntime::new();
        runtime.submit_target(Arc::clone(&executable)).unwrap();
        let (control, mut renderer) = runtime
            .bootstrap_renderer(executable.id(), Arc::clone(&whole))
            .unwrap();
        let action = runtime
            .stage_tile_cohort(draft, PublicationTransport::default())
            .unwrap();
        control.arm_action(&action).unwrap();
        let mut first_frame = [0.0; 2];
        assert_eq!(renderer.render_interleaved(&mut first_frame), 1);
        runtime.poll_publication(&control).unwrap().unwrap();
        let pin = runtime
            .pin_active_export(
                RenderScope::Master,
                executable.descriptor.extent(),
                OutputTailPolicy::Crop,
            )
            .unwrap();
        let exported = runtime.render_export_pin(&pin, &cancellation).unwrap();
        assert!(exported
            .audio
            .interleaved()
            .iter()
            .zip(whole.interleaved())
            .all(|(tile, oracle)| tile.to_bits() == oracle.to_bits()));
        let mut tampered = pin.clone();
        let ExportPinSource::PublishedProducts { products, .. } = &mut tampered.source else {
            panic!("active export must retain published products")
        };
        *products = Vec::new().into();
        assert!(matches!(
            runtime.render_export_pin(&tampered, &cancellation),
            Err(RenderRuntimeError::ExportPinMismatch)
        ));
    }

    #[test]
    fn complete_tile_cohort_uses_the_loop_current_at_staging() {
        let executable = executable_source_plan();
        let cancellation = RenderCancellation::new();
        let whole = executable.render_whole_bounce(&cancellation).unwrap();
        let layout = TileLayout::new(
            &executable.descriptor,
            TileRenderPolicy::new(
                TileGrid::new(4).unwrap(),
                0,
                crate::render_plan::Tileability::Stateless,
            )
            .unwrap(),
        )
        .unwrap();
        // Work began before a loop existed.
        let mut batch = TileRenderBatch::new(1, TileWorkPlan::cold(&layout, None));
        while let Some(job) = batch.take_next_job(None, 0) {
            let product = executable
                .render_tile(&job.spec, &job.cancellation)
                .unwrap();
            batch
                .accept(TileRenderCompletion {
                    generation: job.generation,
                    target: job.target,
                    index: job.spec.index,
                    product,
                })
                .unwrap();
        }
        let draft = batch.finish().unwrap();
        let mut runtime = RenderRuntime::new();
        runtime.submit_target(Arc::clone(&executable)).unwrap();
        let _ = runtime.bootstrap_renderer(executable.id(), whole).unwrap();
        let current_loop = RenderSpan::new(1, 6).unwrap();
        let action = runtime
            .stage_tile_cohort(
                draft,
                PublicationTransport {
                    rolling: true,
                    loop_region: Some(current_loop),
                },
            )
            .unwrap();
        let ticket = action.ticket().expect("current loop arms publication");
        assert_eq!(ticket.cohort.publication_loop, Some(current_loop));
        assert_eq!(
            ticket.gate,
            crate::render_service::PublicationGate::LoopWrap(current_loop)
        );
    }

    #[test]
    fn scoped_bus_tiles_null_bitwise_with_one_whole_scope_execution() {
        let executable = executable_source_plan();
        let bus = executable
            .schedule
            .render_schedule()
            .buses()
            .iter()
            .find(|bus| bus.id != executable.schedule.render_schedule().master())
            .unwrap()
            .id;
        let scope = RenderScope::Bus {
            bus: bus.get(),
            tap: crate::render_plan::BusTap::PostFader,
        };
        let cancellation = RenderCancellation::new();
        let oracle = executable
            .schedule
            .render_scopes(
                executable.schedule.render_schedule().window(),
                std::slice::from_ref(&scope),
                &cancellation,
            )
            .unwrap()
            .output(&scope)
            .unwrap();
        let layout = TileLayout::new_for_scope(
            &executable.descriptor,
            TileRenderPolicy::new(
                TileGrid::new(4).unwrap(),
                0,
                crate::render_plan::Tileability::Stateless,
            )
            .unwrap(),
            scope,
        )
        .unwrap();
        let mut assembled = Vec::new();
        for spec in layout.tiles() {
            let product = executable.render_tile(spec, &cancellation).unwrap();
            assembled.extend_from_slice(product.interleaved());
        }
        assert_eq!(assembled.len(), oracle.len());
        assert!(assembled
            .iter()
            .zip(oracle.iter())
            .all(|(tile, whole)| tile.to_bits() == whole.to_bits()));
    }

    #[test]
    fn master_and_bus_tile_drafts_publish_and_pin_as_one_cohort() {
        let executable = executable_source_plan();
        let cancellation = RenderCancellation::new();
        let whole = executable.render_whole_bounce(&cancellation).unwrap();
        let bus = executable
            .schedule
            .render_schedule()
            .buses()
            .iter()
            .find(|bus| bus.id != executable.schedule.render_schedule().master())
            .unwrap()
            .id;
        let bus_scope = RenderScope::Bus {
            bus: bus.get(),
            tap: crate::render_plan::BusTap::Output,
        };
        let policy = TileRenderPolicy::new(
            TileGrid::new(4).unwrap(),
            0,
            crate::render_plan::Tileability::Stateless,
        )
        .unwrap();
        let mut drafts = Vec::new();
        for scope in [RenderScope::Master, bus_scope.clone()] {
            let layout = TileLayout::new_for_scope(&executable.descriptor, policy, scope).unwrap();
            let mut batch = TileRenderBatch::new(3, TileWorkPlan::cold(&layout, None));
            while let Some(job) = batch.take_next_job(None, 0) {
                let product = executable
                    .render_tile(&job.spec, &job.cancellation)
                    .unwrap();
                batch
                    .accept(TileRenderCompletion {
                        generation: job.generation,
                        target: job.target,
                        index: job.spec.index,
                        product,
                    })
                    .unwrap();
            }
            drafts.push(batch.finish().unwrap());
        }
        let draft = drafts.remove(0).merge(drafts.remove(0)).unwrap();
        let mut runtime = RenderRuntime::new();
        runtime.submit_target(Arc::clone(&executable)).unwrap();
        let (control, mut renderer) = runtime.bootstrap_renderer(executable.id(), whole).unwrap();
        let action = runtime
            .stage_tile_cohort(draft, PublicationTransport::default())
            .unwrap();
        control.arm_action(&action).unwrap();
        let mut frame = [0.0; 2];
        renderer.render_interleaved(&mut frame);
        runtime.poll_publication(&control).unwrap().unwrap();
        let pin = runtime
            .pin_active_export(
                bus_scope.clone(),
                executable.descriptor.extent(),
                OutputTailPolicy::Crop,
            )
            .unwrap();
        let exported = runtime.render_export_pin(&pin, &cancellation).unwrap();
        assert_eq!(exported.scope, bus_scope);
        assert!(exported
            .audio
            .interleaved()
            .iter()
            .any(|sample| *sample != 0.0));
    }

    #[test]
    fn pinned_audition_survives_a_new_cohort_publication() {
        let executable = executable_source_plan();
        let plan = &executable.descriptor;
        let cancellation = RenderCancellation::new();
        let old = executable.render_whole_bounce(&cancellation).unwrap();
        let expected = old.interleaved()[..8].to_vec();
        let key = RenderProductKey::new(
            plan.id.clone(),
            RenderScope::Master,
            plan.extent(),
            ProductPartition::WholeBounce,
            whole_bounce_boundary_recipe(),
        )
        .unwrap();
        let replacement = vec![0.9; plan.extent().len() as usize * 2];
        let new = Arc::new(
            RenderProduct::new(canonical_pcm_digest(&replacement), key, replacement.into())
                .unwrap(),
        );
        let mut runtime = RenderRuntime::new();
        runtime.submit_target(Arc::clone(&executable)).unwrap();
        let (control, mut renderer) = runtime.bootstrap_renderer(executable.id(), old).unwrap();
        let span = RenderSpan::new(0, 4).unwrap();
        let pin = runtime
            .pin_active_audition(RenderScope::Master, span)
            .unwrap();

        let action = runtime
            .stage_whole_bounce(executable.id(), new, PublicationTransport::default())
            .unwrap();
        control.arm_action(&action).unwrap();
        let mut frame = [0.0; 2];
        renderer.render_interleaved(&mut frame);
        runtime.poll_publication(&control).unwrap().unwrap();

        let audition = runtime
            .render_audition_pin(
                &pin,
                AuditionOwner {
                    namespace: 5,
                    local: 6,
                },
                AuditionSubject::Construction,
                AuditionMix::Replace,
            )
            .unwrap();
        assert_eq!(audition.source_cohort().unwrap().sequence, 1);
        assert!(audition
            .interleaved()
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.to_bits() == expected.to_bits()));
    }

    #[test]
    fn fresh_bus_and_track_stem_exports_execute_typed_scopes() {
        let executable = executable_source_plan();
        let source_bus = executable
            .schedule
            .render_schedule()
            .buses()
            .iter()
            .find(|bus| bus.id != executable.schedule.render_schedule().master())
            .unwrap()
            .id;
        let track = executable.schedule.render_schedule().audio_clips()[0].track;
        let scopes = [
            RenderScope::Bus {
                bus: source_bus.get(),
                tap: crate::render_plan::BusTap::Output,
            },
            RenderScope::Track(track.get()),
        ];
        let mut runtime = RenderRuntime::new();
        runtime.submit_target(Arc::clone(&executable)).unwrap();
        let cancellation = RenderCancellation::new();
        for scope in scopes {
            let pin = runtime
                .pin_plan_export(
                    executable.id(),
                    scope.clone(),
                    executable.descriptor.extent(),
                    OutputTailPolicy::Crop,
                )
                .unwrap();
            let rendered = runtime.render_export_pin(&pin, &cancellation).unwrap();
            assert_eq!(rendered.scope, scope);
            assert_eq!(
                rendered.audio.frame_count().0,
                executable.descriptor.extent().len()
            );
            assert!(rendered
                .audio
                .interleaved()
                .iter()
                .any(|sample| *sample != 0.0));
        }
    }

    #[test]
    fn starvation_status_distinguishes_current_fault_from_lifetime_history() {
        let plan = test_plan(1);
        let base = cohort(&plan, 1, [0.1; 8]);
        let (control, mut renderer) = CohortRenderer::new(base).unwrap();
        renderer.note_starvation(3);
        assert_eq!(
            control.status(),
            CohortRendererStatus {
                starvation_events: 1,
                starved_frames: 3,
                publication_queued: false,
                receipt_waiting: false,
                currently_starving: true,
            }
        );
        renderer.seek(ProjectFrame(0));
        let recovered = control.status();
        assert!(!recovered.currently_starving);
        assert_eq!(recovered.starvation_events, 1);
        assert_eq!(recovered.starved_frames, 3);
    }

    #[test]
    fn cancelled_tile_never_produces_a_product() {
        let executable = executable_source_plan();
        let layout = TileLayout::new(
            &executable.descriptor,
            TileRenderPolicy::new(
                TileGrid::new(4).unwrap(),
                0,
                crate::render_plan::Tileability::Stateless,
            )
            .unwrap(),
        )
        .unwrap();
        let cancellation = RenderCancellation::new();
        cancellation.cancel();
        assert!(matches!(
            executable.render_tile(&layout.tiles()[0], &cancellation),
            Err(RenderRuntimeError::Graph(
                crate::compiled_audio_graph::GraphExecutionError::Cancelled
            ))
        ));
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
        let receipt = control.drain_receipt().unwrap();
        assert_eq!(receipt.cohort, new_cohort.id);
        assert_eq!(receipt.outcome, EnvelopeOutcome::Activated);
    }

    #[test]
    fn paused_transport_progresses_publication_and_audition_mailboxes() {
        let old = test_plan(1);
        let new = test_plan(2);
        let old_cohort = cohort(&old, 1, [0.1; 8]);
        let new_cohort = cohort(&new, 2, [0.2; 8]);
        let (control, renderer) = CohortRenderer::new(old_cohort).unwrap();
        let (_transport, mut source) = crate::audio::TransportSource::new(renderer);
        control
            .arm_action(&PublicationAction::Arm(PublicationTicket {
                cohort: Arc::clone(&new_cohort),
                gate: crate::render_service::PublicationGate::LoopWrap(
                    RenderSpan::new(1, 3).unwrap(),
                ),
            }))
            .unwrap();
        let audition = Arc::new(
            TimelineAudition::new(
                TimelineAuditionId {
                    owner: AuditionOwner {
                        namespace: 9,
                        local: 1,
                    },
                    revision: 2,
                    content: digest(91),
                },
                AuditionSubject::Residual,
                AuditionMix::Replace,
                new.extent(),
                new.format(),
                vec![0.9; 8].into(),
            )
            .unwrap(),
        );
        control.set_timeline_audition(audition).unwrap();

        // The outer transport is still stopped and therefore requests no
        // project PCM, but its control boundary must progress both mailboxes.
        assert_eq!(source.next(), Some(0.0));
        assert_eq!(control.drain_receipt().unwrap().cohort, new_cohort.id);
        let audition_receipt = control.drain_audition_receipt().unwrap();
        assert!(audition_receipt.applied);
        assert!(audition_receipt.active.is_some());
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

    #[test]
    fn scoped_audition_replaces_only_its_aligned_project_span() {
        let plan = test_plan(1);
        let base = cohort(&plan, 1, [0.1, 0.1, 0.2, 0.2, 0.3, 0.3, 0.4, 0.4]);
        let (control, mut renderer) = CohortRenderer::new(base).unwrap();
        let audition = Arc::new(
            TimelineAudition::new(
                TimelineAuditionId {
                    owner: AuditionOwner {
                        namespace: 7,
                        local: 5,
                    },
                    revision: 1,
                    content: digest(91),
                },
                AuditionSubject::Harmonic,
                AuditionMix::Replace,
                RenderSpan::new(1, 3).unwrap(),
                plan.format(),
                Arc::from([0.8, 0.8, 0.9, 0.9]),
            )
            .unwrap(),
        );
        control.set_timeline_audition(audition).unwrap();

        let mut output = [0.0; 8];
        renderer.render_interleaved(&mut output);
        assert_eq!(output, [0.1, 0.1, 0.8, 0.8, 0.9, 0.9, 0.4, 0.4]);
        let receipt = control.drain_audition_receipt().unwrap();
        assert!(receipt.applied);
        assert_eq!(receipt.active.unwrap().owner.local, 5);
    }

    #[test]
    fn stale_pane_cannot_clear_a_newer_panes_audition() {
        let plan = test_plan(1);
        let base = cohort(&plan, 1, [0.1; 8]);
        let (control, mut renderer) = CohortRenderer::new(base).unwrap();
        let old_owner = AuditionOwner {
            namespace: 7,
            local: 1,
        };
        let new_owner = AuditionOwner {
            namespace: 7,
            local: 2,
        };
        let old_id = TimelineAuditionId {
            owner: old_owner,
            revision: 1,
            content: digest(1),
        };
        let audition = |owner, sample, revision| {
            Arc::new(
                TimelineAudition::new(
                    TimelineAuditionId {
                        owner,
                        revision,
                        content: digest(revision as u8),
                    },
                    AuditionSubject::Residual,
                    AuditionMix::Replace,
                    plan.extent(),
                    plan.format(),
                    vec![sample; 8].into(),
                )
                .unwrap(),
            )
        };
        control
            .set_timeline_audition(audition(old_owner, 0.5, 1))
            .unwrap();
        let mut frame = [0.0; 2];
        renderer.render_interleaved(&mut frame);
        control.drain_audition_receipt().unwrap();

        // B is newer but has not reached the realtime side yet. A's stale
        // exact clear must not replace/drop B in the incoming mailbox.
        control
            .set_timeline_audition(audition(new_owner, 0.9, 2))
            .unwrap();
        control.clear_timeline_audition(old_id).unwrap();
        renderer.render_interleaved(&mut frame);
        assert_eq!(frame, [0.9; 2]);
        let receipt = control.drain_audition_receipt().unwrap();
        assert!(receipt.applied);
        assert_eq!(receipt.active.unwrap().owner, new_owner);
    }
}
