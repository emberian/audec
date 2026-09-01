//! Concrete adoption of semantic render dependencies by the one DAW engine.
//!
//! [`render_dependencies`](crate::render_dependencies) deliberately stops at
//! a DSP-free dependency schedule. This module is the mechanical bridge from
//! that schedule to the existing [`TileRenderBatch`] and
//! [`ExecutableRenderPlan`] path. It declares exact bus/track consumption,
//! carries tile context and boundary recipes unchanged, gates downstream work
//! on prerequisite availability, and assembles each semantic cohort only when
//! every product is present.
//!
//! This is not an audio graph, cache, worker pool, or publication service. It
//! never mixes PCM and it cannot publish a partial cohort. Every rendered
//! product still comes from [`ExecutableRenderPlan::render_tile`].

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::change_set::ChangeSet;
use crate::daw_project::ProjectDomain;
use crate::daw_render::RenderSchedule;
use crate::mixer::BusId;
use crate::render_dependencies::{
    DependencySchedule, InvalidationReport, ProductCohort, ProductCohortReadiness,
    ProductInvalidation, ProductNodeKey, ProductPurpose, RenderDependencyError,
    RenderDependencyGraph, RenderDependencyNode,
};
use crate::render_plan::{
    OutputTailPolicy, RenderDependencyKey, RenderPlan, RenderScope, RenderSpan,
};
use crate::render_products::{
    CohortProduct, CohortProductProvenance, PlaybackCohort, PlaybackCohortId, RenderProduct,
};
use crate::render_runtime::{ExecutableRenderPlan, RenderRuntimeError};
use crate::render_tiles::{
    canonical_reuse_receipt, RenderTileError, TileCohortDraft, TileDecision, TileLayout,
    TileRenderBatch, TileRenderCompletion, TileRenderJob, TileRenderPolicy, TileWorkPlan,
};

/// Frozen routing facts consumed by dependency declaration.
///
/// `upstream[target]` contains every bus whose output can reach `target`,
/// including `target` itself. Main routes and sends are treated uniformly: an
/// edit to either source can alter the observed target product.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConcreteRenderTopology {
    extent: RenderSpan,
    master: BusId,
    upstream: BTreeMap<BusId, BTreeSet<BusId>>,
    track_buses: BTreeMap<u64, BTreeSet<BusId>>,
    declared_tail_frames: u64,
}

impl ConcreteRenderTopology {
    pub fn from_schedule(schedule: &RenderSchedule) -> Result<Self, DependencyRuntimeError> {
        let window = schedule.window();
        let extent = RenderSpan::new(window.start, window.end)
            .map_err(|_| DependencyRuntimeError::InvalidScheduleExtent)?;
        let buses = schedule
            .buses()
            .iter()
            .map(|bus| bus.id)
            .collect::<BTreeSet<_>>();
        if !buses.contains(&schedule.master()) {
            return Err(DependencyRuntimeError::UnknownTopologyBus(
                schedule.master(),
            ));
        }
        let mut direct_upstream = buses
            .iter()
            .copied()
            .map(|bus| (bus, BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();
        for source in schedule.buses() {
            for route in source.routes.iter() {
                let Some(inputs) = direct_upstream.get_mut(&route.to) else {
                    return Err(DependencyRuntimeError::UnknownTopologyBus(route.to));
                };
                inputs.insert(source.id);
            }
        }
        let mut upstream = BTreeMap::new();
        for target in buses {
            let mut closure = BTreeSet::from([target]);
            let mut frontier = vec![target];
            while let Some(bus) = frontier.pop() {
                for source in &direct_upstream[&bus] {
                    if closure.insert(*source) {
                        frontier.push(*source);
                    }
                }
            }
            upstream.insert(target, closure);
        }
        let mut track_buses = BTreeMap::<u64, BTreeSet<BusId>>::new();
        for clip in schedule.audio_clips() {
            track_buses
                .entry(clip.track.get())
                .or_default()
                .insert(clip.bus);
        }
        // Tracks with no audio clips still exist as valid (silent or
        // instrument-driven) semantic scopes. Their additional source buses
        // can be declared on the request without inventing a fallback here.
        for track in schedule.tracks() {
            track_buses.entry(track.get()).or_default();
        }
        Ok(Self {
            extent,
            master: schedule.master(),
            upstream,
            track_buses,
            declared_tail_frames: schedule.tail().maximum_declared_plugin_tail_frames,
        })
    }

    pub const fn extent(&self) -> RenderSpan {
        self.extent
    }

    pub const fn master(&self) -> BusId {
        self.master
    }

    pub const fn declared_tail_frames(&self) -> u64 {
        self.declared_tail_frames
    }

    pub fn source_buses(
        &self,
        scope: &RenderScope,
    ) -> Result<BTreeSet<BusId>, DependencyRuntimeError> {
        match scope {
            RenderScope::Master => Ok(self.upstream[&self.master].clone()),
            RenderScope::Bus { bus, .. } => self
                .upstream
                .get(&BusId::from_raw(*bus))
                .cloned()
                .ok_or_else(|| DependencyRuntimeError::UnknownRenderScope(scope.clone())),
            RenderScope::Track(track) => self
                .track_buses
                .get(track)
                .cloned()
                .ok_or_else(|| DependencyRuntimeError::UnknownRenderScope(scope.clone())),
            // Explanation PCM is derived by the explanation compiler and is
            // not presently an executable DawEngineSchedule scope.
            RenderScope::Explanation(_) => Err(DependencyRuntimeError::UnsupportedExecutableScope(
                scope.clone(),
            )),
        }
    }
}

/// End-of-project extent promised by the compiler. Tile context handles state
/// across internal boundaries; this separate contract proves that the plan
/// itself has retained the requested output tail.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConcreteOutputContract {
    pub authored_body: RenderSpan,
    pub tail: OutputTailPolicy,
}

impl ConcreteOutputContract {
    pub const fn cropped(extent: RenderSpan) -> Self {
        Self {
            authored_body: extent,
            tail: OutputTailPolicy::Crop,
        }
    }

    fn validate(
        self,
        plan: &RenderPlan,
        topology: &ConcreteRenderTopology,
    ) -> Result<(), DependencyRuntimeError> {
        if topology.extent != plan.extent() {
            return Err(DependencyRuntimeError::SchedulePlanExtentMismatch {
                schedule: topology.extent,
                plan: plan.extent(),
            });
        }
        if !plan.extent().contains_span(self.authored_body) {
            return Err(DependencyRuntimeError::AuthoredBodyOutsidePlan);
        }
        let required = self
            .tail
            .maximum_output_span(self.authored_body)
            .map_err(|error| DependencyRuntimeError::TailContract(error.to_string()))?;
        if !plan.extent().contains_span(required) {
            return Err(DependencyRuntimeError::TailOutsidePlan {
                required,
                plan: plan.extent(),
            });
        }
        Ok(())
    }
}

/// One semantic stream requested from the frozen DAW schedule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConcreteProductRequest {
    pub purpose: ProductPurpose,
    pub scope: RenderScope,
    pub cohort: ProductCohort,
    pub consumed_domains: BTreeSet<ProjectDomain>,
    pub consumed_inputs: BTreeSet<RenderDependencyKey>,
    pub additional_source_buses: BTreeSet<BusId>,
    pub prerequisites: BTreeSet<ProductPurpose>,
}

impl ConcreteProductRequest {
    pub fn playback_master() -> Self {
        Self::new(
            ProductPurpose::Master,
            RenderScope::Master,
            ProductCohort::Playback,
        )
        .consumes_forward_project()
    }

    pub fn new(purpose: ProductPurpose, scope: RenderScope, cohort: ProductCohort) -> Self {
        Self {
            purpose,
            scope,
            cohort,
            consumed_domains: BTreeSet::new(),
            consumed_inputs: BTreeSet::new(),
            additional_source_buses: BTreeSet::new(),
            prerequisites: BTreeSet::new(),
        }
    }

    pub fn consumes_forward_project(mut self) -> Self {
        self.consumed_domains.extend([
            ProjectDomain::Arrangement,
            ProjectDomain::Sequencer,
            ProjectDomain::Automation,
            ProjectDomain::Assets,
            ProjectDomain::Mixer,
            ProjectDomain::SampleKits,
            ProjectDomain::Bindings,
        ]);
        self
    }

    pub fn consumes_domain(mut self, domain: ProjectDomain) -> Self {
        self.consumed_domains.insert(domain);
        self
    }

    pub fn consumes_input(mut self, input: RenderDependencyKey) -> Self {
        self.consumed_inputs.insert(input);
        self
    }

    pub fn reads_bus(mut self, bus: BusId) -> Self {
        self.additional_source_buses.insert(bus);
        self
    }

    pub fn depends_on(mut self, purpose: ProductPurpose) -> Self {
        self.prerequisites.insert(purpose);
        self
    }
}

/// Deterministic set of requested semantic streams. Purpose is the stable
/// identity: declaring the same purpose twice is refused even if its scope is
/// different, preventing an ambiguous prerequisite edge.
#[derive(Clone, Debug)]
pub struct ConcreteProductManifest {
    requests: BTreeMap<ProductPurpose, ConcreteProductRequest>,
}

impl ConcreteProductManifest {
    pub fn playback() -> Self {
        Self {
            requests: BTreeMap::from([(
                ProductPurpose::Master,
                ConcreteProductRequest::playback_master(),
            )]),
        }
    }

    pub fn new(
        requests: impl IntoIterator<Item = ConcreteProductRequest>,
    ) -> Result<Self, DependencyRuntimeError> {
        let mut by_purpose = BTreeMap::new();
        for request in requests {
            if by_purpose
                .insert(request.purpose.clone(), request)
                .is_some()
            {
                return Err(DependencyRuntimeError::DuplicatePurpose);
            }
        }
        if by_purpose.is_empty() {
            return Err(DependencyRuntimeError::EmptyManifest);
        }
        for request in by_purpose.values() {
            if let Some(missing) = request
                .prerequisites
                .iter()
                .find(|purpose| !by_purpose.contains_key(*purpose))
            {
                return Err(DependencyRuntimeError::MissingPurposePrerequisite {
                    product: request.purpose.clone(),
                    prerequisite: missing.clone(),
                });
            }
        }
        Ok(Self {
            requests: by_purpose,
        })
    }

    pub fn requests(&self) -> impl Iterator<Item = &ConcreteProductRequest> {
        self.requests.values()
    }
}

/// Declare tile nodes for every requested stream. All streams use the exact
/// same tiling policy and plan extent, so same-index prerequisite edges have
/// identical core spans and deterministic boundary semantics.
pub fn declare_concrete_render_graph(
    plan: &RenderPlan,
    topology: &ConcreteRenderTopology,
    policy: TileRenderPolicy,
    output: ConcreteOutputContract,
    manifest: &ConcreteProductManifest,
) -> Result<RenderDependencyGraph, DependencyRuntimeError> {
    output.validate(plan, topology)?;

    let mut layouts = BTreeMap::new();
    for request in manifest.requests() {
        topology.source_buses(&request.scope)?;
        layouts.insert(
            request.purpose.clone(),
            TileLayout::new_for_scope(plan, policy, request.scope.clone())?,
        );
    }

    let mut keys = BTreeMap::<(ProductPurpose, i64), ProductNodeKey>::new();
    for request in manifest.requests() {
        for spec in layouts[&request.purpose].tiles() {
            keys.insert(
                (request.purpose.clone(), spec.index),
                ProductNodeKey {
                    purpose: request.purpose.clone(),
                    scope: request.scope.clone(),
                    core: spec.core,
                    partition: crate::render_products::ProductPartition::Tile {
                        grid: spec.grid,
                        index: spec.index,
                    },
                },
            );
        }
    }

    let mut nodes = Vec::new();
    for request in manifest.requests() {
        let inferred_buses = topology.source_buses(&request.scope)?;
        for spec in layouts[&request.purpose].tiles() {
            let key = keys[&(request.purpose.clone(), spec.index)].clone();
            let mut node = RenderDependencyNode::new(
                key,
                spec.context,
                spec.boundary_recipe,
                request.cohort.clone(),
            );
            node.consumed_domains = request.consumed_domains.clone();
            node.consumed_inputs = request.consumed_inputs.clone();
            node.source_buses = inferred_buses
                .union(&request.additional_source_buses)
                .copied()
                .collect();
            for prerequisite in &request.prerequisites {
                node.prerequisites.insert(
                    keys.get(&(prerequisite.clone(), spec.index))
                        .cloned()
                        .ok_or_else(|| DependencyRuntimeError::MissingAlignedPrerequisite {
                            product: request.purpose.clone(),
                            prerequisite: prerequisite.clone(),
                            tile: spec.index,
                        })?,
                );
            }
            nodes.push(node);
        }
    }
    Ok(RenderDependencyGraph::new(plan.id.clone(), nodes)?)
}

/// Reusable result from a previously completed dependency render. Products
/// remain indexed by semantic node rather than by bare tile index, so a bus,
/// stem, and master tile at index zero cannot alias one another.
#[derive(Clone, Debug)]
pub struct DependencyRenderState {
    pub graph: Arc<RenderDependencyGraph>,
    products: BTreeMap<ProductNodeKey, Arc<RenderProduct>>,
    drafts: BTreeMap<ProductCohort, TileCohortDraft>,
}

impl DependencyRenderState {
    pub fn product(&self, node: &ProductNodeKey) -> Option<Arc<RenderProduct>> {
        self.products.get(node).cloned()
    }

    pub fn cohort_draft(&self, cohort: &ProductCohort) -> Option<&TileCohortDraft> {
        self.drafts.get(cohort)
    }

    pub fn playback_draft(&self) -> Option<&TileCohortDraft> {
        self.cohort_draft(&ProductCohort::Playback)
    }

    /// Materialize a ready semantic cohort for adapters that do not use
    /// `RenderRuntime::stage_tile_cohort`. Runtime-owned callers should pass
    /// the draft instead so its sequence allocator remains authoritative.
    pub fn playback_cohort(
        &self,
        cohort: &ProductCohort,
        sequence: u64,
    ) -> Result<Arc<PlaybackCohort>, DependencyRuntimeError> {
        let draft = self
            .drafts
            .get(cohort)
            .ok_or_else(|| DependencyRuntimeError::MissingCohort(cohort.clone()))?;
        Ok(Arc::new(PlaybackCohort::new(
            PlaybackCohortId {
                plan: draft.plan.clone(),
                sequence,
            },
            draft.publication_loop,
            draft.required.clone(),
            draft.products.clone(),
        )?))
    }
}

#[derive(Clone, Debug)]
pub struct DependencyTileRenderJob {
    pub node: ProductNodeKey,
    pub cohort: ProductCohort,
    pub tile: TileRenderJob,
}

#[derive(Clone, Debug)]
pub struct DependencyTileRenderCompletion {
    pub node: ProductNodeKey,
    pub tile: TileRenderCompletion,
}

/// Composite batch over semantic nodes. Each underlying `TileRenderBatch`
/// owns exactly one node. This intentionally reuses its generation,
/// cancellation, product validation, and draft assembly laws while avoiding
/// the bare-index collision that would occur if multiple scopes shared one
/// batch.
#[derive(Debug)]
pub struct DependencyRenderBatch {
    graph: Arc<RenderDependencyGraph>,
    schedule: DependencySchedule,
    batches: BTreeMap<ProductNodeKey, TileRenderBatch>,
    cohorts: BTreeMap<ProductNodeKey, ProductCohort>,
    available: BTreeSet<ProductNodeKey>,
    cancellation: crate::daw_render::RenderCancellation,
}

impl DependencyRenderBatch {
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        generation: u64,
        graph: Arc<RenderDependencyGraph>,
        previous: Option<&DependencyRenderState>,
        changes: &ChangeSet,
        publication_loop: Option<RenderSpan>,
        playhead: i64,
    ) -> Result<Self, DependencyRuntimeError> {
        let previous_graph = previous.map(|state| state.graph.as_ref());
        let report = graph.invalidate_from(previous_graph, changes);
        let schedule =
            DependencySchedule::build(graph.as_ref(), &report, publication_loop, playhead)?;
        let cancellation = crate::daw_render::RenderCancellation::new();
        let proof =
            previous.map(|state| canonical_reuse_receipt(&state.graph.plan, &graph.plan, changes));
        let mut batches = BTreeMap::new();
        let mut cohorts = BTreeMap::new();
        let mut available = BTreeSet::new();
        for node in graph.nodes() {
            let decision = match report
                .decision(&node.key)
                .expect("invalidation report covers graph")
            {
                ProductInvalidation::Render { .. } => {
                    let scheduled = schedule
                        .jobs
                        .iter()
                        .find(|job| job.node == node.key)
                        .expect("every dirty node is scheduled");
                    TileDecision::Render(scheduled.tile_spec(graph.plan.clone())?)
                }
                ProductInvalidation::Reuse => {
                    let state = previous.expect("reuse requires a previous graph");
                    let product = state.products.get(&node.key).cloned().ok_or_else(|| {
                        DependencyRuntimeError::MissingReusableProduct(node.key.clone())
                    })?;
                    let target_key = node
                        .key
                        .product_key(graph.plan.clone(), node.boundary_recipe)?;
                    if product.produced_by.scope != target_key.scope
                        || product.produced_by.core != target_key.core
                        || product.produced_by.partition != target_key.partition
                        || product.produced_by.boundary_recipe != target_key.boundary_recipe
                    {
                        return Err(DependencyRuntimeError::ReusableProductMismatch(
                            node.key.clone(),
                        ));
                    }
                    available.insert(node.key.clone());
                    TileDecision::Reuse(CohortProduct {
                        slot: node.key.slot(),
                        provenance: CohortProductProvenance::Reused {
                            from_plan: product.produced_by.plan.clone(),
                            proof: proof.expect("reuse requires a previous graph"),
                        },
                        product,
                    })
                }
            };
            let work = TileWorkPlan {
                target: graph.plan.clone(),
                publication_loop,
                decisions: vec![decision],
            };
            batches.insert(
                node.key.clone(),
                TileRenderBatch::with_cancellation(generation, work, cancellation.clone()),
            );
            cohorts.insert(node.key.clone(), node.cohort.clone());
        }
        Ok(Self {
            graph,
            schedule,
            batches,
            cohorts,
            available,
            cancellation,
        })
    }

    pub fn cancellation(&self) -> crate::daw_render::RenderCancellation {
        self.cancellation.clone()
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub fn readiness(&self, cohort: &ProductCohort) -> Option<ProductCohortReadiness> {
        self.schedule.readiness(cohort, &self.available)
    }

    pub fn available_nodes(&self) -> &BTreeSet<ProductNodeKey> {
        &self.available
    }

    pub fn remaining(&self) -> usize {
        self.batches.values().map(TileRenderBatch::remaining).sum()
    }

    /// Claim the first dependency-ready node in the scheduler's deterministic
    /// priority/topological order. A downstream job cannot be issued while an
    /// upstream render is merely claimed; it becomes eligible only after the
    /// upstream product has passed `TileRenderBatch::accept`.
    pub fn take_next_job(
        &mut self,
        loop_region: Option<RenderSpan>,
        playhead: i64,
    ) -> Option<DependencyTileRenderJob> {
        if self.is_cancelled() {
            return None;
        }
        for scheduled in self.schedule.jobs.iter() {
            if self.available.contains(&scheduled.node) {
                continue;
            }
            let node = self
                .graph
                .node(&scheduled.node)
                .expect("schedule node belongs to graph");
            if !node
                .prerequisites
                .iter()
                .all(|prerequisite| self.available.contains(prerequisite))
            {
                continue;
            }
            let batch = self
                .batches
                .get_mut(&scheduled.node)
                .expect("every node owns a tile batch");
            if let Some(tile) = batch.take_next_job(loop_region, playhead) {
                return Some(DependencyTileRenderJob {
                    node: scheduled.node.clone(),
                    cohort: self.cohorts[&scheduled.node].clone(),
                    tile,
                });
            }
        }
        None
    }

    pub fn release(&mut self, job: &DependencyTileRenderJob) -> Result<(), DependencyRuntimeError> {
        let batch = self
            .batches
            .get_mut(&job.node)
            .ok_or_else(|| DependencyRuntimeError::UnknownCompletionNode(job.node.clone()))?;
        Ok(batch.release(&job.tile)?)
    }

    pub fn accept(
        &mut self,
        completion: DependencyTileRenderCompletion,
    ) -> Result<(), DependencyRuntimeError> {
        let batch = self.batches.get_mut(&completion.node).ok_or_else(|| {
            DependencyRuntimeError::UnknownCompletionNode(completion.node.clone())
        })?;
        batch.accept(completion.tile)?;
        self.available.insert(completion.node);
        Ok(())
    }

    /// Execute one dependency-ready product through the sole frozen engine.
    /// Worker-pool adapters may instead take jobs and call the same
    /// `ExecutableRenderPlan::render_tile` method concurrently.
    pub fn execute_next(
        &mut self,
        executable: &ExecutableRenderPlan,
        loop_region: Option<RenderSpan>,
        playhead: i64,
    ) -> Result<bool, DependencyRuntimeError> {
        if executable.id() != &self.graph.plan {
            return Err(DependencyRuntimeError::ExecutablePlanMismatch);
        }
        let Some(job) = self.take_next_job(loop_region, playhead) else {
            if self.remaining() == 0 {
                return Ok(false);
            }
            return Err(DependencyRuntimeError::NoDependencyReadyJob);
        };
        let product = executable.render_tile(&job.tile.spec, &job.tile.cancellation)?;
        self.accept(DependencyTileRenderCompletion {
            node: job.node,
            tile: TileRenderCompletion {
                generation: job.tile.generation,
                target: job.tile.target,
                index: job.tile.spec.index,
                product,
            },
        })?;
        Ok(true)
    }

    pub fn finish(self) -> Result<DependencyRenderState, DependencyRuntimeError> {
        if self.is_cancelled() {
            return Err(DependencyRuntimeError::Cancelled);
        }
        if self.remaining() != 0 {
            return Err(DependencyRuntimeError::IncompleteBatch {
                remaining: self.remaining(),
            });
        }
        for cohort in self.schedule.cohorts.keys() {
            if self.readiness(cohort) != Some(ProductCohortReadiness::Ready) {
                return Err(DependencyRuntimeError::IncompleteCohort(cohort.clone()));
            }
        }
        let mut products = BTreeMap::new();
        let mut drafts = BTreeMap::<ProductCohort, TileCohortDraft>::new();
        for (node, batch) in self.batches {
            let cohort = self.cohorts[&node].clone();
            let draft = batch.finish()?;
            let entry = draft
                .products
                .first()
                .expect("single-node batch always contains one product");
            products.insert(node, Arc::clone(&entry.product));
            if let Some(existing) = drafts.remove(&cohort) {
                drafts.insert(cohort, existing.merge(draft)?);
            } else {
                drafts.insert(cohort, draft);
            }
        }
        Ok(DependencyRenderState {
            graph: self.graph,
            products,
            drafts,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DependencyRuntimeError {
    InvalidScheduleExtent,
    UnknownTopologyBus(BusId),
    UnknownRenderScope(RenderScope),
    UnsupportedExecutableScope(RenderScope),
    SchedulePlanExtentMismatch {
        schedule: RenderSpan,
        plan: RenderSpan,
    },
    AuthoredBodyOutsidePlan,
    TailContract(String),
    TailOutsidePlan {
        required: RenderSpan,
        plan: RenderSpan,
    },
    EmptyManifest,
    DuplicatePurpose,
    MissingPurposePrerequisite {
        product: ProductPurpose,
        prerequisite: ProductPurpose,
    },
    MissingAlignedPrerequisite {
        product: ProductPurpose,
        prerequisite: ProductPurpose,
        tile: i64,
    },
    MissingReusableProduct(ProductNodeKey),
    ReusableProductMismatch(ProductNodeKey),
    UnknownCompletionNode(ProductNodeKey),
    ExecutablePlanMismatch,
    NoDependencyReadyJob,
    Cancelled,
    IncompleteBatch {
        remaining: usize,
    },
    IncompleteCohort(ProductCohort),
    MissingCohort(ProductCohort),
    Dependency(String),
    Tile(String),
    Runtime(String),
    Product(String),
}

impl fmt::Display for DependencyRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScheduleExtent => formatter.write_str("render schedule has no extent"),
            Self::UnknownTopologyBus(bus) => {
                write!(formatter, "render topology names missing bus {bus}")
            }
            Self::UnknownRenderScope(scope) => {
                write!(formatter, "render topology cannot resolve scope {scope:?}")
            }
            Self::UnsupportedExecutableScope(scope) => write!(
                formatter,
                "the sole DAW engine cannot execute scope {scope:?}"
            ),
            Self::SchedulePlanExtentMismatch { schedule, plan } => write!(
                formatter,
                "render schedule {}..{} does not match plan {}..{}",
                schedule.start, schedule.end, plan.start, plan.end
            ),
            Self::AuthoredBodyOutsidePlan => {
                formatter.write_str("authored render body is outside the compiled plan")
            }
            Self::TailContract(message) => {
                write!(formatter, "invalid output-tail contract: {message}")
            }
            Self::TailOutsidePlan { required, plan } => write!(
                formatter,
                "required output tail {}..{} exceeds plan {}..{}",
                required.start, required.end, plan.start, plan.end
            ),
            Self::EmptyManifest => formatter.write_str("concrete render manifest is empty"),
            Self::DuplicatePurpose => {
                formatter.write_str("concrete render manifest repeats a product purpose")
            }
            Self::MissingPurposePrerequisite {
                product,
                prerequisite,
            } => write!(
                formatter,
                "product {product:?} depends on undeclared purpose {prerequisite:?}"
            ),
            Self::MissingAlignedPrerequisite {
                product,
                prerequisite,
                tile,
            } => write!(
                formatter,
                "product {product:?} has no tile {tile} aligned with prerequisite {prerequisite:?}"
            ),
            Self::MissingReusableProduct(node) => write!(
                formatter,
                "reuse was proven but product is unavailable for {node:?}"
            ),
            Self::ReusableProductMismatch(node) => write!(
                formatter,
                "cached product does not match reusable node {node:?}"
            ),
            Self::UnknownCompletionNode(node) => write!(
                formatter,
                "completion names unknown dependency node {node:?}"
            ),
            Self::ExecutablePlanMismatch => {
                formatter.write_str("executable render plan does not match dependency batch")
            }
            Self::NoDependencyReadyJob => formatter
                .write_str("render work remains but no dependency-ready job can be claimed"),
            Self::Cancelled => formatter.write_str("dependency render batch was cancelled"),
            Self::IncompleteBatch { remaining } => write!(
                formatter,
                "dependency render batch still has {remaining} product(s)"
            ),
            Self::IncompleteCohort(cohort) => write!(
                formatter,
                "semantic cohort {cohort:?} is not atomically ready"
            ),
            Self::MissingCohort(cohort) => {
                write!(formatter, "semantic cohort {cohort:?} was not rendered")
            }
            Self::Dependency(message) => write!(formatter, "render dependency error: {message}"),
            Self::Tile(message) => write!(formatter, "tile render error: {message}"),
            Self::Runtime(message) => write!(formatter, "render runtime error: {message}"),
            Self::Product(message) => write!(formatter, "render product error: {message}"),
        }
    }
}

impl Error for DependencyRuntimeError {}

impl From<RenderDependencyError> for DependencyRuntimeError {
    fn from(error: RenderDependencyError) -> Self {
        Self::Dependency(error.to_string())
    }
}

impl From<RenderTileError> for DependencyRuntimeError {
    fn from(error: RenderTileError) -> Self {
        Self::Tile(error.to_string())
    }
}

impl From<RenderRuntimeError> for DependencyRuntimeError {
    fn from(error: RenderRuntimeError) -> Self {
        Self::Runtime(error.to_string())
    }
}

impl From<crate::render_products::RenderProductError> for DependencyRuntimeError {
    fn from(error: crate::render_products::RenderProductError) -> Self {
        Self::Product(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_plan::{
        BusTap, DeterminismGrade, EngineRecipeStamp, ExactDigest, ProjectRevisionStamp,
        RenderFormat, RenderPlanId, Tileability,
    };
    use crate::render_products::{CohortReadiness, TileGrid};

    fn digest(byte: u8) -> ExactDigest {
        ExactDigest::new([byte; 32])
    }

    fn plan(revision: u64, snapshot: u8, extent: RenderSpan) -> RenderPlan {
        RenderPlan::new(
            RenderPlanId::new(
                77,
                digest(snapshot),
                ProjectRevisionStamp {
                    aggregate: revision,
                    air: revision,
                    ..ProjectRevisionStamp::default()
                },
                extent,
                EngineRecipeStamp::new(4, RenderFormat::new(48_000, 2).unwrap(), 256, 9, digest(2))
                    .unwrap(),
                Vec::new(),
            )
            .unwrap(),
            DeterminismGrade::BitExact,
            Tileability::BoundedHistory {
                lookbehind_frames: 2,
                lookahead_frames: 1,
            },
        )
    }

    fn topology(extent: RenderSpan, declared_tail_frames: u64) -> ConcreteRenderTopology {
        let source = BusId::from_raw(1);
        let group = BusId::from_raw(2);
        let master = BusId::from_raw(3);
        ConcreteRenderTopology {
            extent,
            master,
            upstream: BTreeMap::from([
                (source, BTreeSet::from([source])),
                (group, BTreeSet::from([source, group])),
                (master, BTreeSet::from([source, group, master])),
            ]),
            track_buses: BTreeMap::from([(10, BTreeSet::from([source]))]),
            declared_tail_frames,
        }
    }

    fn policy() -> TileRenderPolicy {
        TileRenderPolicy::new(
            TileGrid::new(8).unwrap(),
            8,
            Tileability::BoundedHistory {
                lookbehind_frames: 2,
                lookahead_frames: 1,
            },
        )
        .unwrap()
    }

    fn manifest(reverse: bool) -> ConcreteProductManifest {
        let bus = ConcreteProductRequest::new(
            ProductPurpose::Bus { bus: 2 },
            RenderScope::Bus {
                bus: 2,
                tap: BusTap::Output,
            },
            ProductCohort::Stem(2),
        )
        .consumes_forward_project();
        let master =
            ConcreteProductRequest::playback_master().depends_on(ProductPurpose::Bus { bus: 2 });
        let mut requests = vec![bus, master];
        if reverse {
            requests.reverse();
        }
        ConcreteProductManifest::new(requests).unwrap()
    }

    fn fake_product(job: &DependencyTileRenderJob, value: f32) -> Arc<RenderProduct> {
        let channels = usize::from(job.tile.spec.plan.engine.format.channels.get());
        let samples = vec![value; job.tile.spec.core.len() as usize * channels];
        Arc::new(
            RenderProduct::new(
                digest((job.tile.spec.index as u8).wrapping_add(17)),
                job.tile.spec.product_key().unwrap(),
                samples.into(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn declaration_is_insertion_order_independent_and_carries_exact_context() {
        let extent = RenderSpan::new(-8, 24).unwrap();
        let plan = plan(1, 1, extent);
        let topology = topology(extent, 0);
        let left = declare_concrete_render_graph(
            &plan,
            &topology,
            policy(),
            ConcreteOutputContract::cropped(extent),
            &manifest(false),
        )
        .unwrap();
        let right = declare_concrete_render_graph(
            &plan,
            &topology,
            policy(),
            ConcreteOutputContract::cropped(extent),
            &manifest(true),
        )
        .unwrap();
        assert_eq!(left.topological(), right.topological());

        let master = left
            .nodes()
            .find(|node| node.key.purpose == ProductPurpose::Master && node.key.core.start == 0)
            .unwrap();
        assert_eq!(master.context, RenderSpan::new(-2, 9).unwrap());
        assert_eq!(
            master.source_buses,
            BTreeSet::from([BusId::from_raw(1), BusId::from_raw(2), BusId::from_raw(3)])
        );
        assert_eq!(master.preroll_frames(), 2);
        assert_eq!(master.lookahead_frames(), 1);
        assert_eq!(master.prerequisites.len(), 1);
    }

    #[test]
    fn prerequisite_products_gate_jobs_and_cohorts_are_atomic() {
        let extent = RenderSpan::new(-8, 24).unwrap();
        let graph = Arc::new(
            declare_concrete_render_graph(
                &plan(1, 1, extent),
                &topology(extent, 0),
                policy(),
                ConcreteOutputContract::cropped(extent),
                &manifest(false),
            )
            .unwrap(),
        );
        let mut batch = DependencyRenderBatch::prepare(
            12,
            graph,
            None,
            &ChangeSet::default(),
            Some(RenderSpan::new(0, 16).unwrap()),
            3,
        )
        .unwrap();
        assert!(matches!(
            batch.readiness(&ProductCohort::Playback),
            Some(ProductCohortReadiness::Priming { .. })
        ));

        while batch.remaining() != 0 {
            let job = batch
                .take_next_job(Some(RenderSpan::new(0, 16).unwrap()), 3)
                .expect("acyclic graph always exposes dependency-ready work");
            if matches!(job.node.purpose, ProductPurpose::Master) {
                assert!(batch.available_nodes().iter().any(|node| {
                    node.purpose == ProductPurpose::Bus { bus: 2 } && node.core == job.node.core
                }));
            }
            let product = fake_product(&job, 0.25);
            batch
                .accept(DependencyTileRenderCompletion {
                    node: job.node,
                    tile: TileRenderCompletion {
                        generation: job.tile.generation,
                        target: job.tile.target,
                        index: job.tile.spec.index,
                        product,
                    },
                })
                .unwrap();
        }
        assert_eq!(
            batch.readiness(&ProductCohort::Playback),
            Some(ProductCohortReadiness::Ready)
        );
        let state = batch.finish().unwrap();
        let cohort = state.playback_cohort(&ProductCohort::Playback, 91).unwrap();
        assert_eq!(cohort.readiness(), &CohortReadiness::Ready);
        assert!(cohort.covers(&RenderScope::Master, extent));
        assert_eq!(
            state
                .cohort_draft(&ProductCohort::Stem(2))
                .unwrap()
                .products
                .len(),
            4
        );
    }

    #[test]
    fn clean_transition_requires_and_reuses_every_resident_product() {
        let extent = RenderSpan::new(-8, 24).unwrap();
        let old_graph = Arc::new(
            declare_concrete_render_graph(
                &plan(1, 1, extent),
                &topology(extent, 0),
                policy(),
                ConcreteOutputContract::cropped(extent),
                &manifest(false),
            )
            .unwrap(),
        );
        let mut cold =
            DependencyRenderBatch::prepare(1, old_graph, None, &ChangeSet::default(), None, 0)
                .unwrap();
        while let Some(job) = cold.take_next_job(None, 0) {
            let product = fake_product(&job, 0.5);
            cold.accept(DependencyTileRenderCompletion {
                node: job.node,
                tile: TileRenderCompletion {
                    generation: job.tile.generation,
                    target: job.tile.target,
                    index: job.tile.spec.index,
                    product,
                },
            })
            .unwrap();
        }
        let old = cold.finish().unwrap();

        let new_graph = Arc::new(
            declare_concrete_render_graph(
                &plan(2, 2, extent),
                &topology(extent, 0),
                policy(),
                ConcreteOutputContract::cropped(extent),
                &manifest(false),
            )
            .unwrap(),
        );
        let mut changes = ChangeSet::default();
        changes.touch(ProjectDomain::Air);
        let warm =
            DependencyRenderBatch::prepare(2, new_graph, Some(&old), &changes, None, 0).unwrap();
        assert_eq!(warm.remaining(), 0);
        assert_eq!(warm.available_nodes().len(), old.products.len());
        let adopted = warm.finish().unwrap();
        assert!(adopted
            .playback_draft()
            .unwrap()
            .products
            .iter()
            .all(|entry| matches!(entry.provenance, CohortProductProvenance::Reused { .. })));

        let mut incomplete = old.clone();
        let missing = incomplete.products.keys().next().unwrap().clone();
        incomplete.products.remove(&missing);
        let target = Arc::new(
            declare_concrete_render_graph(
                &plan(2, 2, extent),
                &topology(extent, 0),
                policy(),
                ConcreteOutputContract::cropped(extent),
                &manifest(false),
            )
            .unwrap(),
        );
        assert!(matches!(
            DependencyRenderBatch::prepare(3, target, Some(&incomplete), &changes, None, 0),
            Err(DependencyRuntimeError::MissingReusableProduct(node)) if node == missing
        ));
    }

    #[test]
    fn output_tail_must_be_compiled_into_the_exact_plan_extent() {
        let cropped = RenderSpan::new(0, 16).unwrap();
        let cropped_plan = plan(1, 1, cropped);
        // Crop is an explicit policy, not an accidental loss of a declared
        // processor tail.
        declare_concrete_render_graph(
            &cropped_plan,
            &topology(cropped, 4),
            policy(),
            ConcreteOutputContract::cropped(cropped),
            &ConcreteProductManifest::playback(),
        )
        .unwrap();
        let error = declare_concrete_render_graph(
            &cropped_plan,
            &topology(cropped, 4),
            policy(),
            ConcreteOutputContract {
                authored_body: cropped,
                tail: OutputTailPolicy::FixedFrames(4),
            },
            &ConcreteProductManifest::playback(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            DependencyRuntimeError::TailOutsidePlan { .. }
        ));

        let extended = RenderSpan::new(0, 20).unwrap();
        let graph = declare_concrete_render_graph(
            &plan(1, 1, extended),
            &topology(extended, 4),
            policy(),
            ConcreteOutputContract {
                authored_body: cropped,
                tail: OutputTailPolicy::FixedFrames(4),
            },
            &ConcreteProductManifest::playback(),
        )
        .unwrap();
        assert_eq!(graph.plan.compiled_extent, extended);
    }
}
