//! Exact, virtual sample material derived from decoded project audio.
//!
//! A virtual slice remains a range of an immutable source asset. It is not a
//! newly imported file and does not claim a filesystem location. Consolidation
//! is represented separately because it creates a new asset. FNV fingerprints
//! in this module are deterministic lookup hints only: every reuse decision
//! also compares the validated format and every finite PCM sample bit-for-bit.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::assets::{
    AssetFrameRange, AssetId, ContentFingerprint, ContentHashAlgorithm, ContentId,
};
use crate::audio::AudioFormat;
use crate::daw_render::PcmAsset;

const CANONICAL_PCM_DOMAIN: &[u8] = b"audec.decoded-pcm.v1\0";
const FNV_OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
const FNV_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

/// Provenance for an in-project sample that still refers to source PCM.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VirtualSliceRef {
    pub source_asset: AssetId,
    pub source_range: AssetFrameRange,
}

impl VirtualSliceRef {
    pub fn new(
        source_asset: AssetId,
        source_range: AssetFrameRange,
    ) -> Result<Self, SampleMaterialError> {
        let result = Self {
            source_asset,
            source_range,
        };
        result.validate()?;
        Ok(result)
    }

    pub fn validate(self) -> Result<(), SampleMaterialError> {
        validate_asset_id(self.source_asset)?;
        if self.source_range.start.0 >= self.source_range.end.0 {
            return Err(SampleMaterialError::InvalidSourceRange {
                start: self.source_range.start.0,
                end: self.source_range.end.0,
            });
        }
        Ok(())
    }

    pub fn frame_count(self) -> u64 {
        self.source_range.end.0 - self.source_range.start.0
    }
}

/// A source accepted by a sampler zone before runtime PCM resolution.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SourceMaterialRef {
    Asset(AssetId),
    VirtualSlice(VirtualSliceRef),
}

/// Stable namespace for analyzer-local proposal and evidence identities.
///
/// Reconstruction IDs restart for each analysis result. A persisted citation
/// is therefore always the pair `(scope, local)`, never the local integer by
/// itself. The scope is supplied by the analysis/result content identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DerivationScope(pub u128);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ScopedEvidenceRef {
    pub scope: DerivationScope,
    pub local: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ScopedProposalRef {
    pub scope: DerivationScope,
    pub local: u64,
}

/// Durable explanation for why a sampler zone addresses particular material.
///
/// This metadata is intentionally quiet during ordinary beat making, but it
/// is part of project truth and survives save/reopen. It never promotes an
/// anonymous hit family into an asserted instrument identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SampleMaterialProvenance {
    /// A user explicitly mapped a pre-existing whole media-pool asset.
    ExistingAsset,
    /// A user explicitly sampled an exact source range.
    ManualSelection,
    /// Analysis proposed boundaries; the user still chose to construct them.
    OnsetChop {
        analyzer: String,
        evidence: Vec<ScopedEvidenceRef>,
    },
    /// A selected reconstruction proposal supplied this anonymous slice.
    Deprojection {
        proposal: ScopedProposalRef,
        evidence: Vec<ScopedEvidenceRef>,
    },
    /// A phase-bearing analyzer product was explicitly turned into a reusable
    /// whole-asset sampler template. This preserves its derivation without
    /// asserting that the anonymous recurrence family names an instrument.
    AnalysisTemplate {
        analyzer: String,
        evidence: Vec<ScopedEvidenceRef>,
    },
    /// A virtual range was explicitly rendered/copied into a new asset.
    Consolidated(ConsolidatedMaterialRef),
}

impl SampleMaterialProvenance {
    pub fn validate_for(self: &Self, source: SourceMaterialRef) -> Result<(), SampleMaterialError> {
        source.validate()?;
        match (self, source) {
            (Self::ExistingAsset, SourceMaterialRef::Asset(_))
            | (Self::ManualSelection, SourceMaterialRef::VirtualSlice(_)) => Ok(()),
            (Self::OnsetChop { analyzer, evidence }, SourceMaterialRef::VirtualSlice(_)) => {
                if analyzer.trim().is_empty() {
                    return Err(SampleMaterialError::EmptyAnalyzer);
                }
                validate_evidence(evidence)
            }
            (Self::Deprojection { proposal, evidence }, SourceMaterialRef::VirtualSlice(_)) => {
                if proposal.local == 0 {
                    return Err(SampleMaterialError::ZeroProposalReference);
                }
                validate_evidence(evidence)
            }
            (Self::AnalysisTemplate { analyzer, evidence }, SourceMaterialRef::Asset(_)) => {
                if analyzer.trim().is_empty() {
                    return Err(SampleMaterialError::EmptyAnalyzer);
                }
                validate_evidence(evidence)
            }
            (Self::Consolidated(record), SourceMaterialRef::Asset(asset)) => {
                if record.asset != asset {
                    return Err(SampleMaterialError::ConsolidatedAssetMismatch {
                        expected: asset.0,
                        actual: record.asset.0,
                    });
                }
                record.validate()
            }
            _ => Err(SampleMaterialError::ProvenanceSourceMismatch),
        }
    }
}

impl SourceMaterialRef {
    pub fn asset_id(self) -> AssetId {
        match self {
            Self::Asset(asset) => asset,
            Self::VirtualSlice(slice) => slice.source_asset,
        }
    }

    pub fn virtual_slice(self) -> Option<VirtualSliceRef> {
        match self {
            Self::Asset(_) => None,
            Self::VirtualSlice(slice) => Some(slice),
        }
    }

    pub fn validate(self) -> Result<(), SampleMaterialError> {
        match self {
            Self::Asset(asset) => validate_asset_id(asset),
            Self::VirtualSlice(slice) => slice.validate(),
        }
    }
}

/// Provenance for a materialized file produced from a virtual slice.
///
/// This is deliberately not a `SourceMaterialRef` variant: after
/// consolidation the new asset is referenced as `SourceMaterialRef::Asset`,
/// while this record explains where that asset came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConsolidatedMaterialRef {
    pub asset: AssetId,
    pub derived_from: VirtualSliceRef,
    pub decoded_pcm: CanonicalPcmIdentity,
}

impl ConsolidatedMaterialRef {
    pub fn new(
        asset: AssetId,
        derived_from: VirtualSliceRef,
        decoded_pcm: CanonicalPcmIdentity,
    ) -> Result<Self, SampleMaterialError> {
        let result = Self {
            asset,
            derived_from,
            decoded_pcm,
        };
        result.validate()?;
        Ok(result)
    }

    pub fn validate(self) -> Result<(), SampleMaterialError> {
        validate_asset_id(self.asset)?;
        self.derived_from.validate()?;
        if self.decoded_pcm.frame_count != self.derived_from.frame_count() {
            return Err(SampleMaterialError::ConsolidatedFrameCountMismatch {
                expected: self.derived_from.frame_count(),
                actual: self.decoded_pcm.frame_count,
            });
        }
        Ok(())
    }
}

/// A minimally coupled borrowed view of interleaved decoded PCM.
#[derive(Clone, Copy, Debug)]
pub struct DecodedPcmView<'a> {
    pub format: AudioFormat,
    pub interleaved: &'a [f32],
}

impl<'a> DecodedPcmView<'a> {
    pub fn new(format: AudioFormat, interleaved: &'a [f32]) -> Self {
        Self {
            format,
            interleaved,
        }
    }

    pub fn from_pcm_asset(asset: &'a PcmAsset) -> Self {
        Self::new(asset.format, &asset.samples)
    }

    pub fn validate(self) -> Result<(), SampleMaterialError> {
        let channels = usize::from(self.format.channels.get());
        if self.interleaved.is_empty() {
            return Err(SampleMaterialError::EmptyPcm);
        }
        if self.interleaved.len() % channels != 0 {
            return Err(SampleMaterialError::PartialFrame {
                samples: self.interleaved.len(),
                channels,
            });
        }
        if let Some((sample_index, _)) = self
            .interleaved
            .iter()
            .enumerate()
            .find(|(_, sample)| !sample.is_finite())
        {
            return Err(SampleMaterialError::NonFinitePcm { sample_index });
        }
        Ok(())
    }

    pub fn frame_count(self) -> Result<u64, SampleMaterialError> {
        self.validate()?;
        let channels = usize::from(self.format.channels.get());
        u64::try_from(self.interleaved.len() / channels)
            .map_err(|_| SampleMaterialError::PcmTooLarge)
    }
}

impl<'a> From<&'a PcmAsset> for DecodedPcmView<'a> {
    fn from(asset: &'a PcmAsset) -> Self {
        Self::from_pcm_asset(asset)
    }
}

/// Deterministic metadata for canonical decoded PCM.
///
/// `fingerprint` is not collision-resistant. Treat this value as an index key;
/// use [`canonical_pcm_eq`] before reusing or deleting material.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalPcmIdentity {
    pub format: AudioFormat,
    pub frame_count: u64,
    pub fingerprint: ContentFingerprint,
}

/// Owned exact result of resolving a virtual slice.
#[derive(Clone, Debug)]
pub struct ExtractedSampleMaterial {
    pub provenance: VirtualSliceRef,
    pub format: AudioFormat,
    pub interleaved: Arc<[f32]>,
    pub identity: CanonicalPcmIdentity,
}

impl ExtractedSampleMaterial {
    pub fn as_view(&self) -> DecodedPcmView<'_> {
        DecodedPcmView::new(self.format, &self.interleaved)
    }

    pub fn to_pcm_asset(&self) -> PcmAsset {
        PcmAsset::new(self.format, Arc::clone(&self.interleaved))
            .expect("extracted material preserves complete frames")
    }
}

/// Extract a source-frame range without resampling, remixing, or normalization.
pub fn extract_virtual_slice(
    slice: VirtualSliceRef,
    source: &PcmAsset,
) -> Result<ExtractedSampleMaterial, SampleMaterialError> {
    extract_virtual_slice_from_view(slice, DecodedPcmView::from(source))
}

pub fn extract_virtual_slice_from_view(
    slice: VirtualSliceRef,
    source: DecodedPcmView<'_>,
) -> Result<ExtractedSampleMaterial, SampleMaterialError> {
    slice.validate()?;
    let source_frames = source.frame_count()?;
    if slice.source_range.end.0 > source_frames {
        return Err(SampleMaterialError::SourceRangeOutsidePcm {
            end: slice.source_range.end.0,
            frame_count: source_frames,
        });
    }
    let channels = usize::from(source.format.channels.get());
    let start = usize::try_from(slice.source_range.start.0)
        .ok()
        .and_then(|frame| frame.checked_mul(channels))
        .ok_or(SampleMaterialError::PcmTooLarge)?;
    let end = usize::try_from(slice.source_range.end.0)
        .ok()
        .and_then(|frame| frame.checked_mul(channels))
        .ok_or(SampleMaterialError::PcmTooLarge)?;
    let interleaved: Arc<[f32]> = Arc::from(&source.interleaved[start..end]);
    let view = DecodedPcmView::new(source.format, &interleaved);
    let identity = canonical_pcm_identity(view)?;
    Ok(ExtractedSampleMaterial {
        provenance: slice,
        format: source.format,
        interleaved,
        identity,
    })
}

/// Hash canonical decoded PCM as domain tag + LE format/count + finite f32 LE bits.
pub fn canonical_pcm_identity(
    pcm: DecodedPcmView<'_>,
) -> Result<CanonicalPcmIdentity, SampleMaterialError> {
    let frame_count = pcm.frame_count()?;
    let mut hash = FNV_OFFSET;
    let mut bytes_hashed = 0_u64;
    hash_part(&mut hash, &mut bytes_hashed, CANONICAL_PCM_DOMAIN)?;
    hash_part(
        &mut hash,
        &mut bytes_hashed,
        &pcm.format.sample_rate.get().to_le_bytes(),
    )?;
    hash_part(
        &mut hash,
        &mut bytes_hashed,
        &pcm.format.channels.get().to_le_bytes(),
    )?;
    hash_part(&mut hash, &mut bytes_hashed, &frame_count.to_le_bytes())?;
    for sample in pcm.interleaved {
        hash_part(
            &mut hash,
            &mut bytes_hashed,
            &sample.to_bits().to_le_bytes(),
        )?;
    }
    Ok(CanonicalPcmIdentity {
        format: pcm.format,
        frame_count,
        fingerprint: ContentFingerprint {
            algorithm: ContentHashAlgorithm::Fnv1a128NonCryptographic,
            id: ContentId(hash),
            bytes_hashed,
        },
    })
}

/// Exact equality of validated canonical decoded PCM, including signed zero.
pub fn canonical_pcm_eq(
    left: DecodedPcmView<'_>,
    right: DecodedPcmView<'_>,
) -> Result<bool, SampleMaterialError> {
    left.validate()?;
    right.validate()?;
    Ok(left.format == right.format
        && left.interleaved.len() == right.interleaved.len()
        && left
            .interleaved
            .iter()
            .zip(right.interleaved)
            .all(|(left, right)| left.to_bits() == right.to_bits()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReusePolicy {
    SameSourceRange,
    AnyExactDecodedPcm,
}

/// A fingerprint-indexed candidate whose PCM remains available for proof.
#[derive(Clone, Debug)]
pub struct ReuseCandidate<'a, K> {
    pub key: K,
    pub provenance: VirtualSliceRef,
    pub fingerprint_hint: ContentFingerprint,
    pub pcm: DecodedPcmView<'a>,
}

/// Return the first deterministically supplied candidate proven bit-identical.
///
/// FNV and provenance only reduce the candidate set. PCM equality is always
/// checked, so a forged or accidental fingerprint collision cannot authorize
/// reuse.
pub fn find_verified_reuse<'a, K, I>(
    desired: &ExtractedSampleMaterial,
    policy: ReusePolicy,
    candidates: I,
) -> Result<Option<K>, SampleMaterialError>
where
    I: IntoIterator<Item = ReuseCandidate<'a, K>>,
{
    for candidate in candidates {
        if policy == ReusePolicy::SameSourceRange && candidate.provenance != desired.provenance {
            continue;
        }
        if candidate.fingerprint_hint != desired.identity.fingerprint {
            continue;
        }
        if canonical_pcm_eq(desired.as_view(), candidate.pcm)? {
            return Ok(Some(candidate.key));
        }
    }
    Ok(None)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SampleMaterialError {
    ZeroAssetId,
    InvalidSourceRange { start: u64, end: u64 },
    EmptyPcm,
    PartialFrame { samples: usize, channels: usize },
    NonFinitePcm { sample_index: usize },
    SourceRangeOutsidePcm { end: u64, frame_count: u64 },
    ConsolidatedFrameCountMismatch { expected: u64, actual: u64 },
    ConsolidatedAssetMismatch { expected: u64, actual: u64 },
    ProvenanceSourceMismatch,
    EmptyAnalyzer,
    ZeroEvidenceReference,
    DuplicateEvidenceReference,
    ZeroProposalReference,
    PcmTooLarge,
}

impl fmt::Display for SampleMaterialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroAssetId => write!(formatter, "sample material references asset zero"),
            Self::InvalidSourceRange { start, end } => {
                write!(formatter, "invalid source range {start}..{end}")
            }
            Self::EmptyPcm => write!(formatter, "decoded PCM is empty"),
            Self::PartialFrame { samples, channels } => write!(
                formatter,
                "{samples} interleaved samples do not fill {channels}-channel frames"
            ),
            Self::NonFinitePcm { sample_index } => {
                write!(formatter, "decoded PCM sample {sample_index} is non-finite")
            }
            Self::SourceRangeOutsidePcm { end, frame_count } => write!(
                formatter,
                "source range ends at frame {end}, beyond {frame_count} decoded frames"
            ),
            Self::ConsolidatedFrameCountMismatch { expected, actual } => write!(
                formatter,
                "consolidated PCM has {actual} frames; source slice has {expected}"
            ),
            Self::ConsolidatedAssetMismatch { expected, actual } => write!(
                formatter,
                "consolidated provenance names asset {actual}; material names {expected}"
            ),
            Self::ProvenanceSourceMismatch => {
                write!(
                    formatter,
                    "sample provenance does not match its material kind"
                )
            }
            Self::EmptyAnalyzer => write!(formatter, "onset-chop analyzer is empty"),
            Self::ZeroEvidenceReference => write!(formatter, "evidence reference is zero"),
            Self::DuplicateEvidenceReference => {
                write!(formatter, "evidence reference is duplicated")
            }
            Self::ZeroProposalReference => write!(formatter, "proposal reference is zero"),
            Self::PcmTooLarge => write!(formatter, "decoded PCM is too large to address"),
        }
    }
}

impl Error for SampleMaterialError {}

fn validate_asset_id(asset: AssetId) -> Result<(), SampleMaterialError> {
    if asset.0 == 0 {
        Err(SampleMaterialError::ZeroAssetId)
    } else {
        Ok(())
    }
}

fn validate_evidence(evidence: &[ScopedEvidenceRef]) -> Result<(), SampleMaterialError> {
    let mut seen = std::collections::BTreeSet::new();
    for reference in evidence {
        if reference.local == 0 {
            return Err(SampleMaterialError::ZeroEvidenceReference);
        }
        if !seen.insert(*reference) {
            return Err(SampleMaterialError::DuplicateEvidenceReference);
        }
    }
    Ok(())
}

fn hash_part(
    hash: &mut u128,
    bytes_hashed: &mut u64,
    bytes: &[u8],
) -> Result<(), SampleMaterialError> {
    *bytes_hashed = bytes_hashed
        .checked_add(u64::try_from(bytes.len()).map_err(|_| SampleMaterialError::PcmTooLarge)?)
        .ok_or(SampleMaterialError::PcmTooLarge)?;
    for byte in bytes {
        *hash ^= u128::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU16, NonZeroU32};

    use crate::assets::{AssetFrameRange, AssetId, SampleFrames};

    use super::*;

    fn format(channels: u16) -> AudioFormat {
        AudioFormat {
            sample_rate: NonZeroU32::new(48_000).unwrap(),
            channels: NonZeroU16::new(channels).unwrap(),
        }
    }

    fn slice(start: u64, end: u64) -> VirtualSliceRef {
        VirtualSliceRef::new(
            AssetId(7),
            AssetFrameRange::new(SampleFrames(start), SampleFrames(end)).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn extracts_exact_interleaved_frame_range() {
        let source = PcmAsset::new(format(2), Arc::from([0.0, 1.0, 2.0, 3.0, 4.0, 5.0])).unwrap();
        let extracted = extract_virtual_slice(slice(1, 3), &source).unwrap();
        assert_eq!(&*extracted.interleaved, &[2.0, 3.0, 4.0, 5.0]);
        assert_eq!(extracted.identity.frame_count, 2);
    }

    #[test]
    fn identity_includes_format_and_exact_float_bits() {
        let a_samples = [0.0_f32, 0.25];
        let signed_zero_samples = [-0.0_f32, 0.25];
        let a = canonical_pcm_identity(DecodedPcmView::new(format(1), &a_samples)).unwrap();
        let signed_zero =
            canonical_pcm_identity(DecodedPcmView::new(format(1), &signed_zero_samples)).unwrap();
        let stereo = canonical_pcm_identity(DecodedPcmView::new(format(2), &a_samples)).unwrap();
        assert_ne!(a.fingerprint, signed_zero.fingerprint);
        assert_ne!(a.fingerprint, stereo.fingerprint);
    }

    #[test]
    fn rejects_non_finite_pcm() {
        let samples = [0.0, f32::NAN];
        assert_eq!(
            canonical_pcm_identity(DecodedPcmView::new(format(1), &samples)),
            Err(SampleMaterialError::NonFinitePcm { sample_index: 1 })
        );
    }

    #[test]
    fn fnv_match_alone_never_authorizes_reuse() {
        let source = PcmAsset::new(format(1), Arc::from([0.1, 0.2, 0.3])).unwrap();
        let desired = extract_virtual_slice(slice(0, 2), &source).unwrap();
        let impostor_samples = [0.1, 9.0];
        let impostor = ReuseCandidate {
            key: AssetId(99),
            provenance: desired.provenance,
            fingerprint_hint: desired.identity.fingerprint,
            pcm: DecodedPcmView::new(format(1), &impostor_samples),
        };
        assert_eq!(
            find_verified_reuse(&desired, ReusePolicy::SameSourceRange, [impostor]).unwrap(),
            None
        );
    }

    #[test]
    fn verified_reuse_returns_exact_candidate() {
        let source = PcmAsset::new(format(1), Arc::from([0.1, 0.2, 0.3])).unwrap();
        let desired = extract_virtual_slice(slice(0, 2), &source).unwrap();
        let samples = [0.1, 0.2];
        let candidate = ReuseCandidate {
            key: AssetId(42),
            provenance: desired.provenance,
            fingerprint_hint: desired.identity.fingerprint,
            pcm: DecodedPcmView::new(format(1), &samples),
        };
        assert_eq!(
            find_verified_reuse(&desired, ReusePolicy::SameSourceRange, [candidate]).unwrap(),
            Some(AssetId(42))
        );
    }
}
