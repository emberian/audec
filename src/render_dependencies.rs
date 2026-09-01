//! Dependency-aware planning for immutable render products.
//!
//! This module is deliberately not an audio graph or a second renderer. It
//! describes why semantic products (master, bus, stem, audition, comparison)
//! depend on one another and on project/dependency inputs. The resulting
//! invalidation report and deterministic schedule are consumed by the existing
//! `render_tiles` / `render_runtime` path, which remains the sole producer of
//! PCM. No node in this graph can execute DSP, mix samples, or publish a partial
//! cohort.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::change_set::{AudioRange, BusImpact, ChangeSet};
use crate::daw_project::ProjectDomain;
use crate::mixer::BusId;
use crate::render_plan::{
    ExactDigest, RenderDependencyKey, RenderDependencyStamp, RenderPlanId, RenderScope, RenderSpan,
};
use crate::render_products::{ProductPartition, RenderProductKey, RenderSlot};

/// Semantic reason a render product exists. Purpose is independent from
/// `RenderScope`: two products may request the same engine scope while serving
/// different atomic publications and therefore must not alias scheduler state.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProductPurpose {
    Master,
    Bus {
        bus: u64,
    },
    Stem {
        stem: u64,
    },
    Audition {
        request: u64,
    },
    Comparison {
        comparison: u64,
        layer: ComparisonLayer,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ComparisonLayer {
    Original,
    Construction,
    Residual,
    Excess,
}

/// Products in one cohort become eligible for adoption together. This is a
/// scheduler fact only; playback publication still goes through
/// `PlaybackCohort` and its acknowledgement boundary.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProductCohort {
    Playback,
    Stem(u64),
    Audition(u64),
    Comparison(u64),
}

/// Stable semantic slot in the dependency graph.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProductNodeKey {
    pub purpose: ProductPurpose,
    pub scope: RenderScope,
    pub core: RenderSpan,
    pub partition: ProductPartition,
}

impl ProductNodeKey {
    pub fn slot(&self) -> RenderSlot {
        RenderSlot {
            scope: self.scope.clone(),
            span: self.core,
        }
    }

    pub fn product_key(
        &self,
        plan: RenderPlanId,
        boundary_recipe: ExactDigest,
    ) -> Result<RenderProductKey, RenderDependencyError> {
        Ok(RenderProductKey::new(
            plan,
            self.scope.clone(),
            self.core,
            self.partition.clone(),
            boundary_recipe,
        )?)
    }
}

/// One declared render-product dependency.
///
/// `context` is the actual source history/lookahead consulted to produce the
/// core. Invalidation intersects this padded span, so an edit immediately
/// before a tile cannot be incorrectly reused across a release or effect tail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderDependencyNode {
    pub key: ProductNodeKey,
    pub context: RenderSpan,
    pub boundary_recipe: ExactDigest,
    pub cohort: ProductCohort,
    pub consumed_domains: BTreeSet<ProjectDomain>,
    pub consumed_inputs: BTreeSet<RenderDependencyKey>,
    pub source_buses: BTreeSet<BusId>,
    pub prerequisites: BTreeSet<ProductNodeKey>,
}

impl RenderDependencyNode {
    pub fn new(
        key: ProductNodeKey,
        context: RenderSpan,
        boundary_recipe: ExactDigest,
        cohort: ProductCohort,
    ) -> Self {
        Self {
            key,
            context,
            boundary_recipe,
            cohort,
            consumed_domains: BTreeSet::new(),
            consumed_inputs: BTreeSet::new(),
            source_buses: BTreeSet::new(),
            prerequisites: BTreeSet::new(),
        }
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
        self.source_buses.insert(bus);
        self
    }

    pub fn depends_on(mut self, prerequisite: ProductNodeKey) -> Self {
        self.prerequisites.insert(prerequisite);
        self
    }

    pub const fn preroll_frames(&self) -> u64 {
        self.key.core.start.saturating_sub(self.context.start) as u64
    }

    pub const fn lookahead_frames(&self) -> u64 {
        self.context.end.saturating_sub(self.key.core.end) as u64
    }
}

/// Immutable dependency DAG for one exact render plan.
#[derive(Clone, Debug)]
pub struct RenderDependencyGraph {
    pub plan: RenderPlanId,
    nodes: BTreeMap<ProductNodeKey, RenderDependencyNode>,
    downstream: BTreeMap<ProductNodeKey, BTreeSet<ProductNodeKey>>,
    topological: Arc<[ProductNodeKey]>,
}

impl RenderDependencyGraph {
    pub fn new(
        plan: RenderPlanId,
        nodes: impl IntoIterator<Item = RenderDependencyNode>,
    ) -> Result<Self, RenderDependencyError> {
        let mut by_key = BTreeMap::new();
        for node in nodes {
            if by_key.insert(node.key.clone(), node).is_some() {
                return Err(RenderDependencyError::DuplicateNode);
            }
        }
        if by_key.is_empty() {
            return Err(RenderDependencyError::EmptyGraph);
        }
        let declared_inputs = plan
            .dependencies()
            .iter()
            .map(|stamp| stamp.key.clone())
            .collect::<BTreeSet<_>>();
        for node in by_key.values() {
            if !plan.compiled_extent.contains_span(node.key.core) {
                return Err(RenderDependencyError::CoreOutsidePlan {
                    node: node.key.clone(),
                });
            }
            if !plan.compiled_extent.contains_span(node.context)
                || !node.context.contains_span(node.key.core)
            {
                return Err(RenderDependencyError::InvalidContext {
                    node: node.key.clone(),
                    context: node.context,
                });
            }
            if node.boundary_recipe.is_zero() {
                return Err(RenderDependencyError::ZeroBoundaryRecipe {
                    node: node.key.clone(),
                });
            }
            node.key.product_key(plan.clone(), node.boundary_recipe)?;
            if let Some(input) = node
                .consumed_inputs
                .iter()
                .find(|input| !declared_inputs.contains(*input))
            {
                return Err(RenderDependencyError::UnknownConsumedInput {
                    node: node.key.clone(),
                    input: input.clone(),
                });
            }
            for prerequisite in &node.prerequisites {
                if prerequisite == &node.key {
                    return Err(RenderDependencyError::SelfDependency(node.key.clone()));
                }
                if !by_key.contains_key(prerequisite) {
                    return Err(RenderDependencyError::MissingPrerequisite {
                        node: node.key.clone(),
                        prerequisite: prerequisite.clone(),
                    });
                }
            }
        }

        let mut downstream = by_key
            .keys()
            .cloned()
            .map(|key| (key, BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();
        for node in by_key.values() {
            for prerequisite in &node.prerequisites {
                downstream
                    .get_mut(prerequisite)
                    .expect("prerequisite checked")
                    .insert(node.key.clone());
            }
        }
        let topological = topological_order(&by_key, &downstream)?;
        Ok(Self {
            plan,
            nodes: by_key,
            downstream,
            topological: topological.into(),
        })
    }

    pub fn node(&self, key: &ProductNodeKey) -> Option<&RenderDependencyNode> {
        self.nodes.get(key)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &RenderDependencyNode> {
        self.nodes.values()
    }

    pub fn topological(&self) -> &[ProductNodeKey] {
        &self.topological
    }

    pub fn invalidate_from(
        &self,
        previous: Option<&Self>,
        changes: &ChangeSet,
    ) -> InvalidationReport {
        let mut invalidation = self
            .nodes
            .keys()
            .cloned()
            .map(|key| (key, BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();
        let Some(previous) = previous else {
            for reasons in invalidation.values_mut() {
                reasons.insert(InvalidationReason::ColdStart);
            }
            return InvalidationReport::new(self.plan.clone(), invalidation);
        };

        let structural_mismatch = previous.plan.schema_version != self.plan.schema_version
            || previous.plan.project_namespace != self.plan.project_namespace
            || previous.plan.compiled_extent != self.plan.compiled_extent
            || previous.plan.engine.format != self.plan.engine.format;
        if structural_mismatch {
            for reasons in invalidation.values_mut() {
                reasons.insert(InvalidationReason::PlanContractChanged);
            }
            return InvalidationReport::new(self.plan.clone(), invalidation);
        }
        if previous.plan.engine != self.plan.engine {
            for reasons in invalidation.values_mut() {
                reasons.insert(InvalidationReason::EngineRecipeChanged);
            }
        }

        let changed_domains = changed_revision_domains(&previous.plan, &self.plan);
        let exact_change_receipt = changed_domains == changes.domains;
        let unexplained_snapshot = previous.plan.snapshot != self.plan.snapshot
            && changed_domains.is_empty()
            && changed_dependency_keys(previous.plan.dependencies(), self.plan.dependencies())
                .is_empty();
        if !exact_change_receipt || unexplained_snapshot {
            for reasons in invalidation.values_mut() {
                reasons.insert(if unexplained_snapshot {
                    InvalidationReason::UnprovenSnapshotChange
                } else {
                    InvalidationReason::UnprovenChangeSet
                });
            }
        }

        let changed_inputs =
            changed_dependency_keys(previous.plan.dependencies(), self.plan.dependencies());
        for node in self.nodes.values() {
            let reasons = invalidation.get_mut(&node.key).expect("node inserted");
            match previous.nodes.get(&node.key) {
                None => {
                    reasons.insert(InvalidationReason::AddedProduct);
                    continue;
                }
                Some(old) if old != node => {
                    reasons.insert(InvalidationReason::ProductRecipeChanged);
                }
                Some(_) => {}
            }

            for key in node.consumed_inputs.intersection(&changed_inputs) {
                reasons.insert(InvalidationReason::DependencyChanged(key.clone()));
            }
            if changes.routing_changed && !node.source_buses.is_empty() {
                reasons.insert(InvalidationReason::RoutingChanged);
            }

            let range_proof = exact_change_receipt
                && !changes.routing_changed
                && !changes.audio.is_empty()
                && !node.source_buses.is_empty();
            for domain in node.consumed_domains.intersection(&changed_domains) {
                if !range_proof || !is_forward_audio_domain(*domain) {
                    reasons.insert(InvalidationReason::DomainRevision(*domain));
                }
            }
            if range_proof {
                for bus in &node.source_buses {
                    if let Some(impact) = changes.audio.get(bus) {
                        for reason in intersecting_audio_reasons(*bus, impact, node.context) {
                            reasons.insert(reason);
                        }
                    }
                }
            }
        }

        // Propagate in topological order. A downstream product cannot reuse
        // old PCM while any product it semantically consumes is dirty.
        for source in self.topological.iter() {
            if invalidation
                .get(source)
                .is_some_and(|reasons| !reasons.is_empty())
            {
                for target in self.downstream.get(source).into_iter().flatten() {
                    invalidation
                        .get_mut(target)
                        .expect("downstream node exists")
                        .insert(InvalidationReason::UpstreamProduct(source.clone()));
                }
            }
        }
        InvalidationReport::new(self.plan.clone(), invalidation)
    }
}

fn topological_order(
    nodes: &BTreeMap<ProductNodeKey, RenderDependencyNode>,
    downstream: &BTreeMap<ProductNodeKey, BTreeSet<ProductNodeKey>>,
) -> Result<Vec<ProductNodeKey>, RenderDependencyError> {
    let mut indegree = nodes
        .iter()
        .map(|(key, node)| (key.clone(), node.prerequisites.len()))
        .collect::<BTreeMap<_, _>>();
    let mut ready = indegree
        .iter()
        .filter_map(|(key, &degree)| (degree == 0).then_some(key.clone()))
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::with_capacity(nodes.len());
    while let Some(key) = ready.pop_first() {
        ordered.push(key.clone());
        for target in downstream.get(&key).into_iter().flatten() {
            let degree = indegree.get_mut(target).expect("downstream node exists");
            *degree -= 1;
            if *degree == 0 {
                ready.insert(target.clone());
            }
        }
    }
    if ordered.len() != nodes.len() {
        let members = indegree
            .into_iter()
            .filter_map(|(key, degree)| (degree != 0).then_some(key))
            .collect();
        return Err(RenderDependencyError::Cycle { members });
    }
    Ok(ordered)
}

fn changed_revision_domains(
    previous: &RenderPlanId,
    target: &RenderPlanId,
) -> BTreeSet<ProjectDomain> {
    let left = previous.revisions;
    let right = target.revisions;
    [
        (
            ProjectDomain::Arrangement,
            left.arrangement,
            right.arrangement,
        ),
        (ProjectDomain::Sequencer, left.sequencer, right.sequencer),
        (ProjectDomain::Automation, left.automation, right.automation),
        (ProjectDomain::Assets, left.assets, right.assets),
        (ProjectDomain::Mixer, left.mixer, right.mixer),
        (
            ProjectDomain::SampleKits,
            left.sample_kits,
            right.sample_kits,
        ),
        (ProjectDomain::Air, left.air, right.air),
        (ProjectDomain::Bindings, left.bindings, right.bindings),
    ]
    .into_iter()
    .filter_map(|(domain, left, right)| (left != right).then_some(domain))
    .collect()
}

fn changed_dependency_keys(
    previous: &[RenderDependencyStamp],
    target: &[RenderDependencyStamp],
) -> BTreeSet<RenderDependencyKey> {
    let previous = previous
        .iter()
        .map(|stamp| (&stamp.key, stamp))
        .collect::<BTreeMap<_, _>>();
    let target = target
        .iter()
        .map(|stamp| (&stamp.key, stamp))
        .collect::<BTreeMap<_, _>>();
    previous
        .keys()
        .chain(target.keys())
        .filter(|key| previous.get(*key) != target.get(*key))
        .map(|key| (*key).clone())
        .collect()
}

fn is_forward_audio_domain(domain: ProjectDomain) -> bool {
    !matches!(domain, ProjectDomain::Air)
}

fn intersecting_audio_reasons(
    bus: BusId,
    impact: &BusImpact,
    context: RenderSpan,
) -> Vec<InvalidationReason> {
    match impact {
        BusImpact::Whole => vec![InvalidationReason::WholeBus(bus)],
        BusImpact::Ranges(ranges) => ranges
            .iter()
            .filter(|range| range.start < context.end && context.start < range.end)
            .copied()
            .map(|range| InvalidationReason::AudioRange { bus, range })
            .collect(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductInvalidation {
    Reuse,
    Render { reasons: Arc<[InvalidationReason]> },
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum InvalidationReason {
    ColdStart,
    PlanContractChanged,
    EngineRecipeChanged,
    UnprovenChangeSet,
    UnprovenSnapshotChange,
    AddedProduct,
    ProductRecipeChanged,
    DomainRevision(ProjectDomain),
    DependencyChanged(RenderDependencyKey),
    RoutingChanged,
    WholeBus(BusId),
    AudioRange { bus: BusId, range: AudioRange },
    UpstreamProduct(ProductNodeKey),
}

#[derive(Clone, Debug)]
pub struct InvalidationReport {
    pub target: RenderPlanId,
    decisions: BTreeMap<ProductNodeKey, ProductInvalidation>,
}

impl InvalidationReport {
    fn new(
        target: RenderPlanId,
        reasons: BTreeMap<ProductNodeKey, BTreeSet<InvalidationReason>>,
    ) -> Self {
        let decisions = reasons
            .into_iter()
            .map(|(key, reasons)| {
                let decision = if reasons.is_empty() {
                    ProductInvalidation::Reuse
                } else {
                    ProductInvalidation::Render {
                        reasons: reasons.into_iter().collect::<Vec<_>>().into(),
                    }
                };
                (key, decision)
            })
            .collect();
        Self { target, decisions }
    }

    pub fn decision(&self, key: &ProductNodeKey) -> Option<&ProductInvalidation> {
        self.decisions.get(key)
    }

    pub fn render_count(&self) -> usize {
        self.decisions
            .values()
            .filter(|decision| matches!(decision, ProductInvalidation::Render { .. }))
            .count()
    }

    pub fn reuse_count(&self) -> usize {
        self.decisions.len().saturating_sub(self.render_count())
    }

    pub fn diagnostics(&self) -> Vec<RenderInvalidationDiagnostic> {
        self.decisions
            .iter()
            .filter_map(|(node, decision)| match decision {
                ProductInvalidation::Reuse => None,
                ProductInvalidation::Render { reasons } => Some(RenderInvalidationDiagnostic {
                    node: node.clone(),
                    reasons: Arc::clone(reasons),
                }),
            })
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderInvalidationDiagnostic {
    pub node: ProductNodeKey,
    pub reasons: Arc<[InvalidationReason]>,
}

/// One DSP-free unit of work for the existing render runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduledRenderProduct {
    pub node: ProductNodeKey,
    pub context: RenderSpan,
    pub boundary_recipe: ExactDigest,
    pub priority: RenderProductPriority,
    pub reasons: Arc<[InvalidationReason]>,
}

impl ScheduledRenderProduct {
    pub fn product_key(
        &self,
        plan: RenderPlanId,
    ) -> Result<RenderProductKey, RenderDependencyError> {
        self.node.product_key(plan, self.boundary_recipe)
    }

    /// Mechanical handoff into the existing tile executor. The dependency
    /// scheduler never invents another render callback: a tile job becomes the
    /// exact `TileRenderSpec` already consumed by `ExecutableRenderPlan`.
    pub fn tile_spec(
        &self,
        plan: RenderPlanId,
    ) -> Result<crate::render_tiles::TileRenderSpec, RenderDependencyError> {
        let ProductPartition::Tile { grid, index } = &self.node.partition else {
            return Err(RenderDependencyError::NotATile(self.node.clone()));
        };
        // Re-run the existing product identity validation at this boundary.
        self.product_key(plan.clone())?;
        Ok(crate::render_tiles::TileRenderSpec {
            plan,
            scope: self.node.scope.clone(),
            grid: *grid,
            index: *index,
            core: self.node.core,
            context: self.context,
            boundary_recipe: self.boundary_recipe,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderProductPriority {
    Audition,
    Playhead,
    LoopAhead { frames: u64 },
    Comparison,
    Timeline { distance: u64 },
    Background,
}

impl RenderProductPriority {
    fn sort_key(self) -> (u8, u64) {
        match self {
            Self::Audition => (0, 0),
            Self::Playhead => (1, 0),
            Self::LoopAhead { frames } => (2, frames),
            Self::Comparison => (3, 0),
            Self::Timeline { distance } => (4, distance),
            Self::Background => (5, 0),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CohortRequirement {
    pub cohort: ProductCohort,
    pub required: Arc<[ProductNodeKey]>,
    pub reused: Arc<[ProductNodeKey]>,
    pub rendered: Arc<[ProductNodeKey]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductCohortReadiness {
    Priming { missing: Arc<[ProductNodeKey]> },
    Ready,
}

#[derive(Clone, Debug)]
pub struct DependencySchedule {
    pub plan: RenderPlanId,
    pub jobs: Arc<[ScheduledRenderProduct]>,
    pub cohorts: BTreeMap<ProductCohort, CohortRequirement>,
}

impl DependencySchedule {
    pub fn build(
        graph: &RenderDependencyGraph,
        report: &InvalidationReport,
        loop_region: Option<RenderSpan>,
        playhead: i64,
    ) -> Result<Self, RenderDependencyError> {
        if graph.plan != report.target {
            return Err(RenderDependencyError::ReportPlanMismatch);
        }
        let dirty = graph
            .nodes
            .keys()
            .filter(|key| {
                matches!(
                    report.decision(key),
                    Some(ProductInvalidation::Render { .. })
                )
            })
            .cloned()
            .collect::<BTreeSet<_>>();

        // Kahn scheduling over dirty nodes only. Reused prerequisites are
        // already immutable products and therefore satisfy their edges.
        let mut indegree = dirty
            .iter()
            .map(|key| {
                let degree = graph.nodes[key].prerequisites.intersection(&dirty).count();
                (key.clone(), degree)
            })
            .collect::<BTreeMap<_, _>>();
        let mut ready = indegree
            .iter()
            .filter_map(|(key, &degree)| (degree == 0).then_some(key.clone()))
            .collect::<BTreeSet<_>>();
        let mut jobs = Vec::with_capacity(dirty.len());
        while !ready.is_empty() {
            let key = ready
                .iter()
                .min_by_key(|key| {
                    (
                        product_priority(&graph.nodes[*key], loop_region, playhead).sort_key(),
                        (*key).clone(),
                    )
                })
                .expect("ready is nonempty")
                .clone();
            ready.remove(&key);
            let node = &graph.nodes[&key];
            let ProductInvalidation::Render { reasons } =
                report.decision(&key).expect("report covers graph")
            else {
                unreachable!("ready set contains only dirty products")
            };
            jobs.push(ScheduledRenderProduct {
                node: key.clone(),
                context: node.context,
                boundary_recipe: node.boundary_recipe,
                priority: product_priority(node, loop_region, playhead),
                reasons: Arc::clone(reasons),
            });
            for downstream in graph.downstream.get(&key).into_iter().flatten() {
                if let Some(degree) = indegree.get_mut(downstream) {
                    *degree -= 1;
                    if *degree == 0 {
                        ready.insert(downstream.clone());
                    }
                }
            }
        }
        if jobs.len() != dirty.len() {
            return Err(RenderDependencyError::DirtyScheduleCycle);
        }

        let mut grouped: BTreeMap<ProductCohort, Vec<ProductNodeKey>> = BTreeMap::new();
        for node in graph.nodes.values() {
            grouped
                .entry(node.cohort.clone())
                .or_default()
                .push(node.key.clone());
        }
        let cohorts = grouped
            .into_iter()
            .map(|(cohort, required)| {
                let reused = required
                    .iter()
                    .filter(|key| !dirty.contains(*key))
                    .cloned()
                    .collect::<Vec<_>>();
                let rendered = required
                    .iter()
                    .filter(|key| dirty.contains(*key))
                    .cloned()
                    .collect::<Vec<_>>();
                (
                    cohort.clone(),
                    CohortRequirement {
                        cohort,
                        required: required.into(),
                        reused: reused.into(),
                        rendered: rendered.into(),
                    },
                )
            })
            .collect();
        Ok(Self {
            plan: graph.plan.clone(),
            jobs: jobs.into(),
            cohorts,
        })
    }

    /// Cohort adoption is all-or-nothing. `available` must include both reused
    /// products admitted under an explicit receipt and newly rendered products;
    /// merely finishing the dirty subset cannot hide a failed reuse lookup.
    pub fn readiness(
        &self,
        cohort: &ProductCohort,
        available: &BTreeSet<ProductNodeKey>,
    ) -> Option<ProductCohortReadiness> {
        let requirement = self.cohorts.get(cohort)?;
        let missing = requirement
            .required
            .iter()
            .filter(|key| !available.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        Some(if missing.is_empty() {
            ProductCohortReadiness::Ready
        } else {
            ProductCohortReadiness::Priming {
                missing: missing.into(),
            }
        })
    }
}

fn product_priority(
    node: &RenderDependencyNode,
    loop_region: Option<RenderSpan>,
    playhead: i64,
) -> RenderProductPriority {
    if matches!(node.key.purpose, ProductPurpose::Audition { .. }) {
        return RenderProductPriority::Audition;
    }
    if matches!(node.key.purpose, ProductPurpose::Comparison { .. }) {
        return RenderProductPriority::Comparison;
    }
    if matches!(node.cohort, ProductCohort::Stem(_)) {
        return RenderProductPriority::Background;
    }
    if let Some(region) = loop_region.filter(|region| node.key.core.intersects(*region)) {
        let loop_frames = i128::from(region.end) - i128::from(region.start);
        let normalized_playhead = i128::from(region.start)
            + (i128::from(playhead) - i128::from(region.start)).rem_euclid(loop_frames);
        if i128::from(node.key.core.start) <= normalized_playhead
            && normalized_playhead < i128::from(node.key.core.end)
        {
            return RenderProductPriority::Playhead;
        }
        let anchor = node.key.core.start.max(region.start);
        return RenderProductPriority::LoopAhead {
            frames: (i128::from(anchor) - normalized_playhead).rem_euclid(loop_frames) as u64,
        };
    }
    RenderProductPriority::Timeline {
        distance: node.key.core.start.abs_diff(playhead),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderDependencyError {
    EmptyGraph,
    DuplicateNode,
    CoreOutsidePlan {
        node: ProductNodeKey,
    },
    InvalidContext {
        node: ProductNodeKey,
        context: RenderSpan,
    },
    ZeroBoundaryRecipe {
        node: ProductNodeKey,
    },
    UnknownConsumedInput {
        node: ProductNodeKey,
        input: RenderDependencyKey,
    },
    SelfDependency(ProductNodeKey),
    MissingPrerequisite {
        node: ProductNodeKey,
        prerequisite: ProductNodeKey,
    },
    Cycle {
        members: Vec<ProductNodeKey>,
    },
    ReportPlanMismatch,
    DirtyScheduleCycle,
    NotATile(ProductNodeKey),
    Product(crate::render_products::RenderProductError),
}

impl fmt::Display for RenderDependencyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyGraph => formatter.write_str("render dependency graph is empty"),
            Self::DuplicateNode => {
                formatter.write_str("render dependency graph has a duplicate node")
            }
            Self::CoreOutsidePlan { node } => write!(
                formatter,
                "render product core is outside its plan: {node:?}"
            ),
            Self::InvalidContext { node, context } => write!(
                formatter,
                "render product context {}..{} does not cover an in-plan core: {node:?}",
                context.start, context.end
            ),
            Self::ZeroBoundaryRecipe { node } => write!(
                formatter,
                "render product has a zero boundary recipe: {node:?}"
            ),
            Self::UnknownConsumedInput { node, input } => write!(
                formatter,
                "render product {node:?} consumes undeclared plan input {input:?}"
            ),
            Self::SelfDependency(node) => {
                write!(formatter, "render product depends on itself: {node:?}")
            }
            Self::MissingPrerequisite { node, prerequisite } => write!(
                formatter,
                "render product {node:?} names missing prerequisite {prerequisite:?}"
            ),
            Self::Cycle { members } => write!(
                formatter,
                "render dependency graph contains a cycle through {} node(s)",
                members.len()
            ),
            Self::ReportPlanMismatch => {
                formatter.write_str("invalidation report targets another render plan")
            }
            Self::DirtyScheduleCycle => {
                formatter.write_str("dirty render schedule could not satisfy dependency order")
            }
            Self::NotATile(node) => {
                write!(
                    formatter,
                    "render product is not tile-partitioned: {node:?}"
                )
            }
            Self::Product(error) => error.fmt(formatter),
        }
    }
}

impl Error for RenderDependencyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Product(error) => Some(error),
            _ => None,
        }
    }
}

impl From<crate::render_products::RenderProductError> for RenderDependencyError {
    fn from(error: crate::render_products::RenderProductError) -> Self {
        Self::Product(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_plan::{BusTap, EngineRecipeStamp, ProjectRevisionStamp, RenderFormat};
    use crate::render_products::TileGrid;

    fn digest(byte: u8) -> ExactDigest {
        ExactDigest::new([byte; 32])
    }

    fn plan(revision: u64, snapshot: u8, asset_generation: u64) -> RenderPlanId {
        RenderPlanId::new(
            7,
            digest(snapshot),
            ProjectRevisionStamp {
                aggregate: revision,
                arrangement: revision,
                ..ProjectRevisionStamp::default()
            },
            RenderSpan::new(-16, 64).unwrap(),
            EngineRecipeStamp::new(1, RenderFormat::new(48_000, 2).unwrap(), 512, 9, digest(2))
                .unwrap(),
            vec![RenderDependencyStamp {
                key: RenderDependencyKey::MediaAsset(4),
                content: digest(8),
                runtime_generation: asset_generation,
            }],
        )
        .unwrap()
    }

    fn key(purpose: ProductPurpose, scope: RenderScope, start: i64) -> ProductNodeKey {
        let grid = TileGrid::new(16).unwrap();
        ProductNodeKey {
            purpose,
            scope,
            core: RenderSpan::new(start, start + 16).unwrap(),
            partition: ProductPartition::Tile {
                grid,
                index: grid.index_for(start),
            },
        }
    }

    fn node(key: ProductNodeKey, cohort: ProductCohort) -> RenderDependencyNode {
        let context =
            RenderSpan::new(key.core.start.saturating_sub(4).max(-16), key.core.end).unwrap();
        RenderDependencyNode::new(key, context, digest(3), cohort)
    }

    fn representative_graph(
        plan: RenderPlanId,
        reverse: bool,
    ) -> (RenderDependencyGraph, Vec<ProductNodeKey>) {
        let source = BusId::from_raw(10);
        let bus_key = key(
            ProductPurpose::Bus { bus: 10 },
            RenderScope::Bus {
                bus: 10,
                tap: BusTap::Output,
            },
            0,
        );
        let stem_key = key(ProductPurpose::Stem { stem: 2 }, RenderScope::Track(2), 0);
        let master_key = key(ProductPurpose::Master, RenderScope::Master, 0);
        let audition_key = key(
            ProductPurpose::Audition { request: 5 },
            RenderScope::Bus {
                bus: 10,
                tap: BusTap::Output,
            },
            0,
        );
        let original_key = key(
            ProductPurpose::Comparison {
                comparison: 9,
                layer: ComparisonLayer::Original,
            },
            RenderScope::Track(90),
            0,
        );
        let residual_key = key(
            ProductPurpose::Comparison {
                comparison: 9,
                layer: ComparisonLayer::Residual,
            },
            RenderScope::Explanation(crate::render_plan::ExplanationScopeId {
                namespace: 1,
                local: 9,
            }),
            0,
        );
        let mut nodes = vec![
            node(bus_key.clone(), ProductCohort::Playback)
                .reads_bus(source)
                .consumes_domain(ProjectDomain::Arrangement)
                .consumes_input(RenderDependencyKey::MediaAsset(4)),
            node(stem_key.clone(), ProductCohort::Stem(2))
                .reads_bus(source)
                .consumes_domain(ProjectDomain::Arrangement),
            node(master_key.clone(), ProductCohort::Playback)
                .reads_bus(source)
                .depends_on(bus_key.clone()),
            node(audition_key.clone(), ProductCohort::Audition(5)).depends_on(bus_key.clone()),
            node(original_key.clone(), ProductCohort::Comparison(9))
                .consumes_input(RenderDependencyKey::MediaAsset(4)),
            node(residual_key.clone(), ProductCohort::Comparison(9))
                .depends_on(original_key.clone())
                .depends_on(master_key.clone()),
        ];
        if reverse {
            nodes.reverse();
        }
        (
            RenderDependencyGraph::new(plan, nodes).unwrap(),
            vec![
                bus_key,
                stem_key,
                master_key,
                audition_key,
                original_key,
                residual_key,
            ],
        )
    }

    #[test]
    fn topology_is_deterministic_across_insertion_order() {
        let (left, _) = representative_graph(plan(1, 1, 1), false);
        let (right, _) = representative_graph(plan(1, 1, 1), true);
        assert_eq!(left.topological(), right.topological());
        let position = |graph: &RenderDependencyGraph, needle: &ProductNodeKey| {
            graph
                .topological()
                .iter()
                .position(|key| key == needle)
                .unwrap()
        };
        let keys = representative_graph(plan(1, 1, 1), false).1;
        assert!(position(&left, &keys[0]) < position(&left, &keys[2]));
        assert!(position(&left, &keys[2]) < position(&left, &keys[5]));
    }

    #[test]
    fn graph_rejects_missing_and_cyclic_dependencies() {
        let a = key(ProductPurpose::Master, RenderScope::Master, 0);
        let missing = key(ProductPurpose::Stem { stem: 99 }, RenderScope::Track(99), 0);
        let error = RenderDependencyGraph::new(
            plan(1, 1, 1),
            [node(a.clone(), ProductCohort::Playback).depends_on(missing.clone())],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            RenderDependencyError::MissingPrerequisite { prerequisite, .. } if prerequisite == missing
        ));

        let b = key(ProductPurpose::Stem { stem: 2 }, RenderScope::Track(2), 0);
        let error = RenderDependencyGraph::new(
            plan(1, 1, 1),
            [
                node(a.clone(), ProductCohort::Playback).depends_on(b.clone()),
                node(b, ProductCohort::Playback).depends_on(a),
            ],
        )
        .unwrap_err();
        assert!(matches!(error, RenderDependencyError::Cycle { .. }));
    }

    #[test]
    fn range_invalidation_uses_preroll_and_propagates_to_consumers() {
        let (old, keys) = representative_graph(plan(1, 1, 1), false);
        let (new, _) = representative_graph(plan(2, 2, 1), false);
        let mut changes = ChangeSet::default();
        changes
            .touch(ProjectDomain::Arrangement)
            .invalidate_range(BusId::from_raw(10), AudioRange::new(-2, 0).unwrap());
        let report = new.invalidate_from(Some(&old), &changes);
        assert!(matches!(
            report.decision(&keys[0]),
            Some(ProductInvalidation::Render { reasons })
                if reasons.iter().any(|reason| matches!(reason, InvalidationReason::AudioRange { .. }))
        ));
        assert!(matches!(
            report.decision(&keys[2]),
            Some(ProductInvalidation::Render { .. })
        ));
        assert!(matches!(
            report.decision(&keys[3]),
            Some(ProductInvalidation::Render { .. })
        ));
        assert!(matches!(
            report.decision(&keys[5]),
            Some(ProductInvalidation::Render { .. })
        ));
    }

    #[test]
    fn distant_range_reuses_unaffected_products() {
        let (old, keys) = representative_graph(plan(1, 1, 1), false);
        let (new, _) = representative_graph(plan(2, 2, 1), false);
        let mut changes = ChangeSet::default();
        changes
            .touch(ProjectDomain::Arrangement)
            .invalidate_range(BusId::from_raw(10), AudioRange::new(40, 48).unwrap());
        let report = new.invalidate_from(Some(&old), &changes);
        assert_eq!(report.decision(&keys[0]), Some(&ProductInvalidation::Reuse));
        assert_eq!(report.decision(&keys[1]), Some(&ProductInvalidation::Reuse));
        assert_eq!(report.reuse_count(), keys.len());
    }

    #[test]
    fn dependency_change_dirties_only_consumers_then_their_downstream() {
        let (old, keys) = representative_graph(plan(1, 1, 1), false);
        let (new, _) = representative_graph(plan(1, 2, 2), false);
        let report = new.invalidate_from(Some(&old), &ChangeSet::default());
        assert!(matches!(
            report.decision(&keys[0]),
            Some(ProductInvalidation::Render { .. })
        ));
        assert_eq!(report.decision(&keys[1]), Some(&ProductInvalidation::Reuse));
        assert!(matches!(
            report.decision(&keys[2]),
            Some(ProductInvalidation::Render { .. })
        ));
        assert!(matches!(
            report.decision(&keys[4]),
            Some(ProductInvalidation::Render { .. })
        ));
        assert!(matches!(
            report.decision(&keys[5]),
            Some(ProductInvalidation::Render { .. })
        ));
    }

    #[test]
    fn scheduler_prioritizes_audition_but_never_crosses_dependencies() {
        let (graph, keys) = representative_graph(plan(1, 1, 1), false);
        let report = graph.invalidate_from(None, &ChangeSet::default());
        let schedule =
            DependencySchedule::build(&graph, &report, Some(RenderSpan::new(0, 32).unwrap()), 3)
                .unwrap();
        let at = |needle: &ProductNodeKey| {
            schedule
                .jobs
                .iter()
                .position(|job| &job.node == needle)
                .unwrap()
        };
        // Audition depends on bus, so bus must win even though audition has the
        // highest interactive priority. The now-ready audition precedes other
        // independent background products.
        assert!(at(&keys[0]) < at(&keys[3]));
        assert!(at(&keys[3]) < at(&keys[1]));
        assert!(at(&keys[2]) < at(&keys[5]));
    }

    #[test]
    fn cohort_readiness_requires_reused_and_rendered_products_together() {
        let (old, keys) = representative_graph(plan(1, 1, 1), false);
        let (new, _) = representative_graph(plan(2, 2, 1), false);
        let mut changes = ChangeSet::default();
        changes
            .touch(ProjectDomain::Arrangement)
            .invalidate_range(BusId::from_raw(10), AudioRange::new(-2, 0).unwrap());
        let report = new.invalidate_from(Some(&old), &changes);
        let schedule = DependencySchedule::build(&new, &report, None, 0).unwrap();
        let mut available = BTreeSet::from([keys[0].clone()]);
        assert!(matches!(
            schedule.readiness(&ProductCohort::Playback, &available),
            Some(ProductCohortReadiness::Priming { .. })
        ));
        available.insert(keys[2].clone());
        assert_eq!(
            schedule.readiness(&ProductCohort::Playback, &available),
            Some(ProductCohortReadiness::Ready)
        );
    }

    #[test]
    fn product_keys_remain_the_existing_offline_render_identity() {
        let (graph, _) = representative_graph(plan(1, 1, 1), false);
        for node in graph.nodes() {
            let key = node
                .key
                .product_key(graph.plan.clone(), node.boundary_recipe)
                .unwrap();
            assert_eq!(key.plan, graph.plan);
            assert_eq!(key.core, node.key.core);
            assert_eq!(key.scope, node.key.scope);
        }

        let report = graph.invalidate_from(None, &ChangeSet::default());
        let schedule = DependencySchedule::build(&graph, &report, None, 0).unwrap();
        for job in schedule.jobs.iter() {
            let spec = job.tile_spec(graph.plan.clone()).unwrap();
            assert_eq!(spec.plan, graph.plan);
            assert_eq!(spec.core, job.node.core);
            assert_eq!(spec.context, job.context);
        }
    }
}
