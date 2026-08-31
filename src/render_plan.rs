//! Stable identities and immutable metadata for one renderable project state.
//!
//! A render plan is the boundary between editable project state and audio
//! execution.  This module deliberately contains no compiler, DSP, worker,
//! cache, device, or UI ownership.  Adapters freeze those things elsewhere,
//! then describe the result here.  Plan identity is structural rather than a
//! short hash: equality includes every revision, dependency, recipe, and
//! compiled-frame fact supplied by the adapter.

use std::error::Error;
use std::fmt;
use std::num::{NonZeroU16, NonZeroU32};
use std::sync::Arc;

/// An exact 256-bit digest supplied by a boundary that has read the bytes.
///
/// This type does not choose a hash algorithm.  Media import, project codecs,
/// plugin state, and rendered PCM have different byte canonicalizations; the
/// boundary responsible for each must compute a collision-resistant digest
/// and record the algorithm in its own versioned recipe.  Keeping this type
/// opaque prevents a project-local FNV duplicate hint from silently becoming
/// an authenticity or durable-cache identity.
#[derive(Clone, Copy, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExactDigest([u8; 32]);

impl ExactDigest {
    pub const ZERO: Self = Self([0; 32]);

    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn bytes(self) -> [u8; 32] {
        self.0
    }

    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }
}

impl fmt::Debug for ExactDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ExactDigest({self})")
    }
}

impl fmt::Display for ExactDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A non-empty signed, end-exclusive range on the project timeline.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RenderSpan {
    pub start: i64,
    pub end: i64,
}

impl RenderSpan {
    pub fn new(start: i64, end: i64) -> Result<Self, RenderPlanError> {
        if start >= end {
            return Err(RenderPlanError::EmptySpan { start, end });
        }
        Ok(Self { start, end })
    }

    pub const fn len(self) -> u64 {
        self.end.saturating_sub(self.start) as u64
    }

    pub const fn contains(self, frame: i64) -> bool {
        self.start <= frame && frame < self.end
    }

    pub const fn contains_span(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        (start < end).then_some(Self { start, end })
    }
}

/// Native PCM facts that cannot change while an audio host remains open.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RenderFormat {
    pub sample_rate: NonZeroU32,
    pub channels: NonZeroU16,
}

impl RenderFormat {
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, RenderPlanError> {
        Ok(Self {
            sample_rate: NonZeroU32::new(sample_rate).ok_or(RenderPlanError::ZeroSampleRate)?,
            channels: NonZeroU16::new(channels).ok_or(RenderPlanError::ZeroChannels)?,
        })
    }
}

/// Exact copies of the aggregate and per-domain project generations.
///
/// This intentionally does not depend on `daw_project`: an adapter copies the
/// current `ProjectRevisions` here.  The render foundation can therefore be
/// developed and tested before it is wired into the aggregate command path.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProjectRevisionStamp {
    pub aggregate: u64,
    pub arrangement: u64,
    pub sequencer: u64,
    pub automation: u64,
    pub assets: u64,
    pub mixer: u64,
    pub air: u64,
    pub bindings: u64,
}

/// Stable dependency names. Raw numbers remain tagged by their source domain;
/// an asset ID can never compare equal to a plugin or analysis artifact ID.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RenderDependencyKey {
    MediaAsset(u64),
    AnalysisArtifact { namespace: u128, local: u64 },
    PluginInstance(u64),
    ModelArtifact { namespace: u128, local: u64 },
    External { namespace: u128, local: u128 },
}

/// One immutable input consumed by the plan.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RenderDependencyStamp {
    pub key: RenderDependencyKey,
    /// Digest of canonical bytes actually consumed by the renderer.
    pub content: ExactDigest,
    /// Runtime replacement generation, e.g. a decoder cache replacing PCM
    /// while the logical project/asset revision remains unchanged.
    pub runtime_generation: u64,
}

/// Everything about engine execution that can alter output samples.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EngineRecipeStamp {
    /// Bumped when scheduling, DSP, sanitization, or summation semantics change.
    pub engine_abi: u32,
    pub format: RenderFormat,
    /// Canonical partition used by stateful DSP and deterministic tests.
    pub canonical_block_frames: NonZeroU32,
    pub performance_seed: u64,
    /// Canonical digest of instrument definitions, routing runtime facts,
    /// processor states, quality choices, and every other adapter-owned knob.
    pub configuration: ExactDigest,
}

impl EngineRecipeStamp {
    pub fn new(
        engine_abi: u32,
        format: RenderFormat,
        canonical_block_frames: u32,
        performance_seed: u64,
        configuration: ExactDigest,
    ) -> Result<Self, RenderPlanError> {
        Ok(Self {
            engine_abi,
            format,
            canonical_block_frames: NonZeroU32::new(canonical_block_frames)
                .ok_or(RenderPlanError::ZeroBlockFrames)?,
            performance_seed,
            configuration,
        })
    }
}

/// Sample-equivalence strength promised by the compiled engine.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DeterminismGrade {
    /// Same plan and canonical partition produce identical canonical `f32` PCM.
    BitExact,
    /// Samples are comparable only under a declared numerical tolerance.
    StableWithinTolerance,
    /// A processor explicitly reports non-deterministic offline behavior.
    NonDeterministic,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BusTap {
    PreFader,
    PostFader,
    Output,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExplanationScopeId {
    pub namespace: u128,
    pub local: u64,
}

/// A semantic graph output, not a promise about internal cache cut points.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RenderScope {
    Master,
    Bus { bus: u64, tap: BusTap },
    Track(u64),
    Explanation(ExplanationScopeId),
}

/// Output extent after the authored/project range ends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OutputTailPolicy {
    Crop,
    FixedFrames(u64),
    UntilBelow {
        amplitude: f32,
        hold_frames: u64,
        hard_max_frames: u64,
    },
}

impl OutputTailPolicy {
    pub fn validate(self) -> Result<Self, RenderPlanError> {
        match self {
            Self::UntilBelow {
                amplitude,
                hold_frames,
                hard_max_frames,
            } if !amplitude.is_finite()
                || amplitude < 0.0
                || hold_frames == 0
                || hard_max_frames == 0 =>
            {
                Err(RenderPlanError::InvalidTailPolicy)
            }
            _ => Ok(self),
        }
    }
}

/// State-history behavior of the worst node in a compiled graph.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Tileability {
    Stateless,
    BoundedHistory {
        lookbehind_frames: u64,
        lookahead_frames: u64,
    },
    Checkpointable,
    SequentialOnly,
}

/// Structural identity of a render plan.
///
/// This is intentionally not compressed to `u64`. The dependency list is
/// sorted and unique, so equality is a stable exact comparison of all supplied
/// inputs. `snapshot` must cover project state beyond generation counters;
/// counters alone are not portable across reopen/import boundaries.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RenderPlanId {
    pub schema_version: u16,
    pub project_namespace: u128,
    pub snapshot: ExactDigest,
    pub revisions: ProjectRevisionStamp,
    pub compiled_extent: RenderSpan,
    pub engine: EngineRecipeStamp,
    dependencies: Arc<[RenderDependencyStamp]>,
}

impl RenderPlanId {
    pub const SCHEMA_VERSION: u16 = 1;

    pub fn new(
        project_namespace: u128,
        snapshot: ExactDigest,
        revisions: ProjectRevisionStamp,
        compiled_extent: RenderSpan,
        engine: EngineRecipeStamp,
        mut dependencies: Vec<RenderDependencyStamp>,
    ) -> Result<Self, RenderPlanError> {
        dependencies.sort_by(|left, right| left.key.cmp(&right.key));
        for pair in dependencies.windows(2) {
            if pair[0].key == pair[1].key {
                return Err(RenderPlanError::DuplicateDependency(pair[0].key.clone()));
            }
        }
        Ok(Self {
            schema_version: Self::SCHEMA_VERSION,
            project_namespace,
            snapshot,
            revisions,
            compiled_extent,
            engine,
            dependencies: dependencies.into(),
        })
    }

    pub fn dependencies(&self) -> &[RenderDependencyStamp] {
        &self.dependencies
    }
}

/// Immutable plan metadata shared by workers, playback, export, and coverage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderPlan {
    pub id: RenderPlanId,
    pub determinism: DeterminismGrade,
    pub tileability: Tileability,
}

impl RenderPlan {
    pub fn new(id: RenderPlanId, determinism: DeterminismGrade, tileability: Tileability) -> Self {
        Self {
            id,
            determinism,
            tileability,
        }
    }

    pub const fn format(&self) -> RenderFormat {
        self.id.engine.format
    }

    pub const fn extent(&self) -> RenderSpan {
        self.id.compiled_extent
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderPlanError {
    EmptySpan { start: i64, end: i64 },
    ZeroSampleRate,
    ZeroChannels,
    ZeroBlockFrames,
    DuplicateDependency(RenderDependencyKey),
    InvalidTailPolicy,
}

impl fmt::Display for RenderPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySpan { start, end } => {
                write!(
                    formatter,
                    "render span must be non-empty, got {start}..{end}"
                )
            }
            Self::ZeroSampleRate => write!(formatter, "render sample rate must be nonzero"),
            Self::ZeroChannels => write!(formatter, "render channel count must be nonzero"),
            Self::ZeroBlockFrames => write!(formatter, "render block size must be nonzero"),
            Self::DuplicateDependency(key) => {
                write!(
                    formatter,
                    "render dependency {key:?} is listed more than once"
                )
            }
            Self::InvalidTailPolicy => write!(formatter, "render tail policy is invalid"),
        }
    }
}

impl Error for RenderPlanError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: u8) -> ExactDigest {
        ExactDigest::new([byte; 32])
    }

    fn recipe() -> EngineRecipeStamp {
        EngineRecipeStamp::new(3, RenderFormat::new(48_000, 2).unwrap(), 512, 7, digest(9)).unwrap()
    }

    fn plan(dependency_generation: u64) -> RenderPlanId {
        RenderPlanId::new(
            42,
            digest(1),
            ProjectRevisionStamp {
                aggregate: 11,
                arrangement: 4,
                ..ProjectRevisionStamp::default()
            },
            RenderSpan::new(-128, 16_384).unwrap(),
            recipe(),
            vec![RenderDependencyStamp {
                key: RenderDependencyKey::MediaAsset(5),
                content: digest(2),
                runtime_generation: dependency_generation,
            }],
        )
        .unwrap()
    }

    #[test]
    fn identity_is_structural_and_includes_runtime_dependencies() {
        assert_eq!(plan(8), plan(8));
        assert_ne!(plan(8), plan(9));
    }

    #[test]
    fn dependencies_are_canonicalized_and_duplicates_are_rejected() {
        let left = RenderDependencyStamp {
            key: RenderDependencyKey::MediaAsset(2),
            content: digest(2),
            runtime_generation: 0,
        };
        let right = RenderDependencyStamp {
            key: RenderDependencyKey::MediaAsset(1),
            content: digest(1),
            runtime_generation: 0,
        };
        let id = RenderPlanId::new(
            1,
            digest(7),
            ProjectRevisionStamp::default(),
            RenderSpan::new(0, 1).unwrap(),
            recipe(),
            vec![left.clone(), right.clone()],
        )
        .unwrap();
        assert_eq!(id.dependencies(), &[right, left.clone()]);

        assert!(matches!(
            RenderPlanId::new(
                1,
                digest(7),
                ProjectRevisionStamp::default(),
                RenderSpan::new(0, 1).unwrap(),
                recipe(),
                vec![left.clone(), left],
            ),
            Err(RenderPlanError::DuplicateDependency(_))
        ));
    }

    #[test]
    fn signed_spans_retain_negative_preroll() {
        let span = RenderSpan::new(-64, 64).unwrap();
        assert_eq!(span.len(), 128);
        assert!(span.contains(-1));
        assert!(!span.contains(64));
    }
}
