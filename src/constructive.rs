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

use crate::arrangement::{self, ArrangementEditor, Frame, FrameRange, TrackKind};
use crate::daw_project::{
    BridgeError, DawProject, PreparedProjectTransaction, ProjectDomain, ProjectState,
};
use crate::mixer::{BusId, BusKind};
use crate::pattern_lang::{self, EvalContext, TermHash};
use crate::reconstruction::ReconstructionProposalId;
use crate::sample_kit::{KitId, PadId, SampleKit, SampleKitPut, SampleTargetRef, ZoneId};
use crate::sample_material::ReusePolicy;
use crate::sample_material::{CanonicalPcmIdentity, SourceMaterialRef, VirtualSliceRef};
use crate::sequencer::{
    BeatDuration, BeatTime, PatternClip, PatternContent, PatternDefinition, PatternId,
    PatternOrigin, SequencerCommand, StepEvent, StepLane, StepLaneId, StepPattern, TriggerTarget,
};

#[allow(unused_imports)]
pub use crate::sample_material::{DerivationScope, ScopedEvidenceRef, ScopedProposalRef};

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
        resolution: BeatDuration,
        expression: Option<ExpressionIntent>,
        /// True when a human changed the analyzer-authored hypothesis before
        /// publication. Evidence remains linked, but the origin is no longer
        /// represented as the untouched analyzer output.
        diverged: bool,
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
    /// Scheduler microtiming in PPQ ticks. The exact frame offset below is
    /// retained because converting between these domains may be inexact.
    pub micro_offset_ticks: i32,
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
            if zone.decoded_pcm != Some(material.decoded_pcm) {
                return Err(ConstructivePlanError::MaterialIdentityMismatch(
                    material.zone,
                ));
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

    /// Lower every constructive entry point through one aggregate transaction.
    /// Runtime PCM installation remains a separate revision-pinned adapter;
    /// this publishes all durable kit, target, pattern, placement, and route
    /// intent or none of it.
    pub fn prepare(
        self,
        project: &DawProject,
    ) -> Result<PreparedConstructiveApplication, ConstructiveApplyError> {
        self.validate()?;
        if project.revisions().aggregate != self.base_revision {
            return Err(ConstructiveApplyError::RevisionConflict {
                expected: self.base_revision,
                actual: project.revisions().aggregate,
            });
        }

        let mut candidate = project.state().clone();
        let lowered = lower_into_candidate(&mut candidate, &self)?;
        let prepared = project.prepare_transaction(
            self.label.clone(),
            self.base_revision,
            lowered.touched.clone(),
            move |state| {
                *state = candidate;
                Ok::<(), &'static str>(())
            },
        )?;
        Ok(PreparedConstructiveApplication {
            prepared,
            bindings: lowered.bindings,
            materials: self.materials,
            focus: self.focus,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstructiveApplicationBindings {
    pub kit: KitId,
    /// Pads allocated by this edit. Existing destination-kit pads are not
    /// repeated here.
    pub created_pads: Vec<PadId>,
    /// Exact pad/zone identities allocated by this edit, including zones
    /// added to an existing pad.
    pub created_zones: Vec<SampleTargetRef>,
    pub pad_samples: BTreeMap<PadId, crate::sequencer::SampleAssetId>,
    pub pattern: Option<PatternId>,
    pub arrangement_pattern: Option<arrangement::PatternId>,
    pub sequencer_clip: Option<crate::sequencer::PatternClipId>,
    pub arrangement_clip: Option<arrangement::ClipId>,
    pub arrangement_track: Option<arrangement::TrackId>,
    pub output_bus: BusId,
    /// Lane chosen for each `PlannedPattern::steps` entry, in source order.
    /// Expression-generated events are represented by the stored origin and
    /// therefore leave this vector empty.
    pub planned_step_lanes: Vec<StepLaneId>,
}

#[derive(Debug)]
pub struct PreparedConstructiveApplication {
    prepared: PreparedProjectTransaction,
    bindings: ConstructiveApplicationBindings,
    materials: Vec<PlannedMaterial>,
    focus: ConstructiveFocus,
}

impl PreparedConstructiveApplication {
    pub fn base_revision(&self) -> u64 {
        self.prepared.base_revision()
    }

    pub fn bindings(&self) -> &ConstructiveApplicationBindings {
        &self.bindings
    }

    /// Products the runtime media adapter must resolve and publish for the
    /// same committed revision before declaring the edit audible.
    pub fn materials(&self) -> &[PlannedMaterial] {
        &self.materials
    }

    pub fn focus(&self) -> ConstructiveFocus {
        self.focus
    }

    pub fn commit(
        self,
        project: &mut DawProject,
    ) -> Result<ConstructiveApplicationReceipt, ConstructiveApplyError> {
        let revision = project.commit_prepared(self.prepared)?;
        Ok(ConstructiveApplicationReceipt {
            project_revision: revision,
            bindings: self.bindings,
            materials: self.materials,
            focus: self.focus,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConstructiveApplicationReceipt {
    pub project_revision: u64,
    pub bindings: ConstructiveApplicationBindings,
    pub materials: Vec<PlannedMaterial>,
    pub focus: ConstructiveFocus,
}

struct LoweredConstructivePlan {
    touched: BTreeSet<ProjectDomain>,
    bindings: ConstructiveApplicationBindings,
}

fn lower_into_candidate(
    state: &mut ProjectState,
    plan: &ConstructiveEditPlan,
) -> Result<LoweredConstructivePlan, ConstructiveApplyError> {
    let output_bus = ensure_output_bus(state, &plan.kit.after)?;
    state
        .domains
        .sample_kits
        .apply_puts(&[plan.kit.as_put()])
        .map_err(domain)?;

    let mut touched = BTreeSet::from([ProjectDomain::SampleKits]);
    if output_bus.created {
        touched.insert(ProjectDomain::Mixer);
    }
    let mut result = ConstructiveApplicationBindings {
        kit: plan.kit.after.id,
        created_pads: plan
            .kit
            .after
            .pads
            .keys()
            .copied()
            .filter(|pad| {
                plan.kit
                    .before
                    .as_ref()
                    .is_none_or(|before| !before.pads.contains_key(pad))
            })
            .collect(),
        created_zones: plan
            .kit
            .after
            .zones
            .values()
            .filter(|zone| {
                plan.kit
                    .before
                    .as_ref()
                    .is_none_or(|before| !before.zones.contains_key(&zone.id))
            })
            .map(|zone| SampleTargetRef {
                kit: plan.kit.after.id,
                pad: zone.pad,
                zone: zone.id,
            })
            .collect(),
        pad_samples: BTreeMap::new(),
        pattern: None,
        arrangement_pattern: None,
        sequencer_clip: None,
        arrangement_clip: None,
        arrangement_track: None,
        output_bus: output_bus.id,
        planned_step_lanes: Vec::new(),
    };

    let Some(pattern_plan) = &plan.pattern else {
        return Ok(LoweredConstructivePlan {
            touched,
            bindings: result,
        });
    };

    let (trigger_bindings, pad_samples) =
        bind_pattern_targets(state, &plan.kit.after, pattern_plan)?;
    result.pad_samples = pad_samples;
    let pattern_id = state.domains.sequencer.allocate_pattern_id();
    let (steps, origin, planned_step_lanes) =
        lower_pattern(state, pattern_plan, &trigger_bindings)?;
    result.planned_step_lanes = planned_step_lanes;
    let definition = PatternDefinition {
        id: pattern_id,
        name: pattern_plan.name.clone(),
        length: pattern_plan.cycle,
        content: PatternContent::Steps(steps),
        origin,
        revision: 1,
    };
    state
        .domains
        .sequencer
        .execute(
            plan.label.clone(),
            vec![SequencerCommand::PutPattern {
                before: None,
                after: Some(definition),
            }],
        )
        .map_err(domain)?;
    let arrangement_pattern = state
        .bindings
        .bind_pattern_definition(pattern_id)
        .map_err(domain)?;
    result.pattern = Some(pattern_id);
    result.arrangement_pattern = Some(arrangement_pattern);
    touched.extend([ProjectDomain::Sequencer, ProjectDomain::Bindings]);

    if let Some(placement) = &plan.placement {
        lower_placement(
            state,
            placement,
            pattern_plan,
            pattern_id,
            arrangement_pattern,
            output_bus.id,
            &mut result,
            &mut touched,
        )?;
    }

    Ok(LoweredConstructivePlan {
        touched,
        bindings: result,
    })
}

/// Aggregate-internal seam for a larger transaction, notably reconstruction
/// application, that already owns a cloned candidate. This is the same lowerer
/// used by [`ConstructiveEditPlan::prepare`]; it performs no publication.
pub(crate) fn apply_to_project_state(
    state: &mut ProjectState,
    plan: &ConstructiveEditPlan,
) -> Result<ConstructiveApplicationBindings, ConstructiveApplyError> {
    plan.validate()?;
    lower_into_candidate(state, plan).map(|lowered| lowered.bindings)
}

struct EnsuredBus {
    id: BusId,
    created: bool,
}

fn ensure_output_bus(
    state: &mut ProjectState,
    kit: &SampleKit,
) -> Result<EnsuredBus, ConstructiveApplyError> {
    if state.domains.mixer.bus(kit.output.bus).is_some() {
        return Ok(EnsuredBus {
            id: kit.output.bus,
            created: false,
        });
    }
    let created = state
        .domains
        .mixer
        .add_bus(BusKind::Source, kit.name.clone())
        .map_err(domain)?;
    if created != kit.output.bus {
        return Err(ConstructiveApplyError::ClaimedBusMismatch {
            claimed: kit.output.bus,
            allocated: created,
        });
    }
    Ok(EnsuredBus {
        id: created,
        created: true,
    })
}

fn bind_pattern_targets(
    state: &mut ProjectState,
    kit: &SampleKit,
    pattern: &PlannedPattern,
) -> Result<
    (
        BTreeMap<String, TriggerTarget>,
        BTreeMap<PadId, crate::sequencer::SampleAssetId>,
    ),
    ConstructiveApplyError,
> {
    let mut bindings = BTreeMap::new();
    let mut pad_samples = BTreeMap::new();
    for (name, pad) in &pattern.bindings {
        let target = kit
            .primary_target(*pad)
            .ok_or(ConstructiveApplyError::PadHasNoZone(*pad))?;
        let alias = state.bindings.bind_sample_target(target).map_err(domain)?;
        bindings.insert(name.clone(), TriggerTarget::Sample(alias));
        pad_samples.insert(*pad, alias);
    }
    Ok((bindings, pad_samples))
}

fn lower_pattern(
    state: &mut ProjectState,
    pattern: &PlannedPattern,
    bindings: &BTreeMap<String, TriggerTarget>,
) -> Result<(StepPattern, PatternOrigin, Vec<StepLaneId>), ConstructiveApplyError> {
    match &pattern.seed {
        PatternSeed::Expression(intent) => {
            let expression = pattern_lang::parse(&intent.source)
                .map_err(|error| ConstructiveApplyError::PatternLanguage(error.to_string()))?;
            if pattern_lang::term_hash(&expression) != intent.term_hash {
                return Err(ConstructiveApplyError::TermHashMismatch);
            }
            let cycle_index = match intent.cycle_index {
                CycleIndexPolicy::PlacementCycle => 0,
                CycleIndexPolicy::Fixed(index) => index,
            };
            let output = pattern_lang::eval_steps(
                &expression,
                &EvalContext {
                    bindings,
                    cycle: pattern.cycle,
                    seed: intent.seed,
                    cycle_index,
                },
            )
            .map_err(|error| ConstructiveApplyError::PatternLanguage(error.to_string()))?;
            let steps = remap_expression_lanes(state, output.pattern);
            let origin = PatternOrigin::Expression {
                source: intent.source.clone(),
                term_hash: intent.term_hash,
                bindings_hash: pattern_lang::bindings_hash(bindings),
                bindings: bindings.clone(),
                diverged: false,
            };
            Ok((steps, origin, Vec::new()))
        }
        PatternSeed::EmptyGrid { resolution } => {
            let (steps, planned_step_lanes) =
                lower_planned_steps(state, pattern, bindings, *resolution)?;
            Ok((steps, PatternOrigin::Authored, planned_step_lanes))
        }
        PatternSeed::Deprojected {
            proposal,
            resolution,
            expression: _,
            diverged,
        } => {
            let (steps, planned_step_lanes) =
                lower_planned_steps(state, pattern, bindings, *resolution)?;
            Ok((
                steps,
                PatternOrigin::Deprojected {
                    proposal: ReconstructionProposalId::from_raw(proposal.local),
                    diverged: *diverged,
                },
                planned_step_lanes,
            ))
        }
    }
}

fn remap_expression_lanes(state: &mut ProjectState, pattern: StepPattern) -> StepPattern {
    let lanes = pattern
        .lanes
        .into_values()
        .map(|mut lane| {
            let id = state.domains.sequencer.allocate_step_lane_id();
            lane.id = id;
            (id, lane)
        })
        .collect();
    StepPattern {
        resolution: pattern.resolution,
        swing: pattern.swing,
        lanes,
    }
}

fn lower_planned_steps(
    state: &mut ProjectState,
    pattern: &PlannedPattern,
    bindings: &BTreeMap<String, TriggerTarget>,
    resolution: BeatDuration,
) -> Result<(StepPattern, Vec<StepLaneId>), ConstructiveApplyError> {
    let pad_targets: BTreeMap<_, _> = pattern
        .bindings
        .iter()
        .filter_map(|(name, pad)| bindings.get(name).cloned().map(|target| (*pad, target)))
        .collect();
    let mut lanes = BTreeMap::<StepLaneId, StepLane>::new();
    let mut pad_lanes = BTreeMap::<PadId, Vec<StepLaneId>>::new();
    let mut planned_step_lanes = Vec::with_capacity(pattern.steps.len());

    for step in &pattern.steps {
        let index = u32::try_from(step.at.0 / resolution.0 as i64)
            .map_err(|_| ConstructiveApplyError::StepIndexOverflow)?;
        let residue = step.at.0 % resolution.0 as i64;
        let micro_offset = i32::try_from(residue)
            .ok()
            .and_then(|residue| residue.checked_add(step.micro_offset_ticks))
            .ok_or(ConstructiveApplyError::StepMicrotimingOverflow)?;
        let lane_id = pad_lanes
            .get(&step.pad)
            .and_then(|ids| {
                ids.iter()
                    .copied()
                    .find(|id| !lanes[id].steps.contains_key(&index))
            })
            .unwrap_or_else(|| {
                let id = state.domains.sequencer.allocate_step_lane_id();
                let target = pad_targets[&step.pad].clone();
                lanes.insert(
                    id,
                    StepLane {
                        id,
                        name: format!("pad {}", step.pad.get()),
                        target,
                        choke_group: None,
                        steps: BTreeMap::new(),
                    },
                );
                pad_lanes.entry(step.pad).or_default().push(id);
                id
            });
        lanes
            .get_mut(&lane_id)
            .expect("lane allocated above")
            .steps
            .insert(
                index,
                StepEvent {
                    velocity: step.velocity,
                    probability: step.probability,
                    micro_offset,
                    gate: step.gate,
                    ratchets: step.ratchets,
                    pitch_semitones: step.pitch_semitones,
                    pan: step.pan,
                },
            );
        planned_step_lanes.push(lane_id);
    }
    // An empty grid still exposes one editable lane per binding.
    for (pad, target) in pad_targets {
        if !pad_lanes.contains_key(&pad) {
            let id = state.domains.sequencer.allocate_step_lane_id();
            lanes.insert(
                id,
                StepLane {
                    id,
                    name: format!("pad {}", pad.get()),
                    target,
                    choke_group: None,
                    steps: BTreeMap::new(),
                },
            );
        }
    }
    Ok((
        StepPattern {
            resolution,
            swing: 0.0,
            lanes,
        },
        planned_step_lanes,
    ))
}

#[allow(clippy::too_many_arguments)]
fn lower_placement(
    state: &mut ProjectState,
    placement: &PatternPlacementIntent,
    pattern_plan: &PlannedPattern,
    pattern_id: PatternId,
    arrangement_pattern: arrangement::PatternId,
    output_bus: BusId,
    result: &mut ConstructiveApplicationBindings,
    touched: &mut BTreeSet<ProjectDomain>,
) -> Result<(), ConstructiveApplyError> {
    let track = pattern_track_for_bus(state, output_bus)?;
    let clip_id = state.domains.sequencer.allocate_clip_id();
    let sequencer_clip = PatternClip {
        id: clip_id,
        pattern: pattern_id,
        start: placement.start,
        length: placement.length,
        pattern_offset: placement.pattern_offset,
        looped: placement.looped,
        transpose_semitones: placement.transpose_semitones,
        gain: placement.gain,
        muted: false,
    };
    state
        .domains
        .sequencer
        .execute(
            format!("Place {}", pattern_plan.name),
            vec![SequencerCommand::PutClip {
                before: None,
                after: Some(sequencer_clip),
            }],
        )
        .map_err(domain)?;

    let tempo = state.domains.sequencer.tempo_map();
    let start = tempo.beat_to_frame(placement.start).0;
    let end = tempo
        .beat_to_frame(BeatTime(
            placement
                .start
                .0
                .saturating_add(placement.length.0.min(i64::MAX as u64) as i64),
        ))
        .0;
    let mut editor =
        ArrangementEditor::from_state(state.domains.arrangement.clone()).map_err(domain)?;
    let arrangement_clip = editor
        .create_pattern_clip(
            track,
            pattern_plan.name.clone(),
            FrameRange::new(Frame(start), Frame(end)).map_err(domain)?,
            arrangement_pattern,
        )
        .map_err(domain)?;
    state.domains.arrangement = editor.state().clone();
    let clip = state
        .domains
        .arrangement
        .clips
        .get_mut(&arrangement_clip)
        .expect("arrangement editor returned its clip");
    if let arrangement::ClipContent::Pattern(region) = &mut clip.content {
        region.content_offset_frames =
            tempo.beat_to_frame(placement.pattern_offset).0.max(0) as u64;
        region.looped = placement.looped;
    }
    state
        .bindings
        .patterns
        .placements
        .insert(arrangement_clip, clip_id);
    state.domains.arrangement.validate().map_err(domain)?;
    result.sequencer_clip = Some(clip_id);
    result.arrangement_clip = Some(arrangement_clip);
    result.arrangement_track = Some(track);
    touched.extend([
        ProjectDomain::Arrangement,
        ProjectDomain::Sequencer,
        ProjectDomain::Bindings,
    ]);
    Ok(())
}

fn pattern_track_for_bus(
    state: &mut ProjectState,
    output_bus: BusId,
) -> Result<arrangement::TrackId, ConstructiveApplyError> {
    if let Some((track_id, _)) = state
        .bindings
        .mixer
        .tracks
        .iter()
        .find(|(_, bus)| **bus == output_bus)
    {
        let track = state
            .domains
            .arrangement
            .track(*track_id)
            .ok_or(ConstructiveApplyError::MissingBoundTrack(*track_id))?;
        return match track.kind {
            TrackKind::Pattern | TrackKind::Hybrid => Ok(*track_id),
            _ => Err(ConstructiveApplyError::OutputBusOwnedByNonPatternTrack(
                output_bus,
            )),
        };
    }
    let mut editor =
        ArrangementEditor::from_state(state.domains.arrangement.clone()).map_err(domain)?;
    let track = editor
        .create_track("Pads", TrackKind::Pattern)
        .map_err(domain)?;
    state.domains.arrangement = editor.state().clone();
    state.bindings.mixer.tracks.insert(track, output_bus);
    Ok(track)
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
    if matches!(
        pattern.seed,
        PatternSeed::Deprojected {
            resolution: BeatDuration(0),
            ..
        }
    ) {
        return Err(ConstructivePlanError::InvalidPattern(pattern.id));
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
    MaterialIdentityMismatch(ZoneId),
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
            Self::MaterialIdentityMismatch(zone) => write!(
                formatter,
                "material plan does not match decoded identity for zone {}",
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

#[derive(Debug)]
pub enum ConstructiveApplyError {
    Plan(ConstructivePlanError),
    Bridge(BridgeError),
    RevisionConflict { expected: u64, actual: u64 },
    ClaimedBusMismatch { claimed: BusId, allocated: BusId },
    PadHasNoZone(PadId),
    MissingBoundTrack(arrangement::TrackId),
    OutputBusOwnedByNonPatternTrack(BusId),
    PatternLanguage(String),
    TermHashMismatch,
    StepIndexOverflow,
    StepMicrotimingOverflow,
    Domain(String),
}

impl fmt::Display for ConstructiveApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plan(error) => error.fmt(formatter),
            Self::Bridge(error) => error.fmt(formatter),
            Self::RevisionConflict { expected, actual } => write!(
                formatter,
                "constructive edit expected project revision {expected}, found {actual}"
            ),
            Self::ClaimedBusMismatch { claimed, allocated } => write!(
                formatter,
                "constructive edit claimed mixer bus {}, allocator produced {}",
                claimed.get(),
                allocated.get()
            ),
            Self::PadHasNoZone(pad) => {
                write!(formatter, "sample pad {} has no playable zone", pad.get())
            }
            Self::MissingBoundTrack(track) => {
                write!(formatter, "mixer binding names missing track {track}")
            }
            Self::OutputBusOwnedByNonPatternTrack(bus) => write!(
                formatter,
                "sample-kit output bus {} is owned by a non-pattern track",
                bus.get()
            ),
            Self::PatternLanguage(message) => {
                write!(formatter, "pattern expression cannot be lowered: {message}")
            }
            Self::TermHashMismatch => {
                formatter.write_str("pattern expression does not match its term hash")
            }
            Self::StepIndexOverflow => formatter.write_str("planned step index overflows u32"),
            Self::StepMicrotimingOverflow => {
                formatter.write_str("planned step microtiming overflows i32")
            }
            Self::Domain(message) => formatter.write_str(message),
        }
    }
}

impl Error for ConstructiveApplyError {}

impl From<ConstructivePlanError> for ConstructiveApplyError {
    fn from(value: ConstructivePlanError) -> Self {
        Self::Plan(value)
    }
}

impl From<BridgeError> for ConstructiveApplyError {
    fn from(value: BridgeError) -> Self {
        Self::Bridge(value)
    }
}

fn domain(error: impl fmt::Display) -> ConstructiveApplyError {
    ConstructiveApplyError::Domain(error.to_string())
}
