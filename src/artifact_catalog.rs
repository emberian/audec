//! Immutable, content-addressed analysis artifact catalog.
//!
//! Artifacts are computed evidence products, not project truth. The catalog
//! stores immutable payloads behind a descriptor whose output digest is the
//! identity. It never promotes a recurrence family, mask, model output, or
//! reconstruction proposal into an asserted source identity.

#[path = "artifact_comparison_hydration.rs"]
pub mod comparison_hydration;

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::aspect::FrameSpan;
use crate::content_identity::{
    ContentClass, Digest, IdentityError, ProductKey, SchemaTag, Sha256Digest,
};
use crate::content_store::{FsContentStore, ObjectRef, StoreError};
use crate::ontology::Provenance;

const ARTIFACT_PAYLOAD_SCHEMA_NAME: &str = "audec.analysis-artifact-payload";
const ARTIFACT_RECEIPT_SCHEMA_NAME: &str = "audec.analysis-artifact-receipt";
const ARTIFACT_RECEIPT_FORMAT: &str = "audec-analysis-artifact-receipt";
const ARTIFACT_RECEIPT_VERSION: u32 = 1;
const MAX_ARTIFACT_RECEIPT_BYTES: u64 = 4 * 1024 * 1024;
const ARTIFACT_PAYLOAD_DIGEST_DOMAIN: &[u8] = b"audec:canonical-analysis-artifact:v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DigestAlgorithm {
    Sha256,
    Blake3,
    /// Stable cache/dedup hint only; never sufficient for authenticity or a
    /// reading's refusal-grade source match.
    StableNonCryptographic,
}

/// Algorithm-tagged 256-bit digest supplied by the producing boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentDigest {
    pub algorithm: DigestAlgorithm,
    pub bytes: [u8; 32],
}

impl ContentDigest {
    pub const fn new(algorithm: DigestAlgorithm, bytes: [u8; 32]) -> Self {
        Self { algorithm, bytes }
    }

    pub const fn is_strong(self) -> bool {
        matches!(
            self.algorithm,
            DigestAlgorithm::Sha256 | DigestAlgorithm::Blake3
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactId(pub ContentDigest);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArtifactKind {
    LoomSketch,
    Hpss,
    ReconstructionSet,
    ModelClaim,
    SpectralField,
    CoverageField,
    Other(String),
}

/// Collision-resistant digest over a domain tag and length-delimited byte
/// parts. Length prefixes make `["ab", "c"]` distinct from `["a", "bc"]`.
/// This is the common content-addressing primitive for non-PCM artifacts;
/// render products retain their own canonical PCM domain recipe.
pub fn sha256_content(domain: &[u8], parts: &[&[u8]]) -> ContentDigest {
    let mut canonical = Vec::new();
    canonical.extend_from_slice(b"audec:content-address:v1\0");
    canonical.extend_from_slice(&(domain.len() as u64).to_le_bytes());
    canonical.extend_from_slice(domain);
    for part in parts {
        canonical.extend_from_slice(&(part.len() as u64).to_le_bytes());
        canonical.extend_from_slice(part);
    }
    ContentDigest::new(
        DigestAlgorithm::Sha256,
        Sha256Digest::hash_raw(&canonical).bytes(),
    )
}

pub fn canonical_artifact_payload_digest(payload: &[u8]) -> ContentDigest {
    sha256_content(ARTIFACT_PAYLOAD_DIGEST_DOMAIN, &[payload])
}

/// Immutable facts needed to reproduce and resolve one artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactDescriptor {
    pub id: ArtifactId,
    pub kind: ArtifactKind,
    /// Digest of immutable source material, not a path or project-local ID.
    pub source_digest: ContentDigest,
    /// Digest of analyzer identity, version, and normalized parameters.
    pub recipe_digest: ContentDigest,
    /// Digest of the canonical artifact payload. Equal to `id.0`.
    pub output_digest: ContentDigest,
    /// Extent is in canonical project frames for the producing session.
    pub extent: FrameSpan,
    pub sample_rate: u32,
    pub channels: u16,
    pub provenance: Provenance,
}

impl ArtifactDescriptor {
    pub fn validate(&self) -> Result<(), ArtifactCatalogError> {
        if self.id.0 != self.output_digest {
            return Err(ArtifactCatalogError::IdentityMismatch {
                id: self.id,
                output: self.output_digest,
            });
        }
        if self.extent.start >= self.extent.end {
            return Err(ArtifactCatalogError::InvalidExtent(self.extent));
        }
        if self.sample_rate == 0 || self.channels == 0 {
            return Err(ArtifactCatalogError::InvalidFormat {
                sample_rate: self.sample_rate,
                channels: self.channels,
            });
        }
        Ok(())
    }
}

struct ArtifactEntry {
    descriptor: ArtifactDescriptor,
    payload: Arc<dyn Any + Send + Sync>,
}

/// Runtime payload cache. Durable project/reading forms store descriptors and
/// artifact references; they do not serialize this type-erased map.
#[derive(Default)]
pub struct ArtifactCatalog {
    entries: BTreeMap<ArtifactId, ArtifactEntry>,
}

impl fmt::Debug for ArtifactCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactCatalog")
            .field("descriptors", &self.descriptors().collect::<Vec<_>>())
            .finish()
    }
}

impl ArtifactCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &ArtifactDescriptor> {
        self.entries.values().map(|entry| &entry.descriptor)
    }

    pub fn descriptor(&self, id: ArtifactId) -> Option<&ArtifactDescriptor> {
        self.entries.get(&id).map(|entry| &entry.descriptor)
    }

    /// Idempotent insertion is allowed only when the complete descriptor is
    /// identical. A same-digest/different-description collision is explicit.
    pub fn insert<T>(
        &mut self,
        descriptor: ArtifactDescriptor,
        payload: Arc<T>,
    ) -> Result<(), ArtifactCatalogError>
    where
        T: Any + Send + Sync,
    {
        descriptor.validate()?;
        if let Some(existing) = self.entries.get(&descriptor.id) {
            return if existing.descriptor == descriptor {
                Ok(())
            } else {
                Err(ArtifactCatalogError::DescriptorConflict(descriptor.id))
            };
        }
        self.entries.insert(
            descriptor.id,
            ArtifactEntry {
                descriptor,
                payload,
            },
        );
        Ok(())
    }

    pub fn get<T>(&self, id: ArtifactId) -> Result<Arc<T>, ArtifactCatalogError>
    where
        T: Any + Send + Sync,
    {
        let entry = self
            .entries
            .get(&id)
            .ok_or(ArtifactCatalogError::Missing(id))?;
        Arc::clone(&entry.payload)
            .downcast::<T>()
            .map_err(|_| ArtifactCatalogError::PayloadType { id })
    }

    /// Persist canonical artifact bytes and a full typed derivation receipt.
    /// Type-erased runtime objects remain deliberately non-serializable; pane
    /// adapters choose and version their canonical byte representation here.
    pub fn publish_bytes(
        &mut self,
        store: &FsContentStore,
        descriptor: ArtifactDescriptor,
        payload: Arc<Vec<u8>>,
        request: ProductKey,
    ) -> Result<PersistedArtifact, ArtifactPersistenceError> {
        descriptor.validate()?;
        require_strong_descriptor(&descriptor)?;
        let actual_output = canonical_artifact_payload_digest(payload.as_slice());
        if descriptor.output_digest != actual_output {
            return Err(ArtifactPersistenceError::PayloadDigest {
                expected: descriptor.output_digest,
                actual: actual_output,
            });
        }
        let payload_schema = artifact_payload_schema()?;
        if request.output_schema() != &payload_schema {
            return Err(ArtifactPersistenceError::DependencyManifest(
                "product-key output schema does not name canonical analysis bytes".into(),
            ));
        }
        let payload_object = store
            .put_bytes(payload_schema, payload.as_slice())?
            .stored
            .object;
        let receipt = ArtifactReceiptV1 {
            format: ARTIFACT_RECEIPT_FORMAT.into(),
            version: ARTIFACT_RECEIPT_VERSION,
            payload: ArtifactObjectRefDto::from_object(&payload_object),
            request: request.encode_canonical(),
            descriptor: ArtifactDescriptorDto::from_descriptor(&descriptor),
        };
        let receipt_bytes = serde_json::to_vec(&receipt)
            .map_err(|error| ArtifactPersistenceError::Manifest(error.to_string()))?;
        let manifest = store
            .put_bytes(artifact_receipt_schema()?, &receipt_bytes)?
            .stored
            .object;
        self.insert(descriptor.clone(), Arc::clone(&payload))?;
        Ok(PersistedArtifact {
            manifest,
            payload: payload_object,
            request,
            descriptor,
            bytes: payload,
        })
    }

    /// Reopen a byte artifact from a project content root. CAS verification
    /// precedes receipt decoding and catalog publication.
    pub fn reopen_bytes(
        &mut self,
        store: &FsContentStore,
        manifest: &ObjectRef,
    ) -> Result<PersistedArtifact, ArtifactPersistenceError> {
        if manifest.digest.schema() != &artifact_receipt_schema()? {
            return Err(ArtifactPersistenceError::Manifest(
                "content root is not an analysis-artifact receipt".into(),
            ));
        }
        let receipt_bytes = store.read_verified(manifest, MAX_ARTIFACT_RECEIPT_BYTES)?;
        let receipt: ArtifactReceiptV1 = serde_json::from_slice(&receipt_bytes)
            .map_err(|error| ArtifactPersistenceError::Manifest(error.to_string()))?;
        if receipt.format != ARTIFACT_RECEIPT_FORMAT || receipt.version != ARTIFACT_RECEIPT_VERSION
        {
            return Err(ArtifactPersistenceError::Manifest(format!(
                "unsupported analysis receipt {}@{}",
                receipt.format, receipt.version
            )));
        }
        let payload_object = receipt.payload.object_ref()?;
        let payload_schema = artifact_payload_schema()?;
        if payload_object.digest.schema() != &payload_schema {
            return Err(ArtifactPersistenceError::Manifest(
                "analysis receipt points to another content class/schema".into(),
            ));
        }
        let request = ProductKey::decode_canonical(&receipt.request)?;
        if request.output_schema() != &payload_schema {
            return Err(ArtifactPersistenceError::DependencyManifest(
                "receipt dependency manifest names another output schema".into(),
            ));
        }
        let descriptor = receipt.descriptor.into_descriptor()?;
        descriptor.validate()?;
        require_strong_descriptor(&descriptor)?;
        let bytes = Arc::new(store.read_verified(&payload_object, payload_object.byte_len)?);
        let actual_output = canonical_artifact_payload_digest(bytes.as_slice());
        if descriptor.output_digest != actual_output {
            return Err(ArtifactPersistenceError::PayloadDigest {
                expected: descriptor.output_digest,
                actual: actual_output,
            });
        }
        self.insert(descriptor.clone(), Arc::clone(&bytes))?;
        Ok(PersistedArtifact {
            manifest: manifest.clone(),
            payload: payload_object,
            request,
            descriptor,
            bytes,
        })
    }
}

#[derive(Clone, Debug)]
pub struct PersistedArtifact {
    pub manifest: ObjectRef,
    pub payload: ObjectRef,
    pub request: ProductKey,
    pub descriptor: ArtifactDescriptor,
    pub bytes: Arc<Vec<u8>>,
}

#[derive(Debug)]
pub enum ArtifactPersistenceError {
    Catalog(ArtifactCatalogError),
    Identity(IdentityError),
    Store(StoreError),
    Manifest(String),
    DependencyManifest(String),
    WeakDigest {
        field: &'static str,
    },
    PayloadDigest {
        expected: ContentDigest,
        actual: ContentDigest,
    },
}

impl fmt::Display for ArtifactPersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(error) => error.fmt(formatter),
            Self::Identity(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            Self::Manifest(detail) => {
                write!(formatter, "invalid analysis-artifact receipt: {detail}")
            }
            Self::DependencyManifest(detail) => {
                write!(formatter, "invalid analysis dependency manifest: {detail}")
            }
            Self::WeakDigest { field } => write!(
                formatter,
                "persistent artifact refuses non-cryptographic {field}"
            ),
            Self::PayloadDigest { expected, actual } => write!(
                formatter,
                "canonical artifact digest {actual:?} differs from descriptor {expected:?}"
            ),
        }
    }
}

impl std::error::Error for ArtifactPersistenceError {}
impl From<ArtifactCatalogError> for ArtifactPersistenceError {
    fn from(value: ArtifactCatalogError) -> Self {
        Self::Catalog(value)
    }
}
impl From<IdentityError> for ArtifactPersistenceError {
    fn from(value: IdentityError) -> Self {
        Self::Identity(value)
    }
}
impl From<StoreError> for ArtifactPersistenceError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

fn require_strong_descriptor(
    descriptor: &ArtifactDescriptor,
) -> Result<(), ArtifactPersistenceError> {
    for (field, digest) in [
        ("source digest", descriptor.source_digest),
        ("recipe digest", descriptor.recipe_digest),
        ("output digest", descriptor.output_digest),
    ] {
        if !digest.is_strong() {
            return Err(ArtifactPersistenceError::WeakDigest { field });
        }
    }
    Ok(())
}

fn artifact_payload_schema() -> Result<SchemaTag, IdentityError> {
    SchemaTag::analysis_artifact(ARTIFACT_PAYLOAD_SCHEMA_NAME, 1)
}
fn artifact_receipt_schema() -> Result<SchemaTag, IdentityError> {
    SchemaTag::reading_attachment(ARTIFACT_RECEIPT_SCHEMA_NAME, 1)
}

#[derive(Serialize, Deserialize)]
struct ArtifactReceiptV1 {
    format: String,
    version: u32,
    payload: ArtifactObjectRefDto,
    request: Vec<u8>,
    descriptor: ArtifactDescriptorDto,
}

#[derive(Serialize, Deserialize)]
struct ArtifactObjectRefDto {
    schema_class: String,
    schema_name: String,
    schema_version: u32,
    sha256: [u8; 32],
    byte_len: u64,
}

impl ArtifactObjectRefDto {
    fn from_object(object: &ObjectRef) -> Self {
        Self {
            schema_class: object.digest.schema().class().storage_label().into(),
            schema_name: object.digest.schema().name().into(),
            schema_version: object.digest.schema().version(),
            sha256: object.digest.sha256().bytes(),
            byte_len: object.byte_len,
        }
    }
    fn object_ref(self) -> Result<ObjectRef, ArtifactPersistenceError> {
        let class = ContentClass::from_storage_label(&self.schema_class).ok_or_else(|| {
            ArtifactPersistenceError::Manifest(format!(
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
struct ArtifactDescriptorDto {
    id: ContentDigestDto,
    kind: ArtifactKindDto,
    source_digest: ContentDigestDto,
    recipe_digest: ContentDigestDto,
    output_digest: ContentDigestDto,
    extent_start: i64,
    extent_end: i64,
    sample_rate: u32,
    channels: u16,
    provenance: Provenance,
}

impl ArtifactDescriptorDto {
    fn from_descriptor(value: &ArtifactDescriptor) -> Self {
        Self {
            id: ContentDigestDto::from_digest(value.id.0),
            kind: ArtifactKindDto::from_kind(&value.kind),
            source_digest: ContentDigestDto::from_digest(value.source_digest),
            recipe_digest: ContentDigestDto::from_digest(value.recipe_digest),
            output_digest: ContentDigestDto::from_digest(value.output_digest),
            extent_start: value.extent.start,
            extent_end: value.extent.end,
            sample_rate: value.sample_rate,
            channels: value.channels,
            provenance: value.provenance.clone(),
        }
    }

    fn into_descriptor(self) -> Result<ArtifactDescriptor, ArtifactPersistenceError> {
        let extent = FrameSpan::new(self.extent_start, self.extent_end)
            .ok_or_else(|| ArtifactPersistenceError::Manifest("analysis extent is empty".into()))?;
        Ok(ArtifactDescriptor {
            id: ArtifactId(self.id.into_digest()?),
            kind: self.kind.into_kind(),
            source_digest: self.source_digest.into_digest()?,
            recipe_digest: self.recipe_digest.into_digest()?,
            output_digest: self.output_digest.into_digest()?,
            extent,
            sample_rate: self.sample_rate,
            channels: self.channels,
            provenance: self.provenance,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct ContentDigestDto {
    algorithm: String,
    bytes: [u8; 32],
}

impl ContentDigestDto {
    fn from_digest(value: ContentDigest) -> Self {
        let algorithm = match value.algorithm {
            DigestAlgorithm::Sha256 => "sha256",
            DigestAlgorithm::Blake3 => "blake3",
            DigestAlgorithm::StableNonCryptographic => "stable-non-cryptographic",
        };
        Self {
            algorithm: algorithm.into(),
            bytes: value.bytes,
        }
    }
    fn into_digest(self) -> Result<ContentDigest, ArtifactPersistenceError> {
        let algorithm = match self.algorithm.as_str() {
            "sha256" => DigestAlgorithm::Sha256,
            "blake3" => DigestAlgorithm::Blake3,
            "stable-non-cryptographic" => DigestAlgorithm::StableNonCryptographic,
            _ => {
                return Err(ArtifactPersistenceError::Manifest(format!(
                    "unknown descriptor digest algorithm {}",
                    self.algorithm
                )));
            }
        };
        Ok(ContentDigest::new(algorithm, self.bytes))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "kebab-case")]
enum ArtifactKindDto {
    LoomSketch,
    Hpss,
    ReconstructionSet,
    ModelClaim,
    SpectralField,
    CoverageField,
    Other(String),
}

impl ArtifactKindDto {
    fn from_kind(value: &ArtifactKind) -> Self {
        match value {
            ArtifactKind::LoomSketch => Self::LoomSketch,
            ArtifactKind::Hpss => Self::Hpss,
            ArtifactKind::ReconstructionSet => Self::ReconstructionSet,
            ArtifactKind::ModelClaim => Self::ModelClaim,
            ArtifactKind::SpectralField => Self::SpectralField,
            ArtifactKind::CoverageField => Self::CoverageField,
            ArtifactKind::Other(name) => Self::Other(name.clone()),
        }
    }
    fn into_kind(self) -> ArtifactKind {
        match self {
            Self::LoomSketch => ArtifactKind::LoomSketch,
            Self::Hpss => ArtifactKind::Hpss,
            Self::ReconstructionSet => ArtifactKind::ReconstructionSet,
            Self::ModelClaim => ArtifactKind::ModelClaim,
            Self::SpectralField => ArtifactKind::SpectralField,
            Self::CoverageField => ArtifactKind::CoverageField,
            Self::Other(name) => ArtifactKind::Other(name),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactCatalogError {
    Missing(ArtifactId),
    PayloadType {
        id: ArtifactId,
    },
    IdentityMismatch {
        id: ArtifactId,
        output: ContentDigest,
    },
    DescriptorConflict(ArtifactId),
    InvalidExtent(FrameSpan),
    InvalidFormat {
        sample_rate: u32,
        channels: u16,
    },
}

impl fmt::Display for ArtifactCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(id) => write!(formatter, "analysis artifact {id:?} is unavailable"),
            Self::PayloadType { id } => {
                write!(
                    formatter,
                    "analysis artifact {id:?} has a different payload type"
                )
            }
            Self::IdentityMismatch { id, output } => write!(
                formatter,
                "artifact identity {id:?} does not match output digest {output:?}"
            ),
            Self::DescriptorConflict(id) => {
                write!(
                    formatter,
                    "artifact {id:?} has conflicting immutable descriptors"
                )
            }
            Self::InvalidExtent(extent) => write!(formatter, "invalid artifact extent {extent:?}"),
            Self::InvalidFormat {
                sample_rate,
                channels,
            } => write!(
                formatter,
                "invalid artifact format {sample_rate} Hz / {channels} channels"
            ),
        }
    }
}

impl std::error::Error for ArtifactCatalogError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_identity::Digest;
    use crate::ontology::Producer;
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
                "audec-artifact-catalog-{}-{sequence}",
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

    fn digest(value: u8) -> ContentDigest {
        ContentDigest::new(DigestAlgorithm::Sha256, [value; 32])
    }

    fn descriptor(value: u8) -> ArtifactDescriptor {
        ArtifactDescriptor {
            id: ArtifactId(digest(value)),
            kind: ArtifactKind::LoomSketch,
            source_digest: digest(1),
            recipe_digest: digest(2),
            output_digest: digest(value),
            extent: FrameSpan { start: 10, end: 20 },
            sample_rate: 48_000,
            channels: 1,
            provenance: Provenance {
                producer: Producer::Analyzer {
                    name: "test".into(),
                    version: "1".into(),
                    configuration_digest: None,
                },
                created_unix_ms: None,
                source_revision: None,
                note: None,
            },
        }
    }

    #[test]
    fn payload_lookup_is_typed_and_descriptor_collision_is_refused() {
        let mut catalog = ArtifactCatalog::new();
        catalog
            .insert(descriptor(3), Arc::new(vec![1_u8, 2]))
            .unwrap();
        assert_eq!(
            &*catalog.get::<Vec<u8>>(ArtifactId(digest(3))).unwrap(),
            &[1, 2]
        );
        assert!(matches!(
            catalog.get::<String>(ArtifactId(digest(3))),
            Err(ArtifactCatalogError::PayloadType { .. })
        ));

        let mut conflict = descriptor(3);
        conflict.kind = ArtifactKind::Hpss;
        assert_eq!(
            catalog.insert(conflict, Arc::new(vec![1_u8, 2])),
            Err(ArtifactCatalogError::DescriptorConflict(ArtifactId(
                digest(3)
            )))
        );
    }

    #[test]
    fn sha256_core_matches_standard_vector_and_parts_are_delimited() {
        let expected = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(Sha256Digest::hash_raw(b"abc").bytes(), expected);
        assert_eq!(
            Sha256Digest::hash_raw_parts(&[b"a", b"b", b"c"]).bytes(),
            expected
        );
        assert_ne!(
            sha256_content(b"test", &[b"ab", b"c"]),
            sha256_content(b"test", &[b"a", b"bc"])
        );
    }

    #[test]
    fn byte_artifact_reopens_and_corruption_is_refused() {
        let root = TestRoot::new();
        let store = FsContentStore::new(&root.0);
        let payload = Arc::new(b"canonical onset evidence".to_vec());
        let output = canonical_artifact_payload_digest(payload.as_slice());
        let mut description = descriptor(3);
        description.id = ArtifactId(output);
        description.output_digest = output;
        let request = ProductKey::builder(
            artifact_payload_schema().unwrap(),
            Digest::of_bytes(SchemaTag::recipe("test/onset-analyzer", 1).unwrap(), b"v1"),
        )
        .unwrap()
        .build();
        let mut first = ArtifactCatalog::new();
        let persisted = first
            .publish_bytes(
                &store,
                description.clone(),
                payload.clone(),
                request.clone(),
            )
            .unwrap();

        let reopened_store = FsContentStore::new(&root.0);
        let mut reopened_catalog = ArtifactCatalog::new();
        let reopened = reopened_catalog
            .reopen_bytes(&reopened_store, &persisted.manifest)
            .unwrap();
        assert_eq!(reopened.descriptor, description);
        assert_eq!(reopened.request, request);
        assert_eq!(reopened.bytes.as_slice(), payload.as_slice());
        assert_eq!(
            reopened_catalog
                .get::<Vec<u8>>(description.id)
                .unwrap()
                .as_slice(),
            payload.as_slice()
        );

        let payload_path = reopened_store.verify(&persisted.payload).unwrap().path;
        let mut permissions = fs::metadata(&payload_path).unwrap().permissions();
        #[cfg(unix)]
        permissions.set_mode(permissions.mode() | 0o200);
        #[cfg(not(unix))]
        permissions.set_readonly(false);
        fs::set_permissions(&payload_path, permissions).unwrap();
        fs::write(payload_path, b"corrupt").unwrap();
        assert!(reopened_catalog
            .reopen_bytes(&reopened_store, &persisted.manifest)
            .is_err());
    }

    #[test]
    fn persistent_boundary_refuses_weak_or_false_output_identity() {
        let root = TestRoot::new();
        let store = FsContentStore::new(&root.0);
        let payload = Arc::new(b"evidence".to_vec());
        let request = ProductKey::builder(
            artifact_payload_schema().unwrap(),
            Digest::of_bytes(SchemaTag::recipe("test/analyzer", 1).unwrap(), b"v1"),
        )
        .unwrap()
        .build();
        let mut weak = descriptor(3);
        weak.source_digest.algorithm = DigestAlgorithm::StableNonCryptographic;
        assert!(matches!(
            ArtifactCatalog::new().publish_bytes(&store, weak, payload.clone(), request.clone()),
            Err(ArtifactPersistenceError::WeakDigest { .. })
        ));
        assert!(matches!(
            ArtifactCatalog::new().publish_bytes(&store, descriptor(3), payload, request),
            Err(ArtifactPersistenceError::PayloadDigest { .. })
        ));
    }
}
