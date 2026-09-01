//! Bounded execution for immutable analysis products.
//!
//! Panes submit exact recipes and await one-shot completions; they do not own
//! worker threads or start unbounded GPUI background computations. Identical
//! recipes share one physical flight, while every pane retains an independent
//! generation-gated completion receipt. Cancellation is cooperative and only
//! stops shared work after its final eligible subscriber is gone.
//!
//! This runtime does not publish into a project, create an audio transport, or
//! infer semantic names for signal components. Its products are immutable
//! evidence which a control-thread adapter may explicitly retain or promote.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use crate::analysis::{factor_analysis_components_cancellable, Analysis, WaveformBin};
use crate::content_identity::{ContentClass, SchemaHasher, SchemaTag};
use crate::decomposition::{ComponentDecomposition, DecompositionCancellation};
use crate::hpss::{
    separate_harmonic_percussive_cancellable, HpssCancellation, HpssResult, HpssSettings,
};
use crate::loom::{
    EventObservation, FitMetrics, LoomCancellation, SequenceSketch, TemplateBuildConfig,
};
use crate::rhythm::{
    analyze_mono_cancellable, RhythmCancellation, RhythmConfig, RhythmDeprojection,
};
use crate::task_coordinator::{
    AdmissionError, CancellationReason, CanonicalRecipeKey, CompletionOutcome, CompletionReceipt,
    CompletionReport, CoordinatorConfig, DiagnosticSeverity, FlightId, OwnerScope, PaneId,
    PaneScope, ResourceClass, SessionGeneration, SessionId, TaskCoordinator, TaskDiagnostic,
    TaskId, TaskInstant, TaskOwner, TaskPriority, TaskScope, TaskSpec,
};

const COMPONENT_RECIPE_DOMAIN: &str = "audec.analysis.components.v1";
const HPSS_RECIPE_DOMAIN: &str = "audec.analysis.hpss.v1";
const RHYTHM_RECIPE_DOMAIN: &str = "audec.analysis.rhythm.v1";
const LOOM_RECIPE_DOMAIN: &str = "audec.analysis.loom.v1";
const COMPONENT_OWNER_NAMESPACE: u128 = 0x6175_6465_633a_636f_6d70_6f6e_656e_7473;
const DEFAULT_WORKERS: usize = 2;
const DISPLAY_WAVEFORM_BINS: usize = 3_000;

/// Logical owner and supersession generation for one analysis request.
///
/// `project_session` is the live document identity. `namespace` and `local`
/// distinguish independent pane/tool lifecycles inside it. The runtime derives
/// a private coordinator session from all three so refreshing HPSS in one pane
/// cannot accidentally supersede component factorization or another pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalysisProductOwner {
    pub project_session: u64,
    pub namespace: u128,
    pub local: u64,
    pub pane: Option<u64>,
    pub generation: u64,
}

impl AnalysisProductOwner {
    pub const fn components(project_session: u64, generation: u64) -> Self {
        Self {
            project_session,
            namespace: COMPONENT_OWNER_NAMESPACE,
            local: 0,
            pane: None,
            generation,
        }
    }
}

/// Expensive HPSS output plus presentation envelopes computed on the worker.
/// Keeping these envelopes with the immutable product prevents a GPUI repaint
/// from rescanning millions of samples.
#[derive(Clone, Debug)]
pub struct HpssAnalysisProduct {
    pub original: Arc<[f32]>,
    pub separation: Arc<HpssResult>,
    pub original_waveform: Arc<[WaveformBin]>,
    pub harmonic_waveform: Arc<[WaveformBin]>,
    pub percussive_waveform: Arc<[WaveformBin]>,
    pub residual_waveform: Arc<[WaveformBin]>,
}

#[derive(Clone, Debug)]
pub struct LoomAnalysisProduct {
    pub sketch: Arc<SequenceSketch>,
    pub start_sample: usize,
    pub end_sample: usize,
    pub original: Arc<[f32]>,
    pub reconstruction: Arc<[f32]>,
    pub residual: Arc<[f32]>,
    pub original_waveform: Arc<[WaveformBin]>,
    pub reconstruction_waveform: Arc<[WaveformBin]>,
    pub residual_waveform: Arc<[WaveformBin]>,
    pub fit: FitMetrics,
}

#[derive(Clone, Debug)]
pub enum AnalysisProduct {
    Components(Arc<ComponentDecomposition>),
    Hpss(Arc<HpssAnalysisProduct>),
    Rhythm(Arc<RhythmDeprojection>),
    Loom(Arc<LoomAnalysisProduct>),
}

impl AnalysisProduct {
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Components(_) => "recurring components",
            Self::Hpss(_) => "harmonic/percussive separation",
            Self::Rhythm(_) => "rhythm deprojection",
            Self::Loom(_) => "Loom reconstruction",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AnalysisProductCompletion {
    pub receipt: CompletionReceipt,
    pub product: Arc<AnalysisProduct>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnalysisProductError {
    Admission(String),
    Coordination(String),
    Cancelled,
    Failed(String),
    Rejected(String),
    RuntimeStopped,
}

impl fmt::Display for AnalysisProductError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admission(detail) => write!(formatter, "analysis admission refused: {detail}"),
            Self::Coordination(detail) => {
                write!(formatter, "analysis coordination failed: {detail}")
            }
            Self::Cancelled => write!(formatter, "analysis was cancelled"),
            Self::Failed(detail) => write!(formatter, "analysis failed: {detail}"),
            Self::Rejected(detail) => write!(formatter, "analysis result was rejected: {detail}"),
            Self::RuntimeStopped => write!(formatter, "analysis runtime stopped"),
        }
    }
}

impl Error for AnalysisProductError {}

#[derive(Clone, Debug)]
enum AnalysisWork {
    Components {
        base: Arc<Analysis>,
        cancellation: DecompositionCancellation,
    },
    Hpss {
        original: Arc<[f32]>,
        settings: HpssSettings,
        cancellation: HpssCancellation,
    },
    Rhythm {
        mono: Arc<[f32]>,
        sample_rate: u32,
        config: RhythmConfig,
        cancellation: RhythmCancellation,
    },
    Loom {
        mono: Arc<[f32]>,
        sample_rate: u32,
        observations: Arc<[EventObservation]>,
        config: TemplateBuildConfig,
        start_sample: usize,
        end_sample: usize,
        cancellation: LoomCancellation,
    },
}

/// Exact, content-addressed work prepared away from a UI/control thread.
///
/// Recipe hashing can scan an entire recording or spectral atlas. Keeping the
/// recipe and its immutable inputs together lets a host perform that scan on
/// a bounded background executor, then make the actual scheduler admission a
/// short critical section. The fields remain private so callers cannot pair a
/// recipe with different PCM after preparation.
#[derive(Debug)]
pub struct PreparedAnalysisProduct {
    recipe: CanonicalRecipeKey,
    work: AnalysisWork,
}

impl AnalysisWork {
    fn cancel(&self) {
        match self {
            Self::Components { cancellation, .. } => cancellation.cancel(),
            Self::Hpss { cancellation, .. } => cancellation.cancel(),
            Self::Rhythm { cancellation, .. } => cancellation.cancel(),
            Self::Loom { cancellation, .. } => cancellation.cancel(),
        }
    }

    fn execute(&self) -> Result<Arc<AnalysisProduct>, AnalysisProductError> {
        match self {
            Self::Components { base, cancellation } => {
                factor_analysis_components_cancellable(base, cancellation)
                    .map(|value| Arc::new(AnalysisProduct::Components(Arc::new(value))))
                    .map_err(|error| {
                        if cancellation.is_cancelled() {
                            AnalysisProductError::Cancelled
                        } else {
                            AnalysisProductError::Failed(format!("{error:#}"))
                        }
                    })
            }
            Self::Hpss {
                original,
                settings,
                cancellation,
            } => separate_harmonic_percussive_cancellable(original, *settings, cancellation)
                .map(|separation| {
                    let separation = Arc::new(separation);
                    Arc::new(AnalysisProduct::Hpss(Arc::new(HpssAnalysisProduct {
                        original: Arc::clone(original),
                        original_waveform: Arc::from(mono_waveform_bins(
                            original,
                            DISPLAY_WAVEFORM_BINS,
                        )),
                        harmonic_waveform: Arc::from(mono_waveform_bins(
                            &separation.harmonic,
                            DISPLAY_WAVEFORM_BINS,
                        )),
                        percussive_waveform: Arc::from(mono_waveform_bins(
                            &separation.percussive,
                            DISPLAY_WAVEFORM_BINS,
                        )),
                        residual_waveform: Arc::from(mono_waveform_bins(
                            &separation.residual,
                            DISPLAY_WAVEFORM_BINS,
                        )),
                        separation,
                    })))
                })
                .map_err(|error| {
                    if cancellation.is_cancelled() {
                        AnalysisProductError::Cancelled
                    } else {
                        AnalysisProductError::Failed(error.to_string())
                    }
                }),
            Self::Rhythm {
                mono,
                sample_rate,
                config,
                cancellation,
            } => analyze_mono_cancellable(mono, *sample_rate, config, cancellation)
                .map(|result| Arc::new(AnalysisProduct::Rhythm(Arc::new(result))))
                .map_err(|error| {
                    if cancellation.is_cancelled() {
                        AnalysisProductError::Cancelled
                    } else {
                        AnalysisProductError::Failed(error.to_string())
                    }
                }),
            Self::Loom {
                mono,
                sample_rate,
                observations,
                config,
                start_sample,
                end_sample,
                cancellation,
            } => SequenceSketch::infer_cancellable(
                mono,
                *sample_rate,
                observations,
                *config,
                cancellation,
            )
            .map(|sketch| {
                let start = (*start_sample).min(mono.len());
                let end = (*end_sample).min(mono.len()).max(start);
                let original: Arc<[f32]> = Arc::from(mono[start..end].to_vec());
                let reconstruction: Arc<[f32]> =
                    Arc::from(sketch.render_span(start, end.saturating_sub(start)));
                let residual: Arc<[f32]> = Arc::from(
                    original
                        .iter()
                        .zip(reconstruction.iter())
                        .map(|(source, rendered)| source - rendered)
                        .collect::<Vec<_>>(),
                );
                let fit = sketch.fit_span(mono, start, end.saturating_sub(start));
                Arc::new(AnalysisProduct::Loom(Arc::new(LoomAnalysisProduct {
                    sketch: Arc::new(sketch),
                    start_sample: start,
                    end_sample: end,
                    original_waveform: Arc::from(mono_waveform_bins(&original, 2_400)),
                    reconstruction_waveform: Arc::from(mono_waveform_bins(&reconstruction, 2_400)),
                    residual_waveform: Arc::from(mono_waveform_bins(&residual, 2_400)),
                    original,
                    reconstruction,
                    residual,
                    fit,
                })))
            })
            .map_err(|error| {
                if cancellation.is_cancelled() {
                    AnalysisProductError::Cancelled
                } else {
                    AnalysisProductError::Failed(error.to_string())
                }
            }),
        }
    }
}

#[derive(Debug)]
struct FlightWork {
    work: AnalysisWork,
    subscribers: BTreeMap<TaskId, async_channel::Sender<AnalysisTicketResult>>,
}

type AnalysisTicketResult = Result<AnalysisProductCompletion, AnalysisProductError>;

#[derive(Debug)]
struct RuntimeState {
    coordinator: TaskCoordinator,
    flights: BTreeMap<FlightId, FlightWork>,
    stopping: bool,
}

#[derive(Debug)]
struct RuntimeInner {
    state: Mutex<RuntimeState>,
    ready: Condvar,
    clock: AtomicU64,
}

impl RuntimeInner {
    fn now(&self) -> TaskInstant {
        TaskInstant(self.clock.fetch_add(1, Ordering::Relaxed))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RuntimeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cancel_task(&self, task: TaskId) -> bool {
        let mut state = self.lock();
        let Some(flight) = state
            .coordinator
            .snapshot(task)
            .map(|snapshot| snapshot.flight)
        else {
            return false;
        };
        let changed = state
            .coordinator
            .cancel_task(task, CancellationReason::Requested)
            .unwrap_or(false);
        if state.coordinator.flight_is_cancelled(flight) == Some(true) {
            if let Some(work) = state.flights.get(&flight) {
                work.work.cancel();
            }
        }
        self.ready.notify_all();
        changed
    }

    fn cancel_revoked_flights(state: &mut RuntimeState) {
        for (flight, work) in &state.flights {
            if state.coordinator.flight_is_cancelled(*flight) == Some(true) {
                work.work.cancel();
            }
        }
    }

    fn reap_retired_queued_flights(state: &mut RuntimeState) {
        let retired = state
            .flights
            .keys()
            .copied()
            .filter(|flight| state.coordinator.flight_is_cancelled(*flight).is_none())
            .collect::<Vec<_>>();
        for flight in retired {
            let Some(work) = state.flights.remove(&flight) else {
                continue;
            };
            for sender in work.subscribers.into_values() {
                let _ = sender.try_send(Err(AnalysisProductError::Rejected(
                    "request was cancelled before worker dispatch".into(),
                )));
            }
        }
    }
}

/// Independent logical cancellation handle. Cancelling one subscriber does
/// not tear down identical work still requested by another pane.
#[derive(Clone, Debug)]
pub struct AnalysisProductCancellation {
    inner: Arc<RuntimeInner>,
    task: TaskId,
}

impl AnalysisProductCancellation {
    pub fn cancel(&self) -> bool {
        self.inner.cancel_task(self.task)
    }
}

/// One asynchronous completion stream and its independent cancellation handle.
pub struct AnalysisProductTicket {
    cancellation: AnalysisProductCancellation,
    receiver: async_channel::Receiver<AnalysisTicketResult>,
}

impl fmt::Debug for AnalysisProductTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnalysisProductTicket")
            .field("task", &self.cancellation.task)
            .finish_non_exhaustive()
    }
}

impl AnalysisProductTicket {
    pub fn cancellation(&self) -> AnalysisProductCancellation {
        self.cancellation.clone()
    }

    pub async fn receive(self) -> AnalysisTicketResult {
        self.receiver
            .recv()
            .await
            .unwrap_or(Err(AnalysisProductError::RuntimeStopped))
    }
}

/// Process-wide bounded CPU service for classical analysis products.
///
/// The runtime uses two named workers by default, matching the coordinator's
/// CPU resource limit. It is intentionally owned by the application/session
/// shell rather than by individual panes.
#[derive(Debug)]
pub struct AnalysisProductRuntime {
    inner: Arc<RuntimeInner>,
    workers: Vec<JoinHandle<()>>,
}

impl Default for AnalysisProductRuntime {
    fn default() -> Self {
        Self::new(DEFAULT_WORKERS)
            .expect("the built-in analysis worker count and coordinator limits are valid")
    }
}

impl AnalysisProductRuntime {
    pub fn new(worker_count: usize) -> Result<Self, AnalysisProductError> {
        if worker_count == 0 {
            return Err(AnalysisProductError::Admission(
                "analysis worker count must be non-zero".into(),
            ));
        }
        let mut config = CoordinatorConfig::default();
        config
            .resource_limits
            .insert(ResourceClass::Cpu, worker_count);
        let coordinator = TaskCoordinator::new(config)
            .map_err(|error| AnalysisProductError::Coordination(error.to_string()))?;
        let inner = Arc::new(RuntimeInner {
            state: Mutex::new(RuntimeState {
                coordinator,
                flights: BTreeMap::new(),
                stopping: false,
            }),
            ready: Condvar::new(),
            clock: AtomicU64::new(1),
        });
        let mut workers = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let worker_inner = Arc::clone(&inner);
            let worker = thread::Builder::new()
                .name(format!("audec-analysis-{index}"))
                .spawn(move || worker_loop(worker_inner))
                .map_err(|error| AnalysisProductError::Coordination(error.to_string()))?;
            workers.push(worker);
        }
        Ok(Self { inner, workers })
    }

    pub fn submit_components(
        &self,
        owner: AnalysisProductOwner,
        base: Arc<Analysis>,
    ) -> Result<AnalysisProductTicket, AnalysisProductError> {
        self.submit_prepared(owner, Self::prepare_components(base)?)
    }

    pub fn prepare_components(
        base: Arc<Analysis>,
    ) -> Result<PreparedAnalysisProduct, AnalysisProductError> {
        let recipe = component_recipe_key(&base)?;
        Ok(PreparedAnalysisProduct {
            recipe,
            work: AnalysisWork::Components {
                base,
                cancellation: DecompositionCancellation::default(),
            },
        })
    }

    pub fn submit_hpss(
        &self,
        owner: AnalysisProductOwner,
        original: Arc<[f32]>,
        settings: HpssSettings,
    ) -> Result<AnalysisProductTicket, AnalysisProductError> {
        self.submit_prepared(owner, Self::prepare_hpss(original, settings)?)
    }

    pub fn prepare_hpss(
        original: Arc<[f32]>,
        settings: HpssSettings,
    ) -> Result<PreparedAnalysisProduct, AnalysisProductError> {
        let recipe = hpss_recipe_key(&original, settings)?;
        Ok(PreparedAnalysisProduct {
            recipe,
            work: AnalysisWork::Hpss {
                original,
                settings,
                cancellation: HpssCancellation::default(),
            },
        })
    }

    pub fn submit_rhythm(
        &self,
        owner: AnalysisProductOwner,
        mono: Arc<[f32]>,
        sample_rate: u32,
        config: RhythmConfig,
    ) -> Result<AnalysisProductTicket, AnalysisProductError> {
        self.submit_prepared(owner, Self::prepare_rhythm(mono, sample_rate, config)?)
    }

    pub fn prepare_rhythm(
        mono: Arc<[f32]>,
        sample_rate: u32,
        config: RhythmConfig,
    ) -> Result<PreparedAnalysisProduct, AnalysisProductError> {
        let recipe = rhythm_recipe_key(&mono, sample_rate, &config)?;
        Ok(PreparedAnalysisProduct {
            recipe,
            work: AnalysisWork::Rhythm {
                mono,
                sample_rate,
                config,
                cancellation: RhythmCancellation::default(),
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn submit_loom(
        &self,
        owner: AnalysisProductOwner,
        mono: Arc<[f32]>,
        sample_rate: u32,
        observations: Arc<[EventObservation]>,
        config: TemplateBuildConfig,
        start_sample: usize,
        end_sample: usize,
    ) -> Result<AnalysisProductTicket, AnalysisProductError> {
        self.submit_prepared(
            owner,
            Self::prepare_loom(
                mono,
                sample_rate,
                observations,
                config,
                start_sample,
                end_sample,
            )?,
        )
    }

    pub fn prepare_loom(
        mono: Arc<[f32]>,
        sample_rate: u32,
        observations: Arc<[EventObservation]>,
        config: TemplateBuildConfig,
        start_sample: usize,
        end_sample: usize,
    ) -> Result<PreparedAnalysisProduct, AnalysisProductError> {
        let recipe = loom_recipe_key(
            &mono,
            sample_rate,
            &observations,
            config,
            start_sample,
            end_sample,
        )?;
        Ok(PreparedAnalysisProduct {
            recipe,
            work: AnalysisWork::Loom {
                mono,
                sample_rate,
                observations,
                config,
                start_sample,
                end_sample,
                cancellation: LoomCancellation::default(),
            },
        })
    }

    /// Admit already-content-addressed work without rescanning its inputs.
    /// This is the control-thread half of the prepare/submit boundary.
    pub fn submit_prepared(
        &self,
        owner: AnalysisProductOwner,
        prepared: PreparedAnalysisProduct,
    ) -> Result<AnalysisProductTicket, AnalysisProductError> {
        self.submit(owner, prepared.recipe, prepared.work)
    }

    fn submit(
        &self,
        owner: AnalysisProductOwner,
        recipe: CanonicalRecipeKey,
        work: AnalysisWork,
    ) -> Result<AnalysisProductTicket, AnalysisProductError> {
        let now = self.inner.now();
        let coordinator_session = owner_session(owner);
        let generation = SessionGeneration(owner.generation);
        let owner_scope = OwnerScope {
            owner: TaskOwner(
                (owner.namespace as u64)
                    ^ ((owner.namespace >> 64) as u64).rotate_left(17)
                    ^ owner.local,
            ),
            scope: TaskScope {
                session: coordinator_session,
                pane: owner
                    .pane
                    .map_or(PaneScope::Session, |pane| PaneScope::Pane(PaneId(pane))),
            },
        };
        let (sender, receiver) = async_channel::bounded(1);
        let mut state = self.inner.lock();
        if state.stopping {
            return Err(AnalysisProductError::RuntimeStopped);
        }
        state
            .coordinator
            .observe_session(coordinator_session, generation)
            .map_err(|error| AnalysisProductError::Coordination(error.to_string()))?;
        RuntimeInner::cancel_revoked_flights(&mut state);
        let submission = state
            .coordinator
            .submit(
                TaskSpec {
                    owner: owner_scope,
                    generation,
                    recipe,
                    resource: ResourceClass::Cpu,
                    priority: TaskPriority::Foreground,
                    deadline: None,
                },
                now,
            )
            .map_err(map_admission_error)?;
        RuntimeInner::reap_retired_queued_flights(&mut state);
        if let Some(existing) = state.flights.get_mut(&submission.flight) {
            existing.subscribers.insert(submission.task, sender);
        } else {
            let mut subscribers = BTreeMap::new();
            subscribers.insert(submission.task, sender);
            state
                .flights
                .insert(submission.flight, FlightWork { work, subscribers });
        }
        drop(state);
        self.inner.ready.notify_one();
        Ok(AnalysisProductTicket {
            cancellation: AnalysisProductCancellation {
                inner: Arc::clone(&self.inner),
                task: submission.task,
            },
            receiver,
        })
    }
}

impl Drop for AnalysisProductRuntime {
    fn drop(&mut self) {
        {
            let mut state = self.inner.lock();
            state.stopping = true;
            for work in state.flights.values() {
                work.work.cancel();
            }
        }
        self.inner.ready.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker_loop(inner: Arc<RuntimeInner>) {
    loop {
        let (dispatch, work) = {
            let mut state = inner.lock();
            loop {
                if state.stopping {
                    return;
                }
                let now = inner.now();
                let dispatch = state.coordinator.dispatch_next(now);
                RuntimeInner::reap_retired_queued_flights(&mut state);
                if let Some(dispatch) = dispatch {
                    let Some(work) = state
                        .flights
                        .get(&dispatch.flight())
                        .map(|flight| flight.work.clone())
                    else {
                        continue;
                    };
                    break (dispatch, work);
                }
                state = inner
                    .ready
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };

        if dispatch.cancellation().is_cancelled() {
            work.cancel();
        }
        let result = work.execute();
        let report = match &result {
            Ok(_) => CompletionReport {
                outcome: CompletionOutcome::Succeeded { output: None },
                diagnostics: Vec::new(),
            },
            Err(AnalysisProductError::Cancelled) => CompletionReport {
                outcome: CompletionOutcome::Cancelled,
                diagnostics: Vec::new(),
            },
            Err(error) => CompletionReport {
                outcome: CompletionOutcome::Failed {
                    code: "analysis-product-failed".into(),
                    detail: error.to_string(),
                },
                diagnostics: vec![TaskDiagnostic {
                    severity: DiagnosticSeverity::Error,
                    code: "analysis-product-failed".into(),
                    detail: error.to_string(),
                }],
            },
        };
        complete_flight(&inner, dispatch.flight(), report, result);
    }
}

fn complete_flight(
    inner: &RuntimeInner,
    flight: FlightId,
    report: CompletionReport,
    result: Result<Arc<AnalysisProduct>, AnalysisProductError>,
) {
    let mut state = inner.lock();
    let batch = state.coordinator.complete(flight, report, inner.now());
    let Some(work) = state.flights.remove(&flight) else {
        inner.ready.notify_all();
        return;
    };
    let batch = match batch {
        Ok(batch) => batch,
        Err(error) => {
            for sender in work.subscribers.into_values() {
                let _ = sender.try_send(Err(AnalysisProductError::Coordination(error.to_string())));
            }
            inner.ready.notify_all();
            return;
        }
    };
    for receipt in batch.accepted {
        let Some(sender) = work.subscribers.get(&receipt.task()) else {
            continue;
        };
        let completion = result
            .clone()
            .map(|product| AnalysisProductCompletion { receipt, product });
        let _ = sender.try_send(completion);
    }
    for rejected in batch.rejected {
        let Some(sender) = work.subscribers.get(&rejected.receipt.task()) else {
            continue;
        };
        let _ = sender.try_send(Err(AnalysisProductError::Rejected(format!(
            "{:?}",
            rejected.reason
        ))));
    }
    drop(state);
    inner.ready.notify_all();
}

fn owner_session(owner: AnalysisProductOwner) -> SessionId {
    // A deterministic non-cryptographic mixing function is sufficient here:
    // this is process-local coordination identity, not durable content.
    let namespace_low = owner.namespace as u64;
    let namespace_high = (owner.namespace >> 64) as u64;
    let high = owner.project_session.wrapping_mul(0x9e37_79b1_85eb_ca87)
        ^ namespace_high.rotate_left(23)
        ^ namespace_low.rotate_right(7);
    let low = owner.local.wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
        ^ namespace_low
        ^ namespace_high.rotate_left(31);
    SessionId((u128::from(high) << 64) | u128::from(low))
}

fn component_recipe_key(base: &Analysis) -> Result<CanonicalRecipeKey, AnalysisProductError> {
    let mut hasher = recipe_hasher("analysis-components-input")?;
    update_bytes(&mut hasher, base.path.to_string_lossy().as_bytes());
    hasher.update(&base.sample_rate.to_le_bytes());
    hasher.update(&base.channels.to_le_bytes());
    hasher.update(&base.bits_per_sample.to_le_bytes());
    hasher.update(&base.spectral_peak_db.to_bits().to_le_bytes());
    hasher.update(&(base.spectral_db.len() as u64).to_le_bytes());
    for sample in &base.spectral_db {
        hasher.update(&sample.to_bits().to_le_bytes());
    }
    CanonicalRecipeKey::new(COMPONENT_RECIPE_DOMAIN, 1, hasher.finish().bytes())
        .map_err(|error| AnalysisProductError::Coordination(error.to_string()))
}

fn hpss_recipe_key(
    original: &[f32],
    settings: HpssSettings,
) -> Result<CanonicalRecipeKey, AnalysisProductError> {
    let mut hasher = recipe_hasher("analysis-hpss-input")?;
    hasher.update(&(original.len() as u64).to_le_bytes());
    for sample in original {
        hasher.update(&sample.to_bits().to_le_bytes());
    }
    hasher.update(&(settings.fft_size as u64).to_le_bytes());
    hasher.update(&(settings.hop_size as u64).to_le_bytes());
    hasher.update(&settings.soft_mask_power.to_bits().to_le_bytes());
    hasher.update(&(settings.time_median_width as u64).to_le_bytes());
    hasher.update(&(settings.frequency_median_width as u64).to_le_bytes());
    CanonicalRecipeKey::new(HPSS_RECIPE_DOMAIN, 1, hasher.finish().bytes())
        .map_err(|error| AnalysisProductError::Coordination(error.to_string()))
}

fn rhythm_recipe_key(
    mono: &[f32],
    sample_rate: u32,
    config: &RhythmConfig,
) -> Result<CanonicalRecipeKey, AnalysisProductError> {
    let mut hasher = recipe_hasher("analysis-rhythm-input")?;
    hash_pcm(&mut hasher, mono);
    hasher.update(&sample_rate.to_le_bytes());
    for value in [config.fft_size, config.hop_size, config.log_band_count] {
        hasher.update(&(value as u64).to_le_bytes());
    }
    for value in [config.minimum_frequency_hz, config.maximum_frequency_hz] {
        hasher.update(&value.to_bits().to_le_bytes());
    }
    hasher.update(&(config.spectral_max_radius as u64).to_le_bytes());
    for value in [
        config.threshold_window_seconds,
        config.threshold_mad_multiplier,
        config.threshold_floor,
        config.minimum_hit_spacing_seconds,
        config.maximum_span_seconds,
        config.tempo_min_bpm,
        config.tempo_max_bpm,
    ] {
        hasher.update(&value.to_bits().to_le_bytes());
    }
    for value in [config.tempo_hypotheses, config.phase_hypotheses_per_tempo] {
        hasher.update(&(value as u64).to_le_bytes());
    }
    hasher.update(&config.family_similarity_threshold.to_bits().to_le_bytes());
    for value in [config.maximum_families, config.maximum_patterns] {
        hasher.update(&(value as u64).to_le_bytes());
    }
    CanonicalRecipeKey::new(RHYTHM_RECIPE_DOMAIN, 1, hasher.finish().bytes())
        .map_err(|error| AnalysisProductError::Coordination(error.to_string()))
}

fn loom_recipe_key(
    mono: &[f32],
    sample_rate: u32,
    observations: &[EventObservation],
    config: TemplateBuildConfig,
    start_sample: usize,
    end_sample: usize,
) -> Result<CanonicalRecipeKey, AnalysisProductError> {
    let mut hasher = recipe_hasher("analysis-loom-input")?;
    hash_pcm(&mut hasher, mono);
    hasher.update(&sample_rate.to_le_bytes());
    hasher.update(&(observations.len() as u64).to_le_bytes());
    for observation in observations {
        hasher.update(&(observation.sample_index as u64).to_le_bytes());
        hasher.update(&(observation.cluster_id as u64).to_le_bytes());
        hasher.update(&observation.salience.to_bits().to_le_bytes());
        hasher.update(&observation.template_similarity.to_bits().to_le_bytes());
    }
    for value in [
        config.pre_roll_samples,
        config.post_roll_samples,
        config.alignment_radius_samples,
        config.max_exemplars_per_cluster,
        start_sample,
        end_sample,
    ] {
        hasher.update(&(value as u64).to_le_bytes());
    }
    CanonicalRecipeKey::new(LOOM_RECIPE_DOMAIN, 1, hasher.finish().bytes())
        .map_err(|error| AnalysisProductError::Coordination(error.to_string()))
}

fn recipe_hasher(name: &str) -> Result<SchemaHasher, AnalysisProductError> {
    SchemaTag::new(ContentClass::Recipe, name, 1)
        .map(SchemaHasher::new)
        .map_err(|error| AnalysisProductError::Coordination(error.to_string()))
}

fn update_bytes(hasher: &mut SchemaHasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hash_pcm(hasher: &mut SchemaHasher, samples: &[f32]) {
    hasher.update(&(samples.len() as u64).to_le_bytes());
    for sample in samples {
        hasher.update(&sample.to_bits().to_le_bytes());
    }
}

fn map_admission_error(error: AdmissionError) -> AnalysisProductError {
    AnalysisProductError::Admission(error.to_string())
}

fn mono_waveform_bins(samples: &[f32], target_bins: usize) -> Vec<WaveformBin> {
    if samples.is_empty() || target_bins == 0 {
        return Vec::new();
    }
    let bin_count = target_bins.min(samples.len());
    (0..bin_count)
        .map(|bin| {
            let start = samples.len() * bin / bin_count;
            let end = samples.len() * (bin + 1) / bin_count;
            let mut minimum = f32::INFINITY;
            let mut maximum = f32::NEG_INFINITY;
            for sample in samples[start..end].iter().copied() {
                minimum = minimum.min(sample);
                maximum = maximum.max(sample);
            }
            WaveformBin {
                left_min: minimum,
                left_max: maximum,
                right_min: minimum,
                right_max: maximum,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(local: u64, generation: u64) -> AnalysisProductOwner {
        AnalysisProductOwner {
            project_session: 7,
            namespace: 11,
            local,
            pane: Some(local),
            generation,
        }
    }

    #[test]
    fn hpss_recipe_is_exact_and_parameter_sensitive() {
        let samples: Arc<[f32]> = Arc::from([0.0, 0.25, -0.5, 1.0]);
        let first = hpss_recipe_key(&samples, HpssSettings::default()).unwrap();
        let repeat = hpss_recipe_key(&samples, HpssSettings::default()).unwrap();
        assert_eq!(first, repeat);
        let mut changed = HpssSettings::default();
        changed.soft_mask_power = 3.0;
        assert_ne!(first, hpss_recipe_key(&samples, changed).unwrap());
    }

    #[test]
    fn prepared_analysis_keeps_recipe_and_pcm_in_one_opaque_unit() {
        let samples: Arc<[f32]> = Arc::from([0.0, 0.25, -0.5, 1.0]);
        let prepared =
            AnalysisProductRuntime::prepare_hpss(Arc::clone(&samples), HpssSettings::default())
                .unwrap();

        assert_eq!(
            prepared.recipe,
            hpss_recipe_key(&samples, HpssSettings::default()).unwrap()
        );
        let AnalysisWork::Hpss { original, .. } = &prepared.work else {
            panic!("prepared HPSS work changed product kind");
        };
        assert!(Arc::ptr_eq(original, &samples));
    }

    #[test]
    fn rhythm_and_loom_recipes_include_effective_parameters() {
        let samples: Arc<[f32]> = Arc::from([0.0, 0.25, -0.5, 1.0]);
        let rhythm = RhythmConfig::default();
        let first = rhythm_recipe_key(&samples, 48_000, &rhythm).unwrap();
        let mut changed_rhythm = rhythm;
        changed_rhythm.maximum_families += 1;
        assert_ne!(
            first,
            rhythm_recipe_key(&samples, 48_000, &changed_rhythm).unwrap()
        );

        let observations = [EventObservation {
            sample_index: 1,
            cluster_id: 2,
            salience: 0.8,
            template_similarity: 0.9,
        }];
        let loom = TemplateBuildConfig::for_sample_rate(48_000);
        let first = loom_recipe_key(&samples, 48_000, &observations, loom, 0, 4).unwrap();
        assert_ne!(
            first,
            loom_recipe_key(&samples, 48_000, &observations, loom, 1, 4).unwrap()
        );
    }

    #[test]
    fn identical_hpss_requests_share_one_physical_flight() {
        let runtime = AnalysisProductRuntime::new(1).unwrap();
        let samples: Arc<[f32]> = Arc::from(vec![0.0; 8_192]);
        let first = runtime
            .submit_hpss(owner(1, 1), Arc::clone(&samples), HpssSettings::default())
            .unwrap();
        let second = runtime
            .submit_hpss(owner(2, 1), samples, HpssSettings::default())
            .unwrap();
        let first = first.receiver.recv_blocking().unwrap().unwrap();
        let second = second.receiver.recv_blocking().unwrap().unwrap();
        assert_eq!(first.receipt.flight(), second.receipt.flight());
        assert!(Arc::ptr_eq(&first.product, &second.product));
    }

    #[test]
    fn cancelling_one_subscriber_preserves_shared_work() {
        let runtime = AnalysisProductRuntime::new(1).unwrap();
        let samples: Arc<[f32]> = Arc::from(vec![0.0; 32_768]);
        let first = runtime
            .submit_hpss(owner(1, 1), Arc::clone(&samples), HpssSettings::default())
            .unwrap();
        let first_cancel = first.cancellation();
        let second = runtime
            .submit_hpss(owner(2, 1), samples, HpssSettings::default())
            .unwrap();
        assert!(first_cancel.cancel());
        let first = first.receiver.recv_blocking().unwrap();
        let second = second.receiver.recv_blocking().unwrap();
        assert!(matches!(first, Err(AnalysisProductError::Rejected(_))));
        assert!(second.is_ok());
    }

    #[test]
    fn refreshing_an_owner_rejects_its_stale_completion() {
        let runtime = AnalysisProductRuntime::new(1).unwrap();
        let samples: Arc<[f32]> = Arc::from(vec![0.0; 32_768]);
        let old = runtime
            .submit_hpss(owner(1, 1), Arc::clone(&samples), HpssSettings::default())
            .unwrap();
        let new = runtime
            .submit_hpss(owner(1, 2), samples, HpssSettings::default())
            .unwrap();
        assert!(matches!(
            old.receiver.recv_blocking().unwrap(),
            Err(AnalysisProductError::Rejected(_))
        ));
        assert!(new.receiver.recv_blocking().unwrap().is_ok());
    }
}
