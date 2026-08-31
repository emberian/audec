//! Completion adapters for durable creation and promotion receipts.
//!
//! The command kernel deliberately returns project revisions, while product
//! completion requires a concrete object to reveal. This module preserves
//! both: adapters inspect exact put-style commands or typed receipts after a
//! successful commit and return [`RevealRecommendation`] without letting UI
//! concerns leak into domain commands. It does not infer object identities
//! from labels, command text, or equal raw integers across domains.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::arrangement::{ArrangementOperation, ClipContent};
use crate::arrangement_view::ArrangementViewEvent;
use crate::artifact_catalog::ArtifactId;
use crate::assets::{AssetError, AssetId, AssetRegistration, AssetRegistry};
use crate::command::{BindingCommand, CommandEnvelope, DomainCommand};
use crate::comparison::ComparisonId;
use crate::comparison_runtime::ComparisonExecution;
use crate::constructive::{ConstructiveApplicationReceipt, ConstructiveFocus};
use crate::daw_project::{LegacyMigrationReport, ProjectRevisions, ProjectState};
use crate::interpretation::{InterpretationCommand, InterpretationError, InterpretationStore};
use crate::pattern_actions::{PatternAction, PatternActionIntent, PatternEditorTarget};
use crate::pattern_controller::{
    lower_pattern_action, LoweredPatternAction, PatternActionSnapshot, PatternLoweringError,
};
use crate::project_session::{ProjectSession, ProjectSessionError};
use crate::reading::ReadingFile;
use crate::sample_material::SourceMaterialRef;
use crate::sequencer::SequencerCommand;

use super::object_navigation::{
    FindingKind, FindingLocalId, FindingRef, FindingScope, InstrumentRef, ObjectKind, ObjectRef,
    PadRef, PatternOccurrenceRef, RevealIntent, RevealRecommendation, RevealRequest,
};
use super::{
    lower_arrangement_event, ArrangementDispatch, ArrangementExecution, ArrangementExecutionError,
    ArrangementHistoryKind,
};

/// The legacy return shape a caller sees if it bypasses this adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurrentTerminal {
    ExactReceipt,
    TypedIdOnly,
    CommandEnvelopeOnly,
    RevisionOnly,
    InverseCommandsOnly,
    DecodedFileOnly,
    ArtifactIdOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevealIntegration {
    NativeReceiptAdapter,
    AdapterAvailable,
    /// A best available durable neighbor is revealable, but the exact created
    /// object has no `ObjectRef` variant yet (currently automation clips).
    PartialAdapter,
    /// The domain is durable in its own store/file, but is not yet an
    /// aggregate `DawProject` command domain.
    DetachedDurableStore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DurableFlow {
    ManualSample,
    OnsetChop,
    MakeBeat,
    RhythmPromotion,
    ReconstructionPromotion,
    PatternCreate,
    PatternDuplicate,
    ArrangementTrackCreate,
    ArrangementAudioClipDuplicate,
    ArrangementPatternClipDuplicate,
    ArrangementAutomationClipDuplicate,
    ArrangementAudioClipSplit,
    ArrangementPatternClipSplit,
    ArrangementAutomationClipSplit,
    ArrangementMediaInsert,
    ArrangementPatternInsert,
    AssetImport,
    /// A repeated registration currently creates another typed material ID;
    /// content-fingerprint duplicate detection is advisory, not deduplication.
    AssetRepeatedImport,
    ExplanationCreate,
    ComparisonCreate,
    ComparisonResolve,
    CoveragePublish,
    ReadingImport,
    LegacyProjectMigration,
}

/// Machine-readable receipt inventory. Tests treat this as the exhaustive
/// product contract for the creation/promotion flows audited in Cycle 6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurableRevealRule {
    pub flow: DurableFlow,
    pub current_terminal: CurrentTerminal,
    pub primary: ObjectKind,
    pub intent: RevealIntent,
    pub integration: RevealIntegration,
    pub adapter: &'static str,
}

pub const fn durable_reveal_rules() -> &'static [DurableRevealRule] {
    &DURABLE_REVEAL_RULES
}

const DURABLE_REVEAL_RULES: [DurableRevealRule; 24] = [
    native(
        DurableFlow::ManualSample,
        ObjectKind::Pad,
        "recommend_sample_result",
    ),
    native(
        DurableFlow::OnsetChop,
        ObjectKind::Instrument,
        "recommend_sample_result",
    ),
    native(
        DurableFlow::MakeBeat,
        ObjectKind::PatternOccurrence,
        "recommend_constructive",
    ),
    native(
        DurableFlow::RhythmPromotion,
        ObjectKind::PatternOccurrence,
        "recommend_constructive",
    ),
    native(
        DurableFlow::ReconstructionPromotion,
        ObjectKind::Pattern,
        "recommend_reconstruction",
    ),
    envelope(DurableFlow::PatternCreate, ObjectKind::Pattern),
    envelope(DurableFlow::PatternDuplicate, ObjectKind::Pattern),
    revision(DurableFlow::ArrangementTrackCreate, ObjectKind::Track),
    revision(
        DurableFlow::ArrangementAudioClipDuplicate,
        ObjectKind::AudioClip,
    ),
    revision(
        DurableFlow::ArrangementPatternClipDuplicate,
        ObjectKind::PatternOccurrence,
    ),
    partial_revision(
        DurableFlow::ArrangementAutomationClipDuplicate,
        ObjectKind::Automation,
    ),
    revision(
        DurableFlow::ArrangementAudioClipSplit,
        ObjectKind::AudioClip,
    ),
    revision(
        DurableFlow::ArrangementPatternClipSplit,
        ObjectKind::PatternOccurrence,
    ),
    partial_revision(
        DurableFlow::ArrangementAutomationClipSplit,
        ObjectKind::Automation,
    ),
    revision(DurableFlow::ArrangementMediaInsert, ObjectKind::AudioClip),
    revision(
        DurableFlow::ArrangementPatternInsert,
        ObjectKind::PatternOccurrence,
    ),
    id_only(DurableFlow::AssetImport, ObjectKind::Material),
    id_only(DurableFlow::AssetRepeatedImport, ObjectKind::Material),
    detached(
        DurableFlow::ExplanationCreate,
        CurrentTerminal::InverseCommandsOnly,
        ObjectKind::Explanation,
        "apply_interpretation_revealed",
    ),
    detached(
        DurableFlow::ComparisonCreate,
        CurrentTerminal::InverseCommandsOnly,
        ObjectKind::Comparison,
        "apply_interpretation_revealed",
    ),
    detached(
        DurableFlow::ComparisonResolve,
        CurrentTerminal::InverseCommandsOnly,
        ObjectKind::Comparison,
        "apply_interpretation_revealed",
    ),
    detached(
        DurableFlow::CoveragePublish,
        CurrentTerminal::ArtifactIdOnly,
        ObjectKind::Finding,
        "recommend_coverage_artifact",
    ),
    detached(
        DurableFlow::ReadingImport,
        CurrentTerminal::DecodedFileOnly,
        ObjectKind::Reading,
        "recommend_reading",
    ),
    DurableRevealRule {
        flow: DurableFlow::LegacyProjectMigration,
        current_terminal: CurrentTerminal::ExactReceipt,
        primary: ObjectKind::AudioClip,
        intent: RevealIntent::ActivateExisting,
        integration: RevealIntegration::AdapterAvailable,
        adapter: "recommend_legacy_migration",
    },
];

const fn native(
    flow: DurableFlow,
    primary: ObjectKind,
    adapter: &'static str,
) -> DurableRevealRule {
    DurableRevealRule {
        flow,
        current_terminal: CurrentTerminal::ExactReceipt,
        primary,
        intent: RevealIntent::ActivateExisting,
        integration: RevealIntegration::NativeReceiptAdapter,
        adapter,
    }
}

const fn envelope(flow: DurableFlow, primary: ObjectKind) -> DurableRevealRule {
    DurableRevealRule {
        flow,
        current_terminal: CurrentTerminal::CommandEnvelopeOnly,
        primary,
        intent: RevealIntent::ActivateExisting,
        integration: RevealIntegration::AdapterAvailable,
        adapter: "execute_pattern_action_revealed",
    }
}

const fn revision(flow: DurableFlow, primary: ObjectKind) -> DurableRevealRule {
    DurableRevealRule {
        flow,
        current_terminal: CurrentTerminal::RevisionOnly,
        primary,
        intent: RevealIntent::ActivateExisting,
        integration: RevealIntegration::AdapterAvailable,
        adapter: "execute_arrangement_event_revealed",
    }
}

const fn partial_revision(flow: DurableFlow, primary: ObjectKind) -> DurableRevealRule {
    DurableRevealRule {
        flow,
        current_terminal: CurrentTerminal::RevisionOnly,
        primary,
        intent: RevealIntent::ActivateExisting,
        integration: RevealIntegration::PartialAdapter,
        adapter: "execute_arrangement_event_revealed",
    }
}

const fn id_only(flow: DurableFlow, primary: ObjectKind) -> DurableRevealRule {
    DurableRevealRule {
        flow,
        current_terminal: CurrentTerminal::TypedIdOnly,
        primary,
        intent: RevealIntent::ActivateExisting,
        integration: RevealIntegration::AdapterAvailable,
        adapter: "recommend_asset",
    }
}

const fn detached(
    flow: DurableFlow,
    terminal: CurrentTerminal,
    primary: ObjectKind,
    adapter: &'static str,
) -> DurableRevealRule {
    DurableRevealRule {
        flow,
        current_terminal: terminal,
        primary,
        intent: RevealIntent::ActivateExisting,
        integration: RevealIntegration::DetachedDurableStore,
        adapter,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectMutationReceipt {
    pub revisions: ProjectRevisions,
    pub reveal: Option<RevealRecommendation>,
}

/// Execute a validated command once and derive its completion target from the
/// exact after-state. Ordinary edits naturally return `reveal: None`; every
/// creation recognized by the inventory returns a typed recommendation.
pub fn execute_envelope_revealed(
    session: &mut ProjectSession,
    envelope: CommandEnvelope,
) -> Result<ProjectMutationReceipt, ProjectSessionError> {
    let receipt_envelope = envelope.clone();
    let revisions = session.execute(envelope)?;
    let reveal = recommend_command_result(
        &receipt_envelope,
        session.project_snapshot()?.project.state(),
    );
    Ok(ProjectMutationReceipt { revisions, reveal })
}

#[derive(Clone, Debug)]
pub struct ArrangementRevealReceipt {
    pub execution: ArrangementExecution,
    pub reveal: Option<RevealRecommendation>,
}

/// Rich replacement for the revision-only arrangement executor. Existing
/// callers may migrate without changing arrangement lowering or history.
pub fn execute_arrangement_event_revealed(
    session: &mut ProjectSession,
    event: ArrangementViewEvent,
) -> Result<ArrangementRevealReceipt, ArrangementExecutionError> {
    let snapshot = session
        .project_snapshot()
        .map_err(ArrangementExecutionError::Session)?;
    let dispatch =
        lower_arrangement_event(snapshot, event).map_err(ArrangementExecutionError::Lowering)?;
    match dispatch {
        ArrangementDispatch::Apply(validated) => {
            let result = execute_envelope_revealed(session, validated.envelope)
                .map_err(ArrangementExecutionError::Session)?;
            Ok(ArrangementRevealReceipt {
                execution: ArrangementExecution::ProjectChanged(result.revisions),
                reveal: result.reveal,
            })
        }
        ArrangementDispatch::History(history) => {
            let revision = match history.kind {
                ArrangementHistoryKind::Undo => session.undo(),
                ArrangementHistoryKind::Redo => session.redo(),
            }
            .map_err(ArrangementExecutionError::Session)?;
            Ok(ArrangementRevealReceipt {
                execution: revision.map_or(
                    ArrangementExecution::HistoryUnchanged(history.kind),
                    ArrangementExecution::ProjectChanged,
                ),
                reveal: None,
            })
        }
        ArrangementDispatch::SelectionOnly => Ok(ArrangementRevealReceipt {
            execution: ArrangementExecution::SelectionOnly,
            reveal: None,
        }),
        ArrangementDispatch::Seek(frame) => Ok(ArrangementRevealReceipt {
            execution: ArrangementExecution::Seek(frame),
            reveal: None,
        }),
    }
}

#[derive(Clone, Debug)]
pub enum PatternRevealExecution {
    ProjectChanged(ProjectMutationReceipt),
    HistoryChanged(Option<ProjectRevisions>),
    Retarget(PatternEditorTarget),
    PreviewCycle {
        target: PatternEditorTarget,
        cycle_index: u64,
        performance_seed: u64,
    },
}

#[derive(Debug)]
pub enum PatternRevealExecutionError {
    Lowering(PatternLoweringError),
    Session(ProjectSessionError),
}

impl fmt::Display for PatternRevealExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lowering(error) => write!(formatter, "pattern command refused: {error}"),
            Self::Session(error) => write!(formatter, "pattern publication failed: {error}"),
        }
    }
}

impl Error for PatternRevealExecutionError {}

pub fn execute_pattern_action_revealed(
    session: &mut ProjectSession,
    intent: &PatternActionIntent,
) -> Result<PatternRevealExecution, PatternRevealExecutionError> {
    let lowered = {
        let snapshot = session
            .project_snapshot()
            .map_err(PatternRevealExecutionError::Session)?;
        lower_pattern_action(
            PatternActionSnapshot::from_project(&snapshot.project),
            intent,
        )
        .map_err(PatternRevealExecutionError::Lowering)?
    };
    match lowered {
        LoweredPatternAction::Execute(envelope) => {
            let mut receipt = execute_envelope_revealed(session, envelope)
                .map_err(PatternRevealExecutionError::Session)?;
            if let (PatternAction::Duplicate { source, .. }, Some(reveal)) =
                (&intent.action, receipt.reveal.as_mut())
            {
                let source = ObjectRef::Pattern(*source);
                if reveal.request.object != source && !reveal.request.related.contains(&source) {
                    reveal.request.related.push(source);
                }
            }
            Ok(PatternRevealExecution::ProjectChanged(receipt))
        }
        LoweredPatternAction::Undo => session
            .undo()
            .map(PatternRevealExecution::HistoryChanged)
            .map_err(PatternRevealExecutionError::Session),
        LoweredPatternAction::Redo => session
            .redo()
            .map(PatternRevealExecution::HistoryChanged)
            .map_err(PatternRevealExecutionError::Session),
        LoweredPatternAction::Retarget(target) => Ok(PatternRevealExecution::Retarget(target)),
        LoweredPatternAction::PreviewCycle {
            target,
            cycle_index,
            performance_seed,
        } => Ok(PatternRevealExecution::PreviewCycle {
            target,
            cycle_index,
            performance_seed,
        }),
    }
}

/// Derive a completion target from all entities created by one aggregate
/// envelope. The primary is deterministic and the remaining creations and
/// provenance neighbors stay in `related` for Inspector/breadcrumbs.
pub fn recommend_command_result(
    envelope: &CommandEnvelope,
    state: &ProjectState,
) -> Option<RevealRecommendation> {
    let mut pattern_aliases = BTreeMap::new();
    let mut placements = BTreeMap::new();
    let mut sequencer_clips = BTreeMap::new();
    let mut media_aliases = BTreeMap::new();
    for command in &envelope.commands {
        match command {
            DomainCommand::Bindings(BindingCommand::PutPatternDefinitionAlias {
                alias,
                after: Some(pattern),
                ..
            }) => {
                pattern_aliases.insert(*alias, *pattern);
            }
            DomainCommand::Bindings(BindingCommand::PutPatternPlacement {
                clip,
                after: Some(sequence_clip),
                ..
            }) => {
                placements.insert(*clip, *sequence_clip);
            }
            DomainCommand::Bindings(BindingCommand::PutMediaAssetAlias {
                alias,
                after: Some(asset),
                ..
            }) => {
                media_aliases.insert(*alias, *asset);
            }
            DomainCommand::Sequencer(SequencerCommand::PutClip {
                before: None,
                after: Some(clip),
            }) => {
                sequencer_clips.insert(clip.id, clip.pattern);
            }
            _ => {}
        }
    }

    let mut ranked = Vec::<(u8, ObjectRef)>::new();
    for command in &envelope.commands {
        match command {
            DomainCommand::Arrangement(ArrangementOperation::PutClip {
                before: None,
                after: Some(clip),
            }) => match &clip.content {
                ClipContent::Pattern(region) => {
                    let sequencer_clip = placements
                        .get(&clip.id)
                        .copied()
                        .or_else(|| state.bindings.patterns.placements.get(&clip.id).copied());
                    let pattern = sequencer_clip
                        .and_then(|id| sequencer_clips.get(&id).copied())
                        .or_else(|| pattern_aliases.get(&region.pattern).copied())
                        .or_else(|| {
                            state
                                .bindings
                                .patterns
                                .definitions
                                .get(&region.pattern)
                                .copied()
                        });
                    ranked.push((
                        0,
                        ObjectRef::PatternOccurrence(PatternOccurrenceRef {
                            arrangement_clip: clip.id,
                            sequencer_clip,
                            pattern,
                        }),
                    ));
                    ranked.push((6, ObjectRef::Track(clip.track_id)));
                    if let Some(pattern) = pattern {
                        ranked.push((5, ObjectRef::Pattern(pattern)));
                    }
                }
                ClipContent::Audio(audio) => {
                    ranked.push((1, ObjectRef::AudioClip(clip.id)));
                    ranked.push((6, ObjectRef::Track(clip.track_id)));
                    if let Some(asset) = media_aliases.get(&audio.asset).copied().or_else(|| {
                        state
                            .bindings
                            .assets
                            .arrangement_assets
                            .get(&audio.asset)
                            .copied()
                    }) {
                        ranked.push((7, ObjectRef::Material(asset)));
                    }
                }
                ClipContent::Automation(region) => {
                    let lane = state
                        .bindings
                        .automation
                        .lanes
                        .get(&region.parameter)
                        .copied();
                    if let Some(lane) = lane {
                        ranked.push((2, ObjectRef::Automation(lane)));
                    }
                    ranked.push((6, ObjectRef::Track(clip.track_id)));
                }
            },
            DomainCommand::Arrangement(ArrangementOperation::PutTrack {
                before: None,
                after: Some(track),
            }) => {
                ranked.push((4, ObjectRef::Track(track.id)));
                if let Some(bus) = state.bindings.mixer.tracks.get(&track.id).copied() {
                    ranked.push((7, ObjectRef::Bus(bus)));
                }
            }
            DomainCommand::Sequencer(SequencerCommand::PutPattern {
                before: None,
                after: Some(pattern),
            }) => ranked.push((3, ObjectRef::Pattern(pattern.id))),
            DomainCommand::Automation(command) => {
                for change in &command.changes {
                    if change.before.is_none() {
                        if let Some(lane) = &change.after {
                            ranked.push((3, ObjectRef::Automation(lane.id)));
                        }
                    }
                }
            }
            DomainCommand::SampleKits(put) if put.before.is_none() => {
                if let Some(kit) = &put.after {
                    ranked.push((3, ObjectRef::Instrument(InstrumentRef::SampleKit(kit.id))));
                    for pad in kit.pad_order.iter().copied() {
                        ranked.push((
                            5,
                            ObjectRef::Pad(PadRef {
                                kit: kit.id,
                                pad,
                                zone: None,
                            }),
                        ));
                    }
                }
            }
            DomainCommand::Assets(crate::command::AssetCommand::PutAsset {
                id,
                before: None,
                after: Some(_),
            }) => ranked.push((5, ObjectRef::Material(*id))),
            _ => {}
        }
    }
    ranked.sort_by_key(|(rank, object)| (*rank, object.address()));
    let (_, primary) = ranked.first()?.clone();
    let related = ranked.into_iter().map(|(_, object)| object);
    Some(RevealRecommendation {
        request: RevealRequest::new(primary, RevealIntent::ActivateExisting).with_related(related),
        diagnostics: Vec::new(),
    })
}

pub fn recommend_asset(asset: AssetId) -> RevealRecommendation {
    RevealRecommendation {
        request: RevealRequest::new(ObjectRef::Material(asset), RevealIntent::ActivateExisting),
        diagnostics: Vec::new(),
    }
}

pub fn register_asset_revealed(
    registry: &mut AssetRegistry,
    registration: AssetRegistration,
) -> Result<(AssetId, RevealRecommendation), AssetError> {
    let asset = registry.register(registration)?;
    Ok((asset, recommend_asset(asset)))
}

/// Preserve the richer identities retained by the domain-level constructive
/// receipt. Controller publications intentionally stay compact, but callers
/// applying a prepared plan directly should not lose its occurrence, route,
/// pad cohort, or exact source-material breadcrumbs.
pub fn recommend_constructive_application(
    receipt: &ConstructiveApplicationReceipt,
) -> RevealRecommendation {
    let kit = ObjectRef::Instrument(InstrumentRef::SampleKit(receipt.bindings.kit));
    let pattern = receipt.bindings.pattern.map(ObjectRef::Pattern);
    let occurrence = receipt.bindings.arrangement_clip.map(|arrangement_clip| {
        ObjectRef::PatternOccurrence(PatternOccurrenceRef {
            arrangement_clip,
            sequencer_clip: receipt.bindings.sequencer_clip,
            pattern: receipt.bindings.pattern,
        })
    });
    let focused_pad = match receipt.focus {
        ConstructiveFocus::Pad(pad) => Some(ObjectRef::Pad(PadRef {
            kit: receipt.bindings.kit,
            pad,
            zone: None,
        })),
        _ => None,
    };
    let primary = match receipt.focus {
        ConstructiveFocus::Kit => kit.clone(),
        ConstructiveFocus::Pad(_) => focused_pad.clone().unwrap_or_else(|| kit.clone()),
        ConstructiveFocus::Pattern(_) => pattern
            .clone()
            .or_else(|| occurrence.clone())
            .unwrap_or_else(|| kit.clone()),
    };
    let related = std::iter::once(kit)
        .chain(focused_pad)
        .chain(pattern)
        .chain(occurrence)
        .chain(receipt.bindings.pad_samples.keys().copied().map(|pad| {
            ObjectRef::Pad(PadRef {
                kit: receipt.bindings.kit,
                pad,
                zone: None,
            })
        }))
        .chain(
            receipt
                .materials
                .iter()
                .map(|material| ObjectRef::Sample(SourceMaterialRef::VirtualSlice(material.slice))),
        )
        .chain(receipt.bindings.arrangement_track.map(ObjectRef::Track))
        .chain(std::iter::once(ObjectRef::Bus(receipt.bindings.output_bus)));
    RevealRecommendation {
        request: RevealRequest::new(primary, RevealIntent::ActivateExisting)
            .at_revision(receipt.project_revision)
            .with_related(related),
        diagnostics: Vec::new(),
    }
}

pub fn recommend_interpretation_commands(
    commands: &[InterpretationCommand],
) -> Option<RevealRecommendation> {
    let mut ranked = Vec::<(u8, ObjectRef)>::new();
    for command in commands {
        match command {
            InterpretationCommand::PutExplanation {
                before: None,
                after: Some(definition),
            } => ranked.push((2, ObjectRef::Explanation(definition.id))),
            InterpretationCommand::PutComparison {
                before: None,
                after: Some(comparison),
            } => {
                ranked.push((0, ObjectRef::Comparison(comparison.id)));
                ranked.push((3, ObjectRef::Explanation(comparison.explanation)));
                ranked.push((4, ObjectRef::Material(comparison.source.asset)));
                ranked.push((
                    5,
                    ObjectRef::Sample(SourceMaterialRef::VirtualSlice(
                        crate::sample_material::VirtualSliceRef {
                            source_asset: comparison.source.asset,
                            source_range: comparison.source.source_range,
                        },
                    )),
                ));
            }
            InterpretationCommand::PutObservation {
                comparison,
                after: Some(_),
                ..
            } => {
                ranked.push((1, ObjectRef::Comparison(*comparison)));
            }
            _ => {}
        }
    }
    ranked.sort_by_key(|(rank, object)| (*rank, object.address()));
    let (_, primary) = ranked.first()?.clone();
    let related = ranked.into_iter().map(|(_, object)| object);
    Some(RevealRecommendation {
        request: RevealRequest::new(primary, RevealIntent::ActivateExisting).with_related(related),
        diagnostics: Vec::new(),
    })
}

#[derive(Clone, Debug)]
pub struct InterpretationRevealReceipt {
    pub inverse: Vec<InterpretationCommand>,
    pub reveal: Option<RevealRecommendation>,
}

pub fn apply_interpretation_revealed(
    store: &mut InterpretationStore,
    commands: &[InterpretationCommand],
) -> Result<InterpretationRevealReceipt, InterpretationError> {
    let reveal = recommend_interpretation_commands(commands);
    let inverse = store.apply(commands)?;
    Ok(InterpretationRevealReceipt { inverse, reveal })
}

pub fn recommend_comparison_execution(execution: &ComparisonExecution) -> RevealRecommendation {
    RevealRecommendation {
        request: RevealRequest::new(
            ObjectRef::Comparison(execution.comparison),
            RevealIntent::ActivateExisting,
        )
        .with_related([ObjectRef::Explanation(execution.explanation)]),
        diagnostics: Vec::new(),
    }
}

pub fn recommend_coverage_artifact(
    artifact: ArtifactId,
    comparison: ComparisonId,
) -> RevealRecommendation {
    let finding = ObjectRef::Finding(FindingRef {
        kind: FindingKind::Other,
        scope: FindingScope::Artifact(artifact),
        local: FindingLocalId::Claim(comparison.0),
    });
    RevealRecommendation {
        request: RevealRequest::new(finding, RevealIntent::ActivateExisting)
            .with_related([ObjectRef::Comparison(comparison)]),
        diagnostics: Vec::new(),
    }
}

pub fn recommend_reading(reading: &ReadingFile) -> RevealRecommendation {
    RevealRecommendation {
        request: RevealRequest::new(
            ObjectRef::Reading(reading.reading_id),
            RevealIntent::ActivateExisting,
        ),
        diagnostics: Vec::new(),
    }
}

pub fn recommend_legacy_migration(report: &LegacyMigrationReport) -> Option<RevealRecommendation> {
    let mut clips = report.clips.values().copied().collect::<Vec<_>>();
    clips.sort();
    let mut tracks = report.tracks.values().copied().collect::<Vec<_>>();
    tracks.sort();
    let primary = clips
        .first()
        .copied()
        .map(ObjectRef::AudioClip)
        .or_else(|| tracks.first().copied().map(ObjectRef::Track))?;
    let related = clips
        .into_iter()
        .map(ObjectRef::AudioClip)
        .chain(tracks.into_iter().map(ObjectRef::Track));
    Some(RevealRecommendation {
        request: RevealRequest::new(primary, RevealIntent::ActivateExisting).with_related(related),
        diagnostics: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use crate::arrangement::TrackKind;
    use crate::arrangement_view::{ArrangementAction, ArrangementActionIntent};
    use crate::daw_engine::AssetPcmMap;
    use crate::daw_project::DawProject;
    use crate::live_project::LiveProject;
    use crate::pattern_actions::{CreatePatternIntent, PatternAction, PatternEditorMode};
    use crate::project_session::ProjectSessionId;
    use crate::sequencer::{BeatDuration, PPQ};

    fn session() -> ProjectSession {
        let project = DawProject::new("receipt navigation", 48_000, 120.0).unwrap();
        let live = LiveProject::from_project(project, AssetPcmMap::default()).unwrap();
        let mut session = ProjectSession::new(ProjectSessionId(601)).unwrap();
        session.install(live, None).unwrap();
        session
    }

    #[test]
    fn inventory_has_one_machine_rule_per_audited_flow() {
        let rules = durable_reveal_rules();
        let flows = rules.iter().map(|rule| rule.flow).collect::<BTreeSet<_>>();
        assert_eq!(flows.len(), rules.len());
        assert!(rules.iter().all(|rule| !rule.adapter.is_empty()));
        assert!(rules
            .iter()
            .all(|rule| rule.intent == RevealIntent::ActivateExisting));

        let revision_only = rules
            .iter()
            .filter(|rule| rule.current_terminal == CurrentTerminal::RevisionOnly)
            .map(|rule| rule.flow)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            revision_only,
            BTreeSet::from([
                DurableFlow::ArrangementTrackCreate,
                DurableFlow::ArrangementAudioClipDuplicate,
                DurableFlow::ArrangementPatternClipDuplicate,
                DurableFlow::ArrangementAutomationClipDuplicate,
                DurableFlow::ArrangementAudioClipSplit,
                DurableFlow::ArrangementPatternClipSplit,
                DurableFlow::ArrangementAutomationClipSplit,
                DurableFlow::ArrangementMediaInsert,
                DurableFlow::ArrangementPatternInsert,
            ])
        );
        assert!(rules.iter().all(|rule| {
            rule.integration != RevealIntegration::DetachedDurableStore
                || matches!(
                    rule.flow,
                    DurableFlow::ExplanationCreate
                        | DurableFlow::ComparisonCreate
                        | DurableFlow::ComparisonResolve
                        | DurableFlow::CoveragePublish
                        | DurableFlow::ReadingImport
                )
        }));
        let partial = rules
            .iter()
            .filter(|rule| rule.integration == RevealIntegration::PartialAdapter)
            .map(|rule| rule.flow)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            partial,
            BTreeSet::from([
                DurableFlow::ArrangementAutomationClipDuplicate,
                DurableFlow::ArrangementAutomationClipSplit,
            ])
        );
    }

    #[test]
    fn pattern_create_returns_the_exact_new_pattern_reveal() {
        let mut session = session();
        let expected_revision = session.project_snapshot().unwrap().revisions().aggregate;
        let intent = PatternActionIntent {
            expected_project_revision: expected_revision,
            action: PatternAction::Create(CreatePatternIntent {
                mode: PatternEditorMode::Steps,
                name: "New beat".into(),
                length: BeatDuration((PPQ * 4) as u64),
                step_resolution: BeatDuration((PPQ / 4) as u64),
                initial_target: None,
            }),
        };
        let PatternRevealExecution::ProjectChanged(receipt) =
            execute_pattern_action_revealed(&mut session, &intent).unwrap()
        else {
            panic!("pattern create must publish")
        };
        let reveal = receipt.reveal.expect("created pattern is revealable");
        let ObjectRef::Pattern(pattern) = reveal.request.object else {
            panic!("pattern create must reveal its exact pattern")
        };
        assert!(session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .sequencer
            .patterns()
            .get(pattern)
            .is_some());
        assert_eq!(reveal.request.intent, RevealIntent::ActivateExisting);
    }

    #[test]
    fn revision_only_arrangement_create_has_a_rich_adapter() {
        let mut session = session();
        let expected_revision = session.project_snapshot().unwrap().revisions().aggregate;
        let receipt = execute_arrangement_event_revealed(
            &mut session,
            ArrangementViewEvent::Action(ArrangementActionIntent {
                expected_revision,
                action: ArrangementAction::CreateTrack {
                    kind: TrackKind::Audio,
                },
            }),
        )
        .unwrap();
        assert!(matches!(
            receipt.execution,
            ArrangementExecution::ProjectChanged(_)
        ));
        let reveal = receipt.reveal.expect("created track is revealable");
        let ObjectRef::Track(track) = reveal.request.object else {
            panic!("track creation must reveal its exact track")
        };
        let state = session.project_snapshot().unwrap().project.state();
        assert!(state.domains.arrangement.track(track).is_some());
        assert!(reveal
            .request
            .related
            .iter()
            .any(|object| matches!(object, ObjectRef::Bus(_))));
    }

    #[test]
    fn typed_id_and_migration_receipts_have_lossless_reveals() {
        let asset = AssetId(17);
        let material = recommend_asset(asset);
        assert_eq!(material.request.object, ObjectRef::Material(asset));

        let report = LegacyMigrationReport {
            tracks: BTreeMap::from([(
                crate::session::TrackId::from_raw(1),
                crate::arrangement::TrackId::from_raw(8),
            )]),
            clips: BTreeMap::from([(
                crate::session::ClipId::from_raw(2),
                crate::arrangement::ClipId::from_raw(9),
            )]),
            archived_events: 2,
            archived_clusters: 1,
        };
        let migration = recommend_legacy_migration(&report).unwrap();
        assert_eq!(
            migration.request.object,
            ObjectRef::AudioClip(crate::arrangement::ClipId::from_raw(9))
        );
        assert!(migration
            .request
            .related
            .contains(&ObjectRef::Track(crate::arrangement::TrackId::from_raw(8))));
    }
}
