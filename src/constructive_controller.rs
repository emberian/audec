//! Constructive sampling adapter for [`ProjectController`](crate::live_project::ProjectController).
//!
//! Planning runs entirely against one immutable controller snapshot. The
//! resulting domain puts and exact PCM cohort are then submitted through the
//! ordinary aggregate command path, so publication, journaling and undo have
//! one revision boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::arrangement::{self, ArrangementOperation};
use crate::assets::{self, AssetFrameRange, AssetUsageOwner, SampleFrames};
use crate::command::{
    claims_for_commands, AssetCommand, BindingCommand, CommandEnvelope, DomainCommand,
};
use crate::constructive::{
    self, ConstructiveApplicationBindings, ConstructiveCause, ConstructiveEditPlan,
    ConstructiveFocus, KitMutation, MaterialReusePolicy, PatternPlacementIntent, PatternSeed,
    PlannedMaterial, PlannedPattern, PlannedPatternId, PlannedStep,
};
use crate::daw_project::{ProjectBindings, ProjectState};
use crate::daw_render::PcmAsset;
use crate::live_project::{
    LiveProjectSnapshot, ProjectController, ProjectControllerError, ProjectControllerUpdate,
};
use crate::mixer::{BusKind, MixerCommand};
use crate::sample_actions::{
    ChopPreviewIntent, MakeBeatIntent, OnsetChopPreview, SampleAction, SampleActionExecutionClass,
    SampleActionRequest, SampleChopIntent, SampleInspectTarget, SampleKitDestination,
    SampleRequestId, SampleSelection, SamplerViewDisposition, SamplerWorkspaceIntent,
    ZoneEditIntent,
};
use crate::sample_kit::{
    KitId, PadId, SampleKit, SampleKitPut, SamplePad, SampleRouteIntent, SampleTargetRef,
    SampleZone, ZoneId,
};
use crate::sample_material::{
    extract_virtual_slice, CanonicalPcmIdentity, SampleMaterialProvenance, SourceMaterialRef,
    VirtualSliceRef,
};
use crate::sequencer::{self, BeatDuration, BeatTime, PatternId, SequencerCommand};
use crate::ui_drag::{AssetDrag, DropIntent};

#[derive(Clone, Debug)]
pub struct ConstructiveSourceSnapshot {
    pub project_revision: u64,
    pub selection: SampleSelection,
    pub source_range: AssetFrameRange,
    pub pcm: PcmAsset,
}

/// Cheap capture performed while the session/controller is borrowed by GPUI.
/// All large project and PCM values are immutable `Arc` publications.
#[derive(Clone, Debug)]
pub struct SampleActionBackgroundWork {
    request: SampleActionRequest,
    snapshot: LiveProjectSnapshot,
}

impl SampleActionBackgroundWork {
    pub fn request_id(&self) -> SampleRequestId {
        self.request.id
    }

    pub fn prepare(self) -> Result<PreparedSampleAction, ConstructiveControllerError> {
        let request = self.request;
        let payload = match request.action.clone() {
            SampleAction::PreviewChop(intent) => PreparedSampleActionPayload::Preview(
                ProjectController::preview_chop_from_snapshot(&self.snapshot, intent)?,
            ),
            SampleAction::MakeBeat(intent) => {
                let result_focus = intent.result_focus;
                let plan = ProjectController::plan_make_beat_from_snapshot(&self.snapshot, intent)?;
                let mut commit = prepare_constructive_commit(&self.snapshot, plan)?;
                apply_make_beat_focus(&mut commit.publication, result_focus)?;
                PreparedSampleActionPayload::MakeBeat(commit)
            }
            _ => {
                return Err(ConstructiveControllerError::Internal(
                    "immediate sample action was submitted for background planning".into(),
                ))
            }
        };
        Ok(PreparedSampleAction { request, payload })
    }
}

#[derive(Debug)]
pub struct PreparedSampleAction {
    request: SampleActionRequest,
    payload: PreparedSampleActionPayload,
}

impl PreparedSampleAction {
    pub fn request_id(&self) -> SampleRequestId {
        self.request.id
    }

    pub fn action(&self) -> &SampleAction {
        &self.request.action
    }
}

#[derive(Debug)]
enum PreparedSampleActionPayload {
    Preview(OnsetChopPreview),
    MakeBeat(PreparedConstructiveCommit),
}

#[derive(Debug)]
struct PreparedConstructiveCommit {
    base_revision: u64,
    envelope: CommandEnvelope,
    materialized: BTreeMap<SampleTargetRef, PcmAsset>,
    publication: ConstructivePublication,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstructivePublication {
    pub revision: u64,
    pub kit: KitId,
    pub created_pads: Vec<PadId>,
    pub created_zones: Vec<SampleTargetRef>,
    pub pad: Option<PadId>,
    pub pattern: Option<PatternId>,
    pub sequencer_clip: Option<sequencer::PatternClipId>,
    pub arrangement_clip: Option<arrangement::ClipId>,
    pub arrangement_track: Option<arrangement::TrackId>,
    pub output_bus: Option<crate::mixer::BusId>,
    pub focus: ConstructivePublishedFocus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstructivePublishedFocus {
    Stay,
    Kit(KitId),
    Pad {
        kit: KitId,
        pad: PadId,
    },
    Pattern(PatternId),
    Arrangement(arrangement::ClipId),
    Sampler {
        kit: KitId,
        disposition: SamplerViewDisposition,
    },
}

#[derive(Clone, Debug)]
pub struct ConstructiveOutcome {
    pub publication: ConstructivePublication,
    pub update: ProjectControllerUpdate,
}

#[derive(Clone, Debug)]
pub enum SampleActionOutcome {
    Published(ConstructiveOutcome),
    Audition(crate::sample_actions::SampleAuditionIntent),
    Inspect(SampleInspectTarget),
    Preview(OnsetChopPreview),
    Workspace(SamplerWorkspaceIntent),
    /// The current aggregate does not yet persist loop/envelope playback
    /// metadata. These intents are revision-validated and remain typed for
    /// the runtime/editor owner instead of being silently discarded.
    ForwardZoneEdit(ZoneEditIntent),
    /// Non-sampler drops remain typed for the owning arrangement/mixer
    /// adapter; they are never partially interpreted here.
    ForwardDrop(DropIntent),
}

impl ProjectController {
    /// Capture immutable inputs for a heavy sample action without doing PCM
    /// analysis. The returned work value is `Send` and may be prepared on a
    /// background executor without holding session or controller authority.
    pub fn capture_sample_action_work(
        &self,
        request: SampleActionRequest,
    ) -> Result<SampleActionBackgroundWork, ConstructiveControllerError> {
        if request.action.execution_class() != SampleActionExecutionClass::BackgroundPlanning {
            return Err(ConstructiveControllerError::Internal(
                "sample action does not require background planning".into(),
            ));
        }
        Ok(SampleActionBackgroundWork {
            request,
            snapshot: self.snapshot().clone(),
        })
    }

    /// Short authoritative boundary for background-prepared sampling work.
    /// Preview results are ephemeral. Constructive commits reject a changed
    /// aggregate revision before applying their already-lowered command batch.
    pub fn commit_prepared_sample_action(
        &mut self,
        prepared: PreparedSampleAction,
    ) -> Result<SampleActionOutcome, ConstructiveControllerError> {
        match prepared.payload {
            PreparedSampleActionPayload::Preview(preview) => {
                Ok(SampleActionOutcome::Preview(preview))
            }
            PreparedSampleActionPayload::MakeBeat(commit) => self
                .commit_prepared_constructive(commit)
                .map(SampleActionOutcome::Published),
        }
    }

    /// Capture registry facts and decoded PCM from the same immutable project
    /// publication used to pin plan allocation.
    pub fn constructive_source_snapshot(
        &self,
        selection: SampleSelection,
    ) -> Result<ConstructiveSourceSnapshot, ConstructiveControllerError> {
        source_snapshot(self.snapshot(), selection)
    }

    pub fn execute_constructive_plan(
        &mut self,
        plan: ConstructiveEditPlan,
    ) -> Result<ConstructiveOutcome, ConstructiveControllerError> {
        let prepared = prepare_constructive_commit(self.snapshot(), plan)?;
        self.commit_prepared_constructive(prepared)
    }

    fn commit_prepared_constructive(
        &mut self,
        prepared: PreparedConstructiveCommit,
    ) -> Result<ConstructiveOutcome, ConstructiveControllerError> {
        let actual = self.revisions().aggregate;
        if prepared.base_revision != actual {
            return Err(ConstructiveControllerError::RevisionConflict {
                expected: prepared.base_revision,
                actual,
            });
        }
        let update = self.execute_with_sample_pcm(prepared.envelope, prepared.materialized)?;
        let mut publication = prepared.publication;
        publication.revision = update.revisions().aggregate;
        Ok(ConstructiveOutcome {
            publication,
            update,
        })
    }

    pub fn execute_sample_action(
        &mut self,
        action: SampleAction,
    ) -> Result<SampleActionOutcome, ConstructiveControllerError> {
        match action {
            SampleAction::Audition(intent) => Ok(SampleActionOutcome::Audition(intent)),
            SampleAction::Inspect(target) => Ok(SampleActionOutcome::Inspect(target)),
            SampleAction::PreviewChop(intent) => {
                self.preview_chop(intent).map(SampleActionOutcome::Preview)
            }
            SampleAction::Workspace(intent) => Ok(SampleActionOutcome::Workspace(intent)),
            SampleAction::EditZone(intent) => self.execute_zone_edit(intent),
            SampleAction::ApplyDrop(DropIntent::MapAssetToPad { source, kit, pad }) => {
                let plan = self.plan_asset_to_pad(source, kit, pad)?;
                self.execute_constructive_plan(plan)
                    .map(SampleActionOutcome::Published)
            }
            SampleAction::ApplyDrop(intent) => Ok(SampleActionOutcome::ForwardDrop(intent)),
            SampleAction::SetKitOutput {
                kit,
                bus,
                expected_revision,
            } => self
                .edit_kit(expected_revision, kit, None, |kit| {
                    kit.output = SampleRouteIntent::new(bus)
                        .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?;
                    kit.revision = kit.revision.saturating_add(1);
                    Ok(())
                })
                .map(SampleActionOutcome::Published),
            SampleAction::SetPadChoke {
                kit,
                pad,
                choke_group,
                expected_revision,
            } => self
                .edit_kit(expected_revision, kit, Some(pad), |kit| {
                    kit.pads
                        .get_mut(&pad)
                        .ok_or(ConstructiveControllerError::MissingPad { kit: kit.id, pad })?
                        .choke_group = choke_group.map(std::num::NonZeroU32::get);
                    kit.revision = kit.revision.saturating_add(1);
                    Ok(())
                })
                .map(SampleActionOutcome::Published),
            SampleAction::RemoveZone {
                kit,
                pad,
                zone,
                expected_revision,
            } => self
                .remove_zone(expected_revision, kit, pad, zone)
                .map(SampleActionOutcome::Published),
            SampleAction::MakeBeat(intent) => {
                let result_focus = intent.result_focus;
                let plan = self.plan_make_beat(intent)?;
                let mut outcome = self.execute_constructive_plan(plan)?;
                apply_make_beat_focus(&mut outcome.publication, result_focus)?;
                Ok(SampleActionOutcome::Published(outcome))
            }
        }
    }

    fn preview_chop(
        &self,
        intent: ChopPreviewIntent,
    ) -> Result<OnsetChopPreview, ConstructiveControllerError> {
        Self::preview_chop_from_snapshot(self.snapshot(), intent)
    }

    pub fn preview_chop_from_snapshot(
        snapshot: &LiveProjectSnapshot,
        intent: ChopPreviewIntent,
    ) -> Result<OnsetChopPreview, ConstructiveControllerError> {
        let source = source_snapshot(snapshot, intent.source)?;
        let boundaries = match &intent.chop {
            SampleChopIntent::DetectOnsets {
                sensitivity,
                minimum_gap_frames,
                ..
            } => detect_onset_ranges(&source, *sensitivity, *minimum_gap_frames)?
                .into_iter()
                .skip(1)
                .map(|range| range.start)
                .collect(),
            _ => chop_ranges(&source, &intent.chop)?
                .into_iter()
                .skip(1)
                .map(|range| range.start)
                .collect(),
        };
        let analyzer = match intent.chop {
            SampleChopIntent::DetectOnsets { analyzer, .. } => analyzer,
            _ => "deterministic-grid".into(),
        };
        Ok(OnsetChopPreview {
            source: intent.source,
            analyzer,
            boundaries,
            confidence: None,
            diagnostic: None,
        })
    }

    fn execute_zone_edit(
        &mut self,
        intent: ZoneEditIntent,
    ) -> Result<SampleActionOutcome, ConstructiveControllerError> {
        let target = match &intent {
            ZoneEditIntent::Trim { target, .. }
            | ZoneEditIntent::SetLoop { target, .. }
            | ZoneEditIntent::SetEnvelope { target, .. }
            | ZoneEditIntent::SetPlayback { target, .. } => *target,
        };
        let kit = self
            .snapshot()
            .project
            .state()
            .domains
            .sample_kits
            .kits
            .get(&target.kit)
            .ok_or(ConstructiveControllerError::MissingKit(target.kit))?;
        if kit.revision != target.expected_revision {
            return Err(ConstructiveControllerError::KitRevisionConflict {
                kit: target.kit,
                expected: target.expected_revision,
                actual: kit.revision,
            });
        }
        let source_asset = kit
            .zone_for_target(SampleTargetRef {
                kit: target.kit,
                pad: target.pad,
                zone: target.zone,
            })
            .ok_or(ConstructiveControllerError::MissingZone {
                kit: target.kit,
                zone: target.zone,
            })?
            .material
            .asset_id();
        match &intent {
            ZoneEditIntent::Trim { source_range, .. } => {
                return self
                    .trim_zone(target, source_asset, *source_range)
                    .map(SampleActionOutcome::Published);
            }
            ZoneEditIntent::SetLoop {
                enabled,
                source_range,
                ..
            } => {
                if *enabled && source_range.is_none() {
                    return Err(ConstructiveControllerError::InvalidSourceRange);
                }
                if let Some(source_range) = source_range {
                    self.constructive_source_snapshot(SampleSelection {
                        asset: source_asset,
                        source_range: Some(*source_range),
                    })?;
                }
            }
            ZoneEditIntent::SetEnvelope { envelope, .. } if !envelope.is_valid() => {
                return Err(ConstructiveControllerError::InvalidEnvelope);
            }
            ZoneEditIntent::SetEnvelope { .. } => {}
            ZoneEditIntent::SetPlayback {
                gain_db,
                pan,
                tuning_cents,
                ..
            } => {
                if !gain_db.is_finite()
                    || !(-144.0..=48.0).contains(gain_db)
                    || !pan.is_finite()
                    || !(-1.0..=1.0).contains(pan)
                    || !tuning_cents.is_finite()
                    || !(-9_600.0..=9_600.0).contains(tuning_cents)
                {
                    return Err(ConstructiveControllerError::InvalidZonePlayback);
                }
                let (gain_db, pan, tuning_cents) = (*gain_db, *pan, *tuning_cents);
                return self
                    .edit_kit(
                        target.expected_revision,
                        target.kit,
                        Some(target.pad),
                        |kit| {
                            let zone = kit
                                .zones
                                .get_mut(&target.zone)
                                .filter(|zone| zone.pad == target.pad)
                                .ok_or(ConstructiveControllerError::MissingZone {
                                    kit: target.kit,
                                    zone: target.zone,
                                })?;
                            zone.gain_db = gain_db;
                            zone.pan = pan;
                            zone.tuning_cents = tuning_cents;
                            kit.revision = kit.revision.saturating_add(1);
                            Ok(())
                        },
                    )
                    .map(SampleActionOutcome::Published);
            }
        }
        Ok(SampleActionOutcome::ForwardZoneEdit(intent))
    }

    fn trim_zone(
        &mut self,
        target: crate::sample_actions::ZoneEditTarget,
        source_asset: assets::AssetId,
        source_range: AssetFrameRange,
    ) -> Result<ConstructiveOutcome, ConstructiveControllerError> {
        let snapshot = self.snapshot().clone();
        let source = source_snapshot(
            &snapshot,
            SampleSelection {
                asset: source_asset,
                source_range: Some(source_range),
            },
        )?;
        let slice = VirtualSliceRef::new(source_asset, source_range)
            .map_err(|error| ConstructiveControllerError::Material(error.to_string()))?;
        let extracted = extract_virtual_slice(slice, &source.pcm)
            .map_err(|error| ConstructiveControllerError::Material(error.to_string()))?;
        let target_ref = SampleTargetRef {
            kit: target.kit,
            pad: target.pad,
            zone: target.zone,
        };

        let mut candidate = snapshot.project.state().clone();
        let before = candidate
            .domains
            .sample_kits
            .kits
            .get(&target.kit)
            .cloned()
            .ok_or(ConstructiveControllerError::MissingKit(target.kit))?;
        let mut after = before.clone();
        let zone = after
            .zones
            .get_mut(&target.zone)
            .filter(|zone| zone.pad == target.pad)
            .ok_or(ConstructiveControllerError::MissingZone {
                kit: target.kit,
                zone: target.zone,
            })?;
        zone.material = SourceMaterialRef::VirtualSlice(slice);
        zone.decoded_pcm = Some(extracted.identity);
        zone.provenance = SampleMaterialProvenance::ManualSelection;
        after.revision = after.revision.saturating_add(1);
        candidate
            .domains
            .sample_kits
            .apply_puts(&[SampleKitPut {
                before: Some(before),
                after: Some(after),
            }])
            .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?;
        remove_zone_usages(&mut candidate, target.zone)?;
        candidate
            .domains
            .assets
            .add_usage(
                source_asset,
                AssetUsageOwner::SamplerZone {
                    persistent_id: target.zone.get(),
                },
                Some(source_range),
                format!("sample zone {}", target.zone.get()),
            )
            .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?;

        let commands = lower_state_transition(snapshot.project.state(), &candidate)?;
        let envelope = CommandEnvelope {
            label: "Trim sample zone".into(),
            base_revision: snapshot.revisions().aggregate,
            coalesce: None,
            id_claims: claims_for_commands(&commands),
            commands,
        };
        let update = self.execute_with_sample_pcm_patch(
            envelope,
            BTreeMap::from([(target_ref, Some(extracted.to_pcm_asset()))]),
        )?;
        Ok(ConstructiveOutcome {
            publication: ConstructivePublication {
                revision: update.revisions().aggregate,
                kit: target.kit,
                created_pads: Vec::new(),
                created_zones: Vec::new(),
                pad: Some(target.pad),
                pattern: None,
                sequencer_clip: None,
                arrangement_clip: None,
                arrangement_track: None,
                output_bus: None,
                focus: ConstructivePublishedFocus::Pad {
                    kit: target.kit,
                    pad: target.pad,
                },
            },
            update,
        })
    }

    pub fn plan_make_beat(
        &self,
        intent: MakeBeatIntent,
    ) -> Result<ConstructiveEditPlan, ConstructiveControllerError> {
        Self::plan_make_beat_from_snapshot(self.snapshot(), intent)
    }

    pub fn plan_make_beat_from_snapshot(
        snapshot: &LiveProjectSnapshot,
        intent: MakeBeatIntent,
    ) -> Result<ConstructiveEditPlan, ConstructiveControllerError> {
        let source = source_snapshot(snapshot, intent.source)?;
        let ranges = chop_ranges(&source, &intent.chop)?;
        let mut library = snapshot.project.state().domains.sample_kits.clone();
        let before = match intent.kit {
            SampleKitDestination::NewKit => None,
            SampleKitDestination::ExistingKit {
                kit,
                expected_revision,
            } => {
                let current = library
                    .kits
                    .get(&kit)
                    .cloned()
                    .ok_or(ConstructiveControllerError::MissingKit(kit))?;
                if current.revision != expected_revision {
                    return Err(ConstructiveControllerError::KitRevisionConflict {
                        kit,
                        expected: expected_revision,
                        actual: current.revision,
                    });
                }
                Some(current)
            }
        };
        let output_bus = choose_output_bus(snapshot, intent.target_bus, "Sample Kit")?;
        let mut kit = if let Some(before) = &before {
            let mut kit = before.clone();
            if let Some(bus) = intent.target_bus {
                kit.output = SampleRouteIntent::new(bus)
                    .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?;
            }
            kit
        } else {
            SampleKit::new(
                library
                    .allocate_kit_id()
                    .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?,
                "Sample Kit",
                SampleRouteIntent::new(output_bus)
                    .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?,
            )
        };

        let mut materials = Vec::new();
        let mut bindings = BTreeMap::new();
        let mut steps = Vec::new();
        for (index, range) in ranges.into_iter().enumerate() {
            let pad = library
                .allocate_pad_id()
                .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?;
            let zone = library
                .allocate_zone_id()
                .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?;
            let slice = VirtualSliceRef::new(source.selection.asset, range)
                .map_err(|error| ConstructiveControllerError::Material(error.to_string()))?;
            let extracted = extract_virtual_slice(slice, &source.pcm)
                .map_err(|error| ConstructiveControllerError::Material(error.to_string()))?;
            let mut sample_zone =
                SampleZone::new(zone, pad, SourceMaterialRef::VirtualSlice(slice));
            sample_zone.decoded_pcm = Some(extracted.identity);
            sample_zone.provenance = match &intent.chop {
                SampleChopIntent::DetectOnsets { analyzer, .. } => {
                    SampleMaterialProvenance::OnsetChop {
                        analyzer: analyzer.clone(),
                        evidence: Vec::new(),
                    }
                }
                _ => SampleMaterialProvenance::ManualSelection,
            };
            kit.pads
                .insert(pad, SamplePad::new(pad, format!("Pad {}", index + 1)));
            kit.pads.get_mut(&pad).unwrap().zone_order.push(zone);
            kit.pad_order.push(pad);
            kit.zones.insert(zone, sample_zone);
            materials.push(PlannedMaterial {
                zone,
                slice,
                decoded_pcm: extracted.identity,
                reuse: MaterialReusePolicy::RequireNew,
            });
            bindings.insert(format!("p{}", index + 1), pad);
            let at = i64::try_from(index)
                .ok()
                .and_then(|index| index.checked_mul(intent.quantize_ticks as i64))
                .ok_or(ConstructiveControllerError::TimingOverflow)?;
            steps.push(PlannedStep {
                pad,
                at: BeatTime(at),
                gate: BeatDuration(intent.quantize_ticks.max(1)),
                velocity: 1.0,
                probability: 1.0,
                ratchets: 1,
                pitch_semitones: 0.0,
                pan: 0.0,
                micro_offset_ticks: 0,
                original_micro_offset_frames: None,
                exact_source_onset_frame: Some(range.start.0),
                evidence: Vec::new(),
            });
        }
        kit.revision = before
            .as_ref()
            .map_or(1, |kit| kit.revision.saturating_add(1));
        let planned_id = PlannedPatternId::from_raw(1);
        let cycle_ticks = u64::from(intent.bars.max(1))
            .checked_mul(4)
            .and_then(|beats| beats.checked_mul(sequencer::PPQ as u64))
            .ok_or(ConstructiveControllerError::TimingOverflow)?;
        let pattern = PlannedPattern {
            id: planned_id,
            name: "Sample Beat".into(),
            cycle: BeatDuration(cycle_ticks),
            seed: PatternSeed::EmptyGrid {
                resolution: BeatDuration(intent.quantize_ticks.max(1)),
            },
            bindings,
            steps,
        };
        let first_slice = materials
            .first()
            .ok_or(ConstructiveControllerError::EmptyChop)?
            .slice;
        ConstructiveEditPlan::new(
            "Sample selection and make beat",
            source.project_revision,
            vec![ConstructiveCause::ManualSelection {
                material: first_slice,
            }],
            materials,
            KitMutation { before, after: kit },
            Some(pattern),
            Some(PatternPlacementIntent {
                pattern: planned_id,
                start: BeatTime(0),
                length: BeatDuration(cycle_ticks),
                pattern_offset: BeatTime(0),
                looped: true,
                transpose_semitones: 0.0,
                gain: 1.0,
            }),
            ConstructiveFocus::Pattern(planned_id),
        )
        .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))
    }

    /// Build sampler material without authoring a sequencer pattern. This is
    /// the controller-side primitive used by source-timeline one-shot/chop
    /// actions; it deliberately reuses the same validated material planner as
    /// Make Beat and removes only its optional pattern publication.
    pub(crate) fn plan_sample_kit(
        &self,
        source: SampleSelection,
        chop: SampleChopIntent,
        kit: SampleKitDestination,
        target_bus: Option<crate::mixer::BusId>,
        label: impl Into<String>,
    ) -> Result<ConstructiveEditPlan, ConstructiveControllerError> {
        let source_snapshot = self.constructive_source_snapshot(source)?;
        let count = chop_ranges(&source_snapshot, &chop)?.len();
        let ticks_per_bar = 4_u64
            .checked_mul(sequencer::PPQ as u64)
            .ok_or(ConstructiveControllerError::TimingOverflow)?;
        let bars = u16::try_from(
            u64::try_from(count)
                .map_err(|_| ConstructiveControllerError::TimingOverflow)?
                .div_ceil(ticks_per_bar)
                .max(1),
        )
        .map_err(|_| ConstructiveControllerError::TimingOverflow)?;
        let mut plan = self.plan_make_beat(MakeBeatIntent {
            source,
            chop,
            kit,
            target_bus,
            bars,
            quantize_ticks: 1,
            result_focus: crate::sample_actions::MakeBeatResultFocus::Sampler(
                SamplerViewDisposition::RetargetCurrent,
            ),
        })?;
        plan.label = label.into();
        plan.pattern = None;
        plan.placement = None;
        plan.focus = ConstructiveFocus::Kit;
        plan.validate()
            .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?;
        Ok(plan)
    }

    fn plan_asset_to_pad(
        &self,
        source: AssetDrag,
        kit_id: KitId,
        pad: PadId,
    ) -> Result<ConstructiveEditPlan, ConstructiveControllerError> {
        let selection = SampleSelection {
            asset: source.asset,
            source_range: source.source_range,
        };
        let source = self.constructive_source_snapshot(selection)?;
        let before = self
            .snapshot()
            .project
            .state()
            .domains
            .sample_kits
            .kits
            .get(&kit_id)
            .cloned()
            .ok_or(ConstructiveControllerError::MissingKit(kit_id))?;
        if !before.pads.contains_key(&pad) {
            return Err(ConstructiveControllerError::MissingPad { kit: kit_id, pad });
        }
        let mut library = self.snapshot().project.state().domains.sample_kits.clone();
        let zone = library
            .allocate_zone_id()
            .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?;
        let slice = VirtualSliceRef::new(source.selection.asset, source.source_range)
            .map_err(|error| ConstructiveControllerError::Material(error.to_string()))?;
        let extracted = extract_virtual_slice(slice, &source.pcm)
            .map_err(|error| ConstructiveControllerError::Material(error.to_string()))?;
        let mut after = before.clone();
        let mut sample_zone = SampleZone::new(zone, pad, SourceMaterialRef::VirtualSlice(slice));
        sample_zone.decoded_pcm = Some(extracted.identity);
        sample_zone.provenance = SampleMaterialProvenance::ManualSelection;
        after.zones.insert(zone, sample_zone);
        after.pads.get_mut(&pad).unwrap().zone_order.push(zone);
        after.revision = after.revision.saturating_add(1);
        ConstructiveEditPlan::new(
            "Map sample to pad",
            source.project_revision,
            vec![ConstructiveCause::ManualSelection { material: slice }],
            vec![PlannedMaterial {
                zone,
                slice,
                decoded_pcm: extracted.identity,
                reuse: MaterialReusePolicy::RequireNew,
            }],
            KitMutation {
                before: Some(before),
                after,
            },
            None,
            None,
            ConstructiveFocus::Pad(pad),
        )
        .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))
    }

    fn edit_kit(
        &mut self,
        expected_revision: u64,
        kit_id: KitId,
        pad: Option<PadId>,
        edit: impl FnOnce(&mut SampleKit) -> Result<(), ConstructiveControllerError>,
    ) -> Result<ConstructiveOutcome, ConstructiveControllerError> {
        let before = self
            .snapshot()
            .project
            .state()
            .domains
            .sample_kits
            .kits
            .get(&kit_id)
            .cloned()
            .ok_or(ConstructiveControllerError::MissingKit(kit_id))?;
        if before.revision != expected_revision {
            return Err(ConstructiveControllerError::KitRevisionConflict {
                kit: kit_id,
                expected: expected_revision,
                actual: before.revision,
            });
        }
        let mut after = before.clone();
        edit(&mut after)?;
        let commands = vec![DomainCommand::SampleKits(SampleKitPut {
            before: Some(before),
            after: Some(after),
        })];
        let envelope = CommandEnvelope {
            label: "Edit sample kit".into(),
            base_revision: self.revisions().aggregate,
            coalesce: None,
            id_claims: claims_for_commands(&commands),
            commands,
        };
        let update = self.execute(envelope)?;
        Ok(ConstructiveOutcome {
            publication: ConstructivePublication {
                revision: update.revisions().aggregate,
                kit: kit_id,
                created_pads: Vec::new(),
                created_zones: Vec::new(),
                pad,
                pattern: None,
                sequencer_clip: None,
                arrangement_clip: None,
                arrangement_track: None,
                output_bus: None,
                focus: pad.map_or(ConstructivePublishedFocus::Kit(kit_id), |pad| {
                    ConstructivePublishedFocus::Pad { kit: kit_id, pad }
                }),
            },
            update,
        })
    }

    fn remove_zone(
        &mut self,
        expected_revision: u64,
        kit: KitId,
        pad: PadId,
        zone: ZoneId,
    ) -> Result<ConstructiveOutcome, ConstructiveControllerError> {
        let target = SampleTargetRef { kit, pad, zone };
        let mut before_state = self.snapshot().project.state().clone();
        let before = before_state
            .domains
            .sample_kits
            .kits
            .get(&kit)
            .cloned()
            .ok_or(ConstructiveControllerError::MissingKit(kit))?;
        if before.revision != expected_revision {
            return Err(ConstructiveControllerError::KitRevisionConflict {
                kit,
                expected: expected_revision,
                actual: before.revision,
            });
        }
        let mut after = before.clone();
        if after.zones.remove(&zone).is_none() {
            return Err(ConstructiveControllerError::MissingZone { kit, zone });
        }
        after
            .pads
            .get_mut(&pad)
            .ok_or(ConstructiveControllerError::MissingPad { kit, pad })?
            .zone_order
            .retain(|id| *id != zone);
        after.revision = after.revision.saturating_add(1);
        before_state
            .domains
            .sample_kits
            .apply_puts(&[SampleKitPut {
                before: Some(before),
                after: Some(after),
            }])
            .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?;
        remove_zone_usages(&mut before_state, zone)?;
        let commands = lower_state_transition(self.snapshot().project.state(), &before_state)?;
        let envelope = CommandEnvelope {
            label: "Remove sample zone".into(),
            base_revision: self.revisions().aggregate,
            coalesce: None,
            id_claims: claims_for_commands(&commands),
            commands,
        };
        let mut patch = BTreeMap::new();
        patch.insert(target, None);
        let update = self.execute_with_sample_pcm_patch(envelope, patch)?;
        Ok(ConstructiveOutcome {
            publication: ConstructivePublication {
                revision: update.revisions().aggregate,
                kit,
                created_pads: Vec::new(),
                created_zones: Vec::new(),
                pad: Some(pad),
                pattern: None,
                sequencer_clip: None,
                arrangement_clip: None,
                arrangement_track: None,
                output_bus: None,
                focus: ConstructivePublishedFocus::Pad { kit, pad },
            },
            update,
        })
    }
}

fn prepare_constructive_commit(
    snapshot: &LiveProjectSnapshot,
    plan: ConstructiveEditPlan,
) -> Result<PreparedConstructiveCommit, ConstructiveControllerError> {
    if plan.base_revision != snapshot.revisions().aggregate {
        return Err(ConstructiveControllerError::RevisionConflict {
            expected: plan.base_revision,
            actual: snapshot.revisions().aggregate,
        });
    }
    plan.validate()
        .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?;

    let mut candidate = snapshot.project.state().clone();
    let bindings = constructive::apply_to_project_state(&mut candidate, &plan)
        .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?;
    add_material_usages(&mut candidate, &plan)?;
    let commands = lower_state_transition(snapshot.project.state(), &candidate)?;
    let envelope = CommandEnvelope {
        label: plan.label.clone(),
        base_revision: plan.base_revision,
        coalesce: None,
        id_claims: claims_for_commands(&commands),
        commands,
    };
    let materialized = materialize_plan(snapshot, &plan, &bindings)?;
    let focus = resolve_focus(plan.focus, &bindings)?;
    let pad = match plan.focus {
        ConstructiveFocus::Pad(pad) => Some(pad),
        _ => None,
    };
    Ok(PreparedConstructiveCommit {
        base_revision: plan.base_revision,
        envelope,
        materialized,
        publication: ConstructivePublication {
            revision: plan.base_revision,
            kit: bindings.kit,
            created_pads: bindings.created_pads.clone(),
            created_zones: bindings.created_zones.clone(),
            pad,
            pattern: bindings.pattern,
            sequencer_clip: bindings.sequencer_clip,
            arrangement_clip: bindings.arrangement_clip,
            arrangement_track: bindings.arrangement_track,
            output_bus: Some(bindings.output_bus),
            focus,
        },
    })
}

pub(super) fn apply_make_beat_focus(
    publication: &mut ConstructivePublication,
    result_focus: crate::sample_actions::MakeBeatResultFocus,
) -> Result<(), ConstructiveControllerError> {
    publication.focus = match result_focus {
        crate::sample_actions::MakeBeatResultFocus::Stay => ConstructivePublishedFocus::Stay,
        crate::sample_actions::MakeBeatResultFocus::Sampler(disposition) => {
            ConstructivePublishedFocus::Sampler {
                kit: publication.kit,
                disposition,
            }
        }
        crate::sample_actions::MakeBeatResultFocus::PatternEditor => {
            ConstructivePublishedFocus::Pattern(
                publication
                    .pattern
                    .ok_or(ConstructiveControllerError::MissingPublishedPattern)?,
            )
        }
        crate::sample_actions::MakeBeatResultFocus::Arrangement => {
            ConstructivePublishedFocus::Arrangement(
                publication
                    .arrangement_clip
                    .ok_or(ConstructiveControllerError::MissingPublishedArrangementClip)?,
            )
        }
    };
    Ok(())
}

fn source_snapshot(
    snapshot: &LiveProjectSnapshot,
    selection: SampleSelection,
) -> Result<ConstructiveSourceSnapshot, ConstructiveControllerError> {
    let asset = snapshot
        .project
        .state()
        .domains
        .assets
        .get(selection.asset)
        .ok_or(ConstructiveControllerError::MissingAsset(selection.asset))?;
    let pcm = snapshot
        .pcm
        .get(&selection.asset)
        .cloned()
        .ok_or(ConstructiveControllerError::MissingPcm(selection.asset))?;
    let source_range = selection.source_range.unwrap_or(AssetFrameRange {
        start: SampleFrames(0),
        end: asset.metadata().frame_count,
    });
    if !source_range.is_within(asset.metadata().frame_count)
        || source_range.start >= source_range.end
    {
        return Err(ConstructiveControllerError::InvalidSourceRange);
    }
    Ok(ConstructiveSourceSnapshot {
        project_revision: snapshot.revisions().aggregate,
        selection,
        source_range,
        pcm,
    })
}

fn chop_ranges(
    source: &ConstructiveSourceSnapshot,
    chop: &SampleChopIntent,
) -> Result<Vec<AssetFrameRange>, ConstructiveControllerError> {
    match chop {
        SampleChopIntent::OneShot => Ok(vec![source.source_range]),
        SampleChopIntent::EqualSlices { count } => {
            if *count == 0 || u64::from(*count) > source.source_range.len().0 {
                return Err(ConstructiveControllerError::EmptyChop);
            }
            let count = u64::from(*count);
            let length = source.source_range.len().0;
            (0..count)
                .map(|index| {
                    let start = source.source_range.start.0 + length * index / count;
                    let end = source.source_range.start.0 + length * (index + 1) / count;
                    AssetFrameRange::new(SampleFrames(start), SampleFrames(end))
                        .map_err(|_| ConstructiveControllerError::EmptyChop)
                })
                .collect()
        }
        SampleChopIntent::DetectOnsets {
            sensitivity,
            minimum_gap_frames,
            ..
        } => detect_onset_ranges(source, *sensitivity, *minimum_gap_frames),
    }
}

fn detect_onset_ranges(
    source: &ConstructiveSourceSnapshot,
    sensitivity: f32,
    minimum_gap_frames: u64,
) -> Result<Vec<AssetFrameRange>, ConstructiveControllerError> {
    if !sensitivity.is_finite() {
        return Err(ConstructiveControllerError::InvalidOnsetSettings);
    }
    let channels = usize::from(source.pcm.format.channels.get());
    let start = usize::try_from(source.source_range.start.0)
        .map_err(|_| ConstructiveControllerError::TimingOverflow)?;
    let end = usize::try_from(source.source_range.end.0)
        .map_err(|_| ConstructiveControllerError::TimingOverflow)?;
    let mut peaks = Vec::new();
    let mut max_delta = 0.0_f32;
    let mut prior = 0.0_f32;
    for frame in start..end {
        let amplitude = source.pcm.samples[frame * channels..(frame + 1) * channels]
            .iter()
            .map(|sample| sample.abs())
            .fold(0.0_f32, f32::max);
        let delta = (amplitude - prior).max(0.0);
        max_delta = max_delta.max(delta);
        peaks.push(delta);
        prior = amplitude;
    }
    let threshold = max_delta * (1.0 - sensitivity.clamp(0.0, 1.0));
    let mut boundaries = vec![source.source_range.start.0];
    let gap = minimum_gap_frames.max(1);
    for (offset, delta) in peaks.into_iter().enumerate().skip(1) {
        let frame = source.source_range.start.0 + offset as u64;
        if delta >= threshold && frame.saturating_sub(*boundaries.last().unwrap()) >= gap {
            boundaries.push(frame);
        }
    }
    boundaries.push(source.source_range.end.0);
    boundaries.dedup();
    boundaries
        .windows(2)
        .map(|pair| {
            AssetFrameRange::new(SampleFrames(pair[0]), SampleFrames(pair[1]))
                .map_err(|_| ConstructiveControllerError::EmptyChop)
        })
        .collect()
}

pub(super) fn choose_output_bus(
    snapshot: &LiveProjectSnapshot,
    requested: Option<crate::mixer::BusId>,
    name: &str,
) -> Result<crate::mixer::BusId, ConstructiveControllerError> {
    if let Some(bus) = requested {
        if snapshot.project.state().domains.mixer.bus(bus).is_none() {
            return Err(ConstructiveControllerError::MissingBus(bus));
        }
        return Ok(bus);
    }
    let mut mixer = snapshot.project.state().domains.mixer.clone();
    mixer
        .add_bus(BusKind::Source, name)
        .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))
}

fn add_material_usages(
    state: &mut ProjectState,
    plan: &ConstructiveEditPlan,
) -> Result<(), ConstructiveControllerError> {
    for material in &plan.materials {
        state
            .domains
            .assets
            .add_usage(
                material.slice.source_asset,
                AssetUsageOwner::SamplerZone {
                    persistent_id: material.zone.get(),
                },
                Some(material.slice.source_range),
                format!("sample zone {}", material.zone.get()),
            )
            .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?;
    }
    Ok(())
}

fn remove_zone_usages(
    state: &mut ProjectState,
    zone: ZoneId,
) -> Result<(), ConstructiveControllerError> {
    let mut removals = Vec::new();
    for (&asset, record) in state.domains.assets.assets() {
        for (&usage, value) in record.usages() {
            if value.owner
                == (AssetUsageOwner::SamplerZone {
                    persistent_id: zone.get(),
                })
            {
                removals.push((asset, usage));
            }
        }
    }
    for (asset, usage) in removals {
        state
            .domains
            .assets
            .remove_usage(asset, usage)
            .map_err(|error| ConstructiveControllerError::Plan(error.to_string()))?;
    }
    Ok(())
}

fn materialize_plan(
    snapshot: &LiveProjectSnapshot,
    plan: &ConstructiveEditPlan,
    bindings: &ConstructiveApplicationBindings,
) -> Result<BTreeMap<SampleTargetRef, PcmAsset>, ConstructiveControllerError> {
    let kit = &plan.kit.after;
    let mut result = BTreeMap::new();
    for material in &plan.materials {
        let zone =
            kit.zones
                .get(&material.zone)
                .ok_or(ConstructiveControllerError::MissingZone {
                    kit: bindings.kit,
                    zone: material.zone,
                })?;
        let source = snapshot.pcm.get(&material.slice.source_asset).ok_or(
            ConstructiveControllerError::MissingPcm(material.slice.source_asset),
        )?;
        let extracted = extract_virtual_slice(material.slice, source)
            .map_err(|error| ConstructiveControllerError::Material(error.to_string()))?;
        if extracted.identity != material.decoded_pcm {
            return Err(ConstructiveControllerError::MaterialIdentity {
                zone: material.zone,
                expected: material.decoded_pcm,
                actual: extracted.identity,
            });
        }
        result.insert(
            SampleTargetRef {
                kit: bindings.kit,
                pad: zone.pad,
                zone: zone.id,
            },
            extracted.to_pcm_asset(),
        );
    }
    Ok(result)
}

fn resolve_focus(
    focus: ConstructiveFocus,
    bindings: &ConstructiveApplicationBindings,
) -> Result<ConstructivePublishedFocus, ConstructiveControllerError> {
    match focus {
        ConstructiveFocus::Kit => Ok(ConstructivePublishedFocus::Kit(bindings.kit)),
        ConstructiveFocus::Pad(pad) => Ok(ConstructivePublishedFocus::Pad {
            kit: bindings.kit,
            pad,
        }),
        ConstructiveFocus::Pattern(_) => bindings
            .pattern
            .map(ConstructivePublishedFocus::Pattern)
            .ok_or(ConstructiveControllerError::MissingPublishedPattern),
    }
}

fn lower_state_transition(
    before: &ProjectState,
    after: &ProjectState,
) -> Result<Vec<DomainCommand>, ConstructiveControllerError> {
    let mut commands = Vec::new();
    if before.domains.mixer != after.domains.mixer {
        commands.push(DomainCommand::Mixer(
            MixerCommand::build("Constructive routing", &before.domains.mixer, |graph| {
                *graph = after.domains.mixer.clone();
                Ok(())
            })
            .map_err(|error| ConstructiveControllerError::Lower(error.to_string()))?,
        ));
    }
    diff_sample_kits(before, after, &mut commands);
    diff_assets(before, after, &mut commands);
    diff_bindings(&before.bindings, &after.bindings, &mut commands);
    diff_sequencer(before, after, &mut commands);
    diff_arrangement(before, after, &mut commands);
    if commands.is_empty() {
        return Err(ConstructiveControllerError::EmptyTransition);
    }
    Ok(commands)
}

fn diff_sample_kits(before: &ProjectState, after: &ProjectState, out: &mut Vec<DomainCommand>) {
    let ids = before
        .domains
        .sample_kits
        .kits
        .keys()
        .chain(after.domains.sample_kits.kits.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    for id in ids {
        let before = before.domains.sample_kits.kits.get(&id);
        let after = after.domains.sample_kits.kits.get(&id);
        if before != after {
            out.push(DomainCommand::SampleKits(SampleKitPut {
                before: before.cloned(),
                after: after.cloned(),
            }));
        }
    }
}

fn diff_assets(before: &ProjectState, after: &ProjectState, out: &mut Vec<DomainCommand>) {
    let assets = before
        .domains
        .assets
        .assets()
        .keys()
        .chain(after.domains.assets.assets().keys())
        .copied()
        .collect::<BTreeSet<_>>();
    for asset in assets {
        let before_record = before.domains.assets.get(asset);
        let after_record = after.domains.assets.get(asset);
        match (before_record, after_record) {
            (Some(before_record), Some(after_record)) => {
                let usages = before_record
                    .usages()
                    .keys()
                    .chain(after_record.usages().keys())
                    .copied()
                    .collect::<BTreeSet<_>>();
                for usage in usages {
                    let before = before_record.usages().get(&usage);
                    let after = after_record.usages().get(&usage);
                    if before != after {
                        out.push(DomainCommand::Assets(AssetCommand::PutUsage {
                            asset,
                            usage,
                            before: before.cloned(),
                            after: after.cloned(),
                        }));
                    }
                }
            }
            _ if before_record != after_record => {
                out.push(DomainCommand::Assets(AssetCommand::PutAsset {
                    id: asset,
                    before: before_record.cloned(),
                    after: after_record.cloned(),
                }));
            }
            _ => {}
        }
    }
}

fn diff_bindings(before: &ProjectBindings, after: &ProjectBindings, out: &mut Vec<DomainCommand>) {
    macro_rules! diff_map {
        ($before:expr, $after:expr, $variant:ident, $field:ident) => {{
            let keys = $before
                .keys()
                .chain($after.keys())
                .copied()
                .collect::<BTreeSet<_>>();
            for key in keys {
                let left = $before.get(&key);
                let right = $after.get(&key);
                if left != right {
                    out.push(DomainCommand::Bindings(BindingCommand::$variant {
                        $field: key,
                        before: left.copied(),
                        after: right.copied(),
                    }));
                }
            }
        }};
    }
    diff_map!(
        &before.assets.arrangement_assets,
        &after.assets.arrangement_assets,
        PutMediaAssetAlias,
        alias
    );
    diff_map!(
        &before.assets.sequencer_samples,
        &after.assets.sequencer_samples,
        PutSequencerSampleAlias,
        alias
    );
    diff_map!(
        &before.sample_targets.targets,
        &after.sample_targets.targets,
        PutSampleTargetAlias,
        alias
    );
    diff_map!(
        &before.patterns.definitions,
        &after.patterns.definitions,
        PutPatternDefinitionAlias,
        alias
    );
    diff_map!(
        &before.patterns.placements,
        &after.patterns.placements,
        PutPatternPlacement,
        clip
    );
    diff_map!(
        &before.mixer.tracks,
        &after.mixer.tracks,
        PutTrackBus,
        track
    );
}

fn diff_sequencer(before: &ProjectState, after: &ProjectState, out: &mut Vec<DomainCommand>) {
    let before_patterns = before
        .domains
        .sequencer
        .patterns()
        .patterns()
        .map(|pattern| (pattern.id, pattern))
        .collect::<BTreeMap<_, _>>();
    let after_patterns = after
        .domains
        .sequencer
        .patterns()
        .patterns()
        .map(|pattern| (pattern.id, pattern))
        .collect::<BTreeMap<_, _>>();
    for id in before_patterns
        .keys()
        .chain(after_patterns.keys())
        .copied()
        .collect::<BTreeSet<_>>()
    {
        if before_patterns.get(&id) != after_patterns.get(&id) {
            out.push(DomainCommand::Sequencer(SequencerCommand::PutPattern {
                before: before_patterns.get(&id).map(|pattern| (*pattern).clone()),
                after: after_patterns.get(&id).map(|pattern| (*pattern).clone()),
            }));
        }
    }
    let before_clips = before
        .domains
        .sequencer
        .clips()
        .map(|clip| (clip.id, clip))
        .collect::<BTreeMap<_, _>>();
    let after_clips = after
        .domains
        .sequencer
        .clips()
        .map(|clip| (clip.id, clip))
        .collect::<BTreeMap<_, _>>();
    for id in before_clips
        .keys()
        .chain(after_clips.keys())
        .copied()
        .collect::<BTreeSet<_>>()
    {
        if before_clips.get(&id) != after_clips.get(&id) {
            out.push(DomainCommand::Sequencer(SequencerCommand::PutClip {
                before: before_clips.get(&id).map(|clip| (*clip).clone()),
                after: after_clips.get(&id).map(|clip| (*clip).clone()),
            }));
        }
    }
}

fn diff_arrangement(before: &ProjectState, after: &ProjectState, out: &mut Vec<DomainCommand>) {
    for id in before
        .domains
        .arrangement
        .clips
        .keys()
        .filter(|id| !after.domains.arrangement.clips.contains_key(id))
    {
        out.push(DomainCommand::Arrangement(ArrangementOperation::PutClip {
            before: before.domains.arrangement.clips.get(id).cloned(),
            after: None,
        }));
    }
    for id in before
        .domains
        .arrangement
        .tracks
        .keys()
        .chain(after.domains.arrangement.tracks.keys())
        .copied()
        .collect::<BTreeSet<_>>()
    {
        let left = before.domains.arrangement.tracks.get(&id);
        let right = after.domains.arrangement.tracks.get(&id);
        let mut comparable_left = left.cloned();
        let mut comparable_right = right.cloned();
        if let Some(track) = &mut comparable_left {
            track.clip_ids.clear();
        }
        if let Some(track) = &mut comparable_right {
            track.clip_ids.clear();
        }
        if comparable_left != comparable_right {
            // Clip puts maintain the redundant track index. Keeping the
            // current index here avoids referring to a new clip before that
            // clip command has executed.
            let mut command_before = left.cloned();
            if let Some(track) = &mut command_before {
                track
                    .clip_ids
                    .retain(|clip| after.domains.arrangement.clips.contains_key(clip));
            }
            let mut command_after = right.cloned();
            if let Some(track) = &mut command_after {
                track.clip_ids = command_before
                    .as_ref()
                    .map_or_else(Vec::new, |track| track.clip_ids.clone());
            }
            out.push(DomainCommand::Arrangement(ArrangementOperation::PutTrack {
                before: command_before,
                after: command_after,
            }));
        }
    }
    for id in after.domains.arrangement.clips.keys() {
        let left = before.domains.arrangement.clips.get(id);
        let right = after.domains.arrangement.clips.get(id);
        if left != right {
            out.push(DomainCommand::Arrangement(ArrangementOperation::PutClip {
                before: left.cloned(),
                after: right.cloned(),
            }));
        }
    }
}

#[derive(Debug)]
pub enum ConstructiveControllerError {
    RevisionConflict {
        expected: u64,
        actual: u64,
    },
    KitRevisionConflict {
        kit: KitId,
        expected: u64,
        actual: u64,
    },
    MissingAsset(assets::AssetId),
    MissingPcm(assets::AssetId),
    MissingKit(KitId),
    MissingPad {
        kit: KitId,
        pad: PadId,
    },
    MissingZone {
        kit: KitId,
        zone: ZoneId,
    },
    MissingBus(crate::mixer::BusId),
    MissingPublishedPattern,
    MissingPublishedArrangementClip,
    InvalidSourceRange,
    InvalidOnsetSettings,
    InvalidEnvelope,
    InvalidZonePlayback,
    EmptyChop,
    EmptyTransition,
    TimingOverflow,
    Material(String),
    MaterialIdentity {
        zone: ZoneId,
        expected: CanonicalPcmIdentity,
        actual: CanonicalPcmIdentity,
    },
    Plan(String),
    Lower(String),
    Controller(ProjectControllerError),
    Internal(String),
}

impl fmt::Display for ConstructiveControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for ConstructiveControllerError {}

impl From<ProjectControllerError> for ConstructiveControllerError {
    fn from(error: ProjectControllerError) -> Self {
        Self::Controller(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata,
    };
    use crate::audio::AudioFormat;
    use crate::daw_project::ProjectDomain;
    use crate::live_project::{LiveProject, ProjectControllerConfig, SourceMaterialMetadata};
    use crate::sample_actions::{MakeBeatResultFocus, ZoneEditTarget};

    fn controller_with_source() -> (ProjectController, assets::AssetId) {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/controller-source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = assets::AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "controller source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 48_000,
                    channels: 1,
                    frame_count: SampleFrames(8),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"constructive controller source"),
                provenance: AssetProvenance::new(
                    1,
                    AssetOrigin::ImportedFile {
                        importer: "test".into(),
                    },
                    location,
                ),
                tags: BTreeSet::new(),
                favorite: false,
            })
            .unwrap();
        let pcm = PcmAsset::new(
            AudioFormat::new(48_000, 1).unwrap(),
            Arc::from([0.0, 0.8, 0.2, 0.0, 0.0, 0.6, 0.1, 0.0]),
        )
        .unwrap();
        let live = LiveProject::from_source_material(
            SourceMaterialMetadata::new("Controller", "Source"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        (
            ProjectController::with_config(live, ProjectControllerConfig::default()).unwrap(),
            asset,
        )
    }

    fn make_beat_request(asset: assets::AssetId, id: u64) -> SampleActionRequest {
        SampleActionRequest {
            id: SampleRequestId(id),
            action: SampleAction::MakeBeat(MakeBeatIntent {
                source: SampleSelection::whole_asset(asset),
                chop: SampleChopIntent::EqualSlices { count: 2 },
                kit: SampleKitDestination::NewKit,
                target_bus: None,
                bars: 1,
                quantize_ticks: sequencer::PPQ as u64,
                result_focus: MakeBeatResultFocus::PatternEditor,
            }),
        }
    }

    #[test]
    fn background_sample_work_and_prepared_result_are_sendable() {
        fn assert_send<T: Send>() {}
        assert_send::<SampleActionBackgroundWork>();
        assert_send::<PreparedSampleAction>();
        assert_send::<Result<PreparedSampleAction, ConstructiveControllerError>>();
    }

    #[test]
    fn prepared_make_beat_has_a_short_revision_guarded_commit_boundary() {
        let (mut controller, asset) = controller_with_source();
        let first = controller
            .capture_sample_action_work(make_beat_request(asset, 1))
            .unwrap()
            .prepare()
            .unwrap();
        let stale = controller
            .capture_sample_action_work(make_beat_request(asset, 2))
            .unwrap()
            .prepare()
            .unwrap();

        let outcome = controller.commit_prepared_sample_action(first).unwrap();
        assert!(matches!(outcome, SampleActionOutcome::Published(_)));
        assert!(matches!(
            controller.commit_prepared_sample_action(stale),
            Err(ConstructiveControllerError::RevisionConflict { .. })
        ));
    }

    #[test]
    fn make_beat_publishes_every_domain_and_exact_pcm_as_one_undo_step() {
        let (mut controller, asset) = controller_with_source();
        let initial_revision = controller.revisions().aggregate;
        let outcome = controller
            .execute_sample_action(SampleAction::MakeBeat(MakeBeatIntent {
                source: SampleSelection::whole_asset(asset),
                chop: SampleChopIntent::EqualSlices { count: 2 },
                kit: SampleKitDestination::NewKit,
                target_bus: None,
                bars: 1,
                quantize_ticks: sequencer::PPQ as u64,
                result_focus: MakeBeatResultFocus::PatternEditor,
            }))
            .unwrap();
        let SampleActionOutcome::Published(outcome) = outcome else {
            panic!("make beat must publish a constructive edit")
        };
        assert_eq!(outcome.publication.revision, initial_revision + 1);
        assert!(outcome.publication.pattern.is_some());
        assert!(outcome.publication.arrangement_clip.is_some());
        assert_eq!(outcome.publication.created_pads.len(), 2);
        assert_eq!(outcome.publication.created_zones.len(), 2);
        assert!(outcome
            .publication
            .created_zones
            .iter()
            .all(|target| target.kit == outcome.publication.kit
                && outcome.publication.created_pads.contains(&target.pad)));
        assert!(outcome.publication.sequencer_clip.is_some());
        assert!(outcome.publication.arrangement_track.is_some());
        assert!(outcome.publication.output_bus.is_some());
        for domain in [
            ProjectDomain::SampleKits,
            ProjectDomain::Assets,
            ProjectDomain::Bindings,
            ProjectDomain::Sequencer,
            ProjectDomain::Arrangement,
            ProjectDomain::Mixer,
        ] {
            assert!(outcome.update.change_set.domains.contains(&domain));
        }
        let published_kit = outcome.publication.kit;
        let published_pattern = outcome.publication.pattern.unwrap();
        let published_clip = outcome.publication.arrangement_clip.unwrap();
        assert_eq!(
            controller
                .snapshot()
                .project
                .state()
                .bindings
                .patterns
                .placements
                .get(&published_clip)
                .copied(),
            outcome.publication.sequencer_clip
        );
        assert_eq!(controller.snapshot().sample_pcm.len(), 2);
        assert_eq!(controller.journal_records().len(), 1);

        controller.undo().unwrap().unwrap();
        assert!(controller
            .snapshot()
            .project
            .state()
            .domains
            .sample_kits
            .kits
            .is_empty());
        assert!(controller.snapshot().sample_pcm.is_empty());
        assert!(!controller.can_undo());

        controller.redo().unwrap().unwrap();
        let redone = controller.snapshot();
        assert!(redone
            .project
            .state()
            .domains
            .sample_kits
            .kits
            .contains_key(&published_kit));
        assert!(redone
            .project
            .state()
            .domains
            .sequencer
            .patterns()
            .get(published_pattern)
            .is_some());
        assert!(redone
            .project
            .state()
            .domains
            .arrangement
            .clips
            .contains_key(&published_clip));
        redone.project.require_valid().unwrap();
        assert_eq!(controller.snapshot().sample_pcm.len(), 2);
        assert_eq!(controller.journal_records().len(), 3);

        // A second cycle proves both halves of history refresh their
        // domain-local optimistic revisions after every application.
        controller.undo().unwrap().unwrap();
        assert!(controller.snapshot().sample_pcm.is_empty());
        controller.redo().unwrap().unwrap();
        assert_eq!(controller.snapshot().sample_pcm.len(), 2);
        assert_eq!(controller.journal_records().len(), 5);
    }

    #[test]
    fn pad_choke_and_zone_playback_edits_publish_authoritative_kit_revisions() {
        let (mut controller, asset) = controller_with_source();
        let published = controller
            .execute_sample_action(make_beat_request(asset, 1).action)
            .unwrap();
        let SampleActionOutcome::Published(published) = published else {
            panic!("fixture make beat must publish")
        };
        let kit_id = published.publication.kit;
        let target = published.publication.created_zones[0];
        let initial_kit = controller
            .snapshot()
            .project
            .state()
            .domains
            .sample_kits
            .kits
            .get(&kit_id)
            .unwrap()
            .clone();

        let playback = controller
            .execute_sample_action(SampleAction::EditZone(ZoneEditIntent::SetPlayback {
                target: ZoneEditTarget {
                    kit: kit_id,
                    pad: target.pad,
                    zone: target.zone,
                    expected_revision: initial_kit.revision,
                },
                gain_db: -4.5,
                pan: 0.35,
                tuning_cents: -250.0,
            }))
            .unwrap();
        assert!(matches!(playback, SampleActionOutcome::Published(_)));
        let playback_kit = controller
            .snapshot()
            .project
            .state()
            .domains
            .sample_kits
            .kits
            .get(&kit_id)
            .unwrap()
            .clone();
        let zone = playback_kit.zones.get(&target.zone).unwrap();
        assert_eq!(zone.gain_db, -4.5);
        assert_eq!(zone.pan, 0.35);
        assert_eq!(zone.tuning_cents, -250.0);
        assert_eq!(playback_kit.revision, initial_kit.revision + 1);

        let choke = controller
            .execute_sample_action(SampleAction::SetPadChoke {
                kit: kit_id,
                pad: target.pad,
                choke_group: std::num::NonZeroU32::new(3),
                expected_revision: playback_kit.revision,
            })
            .unwrap();
        assert!(matches!(choke, SampleActionOutcome::Published(_)));
        let final_snapshot = controller.snapshot();
        let final_kit = final_snapshot
            .project
            .state()
            .domains
            .sample_kits
            .kits
            .get(&kit_id)
            .unwrap();
        assert_eq!(final_kit.pads[&target.pad].choke_group, Some(3));
        assert_eq!(final_kit.revision, playback_kit.revision + 1);
    }
}
