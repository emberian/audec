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
    CoverageField,
    Other(String),
}

/// Collision-resistant digest over a domain tag and length-delimited byte
/// parts. Length prefixes make `["ab", "c"]` distinct from `["a", "bc"]`.
/// This is the common content-addressing primitive for non-PCM artifacts;
/// render products retain their own canonical PCM domain recipe.
pub fn sha256_content(domain: &[u8], parts: &[&[u8]]) -> ContentDigest {
    let mut digest = Sha256::new();
    digest.update(b"audec:content-address:v1\0");
    digest.update(&(domain.len() as u64).to_le_bytes());
    digest.update(domain);
    for part in parts {
        digest.update(&(part.len() as u64).to_le_bytes());
        digest.update(part);
    }
    ContentDigest::new(DigestAlgorithm::Sha256, digest.finalize())
}

struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffered: usize,
    bytes: u64,
}

impl Sha256 {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: Self::INITIAL,
            buffer: [0; 64],
            buffered: 0,
            bytes: 0,
        }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        self.bytes = self.bytes.wrapping_add(bytes.len() as u64);
        if self.buffered > 0 {
            let copied = (64 - self.buffered).min(bytes.len());
            self.buffer[self.buffered..self.buffered + copied].copy_from_slice(&bytes[..copied]);
            self.buffered += copied;
            bytes = &bytes[copied..];
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }
        while bytes.len() >= 64 {
            let block: &[u8; 64] = bytes[..64].try_into().expect("exact SHA-256 block");
            self.compress(block);
            bytes = &bytes[64..];
        }
        self.buffer[..bytes.len()].copy_from_slice(bytes);
        self.buffered = bytes.len();
    }

    fn finalize(mut self) -> [u8; 32] {
        let bit_length = self.bytes.wrapping_mul(8);
        self.buffer[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > 56 {
            self.buffer[self.buffered..].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffer = [0; 64];
        } else {
            self.buffer[self.buffered..56].fill(0);
        }
        self.buffer[56..64].copy_from_slice(&bit_length.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);
        let mut output = [0; 32];
        for (chunk, word) in output.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        output
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut words = [0_u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(chunk.try_into().expect("four-byte SHA word"));
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(Self::K[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (target, addition) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *target = target.wrapping_add(addition);
        }
    }
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

    #[test]
    fn sha256_core_matches_standard_vector_and_parts_are_delimited() {
        let mut digest = Sha256::new();
        digest.update(b"abc");
        assert_eq!(
            digest.finalize(),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
        assert_ne!(
            sha256_content(b"test", &[b"ab", b"c"]),
            sha256_content(b"test", &[b"a", b"bc"])
        );
    }
}
