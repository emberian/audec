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

use serde::{Deserialize, Serialize};

use crate::content_identity::{
    ContentClass, Digest, IdentityError, ProductKey, SchemaTag, Sha256Digest,
};
use crate::content_store::{FsContentStore, ObjectRef, StoreError};
use crate::render_plan::{
    BusTap, EngineRecipeStamp, ExactDigest, ExplanationScopeId, ProjectRevisionStamp,
    RenderDependencyKey, RenderDependencyStamp, RenderFormat, RenderPlanId, RenderScope,
    RenderSpan,
};

const RENDER_PCM_SCHEMA_NAME: &str = "audec.canonical-f32le-pcm";
const RENDER_RECEIPT_SCHEMA_NAME: &str = "audec.render-product-receipt";
const RENDER_RECEIPT_FORMAT: &str = "audec-render-product-receipt";
const RENDER_RECEIPT_VERSION: u32 = 1;
const LEGACY_PCM_DIGEST_DOMAIN: &[u8] = b"audec:canonical-f32le-pcm:v1\0";
const MAX_RECEIPT_BYTES: u64 = 4 * 1024 * 1024;

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
            let expected =
                tile.intersection(plan.compiled_extent)
                    .ok_or(RenderProductError::OutsidePlan {
                        product: tile,
                        plan: plan.compiled_extent,
                    })?;
            if expected != core {
                return Err(RenderProductError::TileSpanMismatch {
                    expected,
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

    /// Publish immutable PCM and its complete derivation receipt. The returned
    /// receipt object is what a project pins in its generic content-root
    /// manifest; the payload path is never durable identity.
    pub fn publish(
        &mut self,
        store: &FsContentStore,
        product: Arc<RenderProduct>,
        request: ProductKey,
    ) -> Result<PersistedRenderProduct, RenderPersistenceError> {
        let payload_schema = canonical_render_pcm_schema()?;
        if request.output_schema() != &payload_schema {
            return Err(RenderPersistenceError::DependencyManifest(
                "product-key output schema does not name canonical render PCM".into(),
            ));
        }
        let payload_bytes = encode_pcm(product.interleaved());
        let legacy = legacy_pcm_digest(&payload_bytes);
        if legacy != product.id.pcm {
            return Err(RenderPersistenceError::LegacyPcmDigest {
                expected: product.id.pcm,
                actual: legacy,
            });
        }
        let payload = store
            .put_bytes(payload_schema, &payload_bytes)?
            .stored
            .object;
        let receipt = RenderReceiptV1 {
            format: RENDER_RECEIPT_FORMAT.into(),
            version: RENDER_RECEIPT_VERSION,
            payload: ObjectRefDto::from_object(&payload),
            request: request.encode_canonical(),
            legacy_pcm_sha256: product.id.pcm.bytes(),
            produced_by: RenderProductKeyDto::from_key(&product.produced_by),
        };
        let receipt_bytes = serde_json::to_vec(&receipt)
            .map_err(|error| RenderPersistenceError::Manifest(error.to_string()))?;
        let manifest = store
            .put_bytes(render_product_receipt_schema()?, &receipt_bytes)?
            .stored
            .object;
        let product = self.insert(product)?;
        Ok(PersistedRenderProduct {
            manifest,
            payload,
            request,
            product,
        })
    }

    /// Rehydrate a render product after restart. Both receipt and PCM are
    /// schema/digest verified by the CAS before any typed value is rebuilt.
    pub fn reopen(
        &mut self,
        store: &FsContentStore,
        manifest: &ObjectRef,
    ) -> Result<PersistedRenderProduct, RenderPersistenceError> {
        if manifest.digest.schema() != &render_product_receipt_schema()? {
            return Err(RenderPersistenceError::Manifest(
                "content root is not a render-product receipt".into(),
            ));
        }
        let receipt_bytes = store.read_verified(manifest, MAX_RECEIPT_BYTES)?;
        let receipt: RenderReceiptV1 = serde_json::from_slice(&receipt_bytes)
            .map_err(|error| RenderPersistenceError::Manifest(error.to_string()))?;
        if receipt.format != RENDER_RECEIPT_FORMAT || receipt.version != RENDER_RECEIPT_VERSION {
            return Err(RenderPersistenceError::Manifest(format!(
                "unsupported render receipt {}@{}",
                receipt.format, receipt.version
            )));
        }
        let payload = receipt.payload.object_ref()?;
        let payload_schema = canonical_render_pcm_schema()?;
        if payload.digest.schema() != &payload_schema {
            return Err(RenderPersistenceError::Manifest(
                "render receipt points to a non-PCM content class/schema".into(),
            ));
        }
        let request = ProductKey::decode_canonical(&receipt.request)?;
        if request.output_schema() != &payload_schema {
            return Err(RenderPersistenceError::DependencyManifest(
                "receipt dependency manifest names another output schema".into(),
            ));
        }
        let bytes = store.read_verified(&payload, payload.byte_len)?;
        let actual = legacy_pcm_digest(&bytes);
        let expected = ExactDigest::new(receipt.legacy_pcm_sha256);
        if actual != expected {
            return Err(RenderPersistenceError::LegacyPcmDigest { expected, actual });
        }
        let interleaved = decode_pcm(&bytes)?;
        let produced_by = receipt.produced_by.into_key()?;
        let product = Arc::new(RenderProduct::new(
            expected,
            produced_by,
            interleaved.into(),
        )?);
        let product = self.insert(product)?;
        Ok(PersistedRenderProduct {
            manifest: manifest.clone(),
            payload,
            request,
            product,
        })
    }
}

#[derive(Clone, Debug)]
pub struct PersistedRenderProduct {
    pub manifest: ObjectRef,
    pub payload: ObjectRef,
    pub request: ProductKey,
    pub product: Arc<RenderProduct>,
}

#[derive(Debug)]
pub enum RenderPersistenceError {
    Identity(IdentityError),
    Store(StoreError),
    Product(RenderProductError),
    Manifest(String),
    DependencyManifest(String),
    LegacyPcmDigest {
        expected: ExactDigest,
        actual: ExactDigest,
    },
}

impl fmt::Display for RenderPersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identity(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            Self::Product(error) => error.fmt(formatter),
            Self::Manifest(detail) => write!(formatter, "invalid render-product receipt: {detail}"),
            Self::DependencyManifest(detail) => {
                write!(formatter, "invalid render dependency manifest: {detail}")
            }
            Self::LegacyPcmDigest { expected, actual } => write!(
                formatter,
                "canonical PCM digest {actual} differs from receipt {expected}"
            ),
        }
    }
}

impl Error for RenderPersistenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Identity(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Product(error) => Some(error),
            _ => None,
        }
    }
}

impl From<IdentityError> for RenderPersistenceError {
    fn from(value: IdentityError) -> Self {
        Self::Identity(value)
    }
}
impl From<StoreError> for RenderPersistenceError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}
impl From<RenderProductError> for RenderPersistenceError {
    fn from(value: RenderProductError) -> Self {
        Self::Product(value)
    }
}

/// Canonical byte schema used by durable render-product payloads and request
/// keys. Public construction lives here so cache clients cannot accidentally
/// mint a lookalike schema with a different version.
pub fn canonical_render_pcm_schema() -> Result<SchemaTag, IdentityError> {
    SchemaTag::render_product(RENDER_PCM_SCHEMA_NAME, 1)
}

/// Durable receipt schema used to discover render products in the generic
/// content store after restart.
pub fn render_product_receipt_schema() -> Result<SchemaTag, IdentityError> {
    SchemaTag::reading_attachment(RENDER_RECEIPT_SCHEMA_NAME, 1)
}

fn encode_pcm(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len().saturating_mul(4));
    for sample in samples {
        bytes.extend_from_slice(&sample.to_bits().to_le_bytes());
    }
    bytes
}

fn decode_pcm(bytes: &[u8]) -> Result<Vec<f32>, RenderPersistenceError> {
    if bytes.len() % 4 != 0 {
        return Err(RenderPersistenceError::Manifest(
            "canonical PCM byte length is not divisible by four".into(),
        ));
    }
    let mut samples = Vec::with_capacity(bytes.len() / 4);
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let sample = f32::from_bits(u32::from_le_bytes(chunk.try_into().unwrap()));
        if !sample.is_finite() {
            return Err(RenderPersistenceError::Product(
                RenderProductError::NonFiniteSample { index },
            ));
        }
        samples.push(sample);
    }
    Ok(samples)
}

fn legacy_pcm_digest(bytes: &[u8]) -> ExactDigest {
    ExactDigest::new(Sha256Digest::hash_raw_parts(&[LEGACY_PCM_DIGEST_DOMAIN, bytes]).bytes())
}

#[derive(Serialize, Deserialize)]
struct RenderReceiptV1 {
    format: String,
    version: u32,
    payload: ObjectRefDto,
    request: Vec<u8>,
    legacy_pcm_sha256: [u8; 32],
    produced_by: RenderProductKeyDto,
}

#[derive(Serialize, Deserialize)]
struct ObjectRefDto {
    schema_class: String,
    schema_name: String,
    schema_version: u32,
    sha256: [u8; 32],
    byte_len: u64,
}

impl ObjectRefDto {
    fn from_object(object: &ObjectRef) -> Self {
        Self {
            schema_class: object.digest.schema().class().storage_label().into(),
            schema_name: object.digest.schema().name().into(),
            schema_version: object.digest.schema().version(),
            sha256: object.digest.sha256().bytes(),
            byte_len: object.byte_len,
        }
    }

    fn object_ref(self) -> Result<ObjectRef, RenderPersistenceError> {
        let class = ContentClass::from_storage_label(&self.schema_class).ok_or_else(|| {
            RenderPersistenceError::Manifest(format!(
                "unknown payload schema class {}",
                self.schema_class
            ))
        })?;
        let schema = SchemaTag::new(class, self.schema_name, self.schema_version)?;
        Ok(ObjectRef {
            digest: Digest::from_verified_parts(schema, Sha256Digest::from_bytes(self.sha256)),
            byte_len: self.byte_len,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct RenderProductKeyDto {
    plan: RenderPlanIdDto,
    scope: RenderScopeDto,
    core_start: i64,
    core_end: i64,
    partition: ProductPartitionDto,
    boundary_recipe: [u8; 32],
}

impl RenderProductKeyDto {
    fn from_key(key: &RenderProductKey) -> Self {
        Self {
            plan: RenderPlanIdDto::from_plan(&key.plan),
            scope: RenderScopeDto::from_scope(&key.scope),
            core_start: key.core.start,
            core_end: key.core.end,
            partition: ProductPartitionDto::from_partition(&key.partition),
            boundary_recipe: key.boundary_recipe.bytes(),
        }
    }

    fn into_key(self) -> Result<RenderProductKey, RenderPersistenceError> {
        RenderProductKey::new(
            self.plan.into_plan()?,
            self.scope.into_scope(),
            RenderSpan::new(self.core_start, self.core_end)
                .map_err(|error| RenderPersistenceError::Manifest(error.to_string()))?,
            self.partition.into_partition()?,
            ExactDigest::new(self.boundary_recipe),
        )
        .map_err(RenderPersistenceError::Product)
    }
}

#[derive(Serialize, Deserialize)]
struct RenderPlanIdDto {
    schema_version: u16,
    project_namespace: u128,
    snapshot: [u8; 32],
    revisions: [u64; 9],
    extent_start: i64,
    extent_end: i64,
    engine_abi: u32,
    sample_rate: u32,
    channels: u16,
    canonical_block_frames: u32,
    performance_seed: u64,
    configuration: [u8; 32],
    dependencies: Vec<RenderDependencyDto>,
}

impl RenderPlanIdDto {
    fn from_plan(plan: &RenderPlanId) -> Self {
        let revision = plan.revisions;
        Self {
            schema_version: plan.schema_version,
            project_namespace: plan.project_namespace,
            snapshot: plan.snapshot.bytes(),
            revisions: [
                revision.aggregate,
                revision.arrangement,
                revision.sequencer,
                revision.automation,
                revision.assets,
                revision.mixer,
                revision.sample_kits,
                revision.air,
                revision.bindings,
            ],
            extent_start: plan.compiled_extent.start,
            extent_end: plan.compiled_extent.end,
            engine_abi: plan.engine.engine_abi,
            sample_rate: plan.engine.format.sample_rate.get(),
            channels: plan.engine.format.channels.get(),
            canonical_block_frames: plan.engine.canonical_block_frames.get(),
            performance_seed: plan.engine.performance_seed,
            configuration: plan.engine.configuration.bytes(),
            dependencies: plan
                .dependencies()
                .iter()
                .map(RenderDependencyDto::from_dependency)
                .collect(),
        }
    }

    fn into_plan(self) -> Result<RenderPlanId, RenderPersistenceError> {
        if self.schema_version != RenderPlanId::SCHEMA_VERSION {
            return Err(RenderPersistenceError::Manifest(format!(
                "unsupported render plan schema {}",
                self.schema_version
            )));
        }
        let revisions = ProjectRevisionStamp {
            aggregate: self.revisions[0],
            arrangement: self.revisions[1],
            sequencer: self.revisions[2],
            automation: self.revisions[3],
            assets: self.revisions[4],
            mixer: self.revisions[5],
            sample_kits: self.revisions[6],
            air: self.revisions[7],
            bindings: self.revisions[8],
        };
        let format = RenderFormat::new(self.sample_rate, self.channels)
            .map_err(|error| RenderPersistenceError::Manifest(error.to_string()))?;
        let engine = EngineRecipeStamp::new(
            self.engine_abi,
            format,
            self.canonical_block_frames,
            self.performance_seed,
            ExactDigest::new(self.configuration),
        )
        .map_err(|error| RenderPersistenceError::Manifest(error.to_string()))?;
        let extent = RenderSpan::new(self.extent_start, self.extent_end)
            .map_err(|error| RenderPersistenceError::Manifest(error.to_string()))?;
        let dependencies = self
            .dependencies
            .into_iter()
            .map(RenderDependencyDto::into_dependency)
            .collect();
        RenderPlanId::new(
            self.project_namespace,
            ExactDigest::new(self.snapshot),
            revisions,
            extent,
            engine,
            dependencies,
        )
        .map_err(|error| RenderPersistenceError::Manifest(error.to_string()))
    }
}

#[derive(Serialize, Deserialize)]
struct RenderDependencyDto {
    key: RenderDependencyKeyDto,
    content: [u8; 32],
    runtime_generation: u64,
}

impl RenderDependencyDto {
    fn from_dependency(value: &RenderDependencyStamp) -> Self {
        Self {
            key: RenderDependencyKeyDto::from_key(&value.key),
            content: value.content.bytes(),
            runtime_generation: value.runtime_generation,
        }
    }
    fn into_dependency(self) -> RenderDependencyStamp {
        RenderDependencyStamp {
            key: self.key.into_key(),
            content: ExactDigest::new(self.content),
            runtime_generation: self.runtime_generation,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum RenderDependencyKeyDto {
    MediaAsset { id: u64 },
    AnalysisArtifact { namespace: u128, local: u64 },
    PluginInstance { id: u64 },
    ModelArtifact { namespace: u128, local: u64 },
    External { namespace: u128, local: u128 },
}

impl RenderDependencyKeyDto {
    fn from_key(value: &RenderDependencyKey) -> Self {
        match value {
            RenderDependencyKey::MediaAsset(id) => Self::MediaAsset { id: *id },
            RenderDependencyKey::AnalysisArtifact { namespace, local } => Self::AnalysisArtifact {
                namespace: *namespace,
                local: *local,
            },
            RenderDependencyKey::PluginInstance(id) => Self::PluginInstance { id: *id },
            RenderDependencyKey::ModelArtifact { namespace, local } => Self::ModelArtifact {
                namespace: *namespace,
                local: *local,
            },
            RenderDependencyKey::External { namespace, local } => Self::External {
                namespace: *namespace,
                local: *local,
            },
        }
    }
    fn into_key(self) -> RenderDependencyKey {
        match self {
            Self::MediaAsset { id } => RenderDependencyKey::MediaAsset(id),
            Self::AnalysisArtifact { namespace, local } => {
                RenderDependencyKey::AnalysisArtifact { namespace, local }
            }
            Self::PluginInstance { id } => RenderDependencyKey::PluginInstance(id),
            Self::ModelArtifact { namespace, local } => {
                RenderDependencyKey::ModelArtifact { namespace, local }
            }
            Self::External { namespace, local } => {
                RenderDependencyKey::External { namespace, local }
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum RenderScopeDto {
    Master,
    Bus { bus: u64, tap: BusTapDto },
    Track { track: u64 },
    Explanation { namespace: u128, local: u64 },
}

impl RenderScopeDto {
    fn from_scope(value: &RenderScope) -> Self {
        match value {
            RenderScope::Master => Self::Master,
            RenderScope::Bus { bus, tap } => Self::Bus {
                bus: *bus,
                tap: BusTapDto::from_tap(*tap),
            },
            RenderScope::Track(track) => Self::Track { track: *track },
            RenderScope::Explanation(id) => Self::Explanation {
                namespace: id.namespace,
                local: id.local,
            },
        }
    }
    fn into_scope(self) -> RenderScope {
        match self {
            Self::Master => RenderScope::Master,
            Self::Bus { bus, tap } => RenderScope::Bus {
                bus,
                tap: tap.into_tap(),
            },
            Self::Track { track } => RenderScope::Track(track),
            Self::Explanation { namespace, local } => {
                RenderScope::Explanation(ExplanationScopeId { namespace, local })
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum BusTapDto {
    PreFader,
    PostFader,
    Output,
}
impl BusTapDto {
    fn from_tap(value: BusTap) -> Self {
        match value {
            BusTap::PreFader => Self::PreFader,
            BusTap::PostFader => Self::PostFader,
            BusTap::Output => Self::Output,
        }
    }
    fn into_tap(self) -> BusTap {
        match self {
            Self::PreFader => BusTap::PreFader,
            Self::PostFader => BusTap::PostFader,
            Self::Output => BusTap::Output,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum ProductPartitionDto {
    WholeBounce,
    Tile { tile_frames: u32, index: i64 },
    ContiguousRun { anchor_frame: i64, sequence: u32 },
}

impl ProductPartitionDto {
    fn from_partition(value: &ProductPartition) -> Self {
        match value {
            ProductPartition::WholeBounce => Self::WholeBounce,
            ProductPartition::Tile { grid, index } => Self::Tile {
                tile_frames: grid.tile_frames(),
                index: *index,
            },
            ProductPartition::ContiguousRun {
                anchor_frame,
                sequence,
            } => Self::ContiguousRun {
                anchor_frame: *anchor_frame,
                sequence: *sequence,
            },
        }
    }
    fn into_partition(self) -> Result<ProductPartition, RenderPersistenceError> {
        Ok(match self {
            Self::WholeBounce => ProductPartition::WholeBounce,
            Self::Tile { tile_frames, index } => ProductPartition::Tile {
                grid: TileGrid::new(tile_frames).map_err(RenderPersistenceError::Product)?,
                index,
            },
            Self::ContiguousRun {
                anchor_frame,
                sequence,
            } => ProductPartition::ContiguousRun {
                anchor_frame,
                sequence,
            },
        })
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
    use crate::content_identity::Digest;
    use crate::render_plan::{
        DeterminismGrade, EngineRecipeStamp, ProjectRevisionStamp, RenderPlan, Tileability,
    };
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);
    struct TestRoot(PathBuf);
    impl TestRoot {
        fn new() -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "audec-render-products-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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
    fn edge_tile_core_is_clipped_to_the_compiled_plan() {
        let plan = plan(1);
        let grid = TileGrid::new(16).unwrap();
        let key = RenderProductKey::new(
            plan.id.clone(),
            RenderScope::Master,
            RenderSpan::new(48, 64).unwrap(),
            ProductPartition::Tile { grid, index: 3 },
            digest(8),
        )
        .unwrap();
        assert_eq!(key.core, RenderSpan::new(48, 64).unwrap());

        let edge_id = RenderPlanId::new(
            plan.id.project_namespace,
            plan.id.snapshot,
            plan.id.revisions,
            RenderSpan::new(-9, 61).unwrap(),
            plan.id.engine.clone(),
            plan.id.dependencies().to_vec(),
        )
        .unwrap();
        let edge_plan = RenderPlan::new(edge_id, plan.determinism, plan.tileability);
        let clipped = RenderProductKey::new(
            edge_plan.id,
            RenderScope::Master,
            RenderSpan::new(48, 61).unwrap(),
            ProductPartition::Tile { grid, index: 3 },
            digest(8),
        )
        .unwrap();
        assert_eq!(clipped.core, RenderSpan::new(48, 61).unwrap());
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

    #[test]
    fn render_product_reopens_with_exact_plan_and_pcm_identity() {
        let root = TestRoot::new();
        let store = FsContentStore::new(&root.0);
        let plan = plan(7);
        let span = RenderSpan::new(0, 16).unwrap();
        let samples = (0..32)
            .map(|index| index as f32 / 31.0 - 0.5)
            .collect::<Vec<_>>();
        let pcm_digest = legacy_pcm_digest(&encode_pcm(&samples));
        let key = RenderProductKey::new(
            plan.id.clone(),
            RenderScope::Master,
            span,
            ProductPartition::WholeBounce,
            digest(4),
        )
        .unwrap();
        let product = Arc::new(RenderProduct::new(pcm_digest, key, samples.into()).unwrap());
        let request = ProductKey::builder(
            canonical_render_pcm_schema().unwrap(),
            Digest::of_bytes(
                SchemaTag::recipe("test/render-engine", 1).unwrap(),
                b"engine",
            ),
        )
        .unwrap()
        .build();
        let mut first_catalog = RenderProductCatalog::default();
        let persisted = first_catalog
            .publish(&store, product, request.clone())
            .unwrap();

        let reopened_store = FsContentStore::new(&root.0);
        let mut reopened_catalog = RenderProductCatalog::default();
        let reopened = reopened_catalog
            .reopen(&reopened_store, &persisted.manifest)
            .unwrap();
        assert_eq!(reopened.product.id.pcm, pcm_digest);
        assert_eq!(reopened.product.produced_by.plan, plan.id);
        assert_eq!(reopened.request, request);
        assert_eq!(reopened_catalog.len(), 1);

        let payload_path = reopened_store.verify(&persisted.payload).unwrap().path;
        let mut permissions = fs::metadata(&payload_path).unwrap().permissions();
        #[cfg(unix)]
        permissions.set_mode(permissions.mode() | 0o200);
        #[cfg(not(unix))]
        permissions.set_readonly(false);
        fs::set_permissions(&payload_path, permissions).unwrap();
        fs::write(payload_path, b"corrupt").unwrap();
        assert!(reopened_catalog
            .reopen(&reopened_store, &persisted.manifest)
            .is_err());
    }
}
