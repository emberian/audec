//! Pure plans for turning selected or inferred material into playable project
//! objects.
//!
//! A [`ConstructiveEditPlan`] is deliberately not an editor command and does
//! not acquire project locks.  It is a complete, validated description of one
//! user-meaningful edit which a bridge can lower into a `CommandEnvelope`, ID
//! claims, and runtime PCM materializations atomically.  Manual sampling,
//! onset chopping, notation, and deprojection share this representation so
//! none of those entry points owns a private pattern or sampler model.
//!
//! This module does not claim that an analysis family names an instrument, or
//! that a virtual slice is a newly recorded/file-backed asset.  Consolidation
//! is an explicit later operation in `sample_material`.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::pattern_lang::TermHash;
use crate::sample_kit::{PadId, SampleKit, SampleKitPut, ZoneId};
use crate::sample_material::ReusePolicy;
use crate::sample_material::{CanonicalPcmIdentity, SourceMaterialRef, VirtualSliceRef};
use crate::sequencer::{BeatDuration, BeatTime};

/// Version of the pure planning value, independent of the project-file schema.
pub const CONSTRUCTIVE_PLAN_SCHEMA_VERSION: u32 = 1;

macro_rules! typed_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u64);

        impl $name {
            pub const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

typed_id!(PlannedPatternId);

/// Namespace for analyzer-local proposal/evidence IDs.
///
/// Local reconstruction IDs restart for every analysis run; pairing them with
/// a stable run fingerprint prevents a later codec from mistaking two runs'
/// `proposal 1` for the same derivation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DerivationScope(pub u128);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScopedEvidenceRef {
    pub scope: DerivationScope,
    pub local: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScopedProposalRef {
    pub scope: DerivationScope,
    pub local: u64,
}

/// Why this plan exists. Several causes can coexist: for example a rhythm
/// proposal may supply slices while a human-authored expression supplies the
/// pattern.
#[derive(Clone, Debug, PartialEq)]
pub enum ConstructiveCause {
    ManualSelection {
        material: VirtualSliceRef,
    },
    OnsetChop {
        material: VirtualSliceRef,
        analyzer: String,
        evidence: Vec<ScopedEvidenceRef>,
    },
    Notation {
        source: String,
        term_hash: TermHash,
    },
    Deprojection {
        proposal: ScopedProposalRef,
        evidence: Vec<ScopedEvidenceRef>,
    },
}

/// Whether an adapter should materialize a new runtime product or may reuse a
/// previously materialized one after exact canonical PCM comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaterialReusePolicy {
    RequireNew,
    /// A bridge may reuse only after `sample_material::find_verified_reuse`
    /// proves the requested provenance/content policy.
    ReuseIfExactlyVerified(ReusePolicy),
}

/// One virtual zone's expected decoded PCM product.
///
/// The canonical identity is only a fast/content-address key. Reuse still
/// requires the exact comparison implemented by `sample_material`; a bridge
/// must not treat the non-cryptographic registry fingerprint as proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedMaterial {
    pub zone: ZoneId,
    pub slice: VirtualSliceRef,
    pub decoded_pcm: CanonicalPcmIdentity,
    pub reuse: MaterialReusePolicy,
}

/// A put-style kit edit. `before: None` creates the kit. Deleting kits is not
/// part of constructive creation; the inverse command produced by the bridge
/// may naturally use `after: None`.
#[derive(Clone, Debug, PartialEq)]
pub struct KitMutation {
    pub before: Option<SampleKit>,
    pub after: SampleKit,
}

impl KitMutation {
    pub fn as_put(&self) -> SampleKitPut {
        SampleKitPut {
            before: self.before.clone(),
            after: Some(self.after.clone()),
        }
    }
}

/// The cycle-index contract for a generated expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CycleIndexPolicy {
    /// Evaluate with the actual zero-based repetition index of each canonical
    /// pattern placement. This is the production default.
    PlacementCycle,
    /// Freeze a particular cycle only for an explicit rendered/committed
    /// variation. This must never be chosen implicitly by an editor.
    Fixed(u64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExpressionIntent {
    pub source: String,
    pub term_hash: TermHash,
    pub seed: u64,
    pub cycle_index: CycleIndexPolicy,
}

/// How the initial pattern body is obtained.
#[derive(Clone, Debug, PartialEq)]
pub enum PatternSeed {
    /// Empty lanes ready for direct grid performance/editing.
    EmptyGrid { resolution: BeatDuration },
    /// Pure notation evaluated after symbolic pad bindings have been resolved
    /// to concrete sequencer targets.
    Expression(ExpressionIntent),
    /// Evidence-derived placements. An optional expression is the compact
    /// explanation of those placements; it does not erase the exact events.
    Deprojected {
        proposal: ScopedProposalRef,
        expression: Option<ExpressionIntent>,
    },
}

/// A step before lowering into the sequencer grid. Tick timing supports normal
/// editing; the original frame offset remains alongside it for honest
/// reconstruction receipts and later re-quantization.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedStep {
    pub pad: PadId,
    pub at: BeatTime,
    pub gate: BeatDuration,
    pub velocity: f32,
    pub probability: f32,
    pub ratchets: u8,
    pub pitch_semitones: f32,
    pub pan: f32,
    pub original_micro_offset_frames: Option<i64>,
    pub exact_source_onset_frame: Option<u64>,
    pub evidence: Vec<ScopedEvidenceRef>,
}

/// Pattern names bind to persisted pads, not transient lane indices or guessed
/// instrument labels. The bridge chooses stable `StepLaneId`s and concrete
/// trigger aliases when the plan is applied.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedPattern {
    pub id: PlannedPatternId,
    pub name: String,
    pub cycle: BeatDuration,
    pub seed: PatternSeed,
    pub bindings: BTreeMap<String, PadId>,
    pub steps: Vec<PlannedStep>,
}

/// One canonical arrangement/sequencer placement. Frame coordinates are not
/// duplicated here; the application adapter derives them from the project's
/// real tempo map and then authors both linked representations together.
#[derive(Clone, Debug, PartialEq)]
pub struct PatternPlacementIntent {
    pub pattern: PlannedPatternId,
    pub start: BeatTime,
    pub length: BeatDuration,
    pub pattern_offset: BeatTime,
    pub looped: bool,
    pub transpose_semitones: f32,
    pub gain: f32,
}

/// The UI target to reveal after successful publication. It is a hint, not a
/// mutation and therefore does not participate in project undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstructiveFocus {
    Kit,
    Pad(PadId),
    Pattern(PlannedPatternId),
}

/// One atomic constructive edit, ready for a project-specific adapter.
#[derive(Clone, Debug, PartialEq)]
pub struct ConstructiveEditPlan {
    pub schema_version: u32,
    pub label: String,
    pub base_revision: u64,
    pub causes: Vec<ConstructiveCause>,
    pub materials: Vec<PlannedMaterial>,
    pub kit: KitMutation,
    pub pattern: Option<PlannedPattern>,
    pub placement: Option<PatternPlacementIntent>,
    pub focus: ConstructiveFocus,
}

impl ConstructiveEditPlan {
    pub fn new(
        label: impl Into<String>,
        base_revision: u64,
        causes: Vec<ConstructiveCause>,
        materials: Vec<PlannedMaterial>,
        kit: KitMutation,
        pattern: Option<PlannedPattern>,
        placement: Option<PatternPlacementIntent>,
        focus: ConstructiveFocus,
    ) -> Result<Self, ConstructivePlanError> {
        let plan = Self {
            schema_version: CONSTRUCTIVE_PLAN_SCHEMA_VERSION,
            label: label.into(),
            base_revision,
            causes,
            materials,
            kit,
            pattern,
            placement,
            focus,
        };
        plan.validate()?;
        Ok(plan)
    }

    pub fn validate(&self) -> Result<(), ConstructivePlanError> {
        if self.schema_version != CONSTRUCTIVE_PLAN_SCHEMA_VERSION {
            return Err(ConstructivePlanError::UnsupportedSchema(
                self.schema_version,
            ));
        }
        if self.label.trim().is_empty() {
            return Err(ConstructivePlanError::EmptyLabel);
        }
        if self.causes.is_empty() {
            return Err(ConstructivePlanError::MissingCause);
        }

        self.kit
            .after
            .validate()
            .map_err(|_| ConstructivePlanError::InvalidKitMutation(self.kit.after.id.get()))?;
        if self
            .kit
            .before
            .as_ref()
            .is_some_and(|before| before.id != self.kit.after.id)
        {
            return Err(ConstructivePlanError::InvalidKitMutation(
                self.kit.after.id.get(),
            ));
        }

        let mut zones = BTreeSet::new();
        for material in &self.materials {
            if !zones.insert(material.zone) {
                return Err(ConstructivePlanError::DuplicateMaterialZone(material.zone));
            }
            let Some(zone) = self.kit.after.zones.get(&material.zone) else {
                return Err(ConstructivePlanError::UnknownMaterialZone(material.zone));
            };
            if zone.material != SourceMaterialRef::VirtualSlice(material.slice) {
                return Err(ConstructivePlanError::MaterialSourceMismatch(material.zone));
            }
            if material.decoded_pcm.frame_count != material.slice.frame_count() {
                return Err(ConstructivePlanError::MaterialFrameCountMismatch(
                    material.zone,
                ));
            }
        }

        if let Some(pattern) = &self.pattern {
            validate_pattern(pattern, &self.kit.after)?;
        }
        if let Some(placement) = &self.placement {
            let Some(pattern) = &self.pattern else {
                return Err(ConstructivePlanError::PlacementWithoutPattern);
            };
            if placement.pattern != pattern.id {
                return Err(ConstructivePlanError::PlacementPatternMismatch {
                    expected: pattern.id,
                    actual: placement.pattern,
                });
            }
            if placement.length.0 == 0
                || placement.pattern_offset.0 < 0
                || !placement.transpose_semitones.is_finite()
                || !placement.gain.is_finite()
                || placement.gain < 0.0
            {
                return Err(ConstructivePlanError::InvalidPlacement);
            }
        }

        match self.focus {
            ConstructiveFocus::Kit => {}
            ConstructiveFocus::Pad(id) if !self.kit.after.pads.contains_key(&id) => {
                return Err(ConstructivePlanError::UnknownFocusPad(id));
            }
            ConstructiveFocus::Pattern(id)
                if self.pattern.as_ref().map(|pattern| pattern.id) != Some(id) =>
            {
                return Err(ConstructivePlanError::UnknownFocusPattern(id));
            }
            _ => {}
        }
        Ok(())
    }

    pub fn affected_zones(&self) -> BTreeSet<ZoneId> {
        self.materials
            .iter()
            .map(|material| material.zone)
            .collect()
    }
}

fn validate_pattern(
    pattern: &PlannedPattern,
    kit: &SampleKit,
) -> Result<(), ConstructivePlanError> {
    if pattern.id.get() == 0
        || pattern.name.trim().is_empty()
        || pattern.cycle.0 == 0
        || pattern.cycle.0 > i64::MAX as u64
    {
        return Err(ConstructivePlanError::InvalidPattern(pattern.id));
    }
    if let PatternSeed::EmptyGrid { resolution } = &pattern.seed {
        if resolution.0 == 0 {
            return Err(ConstructivePlanError::InvalidPattern(pattern.id));
        }
    }
    let expression = match &pattern.seed {
        PatternSeed::Expression(expression) => Some(expression),
        PatternSeed::Deprojected { expression, .. } => expression.as_ref(),
        PatternSeed::EmptyGrid { .. } => None,
    };
    if expression.is_some_and(|expression| expression.source.trim().is_empty()) {
        return Err(ConstructivePlanError::EmptyExpression(pattern.id));
    }
    if pattern
        .bindings
        .keys()
        .any(|binding| binding.trim().is_empty())
    {
        return Err(ConstructivePlanError::EmptyBinding(pattern.id));
    }
    if pattern
        .bindings
        .values()
        .any(|pad| !kit.pads.contains_key(pad))
    {
        return Err(ConstructivePlanError::UnknownPatternPad(pattern.id));
    }
    let bound_pads: BTreeSet<_> = pattern.bindings.values().copied().collect();
    for step in &pattern.steps {
        if !bound_pads.contains(&step.pad) {
            return Err(ConstructivePlanError::UnboundStepPad {
                pattern: pattern.id,
                pad: step.pad,
            });
        }
        if step.at.0 < 0
            || step.at.0 >= pattern.cycle.0.min(i64::MAX as u64) as i64
            || step.gate.0 == 0
            || !unit(step.velocity)
            || !unit(step.probability)
            || step.ratchets == 0
            || !step.pitch_semitones.is_finite()
            || !step.pan.is_finite()
            || !(-1.0..=1.0).contains(&step.pan)
        {
            return Err(ConstructivePlanError::InvalidStep {
                pattern: pattern.id,
                at: step.at,
            });
        }
    }
    Ok(())
}

fn unit(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConstructivePlanError {
    UnsupportedSchema(u32),
    EmptyLabel,
    MissingCause,
    InvalidKitMutation(u64),
    DuplicateMaterialZone(ZoneId),
    UnknownMaterialZone(ZoneId),
    MaterialSourceMismatch(ZoneId),
    MaterialFrameCountMismatch(ZoneId),
    InvalidPattern(PlannedPatternId),
    EmptyExpression(PlannedPatternId),
    EmptyBinding(PlannedPatternId),
    UnboundStepPad {
        pattern: PlannedPatternId,
        pad: PadId,
    },
    InvalidStep {
        pattern: PlannedPatternId,
        at: BeatTime,
    },
    PlacementWithoutPattern,
    PlacementPatternMismatch {
        expected: PlannedPatternId,
        actual: PlannedPatternId,
    },
    InvalidPlacement,
    UnknownFocusPad(PadId),
    UnknownPatternPad(PlannedPatternId),
    UnknownFocusPattern(PlannedPatternId),
}

impl fmt::Display for ConstructivePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported constructive-plan schema {version}")
            }
            Self::EmptyLabel => formatter.write_str("constructive plan label is empty"),
            Self::MissingCause => formatter.write_str("constructive plan has no provenance cause"),
            Self::InvalidKitMutation(kit) => {
                write!(
                    formatter,
                    "constructive plan has an invalid kit mutation for {kit}"
                )
            }
            Self::DuplicateMaterialZone(zone) => {
                write!(
                    formatter,
                    "zone {} has more than one material plan",
                    zone.get()
                )
            }
            Self::UnknownMaterialZone(zone) => {
                write!(formatter, "material plan names unknown zone {}", zone.get())
            }
            Self::MaterialSourceMismatch(zone) => write!(
                formatter,
                "material plan does not match virtual source for zone {}",
                zone.get()
            ),
            Self::MaterialFrameCountMismatch(zone) => write!(
                formatter,
                "material plan frame count does not match zone {} source range",
                zone.get()
            ),
            Self::InvalidPattern(pattern) => {
                write!(formatter, "planned pattern {} is invalid", pattern.get())
            }
            Self::EmptyExpression(pattern) => {
                write!(
                    formatter,
                    "planned pattern {} has an empty expression",
                    pattern.get()
                )
            }
            Self::EmptyBinding(pattern) => {
                write!(
                    formatter,
                    "planned pattern {} has an empty binding name",
                    pattern.get()
                )
            }
            Self::UnboundStepPad { pattern, pad } => write!(
                formatter,
                "planned pattern {} uses unbound pad {}",
                pattern.get(),
                pad.get()
            ),
            Self::InvalidStep { pattern, at } => write!(
                formatter,
                "planned pattern {} has an invalid step at tick {}",
                pattern.get(),
                at.0
            ),
            Self::PlacementWithoutPattern => {
                formatter.write_str("pattern placement exists without a planned pattern")
            }
            Self::PlacementPatternMismatch { expected, actual } => write!(
                formatter,
                "placement names pattern {}, expected {}",
                actual.get(),
                expected.get()
            ),
            Self::InvalidPlacement => formatter.write_str("pattern placement is invalid"),
            Self::UnknownFocusPad(pad) => {
                write!(formatter, "focus names unknown pad {}", pad.get())
            }
            Self::UnknownPatternPad(pattern) => write!(
                formatter,
                "planned pattern {} binds a pad outside its kit",
                pattern.get()
            ),
            Self::UnknownFocusPattern(pattern) => write!(
                formatter,
                "focus names unknown planned pattern {}",
                pattern.get()
            ),
        }
    }
}

impl Error for ConstructivePlanError {}
