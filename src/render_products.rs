//! Immutable rendered PCM products and coherent playback cohorts.
//!
//! Products are content-addressed audio buffers. Cohorts are publication
//! manifests: they map every required timeline slot for one target plan to a
//! pinned product. A cohort may reuse an older plan's product only with an
//! explicit proof receipt. The realtime adapter consumes a ready cohort; it
//! never discovers, schedules, hashes, or validates products itself.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::render_plan::{ExactDigest, RenderFormat, RenderPlanId, RenderScope, RenderSpan};

/// A power-of-two grid on the signed project timeline.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TileGrid {
    tile_frames: u32,
}

impl TileGrid {
    pub fn new(tile_frames: u32) -> Result<Self, RenderProductError> {
        if !tile_frames.is_power_of_two() {
            return Err(RenderProductError::InvalidTileFrames(tile_frames));
        }
        Ok(Self { tile_frames })
    }

    pub const fn tile_frames(self) -> u32 {
        self.tile_frames
    }

    pub fn index_for(self, frame: i64) -> i64 {
        frame.div_euclid(i64::from(self.tile_frames))
    }

    pub fn span(self, index: i64) -> Result<RenderSpan, RenderProductError> {
        let frames = i128::from(self.tile_frames);
        let start = i128::from(index) * frames;
        let end = start + frames;
        if start < i128::from(i64::MIN) || end > i128::from(i64::MAX) {
            return Err(RenderProductError::TileRangeOverflow { index });
        }
        RenderSpan::new(start as i64, end as i64)
            .map_err(|_| RenderProductError::TileRangeOverflow { index })
    }
}

/// The execution partition that produced a product. Whole-bounce is a normal,
/// first-class policy; tiles are a later optimization behind the same key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProductPartition {
    WholeBounce,
    Tile { grid: TileGrid, index: i64 },
    ContiguousRun { anchor_frame: i64, sequence: u32 },
}

/// Derivation identity: which immutable plan was asked for which output span.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RenderProductKey {
    pub plan: RenderPlanId,
    pub scope: RenderScope,
    pub core: RenderSpan,
    pub partition: ProductPartition,
    /// State/checkpoint/warmup recipe used at this product boundary.
    pub boundary_recipe: ExactDigest,
}

impl RenderProductKey {
    pub fn new(
        plan: RenderPlanId,
        scope: RenderScope,
        core: RenderSpan,
        partition: ProductPartition,
        boundary_recipe: ExactDigest,
    ) -> Result<Self, RenderProductError> {
        if !plan.compiled_extent.contains_span(core) {
            return Err(RenderProductError::OutsidePlan {
                product: core,
                plan: plan.compiled_extent,
            });
        }
        if let ProductPartition::Tile { grid, index } = partition {
            let tile = grid.span(index)?;
            if tile != core {
                return Err(RenderProductError::TileSpanMismatch {
                    expected: tile,
                    actual: core,
                });
            }
        }
        Ok(Self {
            plan,
            scope,
            core,
            partition,
            boundary_recipe,
        })
    }
}

/// Content identity of canonical, finite, interleaved `f32` PCM.
///
/// `pcm` is a digest over explicit little-endian sample bits after non-finite
/// quarantine. Timeline position and producing plan are intentionally absent:
/// identical audio can be reused at the same requested slot by another plan.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RenderProductId {
    pub pcm: ExactDigest,
    pub format: RenderFormat,
    pub frames: u64,
}

/// One immutable resident render product.
#[derive(Clone, Debug)]
pub struct RenderProduct {
    pub id: RenderProductId,
    pub produced_by: RenderProductKey,
    interleaved: Arc<[f32]>,
}

impl RenderProduct {
    pub fn new(
        pcm_digest: ExactDigest,
        produced_by: RenderProductKey,
        interleaved: Arc<[f32]>,
    ) -> Result<Self, RenderProductError> {
        let format = produced_by.plan.engine.format;
        let channels = usize::from(format.channels.get());
        let expected_samples = usize::try_from(produced_by.core.len())
            .ok()
            .and_then(|frames| frames.checked_mul(channels))
            .ok_or(RenderProductError::ProductTooLarge)?;
        if interleaved.len() != expected_samples {
            return Err(RenderProductError::SampleCount {
                expected: expected_samples,
                actual: interleaved.len(),
            });
        }
        if let Some(index) = interleaved.iter().position(|sample| !sample.is_finite()) {
            return Err(RenderProductError::NonFiniteSample { index });
        }
        Ok(Self {
            id: RenderProductId {
                pcm: pcm_digest,
                format,
                frames: produced_by.core.len(),
            },
            produced_by,
            interleaved,
        })
    }

    pub fn interleaved(&self) -> &[f32] {
        &self.interleaved
    }

    pub fn shared_interleaved(&self) -> Arc<[f32]> {
        Arc::clone(&self.interleaved)
    }

    fn payload_bits_equal(&self, other: &Self) -> bool {
        self.id == other.id
            && self.interleaved.len() == other.interleaved.len()
            && self
                .interleaved
                .iter()
                .zip(other.interleaved.iter())
                .all(|(left, right)| left.to_bits() == right.to_bits())
    }
}

/// Minimal content store. Budgeting and LRU policy belong to the later cache
/// implementation; this catalog already rejects an ID collision whose payload
/// bits disagree.
#[derive(Clone, Debug, Default)]
pub struct RenderProductCatalog {
    products: BTreeMap<RenderProductId, Arc<RenderProduct>>,
}

impl RenderProductCatalog {
    pub fn get(&self, id: &RenderProductId) -> Option<Arc<RenderProduct>> {
        self.products.get(id).cloned()
    }

    pub fn insert(
        &mut self,
        product: Arc<RenderProduct>,
    ) -> Result<Arc<RenderProduct>, RenderProductError> {
        if let Some(existing) = self.products.get(&product.id) {
            if !existing.payload_bits_equal(&product) {
                return Err(RenderProductError::DigestCollision(product.id));
            }
            if existing.produced_by == product.produced_by {
                return Ok(Arc::clone(existing));
            }
            // Product identity deliberately addresses PCM rather than its
            // derivation. Share the canonical allocation without replacing
            // the caller's exact plan/boundary provenance with whichever
            // derivation happened to enter the catalog first.
            return Ok(Arc::new(RenderProduct {
                id: product.id,
                produced_by: product.produced_by.clone(),
                interleaved: existing.shared_interleaved(),
            }));
        }
        self.products.insert(product.id, Arc::clone(&product));
        Ok(product)
    }

    pub fn len(&self) -> usize {
        self.products.len()
    }

    pub fn is_empty(&self) -> bool {
        self.products.is_empty()
    }
}

/// One exact semantic timeline slot required by a playback publication.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RenderSlot {
    pub scope: RenderScope,
    pub span: RenderSpan,
}

/// Why a product created under another plan is valid in the target cohort.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CohortProductProvenance {
    RenderedForTarget,
    Reused {
        from_plan: RenderPlanId,
        /// Digest of the invalidation/reuse proof, not merely a UI label.
        proof: ExactDigest,
    },
}

/// A pinned mapping from a target-plan slot to resident PCM.
#[derive(Clone, Debug)]
pub struct CohortProduct {
    pub slot: RenderSlot,
    pub product: Arc<RenderProduct>,
    pub provenance: CohortProductProvenance,
}

impl CohortProduct {
    fn validate_for(&self, target: &RenderPlanId) -> Result<(), RenderProductError> {
        if self.product.produced_by.scope != self.slot.scope
            || self.product.produced_by.core != self.slot.span
        {
            return Err(RenderProductError::ProductSlotMismatch);
        }
        match &self.provenance {
            CohortProductProvenance::RenderedForTarget
                if self.product.produced_by.plan != *target =>
            {
                Err(RenderProductError::WrongProductPlan)
            }
            CohortProductProvenance::Reused { from_plan, proof }
                if self.product.produced_by.plan != *from_plan || proof.is_zero() =>
            {
                Err(RenderProductError::InvalidReuseProof)
            }
            _ => Ok(()),
        }
    }
}

/// Session-local publication identity. The plan component prevents sequence
/// reuse from confusing two target revisions.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlaybackCohortId {
    pub plan: RenderPlanId,
    pub sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CohortReadiness {
    Priming { missing: Arc<[RenderSlot]> },
    Ready,
}

/// A coherent manifest for one future table swap.
///
/// Entries pin product Arcs, so an LRU cannot evict active or staged audio.
/// A ready cohort has every required slot and can be translated mechanically
/// into the realtime renderer's table.
#[derive(Clone, Debug)]
pub struct PlaybackCohort {
    pub id: PlaybackCohortId,
    /// If present, publication waits for this exact loop's wrap.
    pub publication_loop: Option<RenderSpan>,
    required: Arc<[RenderSlot]>,
    entries: BTreeMap<RenderSlot, CohortProduct>,
    readiness: CohortReadiness,
}

impl PlaybackCohort {
    pub fn new(
        id: PlaybackCohortId,
        publication_loop: Option<RenderSpan>,
        mut required: Vec<RenderSlot>,
        products: Vec<CohortProduct>,
    ) -> Result<Self, RenderProductError> {
        required.sort();
        required.dedup();
        if required.is_empty() {
            return Err(RenderProductError::EmptyCohort);
        }
        for adjacent in required.windows(2) {
            let [left, right] = adjacent else {
                unreachable!("windows(2) always yields two entries")
            };
            if left.scope == right.scope && left.span.intersects(right.span) {
                return Err(RenderProductError::OverlappingCohortSlots {
                    left: left.clone(),
                    right: right.clone(),
                });
            }
        }
        let required_set: BTreeSet<_> = required.iter().cloned().collect();
        let mut entries = BTreeMap::new();
        for product in products {
            if !required_set.contains(&product.slot) {
                return Err(RenderProductError::UnexpectedCohortSlot(product.slot));
            }
            product.validate_for(&id.plan)?;
            let slot = product.slot.clone();
            if entries.insert(slot.clone(), product).is_some() {
                return Err(RenderProductError::DuplicateCohortSlot(slot));
            }
        }
        let missing: Arc<[RenderSlot]> = required
            .iter()
            .filter(|slot| !entries.contains_key(*slot))
            .cloned()
            .collect::<Vec<_>>()
            .into();
        let readiness = if missing.is_empty() {
            CohortReadiness::Ready
        } else {
            CohortReadiness::Priming { missing }
        };
        Ok(Self {
            id,
            publication_loop,
            required: required.into(),
            entries,
            readiness,
        })
    }

    pub fn required(&self) -> &[RenderSlot] {
        &self.required
    }

    pub fn products(&self) -> impl Iterator<Item = &CohortProduct> {
        self.entries.values()
    }

    pub fn readiness(&self) -> &CohortReadiness {
        &self.readiness
    }

    pub fn is_ready(&self) -> bool {
        self.readiness == CohortReadiness::Ready
    }

    pub fn covers(&self, scope: &RenderScope, span: RenderSpan) -> bool {
        let mut cursor = span.start;
        for entry in self
            .entries
            .values()
            .filter(|entry| &entry.slot.scope == scope && entry.slot.span.intersects(span))
        {
            let overlap = entry
                .slot
                .span
                .intersection(span)
                .expect("filtered overlap");
            if overlap.start > cursor {
                return false;
            }
            cursor = cursor.max(overlap.end);
            if cursor >= span.end {
                return true;
            }
        }
        false
    }

    pub fn product_ids_covering(
        &self,
        scope: &RenderScope,
        span: RenderSpan,
    ) -> Option<Vec<RenderProductId>> {
        if !self.covers(scope, span) {
            return None;
        }
        let mut ids = Vec::new();
        for entry in self
            .entries
            .values()
            .filter(|entry| &entry.slot.scope == scope && entry.slot.span.intersects(span))
        {
            if ids.last() != Some(&entry.product.id) {
                ids.push(entry.product.id);
            }
        }
        Some(ids)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderProductError {
    InvalidTileFrames(u32),
    TileRangeOverflow {
        index: i64,
    },
    TileSpanMismatch {
        expected: RenderSpan,
        actual: RenderSpan,
    },
    OutsidePlan {
        product: RenderSpan,
        plan: RenderSpan,
    },
    ProductTooLarge,
    SampleCount {
        expected: usize,
        actual: usize,
    },
    NonFiniteSample {
        index: usize,
    },
    DigestCollision(RenderProductId),
    ProductSlotMismatch,
    WrongProductPlan,
    InvalidReuseProof,
    EmptyCohort,
    OverlappingCohortSlots {
        left: RenderSlot,
        right: RenderSlot,
    },
    UnexpectedCohortSlot(RenderSlot),
    DuplicateCohortSlot(RenderSlot),
}

impl fmt::Display for RenderProductError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTileFrames(frames) => {
                write!(
                    formatter,
                    "tile size must be a nonzero power of two, got {frames}"
                )
            }
            Self::TileRangeOverflow { index } => {
                write!(
                    formatter,
                    "tile {index} overflows the signed project timeline"
                )
            }
            Self::TileSpanMismatch { expected, actual } => write!(
                formatter,
                "tile span {}..{} does not match {}..{}",
                actual.start, actual.end, expected.start, expected.end
            ),
            Self::OutsidePlan { product, plan } => write!(
                formatter,
                "product {}..{} lies outside plan {}..{}",
                product.start, product.end, plan.start, plan.end
            ),
            Self::ProductTooLarge => write!(formatter, "render product is too large"),
            Self::SampleCount { expected, actual } => {
                write!(
                    formatter,
                    "render product has {actual} samples, expected {expected}"
                )
            }
            Self::NonFiniteSample { index } => {
                write!(
                    formatter,
                    "render product contains a non-finite sample at {index}"
                )
            }
            Self::DigestCollision(id) => {
                write!(formatter, "render product digest collision for {id:?}")
            }
            Self::ProductSlotMismatch => {
                write!(
                    formatter,
                    "cohort slot differs from the product's scope or span"
                )
            }
            Self::WrongProductPlan => {
                write!(
                    formatter,
                    "cohort product was not rendered for its target plan"
                )
            }
            Self::InvalidReuseProof => write!(formatter, "cohort reuse proof is invalid"),
            Self::EmptyCohort => write!(formatter, "playback cohort requires at least one slot"),
            Self::OverlappingCohortSlots { left, right } => write!(
                formatter,
                "playback cohort has ambiguous overlapping slots {left:?} and {right:?}"
            ),
            Self::UnexpectedCohortSlot(slot) => {
                write!(formatter, "cohort product for unrequested slot {slot:?}")
            }
            Self::DuplicateCohortSlot(slot) => {
                write!(formatter, "cohort contains duplicate slot {slot:?}")
            }
        }
    }
}

impl Error for RenderProductError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_plan::{
        DeterminismGrade, EngineRecipeStamp, ProjectRevisionStamp, RenderPlan, Tileability,
    };

    fn digest(byte: u8) -> ExactDigest {
        ExactDigest::new([byte; 32])
    }

    fn plan(revision: u64) -> RenderPlan {
        let format = RenderFormat::new(48_000, 2).unwrap();
        let engine = EngineRecipeStamp::new(1, format, 512, 0, digest(3)).unwrap();
        let id = RenderPlanId::new(
            1,
            digest(revision as u8),
            ProjectRevisionStamp {
                aggregate: revision,
                ..ProjectRevisionStamp::default()
            },
            RenderSpan::new(-16, 64).unwrap(),
            engine,
            Vec::new(),
        )
        .unwrap();
        RenderPlan::new(id, DeterminismGrade::BitExact, Tileability::Stateless)
    }

    fn product(plan: &RenderPlan, span: RenderSpan, byte: u8) -> Arc<RenderProduct> {
        let key = RenderProductKey::new(
            plan.id.clone(),
            RenderScope::Master,
            span,
            ProductPartition::WholeBounce,
            digest(4),
        )
        .unwrap();
        Arc::new(
            RenderProduct::new(
                digest(byte),
                key,
                vec![f32::from(byte); span.len() as usize * 2].into(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn signed_tile_grid_uses_euclidean_ranges() {
        let grid = TileGrid::new(16).unwrap();
        assert_eq!(grid.index_for(-1), -1);
        assert_eq!(grid.span(-1).unwrap(), RenderSpan::new(-16, 0).unwrap());
        assert_eq!(grid.span(0).unwrap(), RenderSpan::new(0, 16).unwrap());
    }

    #[test]
    fn whole_bounce_is_a_ready_single_product_cohort() {
        let plan = plan(1);
        let span = RenderSpan::new(0, 64).unwrap();
        let slot = RenderSlot {
            scope: RenderScope::Master,
            span,
        };
        let cohort = PlaybackCohort::new(
            PlaybackCohortId {
                plan: plan.id.clone(),
                sequence: 1,
            },
            Some(RenderSpan::new(0, 32).unwrap()),
            vec![slot.clone()],
            vec![CohortProduct {
                slot,
                product: product(&plan, span, 1),
                provenance: CohortProductProvenance::RenderedForTarget,
            }],
        )
        .unwrap();
        assert!(cohort.is_ready());
        assert!(cohort.covers(&RenderScope::Master, RenderSpan::new(8, 24).unwrap()));
    }

    #[test]
    fn missing_product_keeps_a_cohort_in_priming_state() {
        let plan = plan(1);
        let cohort = PlaybackCohort::new(
            PlaybackCohortId {
                plan: plan.id,
                sequence: 2,
            },
            None,
            vec![RenderSlot {
                scope: RenderScope::Master,
                span: RenderSpan::new(0, 16).unwrap(),
            }],
            Vec::new(),
        )
        .unwrap();
        assert!(matches!(
            cohort.readiness(),
            CohortReadiness::Priming { missing } if missing.len() == 1
        ));
    }

    #[test]
    fn cross_plan_reuse_requires_an_explicit_nonzero_proof() {
        let old = plan(1);
        let new = plan(2);
        let span = RenderSpan::new(0, 16).unwrap();
        let slot = RenderSlot {
            scope: RenderScope::Master,
            span,
        };
        let reused = product(&old, span, 9);
        let invalid = PlaybackCohort::new(
            PlaybackCohortId {
                plan: new.id,
                sequence: 3,
            },
            None,
            vec![slot.clone()],
            vec![CohortProduct {
                slot,
                product: reused,
                provenance: CohortProductProvenance::Reused {
                    from_plan: old.id,
                    proof: ExactDigest::ZERO,
                },
            }],
        );
        assert!(matches!(
            invalid,
            Err(RenderProductError::InvalidReuseProof)
        ));
    }

    #[test]
    fn catalog_deduplicates_pcm_without_erasing_derivation() {
        let old = plan(1);
        let new = plan(2);
        let span = RenderSpan::new(0, 16).unwrap();
        let old_product = product(&old, span, 9);
        let new_product = product(&new, span, 9);
        let mut catalog = RenderProductCatalog::default();
        let old_product = catalog.insert(old_product).unwrap();
        let new_product = catalog.insert(new_product).unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(old_product.id, new_product.id);
        assert_eq!(new_product.produced_by.plan, new.id);
        assert!(Arc::ptr_eq(
            &old_product.shared_interleaved(),
            &new_product.shared_interleaved()
        ));
    }
}
