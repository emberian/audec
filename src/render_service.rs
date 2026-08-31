//! Pure control-side state for render planning and persistent-host publication.
//!
//! This is not a worker pool and does not touch an audio device. It owns the
//! semantic handoff between them: newest target plan, coherent staged cohort,
//! publication gate, audio-thread acknowledgement, visible readiness/failure,
//! and export pins. A host adapter turns [`PublicationAction`] into a lock-free
//! table mailbox and acknowledges only after the realtime renderer has swapped.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::render_plan::{
    DeterminismGrade, OutputTailPolicy, RenderPlan, RenderPlanId, RenderScope, RenderSpan,
};
use crate::render_products::{PlaybackCohort, PlaybackCohortId, RenderProductId};

/// Transport facts needed to choose a coherent publication boundary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PublicationTransport {
    pub rolling: bool,
    pub loop_region: Option<RenderSpan>,
}

/// A boundary reported by the persistent playback adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationBoundary {
    /// One renderer/DSP quantum ended outside a loop.
    RenderQuantum {
        next_frame: i64,
    },
    /// The output timeline wrapped from this loop's end to its start.
    LoopWrap {
        region: RenderSpan,
    },
    /// Seek/stop replaced continuity, so no old/new pass can be hybridized.
    Discontinuity {
        next_frame: i64,
    },
    Stopped,
}

/// The default protects one loop pass from containing two project revisions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PublicationPolicy {
    #[default]
    TransportCoherent,
    /// Useful for an offline harness, never the normal rolling transport.
    Immediate,
}

/// An immutable condition evaluated by the persistent renderer without asking
/// the control-side service for a decision in the realtime callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationGate {
    Immediate,
    NextRenderQuantum,
    LoopWrap(RenderSpan),
}

/// One pre-armed table swap. Discontinuities and stop satisfy every gate: no
/// continuous old/new pass exists across either boundary.
#[derive(Clone, Debug)]
pub struct PublicationTicket {
    pub cohort: Arc<PlaybackCohort>,
    pub gate: PublicationGate,
}

impl PublicationTicket {
    pub fn accepts(&self, boundary: PublicationBoundary) -> bool {
        match (self.gate, boundary) {
            (_, PublicationBoundary::Stopped | PublicationBoundary::Discontinuity { .. }) => true,
            (PublicationGate::Immediate, _) => true,
            (PublicationGate::NextRenderQuantum, PublicationBoundary::RenderQuantum { .. }) => true,
            (PublicationGate::LoopWrap(expected), PublicationBoundary::LoopWrap { region }) => {
                region == expected
            }
            _ => false,
        }
    }
}

/// Work handed to the persistent-host adapter. Publication is two-phase: the
/// service does not call a cohort active until the audio side acknowledges it.
#[derive(Clone, Debug)]
pub enum PublicationAction {
    None,
    Arm(PublicationTicket),
}

impl PublicationAction {
    pub fn cohort(&self) -> Option<&Arc<PlaybackCohort>> {
        match self {
            Self::None => None,
            Self::Arm(ticket) => Some(&ticket.cohort),
        }
    }

    pub fn ticket(&self) -> Option<&PublicationTicket> {
        match self {
            Self::None => None,
            Self::Arm(ticket) => Some(ticket),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderFailureStage {
    PlanCompilation,
    ProductRender,
    ProductValidation,
    Publication,
    Export,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderFailure {
    pub plan: RenderPlanId,
    pub stage: RenderFailureStage,
    pub message: String,
}

impl RenderFailure {
    pub fn new(plan: RenderPlanId, stage: RenderFailureStage, message: impl Into<String>) -> Self {
        Self {
            plan,
            stage,
            message: message.into(),
        }
    }
}

/// User-facing truth about the relationship between desired and audible PCM.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderAvailability {
    Empty,
    Priming {
        target: RenderPlanId,
    },
    Ready {
        active: PlaybackCohortId,
    },
    /// A coherent old revision remains audible while a new one is prepared.
    Stale {
        active: PlaybackCohortId,
        target: RenderPlanId,
        candidate_ready: bool,
        publication_in_flight: bool,
    },
    /// Same plan, new coverage/table waiting for a safe publication boundary.
    Updating {
        active: PlaybackCohortId,
        candidate_ready: bool,
        publication_in_flight: bool,
    },
    /// Failure never erases the last coherent active cohort.
    Failed {
        active: Option<PlaybackCohortId>,
        target: RenderPlanId,
        failure: RenderFailure,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderServiceStatus {
    pub availability: RenderAvailability,
    pub target: Option<RenderPlanId>,
    pub active: Option<PlaybackCohortId>,
    pub staged: Option<PlaybackCohortId>,
    pub publication_in_flight: Option<PlaybackCohortId>,
}

/// Pure foundation beneath later workers, queues, LRU storage, and GPUI glue.
#[derive(Clone, Debug)]
pub struct RenderService {
    policy: PublicationPolicy,
    transport: PublicationTransport,
    plans: BTreeMap<RenderPlanId, Arc<RenderPlan>>,
    target: Option<RenderPlanId>,
    active: Option<Arc<PlaybackCohort>>,
    staged: Option<Arc<PlaybackCohort>>,
    publication_in_flight: Option<Arc<PlaybackCohort>>,
    target_failure: Option<RenderFailure>,
}

impl Default for RenderService {
    fn default() -> Self {
        Self::new(PublicationPolicy::TransportCoherent)
    }
}

impl RenderService {
    pub fn new(policy: PublicationPolicy) -> Self {
        Self {
            policy,
            transport: PublicationTransport::default(),
            plans: BTreeMap::new(),
            target: None,
            active: None,
            staged: None,
            publication_in_flight: None,
            target_failure: None,
        }
    }

    /// Register an immutable compiled plan and make it the newest desired
    /// project revision. A same-ID descriptor mismatch is rejected rather than
    /// trusting a compact identity collision or adapter bug.
    pub fn submit_target(&mut self, plan: Arc<RenderPlan>) -> Result<(), RenderServiceError> {
        if let Some(existing) = self.plans.get(&plan.id) {
            if **existing != *plan {
                return Err(RenderServiceError::PlanIdentityCollision(plan.id.clone()));
            }
        } else {
            self.plans.insert(plan.id.clone(), Arc::clone(&plan));
        }
        self.target = Some(plan.id.clone());
        self.target_failure = None;
        if self
            .staged
            .as_ref()
            .is_some_and(|cohort| cohort.id.plan != plan.id)
        {
            self.staged = None;
        }
        Ok(())
    }

    /// Stage one complete manifest. Incomplete cohorts remain scheduler state;
    /// they must never be armed for the realtime renderer.
    pub fn stage_cohort(
        &mut self,
        cohort: Arc<PlaybackCohort>,
    ) -> Result<PublicationAction, RenderServiceError> {
        let target = self
            .target
            .as_ref()
            .ok_or(RenderServiceError::NoTargetPlan)?;
        if &cohort.id.plan != target {
            return Err(RenderServiceError::ObsoleteCohort {
                expected: target.clone(),
                actual: cohort.id.plan.clone(),
            });
        }
        if !cohort.is_ready() {
            return Err(RenderServiceError::IncompleteCohort(cohort.id.clone()));
        }
        self.staged = Some(cohort);
        self.target_failure = None;
        if self.publication_in_flight.is_none() {
            return Ok(self.arm_staged());
        }
        Ok(PublicationAction::None)
    }

    /// Update loop/rolling facts. Stopping is a discontinuity and may publish
    /// immediately; changing a loop while rolling waits for a matching future
    /// boundary/cohort instead of guessing that old coverage is sufficient.
    pub fn update_transport(&mut self, transport: PublicationTransport) -> PublicationAction {
        self.transport = transport;
        if self.publication_in_flight.is_none() && self.staged.is_some() {
            self.arm_staged()
        } else {
            PublicationAction::None
        }
    }

    /// Arm a staged cohort after a previous publication is acknowledged. The
    /// scheduler may stage the next target while one ticket is already in the
    /// host mailbox; only one ticket is in flight at a time.
    pub fn arm_staged(&mut self) -> PublicationAction {
        if self.publication_in_flight.is_some() {
            return PublicationAction::None;
        }
        let Some(cohort) = self.staged.take() else {
            return PublicationAction::None;
        };
        let gate = if self.policy == PublicationPolicy::Immediate || !self.transport.rolling {
            PublicationGate::Immediate
        } else if self.active.is_none() {
            // No coherent older revision exists, so cold playback may begin at
            // the next renderer quantum rather than waiting through a loop.
            PublicationGate::NextRenderQuantum
        } else if let Some(loop_region) = self.transport.loop_region {
            // A cohort built for different loop coverage must not be armed
            // under the current loop. Leave it staged for the scheduler to
            // replace or for a later matching transport update.
            if cohort.publication_loop != Some(loop_region) {
                self.staged = Some(cohort);
                return PublicationAction::None;
            }
            PublicationGate::LoopWrap(loop_region)
        } else {
            PublicationGate::NextRenderQuantum
        };
        self.publication_in_flight = Some(Arc::clone(&cohort));
        PublicationAction::Arm(PublicationTicket { cohort, gate })
    }

    /// Mark a cohort active only after the persistent renderer confirms its
    /// table swap. Retired product Arcs can then be reclaimed off the RT thread.
    pub fn acknowledge_publication(
        &mut self,
        cohort: &PlaybackCohortId,
    ) -> Result<Option<Arc<PlaybackCohort>>, RenderServiceError> {
        let inflight = self
            .publication_in_flight
            .take()
            .ok_or(RenderServiceError::NoPublicationInFlight)?;
        if &inflight.id != cohort {
            let actual = inflight.id.clone();
            self.publication_in_flight = Some(inflight);
            return Err(RenderServiceError::PublicationAcknowledgementMismatch {
                expected: actual,
                actual: cohort.clone(),
            });
        }
        let retired = self.active.replace(inflight);
        if self.target.as_ref() == Some(&cohort.plan) {
            self.target_failure = None;
        }
        Ok(retired)
    }

    pub fn reject_publication(
        &mut self,
        cohort: &PlaybackCohortId,
        message: impl Into<String>,
    ) -> Result<(), RenderServiceError> {
        let inflight = self
            .publication_in_flight
            .take()
            .ok_or(RenderServiceError::NoPublicationInFlight)?;
        if &inflight.id != cohort {
            let actual = inflight.id.clone();
            self.publication_in_flight = Some(inflight);
            return Err(RenderServiceError::PublicationAcknowledgementMismatch {
                expected: actual,
                actual: cohort.clone(),
            });
        }
        if self.target.as_ref() == Some(&cohort.plan) {
            self.target_failure = Some(RenderFailure::new(
                cohort.plan.clone(),
                RenderFailureStage::Publication,
                message,
            ));
        }
        Ok(())
    }

    /// Record a compile/render failure for the newest target. Obsolete worker
    /// failures are ignored explicitly and cannot cover a newer successful job.
    pub fn record_failure(&mut self, failure: RenderFailure) -> bool {
        if self.target.as_ref() != Some(&failure.plan) {
            return false;
        }
        self.staged = None;
        self.target_failure = Some(failure);
        true
    }

    pub fn active_cohort(&self) -> Option<Arc<PlaybackCohort>> {
        self.active.clone()
    }

    pub fn target_plan(&self) -> Option<Arc<RenderPlan>> {
        self.target
            .as_ref()
            .and_then(|identity| self.plans.get(identity))
            .cloned()
    }

    pub fn status(&self) -> RenderServiceStatus {
        let active_id = self.active.as_ref().map(|cohort| cohort.id.clone());
        let target = self.target.clone();
        let availability = match (&self.target, &self.active, &self.target_failure) {
            (None, None, _) => RenderAvailability::Empty,
            (Some(target), active, Some(failure)) => RenderAvailability::Failed {
                active: active.as_ref().map(|cohort| cohort.id.clone()),
                target: target.clone(),
                failure: failure.clone(),
            },
            (Some(target), None, None) => RenderAvailability::Priming {
                target: target.clone(),
            },
            (Some(target), Some(active), None) if active.id.plan != *target => {
                RenderAvailability::Stale {
                    active: active.id.clone(),
                    target: target.clone(),
                    candidate_ready: self.staged.is_some(),
                    publication_in_flight: self.publication_in_flight.is_some(),
                }
            }
            (Some(_), Some(active), None)
                if self.staged.is_some() || self.publication_in_flight.is_some() =>
            {
                RenderAvailability::Updating {
                    active: active.id.clone(),
                    candidate_ready: self.staged.is_some(),
                    publication_in_flight: self.publication_in_flight.is_some(),
                }
            }
            (Some(_), Some(active), None) => RenderAvailability::Ready {
                active: active.id.clone(),
            },
            (None, Some(active), _) => RenderAvailability::Ready {
                active: active.id.clone(),
            },
        };
        RenderServiceStatus {
            availability,
            target,
            active: active_id,
            staged: self.staged.as_ref().map(|cohort| cohort.id.clone()),
            publication_in_flight: self
                .publication_in_flight
                .as_ref()
                .map(|cohort| cohort.id.clone()),
        }
    }

    /// Pin an immutable plan for a fresh export execution. The later executor
    /// resolves the engine schedule by exact plan identity.
    pub fn pin_plan_export(
        &self,
        plan: &RenderPlanId,
        scope: RenderScope,
        span: RenderSpan,
        tail: OutputTailPolicy,
    ) -> Result<ExportPin, RenderServiceError> {
        let plan = self
            .plans
            .get(plan)
            .cloned()
            .ok_or_else(|| RenderServiceError::UnknownPlan(plan.clone()))?;
        if !plan.extent().contains_span(span) {
            return Err(RenderServiceError::ExportOutsidePlan {
                requested: span,
                plan: plan.extent(),
            });
        }
        tail.validate()
            .map_err(|_| RenderServiceError::InvalidExportTail)?;
        Ok(ExportPin {
            plan,
            scope,
            span,
            tail,
            source: ExportPinSource::FreshPlanRender,
        })
    }

    /// Pin already-published PCM. Holding the cohort Arc keeps every referenced
    /// product resident even if playback publishes a newer table during export.
    pub fn pin_active_products_export(
        &self,
        scope: RenderScope,
        span: RenderSpan,
        tail: OutputTailPolicy,
    ) -> Result<ExportPin, RenderServiceError> {
        tail.validate()
            .map_err(|_| RenderServiceError::InvalidExportTail)?;
        let cohort = self
            .active
            .as_ref()
            .cloned()
            .ok_or(RenderServiceError::NoActiveCohort)?;
        let products = cohort
            .product_ids_covering(&scope, span)
            .ok_or(RenderServiceError::ActiveCohortDoesNotCover(span))?;
        let plan = self
            .plans
            .get(&cohort.id.plan)
            .cloned()
            .ok_or_else(|| RenderServiceError::UnknownPlan(cohort.id.plan.clone()))?;
        Ok(ExportPin {
            plan,
            scope,
            span,
            tail,
            source: ExportPinSource::PublishedProducts {
                cohort,
                products: products.into(),
            },
        })
    }
}

/// Exact source pinned for one export. Encoding settings live in `export.rs`;
/// this pin identifies only the immutable render-stage PCM and tail request.
#[derive(Clone, Debug)]
pub struct ExportPin {
    pub plan: Arc<RenderPlan>,
    pub scope: RenderScope,
    pub span: RenderSpan,
    pub tail: OutputTailPolicy,
    pub source: ExportPinSource,
}

impl ExportPin {
    pub fn determinism(&self) -> DeterminismGrade {
        self.plan.determinism
    }
}

#[derive(Clone, Debug)]
pub enum ExportPinSource {
    FreshPlanRender,
    PublishedProducts {
        cohort: Arc<PlaybackCohort>,
        products: Arc<[RenderProductId]>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderServiceError {
    PlanIdentityCollision(RenderPlanId),
    NoTargetPlan,
    UnknownPlan(RenderPlanId),
    ObsoleteCohort {
        expected: RenderPlanId,
        actual: RenderPlanId,
    },
    IncompleteCohort(PlaybackCohortId),
    NoPublicationInFlight,
    PublicationAcknowledgementMismatch {
        expected: PlaybackCohortId,
        actual: PlaybackCohortId,
    },
    NoActiveCohort,
    ExportOutsidePlan {
        requested: RenderSpan,
        plan: RenderSpan,
    },
    ActiveCohortDoesNotCover(RenderSpan),
    InvalidExportTail,
}

impl fmt::Display for RenderServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlanIdentityCollision(_) => {
                write!(formatter, "render plan identity maps to different metadata")
            }
            Self::NoTargetPlan => write!(formatter, "no target render plan is registered"),
            Self::UnknownPlan(_) => write!(formatter, "render plan is not registered"),
            Self::ObsoleteCohort { .. } => {
                write!(formatter, "playback cohort targets an obsolete render plan")
            }
            Self::IncompleteCohort(_) => {
                write!(formatter, "incomplete playback cohort cannot be published")
            }
            Self::NoPublicationInFlight => {
                write!(formatter, "no publication awaits acknowledgement")
            }
            Self::PublicationAcknowledgementMismatch { .. } => {
                write!(
                    formatter,
                    "publication acknowledgement names the wrong cohort"
                )
            }
            Self::NoActiveCohort => write!(formatter, "there is no published playback cohort"),
            Self::ExportOutsidePlan { requested, plan } => write!(
                formatter,
                "export {}..{} lies outside plan {}..{}",
                requested.start, requested.end, plan.start, plan.end
            ),
            Self::ActiveCohortDoesNotCover(span) => write!(
                formatter,
                "published playback products do not cover {}..{}",
                span.start, span.end
            ),
            Self::InvalidExportTail => write!(formatter, "export tail policy is invalid"),
        }
    }
}

impl Error for RenderServiceError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_plan::{
        EngineRecipeStamp, ExactDigest, ProjectRevisionStamp, RenderFormat, Tileability,
    };
    use crate::render_products::{
        CohortProduct, CohortProductProvenance, ProductPartition, RenderProduct, RenderProductKey,
        RenderSlot,
    };

    fn digest(byte: u8) -> ExactDigest {
        ExactDigest::new([byte; 32])
    }

    fn plan(revision: u64) -> Arc<RenderPlan> {
        let format = RenderFormat::new(48_000, 2).unwrap();
        let engine = EngineRecipeStamp::new(1, format, 512, 0, digest(3)).unwrap();
        let id = RenderPlanId::new(
            9,
            digest(revision as u8),
            ProjectRevisionStamp {
                aggregate: revision,
                ..ProjectRevisionStamp::default()
            },
            RenderSpan::new(0, 64).unwrap(),
            engine,
            Vec::new(),
        )
        .unwrap();
        Arc::new(RenderPlan::new(
            id,
            DeterminismGrade::BitExact,
            Tileability::Stateless,
        ))
    }

    fn cohort(
        plan: &RenderPlan,
        sequence: u64,
        loop_region: Option<RenderSpan>,
    ) -> Arc<PlaybackCohort> {
        let span = RenderSpan::new(0, 64).unwrap();
        let slot = RenderSlot {
            scope: RenderScope::Master,
            span,
        };
        let key = RenderProductKey::new(
            plan.id.clone(),
            RenderScope::Master,
            span,
            ProductPartition::WholeBounce,
            digest(4),
        )
        .unwrap();
        let product = Arc::new(
            RenderProduct::new(digest(sequence as u8), key, vec![0.0; 128].into()).unwrap(),
        );
        Arc::new(
            PlaybackCohort::new(
                PlaybackCohortId {
                    plan: plan.id.clone(),
                    sequence,
                },
                loop_region,
                vec![slot.clone()],
                vec![CohortProduct {
                    slot,
                    product,
                    provenance: CohortProductProvenance::RenderedForTarget,
                }],
            )
            .unwrap(),
        )
    }

    fn publish_initial(service: &mut RenderService, plan: &Arc<RenderPlan>) {
        service.submit_target(Arc::clone(plan)).unwrap();
        let action = service.stage_cohort(cohort(plan, 1, None)).unwrap();
        let id = action.cohort().unwrap().id.clone();
        service.acknowledge_publication(&id).unwrap();
    }

    #[test]
    fn loop_revision_waits_for_matching_wrap_and_requires_acknowledgement() {
        let old = plan(1);
        let new = plan(2);
        let loop_region = RenderSpan::new(16, 48).unwrap();
        let mut service = RenderService::default();
        publish_initial(&mut service, &old);
        service.update_transport(PublicationTransport {
            rolling: true,
            loop_region: Some(loop_region),
        });
        service.submit_target(Arc::clone(&new)).unwrap();
        let action = service
            .stage_cohort(cohort(&new, 2, Some(loop_region)))
            .unwrap();
        let ticket = action.ticket().expect("armed loop publication");
        assert_eq!(ticket.gate, PublicationGate::LoopWrap(loop_region));
        assert!(!ticket.accepts(PublicationBoundary::RenderQuantum { next_frame: 24 }));
        assert!(ticket.accepts(PublicationBoundary::LoopWrap {
            region: loop_region,
        }));
        let next = action.cohort().unwrap().id.clone();
        assert!(matches!(
            service.status().availability,
            RenderAvailability::Stale {
                publication_in_flight: true,
                ..
            }
        ));
        service.acknowledge_publication(&next).unwrap();
        assert!(matches!(
            service.status().availability,
            RenderAvailability::Ready { .. }
        ));
    }

    #[test]
    fn target_failure_keeps_the_old_cohort_audible_and_visible() {
        let old = plan(1);
        let new = plan(2);
        let mut service = RenderService::default();
        publish_initial(&mut service, &old);
        service.submit_target(Arc::clone(&new)).unwrap();
        assert!(service.record_failure(RenderFailure::new(
            new.id.clone(),
            RenderFailureStage::ProductRender,
            "fixture failure",
        )));
        assert_eq!(service.active_cohort().unwrap().id.plan, old.id);
        assert!(matches!(
            service.status().availability,
            RenderAvailability::Failed {
                active: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn stopped_transport_can_replace_an_existing_cohort_immediately() {
        let old = plan(1);
        let new = plan(2);
        let mut service = RenderService::default();
        publish_initial(&mut service, &old);
        service.submit_target(Arc::clone(&new)).unwrap();
        let action = service.stage_cohort(cohort(&new, 2, None)).unwrap();
        let next = action.cohort().expect("stopped publication").id.clone();
        service.acknowledge_publication(&next).unwrap();
        assert_eq!(service.active_cohort().unwrap().id.plan, new.id);
    }

    #[test]
    fn active_export_pin_keeps_published_products_and_exact_plan() {
        let plan = plan(1);
        let mut service = RenderService::default();
        publish_initial(&mut service, &plan);
        let pin = service
            .pin_active_products_export(
                RenderScope::Master,
                RenderSpan::new(8, 32).unwrap(),
                OutputTailPolicy::Crop,
            )
            .unwrap();
        assert_eq!(pin.plan.id, plan.id);
        match pin.source {
            ExportPinSource::PublishedProducts { cohort, products } => {
                assert_eq!(cohort.id.sequence, 1);
                assert_eq!(products.len(), 1);
            }
            ExportPinSource::FreshPlanRender => panic!("expected product pin"),
        }
    }
}
