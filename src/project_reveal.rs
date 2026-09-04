//! Revision-guarded reveal receipts for asynchronous navigation.
//!
//! Navigation proposes typed objects; the authoritative session proves that
//! those objects still exist immediately before a UI applies a reveal.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use crate::arrangement::ClipContent;
use crate::automation::AutomationLaneId;
use crate::live_project::LiveProjectSnapshot;
use crate::mixer::BusId;
use crate::ontology::SourceId;
use crate::project_controller::{
    recommend_constructive, AutomationOccurrenceRef, InstrumentRef, ObjectRef, PadRef,
    PatternOccurrenceRef, RevealIntent, RevealRequest,
};
use crate::sample_material::SourceMaterialRef;
use crate::sequencer::PatternId;
use crate::workspace_document::{EditorTarget, WorkspaceDocument, WorkspaceViewId};

use super::{ProjectSession, ProjectSessionError, ProjectSessionId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevealGuard {
    pub session: ProjectSessionId,
    pub document_generation: u64,
    pub publication_generation: u64,
    pub project_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevealReceipt {
    pub guard: RevealGuard,
    pub request: RevealRequest,
    /// Ordered durable ancestors or related objects to try if the primary
    /// object disappears through undo or another authoritative edit.
    pub predecessors: Vec<ObjectRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevealFreshness {
    ExactPublication,
    /// The document is unchanged, but publication/revision advanced. The
    /// object was re-resolved and may have been recreated by redo.
    RevalidatedCurrent,
    /// An interpretive identity (finding, explanation, comparison, reading)
    /// that no project revision can prove, guarded by the document generation
    /// alone. Refusing to issue these made every Investigate and Readings row
    /// unrevealable: `issue_reveal` is the only path to a reveal, and it
    /// rejected them for not being in `DawProject`. The guard is weaker and
    /// the receipt says so instead of pretending to a revision proof.
    DocumentScoped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevealFallback {
    ProjectOverview,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevealRejection {
    WrongSession {
        expected: ProjectSessionId,
        actual: ProjectSessionId,
    },
    DocumentReplaced {
        expected_generation: u64,
        actual_generation: u64,
    },
    NoProject,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevealDisposition {
    Current {
        freshness: RevealFreshness,
    },
    Predecessor {
        deleted: ObjectRef,
        target: ObjectRef,
    },
    Fallback {
        deleted: ObjectRef,
        target: RevealFallback,
    },
    Rejected(RevealRejection),
}

/// Only `request: Some` may be passed to `ObjectNavigator`. A fallback or
/// rejection never leaks the deleted primary identity into a workspace plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevealResolution {
    pub disposition: RevealDisposition,
    pub request: Option<RevealRequest>,
    pub guard: Option<RevealGuard>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceRevealTargetIssueReason {
    MissingProjectObject,
    NotProjectDurable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceRevealTargetIssue {
    pub view: WorkspaceViewId,
    pub target: EditorTarget,
    pub object: Option<ObjectRef>,
    pub reason: WorkspaceRevealTargetIssueReason,
}

impl fmt::Display for WorkspaceRevealTargetIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.object, &self.reason) {
            (Some(object), WorkspaceRevealTargetIssueReason::MissingProjectObject) => write!(
                formatter,
                "workspace view {} targets missing {}",
                self.view.0,
                object.address()
            ),
            (_, WorkspaceRevealTargetIssueReason::NotProjectDurable) => write!(
                formatter,
                "workspace view {} has a target this project snapshot cannot durably resolve",
                self.view.0
            ),
            (None, WorkspaceRevealTargetIssueReason::MissingProjectObject) => write!(
                formatter,
                "workspace view {} has an invalid project target",
                self.view.0
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceTargetResolution {
    ProjectSurface,
    Object(ObjectRef),
    Missing(ObjectRef),
    NotProjectDurable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectRevealError {
    Session(ProjectSessionError),
    MissingObject(ObjectRef),
}

impl fmt::Display for ProjectRevealError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => error.fmt(formatter),
            Self::MissingObject(object) => {
                write!(
                    formatter,
                    "cannot issue reveal for missing {}",
                    object.address()
                )
            }
        }
    }
}

impl Error for ProjectRevealError {}

impl From<ProjectSessionError> for ProjectRevealError {
    fn from(error: ProjectSessionError) -> Self {
        Self::Session(error)
    }
}

impl ProjectSession {
    pub fn issue_reveal(
        &self,
        request: RevealRequest,
    ) -> Result<RevealReceipt, ProjectRevealError> {
        let snapshot = self.project_snapshot()?;
        match object_resolution(snapshot, &request.object) {
            ObjectResolution::Present | ObjectResolution::NotProjectDurable => {}
            ObjectResolution::Missing => {
                return Err(ProjectRevealError::MissingObject(request.object));
            }
        }
        Ok(RevealReceipt {
            guard: self.current_reveal_guard(snapshot),
            predecessors: predecessor_chain(&request),
            request,
        })
    }

    pub fn issue_constructive_reveal(
        &self,
        publication: &crate::project_controller::ConstructivePublication,
    ) -> Result<RevealReceipt, ProjectRevealError> {
        self.issue_reveal(recommend_constructive(publication).request)
    }

    pub fn resolve_reveal(&self, receipt: &RevealReceipt) -> RevealResolution {
        if receipt.guard.session != self.id() {
            return rejected(RevealRejection::WrongSession {
                expected: receipt.guard.session,
                actual: self.id(),
            });
        }
        if receipt.guard.document_generation != self.document_generation() {
            return rejected(RevealRejection::DocumentReplaced {
                expected_generation: receipt.guard.document_generation,
                actual_generation: self.document_generation(),
            });
        }
        let Ok(snapshot) = self.project_snapshot() else {
            return rejected(RevealRejection::NoProject);
        };
        let guard = self.current_reveal_guard(snapshot);
        let freshness = match object_resolution(snapshot, &receipt.request.object) {
            ObjectResolution::Present if guard == receipt.guard => {
                Some(RevealFreshness::ExactPublication)
            }
            ObjectResolution::Present => Some(RevealFreshness::RevalidatedCurrent),
            ObjectResolution::NotProjectDurable => Some(RevealFreshness::DocumentScoped),
            ObjectResolution::Missing => None,
        };
        if let Some(freshness) = freshness {
            return RevealResolution {
                disposition: RevealDisposition::Current { freshness },
                request: Some(sanitize_request(snapshot, receipt.request.clone())),
                guard: Some(guard),
            };
        }
        for predecessor in &receipt.predecessors {
            if object_resolution(snapshot, predecessor) == ObjectResolution::Present {
                let mut request = receipt.request.clone();
                request.object = predecessor.clone();
                request.intent = RevealIntent::ActivateExisting;
                return RevealResolution {
                    disposition: RevealDisposition::Predecessor {
                        deleted: receipt.request.object.clone(),
                        target: predecessor.clone(),
                    },
                    request: Some(sanitize_request(snapshot, request)),
                    guard: Some(guard),
                };
            }
        }
        RevealResolution {
            disposition: RevealDisposition::Fallback {
                deleted: receipt.request.object.clone(),
                target: RevealFallback::ProjectOverview,
            },
            request: None,
            guard: Some(guard),
        }
    }

    /// Final short guard for applying a previously resolved workspace plan.
    pub fn reveal_guard_is_current(&self, guard: RevealGuard) -> bool {
        self.project_snapshot()
            .map(|snapshot| self.current_reveal_guard(snapshot) == guard)
            .unwrap_or(false)
    }

    pub fn resolve_workspace_target(&self, target: &EditorTarget) -> WorkspaceTargetResolution {
        let Ok(snapshot) = self.project_snapshot() else {
            return WorkspaceTargetResolution::NotProjectDurable;
        };
        workspace_target_resolution(snapshot, target)
    }

    pub fn validate_workspace_reveal_targets(
        &self,
        document: &WorkspaceDocument,
    ) -> Vec<WorkspaceRevealTargetIssue> {
        document
            .views
            .values()
            .filter_map(|descriptor| {
                let resolution = self.resolve_workspace_target(&descriptor.target);
                match resolution {
                    WorkspaceTargetResolution::ProjectSurface
                    | WorkspaceTargetResolution::Object(_) => None,
                    WorkspaceTargetResolution::Missing(object) => {
                        Some(WorkspaceRevealTargetIssue {
                            view: descriptor.id,
                            target: descriptor.target.clone(),
                            object: Some(object),
                            reason: WorkspaceRevealTargetIssueReason::MissingProjectObject,
                        })
                    }
                    WorkspaceTargetResolution::NotProjectDurable => {
                        Some(WorkspaceRevealTargetIssue {
                            view: descriptor.id,
                            target: descriptor.target.clone(),
                            object: None,
                            reason: WorkspaceRevealTargetIssueReason::NotProjectDurable,
                        })
                    }
                }
            })
            .collect()
    }

    fn current_reveal_guard(&self, snapshot: &LiveProjectSnapshot) -> RevealGuard {
        RevealGuard {
            session: self.id(),
            document_generation: self.document_generation(),
            publication_generation: self.snapshot().generation,
            project_revision: snapshot.revisions().aggregate,
        }
    }
}

fn rejected(reason: RevealRejection) -> RevealResolution {
    RevealResolution {
        disposition: RevealDisposition::Rejected(reason),
        request: None,
        guard: None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObjectResolution {
    Present,
    Missing,
    NotProjectDurable,
}

fn object_resolution(snapshot: &LiveProjectSnapshot, object: &ObjectRef) -> ObjectResolution {
    let state = snapshot.project.state();
    let present = match object {
        ObjectRef::Material(asset) => state.domains.assets.get(*asset).is_some(),
        ObjectRef::Sample(SourceMaterialRef::Asset(asset)) => {
            state.domains.assets.get(*asset).is_some()
        }
        ObjectRef::Sample(SourceMaterialRef::VirtualSlice(slice)) => state
            .domains
            .assets
            .get(slice.source_asset)
            .is_some_and(|asset| slice.source_range.is_within(asset.metadata().frame_count)),
        ObjectRef::Instrument(InstrumentRef::SampleKit(kit)) => {
            state.domains.sample_kits.kits.contains_key(kit)
        }
        ObjectRef::Pad(PadRef { kit, pad, zone }) => {
            state.domains.sample_kits.kits.get(kit).is_some_and(|kit| {
                kit.pads.contains_key(pad)
                    && zone.is_none_or(|zone| {
                        kit.zones
                            .get(&zone)
                            .is_some_and(|candidate| candidate.pad == *pad)
                    })
            })
        }
        ObjectRef::Pattern(pattern) => state.domains.sequencer.patterns().get(*pattern).is_some(),
        ObjectRef::PatternOccurrence(occurrence) => occurrence_exists(snapshot, *occurrence),
        ObjectRef::AudioClip(clip) => state
            .domains
            .arrangement
            .clip(*clip)
            .is_some_and(|clip| matches!(clip.content, ClipContent::Audio(_))),
        ObjectRef::Track(track) => state.domains.arrangement.track(*track).is_some(),
        ObjectRef::Bus(bus) => state.domains.mixer.bus(*bus).is_some(),
        ObjectRef::Automation(lane) => state.domains.automation.lane(*lane).is_some(),
        ObjectRef::AutomationOccurrence(occurrence) => {
            automation_occurrence_exists(snapshot, *occurrence)
        }
        ObjectRef::Finding(_)
        | ObjectRef::Explanation(_)
        | ObjectRef::Comparison(_)
        | ObjectRef::Reading(_) => return ObjectResolution::NotProjectDurable,
    };
    if present {
        ObjectResolution::Present
    } else {
        ObjectResolution::Missing
    }
}

fn occurrence_exists(snapshot: &LiveProjectSnapshot, occurrence: PatternOccurrenceRef) -> bool {
    let state = snapshot.project.state();
    let Some(clip) = state.domains.arrangement.clip(occurrence.arrangement_clip) else {
        return false;
    };
    let ClipContent::Pattern(region) = &clip.content else {
        return false;
    };
    let bound_pattern = state
        .bindings
        .patterns
        .definitions
        .get(&region.pattern)
        .copied();
    if let Some(pattern) = occurrence.pattern {
        if bound_pattern != Some(pattern)
            || state.domains.sequencer.patterns().get(pattern).is_none()
        {
            return false;
        }
    }
    if let Some(sequencer_clip) = occurrence.sequencer_clip {
        if state
            .bindings
            .patterns
            .placements
            .get(&occurrence.arrangement_clip)
            != Some(&sequencer_clip)
        {
            return false;
        }
        let Some(sequencer_clip) = state.domains.sequencer.clip(sequencer_clip) else {
            return false;
        };
        if bound_pattern != Some(sequencer_clip.pattern)
            || occurrence
                .pattern
                .is_some_and(|pattern| pattern != sequencer_clip.pattern)
        {
            return false;
        }
    }
    true
}

fn automation_occurrence_exists(
    snapshot: &LiveProjectSnapshot,
    occurrence: AutomationOccurrenceRef,
) -> bool {
    let state = snapshot.project.state();
    let Some(clip) = state.domains.arrangement.clip(occurrence.arrangement_clip) else {
        return false;
    };
    let ClipContent::Automation(region) = &clip.content else {
        return false;
    };
    state
        .bindings
        .automation
        .lanes
        .get(&region.parameter)
        .is_some_and(|lane| *lane == occurrence.lane)
        && state.domains.automation.lane(occurrence.lane).is_some()
}

fn predecessor_chain(request: &RevealRequest) -> Vec<ObjectRef> {
    let mut candidates = Vec::new();
    match &request.object {
        ObjectRef::Pad(pad) => {
            candidates.push(ObjectRef::Instrument(InstrumentRef::SampleKit(pad.kit)))
        }
        ObjectRef::PatternOccurrence(occurrence) => {
            if let Some(pattern) = occurrence.pattern {
                candidates.push(ObjectRef::Pattern(pattern));
            }
        }
        ObjectRef::AutomationOccurrence(occurrence) => {
            candidates.push(ObjectRef::Automation(occurrence.lane));
        }
        ObjectRef::Sample(SourceMaterialRef::Asset(asset)) => {
            candidates.push(ObjectRef::Material(*asset));
        }
        ObjectRef::Sample(SourceMaterialRef::VirtualSlice(slice)) => {
            candidates.push(ObjectRef::Material(slice.source_asset));
        }
        _ => {}
    }
    candidates.extend(request.related.iter().cloned());
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|candidate| candidate != &request.object && seen.insert(candidate.clone()))
        .collect()
}

fn sanitize_request(snapshot: &LiveProjectSnapshot, mut request: RevealRequest) -> RevealRequest {
    // The receipt remains pinned to its originating publication, while the
    // request handed to ObjectNavigator is pinned to this successful current
    // resolution. This lets `plan_at_revision` be the final short guard after
    // undo/redo without accepting an unproved stale request.
    request.expected_project_revision = Some(snapshot.revisions().aggregate);
    request
        .related
        .retain(|object| object_resolution(snapshot, object) == ObjectResolution::Present);
    request
}

fn workspace_target_resolution(
    snapshot: &LiveProjectSnapshot,
    target: &EditorTarget,
) -> WorkspaceTargetResolution {
    let object = match target {
        EditorTarget::Project
        | EditorTarget::Arrangement
        | EditorTarget::Assets
        | EditorTarget::Inspector
        | EditorTarget::Mixer { bus_id: None }
        | EditorTarget::Analysis { source_id: None } => {
            return WorkspaceTargetResolution::ProjectSurface;
        }
        EditorTarget::PatternDefinition { id } => ObjectRef::Pattern(PatternId::from_raw(*id)),
        EditorTarget::AutomationLane { id } => {
            ObjectRef::Automation(AutomationLaneId::from_raw(*id))
        }
        EditorTarget::Mixer { bus_id: Some(id) } => ObjectRef::Bus(BusId::from_raw(*id)),
        EditorTarget::Analysis {
            source_id: Some(id),
        } => {
            return if snapshot
                .project
                .state()
                .domains
                .air
                .sources
                .contains_key(&SourceId::new(*id))
            {
                WorkspaceTargetResolution::ProjectSurface
            } else {
                WorkspaceTargetResolution::NotProjectDurable
            };
        }
        EditorTarget::Explanation { .. }
        | EditorTarget::Render { .. }
        | EditorTarget::Extension { .. } => {
            return WorkspaceTargetResolution::NotProjectDurable;
        }
    };
    match object_resolution(snapshot, &object) {
        ObjectResolution::Present => WorkspaceTargetResolution::Object(object),
        ObjectResolution::Missing => WorkspaceTargetResolution::Missing(object),
        ObjectResolution::NotProjectDurable => WorkspaceTargetResolution::NotProjectDurable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        AssetRegistry, ContentFingerprint, DecodedAudioMetadata, SampleFrames,
    };
    use crate::audio::AudioFormat;
    use crate::automation::{
        AutomationCommand, LaneChange, MixerTarget, ParameterAddress, ParameterDescriptor,
        ParameterUnit, SmoothingPolicy, TimeDomain, ValueMapping,
    };
    use crate::command::{BindingCommand, CommandEnvelope, DomainCommand};
    use crate::daw_project::ProjectDomain;
    use crate::daw_render::PcmAsset;
    use crate::live_project::{LiveProject, SourceMaterialMetadata};
    use crate::project_controller::WorkbenchSampleIntent;
    use crate::sample_actions::{MakeBeatResultFocus, SampleChopIntent, SampleKitDestination};
    use crate::session::{Sample, SampleRange};

    fn session(id: u64) -> ProjectSession {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/reveal-source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "reveal source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 48_000,
                    channels: 1,
                    frame_count: SampleFrames(8),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"reveal-source"),
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
            Arc::from([0.0, 0.8, 0.1, 0.0, 0.7, 0.2, 0.0, 0.1]),
        )
        .unwrap();
        let live = LiveProject::from_source_material(
            SourceMaterialMetadata::new("Reveal", "Source"),
            registry,
            asset,
            pcm,
        )
        .unwrap();
        let mut session = ProjectSession::new(ProjectSessionId(id)).unwrap();
        session.install(live, None).unwrap();
        session
    }

    fn make_beat(
        session: &mut ProjectSession,
        kit: SampleKitDestination,
        start: i64,
        end: i64,
    ) -> crate::project_controller::WorkbenchSampleOutcome {
        session
            .publish_primary_workbench_range(
                SampleRange::new(Sample::new(start), Sample::new(end)),
                WorkbenchSampleIntent::MakeBeat {
                    chop: SampleChopIntent::EqualSlices { count: 2 },
                    kit,
                    target_bus: None,
                    bars: 1,
                    quantize_ticks: 120,
                    result_focus: MakeBeatResultFocus::PatternEditor,
                },
            )
            .unwrap()
    }

    #[test]
    fn an_interpretive_identity_is_revealable_and_says_its_guard_is_the_document() {
        let session = session(83);
        let finding = ObjectRef::Finding(crate::project_controller::FindingRef {
            kind: crate::project_controller::FindingKind::Components,
            scope: crate::project_controller::FindingScope::Derivation(
                crate::sample_material::DerivationScope(4),
            ),
            local: crate::project_controller::FindingLocalId::Claim(9),
        });
        let receipt = session
            .issue_reveal(RevealRequest::new(
                finding.clone(),
                RevealIntent::ActivateExisting,
            ))
            .expect("an Investigate row must be revealable");
        let resolution = session.resolve_reveal(&receipt);
        assert_eq!(
            resolution.disposition,
            RevealDisposition::Current {
                freshness: RevealFreshness::DocumentScoped
            }
        );
        assert_eq!(
            resolution.request.map(|request| request.object),
            Some(finding)
        );
    }

    #[test]
    fn undo_falls_back_and_redo_revalidates_recreated_constructive_object() {
        let mut session = session(71);
        let result = make_beat(&mut session, SampleKitDestination::NewKit, 0, 8);
        let receipt = session
            .issue_constructive_reveal(&result.constructive.publication)
            .unwrap();
        let exact = session.resolve_reveal(&receipt);
        assert!(matches!(
            exact.disposition,
            RevealDisposition::Current {
                freshness: RevealFreshness::ExactPublication
            }
        ));
        assert!(session.reveal_guard_is_current(exact.guard.unwrap()));

        session.undo().unwrap();
        let undone = session.resolve_reveal(&receipt);
        assert!(matches!(
            undone.disposition,
            RevealDisposition::Fallback {
                target: RevealFallback::ProjectOverview,
                ..
            }
        ));
        assert!(undone.request.is_none());
        assert!(!session.reveal_guard_is_current(receipt.guard));

        session.redo().unwrap();
        let redone = session.resolve_reveal(&receipt);
        assert!(matches!(
            redone.disposition,
            RevealDisposition::Current {
                freshness: RevealFreshness::RevalidatedCurrent
            }
        ));
        let request = redone.request.unwrap();
        assert_eq!(
            request.object, receipt.request.object,
            "redo recreates the exact durable identity"
        );
        assert_eq!(
            request.expected_project_revision,
            Some(session.project_snapshot().unwrap().revisions().aggregate)
        );
    }

    #[test]
    fn undo_of_edit_to_existing_construction_reveals_typed_predecessor() {
        let mut session = session(72);
        let first = make_beat(&mut session, SampleKitDestination::NewKit, 0, 8);
        let kit = first.constructive.publication.kit;
        let expected_revision = session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .sample_kits
            .kits[&kit]
            .revision;
        let second = make_beat(
            &mut session,
            SampleKitDestination::ExistingKit {
                kit,
                expected_revision,
            },
            1,
            7,
        );
        let receipt = session
            .issue_constructive_reveal(&second.constructive.publication)
            .unwrap();

        session.undo().unwrap();
        let resolution = session.resolve_reveal(&receipt);
        assert!(matches!(
            resolution.disposition,
            RevealDisposition::Predecessor { .. }
        ));
        assert_eq!(
            resolution.request.unwrap().object,
            ObjectRef::Instrument(InstrumentRef::SampleKit(kit))
        );
    }

    #[test]
    fn replacing_document_rejects_old_receipt_even_when_ids_coincide() {
        let mut session = session(73);
        let result = make_beat(&mut session, SampleKitDestination::NewKit, 0, 8);
        let receipt = session
            .issue_constructive_reveal(&result.constructive.publication)
            .unwrap();
        let snapshot = session.project_snapshot().unwrap().clone();
        let old_generation = session.document_generation();
        session
            .install(
                LiveProject::from_project(
                    snapshot.project.as_ref().clone(),
                    snapshot.pcm.as_ref().clone(),
                )
                .unwrap(),
                None,
            )
            .unwrap();
        assert!(session.document_generation() > old_generation);
        assert!(matches!(
            session.resolve_reveal(&receipt).disposition,
            RevealDisposition::Rejected(RevealRejection::DocumentReplaced { .. })
        ));
    }

    #[test]
    fn automation_occurrence_requires_exact_binding_then_reveals_lane_or_fallback() {
        let mut session = session(74);
        let snapshot = session.project_snapshot().unwrap().clone();
        let mut project = snapshot.project.as_ref().clone();
        let source_bus = project.state().domains.mixer.master();
        let mut occurrence = None;
        let mut alias = None;
        project
            .transact(
                "automation occurrence fixture",
                project.revisions().aggregate,
                BTreeSet::from([
                    ProjectDomain::Arrangement,
                    ProjectDomain::Automation,
                    ProjectDomain::Bindings,
                ]),
                |state| -> Result<(), String> {
                    let address = ParameterAddress::Mixer(MixerTarget::BusGain(source_bus.get()));
                    state
                        .domains
                        .automation
                        .register_parameter(ParameterDescriptor {
                            address: address.clone(),
                            name: "Gain".into(),
                            unit: ParameterUnit::Decibels,
                            minimum: -60.0,
                            maximum: 12.0,
                            default: 0.0,
                            mapping: ValueMapping::Linear,
                            smoothing: SmoothingPolicy::LinearFrames(32),
                        })
                        .map_err(|error| error.to_string())?;
                    let lane = state
                        .domains
                        .automation
                        .create_lane("Gain", address, TimeDomain::Frames)
                        .map_err(|error| error.to_string())?;
                    let parameter = state
                        .bindings
                        .bind_automation_lane(lane)
                        .map_err(|error| error.to_string())?;
                    let mut arrangement = crate::arrangement::ArrangementEditor::from_state(
                        state.domains.arrangement.clone(),
                    )
                    .map_err(|error| error.to_string())?;
                    let track = arrangement
                        .create_track("Automation", crate::arrangement::TrackKind::Automation)
                        .map_err(|error| error.to_string())?;
                    let clip = arrangement
                        .create_automation_clip(
                            track,
                            "Gain",
                            crate::arrangement::FrameRange::from_start_and_len(
                                crate::arrangement::Frame::ZERO,
                                64,
                            )
                            .map_err(|error| error.to_string())?,
                            parameter,
                        )
                        .map_err(|error| error.to_string())?;
                    state.domains.arrangement = arrangement.state().clone();
                    occurrence = Some(AutomationOccurrenceRef {
                        arrangement_clip: clip,
                        lane,
                    });
                    alias = Some(parameter);
                    Ok(())
                },
            )
            .unwrap();
        session
            .install(
                LiveProject::from_project(project, snapshot.pcm.as_ref().clone()).unwrap(),
                None,
            )
            .unwrap();
        let occurrence = occurrence.unwrap();
        let alias = alias.unwrap();
        let receipt = session
            .issue_reveal(RevealRequest::new(
                ObjectRef::AutomationOccurrence(occurrence),
                RevealIntent::ActivateExisting,
            ))
            .unwrap();

        let clip = session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .arrangement
            .clip(occurrence.arrangement_clip)
            .unwrap()
            .clone();
        let commands = vec![DomainCommand::Arrangement(
            crate::arrangement::ArrangementOperation::PutClip {
                before: Some(clip),
                after: None,
            },
        )];
        session
            .execute(CommandEnvelope {
                label: "Delete automation occurrence".into(),
                base_revision: session.project_snapshot().unwrap().revisions().aggregate,
                coalesce: None,
                id_claims: crate::command::claims_for_commands(&commands),
                commands,
            })
            .unwrap();
        let predecessor = session.resolve_reveal(&receipt);
        assert_eq!(
            predecessor.request.unwrap().object,
            ObjectRef::Automation(occurrence.lane)
        );

        let lane = session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .automation
            .lane(occurrence.lane)
            .unwrap()
            .clone();
        let commands = vec![
            DomainCommand::Automation(AutomationCommand {
                label: "Delete lane".into(),
                parameters: Vec::new(),
                changes: vec![LaneChange {
                    before: Some(lane),
                    after: None,
                }],
            }),
            DomainCommand::Bindings(BindingCommand::PutAutomationLaneAlias {
                alias,
                before: Some(occurrence.lane),
                after: None,
            }),
        ];
        session
            .execute(CommandEnvelope {
                label: "Delete automation lane".into(),
                base_revision: session.project_snapshot().unwrap().revisions().aggregate,
                coalesce: None,
                id_claims: crate::command::claims_for_commands(&commands),
                commands,
            })
            .unwrap();
        assert!(matches!(
            session.resolve_reveal(&receipt).disposition,
            RevealDisposition::Fallback {
                target: RevealFallback::ProjectOverview,
                ..
            }
        ));
    }
}
