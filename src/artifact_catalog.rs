//! Immutable, content-addressed analysis artifact catalog.
//!
//! Artifacts are computed evidence products, not project truth. The catalog
//! stores immutable payloads behind a descriptor whose output digest is the
//! identity. It never promotes a recurrence family, mask, model output, or
//! reconstruction proposal into an asserted source identity.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::aspect::FrameSpan;
use crate::ontology::Provenance;

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
    Other(String),
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
    use crate::ontology::Producer;

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
}
