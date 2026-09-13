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
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::artifact_catalog::sha256_content;
use crate::change_set::{AudioRange, BusImpact, ChangeSet};
use crate::content_identity::{
    DependencySlot, Digest, IdentityError, ProductKey, RuntimeDependency, SchemaTag,
};
use crate::content_store::{FsContentStore, ObjectPin, ObjectRef, ReferenceWrite, StoreError};
use crate::daw_project::ProjectDomain;
use crate::render_plan::{
    DeterminismGrade, EngineRecipeStamp, ExactDigest, ProjectRevisionStamp, RenderDependencyStamp,
    RenderPlan, RenderPlanId, RenderScope, RenderSpan, Tileability,
};
use crate::render_products::{
    canonical_render_pcm_schema, render_product_receipt_schema, CohortProduct,
    CohortProductProvenance, ProductPartition, RenderPersistenceError, RenderProduct,
    RenderProductCatalog, RenderProductId, RenderProductKey, RenderProductReceipt, RenderSlot,
    TileGrid,
};

pub const DEFAULT_TILE_FRAMES: u32 = 1 << 16;

/// Longest preroll a tile may be given, in frames.
///
/// A tile's context is render work, not resident memory: rendering tile *i*
/// from `core.start - C` costs `(C + tile) / tile` times one tile's engine
/// time. At four tiles that is five times, which is still far less than
/// re-rendering the whole project for one edit; beyond it a whole bounce is
/// the cheaper honest answer and the fallback says so by name.
///
/// This is deliberately larger than one tile. A sampler pad or a synth voice
/// whose tail crosses a tile boundary declares a real, bounded history; a
/// ceiling equal to the tile size refused every one of them and sent projects
/// with any instrument straight to whole bounces.
pub const DEFAULT_TILE_CONTEXT_FRAMES: u64 = 4 * DEFAULT_TILE_FRAMES as u64;

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
///
/// The context is the *whole* preroll. `TileLayout` extends it back by the
/// plan's declared lookbehind and `ExecutableRenderPlan::render_tile` tells
/// the engine so (`HistorySupply::Span`), so the history flows through once.
/// An engine that prerolled again on top would render every interior tile
/// from `core.start - 2N` while [`canonical_boundary_recipe`] — and therefore
/// every cached product keyed on it — still says `N`.
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

const TILE_PRODUCT_RECIPE_SCHEMA: &str = "audec.render-tile-request";
const TILE_SNAPSHOT_OBSERVATION_SCHEMA: &str = "audec.render-project-snapshot-stamp";
const TILE_DEPENDENCY_OBSERVATION_SCHEMA: &str = "audec.render-dependency-stamp";

/// A portable request key for the exact PCM represented by one tile spec.
///
/// Session-local revision counters and project namespaces are deliberately
/// absent: the snapshot, engine recipe, dependency observations, scope and
/// context law are the audible inputs. Consequently reopening the same project
/// (or undoing back to the same content) can reuse bytes while the resident
/// [`RenderProduct`] is re-keyed to the current structural plan before it is
/// admitted to a cohort.
pub fn tile_product_request(spec: &TileRenderSpec) -> Result<ProductKey, TileProductCacheError> {
    let mut recipe_bytes = Vec::new();
    recipe_bytes.extend_from_slice(&spec.plan.schema_version.to_le_bytes());
    recipe_bytes.extend_from_slice(&spec.plan.snapshot.bytes());
    recipe_bytes.extend_from_slice(&spec.plan.compiled_extent.start.to_le_bytes());
    recipe_bytes.extend_from_slice(&spec.plan.compiled_extent.end.to_le_bytes());
    encode_engine_recipe(&mut recipe_bytes, &spec.plan.engine);
    encode_scope(&mut recipe_bytes, &spec.scope);
    recipe_bytes.extend_from_slice(&spec.grid.tile_frames().to_le_bytes());
    recipe_bytes.extend_from_slice(&spec.index.to_le_bytes());
    recipe_bytes.extend_from_slice(&spec.core.start.to_le_bytes());
    recipe_bytes.extend_from_slice(&spec.core.end.to_le_bytes());
    recipe_bytes.extend_from_slice(&spec.context.start.to_le_bytes());
    recipe_bytes.extend_from_slice(&spec.context.end.to_le_bytes());
    recipe_bytes.extend_from_slice(&spec.boundary_recipe.bytes());

    let recipe = Digest::of_bytes(
        SchemaTag::recipe(TILE_PRODUCT_RECIPE_SCHEMA, 1)?,
        &recipe_bytes,
    );
    let mut builder = ProductKey::builder(canonical_render_pcm_schema()?, recipe)?;

    let snapshot_observation = Digest::of_bytes(
        SchemaTag::runtime_observation(TILE_SNAPSHOT_OBSERVATION_SCHEMA, 1)?,
        &spec.plan.snapshot.bytes(),
    );
    builder = builder.runtime(
        DependencySlot::new("project-snapshot", 0)?,
        RuntimeDependency::new("audec.project-snapshot", 0, snapshot_observation)?,
    )?;

    for (ordinal, dependency) in spec.plan.dependencies().iter().enumerate() {
        let slot = u32::try_from(ordinal).map_err(|_| {
            TileProductCacheError::Invalid(
                "render plan contains more than u32::MAX dependencies".into(),
            )
        })?;
        let mut observation_bytes = Vec::new();
        let (role, provider) = encode_dependency_key(&mut observation_bytes, &dependency.key);
        observation_bytes.extend_from_slice(&dependency.content.bytes());
        let observation = Digest::of_bytes(
            SchemaTag::runtime_observation(TILE_DEPENDENCY_OBSERVATION_SCHEMA, 1)?,
            &observation_bytes,
        );
        builder = builder.runtime(
            DependencySlot::new(role, slot)?,
            RuntimeDependency::new(provider, dependency.runtime_generation, observation)?,
        )?;
    }
    Ok(builder.build())
}

fn encode_engine_recipe(output: &mut Vec<u8>, engine: &EngineRecipeStamp) {
    output.extend_from_slice(&engine.engine_abi.to_le_bytes());
    output.extend_from_slice(&engine.format.sample_rate.get().to_le_bytes());
    output.extend_from_slice(&engine.format.channels.get().to_le_bytes());
    output.extend_from_slice(&engine.canonical_block_frames.get().to_le_bytes());
    output.extend_from_slice(&engine.performance_seed.to_le_bytes());
    output.extend_from_slice(&engine.configuration.bytes());
}

fn encode_scope(output: &mut Vec<u8>, scope: &RenderScope) {
    match scope {
        RenderScope::Master => output.push(0),
        RenderScope::Bus { bus, tap } => {
            output.push(1);
            output.extend_from_slice(&bus.to_le_bytes());
            output.push(match tap {
                crate::render_plan::BusTap::PreFader => 0,
                crate::render_plan::BusTap::PostFader => 1,
                crate::render_plan::BusTap::Output => 2,
            });
        }
        RenderScope::Track(track) => {
            output.push(2);
            output.extend_from_slice(&track.to_le_bytes());
        }
        RenderScope::Explanation(explanation) => {
            output.push(3);
            output.extend_from_slice(&explanation.namespace.to_le_bytes());
            output.extend_from_slice(&explanation.local.to_le_bytes());
        }
    }
}

fn encode_dependency_key<'a>(
    output: &mut Vec<u8>,
    key: &'a crate::render_plan::RenderDependencyKey,
) -> (&'static str, &'static str) {
    use crate::render_plan::RenderDependencyKey;
    match key {
        RenderDependencyKey::MediaAsset(local) => {
            output.push(0);
            output.extend_from_slice(&local.to_le_bytes());
            ("media", "audec.media-asset")
        }
        RenderDependencyKey::AnalysisArtifact { namespace, local } => {
            output.push(1);
            output.extend_from_slice(&namespace.to_le_bytes());
            output.extend_from_slice(&local.to_le_bytes());
            ("analysis", "audec.analysis-artifact")
        }
        RenderDependencyKey::PluginInstance(local) => {
            output.push(2);
            output.extend_from_slice(&local.to_le_bytes());
            ("plugin", "audec.plugin-instance")
        }
        RenderDependencyKey::ModelArtifact { namespace, local } => {
            output.push(3);
            output.extend_from_slice(&namespace.to_le_bytes());
            output.extend_from_slice(&local.to_le_bytes());
            ("model", "audec.model-artifact")
        }
        RenderDependencyKey::External { namespace, local } => {
            output.push(4);
            output.extend_from_slice(&namespace.to_le_bytes());
            output.extend_from_slice(&local.to_le_bytes());
            ("external", "audec.external-dependency")
        }
    }
}

/// Non-fatal cache diagnostic retained for inspection by the controller. A
/// suspect receipt is never used merely because an engine render would be
/// slower.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TileProductCacheDiagnostic {
    pub code: &'static str,
    pub detail: String,
    pub manifest: Option<ObjectRef>,
}

/// The store namespace mapping one tile request key to the receipt that
/// answers it. Versioned in the name: a change to what a reference means gets
/// a new namespace and a fresh index pass rather than a migration.
pub const RENDER_REQUEST_NAMESPACE: &str = "render-request-v1";
/// Set once a walk has written a reference for every tile receipt the store
/// holds, so no later launch walks it again.
pub const RENDER_REQUEST_INDEX_MARK: &str = "complete";
/// How many of an index pass's diagnostics are kept. A store with a million
/// damaged objects has one problem, not a million; the count is reported.
const INDEX_DIAGNOSTIC_LIMIT: usize = 32;

/// Where the request index stands. `Absent` and `Indexing` both mean a hit is
/// not yet guaranteed for receipts written before this store had an index;
/// neither makes a miss wrong, because a miss renders the tile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderIndexState {
    /// The store carries the completion mark: every receipt in it was indexed.
    Complete,
    /// No mark yet, and no pass running.
    Absent,
    /// A pass is walking the store right now.
    Indexing,
    /// A pass ran and could not finish. The store is usable; some receipts
    /// may simply never be found again.
    Failed,
}

impl RenderIndexState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Absent => "absent",
            Self::Indexing => "indexing",
            Self::Failed => "failed",
        }
    }
}

/// What the request-index pass has done so far. Shared with the background
/// task by `Arc`, so a status request never waits on the walk itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderIndexStatus {
    pub state: RenderIndexState,
    pub objects_seen: u64,
    pub receipts_indexed: u64,
    pub references_written: u64,
    pub elapsed_ms: u64,
    pub diagnostics: u64,
    pub failure: Option<String>,
}

impl RenderIndexStatus {
    fn new(state: RenderIndexState) -> Self {
        Self {
            state,
            objects_seen: 0,
            receipts_indexed: 0,
            references_written: 0,
            elapsed_ms: 0,
            diagnostics: 0,
            failure: None,
        }
    }
}

/// Verified restart cache over generic CAS objects. Both the derivation
/// receipt and its referenced PCM are pinned in the store while this cache is
/// live because the generic store intentionally does not infer manifest
/// reachability.
///
/// **Opening reads nothing.** A receipt is found, read, verified and pinned by
/// the render that asks for its recipe: `hydrate` resolves the request key
/// through the store's `render-request-v1` reference namespace in a constant
/// number of reads, whatever the store holds. Adoption at open — a walk of
/// every object plus two fsynced pin writes per receipt — made the app's
/// start-up cost the size of its cache (106 s to the control socket on a
/// 541k-file store), and bought a launch nothing: a launch does not know which
/// tiles it will want. See docs/design/STORE_OPEN.md.
///
/// What the cache keeps in memory is the receipt, never the PCM, and only for
/// the tiles this session actually touched.
#[derive(Debug)]
pub struct TileProductCache {
    store: FsContentStore,
    owner: String,
    catalog: RenderProductCatalog,
    entries: BTreeMap<ProductKey, RenderProductReceipt>,
    ambiguous: BTreeSet<ProductKey>,
    /// Requests whose stored answer was found and refused this session (a
    /// receipt that would not decode, a payload that would not verify). They
    /// are remembered so a render loop asking tile after tile does not pay the
    /// same failed reads again.
    refused: BTreeSet<ProductKey>,
    pins: BTreeMap<ObjectRef, ObjectPin>,
    diagnostics: Vec<TileProductCacheDiagnostic>,
    index: Arc<Mutex<RenderIndexStatus>>,
}

impl TileProductCache {
    /// Open in O(1) reads: the store's layout is ensured and its index mark is
    /// read. Nothing is walked, read, pinned or adopted.
    pub fn open(
        store: FsContentStore,
        owner: impl Into<String>,
    ) -> Result<Self, TileProductCacheError> {
        let owner = owner.into();
        store.ensure_layout()?;
        let indexed = store.reference_mark(RENDER_REQUEST_NAMESPACE, RENDER_REQUEST_INDEX_MARK)?;
        Ok(Self {
            store,
            owner,
            catalog: RenderProductCatalog::default(),
            entries: BTreeMap::new(),
            ambiguous: BTreeSet::new(),
            refused: BTreeSet::new(),
            pins: BTreeMap::new(),
            diagnostics: Vec::new(),
            index: Arc::new(Mutex::new(RenderIndexStatus::new(if indexed {
                RenderIndexState::Complete
            } else {
                RenderIndexState::Absent
            }))),
        })
    }

    pub fn store(&self) -> &FsContentStore {
        &self.store
    }

    /// Receipts adopted **this session** — the tiles a render asked for and
    /// got, plus the tiles it published. Never the size of the store.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// The shared index status. Handed to the background pass so it can report
    /// progress without taking the cache lock, and read by `status.store`.
    pub fn index_handle(&self) -> Arc<Mutex<RenderIndexStatus>> {
        Arc::clone(&self.index)
    }

    pub fn index_status(&self) -> RenderIndexStatus {
        self.index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Hand the index pass's findings to the cache, where the render path
    /// drains them with everything else it was told.
    pub fn deposit_diagnostics(&mut self, diagnostics: Vec<TileProductCacheDiagnostic>) {
        self.diagnostics.extend(diagnostics);
    }

    /// Resident bytes this cache is holding right now. Opening a cache costs
    /// zero of them; only a hydrate spends any.
    pub fn resident_accounting(&self) -> crate::render_products::RenderCatalogAccounting {
        self.catalog.accounting()
    }

    pub fn diagnostics(&self) -> &[TileProductCacheDiagnostic] {
        &self.diagnostics
    }

    pub fn take_diagnostics(&mut self) -> Vec<TileProductCacheDiagnostic> {
        std::mem::take(&mut self.diagnostics)
    }

    /// Bring one product's PCM back by content identity and re-key it to the
    /// caller's derivation. This is how a cohort kept as receipts becomes
    /// audible again without anything having pinned its samples.
    pub fn rehydrate_product(
        &mut self,
        id: RenderProductId,
        produced_by: &RenderProductKey,
    ) -> Result<Option<Arc<RenderProduct>>, TileProductCacheError> {
        let Some(receipt) = self
            .entries
            .values()
            .find(|receipt| receipt.id == id)
            .cloned()
        else {
            return Ok(None);
        };
        let restored = self.catalog.rehydrate(&self.store, &receipt)?;
        if &restored.produced_by == produced_by {
            return Ok(Some(restored));
        }
        Ok(Some(Arc::new(RenderProduct::new(
            id.pcm,
            produced_by.clone(),
            restored.shared_interleaved(),
        )?)))
    }

    /// Rehydrate exact PCM and mint current structural provenance only after
    /// both the request and the persisted derivation agree with the spec.
    pub fn hydrate(
        &mut self,
        spec: &TileRenderSpec,
    ) -> Result<Option<Arc<RenderProduct>>, TileProductCacheError> {
        let request = tile_product_request(spec)?;
        if self.ambiguous.contains(&request) || self.refused.contains(&request) {
            return Ok(None);
        }
        if !self.entries.contains_key(&request) && !self.adopt_from_index(&request)? {
            return Ok(None);
        }
        let Some(receipt) = self.entries.get(&request).cloned() else {
            return Ok(None);
        };
        if !persisted_derivation_matches(spec, &receipt.produced_by) {
            self.entries.remove(&request);
            self.diagnostics.push(TileProductCacheDiagnostic {
                code: "stale-render-receipt",
                detail: "request matched but persisted tile derivation did not".into(),
                manifest: Some(receipt.manifest),
            });
            return Ok(None);
        }
        // A payload that no longer verifies is a cache miss with a name, not
        // a failed render: the engine can always produce this tile again.
        let restored = match self.catalog.rehydrate(&self.store, &receipt) {
            Ok(restored) => restored,
            Err(error) => {
                self.entries.remove(&request);
                self.diagnostics.push(TileProductCacheDiagnostic {
                    code: "render-payload-rejected",
                    detail: error.to_string(),
                    manifest: Some(receipt.manifest),
                });
                return Ok(None);
            }
        };
        let current_key = spec.product_key()?;
        let product = Arc::new(RenderProduct::new(
            restored.id.pcm,
            current_key,
            restored.shared_interleaved(),
        )?);
        Ok(Some(self.catalog.insert(product)?))
    }

    /// Publish only after the ordinary render path has produced an exact
    /// target tile. A cache failure never changes the in-memory result.
    pub fn publish(
        &mut self,
        spec: &TileRenderSpec,
        product: Arc<RenderProduct>,
    ) -> Result<(), TileProductCacheError> {
        if product.produced_by != spec.product_key()? {
            return Err(TileProductCacheError::Invalid(format!(
                "rendered tile {} does not match the cache publication spec",
                spec.index
            )));
        }
        let request = tile_product_request(spec)?;
        let persisted = self.catalog.publish(&self.store, product, request)?;
        let receipt = RenderProductReceipt {
            manifest: persisted.manifest,
            payload: persisted.payload,
            request: persisted.request,
            id: persisted.product.id,
            produced_by: persisted.product.produced_by.clone(),
        };
        // A receipt nothing can find is a receipt nothing will ever use. The
        // reference is written after the objects are durable, so the worst a
        // crash here leaves is an orphan the next render replaces — and a
        // reference that cannot be written costs this tile a future cache hit,
        // never this render.
        self.refused.remove(&receipt.request);
        if let Err(error) = self.index_receipt(&receipt) {
            self.diagnostics.push(TileProductCacheDiagnostic {
                code: "render-index-unwritable",
                detail: error.to_string(),
                manifest: Some(receipt.manifest.clone()),
            });
        }
        self.adopt(receipt)
    }

    /// Resolve one request key through the store's reference namespace and
    /// adopt what it names. Returns whether the request is now in `entries`.
    ///
    /// The reference is checked, never believed: the CAS verifies the
    /// manifest's bytes against its digest, and the decoded receipt has to
    /// name the request we asked for. A reference that disagrees with the
    /// objects is removed rather than trusted, because a hint that is wrong
    /// once is wrong every launch.
    fn adopt_from_index(&mut self, request: &ProductKey) -> Result<bool, TileProductCacheError> {
        let name = request_reference_name(request);
        let manifest = match self.store.read_reference(RENDER_REQUEST_NAMESPACE, &name) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => return Ok(false),
            Err(error) => {
                self.refuse(
                    request.clone(),
                    "render-index-unreadable",
                    error.to_string(),
                    None,
                );
                let _ = self.store.remove_reference(RENDER_REQUEST_NAMESPACE, &name);
                return Ok(false);
            }
        };
        let receipt = match RenderProductCatalog::read_receipt(&self.store, &manifest) {
            Ok(receipt) => receipt,
            Err(error) => {
                self.refuse(
                    request.clone(),
                    "render-receipt-rejected",
                    error.to_string(),
                    Some(manifest),
                );
                let _ = self.store.remove_reference(RENDER_REQUEST_NAMESPACE, &name);
                return Ok(false);
            }
        };
        if &receipt.request != request
            || !matches!(receipt.produced_by.partition, ProductPartition::Tile { .. })
        {
            self.refuse(
                request.clone(),
                "render-index-disagrees",
                "the request index names a receipt for another request".into(),
                Some(manifest),
            );
            let _ = self.store.remove_reference(RENDER_REQUEST_NAMESPACE, &name);
            return Ok(false);
        }
        // Adopting reaches the payload only to pin it. A payload that cannot
        // be pinned is a receipt this cache will not offer, not a cache that
        // fails.
        if let Err(error) = self.adopt(receipt) {
            self.refuse(
                request.clone(),
                "render-payload-rejected",
                error.to_string(),
                Some(manifest),
            );
            return Ok(false);
        }
        Ok(self.entries.contains_key(request))
    }

    fn refuse(
        &mut self,
        request: ProductKey,
        code: &'static str,
        detail: String,
        manifest: Option<ObjectRef>,
    ) {
        self.refused.insert(request);
        self.diagnostics.push(TileProductCacheDiagnostic {
            code,
            detail,
            manifest,
        });
    }

    fn index_receipt(
        &mut self,
        receipt: &RenderProductReceipt,
    ) -> Result<(), TileProductCacheError> {
        match self.store.put_reference(
            RENDER_REQUEST_NAMESPACE,
            &request_reference_name(&receipt.request),
            receipt.manifest.clone(),
        )? {
            ReferenceWrite::Created => {}
            ReferenceWrite::Present(existing) if existing == receipt.manifest => {}
            // One request key, two different receipts: the engine gave two
            // answers for the same declared inputs. Refuse to serve either.
            ReferenceWrite::Present(existing) => {
                self.entries.remove(&receipt.request);
                self.ambiguous.insert(receipt.request.clone());
                self.diagnostics.push(TileProductCacheDiagnostic {
                    code: "ambiguous-render-request",
                    detail: format!(
                        "one product request names disagreeing PCM receipts {} and {}",
                        existing.digest, receipt.manifest.digest
                    ),
                    manifest: Some(receipt.manifest.clone()),
                });
            }
        }
        Ok(())
    }

    fn adopt(&mut self, receipt: RenderProductReceipt) -> Result<(), TileProductCacheError> {
        let request = receipt.request.clone();
        if self.ambiguous.contains(&request) {
            return Ok(());
        }
        if let Some(existing) = self.entries.get(&request) {
            if existing.id != receipt.id {
                let previous = existing.manifest.clone();
                self.entries.remove(&request);
                self.ambiguous.insert(request);
                self.diagnostics.push(TileProductCacheDiagnostic {
                    code: "ambiguous-render-request",
                    detail: format!(
                        "one product request names disagreeing PCM receipts {} and {}",
                        previous.digest, receipt.manifest.digest
                    ),
                    manifest: Some(receipt.manifest),
                });
            }
            return Ok(());
        }
        self.pin_object(receipt.manifest.clone())?;
        self.pin_object(receipt.payload.clone())?;
        self.entries.insert(request, receipt);
        Ok(())
    }

    fn pin_object(&mut self, object: ObjectRef) -> Result<(), TileProductCacheError> {
        if self.pins.contains_key(&object) {
            return Ok(());
        }
        let pin = self.store.pin(&self.owner, object.clone())?;
        self.pins.insert(object, pin);
        Ok(())
    }
}

/// The store name under which the receipt answering this request is found.
/// The product key already commits to every audible input; its digest is what
/// names the answer, so the lookup costs one `open` rather than a walk.
pub fn request_reference_name(request: &ProductKey) -> String {
    request.digest().sha256().to_hex()
}

/// Give a store written before the request index one, by walking it once.
///
/// This is the only walk left, and it is not on the launch path: it runs on
/// the background executor after the window exists, against its own store
/// handle, holding no cache lock. A render that wants a tile while it runs
/// either finds the reference already written or misses and renders — which
/// is what it would have done anyway. When the walk finishes, the namespace is
/// marked and no later launch repeats it.
///
/// Ambiguity (two receipts claiming one request key with disagreeing PCM) is
/// found here, by the only reader that sees the whole store.
pub fn rebuild_render_request_index(
    store: &FsContentStore,
    progress: &Mutex<RenderIndexStatus>,
) -> Result<Vec<TileProductCacheDiagnostic>, TileProductCacheError> {
    let started = Instant::now();
    let publish = |progress: &Mutex<RenderIndexStatus>, status: RenderIndexStatus| {
        *progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = status;
    };
    let mut status = RenderIndexStatus::new(RenderIndexState::Indexing);
    publish(progress, status.clone());

    let mut diagnostics: Vec<TileProductCacheDiagnostic> = Vec::new();
    let mut record = |code: &'static str, detail: String, manifest: Option<ObjectRef>| {
        if diagnostics.len() < INDEX_DIAGNOSTIC_LIMIT {
            diagnostics.push(TileProductCacheDiagnostic {
                code,
                detail,
                manifest,
            });
        }
    };

    let inventory = match store.inventory() {
        Ok(inventory) => inventory,
        Err(error) => {
            status.state = RenderIndexState::Failed;
            status.failure = Some(error.to_string());
            status.elapsed_ms = started.elapsed().as_millis() as u64;
            publish(progress, status);
            return Err(error.into());
        }
    };
    for diagnostic in &inventory.diagnostics {
        status.diagnostics += 1;
        record(
            "cas-inventory",
            format!("{}: {}", diagnostic.path.display(), diagnostic.message),
            None,
        );
    }
    let receipt_schema = render_product_receipt_schema()?;
    let receipts = inventory
        .objects
        .into_iter()
        .filter(|stored| stored.object.digest.schema() == &receipt_schema);
    for stored in receipts {
        status.objects_seen += 1;
        let manifest = stored.object;
        let receipt = match RenderProductCatalog::read_receipt(store, &manifest) {
            Ok(receipt) => receipt,
            Err(error) => {
                status.diagnostics += 1;
                record(
                    "render-receipt-rejected",
                    error.to_string(),
                    Some(manifest.clone()),
                );
                continue;
            }
        };
        if !matches!(receipt.produced_by.partition, ProductPartition::Tile { .. }) {
            continue;
        }
        status.receipts_indexed += 1;
        match store.put_reference(
            RENDER_REQUEST_NAMESPACE,
            &request_reference_name(&receipt.request),
            manifest.clone(),
        ) {
            Ok(ReferenceWrite::Created) => status.references_written += 1,
            Ok(ReferenceWrite::Present(existing)) if existing == manifest => {}
            Ok(ReferenceWrite::Present(existing)) => {
                status.diagnostics += 1;
                record(
                    "ambiguous-render-request",
                    format!(
                        "one product request names disagreeing PCM receipts {} and {}",
                        existing.digest, manifest.digest
                    ),
                    Some(manifest.clone()),
                );
            }
            Err(error) => {
                status.state = RenderIndexState::Failed;
                status.failure = Some(error.to_string());
                status.elapsed_ms = started.elapsed().as_millis() as u64;
                publish(progress, status);
                return Err(error.into());
            }
        }
        if status.objects_seen % 1000 == 0 {
            status.elapsed_ms = started.elapsed().as_millis() as u64;
            publish(progress, status.clone());
        }
    }
    store.set_reference_mark(RENDER_REQUEST_NAMESPACE, RENDER_REQUEST_INDEX_MARK)?;
    status.state = RenderIndexState::Complete;
    status.elapsed_ms = started.elapsed().as_millis() as u64;
    publish(progress, status);
    Ok(diagnostics)
}

fn persisted_derivation_matches(spec: &TileRenderSpec, key: &RenderProductKey) -> bool {
    key.plan.schema_version == spec.plan.schema_version
        && key.plan.snapshot == spec.plan.snapshot
        && key.plan.compiled_extent == spec.plan.compiled_extent
        && key.plan.engine == spec.plan.engine
        && key.plan.dependencies() == spec.plan.dependencies()
        && key.scope == spec.scope
        && key.core == spec.core
        && key.boundary_recipe == spec.boundary_recipe
        && matches!(
            key.partition,
            ProductPartition::Tile { grid, index }
                if grid == spec.grid && index == spec.index
        )
}

#[derive(Debug)]
pub enum TileProductCacheError {
    Identity(IdentityError),
    Store(StoreError),
    Persistence(RenderPersistenceError),
    Tile(RenderTileError),
    Product(crate::render_products::RenderProductError),
    Invalid(String),
}

impl fmt::Display for TileProductCacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identity(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            Self::Persistence(error) => error.fmt(formatter),
            Self::Tile(error) => error.fmt(formatter),
            Self::Product(error) => error.fmt(formatter),
            Self::Invalid(detail) => formatter.write_str(detail),
        }
    }
}

impl Error for TileProductCacheError {}

impl From<IdentityError> for TileProductCacheError {
    fn from(error: IdentityError) -> Self {
        Self::Identity(error)
    }
}
impl From<StoreError> for TileProductCacheError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
impl From<RenderPersistenceError> for TileProductCacheError {
    fn from(error: RenderPersistenceError) -> Self {
        Self::Persistence(error)
    }
}
impl From<RenderTileError> for TileProductCacheError {
    fn from(error: RenderTileError) -> Self {
        Self::Tile(error)
    }
}
impl From<crate::render_products::RenderProductError> for TileProductCacheError {
    fn from(error: crate::render_products::RenderProductError) -> Self {
        Self::Product(error)
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
        Self::new_for_scope(plan, policy, RenderScope::Master)
    }

    pub fn new_for_scope(
        plan: &RenderPlan,
        policy: TileRenderPolicy,
        scope: RenderScope,
    ) -> Result<Self, RenderTileError> {
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
                scope: scope.clone(),
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
                !scope_impact_intersects(&proof.changes, &spec.scope, spec.context)
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
                None if loop_region.is_none() && spec.core.contains(playhead) => {
                    (0_u8, 0, spec.index)
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

fn scope_impact_intersects(changes: &ChangeSet, scope: &RenderScope, span: RenderSpan) -> bool {
    let range = AudioRange {
        start: span.start,
        end: span.end,
    };
    let intersects = |impact: &BusImpact| match impact {
        BusImpact::Whole => true,
        BusImpact::Ranges(ranges) => ranges
            .iter()
            .any(|changed| changed.start < range.end && range.start < changed.end),
    };
    match scope {
        RenderScope::Bus { bus, .. } => changes
            .audio
            .get(&crate::mixer::BusId::from_raw(*bus))
            .is_some_and(intersects),
        // Track/explanation dependency propagation is not yet independently
        // proven, so retain the conservative master law.
        RenderScope::Master | RenderScope::Track(_) | RenderScope::Explanation(_) => {
            changes.audio.values().any(intersects)
        }
    }
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

impl TileCohortDraft {
    /// Combine independently scheduled semantic scopes into one atomic cohort.
    /// Products still come from the same plan and retain per-scope derivation;
    /// the realtime service will publish only after all merged slots are ready.
    pub fn merge(mut self, other: Self) -> Result<Self, RenderTileError> {
        if self.plan != other.plan {
            return Err(RenderTileError::CompletionPlanMismatch);
        }
        if self.publication_loop != other.publication_loop {
            return Err(RenderTileError::PublicationLoopMismatch);
        }
        self.required.extend(other.required);
        self.products.extend(other.products);
        Ok(self)
    }
}

#[derive(Clone, Debug)]
pub struct TileRenderJob {
    pub generation: u64,
    pub target: RenderPlanId,
    pub spec: TileRenderSpec,
    /// Scheduling fact only. It is deliberately absent from product identity:
    /// changing the playhead can reorder work but can never change PCM.
    pub priority: TileRenderPriority,
    pub cancellation: crate::daw_render::RenderCancellation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TileRenderPriority {
    /// The dirty core currently contains the normalized loop playhead.
    Playhead,
    /// Dirty core reached after this many frames of forward loop travel.
    LoopAhead { frames: u64 },
    /// Work outside the active loop, ordered only after loop coverage.
    OutsideLoop { distance: u64 },
    /// Non-looping transport work, nearest to the playhead first.
    Timeline { distance: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TileRenderBatchStatus {
    pub generation: u64,
    pub target: RenderPlanId,
    pub total_tiles: usize,
    pub reused_tiles: usize,
    pub rendered_tiles: usize,
    pub claimed_tiles: usize,
    pub remaining_tiles: usize,
    /// True when the target revision's tile under the playhead is still dirty.
    /// Playback is not starved: the prior coherent cohort remains authoritative
    /// until every target slot is ready and one atomic publication can occur.
    pub playhead_dirty: bool,
    pub next_dirty: Option<RenderSpan>,
    pub cancelled: bool,
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
    claimed: BTreeSet<i64>,
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
            claimed: BTreeSet::new(),
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
            .filter(|spec| {
                !self.rendered.contains_key(&spec.index) && !self.claimed.contains(&spec.index)
            })
            .map(|spec| self.job(spec, loop_region, playhead))
            .collect()
    }

    /// Claim the most urgent unassigned tile. Worker-pool adapters can call
    /// this repeatedly up to their concurrency limit without issuing the same
    /// tile twice. A failed worker may return the claim with [`Self::release`].
    pub fn take_next_job(
        &mut self,
        loop_region: Option<RenderSpan>,
        playhead: i64,
    ) -> Option<TileRenderJob> {
        if self.is_cancelled() {
            return None;
        }
        let spec = self
            .work
            .prioritized_render_specs(loop_region, playhead)
            .into_iter()
            .find(|spec| {
                !self.rendered.contains_key(&spec.index) && !self.claimed.contains(&spec.index)
            })?;
        self.claimed.insert(spec.index);
        Some(self.job(spec, loop_region, playhead))
    }

    pub fn release(&mut self, job: &TileRenderJob) -> Result<(), RenderTileError> {
        if job.generation != self.generation || job.target != self.target {
            return Err(RenderTileError::CompletionPlanMismatch);
        }
        self.claimed.remove(&job.spec.index);
        Ok(())
    }

    pub fn status(&self, loop_region: Option<RenderSpan>, playhead: i64) -> TileRenderBatchStatus {
        let next = self.jobs(loop_region, playhead).into_iter().next();
        let playhead_dirty = self.work.decisions.iter().any(|decision| {
            matches!(decision, TileDecision::Render(spec)
                if spec.core.contains(playhead) && !self.rendered.contains_key(&spec.index))
        });
        TileRenderBatchStatus {
            generation: self.generation,
            target: self.target.clone(),
            total_tiles: self.work.decisions.len(),
            reused_tiles: self.work.reuse_count(),
            rendered_tiles: self.rendered.len(),
            claimed_tiles: self.claimed.len(),
            remaining_tiles: self.remaining(),
            playhead_dirty,
            next_dirty: next.map(|job| job.spec.core),
            cancelled: self.is_cancelled(),
        }
    }

    fn job(
        &self,
        spec: TileRenderSpec,
        loop_region: Option<RenderSpan>,
        playhead: i64,
    ) -> TileRenderJob {
        let priority = tile_priority(&spec, loop_region, playhead);
        TileRenderJob {
            generation: self.generation,
            target: self.target.clone(),
            spec,
            priority,
            cancellation: self.cancellation.clone(),
        }
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
        self.claimed.remove(&completion.index);
        self.rendered.insert(completion.index, completion.product);
        Ok(())
    }

    pub fn remaining(&self) -> usize {
        self.work.render_count().saturating_sub(self.rendered.len())
    }

    /// The cohort as it stands: every slot this target requires, mapped to the
    /// products that exist so far. Reused slots are already whole, so a draft
    /// taken part way through a batch is a priming manifest the renderer can
    /// play with the previous cohort under the slots still missing.
    ///
    /// Taking one costs a clone of the decisions, never of any PCM.
    pub fn draft_so_far(&self) -> Option<TileCohortDraft> {
        if self.is_cancelled() {
            return None;
        }
        let mut required = Vec::with_capacity(self.work.decisions.len());
        let mut products = Vec::new();
        for decision in &self.work.decisions {
            required.push(decision.slot());
            match decision {
                TileDecision::Reuse(product) => products.push(product.clone()),
                TileDecision::Render(spec) => {
                    let Some(product) = self.rendered.get(&spec.index) else {
                        continue;
                    };
                    products.push(CohortProduct {
                        slot: spec.slot(),
                        product: Arc::clone(product),
                        provenance: CohortProductProvenance::RenderedForTarget,
                    });
                }
            }
        }
        if products.is_empty() {
            return None;
        }
        Some(TileCohortDraft {
            plan: self.target.clone(),
            publication_loop: self.work.publication_loop,
            required,
            products,
        })
    }

    pub fn finish(self) -> Result<TileCohortDraft, RenderTileError> {
        if self.is_cancelled() {
            return Err(RenderTileError::BatchCancelled);
        }
        self.work.finish(self.rendered)
    }
}

fn tile_priority(
    spec: &TileRenderSpec,
    loop_region: Option<RenderSpan>,
    playhead: i64,
) -> TileRenderPriority {
    let Some(region) = loop_region.filter(|region| spec.core.intersects(*region)) else {
        return if loop_region.is_some() {
            TileRenderPriority::OutsideLoop {
                distance: spec.core.start.abs_diff(playhead),
            }
        } else if spec.core.contains(playhead) {
            TileRenderPriority::Playhead
        } else {
            TileRenderPriority::Timeline {
                distance: spec.core.start.abs_diff(playhead),
            }
        };
    };
    let loop_frames = i128::from(region.end) - i128::from(region.start);
    let normalized_playhead = i128::from(region.start)
        + (i128::from(playhead) - i128::from(region.start)).rem_euclid(loop_frames);
    if i128::from(spec.core.start) <= normalized_playhead
        && normalized_playhead < i128::from(spec.core.end)
    {
        return TileRenderPriority::Playhead;
    }
    let anchor = spec.core.start.max(region.start);
    TileRenderPriority::LoopAhead {
        frames: (i128::from(anchor) - normalized_playhead).rem_euclid(loop_frames) as u64,
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
    PublicationLoopMismatch,
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
            Self::PublicationLoopMismatch => {
                formatter.write_str("scoped tile drafts name different publication loops")
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
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::mixer::BusId;
    use crate::render_plan::{EngineRecipeStamp, RenderFormat};
    use crate::render_products::{
        PlaybackCohort, PlaybackCohortId, RenderProduct, RenderProductCatalog,
    };

    static CACHE_ROOT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct CacheRoot(PathBuf);

    impl CacheRoot {
        fn new() -> Self {
            let sequence = CACHE_ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "audec-render-tile-cache-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for CacheRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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

    /// A store holding `receipts` published tile receipts, the way the app
    /// leaves one behind. The cache is dropped before it is returned, so its
    /// pins are released and what remains is exactly a store at rest.
    fn published_store(root: &CacheRoot, first_snapshot: u8, receipts: usize) -> FsContentStore {
        let store = FsContentStore::new(&root.0);
        let mut cache = TileProductCache::open(store.clone(), "tile-test-fill").unwrap();
        let mut published = 0_usize;
        let mut snapshot = first_snapshot;
        while published < receipts {
            let target = plan(1, snapshot, Tileability::Stateless);
            let layout = TileLayout::new(&target, policy(target.tileability)).unwrap();
            for spec in layout.tiles() {
                if published == receipts {
                    break;
                }
                cache
                    .publish(spec, tile_product(spec, published as f32 * 0.01))
                    .unwrap();
                published += 1;
            }
            snapshot = snapshot.wrapping_add(17);
        }
        store
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

    /// Fill a store with real tile receipts, so a launch can be measured
    /// against a store that has been in use for months instead of one made
    /// this minute. Never part of a gate; `scripts/live/store_open.sh` drives
    /// it:
    ///
    /// ```text
    /// AUDEC_STORE_FILL_ROOT=/tmp/big AUDEC_STORE_FILL_RECEIPTS=100000 \
    ///   AUDEC_STORE_FILL_INDEX=1 cargo test --lib -- \
    ///   render_tiles::tests::fill_a_store_for_measurement --ignored --nocapture
    /// ```
    ///
    /// The receipts are ordinary: published through `RenderProductCatalog`
    /// with real product keys and real canonical PCM, so the walk a launch
    /// used to do reads exactly what it always read. Pins are not taken —
    /// the app releases its own on exit, and a store at rest has none.
    #[test]
    #[ignore = "writes a very large store; driven by scripts/live/store_open.sh"]
    fn fill_a_store_for_measurement() {
        let root =
            PathBuf::from(std::env::var("AUDEC_STORE_FILL_ROOT").expect("AUDEC_STORE_FILL_ROOT"));
        let receipts: usize = std::env::var("AUDEC_STORE_FILL_RECEIPTS")
            .expect("AUDEC_STORE_FILL_RECEIPTS")
            .parse()
            .expect("receipt count");
        let index = std::env::var("AUDEC_STORE_FILL_INDEX").as_deref() != Ok("0");
        let frames: i64 = std::env::var("AUDEC_STORE_FILL_TILE_FRAMES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1024);

        fs::create_dir_all(&root).unwrap();
        let store = FsContentStore::new(&root);
        store.ensure_layout().unwrap();
        let grid = TileGrid::new(frames as u32).unwrap();
        let boundary = canonical_boundary_recipe(grid, Tileability::Stateless);
        let engine =
            EngineRecipeStamp::new(1, RenderFormat::new(48_000, 2).unwrap(), 512, 0, digest(3))
                .unwrap();
        let tiles_per_plan: i64 = 1024;
        let extent = RenderSpan::new(0, tiles_per_plan * frames).unwrap();
        let workers: usize = std::env::var("AUDEC_STORE_FILL_WORKERS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(8);
        let started = std::time::Instant::now();
        let done = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for worker in 0..workers {
                let store = store.clone();
                let engine = engine.clone();
                let done = &done;
                scope.spawn(move || {
                    let mut catalog = RenderProductCatalog::new(4 * 1024 * 1024);
                    fill_receipts(
                        &store,
                        &mut catalog,
                        &engine,
                        grid,
                        boundary,
                        extent,
                        frames,
                        tiles_per_plan,
                        (worker..receipts).step_by(workers),
                        index,
                        done,
                        started,
                    );
                });
            }
        });
        if index {
            store
                .set_reference_mark(RENDER_REQUEST_NAMESPACE, RENDER_REQUEST_INDEX_MARK)
                .unwrap();
        }
        println!(
            "filled {receipts} receipts under {} in {:.1}s (index: {index})",
            root.display(),
            started.elapsed().as_secs_f64()
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn fill_receipts(
        store: &FsContentStore,
        catalog: &mut RenderProductCatalog,
        engine: &EngineRecipeStamp,
        grid: TileGrid,
        boundary: ExactDigest,
        extent: RenderSpan,
        frames: i64,
        tiles_per_plan: i64,
        ordinals: impl Iterator<Item = usize>,
        index: bool,
        done: &AtomicU64,
        started: std::time::Instant,
    ) {
        for ordinal in ordinals {
            let mut snapshot = [0_u8; 32];
            snapshot[..8].copy_from_slice(&((ordinal as u64) / 1024).to_le_bytes());
            let index_in_plan = (ordinal as i64) % tiles_per_plan;
            let plan = RenderPlanId::new(
                7,
                ExactDigest::new(snapshot),
                ProjectRevisionStamp::default(),
                extent,
                engine.clone(),
                Vec::new(),
            )
            .unwrap();
            let core =
                RenderSpan::new(index_in_plan * frames, (index_in_plan + 1) * frames).unwrap();
            let spec = TileRenderSpec {
                plan,
                scope: RenderScope::Master,
                grid,
                index: index_in_plan,
                core,
                context: core,
                boundary_recipe: boundary,
            };
            let samples = vec![ordinal as f32 * 1e-6; (frames * 2) as usize];
            let product = Arc::new(
                RenderProduct::new(
                    crate::render_runtime::canonical_pcm_digest(&samples),
                    spec.product_key().unwrap(),
                    samples.into(),
                )
                .unwrap(),
            );
            let request = tile_product_request(&spec).unwrap();
            let persisted = catalog.publish(&store, product, request.clone()).unwrap();
            if index {
                store
                    .put_reference(
                        RENDER_REQUEST_NAMESPACE,
                        &request_reference_name(&request),
                        persisted.manifest,
                    )
                    .unwrap();
            }
            let complete = done.fetch_add(1, Ordering::Relaxed) + 1;
            if complete % 5000 == 0 {
                println!(
                    "filled {complete} receipts in {:.1}s",
                    started.elapsed().as_secs_f64()
                );
            }
        }
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
    fn tile_request_is_portable_across_session_counters_but_commits_context() {
        let first = plan(1, 7, Tileability::Stateless);
        let first_layout = TileLayout::new(&first, policy(first.tileability)).unwrap();
        let first_spec = &first_layout.tiles()[0];

        let mut reopened = first.clone();
        reopened.id.project_namespace = 999;
        reopened.id.revisions.aggregate = 91;
        reopened.id.revisions.arrangement = 73;
        let reopened_layout = TileLayout::new(&reopened, policy(reopened.tileability)).unwrap();
        assert_eq!(
            tile_product_request(first_spec).unwrap(),
            tile_product_request(&reopened_layout.tiles()[0]).unwrap()
        );

        let mut changed_context = first_spec.clone();
        changed_context.context = RenderSpan::new(0, 5).unwrap();
        assert_ne!(
            tile_product_request(first_spec).unwrap(),
            tile_product_request(&changed_context).unwrap()
        );
    }

    #[test]
    fn persistent_tile_cache_reopens_and_rekeys_verified_pcm() {
        let root = CacheRoot::new();
        let store = FsContentStore::new(&root.0);
        let first = plan(1, 7, Tileability::Stateless);
        let layout = TileLayout::new(&first, policy(first.tileability)).unwrap();
        let spec = &layout.tiles()[1];
        let original = tile_product(spec, 0.375);
        {
            let mut cache = TileProductCache::open(store.clone(), "tile-test-first").unwrap();
            cache.publish(spec, Arc::clone(&original)).unwrap();
            assert_eq!(cache.entry_count(), 1);
            assert_eq!(store.inventory().unwrap().pins, 2);
        }

        let mut reopened_plan = first.clone();
        reopened_plan.id.project_namespace = 998;
        reopened_plan.id.revisions.aggregate = 41;
        reopened_plan.id.revisions.arrangement = 39;
        let reopened_layout =
            TileLayout::new(&reopened_plan, policy(reopened_plan.tileability)).unwrap();
        let reopened_spec = &reopened_layout.tiles()[1];
        let mut cache = TileProductCache::open(store, "tile-test-reopen").unwrap();
        let hydrated = cache.hydrate(reopened_spec).unwrap().unwrap();
        assert_eq!(hydrated.id, original.id);
        assert_eq!(hydrated.produced_by, reopened_spec.product_key().unwrap());
        assert_eq!(hydrated.interleaved(), original.interleaved());
        assert!(cache.diagnostics().is_empty());
    }

    /// Opening a restart cache reads *nothing*: not the objects, not the
    /// receipts, not one pin. A launch does not know which tiles it will want,
    /// and the store may hold half a million it will not. The measured claim
    /// is that the reads at open do not depend on how much the store holds.
    #[test]
    fn opening_the_cache_costs_no_reads_however_many_receipts_the_store_hold() {
        let small_root = CacheRoot::new();
        let small = published_store(&small_root, 1, 4);
        let large_root = CacheRoot::new();
        let large = published_store(&large_root, 2, 64);

        let before_small = small.object_opens();
        let opened_small = TileProductCache::open(small.clone(), "tile-open-small").unwrap();
        let small_opens = small.object_opens() - before_small;

        let before_large = large.object_opens();
        let opened_large = TileProductCache::open(large.clone(), "tile-open-large").unwrap();
        let large_opens = large.object_opens() - before_large;

        assert_eq!(
            (small_opens, large_opens),
            (0, 0),
            "opening a product cache read object files"
        );
        assert_eq!(opened_small.entry_count(), 0);
        assert_eq!(opened_large.entry_count(), 0);
        // Nothing is pinned on behalf of a render nobody has asked for.
        assert_eq!(large.inventory().unwrap().pins, 0);
    }

    /// The first hydrate is what spends reads and bytes, and it spends a
    /// constant number of them: the reference, the receipt, the payload.
    #[test]
    fn the_render_that_wants_a_tile_adopts_it_and_nothing_else() {
        let root = CacheRoot::new();
        let target = plan(1, 7, Tileability::Stateless);
        let layout = TileLayout::new(&target, policy(target.tileability)).unwrap();
        let store = FsContentStore::new(&root.0);
        {
            let mut cache = TileProductCache::open(store.clone(), "tile-test-receipts").unwrap();
            for spec in layout.tiles() {
                cache.publish(spec, tile_product(spec, 0.125)).unwrap();
            }
            assert_eq!(cache.entry_count(), layout.tiles().len());
        }
        assert!(layout.tiles().len() > 1);

        let mut reopened = TileProductCache::open(store.clone(), "tile-test-receipts-2").unwrap();
        assert_eq!(reopened.entry_count(), 0);
        assert_eq!(reopened.resident_accounting().resident_bytes, 0);
        assert_eq!(reopened.resident_accounting().entries, 0);

        let spec = &layout.tiles()[0];
        let before = store.object_opens();
        let hydrated = reopened.hydrate(spec).unwrap().expect("receipt hydrates");
        let opens = store.object_opens() - before;
        assert_eq!(hydrated.produced_by, spec.product_key().unwrap());
        assert_eq!(reopened.entry_count(), 1, "one tile wanted, one adopted");
        assert!(
            opens <= 6,
            "hydrating one tile opened {opens} object files; it should read the manifest, \
             verify both objects it pins, and read the payload"
        );
        assert_eq!(
            reopened.resident_accounting().resident_bytes,
            (hydrated.interleaved().len() * size_of::<f32>()) as u64
        );
        assert_eq!(reopened.resident_accounting().entries, 1);
        // Two pins for the one adopted tile, not two per receipt in the store.
        assert_eq!(store.inventory().unwrap().pins, 2);
    }

    /// A store written before the request index existed keeps its tiles: the
    /// background pass walks it once, writes the references, and marks the
    /// namespace so no later launch walks again.
    #[test]
    fn a_store_without_an_index_is_walked_once_and_then_never_again() {
        let root = CacheRoot::new();
        let store = FsContentStore::new(&root.0);
        let target = plan(1, 7, Tileability::Stateless);
        let layout = TileLayout::new(&target, policy(target.tileability)).unwrap();
        let spec = &layout.tiles()[0];
        let published = tile_product(spec, 0.5);
        // Published the way a previous version of audec published: the objects
        // are written, and nothing names them by request.
        RenderProductCatalog::default()
            .publish(
                &store,
                Arc::clone(&published),
                tile_product_request(spec).unwrap(),
            )
            .unwrap();

        let mut cold = TileProductCache::open(store.clone(), "tile-index-cold").unwrap();
        assert_eq!(cold.index_status().state, RenderIndexState::Absent);
        assert!(
            cold.hydrate(spec).unwrap().is_none(),
            "an unindexed receipt is a miss, not a walk"
        );

        let status = Mutex::new(RenderIndexStatus::new(RenderIndexState::Absent));
        let diagnostics = rebuild_render_request_index(&store, &status).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let status = status.into_inner().unwrap();
        assert_eq!(status.state, RenderIndexState::Complete);
        assert_eq!((status.receipts_indexed, status.references_written), (1, 1));

        let mut warm = TileProductCache::open(store.clone(), "tile-index-warm").unwrap();
        assert_eq!(warm.index_status().state, RenderIndexState::Complete);
        let hydrated = warm
            .hydrate(spec)
            .unwrap()
            .expect("indexed receipt hydrates");
        assert_eq!(hydrated.interleaved(), published.interleaved());

        // The second pass is not a second walk: the mark is the whole answer.
        let before = store.object_opens();
        let again = TileProductCache::open(store.clone(), "tile-index-again").unwrap();
        assert_eq!(store.object_opens() - before, 0);
        assert_eq!(again.index_status().state, RenderIndexState::Complete);
    }

    /// An index that disagrees with the objects is not believed. The reference
    /// is a hint; the receipt behind it has to name the request that asked.
    #[test]
    fn a_reference_that_disagrees_with_the_objects_is_removed_not_trusted() {
        let root = CacheRoot::new();
        let store = FsContentStore::new(&root.0);
        let target = plan(1, 7, Tileability::Stateless);
        let layout = TileLayout::new(&target, policy(target.tileability)).unwrap();
        let wanted = &layout.tiles()[0];
        let other = &layout.tiles()[1];
        {
            let mut cache = TileProductCache::open(store.clone(), "tile-index-liar").unwrap();
            cache.publish(other, tile_product(other, 0.25)).unwrap();
        }
        let other_manifest = store
            .read_reference(
                RENDER_REQUEST_NAMESPACE,
                &request_reference_name(&tile_product_request(other).unwrap()),
            )
            .unwrap()
            .expect("the published tile is indexed");
        // Point the wanted tile's name at the other tile's receipt.
        let wanted_name = request_reference_name(&tile_product_request(wanted).unwrap());
        store
            .put_reference(RENDER_REQUEST_NAMESPACE, &wanted_name, other_manifest)
            .unwrap();

        let mut cache = TileProductCache::open(store.clone(), "tile-index-liar-2").unwrap();
        assert!(cache.hydrate(wanted).unwrap().is_none());
        assert!(cache
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "render-index-disagrees"));
        assert_eq!(
            store
                .read_reference(RENDER_REQUEST_NAMESPACE, &wanted_name)
                .unwrap(),
            None,
            "a hint that lied once would lie every launch"
        );
        // The tile it did name is still exactly as findable as it was.
        assert!(cache.hydrate(other).unwrap().is_some());
    }

    /// A reference whose object has been collected is a miss with a name, not
    /// an error and not a crash: GC never reads `refs/`, so this is the
    /// ordinary end of a hint's life.
    #[test]
    fn a_reference_to_a_collected_object_is_a_named_miss() {
        let root = CacheRoot::new();
        let store = FsContentStore::new(&root.0);
        let target = plan(1, 7, Tileability::Stateless);
        let layout = TileLayout::new(&target, policy(target.tileability)).unwrap();
        let spec = &layout.tiles()[0];
        {
            let mut cache = TileProductCache::open(store.clone(), "tile-index-gone").unwrap();
            cache.publish(spec, tile_product(spec, 0.75)).unwrap();
        }
        let receipt_schema = render_product_receipt_schema().unwrap();
        let manifest = store
            .inventory()
            .unwrap()
            .objects
            .into_iter()
            .find(|stored| stored.object.digest.schema() == &receipt_schema)
            .unwrap();
        fs::remove_file(&manifest.path).unwrap();

        let mut cache = TileProductCache::open(store.clone(), "tile-index-gone-2").unwrap();
        assert!(cache.hydrate(spec).unwrap().is_none());
        assert!(cache
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "render-receipt-rejected"));
        assert!(cache.hydrate(spec).unwrap().is_none());
    }

    #[test]
    fn corrupt_pcm_is_diagnosed_and_never_hydrated() {
        let root = CacheRoot::new();
        let store = FsContentStore::new(&root.0);
        let target = plan(1, 7, Tileability::Stateless);
        let layout = TileLayout::new(&target, policy(target.tileability)).unwrap();
        let spec = &layout.tiles()[0];
        {
            let mut cache = TileProductCache::open(store.clone(), "tile-test-publish").unwrap();
            cache.publish(spec, tile_product(spec, 0.25)).unwrap();
        }
        let pcm_schema = canonical_render_pcm_schema().unwrap();
        let payload = store
            .inventory()
            .unwrap()
            .objects
            .into_iter()
            .find(|stored| stored.object.digest.schema() == &pcm_schema)
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = fs::metadata(&payload.path).unwrap().permissions();
            permissions.set_mode(permissions.mode() | 0o200);
            fs::set_permissions(&payload.path, permissions).unwrap();
        }
        #[cfg(not(unix))]
        {
            let mut permissions = fs::metadata(&payload.path).unwrap().permissions();
            permissions.set_readonly(false);
            fs::set_permissions(&payload.path, permissions).unwrap();
        }
        fs::write(&payload.path, b"corrupt").unwrap();

        // Opening reads receipts, not payloads, so the corruption is found on
        // the hydrate that wants the bytes. Either way it is a named miss and
        // the tile is rendered again rather than played wrong.
        let mut reopened = TileProductCache::open(store, "tile-test-corrupt").unwrap();
        assert!(reopened.hydrate(spec).unwrap().is_none());
        assert!(reopened
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "render-payload-rejected"
                || diagnostic.code == "render-receipt-rejected"));
        assert!(reopened.hydrate(spec).unwrap().is_none());
    }

    #[test]
    fn matching_request_with_stale_derivation_is_refused() {
        let root = CacheRoot::new();
        let store = FsContentStore::new(&root.0);
        let target = plan(1, 7, Tileability::Stateless);
        let target_layout = TileLayout::new(&target, policy(target.tileability)).unwrap();
        let target_spec = &target_layout.tiles()[0];
        let stale = plan(2, 8, Tileability::Stateless);
        let stale_layout = TileLayout::new(&stale, policy(stale.tileability)).unwrap();
        let stale_product = tile_product(&stale_layout.tiles()[0], 0.75);
        RenderProductCatalog::default()
            .publish(
                &store,
                stale_product,
                tile_product_request(target_spec).unwrap(),
            )
            .unwrap();

        let status = Mutex::new(RenderIndexStatus::new(RenderIndexState::Absent));
        rebuild_render_request_index(&store, &status).unwrap();

        let mut cache = TileProductCache::open(store, "tile-test-stale").unwrap();
        assert!(cache.hydrate(target_spec).unwrap().is_none());
        assert!(cache
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "stale-render-receipt"));
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
    fn bus_tile_reuse_uses_that_scopes_exact_impact_receipt() {
        let old = plan(1, 1, Tileability::Stateless);
        let mut new = plan(2, 2, Tileability::Stateless);
        new.id.revisions.arrangement = 2;
        let scope = RenderScope::Bus {
            bus: 2,
            tap: crate::render_plan::BusTap::Output,
        };
        let old_layout =
            TileLayout::new_for_scope(&old, policy(old.tileability), scope.clone()).unwrap();
        let new_layout = TileLayout::new_for_scope(&new, policy(new.tileability), scope).unwrap();
        let old_cohort = cohort(&old, &old_layout);
        let mut changes = ChangeSet::default();
        changes
            .touch(ProjectDomain::Arrangement)
            .invalidate_range(BusId::from_raw(1), AudioRange::new(0, 16).unwrap());
        let work = TileWorkPlan::derive(
            &old_cohort,
            &old,
            &new,
            &new_layout,
            None,
            &TileReuseProof::new(digest(12), changes).unwrap(),
        )
        .unwrap();
        assert_eq!(work.reuse_count(), 4);
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
    fn batch_claims_playhead_then_loop_ahead_without_duplicate_issue() {
        let target = plan(1, 1, Tileability::Stateless);
        let layout = TileLayout::new(&target, policy(target.tileability)).unwrap();
        let loop_region = Some(RenderSpan::new(4, 12).unwrap());
        let mut batch = TileRenderBatch::new(8, TileWorkPlan::cold(&layout, loop_region));

        let current = batch.take_next_job(loop_region, 9).unwrap();
        assert_eq!(current.spec.index, 2);
        assert_eq!(current.priority, TileRenderPriority::Playhead);
        let ahead = batch.take_next_job(loop_region, 9).unwrap();
        assert_eq!(ahead.spec.index, 1);
        assert!(matches!(
            ahead.priority,
            TileRenderPriority::LoopAhead { .. }
        ));
        assert_ne!(current.spec.index, ahead.spec.index);

        let status = batch.status(loop_region, 9);
        assert_eq!(status.claimed_tiles, 2);
        assert_eq!(status.remaining_tiles, 4);
        assert!(status.playhead_dirty);
        batch.release(&current).unwrap();
        assert_eq!(batch.take_next_job(loop_region, 9).unwrap().spec.index, 2);
    }

    #[test]
    fn unclaimed_work_reprioritizes_when_the_live_playhead_moves() {
        let target = plan(1, 1, Tileability::Stateless);
        let layout = TileLayout::new(&target, policy(target.tileability)).unwrap();
        let mut covering = TileRenderBatch::new(8, TileWorkPlan::cold(&layout, None));
        let current = covering.take_next_job(None, 7).unwrap();
        assert_eq!(current.spec.index, 1);
        assert_eq!(current.priority, TileRenderPriority::Playhead);

        let mut batch = TileRenderBatch::new(9, TileWorkPlan::cold(&layout, None));
        assert_eq!(batch.take_next_job(None, 0).unwrap().spec.index, 0);
        assert_eq!(batch.take_next_job(None, 15).unwrap().spec.index, 3);
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
