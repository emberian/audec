//! Incremental master-render planning over the one authoritative DAW engine.
//!
//! Tiles are an execution partition, never a second audio graph. Every tile is
//! rendered by `ExecutableRenderPlan`'s frozen `DawEngineSchedule`, assembled
//! into the same `PlaybackCohort` consumed by audition and export, and may cross
//! a revision only with an explicit range/dependency proof. This module does
//! not own threads, devices, project mutation, or cache eviction.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::artifact_catalog::sha256_content;
use crate::change_set::{AudioRange, BusImpact, ChangeSet};
use crate::daw_project::ProjectDomain;
use crate::render_plan::{
    DeterminismGrade, EngineRecipeStamp, ExactDigest, ProjectRevisionStamp, RenderDependencyStamp,
    RenderPlan, RenderPlanId, RenderScope, RenderSpan, Tileability,
};
use crate::render_products::{
    CohortProduct, CohortProductProvenance, ProductPartition, RenderProduct, RenderProductKey,
    RenderSlot, TileGrid,
};

pub const DEFAULT_TILE_FRAMES: u32 = 1 << 16;

/// Revisions the v1 master product actually consumes. AIR remains outside the
/// forward audio graph, so an evidence-only edit can re-key every tile without
/// rerendering PCM.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ConsumedRevisionStamp {
    pub arrangement: u64,
    pub sequencer: u64,
    pub automation: u64,
    pub assets: u64,
    pub mixer: u64,
    pub sample_kits: u64,
    pub bindings: u64,
}

impl ConsumedRevisionStamp {
    pub const fn master(revisions: ProjectRevisionStamp) -> Self {
        Self {
            arrangement: revisions.arrangement,
            sequencer: revisions.sequencer,
            automation: revisions.automation,
            assets: revisions.assets,
            mixer: revisions.mixer,
            sample_kits: revisions.sample_kits,
            bindings: revisions.bindings,
        }
    }

    pub fn changed_domains(self, next: Self) -> BTreeSet<ProjectDomain> {
        let mut changed = BTreeSet::new();
        for (domain, left, right) in [
            (
                ProjectDomain::Arrangement,
                self.arrangement,
                next.arrangement,
            ),
            (ProjectDomain::Sequencer, self.sequencer, next.sequencer),
            (ProjectDomain::Automation, self.automation, next.automation),
            (ProjectDomain::Assets, self.assets, next.assets),
            (ProjectDomain::Mixer, self.mixer, next.mixer),
            (
                ProjectDomain::SampleKits,
                self.sample_kits,
                next.sample_kits,
            ),
            (ProjectDomain::Bindings, self.bindings, next.bindings),
        ] {
            if left != right {
                changed.insert(domain);
            }
        }
        changed
    }
}

/// Exact inputs common to all tiles of a plan. `RenderProductKey` still keeps
/// the complete structural plan identity; this projection exists for fast,
/// explicit invalidation decisions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TileInputStamp {
    pub consumed_revisions: ConsumedRevisionStamp,
    pub engine: EngineRecipeStamp,
    pub dependencies: Arc<[RenderDependencyStamp]>,
}

impl TileInputStamp {
    pub fn from_plan(plan: &RenderPlan) -> Self {
        Self {
            consumed_revisions: ConsumedRevisionStamp::master(plan.id.revisions),
            engine: plan.id.engine.clone(),
            dependencies: plan.id.dependencies().to_vec().into(),
        }
    }
}

/// Stable tiling and boundary-state recipe selected by the project audio
/// controller. The digest is computed by that versioned adapter and must name
/// the exact context/checkpoint semantics; zero can never authorize reuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TileRenderPolicy {
    pub grid: TileGrid,
    pub maximum_context_frames: u64,
    pub boundary_recipe: ExactDigest,
    pub tileability: Tileability,
}

impl TileRenderPolicy {
    pub fn new(
        grid: TileGrid,
        maximum_context_frames: u64,
        tileability: Tileability,
    ) -> Result<Self, RenderTileError> {
        Ok(Self {
            grid,
            maximum_context_frames,
            boundary_recipe: canonical_boundary_recipe(grid, tileability),
            tileability,
        })
    }
}

/// SHA-256 identity for the exact independent-tile boundary contract. The
/// context ceiling is deliberately absent: it is a refusal threshold, not an
/// audible input. The actual history/lookahead requirement and grid are exact.
pub fn canonical_boundary_recipe(grid: TileGrid, tileability: Tileability) -> ExactDigest {
    let mut recipe = [0_u8; 21];
    recipe[..4].copy_from_slice(&grid.tile_frames().to_le_bytes());
    match tileability {
        Tileability::Stateless => recipe[4] = 0,
        Tileability::BoundedHistory {
            lookbehind_frames,
            lookahead_frames,
        } => {
            recipe[4] = 1;
            recipe[5..13].copy_from_slice(&lookbehind_frames.to_le_bytes());
            recipe[13..21].copy_from_slice(&lookahead_frames.to_le_bytes());
        }
        Tileability::Checkpointable => recipe[4] = 2,
        Tileability::SequentialOnly => recipe[4] = 3,
    }
    ExactDigest::new(sha256_content(b"audec:render-tile-boundary:v1", &[&recipe]).bytes)
}

/// Collision-resistant receipt for one exact before/after transition and its
/// normalized invalidation consequences. This is the proof identity stored on
/// every cross-plan cohort entry; a range list reused for another transition
/// therefore cannot impersonate the original command receipt.
pub fn canonical_reuse_receipt(
    previous: &RenderPlanId,
    target: &RenderPlanId,
    changes: &ChangeSet,
) -> ExactDigest {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(&(changes.domains.len() as u64).to_le_bytes());
    for domain in &changes.domains {
        encoded.push(match domain {
            ProjectDomain::Arrangement => 0,
            ProjectDomain::Sequencer => 1,
            ProjectDomain::Automation => 2,
            ProjectDomain::Assets => 3,
            ProjectDomain::Mixer => 4,
            ProjectDomain::SampleKits => 5,
            ProjectDomain::Air => 6,
            ProjectDomain::Bindings => 7,
        });
    }
    encoded.push(u8::from(changes.routing_changed));
    encoded.extend_from_slice(&(changes.audio.len() as u64).to_le_bytes());
    for (bus, impact) in &changes.audio {
        encoded.extend_from_slice(&bus.get().to_le_bytes());
        match impact {
            BusImpact::Whole => encoded.push(0),
            BusImpact::Ranges(ranges) => {
                encoded.push(1);
                encoded.extend_from_slice(&(ranges.len() as u64).to_le_bytes());
                for range in ranges {
                    encoded.extend_from_slice(&range.start.to_le_bytes());
                    encoded.extend_from_slice(&range.end.to_le_bytes());
                }
            }
        }
    }
    let previous = previous.snapshot.bytes();
    let target = target.snapshot.bytes();
    ExactDigest::new(
        sha256_content(
            b"audec:render-tile-reuse-receipt:v1",
            &[&previous, &target, &encoded],
        )
        .bytes,
    )
}

/// One engine invocation. `context` is rendered through the ordinary frozen
/// schedule, then discarded down to `core`; this is the explicit tail/preroll
/// law rather than hidden state in a tile worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TileRenderSpec {
    pub plan: RenderPlanId,
    pub scope: RenderScope,
    pub grid: TileGrid,
    pub index: i64,
    pub core: RenderSpan,
    pub context: RenderSpan,
    pub boundary_recipe: ExactDigest,
}

impl TileRenderSpec {
    pub const fn lookbehind_frames(&self) -> u64 {
        self.core.start.saturating_sub(self.context.start) as u64
    }

    pub const fn lookahead_frames(&self) -> u64 {
        self.context.end.saturating_sub(self.core.end) as u64
    }

    pub fn slot(&self) -> RenderSlot {
        RenderSlot {
            scope: self.scope.clone(),
            span: self.core,
        }
    }

    pub fn product_key(&self) -> Result<RenderProductKey, RenderTileError> {
        Ok(RenderProductKey::new(
            self.plan.clone(),
            self.scope.clone(),
            self.core,
            ProductPartition::Tile {
                grid: self.grid,
                index: self.index,
            },
            self.boundary_recipe,
        )?)
    }
}

/// Complete deterministic partition of one plan extent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TileLayout {
    pub plan: RenderPlanId,
    pub input: TileInputStamp,
    pub policy: TileRenderPolicy,
    tiles: Arc<[TileRenderSpec]>,
}

impl TileLayout {
    pub fn new(plan: &RenderPlan, policy: TileRenderPolicy) -> Result<Self, RenderTileError> {
        if policy.tileability != plan.tileability {
            return Err(RenderTileError::PolicyTileabilityMismatch {
                policy: policy.tileability,
                plan: plan.tileability,
            });
        }
        let (lookbehind, lookahead) = match plan.tileability {
            Tileability::Stateless => (0, 0),
            Tileability::BoundedHistory {
                lookbehind_frames,
                lookahead_frames,
            } => (lookbehind_frames, lookahead_frames),
            Tileability::Checkpointable => return Err(RenderTileError::CheckpointRequired),
            Tileability::SequentialOnly => return Err(RenderTileError::SequentialOnly),
        };
        let required = lookbehind.max(lookahead);
        if required > policy.maximum_context_frames {
            return Err(RenderTileError::ContextCeilingExceeded {
                required,
                ceiling: policy.maximum_context_frames,
            });
        }
        let extent = plan.extent();
        let first = policy.grid.index_for(extent.start);
        let last = policy.grid.index_for(extent.end - 1);
        let count = last
            .checked_sub(first)
            .and_then(|distance| distance.checked_add(1))
            .and_then(|count| usize::try_from(count).ok())
            .ok_or(RenderTileError::TooManyTiles)?;
        let mut tiles = Vec::with_capacity(count);
        for index in first..=last {
            let full = policy.grid.span(index)?;
            let core = full
                .intersection(extent)
                .ok_or(RenderTileError::TileOutsidePlan(index))?;
            let lookbehind =
                i64::try_from(lookbehind).map_err(|_| RenderTileError::ContextOverflow)?;
            let lookahead =
                i64::try_from(lookahead).map_err(|_| RenderTileError::ContextOverflow)?;
            let context_start = core
                .start
                .checked_sub(lookbehind)
                .ok_or(RenderTileError::ContextOverflow)?
                .max(extent.start);
            let context_end = core
                .end
                .checked_add(lookahead)
                .ok_or(RenderTileError::ContextOverflow)?
                .min(extent.end);
            let context = RenderSpan::new(context_start, context_end)
                .map_err(|_| RenderTileError::ContextOverflow)?;
            tiles.push(TileRenderSpec {
                plan: plan.id.clone(),
                scope: RenderScope::Master,
                grid: policy.grid,
                index,
                core,
                context,
                boundary_recipe: policy.boundary_recipe,
            });
        }
        Ok(Self {
            plan: plan.id.clone(),
            input: TileInputStamp::from_plan(plan),
            policy,
            tiles: tiles.into(),
        })
    }

    pub fn tiles(&self) -> &[TileRenderSpec] {
        &self.tiles
    }

    pub fn required_slots(&self) -> Vec<RenderSlot> {
        self.tiles.iter().map(TileRenderSpec::slot).collect()
    }
}

#[derive(Clone, Debug)]
pub enum TileDecision {
    Render(TileRenderSpec),
    Reuse(CohortProduct),
}

impl TileDecision {
    pub fn slot(&self) -> RenderSlot {
        match self {
            Self::Render(spec) => spec.slot(),
            Self::Reuse(product) => product.slot.clone(),
        }
    }
}

/// Explicit immutable proof supplied by the command/publication boundary.
/// The digest names the canonical before/after/change-set receipt; the planner
/// additionally verifies all structural facts it can observe locally.
#[derive(Clone, Debug)]
pub struct TileReuseProof {
    pub id: ExactDigest,
    pub changes: ChangeSet,
}

impl TileReuseProof {
    pub fn new(id: ExactDigest, changes: ChangeSet) -> Result<Self, RenderTileError> {
        if id.is_zero() {
            return Err(RenderTileError::ZeroReuseProof);
        }
        Ok(Self { id, changes })
    }
}

/// Work for one target plan. Reused products retain their old derivation and
/// are admitted to the new cohort only by `CohortProductProvenance::Reused`.
#[derive(Clone, Debug)]
pub struct TileWorkPlan {
    pub target: RenderPlanId,
    pub publication_loop: Option<RenderSpan>,
    pub decisions: Vec<TileDecision>,
}

impl TileWorkPlan {
    pub fn cold(layout: &TileLayout, publication_loop: Option<RenderSpan>) -> Self {
        Self {
            target: layout.plan.clone(),
            publication_loop,
            decisions: layout
                .tiles()
                .iter()
                .cloned()
                .map(TileDecision::Render)
                .collect(),
        }
    }

    pub fn derive(
        previous: &crate::render_products::PlaybackCohort,
        previous_plan: &RenderPlan,
        target_plan: &RenderPlan,
        target_layout: &TileLayout,
        publication_loop: Option<RenderSpan>,
        proof: &TileReuseProof,
    ) -> Result<Self, RenderTileError> {
        if previous.id.plan != previous_plan.id || target_layout.plan != target_plan.id {
            return Err(RenderTileError::PlanIdentityMismatch);
        }
        if previous_plan.id.schema_version != target_plan.id.schema_version
            || previous_plan.id.project_namespace != target_plan.id.project_namespace
            || previous_plan.extent() != target_plan.extent()
            || previous_plan.format() != target_plan.format()
            || previous_plan.determinism != DeterminismGrade::BitExact
            || target_plan.determinism != DeterminismGrade::BitExact
            || previous_plan.id.engine != target_plan.id.engine
            || previous_plan.tileability != target_plan.tileability
        {
            return Ok(Self::cold(target_layout, publication_loop));
        }
        let previous_stamp = ConsumedRevisionStamp::master(previous_plan.id.revisions);
        let target_stamp = target_layout.input.consumed_revisions;
        let changed = previous_stamp.changed_domains(target_stamp);
        let mut all_changed = changed.clone();
        if previous_plan.id.revisions.air != target_plan.id.revisions.air {
            all_changed.insert(ProjectDomain::Air);
        }
        // Dependency stamps may change for one edited sample/plugin while
        // distant timeline ranges remain bit-identical. They therefore do not
        // force a cold plan by themselves: crossing that identity boundary is
        // legal only below, under the exact command ChangeSet receipt. An
        // unexplained dependency/snapshot change has no changed domain and
        // consequently renders every tile.
        let exact_domain_proof = all_changed == proof.changes.domains;
        let same_plan = previous_plan.id == target_plan.id;
        let air_only_change = all_changed == BTreeSet::from([ProjectDomain::Air])
            && exact_domain_proof
            && !proof.changes.routing_changed
            && proof.changes.audio.is_empty();
        let range_proof_usable = !changed.is_empty()
            && exact_domain_proof
            && !proof.changes.routing_changed
            && !proof.changes.audio.is_empty();
        let previous_by_slot = previous
            .products()
            .map(|entry| (entry.slot.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let mut decisions = Vec::with_capacity(target_layout.tiles().len());
        for spec in target_layout.tiles() {
            let slot = spec.slot();
            let clean = if same_plan || air_only_change {
                true
            } else if range_proof_usable {
                !master_impact_intersects(&proof.changes, spec.context)
            } else {
                false
            };
            let reusable = clean
                .then(|| previous_by_slot.get(&slot).copied())
                .flatten()
                .filter(|entry| {
                    matches!(
                        entry.product.produced_by.partition,
                        ProductPartition::Tile { grid, index }
                            if grid == spec.grid && index == spec.index
                    ) && entry.product.produced_by.boundary_recipe == spec.boundary_recipe
                });
            if let Some(entry) = reusable {
                decisions.push(TileDecision::Reuse(CohortProduct {
                    slot,
                    product: Arc::clone(&entry.product),
                    provenance: CohortProductProvenance::Reused {
                        from_plan: previous_plan.id.clone(),
                        proof: proof.id,
                    },
                }));
            } else {
                decisions.push(TileDecision::Render(spec.clone()));
            }
        }
        Ok(Self {
            target: target_plan.id.clone(),
            publication_loop,
            decisions,
        })
    }

    pub fn render_count(&self) -> usize {
        self.decisions
            .iter()
            .filter(|decision| matches!(decision, TileDecision::Render(_)))
            .count()
    }

    pub fn reuse_count(&self) -> usize {
        self.decisions.len().saturating_sub(self.render_count())
    }

    /// Render jobs inside the active loop first, ordered forward from the
    /// playhead with wrap. Remaining jobs follow in timeline order. This is
    /// scheduling metadata only; it cannot alter samples or publication gates.
    pub fn prioritized_render_specs(
        &self,
        loop_region: Option<RenderSpan>,
        playhead: i64,
    ) -> Vec<TileRenderSpec> {
        let mut specs = self
            .decisions
            .iter()
            .filter_map(|decision| match decision {
                TileDecision::Render(spec) => Some(spec.clone()),
                TileDecision::Reuse(_) => None,
            })
            .collect::<Vec<_>>();
        specs.sort_by_key(|spec| {
            match loop_region.filter(|region| spec.core.intersects(*region)) {
                Some(region) => {
                    let anchor = spec.core.start.max(region.start);
                    let loop_frames = i128::from(region.end) - i128::from(region.start);
                    let normalized_playhead = i128::from(region.start)
                        + (i128::from(playhead) - i128::from(region.start)).rem_euclid(loop_frames);
                    // The tile currently covering the playhead is immediately
                    // useful even though its core begins behind the playhead.
                    // Only tiles wholly ahead/behind it should be ordered by
                    // their forward loop distance.
                    let distance = if i128::from(spec.core.start) <= normalized_playhead
                        && normalized_playhead < i128::from(spec.core.end)
                    {
                        0
                    } else {
                        (i128::from(anchor) - normalized_playhead).rem_euclid(loop_frames) as u64
                    };
                    (0_u8, distance, spec.index)
                }
                None => (1_u8, spec.core.start.abs_diff(playhead), spec.index),
            }
        });
        specs
    }

    pub fn finish(
        self,
        rendered: BTreeMap<i64, Arc<RenderProduct>>,
    ) -> Result<TileCohortDraft, RenderTileError> {
        let mut required = Vec::with_capacity(self.decisions.len());
        let mut products = Vec::with_capacity(self.decisions.len());
        let mut consumed = BTreeSet::new();
        for decision in self.decisions {
            let slot = decision.slot();
            required.push(slot.clone());
            match decision {
                TileDecision::Reuse(product) => products.push(product),
                TileDecision::Render(spec) => {
                    let product = rendered
                        .get(&spec.index)
                        .cloned()
                        .ok_or(RenderTileError::MissingRenderedTile(spec.index))?;
                    if product.produced_by.plan != self.target
                        || product.produced_by.scope != spec.scope
                        || product.produced_by.core != spec.core
                        || product.produced_by.boundary_recipe != spec.boundary_recipe
                        || !matches!(
                            product.produced_by.partition,
                            ProductPartition::Tile { grid, index }
                                if grid == spec.grid && index == spec.index
                        )
                    {
                        return Err(RenderTileError::RenderedTileMismatch(spec.index));
                    }
                    consumed.insert(spec.index);
                    products.push(CohortProduct {
                        slot,
                        product,
                        provenance: CohortProductProvenance::RenderedForTarget,
                    });
                }
            }
        }
        if let Some(unexpected) = rendered.keys().find(|index| !consumed.contains(index)) {
            return Err(RenderTileError::UnexpectedRenderedTile(*unexpected));
        }
        Ok(TileCohortDraft {
            plan: self.target,
            publication_loop: self.publication_loop,
            required,
            products,
        })
    }
}

fn master_impact_intersects(changes: &ChangeSet, span: RenderSpan) -> bool {
    let range = AudioRange {
        start: span.start,
        end: span.end,
    };
    changes.audio.values().any(|impact| match impact {
        BusImpact::Whole => true,
        BusImpact::Ranges(ranges) => ranges
            .iter()
            .any(|changed| changed.start < range.end && range.start < changed.end),
    })
}

/// Fully populated cohort material waiting for the ordinary RenderRuntime
/// sequence/publication service. It cannot be played directly.
#[derive(Clone, Debug)]
pub struct TileCohortDraft {
    pub plan: RenderPlanId,
    pub publication_loop: Option<RenderSpan>,
    pub required: Vec<RenderSlot>,
    pub products: Vec<CohortProduct>,
}

#[derive(Clone, Debug)]
pub struct TileRenderJob {
    pub generation: u64,
    pub target: RenderPlanId,
    pub spec: TileRenderSpec,
    pub cancellation: crate::daw_render::RenderCancellation,
}

#[derive(Clone, Debug)]
pub struct TileRenderCompletion {
    pub generation: u64,
    pub target: RenderPlanId,
    pub index: i64,
    pub product: Arc<RenderProduct>,
}

#[derive(Debug)]
pub struct TileRenderBatch {
    pub generation: u64,
    pub target: RenderPlanId,
    pub work: TileWorkPlan,
    cancellation: crate::daw_render::RenderCancellation,
    rendered: BTreeMap<i64, Arc<RenderProduct>>,
}

impl TileRenderBatch {
    pub fn new(generation: u64, work: TileWorkPlan) -> Self {
        Self::with_cancellation(
            generation,
            work,
            crate::daw_render::RenderCancellation::new(),
        )
    }

    pub fn with_cancellation(
        generation: u64,
        work: TileWorkPlan,
        cancellation: crate::daw_render::RenderCancellation,
    ) -> Self {
        Self {
            generation,
            target: work.target.clone(),
            work,
            cancellation,
            rendered: BTreeMap::new(),
        }
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

    pub fn jobs(&self, loop_region: Option<RenderSpan>, playhead: i64) -> Vec<TileRenderJob> {
        self.work
            .prioritized_render_specs(loop_region, playhead)
            .into_iter()
            .filter(|spec| !self.rendered.contains_key(&spec.index))
            .map(|spec| TileRenderJob {
                generation: self.generation,
                target: self.target.clone(),
                spec,
                cancellation: self.cancellation.clone(),
            })
            .collect()
    }

    /// Accept a worker result only while this exact target generation remains
    /// live. Superseded/cancelled work is discarded before it can enter a
    /// cohort, even if the engine happened to finish at the cancellation edge.
    pub fn accept(&mut self, completion: TileRenderCompletion) -> Result<(), RenderTileError> {
        if self.is_cancelled() {
            return Err(RenderTileError::BatchCancelled);
        }
        if completion.generation != self.generation {
            return Err(RenderTileError::StaleCompletionGeneration {
                expected: self.generation,
                actual: completion.generation,
            });
        }
        if completion.target != self.target {
            return Err(RenderTileError::CompletionPlanMismatch);
        }
        let Some(spec) = self
            .work
            .decisions
            .iter()
            .find_map(|decision| match decision {
                TileDecision::Render(spec) if spec.index == completion.index => Some(spec),
                _ => None,
            })
        else {
            return Err(RenderTileError::UnexpectedRenderedTile(completion.index));
        };
        if completion.product.produced_by.plan != self.target
            || completion.product.produced_by.scope != spec.scope
            || completion.product.produced_by.core != spec.core
            || completion.product.produced_by.boundary_recipe != spec.boundary_recipe
            || !matches!(
                completion.product.produced_by.partition,
                ProductPartition::Tile { grid, index }
                    if grid == spec.grid && index == spec.index
            )
        {
            return Err(RenderTileError::RenderedTileMismatch(completion.index));
        }
        if self.rendered.contains_key(&completion.index) {
            return Err(RenderTileError::DuplicateCompletion(completion.index));
        }
        self.rendered.insert(completion.index, completion.product);
        Ok(())
    }

    pub fn remaining(&self) -> usize {
        self.work.render_count().saturating_sub(self.rendered.len())
    }

    pub fn finish(self) -> Result<TileCohortDraft, RenderTileError> {
        if self.is_cancelled() {
            return Err(RenderTileError::BatchCancelled);
        }
        self.work.finish(self.rendered)
    }
}

#[derive(Debug)]
pub enum RenderTileError {
    ZeroReuseProof,
    PolicyTileabilityMismatch {
        policy: Tileability,
        plan: Tileability,
    },
    ContextCeilingExceeded {
        required: u64,
        ceiling: u64,
    },
    ContextOverflow,
    CheckpointRequired,
    SequentialOnly,
    TooManyTiles,
    TileOutsidePlan(i64),
    PlanIdentityMismatch,
    MissingRenderedTile(i64),
    UnexpectedRenderedTile(i64),
    RenderedTileMismatch(i64),
    BatchCancelled,
    StaleCompletionGeneration {
        expected: u64,
        actual: u64,
    },
    CompletionPlanMismatch,
    DuplicateCompletion(i64),
    Product(crate::render_products::RenderProductError),
}

impl fmt::Display for RenderTileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroReuseProof => formatter.write_str("tile reuse proof cannot be zero"),
            Self::PolicyTileabilityMismatch { .. } => {
                formatter.write_str("tile policy and render plan disagree about graph history")
            }
            Self::ContextCeilingExceeded { required, ceiling } => write!(
                formatter,
                "tile requires {required} context frames, exceeding ceiling {ceiling}"
            ),
            Self::ContextOverflow => formatter.write_str("tile context overflows the timeline"),
            Self::CheckpointRequired => {
                formatter.write_str("checkpointable graph requires a state-anchor adapter")
            }
            Self::SequentialOnly => {
                formatter.write_str("sequential-only graph cannot render independent tiles")
            }
            Self::TooManyTiles => formatter.write_str("tile layout is too large"),
            Self::TileOutsidePlan(index) => write!(formatter, "tile {index} misses plan extent"),
            Self::PlanIdentityMismatch => formatter.write_str("tile plan identity mismatch"),
            Self::MissingRenderedTile(index) => write!(formatter, "tile {index} was not rendered"),
            Self::UnexpectedRenderedTile(index) => {
                write!(formatter, "unexpected rendered tile {index}")
            }
            Self::RenderedTileMismatch(index) => {
                write!(formatter, "rendered tile {index} has the wrong derivation")
            }
            Self::BatchCancelled => formatter.write_str("tile render batch was cancelled"),
            Self::StaleCompletionGeneration { expected, actual } => write!(
                formatter,
                "tile completion generation {actual} is stale; current generation is {expected}"
            ),
            Self::CompletionPlanMismatch => {
                formatter.write_str("tile completion belongs to another render plan")
            }
            Self::DuplicateCompletion(index) => {
                write!(formatter, "tile {index} completed more than once")
            }
            Self::Product(error) => error.fmt(formatter),
        }
    }
}

impl Error for RenderTileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Product(error) => Some(error),
            _ => None,
        }
    }
}

impl From<crate::render_products::RenderProductError> for RenderTileError {
    fn from(error: crate::render_products::RenderProductError) -> Self {
        Self::Product(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mixer::BusId;
    use crate::render_plan::{EngineRecipeStamp, RenderFormat};
    use crate::render_products::{PlaybackCohort, PlaybackCohortId, RenderProduct};

    fn digest(byte: u8) -> ExactDigest {
        ExactDigest::new([byte; 32])
    }

    fn plan(revision: u64, snapshot: u8, tileability: Tileability) -> RenderPlan {
        let id = RenderPlanId::new(
            7,
            digest(snapshot),
            ProjectRevisionStamp {
                aggregate: revision,
                arrangement: revision,
                ..ProjectRevisionStamp::default()
            },
            RenderSpan::new(0, 16).unwrap(),
            EngineRecipeStamp::new(1, RenderFormat::new(48_000, 2).unwrap(), 4, 0, digest(3))
                .unwrap(),
            Vec::new(),
        )
        .unwrap();
        RenderPlan::new(id, DeterminismGrade::BitExact, tileability)
    }

    fn policy(tileability: Tileability) -> TileRenderPolicy {
        TileRenderPolicy::new(TileGrid::new(4).unwrap(), 8, tileability).unwrap()
    }

    fn tile_product(spec: &TileRenderSpec, value: f32) -> Arc<RenderProduct> {
        Arc::new(
            RenderProduct::new(
                crate::render_runtime::canonical_pcm_digest(&vec![
                    value;
                    spec.core.len() as usize * 2
                ]),
                spec.product_key().unwrap(),
                vec![value; spec.core.len() as usize * 2].into(),
            )
            .unwrap(),
        )
    }

    fn cohort(plan: &RenderPlan, layout: &TileLayout) -> PlaybackCohort {
        let products = layout
            .tiles()
            .iter()
            .map(|spec| CohortProduct {
                slot: spec.slot(),
                product: tile_product(spec, spec.index as f32),
                provenance: CohortProductProvenance::RenderedForTarget,
            })
            .collect();
        PlaybackCohort::new(
            PlaybackCohortId {
                plan: plan.id.clone(),
                sequence: 1,
            },
            None,
            layout.required_slots(),
            products,
        )
        .unwrap()
    }

    #[test]
    fn bounded_history_expands_context_and_honors_the_ceiling() {
        let bounded = plan(
            1,
            1,
            Tileability::BoundedHistory {
                lookbehind_frames: 3,
                lookahead_frames: 2,
            },
        );
        let layout = TileLayout::new(&bounded, policy(bounded.tileability)).unwrap();
        assert_eq!(layout.tiles()[1].core, RenderSpan::new(4, 8).unwrap());
        assert_eq!(layout.tiles()[1].context, RenderSpan::new(1, 10).unwrap());
        let too_small =
            TileRenderPolicy::new(TileGrid::new(4).unwrap(), 2, bounded.tileability).unwrap();
        assert!(matches!(
            TileLayout::new(&bounded, too_small),
            Err(RenderTileError::ContextCeilingExceeded {
                required: 3,
                ceiling: 2
            })
        ));
    }

    #[test]
    fn range_invalidation_rerenders_only_intersecting_tiles_and_reuses_arcs() {
        let old = plan(1, 1, Tileability::Stateless);
        let mut new = plan(2, 2, Tileability::Stateless);
        new.id.revisions.arrangement = 2;
        let old_layout = TileLayout::new(&old, policy(old.tileability)).unwrap();
        let new_layout = TileLayout::new(&new, policy(new.tileability)).unwrap();
        let old_cohort = cohort(&old, &old_layout);
        let old_first = old_cohort.products().next().unwrap().product.clone();
        let mut changes = ChangeSet::default();
        changes
            .touch(ProjectDomain::Arrangement)
            .invalidate_range(BusId::from_raw(1), AudioRange::new(8, 12).unwrap());
        let proof = TileReuseProof::new(digest(9), changes).unwrap();
        let work =
            TileWorkPlan::derive(&old_cohort, &old, &new, &new_layout, None, &proof).unwrap();
        assert_eq!(work.render_count(), 1);
        assert_eq!(work.reuse_count(), 3);
        let TileDecision::Reuse(first) = &work.decisions[0] else {
            panic!("unaffected first tile should be reused")
        };
        assert!(Arc::ptr_eq(&old_first, &first.product));
        assert!(matches!(work.decisions[2], TileDecision::Render(_)));
    }

    #[test]
    fn air_only_revision_rekeys_pcm_but_unproved_snapshot_change_does_not() {
        let old = plan(1, 1, Tileability::Stateless);
        let old_layout = TileLayout::new(&old, policy(old.tileability)).unwrap();
        let old_cohort = cohort(&old, &old_layout);

        let mut air = old.clone();
        air.id.snapshot = digest(2);
        air.id.revisions.aggregate = 2;
        air.id.revisions.air = 1;
        let air_layout = TileLayout::new(&air, policy(air.tileability)).unwrap();
        let mut air_changes = ChangeSet::default();
        air_changes.touch(ProjectDomain::Air);
        let air_work = TileWorkPlan::derive(
            &old_cohort,
            &old,
            &air,
            &air_layout,
            None,
            &TileReuseProof::new(digest(9), air_changes).unwrap(),
        )
        .unwrap();
        assert_eq!(air_work.reuse_count(), 4);

        let mut unexplained = old.clone();
        unexplained.id.snapshot = digest(3);
        unexplained.id.revisions.aggregate = 2;
        let unexplained_layout =
            TileLayout::new(&unexplained, policy(unexplained.tileability)).unwrap();
        let unexplained_work = TileWorkPlan::derive(
            &old_cohort,
            &old,
            &unexplained,
            &unexplained_layout,
            None,
            &TileReuseProof::new(digest(10), ChangeSet::default()).unwrap(),
        )
        .unwrap();
        assert_eq!(unexplained_work.render_count(), 4);
    }

    #[test]
    fn reuse_receipt_pins_transition_and_normalized_ranges() {
        let old = plan(1, 1, Tileability::Stateless);
        let next = plan(2, 2, Tileability::Stateless);
        let other = plan(2, 3, Tileability::Stateless);
        let mut left = ChangeSet::default();
        left.touch(ProjectDomain::Arrangement)
            .invalidate_range(BusId::from_raw(4), AudioRange::new(2, 6).unwrap());
        let mut right = ChangeSet::default();
        right
            .touch(ProjectDomain::Arrangement)
            .invalidate_range(BusId::from_raw(4), AudioRange::new(3, 7).unwrap());
        let receipt = canonical_reuse_receipt(&old.id, &next.id, &left);
        assert!(!receipt.is_zero());
        assert_ne!(receipt, canonical_reuse_receipt(&old.id, &other.id, &left));
        assert_ne!(receipt, canonical_reuse_receipt(&old.id, &next.id, &right));
    }

    #[test]
    fn preroll_makes_an_earlier_edit_dirty_for_the_following_tile() {
        let old = plan(
            1,
            1,
            Tileability::BoundedHistory {
                lookbehind_frames: 3,
                lookahead_frames: 0,
            },
        );
        let new = plan(
            2,
            2,
            Tileability::BoundedHistory {
                lookbehind_frames: 3,
                lookahead_frames: 0,
            },
        );
        let old_layout = TileLayout::new(&old, policy(old.tileability)).unwrap();
        let new_layout = TileLayout::new(&new, policy(new.tileability)).unwrap();
        let old_cohort = cohort(&old, &old_layout);
        let mut changes = ChangeSet::default();
        changes
            .touch(ProjectDomain::Arrangement)
            .invalidate_range(BusId::from_raw(1), AudioRange::new(2, 3).unwrap());
        let work = TileWorkPlan::derive(
            &old_cohort,
            &old,
            &new,
            &new_layout,
            None,
            &TileReuseProof::new(digest(9), changes).unwrap(),
        )
        .unwrap();
        assert!(matches!(work.decisions[0], TileDecision::Render(_)));
        assert!(matches!(work.decisions[1], TileDecision::Render(_)));
        assert!(matches!(work.decisions[2], TileDecision::Reuse(_)));
    }

    #[test]
    fn loop_jobs_are_prioritized_forward_from_the_playhead() {
        let target = plan(1, 1, Tileability::Stateless);
        let layout = TileLayout::new(&target, policy(target.tileability)).unwrap();
        let work = TileWorkPlan::cold(&layout, Some(RenderSpan::new(4, 12).unwrap()));
        let order = work
            .prioritized_render_specs(Some(RenderSpan::new(4, 12).unwrap()), 9)
            .into_iter()
            .map(|spec| spec.index)
            .collect::<Vec<_>>();
        assert_eq!(order[..2], [2, 1]);
    }

    #[test]
    fn batch_cancellation_is_shared_with_all_tile_engine_calls() {
        let target = plan(1, 1, Tileability::Stateless);
        let layout = TileLayout::new(&target, policy(target.tileability)).unwrap();
        let mut batch = TileRenderBatch::new(4, TileWorkPlan::cold(&layout, None));
        let worker_token = batch.cancellation();
        let job = batch.jobs(None, 0).remove(0);
        let product = tile_product(&job.spec, 0.5);
        assert!(matches!(
            batch.accept(TileRenderCompletion {
                generation: 3,
                target: job.target.clone(),
                index: job.spec.index,
                product: Arc::clone(&product),
            }),
            Err(RenderTileError::StaleCompletionGeneration {
                expected: 4,
                actual: 3
            })
        ));
        assert_eq!(batch.remaining(), 4);
        assert!(!worker_token.is_cancelled());
        batch.cancel();
        assert!(worker_token.is_cancelled());
        assert!(matches!(
            batch.accept(TileRenderCompletion {
                generation: 4,
                target: job.target,
                index: job.spec.index,
                product,
            }),
            Err(RenderTileError::BatchCancelled)
        ));
    }
}
