//! Completion adapters for durable creation and promotion receipts.
//!
//! The command kernel deliberately returns project revisions, while product
//! completion requires a concrete object to reveal. This module preserves
//! both: adapters inspect exact put-style commands or typed receipts after a
//! successful commit and return [`RevealRecommendation`] without letting UI
//! concerns leak into domain commands. It does not infer object identities
//! from labels, command text, or equal raw integers across domains.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::arrangement::{ArrangementOperation, ClipContent, ClipId, TrackId};
use crate::arrangement_view::ArrangementViewEvent;
use crate::artifact_catalog::ArtifactId;
use crate::assets::{AssetError, AssetId, AssetRegistration, AssetRegistry};
use crate::command::{BindingCommand, CommandEnvelope, DomainCommand};
use crate::comparison::ComparisonId;
use crate::comparison_runtime::ComparisonExecution;
use crate::constructive::{ConstructiveApplicationReceipt, ConstructiveFocus};
use crate::control_views::control_actions::{
    ControlAction, ControlSessionAdapter, CreatedControlIdentity,
};
use crate::daw_project::{ProjectRevisions, ProjectState};
use crate::daw_render::PcmAsset;
use crate::interpretation::{InterpretationCommand, InterpretationError, InterpretationStore};
use crate::live_project::AssetImportDisposition;
use crate::pattern_actions::{PatternAction, PatternActionIntent, PatternEditorTarget};
use crate::pattern_controller::{
    lower_pattern_action, LoweredPatternAction, PatternActionSnapshot, PatternLoweringError,
};
use crate::project_session::{ProjectEditReceipt, ProjectSession, ProjectSessionError};
use crate::reading::ReadingFile;
use crate::sample_material::{CanonicalPcmIdentity, SourceMaterialRef};
use crate::sequencer::{PatternId, SequencerCommand};

use super::object_navigation::{
    AutomationOccurrenceRef, FindingKind, FindingLocalId, FindingRef, FindingScope, InstrumentRef,
    ObjectKind, ObjectRef, PadRef, PatternOccurrenceRef, RevealIntent, RevealRecommendation,
    RevealRequest, SelectionConsequence,
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
    /// A repeated import reuses an existing identity only after the fingerprint
    /// hint and canonical decoded PCM compare bit-for-bit.
    AssetRepeatedImport,
    ExplanationCreate,
    ComparisonCreate,
    ComparisonResolve,
    CoveragePublish,
    ReadingImport,
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

const DURABLE_REVEAL_RULES: [DurableRevealRule; 23] = [
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
    revision(
        DurableFlow::ArrangementAutomationClipDuplicate,
        ObjectKind::AutomationOccurrence,
    ),
    revision(
        DurableFlow::ArrangementAudioClipSplit,
        ObjectKind::AudioClip,
    ),
    revision(
        DurableFlow::ArrangementPatternClipSplit,
        ObjectKind::PatternOccurrence,
    ),
    revision(
        DurableFlow::ArrangementAutomationClipSplit,
        ObjectKind::AutomationOccurrence,
    ),
    revision(DurableFlow::ArrangementMediaInsert, ObjectKind::AudioClip),
    revision(
        DurableFlow::ArrangementPatternInsert,
        ObjectKind::PatternOccurrence,
    ),
    native(
        DurableFlow::AssetImport,
        ObjectKind::Material,
        "import_asset_revealed",
    ),
    native(
        DurableFlow::AssetRepeatedImport,
        ObjectKind::Material,
        "import_asset_revealed",
    ),
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

#[derive(Clone, Debug)]
pub struct ProjectMutationReceipt {
    pub edit: ProjectEditReceipt,
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
    let edit = session.execute_envelope(envelope)?;
    let mut reveal =
        recommend_command_result(&receipt_envelope, edit.publication.snapshot.project.state());
    if let Some(reveal) = reveal.as_mut() {
        reveal.request.expected_project_revision = Some(edit.publication.revisions.aggregate);
    }
    Ok(ProjectMutationReceipt { edit, reveal })
}

#[derive(Clone, Debug)]
pub struct ControlRevealReceipt {
    pub revisions: Option<ProjectRevisions>,
    pub primary: Option<ObjectRef>,
    pub reveal: Option<RevealRecommendation>,
}

/// Execute a mixer/automation intent and name the object a create action allocated.
///
/// [`ProjectSession::execute_control_action_for_editor`] returns revisions only.
/// Mixer commands are aggregate-granular and do not name a new bus, so this
/// adapter captures [`ControlSessionAdapter::created_identity`] before execute
/// and confirms the object exists on the published snapshot.
pub fn execute_control_action_revealed(
    session: &mut ProjectSession,
    editor_session: u64,
    action: ControlAction,
) -> Result<ControlRevealReceipt, ProjectSessionError> {
    let (pre_aggregate, primary) = {
        let snapshot = session.project_snapshot()?;
        let domains = &snapshot.project.state().domains;
        let adapter = ControlSessionAdapter::new(
            snapshot.revisions().aggregate,
            editor_session,
            &domains.mixer,
            &domains.automation,
        );
        let identity = adapter
            .created_identity(&action)
            .map_err(|error| ProjectSessionError::Action(error.to_string()))?;
        let primary = identity.map(|identity| match identity {
            CreatedControlIdentity::MixerBus(id) => ObjectRef::Bus(id),
            CreatedControlIdentity::AutomationLane(id) => ObjectRef::Automation(id),
        });
        (snapshot.revisions().aggregate, primary)
    };
    let revisions = session.execute_control_action_for_editor(editor_session, action)?;
    let Some(primary) = primary else {
        return Ok(ControlRevealReceipt {
            revisions,
            primary: None,
            reveal: None,
        });
    };
    let exists = {
        let snapshot = session.project_snapshot()?;
        let domains = &snapshot.project.state().domains;
        match &primary {
            ObjectRef::Bus(id) => domains.mixer.bus(*id).is_some(),
            ObjectRef::Automation(id) => domains.automation.lane(*id).is_some(),
            _ => false,
        }
    };
    if !exists {
        return Err(ProjectSessionError::Action(format!(
            "control create allocated {primary:?} but the published snapshot does not contain it"
        )));
    }
    let revision = revisions
        .map(|revisions| revisions.aggregate)
        .unwrap_or(pre_aggregate);
    Ok(ControlRevealReceipt {
        revisions,
        primary: Some(primary.clone()),
        reveal: Some(RevealRecommendation {
            request: RevealRequest::new(primary, RevealIntent::ActivateExisting)
                .at_revision(revision),
            diagnostics: Vec::new(),
        }),
    })
}

#[derive(Clone, Debug)]
pub struct ArrangementRevealReceipt {
    pub execution: ArrangementExecution,
    pub edit: Option<ProjectEditReceipt>,
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
                execution: ArrangementExecution::ProjectChanged(result.edit.publication.revisions),
                edit: Some(result.edit),
                reveal: result.reveal,
            })
        }
        ArrangementDispatch::History(history) => {
            let edit = match history.kind {
                ArrangementHistoryKind::Undo => session.undo_with_receipt(),
                ArrangementHistoryKind::Redo => session.redo_with_receipt(),
            }
            .map_err(ArrangementExecutionError::Session)?;
            Ok(ArrangementRevealReceipt {
                execution: edit.as_ref().map_or(
                    ArrangementExecution::HistoryUnchanged(history.kind),
                    |edit| ArrangementExecution::ProjectChanged(edit.publication.revisions),
                ),
                edit,
                reveal: None,
            })
        }
        ArrangementDispatch::SelectionOnly => Ok(ArrangementRevealReceipt {
            execution: ArrangementExecution::SelectionOnly,
            edit: None,
            reveal: None,
        }),
        ArrangementDispatch::Seek(frame) => Ok(ArrangementRevealReceipt {
            execution: ArrangementExecution::Seek(frame),
            edit: None,
            reveal: None,
        }),
    }
}

/// Selection a host should apply after [`execute_arrangement_event_revealed`].
///
/// Revision-only `execute_arrangement_event` cannot name the created object.
/// This returns the created [`ObjectRef`] as [`SelectionConsequence::primary`],
/// never a predecessor. `None` means the event did not create a durable
/// object (selection-only, seek, history, or an ordinary non-creating edit)
/// and the host should keep any gesture selection unchanged.
pub fn apply_arrangement_reveal_selection(
    receipt: &ArrangementRevealReceipt,
) -> Option<SelectionConsequence> {
    receipt.reveal.as_ref().map(reveal_selection_consequence)
}

#[derive(Clone, Debug)]
pub enum PatternRevealExecution {
    ProjectChanged(ProjectMutationReceipt),
    HistoryChanged(Option<ProjectEditReceipt>),
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
            .undo_with_receipt()
            .map(PatternRevealExecution::HistoryChanged)
            .map_err(PatternRevealExecutionError::Session),
        LoweredPatternAction::Redo => session
            .redo_with_receipt()
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

/// Selection a host should apply after [`execute_pattern_action_revealed`].
///
/// Create and duplicate name the new [`ObjectRef::Pattern`]. History, retarget,
/// and preview-cycle outcomes have no created object (`None`).
pub fn apply_pattern_reveal_selection(
    execution: &PatternRevealExecution,
) -> Option<SelectionConsequence> {
    match execution {
        PatternRevealExecution::ProjectChanged(receipt) => {
            receipt.reveal.as_ref().map(reveal_selection_consequence)
        }
        PatternRevealExecution::HistoryChanged(_)
        | PatternRevealExecution::Retarget(_)
        | PatternRevealExecution::PreviewCycle { .. } => None,
    }
}

/// Exact created-object selection carried by a typed completion receipt.
pub fn reveal_selection_consequence(reveal: &RevealRecommendation) -> SelectionConsequence {
    SelectionConsequence {
        primary: reveal.request.object.clone(),
        related: reveal.request.related.clone(),
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
    let mut predecessor_clips = BTreeSet::new();
    let mut predecessor_tracks = BTreeSet::new();
    let mut predecessor_patterns = BTreeSet::new();
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
            DomainCommand::Arrangement(ArrangementOperation::PutClip {
                before: Some(clip),
                ..
            }) => {
                predecessor_clips.insert(clip.id);
            }
            DomainCommand::Arrangement(ArrangementOperation::PutTrack {
                before: Some(track),
                ..
            }) => {
                predecessor_tracks.insert(track.id);
            }
            DomainCommand::Sequencer(SequencerCommand::PutPattern {
                before: Some(pattern),
                ..
            }) => {
                predecessor_patterns.insert(pattern.id);
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
                        ranked.push((
                            2,
                            ObjectRef::AutomationOccurrence(AutomationOccurrenceRef {
                                arrangement_clip: clip.id,
                                lane,
                            }),
                        ));
                        ranked.push((5, ObjectRef::Automation(lane)));
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
    // Creations are `before: None`. Identities rewritten in the same envelope
    // (`before: Some`) stay related at most; they must not win primary.
    let (_, primary) = ranked
        .iter()
        .find(|(_, object)| {
            !object_is_rewritten_predecessor(
                object,
                &predecessor_clips,
                &predecessor_tracks,
                &predecessor_patterns,
            )
        })
        .or_else(|| ranked.first())
        .cloned()?;
    let related = ranked.into_iter().map(|(_, object)| object);
    Some(RevealRecommendation {
        request: RevealRequest::new(primary, RevealIntent::ActivateExisting).with_related(related),
        diagnostics: Vec::new(),
    })
}

fn object_is_rewritten_predecessor(
    object: &ObjectRef,
    clips: &BTreeSet<ClipId>,
    tracks: &BTreeSet<TrackId>,
    patterns: &BTreeSet<PatternId>,
) -> bool {
    match object {
        ObjectRef::AudioClip(clip) => clips.contains(clip),
        ObjectRef::PatternOccurrence(occurrence) => clips.contains(&occurrence.arrangement_clip),
        ObjectRef::AutomationOccurrence(occurrence) => clips.contains(&occurrence.arrangement_clip),
        ObjectRef::Track(track) => tracks.contains(track),
        ObjectRef::Pattern(pattern) => patterns.contains(pattern),
        _ => false,
    }
}

pub fn recommend_asset(asset: AssetId) -> RevealRecommendation {
    RevealRecommendation {
        request: RevealRequest::new(ObjectRef::Material(asset), RevealIntent::ActivateExisting),
        diagnostics: Vec::new(),
    }
}

/// Canonical installed-project publication. Unlike detached registry
/// registration, creation includes the aggregate edit receipt and decoded PCM
/// is already visible in that receipt's snapshot. Exact-content reuse is an
/// explicit non-edit publication.
#[derive(Clone, Debug)]
pub struct AssetPublication {
    pub asset: AssetId,
    pub disposition: AssetImportDisposition,
    pub decoded_pcm: CanonicalPcmIdentity,
    pub duplicate_predecessors: Vec<AssetId>,
    pub edit: Option<ProjectEditReceipt>,
    pub reveal: RevealRecommendation,
}

pub fn import_asset_revealed(
    session: &mut ProjectSession,
    expected_revision: u64,
    registration: AssetRegistration,
    pcm: PcmAsset,
) -> Result<AssetPublication, ProjectSessionError> {
    let imported = session.import_asset(expected_revision, registration, pcm)?;
    let mut reveal = recommend_asset(imported.asset);
    reveal.request.expected_project_revision =
        Some(imported.edit.as_ref().map_or(expected_revision, |edit| {
            edit.publication.revisions.aggregate
        }));
    reveal.request.related.extend(
        imported
            .duplicate_predecessors
            .iter()
            .copied()
            .map(ObjectRef::Material),
    );
    Ok(AssetPublication {
        asset: imported.asset,
        disposition: imported.disposition,
        decoded_pcm: imported.decoded_pcm,
        duplicate_predecessors: imported.duplicate_predecessors,
        edit: imported.edit,
        reveal,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct AssetRegistrationPublication {
    pub asset: AssetId,
    /// Same-fingerprint predecessors are duplicate hints, never automatic
    /// identity merges.
    pub duplicate_predecessors: Vec<AssetId>,
    pub reveal: RevealRecommendation,
}

/// Typed publication for bootstrap or detached registry registration. An
/// installed project must still import through its controller so metadata and
/// decoded PCM share undo/redo authority.
pub fn register_asset_revealed(
    registry: &mut AssetRegistry,
    registration: AssetRegistration,
) -> Result<AssetRegistrationPublication, AssetError> {
    let duplicate_predecessors: Vec<AssetId> = registry
        .assets()
        .values()
        .filter(|asset| asset.content() == registration.content)
        .map(|asset| asset.id())
        .collect();
    let asset = registry.register(registration)?;
    let mut reveal = recommend_asset(asset);
    reveal.request.related.extend(
        duplicate_predecessors
            .iter()
            .copied()
            .map(ObjectRef::Material),
    );
    Ok(AssetRegistrationPublication {
        asset,
        duplicate_predecessors,
        reveal,
    })
}

/// Preserve every identity retained by the domain-level constructive receipt.
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
        .chain(receipt.bindings.created_pads.iter().copied().map(|pad| {
            ObjectRef::Pad(PadRef {
                kit: receipt.bindings.kit,
                pad,
                zone: None,
            })
        }))
        .chain(receipt.bindings.created_zones.iter().map(|target| {
            ObjectRef::Pad(PadRef {
                kit: target.kit,
                pad: target.pad,
                zone: Some(target.zone),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use crate::arrangement::{ClipId, Frame, FrameRange, TrackKind};
    use crate::arrangement_interaction::{
        ArrangementEdit, ArrangementEditIntent, ClipMove, GestureCommit, SelectionIntent,
        SelectionMode,
    };
    use crate::arrangement_view::{ArrangementAction, ArrangementActionIntent};
    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata, SampleFrames,
    };
    use crate::audio::AudioFormat;
    use crate::automation::{BindingMode, MixerTarget, ParameterAddress, TimeDomain};
    use crate::control_views::control_actions::{
        AutomationAction, AutomationActionIntent, ControlAction, MixerAction, MixerActionIntent,
    };
    use crate::daw_engine::AssetPcmMap;
    use crate::daw_project::DawProject;
    use crate::daw_render::PcmAsset;
    use crate::live_project::{LiveProject, SourceMaterialMetadata};
    use crate::mixer::BusKind;
    use crate::pattern_actions::{CreatePatternIntent, PatternAction, PatternEditorMode};
    use crate::project_session::ProjectSessionId;
    use crate::sequencer::{BeatDuration, PatternId, PPQ};
    use crate::ui_drag::DropIntent;

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
        assert_eq!(
            reveal.request.expected_project_revision,
            Some(receipt.edit.publication.revisions.aggregate)
        );
        assert!(receipt.edit.publication.change_set.is_some());
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
        let edit = receipt.edit.as_ref().expect("arrangement edit receipt");
        assert!(edit.publication.change_set.is_some());
        let reveal = receipt.reveal.expect("created track is revealable");
        assert_eq!(
            reveal.request.expected_project_revision,
            Some(edit.publication.revisions.aggregate)
        );
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
    fn typed_id_receipts_have_lossless_reveals() {
        let asset = AssetId(17);
        let material = recommend_asset(asset);
        assert_eq!(material.request.object, ObjectRef::Material(asset));
    }

    fn audio_session() -> (ProjectSession, ClipId) {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/fixture/receipt-navigation-source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "receipt source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 48_000,
                    channels: 1,
                    frame_count: SampleFrames(16),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"receipt-navigation-audio-fixture"),
                provenance: AssetProvenance::new(
                    1,
                    AssetOrigin::ImportedFile {
                        importer: "receipt-navigation-test".into(),
                    },
                    location,
                ),
                tags: BTreeSet::new(),
                favorite: false,
            })
            .unwrap();
        let pcm = PcmAsset::new(
            AudioFormat::new(48_000, 1).unwrap(),
            Arc::from((0..16).map(|frame| frame as f32 / 16.0).collect::<Vec<_>>()),
        )
        .unwrap();
        let live = LiveProject::from_source_material(
            SourceMaterialMetadata::new("Receipt navigation", "Source"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        let clip = live.source_ids().clip;
        let mut session = ProjectSession::new(ProjectSessionId(602)).unwrap();
        session.install(live, None).unwrap();
        (session, clip)
    }

    fn create_pattern(session: &mut ProjectSession, name: &str) -> PatternId {
        let expected_revision = session.project_snapshot().unwrap().revisions().aggregate;
        let intent = PatternActionIntent {
            expected_project_revision: expected_revision,
            action: PatternAction::Create(CreatePatternIntent {
                mode: PatternEditorMode::Steps,
                name: name.into(),
                length: BeatDuration((PPQ * 4) as u64),
                step_resolution: BeatDuration((PPQ / 4) as u64),
                initial_target: None,
            }),
        };
        let execution = execute_pattern_action_revealed(session, &intent).unwrap();
        let selection = apply_pattern_reveal_selection(&execution).expect("created pattern");
        let ObjectRef::Pattern(pattern) = selection.primary else {
            panic!("pattern create must name ObjectRef::Pattern")
        };
        pattern
    }

    fn duplicate_clip(session: &mut ProjectSession, clip: ClipId) -> ArrangementRevealReceipt {
        let (expected_revision, before) = {
            let snapshot = session.project_snapshot().unwrap();
            (
                snapshot.revisions().aggregate,
                snapshot
                    .project
                    .state()
                    .domains
                    .arrangement
                    .clip(clip)
                    .unwrap()
                    .clone(),
            )
        };
        execute_arrangement_event_revealed(
            session,
            ArrangementViewEvent::Commit(GestureCommit {
                selection: None,
                edit: Some(ArrangementEditIntent {
                    expected_revision,
                    edit: ArrangementEdit::MoveClips {
                        moves: vec![ClipMove {
                            clip_id: clip,
                            from_track: before.track_id,
                            to_track: before.track_id,
                            from: before.placement,
                            to: before.placement,
                        }],
                        duplicate: true,
                    },
                }),
            }),
        )
        .unwrap()
    }

    fn split_clip(session: &mut ProjectSession, clip: ClipId) -> ArrangementRevealReceipt {
        let (expected_revision, at) = {
            let snapshot = session.project_snapshot().unwrap();
            let before = snapshot
                .project
                .state()
                .domains
                .arrangement
                .clip(clip)
                .unwrap();
            (
                snapshot.revisions().aggregate,
                Frame(before.placement.start.0 + (before.placement.len() as i64) / 2),
            )
        };
        execute_arrangement_event_revealed(
            session,
            ArrangementViewEvent::Action(ArrangementActionIntent {
                expected_revision,
                action: ArrangementAction::SplitClip { clip, at },
            }),
        )
        .unwrap()
    }

    fn assert_created_clip_is_new(receipt: &ArrangementRevealReceipt, source: ClipId) -> ObjectRef {
        let selection =
            apply_arrangement_reveal_selection(receipt).expect("creating edit names an object");
        match &selection.primary {
            ObjectRef::AudioClip(clip) => {
                assert_ne!(*clip, source, "reveal must not be the source clip")
            }
            ObjectRef::PatternOccurrence(occurrence) => assert_ne!(
                occurrence.arrangement_clip, source,
                "reveal must not be the source occurrence"
            ),
            other => panic!("clip create/duplicate/split must name the new clip, got {other:?}"),
        }
        let state = receipt
            .edit
            .as_ref()
            .expect("creating edit has a publication")
            .publication
            .snapshot
            .project
            .state();
        match &selection.primary {
            ObjectRef::AudioClip(clip) => {
                assert!(state.domains.arrangement.clip(*clip).is_some());
                assert!(state.domains.arrangement.clip(source).is_some());
            }
            ObjectRef::PatternOccurrence(occurrence) => {
                assert!(state
                    .domains
                    .arrangement
                    .clip(occurrence.arrangement_clip)
                    .is_some());
                assert!(state.domains.arrangement.clip(source).is_some());
            }
            _ => unreachable!(),
        }
        selection.primary
    }

    #[test]
    fn audio_duplicate_and_split_reveal_the_new_clip_not_the_source() {
        let (mut session, source) = audio_session();
        let duplicated = duplicate_clip(&mut session, source);
        let ObjectRef::AudioClip(duplicate) = assert_created_clip_is_new(&duplicated, source)
        else {
            panic!("audio duplicate must recommend ObjectRef::AudioClip")
        };
        session
            .issue_reveal(duplicated.reveal.as_ref().unwrap().request.clone())
            .unwrap();

        let split = split_clip(&mut session, source);
        let ObjectRef::AudioClip(right) = assert_created_clip_is_new(&split, source) else {
            panic!("audio split must recommend ObjectRef::AudioClip")
        };
        assert_ne!(right, duplicate);
        assert_ne!(right, source);
    }

    #[test]
    fn pattern_occurrence_duplicate_and_split_reveal_the_new_occurrence() {
        let mut session = session();
        let pattern = create_pattern(&mut session, "Phrase");
        let expected_revision = session.project_snapshot().unwrap().revisions().aggregate;
        let inserted = execute_arrangement_event_revealed(
            &mut session,
            ArrangementViewEvent::Action(ArrangementActionIntent {
                expected_revision,
                action: ArrangementAction::Drop(DropIntent::InsertPattern {
                    pattern,
                    track: None,
                    at: Frame(0),
                    make_unique: false,
                }),
            }),
        )
        .unwrap();
        let ObjectRef::PatternOccurrence(source) = apply_arrangement_reveal_selection(&inserted)
            .expect("pattern insert names the occurrence")
            .primary
        else {
            panic!("pattern insert must recommend PatternOccurrence")
        };

        let duplicated = duplicate_clip(&mut session, source.arrangement_clip);
        let ObjectRef::PatternOccurrence(duplicate) =
            assert_created_clip_is_new(&duplicated, source.arrangement_clip)
        else {
            panic!("pattern duplicate must recommend PatternOccurrence")
        };
        assert_ne!(duplicate.arrangement_clip, source.arrangement_clip);

        let split = split_clip(&mut session, source.arrangement_clip);
        let ObjectRef::PatternOccurrence(right) =
            assert_created_clip_is_new(&split, source.arrangement_clip)
        else {
            panic!("pattern split must recommend PatternOccurrence")
        };
        assert_ne!(right.arrangement_clip, source.arrangement_clip);
        assert_ne!(right.arrangement_clip, duplicate.arrangement_clip);
    }

    #[test]
    fn selection_only_and_non_creating_arrangement_events_have_no_reveal() {
        let (mut session, source) = audio_session();
        let selection_only = execute_arrangement_event_revealed(
            &mut session,
            ArrangementViewEvent::Commit(GestureCommit {
                selection: Some(SelectionIntent::Clips {
                    ids: BTreeSet::from([source]),
                    primary: Some(source),
                    mode: SelectionMode::Replace,
                }),
                edit: None,
            }),
        )
        .unwrap();
        assert!(matches!(
            selection_only.execution,
            ArrangementExecution::SelectionOnly
        ));
        assert!(selection_only.reveal.is_none());
        assert!(apply_arrangement_reveal_selection(&selection_only).is_none());

        let seek = execute_arrangement_event_revealed(
            &mut session,
            ArrangementViewEvent::SeekRequested(Frame(3)),
        )
        .unwrap();
        assert!(seek.reveal.is_none());
        assert!(apply_arrangement_reveal_selection(&seek).is_none());

        let (expected_revision, before) = {
            let snapshot = session.project_snapshot().unwrap();
            (
                snapshot.revisions().aggregate,
                snapshot
                    .project
                    .state()
                    .domains
                    .arrangement
                    .clip(source)
                    .unwrap()
                    .clone(),
            )
        };
        let moved_to =
            FrameRange::from_start_and_len(before.placement.end, before.placement.len()).unwrap();
        let moved = execute_arrangement_event_revealed(
            &mut session,
            ArrangementViewEvent::Commit(GestureCommit {
                selection: None,
                edit: Some(ArrangementEditIntent {
                    expected_revision,
                    edit: ArrangementEdit::MoveClips {
                        moves: vec![ClipMove {
                            clip_id: source,
                            from_track: before.track_id,
                            to_track: before.track_id,
                            from: before.placement,
                            to: moved_to,
                        }],
                        duplicate: false,
                    },
                }),
            }),
        )
        .unwrap();
        assert!(matches!(
            moved.execution,
            ArrangementExecution::ProjectChanged(_)
        ));
        assert!(moved.reveal.is_none());
        assert!(apply_arrangement_reveal_selection(&moved).is_none());
    }

    #[test]
    fn pattern_duplicate_reveals_the_new_pattern_not_the_source() {
        let mut session = session();
        let source = create_pattern(&mut session, "Lead");
        let expected_revision = session.project_snapshot().unwrap().revisions().aggregate;
        let source_revision = session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .sequencer
            .patterns()
            .get(source)
            .unwrap()
            .revision;
        let execution = execute_pattern_action_revealed(
            &mut session,
            &PatternActionIntent {
                expected_project_revision: expected_revision,
                action: PatternAction::Duplicate {
                    source,
                    expected_pattern_revision: source_revision,
                    name: "Lead copy".into(),
                },
            },
        )
        .unwrap();
        let selection = apply_pattern_reveal_selection(&execution).expect("duplicated pattern");
        let ObjectRef::Pattern(duplicate) = selection.primary else {
            panic!("pattern duplicate must recommend ObjectRef::Pattern")
        };
        assert_ne!(duplicate, source);
        assert!(selection.related.contains(&ObjectRef::Pattern(source)));
        let patterns = session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .sequencer
            .patterns();
        assert!(patterns.get(source).is_some());
        assert!(patterns.get(duplicate).is_some());
        let PatternRevealExecution::ProjectChanged(receipt) = &execution else {
            panic!("pattern duplicate must publish")
        };
        session
            .issue_reveal(receipt.reveal.as_ref().unwrap().request.clone())
            .unwrap();
    }

    const CONTROL_EDITOR: u64 = 701;

    fn mixer_control(session: &ProjectSession, action: MixerAction) -> ControlAction {
        let revision = session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .revision();
        ControlAction::Mixer(MixerActionIntent::new(revision, action))
    }

    fn mixer_bus_ids(session: &ProjectSession) -> BTreeSet<crate::mixer::BusId> {
        session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .bus_order()
            .iter()
            .copied()
            .collect()
    }

    fn assert_created_bus(
        session: &ProjectSession,
        receipt: &ControlRevealReceipt,
        kind: BusKind,
    ) -> crate::mixer::BusId {
        let ObjectRef::Bus(id) = receipt.primary.clone().expect("create names a bus") else {
            panic!("mixer create must recommend ObjectRef::Bus")
        };
        let reveal = receipt.reveal.as_ref().expect("created bus is revealable");
        assert_eq!(reveal.request.object, ObjectRef::Bus(id));
        assert_eq!(reveal.request.intent, RevealIntent::ActivateExisting);
        assert_eq!(
            reveal.request.expected_project_revision,
            receipt.revisions.map(|revisions| revisions.aggregate)
        );
        assert!(receipt.revisions.is_some());
        let mixer = &session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer;
        assert_eq!(mixer.bus(id).unwrap().kind(), kind);
        id
    }

    #[test]
    fn add_return_reveal_names_the_created_bus() {
        let mut session = session();
        let action = mixer_control(
            &session,
            MixerAction::AddReturn {
                name: "Room".into(),
            },
        );
        let receipt =
            execute_control_action_revealed(&mut session, CONTROL_EDITOR, action).unwrap();
        let id = assert_created_bus(&session, &receipt, BusKind::Return);
        session.undo().unwrap();
        assert!(session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .bus(id)
            .is_none());
    }

    #[test]
    fn add_group_reveal_names_the_created_bus() {
        let mut session = session();
        let action = mixer_control(
            &session,
            MixerAction::AddBus {
                kind: BusKind::Group,
                name: "Music".into(),
            },
        );
        let receipt =
            execute_control_action_revealed(&mut session, CONTROL_EDITOR, action).unwrap();
        let id = assert_created_bus(&session, &receipt, BusKind::Group);
        session.undo().unwrap();
        assert!(session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .bus(id)
            .is_none());
    }

    #[test]
    fn gain_change_has_no_created_identity() {
        let (mut session, _) = audio_session();
        let bus = session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .buses()
            .find(|bus| bus.kind() == BusKind::Source)
            .expect("source material installs a source bus")
            .id();
        let buses_before = mixer_bus_ids(&session);
        let action = mixer_control(&session, MixerAction::SetGainDb { bus, gain_db: -6.0 });
        let receipt =
            execute_control_action_revealed(&mut session, CONTROL_EDITOR, action).unwrap();
        assert!(receipt.primary.is_none());
        assert!(receipt.reveal.is_none());
        assert!(receipt.revisions.is_some());
        assert_eq!(mixer_bus_ids(&session), buses_before);
        assert_eq!(
            session
                .project_snapshot()
                .unwrap()
                .project
                .state()
                .domains
                .mixer
                .bus(bus)
                .unwrap()
                .fader()
                .gain_db(),
            -6.0
        );
    }

    #[test]
    fn create_lane_reveal_names_the_created_lane() {
        let mut session = session();
        let action = {
            let snapshot = session.project_snapshot().unwrap();
            let domains = &snapshot.project.state().domains;
            let bus = domains.mixer.master();
            ControlAction::Automation(AutomationActionIntent::new(
                domains.automation.revision(),
                AutomationAction::CreateLane {
                    name: "Master gain".into(),
                    target: ParameterAddress::Mixer(MixerTarget::BusGain(bus.get())),
                    domain: TimeDomain::Frames,
                    binding: BindingMode::Replace,
                },
            ))
        };
        let receipt =
            execute_control_action_revealed(&mut session, CONTROL_EDITOR, action).unwrap();
        let ObjectRef::Automation(id) = receipt.primary.clone().expect("CreateLane names a lane")
        else {
            panic!("lane create must recommend ObjectRef::Automation")
        };
        let reveal = receipt.reveal.as_ref().expect("created lane is revealable");
        assert_eq!(reveal.request.object, ObjectRef::Automation(id));
        assert_eq!(reveal.request.intent, RevealIntent::ActivateExisting);
        assert_eq!(
            reveal.request.expected_project_revision,
            receipt.revisions.map(|revisions| revisions.aggregate)
        );
        assert!(session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .automation
            .lane(id)
            .is_some());
        session.undo().unwrap();
        assert!(session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .automation
            .lane(id)
            .is_none());
    }
}
